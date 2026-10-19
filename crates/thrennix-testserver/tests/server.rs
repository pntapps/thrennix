use std::time::{Duration, Instant};

use thrennix_testserver::{pattern_byte, spawn_local, ServerConfig};

async fn server() -> String {
    spawn_local(ServerConfig { log: false }).await.unwrap()
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .no_gzip()
        .no_brotli()
        .build()
        .unwrap()
}

fn check_pattern(offset: u64, data: &[u8]) {
    for (k, b) in data.iter().enumerate() {
        assert_eq!(
            *b,
            pattern_byte(offset + k as u64),
            "mismatch at {}",
            offset + k as u64
        );
    }
}

#[tokio::test]
async fn head_and_headers() {
    let base = server().await;
    let r = client()
        .head(format!("{base}/file/10M"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let h = r.headers();
    assert_eq!(h["content-length"], "10485760");
    assert_eq!(h["accept-ranges"], "bytes");
    assert_eq!(h["etag"], "\"size-10485760\"");
    assert!(h.contains_key("last-modified"));
    assert_eq!(
        h["content-disposition"],
        "attachment; filename=\"test-10M.bin\""
    );
}

#[tokio::test]
async fn full_body_and_ranges() {
    let base = server().await;
    let c = client();
    let body = c
        .get(format!("{base}/file/1000000"))
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    assert_eq!(body.len(), 1_000_000);
    check_pattern(0, &body);

    let r = c
        .get(format!("{base}/file/1000000"))
        .header("Range", "bytes=1000-1999")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 206);
    assert_eq!(r.headers()["content-range"], "bytes 1000-1999/1000000");
    let b = r.bytes().await.unwrap();
    assert_eq!(b.len(), 1000);
    check_pattern(1000, &b);

    let r = c
        .get(format!("{base}/file/1000000"))
        .header("Range", "bytes=-10")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 206);
    assert_eq!(r.headers()["content-range"], "bytes 999990-999999/1000000");
    check_pattern(999_990, &r.bytes().await.unwrap());

    let r = c
        .get(format!("{base}/file/1000000"))
        .header("Range", "bytes=999000-")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 206);
    check_pattern(999_000, &r.bytes().await.unwrap());

    let r = c
        .get(format!("{base}/file/1000000"))
        .header("Range", "bytes=2000000-")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 416);
    assert_eq!(r.headers()["content-range"], "bytes */1000000");
}

#[tokio::test]
async fn if_range_and_etag_override_and_norange() {
    let base = server().await;
    let c = client();
    let url = format!("{base}/file/1M");
    // Matching If-Range: 206
    let r = c
        .get(&url)
        .header("Range", "bytes=0-9")
        .header("If-Range", "\"size-1048576\"")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 206);
    // Changed file (etag override): If-Range mismatch -> full 200
    let r = c
        .get(format!("{url}?etag=v2"))
        .header("Range", "bytes=0-9")
        .header("If-Range", "\"size-1048576\"")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()["etag"], "\"v2\"");
    assert_eq!(r.bytes().await.unwrap().len(), 1 << 20);
    // Last-Modified If-Range
    let r = c
        .get(&url)
        .header("Range", "bytes=0-9")
        .header("If-Range", thrennix_testserver::LAST_MODIFIED)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 206);
    // norange
    let r = c
        .get(format!("{url}?norange=1"))
        .header("Range", "bytes=0-9")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()["accept-ranges"], "none");
    assert_eq!(r.bytes().await.unwrap().len(), 1 << 20);
}

#[tokio::test]
async fn per_connection_throttle_accuracy() {
    let base = server().await;
    // 3 MiB at 1024 KiB/s => ~3 s
    let t = Instant::now();
    let b = client()
        .get(format!("{base}/file/3M?rate=1024"))
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    let secs = t.elapsed().as_secs_f64();
    assert_eq!(b.len(), 3 << 20);
    let rate_kib = b.len() as f64 / 1024.0 / secs;
    assert!(
        (rate_kib - 1024.0).abs() / 1024.0 < 0.15,
        "rate {rate_kib:.0} KiB/s, {secs:.2}s"
    );
}

#[tokio::test]
async fn shared_throttle_accuracy() {
    let base = server().await;
    // 3 parallel 1 MiB ranges sharing 1024 KiB/s => ~3 s total
    let c = client();
    let url = format!("{base}/file/3M?total_rate=1024");
    let t = Instant::now();
    let mut tasks = Vec::new();
    for i in 0..3u64 {
        let c = c.clone();
        let url = url.clone();
        tasks.push(tokio::spawn(async move {
            let s = i << 20;
            let e = s + (1 << 20) - 1;
            c.get(url)
                .header("Range", format!("bytes={s}-{e}"))
                .send()
                .await
                .unwrap()
                .bytes()
                .await
                .unwrap()
                .len()
        }));
    }
    let mut total = 0;
    for t in tasks {
        total += t.await.unwrap();
    }
    let secs = t.elapsed().as_secs_f64();
    assert_eq!(total, 3 << 20);
    let rate_kib = total as f64 / 1024.0 / secs;
    assert!(
        (rate_kib - 1024.0).abs() / 1024.0 < 0.15,
        "rate {rate_kib:.0} KiB/s, {secs:.2}s"
    );
}

#[tokio::test]
async fn max_conns_returns_503_and_stats() {
    let base = server().await;
    let c = client();
    let url = format!("{base}/file/10M?rate=256&max_conns=2");
    let r1 = c.get(&url).send().await.unwrap();
    let r2 = c.get(&url).send().await.unwrap();
    assert_eq!(r1.status(), 200);
    assert_eq!(r2.status(), 200);
    let r3 = c.get(&url).send().await.unwrap();
    assert_eq!(r3.status(), 503);
    let stats: serde_json::Value = serde_json::from_slice(
        &c.get(format!("{base}/stats"))
            .send()
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(stats["peak_concurrency"], 2);
    assert_eq!(stats["rejected_503"], 1);
    drop((r1, r2));
    tokio::time::sleep(Duration::from_millis(200)).await;
    let r4 = c.get(&url).send().await.unwrap();
    assert_eq!(r4.status(), 200);
    drop(r4);
    let r = c.post(format!("{base}/stats/reset")).send().await.unwrap();
    assert_eq!(r.status(), 204);
}

#[tokio::test]
async fn fail_drops_connection() {
    let base = server().await;
    let r = client()
        .get(format!("{base}/file/4M?fail=1"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert!(r.bytes().await.is_err(), "body should be cut short");
}

#[tokio::test]
async fn latency_delays_first_byte() {
    let base = server().await;
    let t = Instant::now();
    let r = client()
        .get(format!("{base}/file/1K?latency=300"))
        .send()
        .await
        .unwrap();
    assert!(t.elapsed() >= Duration::from_millis(290));
    assert_eq!(r.bytes().await.unwrap().len(), 1024);
}
