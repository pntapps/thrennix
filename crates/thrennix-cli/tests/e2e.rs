//! End-to-end tests: in-process thrennix-testserver + the real thrennix-cli binary (and the engine API
//! for pause, since sending Ctrl+C to a child process is not portable on Windows).

use std::path::{Path, PathBuf};
use std::process::Output;
use std::time::{Duration, Instant};

use thrennix_core::{DownloadRequest, Engine, EngineConfig, Error};
use thrennix_testserver::{fill_pattern, spawn_local, ServerConfig};

const CLI: &str = env!("CARGO_BIN_EXE_thrennix-cli");

async fn server() -> String {
    spawn_local(ServerConfig { log: false }).await.unwrap()
}

fn temp_dir(name: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let d = std::env::temp_dir().join(format!(
        "thrennix-e2e-{name}-{}-{nanos}",
        std::process::id()
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

async fn cli(args: &[&str]) -> Output {
    tokio::process::Command::new(CLI)
        .args(args)
        .kill_on_drop(true)
        .output()
        .await
        .unwrap()
}

fn summary(out: &Output) -> serde_json::Value {
    let stdout = String::from_utf8_lossy(&out.stdout);
    let line = stdout.lines().last().unwrap_or_else(|| {
        panic!(
            "no JSON on stdout; stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        )
    });
    serde_json::from_str(line).unwrap_or_else(|e| panic!("bad JSON {line:?}: {e}"))
}

fn assert_ok(out: &Output) {
    assert!(
        out.status.success(),
        "thrennix-cli failed ({:?})\nstdout: {}\nstderr: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Verify the file has exactly `size` bytes of the server pattern.
fn verify_pattern(path: &Path, size: u64) {
    let data = std::fs::read(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    assert_eq!(data.len() as u64, size, "size of {}", path.display());
    let mut expect = vec![0u8; 1 << 20];
    for (i, chunk) in data.chunks(1 << 20).enumerate() {
        let off = (i as u64) << 20;
        fill_pattern(off, &mut expect[..chunk.len()]);
        if chunk != &expect[..chunk.len()] {
            let k = chunk.iter().zip(&expect).position(|(a, b)| a != b).unwrap();
            panic!("content mismatch at byte {}", off + k as u64);
        }
    }
}

fn no_state_left(dir: &Path) {
    let left: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".thrennix"))
        .collect();
    assert!(left.is_empty(), "state files left behind: {left:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn byte_exact_64mib() {
    let base = server().await;
    let dir = temp_dir("exact");
    let out = cli(&[
        &format!("{base}/file/64M"),
        "-o",
        dir.to_str().unwrap(),
        "--json",
    ])
    .await;
    assert_ok(&out);
    let s = summary(&out);
    assert_eq!(s["status"], "completed");
    assert_eq!(s["bytes"], 64u64 << 20);
    let path = dir.join("test-64M.bin");
    verify_pattern(&path, 64 << 20);
    no_state_left(&dir);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread")]
async fn multi_connection_beats_single_on_throttled_server() {
    let base = server().await;
    let url = format!("{base}/file/32M?rate=1024");
    let d1 = temp_dir("single");
    let dn = temp_dir("multi");
    let (single, multi) = tokio::join!(
        async {
            let t = Instant::now();
            let o = cli(&[
                &url,
                "-o",
                d1.to_str().unwrap(),
                "-c",
                "1",
                "-m",
                "1",
                "--json",
            ])
            .await;
            (o, t.elapsed())
        },
        async {
            let t = Instant::now();
            let o = cli(&[&url, "-o", dn.to_str().unwrap(), "--json"]).await;
            (o, t.elapsed())
        }
    );
    assert_ok(&single.0);
    assert_ok(&multi.0);
    verify_pattern(&d1.join("test-32M.bin"), 32 << 20);
    verify_pattern(&dn.join("test-32M.bin"), 32 << 20);
    let ratio = single.1.as_secs_f64() / multi.1.as_secs_f64();
    eprintln!(
        "single {:.2}s, multi {:.2}s, speedup {ratio:.1}x",
        single.1.as_secs_f64(),
        multi.1.as_secs_f64()
    );
    assert!(ratio >= 4.0, "multi-connection speedup only {ratio:.2}x");
    let _ = std::fs::remove_dir_all(&d1);
    let _ = std::fs::remove_dir_all(&dn);
}

#[tokio::test(flavor = "multi_thread")]
async fn norange_server_still_downloads() {
    let base = server().await;
    let dir = temp_dir("norange");
    let out = cli(&[
        &format!("{base}/file/16M?norange=1"),
        "-o",
        dir.to_str().unwrap(),
        "--json",
    ])
    .await;
    assert_ok(&out);
    verify_pattern(&dir.join("test-16M.bin"), 16 << 20);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread")]
async fn flaky_server_completes_byte_exact() {
    let base = server().await;
    let dir = temp_dir("fail");
    let out = cli(&[
        &format!("{base}/file/48M?fail=0.05&rate=8192"),
        "-o",
        dir.to_str().unwrap(),
        "--json",
    ])
    .await;
    assert_ok(&out);
    verify_pattern(&dir.join("test-48M.bin"), 48 << 20);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread")]
async fn connection_limited_server_completes() {
    let base = server().await;
    let dir = temp_dir("maxconns");
    let out = cli(&[
        &format!("{base}/file/32M?max_conns=4&rate=4096"),
        "-o",
        dir.to_str().unwrap(),
        "--json",
    ])
    .await;
    assert_ok(&out);
    verify_pattern(&dir.join("test-32M.bin"), 32 << 20);
    let stats: serde_json::Value = serde_json::from_slice(
        &reqwest::get(format!("{base}/stats"))
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap(),
    )
    .unwrap();
    eprintln!("stats: {stats}");
    assert!(stats["peak_concurrency"].as_u64().unwrap() <= 4);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Start via the engine, pause after ~1.5 s, check the state file, then return.
async fn start_and_pause(url: &str, dir: &Path) -> PathBuf {
    let engine = Engine::new(EngineConfig::default());
    let handle = engine.start(DownloadRequest {
        url: url.to_string(),
        dest_dir: dir.to_path_buf(),
        ..Default::default()
    });
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let before = handle.progress();
    handle.pause();
    let res = tokio::time::timeout(Duration::from_secs(20), handle.wait())
        .await
        .expect("wait after pause timed out");
    assert!(
        matches!(res, Err(Error::Paused)),
        "expected Paused, got {res:?}"
    );
    let p = handle.progress();
    assert!(
        p.downloaded > 0,
        "nothing downloaded before pause ({before:?})"
    );
    assert!(
        p.total.is_some_and(|t| p.downloaded < t),
        "download finished before pause"
    );
    let state = PathBuf::from(format!("{}.thrennix", p.path.display()));
    assert!(state.exists(), "state file {} missing", state.display());
    state
}

#[tokio::test(flavor = "multi_thread")]
async fn pause_then_rerun_same_command_resumes_byte_exact() {
    let base = server().await;
    let dir = temp_dir("pause");
    let url = format!("{base}/file/64M?rate=2048");
    start_and_pause(&url, &dir).await;
    let out = cli(&[&url, "-o", dir.to_str().unwrap(), "--json"]).await;
    assert_ok(&out);
    let s = summary(&out);
    assert!(
        s["session_bytes"].as_u64().unwrap() < 64 << 20,
        "resume re-downloaded everything: {s}"
    );
    verify_pattern(&dir.join("test-64M.bin"), 64 << 20);
    no_state_left(&dir);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread")]
async fn pause_then_resume_subcommand_with_new_url() {
    let base = server().await;
    let dir = temp_dir("resume-url");
    let state = start_and_pause(&format!("{base}/file/64M?rate=2048"), &dir).await;
    // "Expired link": continue from a different URL serving the same file.
    let out = cli(&[
        "resume",
        state.to_str().unwrap(),
        "--url",
        &format!("{base}/file/64M"),
        "--json",
    ])
    .await;
    assert_ok(&out);
    verify_pattern(&dir.join("test-64M.bin"), 64 << 20);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread")]
async fn etag_change_during_resume_is_handled() {
    let base = server().await;
    let dir = temp_dir("etag");
    let state = start_and_pause(&format!("{base}/file/64M?rate=2048&etag=v1"), &dir).await;
    let out = cli(&[
        "resume",
        state.to_str().unwrap(),
        "--url",
        &format!("{base}/file/64M?etag=v2"),
        "--json",
    ])
    .await;
    let stderr = String::from_utf8_lossy(&out.stderr);
    eprintln!(
        "etag change -> exit {:?}: {}",
        out.status.code(),
        stderr.trim()
    );
    if out.status.success() {
        // Engine chose to restart from scratch: the result must still be byte-exact.
        verify_pattern(&dir.join("test-64M.bin"), 64 << 20);
    } else {
        assert_eq!(out.status.code(), Some(1), "stderr: {stderr}");
        assert!(
            stderr.to_lowercase().contains("changed"),
            "expected RemoteChanged, stderr: {stderr}"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread")]
async fn bad_url_exits_with_1() {
    let base = server().await;
    let dir = temp_dir("bad");
    let out = cli(&[
        &format!("{base}/file/notasize"),
        "-o",
        dir.to_str().unwrap(),
        "--json",
    ])
    .await;
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(summary(&out)["status"], "error");
    let _ = std::fs::remove_dir_all(&dir);
}
