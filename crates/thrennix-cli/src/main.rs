//! thrennix-cli: command-line front end for the thrennix download engine.

use std::collections::HashSet;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant};

use clap::{Parser, Subcommand};
use thrennix_core::{
    DownloadHandle, DownloadRequest, Engine, EngineConfig, Error, Progress, Status,
};

#[derive(Parser, Debug)]
#[command(
    name = "thrennix-cli",
    version,
    about = "Fast multi-connection downloader (thrennix engine)",
    args_conflicts_with_subcommands = true,
    subcommand_negates_reqs = true
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// URL to download.
    #[arg(required = true)]
    url: Option<String>,

    /// Output directory (default: current directory).
    #[arg(short = 'o', long = "output-dir")]
    output_dir: Option<PathBuf>,

    /// Output file name (default: from Content-Disposition or the URL).
    #[arg(short = 'f', long = "file-name")]
    file_name: Option<String>,

    #[command(flatten)]
    common: Common,

    /// Raw Cookie header value.
    #[arg(long)]
    cookie: Option<String>,

    /// Referer header.
    #[arg(long)]
    referer: Option<String>,

    /// User-Agent header.
    #[arg(long = "user-agent", short = 'A')]
    user_agent: Option<String>,

    /// Extra header, "Name: value" (repeatable).
    #[arg(short = 'H', long = "header")]
    headers: Vec<String>,
}

#[derive(clap::Args, Debug, Clone)]
struct Common {
    /// Initial number of connections.
    #[arg(short = 'c', long = "connections")]
    connections: Option<usize>,

    /// Maximum number of connections.
    #[arg(short = 'm', long = "max-connections")]
    max_connections: Option<usize>,

    /// Print the final summary as one JSON line on stdout.
    #[arg(long, global = true)]
    json: bool,

    /// Disable the live progress line.
    #[arg(long, short = 'q', global = true)]
    quiet: bool,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Resume a paused download from its `.thrennix` state file.
    Resume {
        /// Path to the `<file>.thrennix` state file.
        state_file: PathBuf,
        /// Continue with a new URL (e.g. the old link expired).
        #[arg(long)]
        url: Option<String>,
        #[command(flatten)]
        common: Common,
    },
}

const EXIT_ERROR: u8 = 1;
const EXIT_PAUSED: u8 = 130;

fn main() -> ExitCode {
    let cli = Cli::parse();
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("error: cannot start runtime: {e}");
            return ExitCode::from(EXIT_ERROR);
        }
    };
    rt.block_on(run(cli))
}

fn engine_config(c: &Common) -> EngineConfig {
    let mut cfg = EngineConfig::default();
    if let Some(m) = c.max_connections {
        cfg.max_connections = m.max(1);
    }
    if let Some(n) = c.connections {
        cfg.initial_connections = n.max(1);
    }
    if cfg.initial_connections > cfg.max_connections {
        cfg.initial_connections = cfg.max_connections;
    }
    cfg
}

fn parse_header(h: &str) -> Result<(String, String), String> {
    let (name, value) = h
        .split_once(':')
        .ok_or_else(|| format!("invalid header {h:?}, expected \"Name: value\""))?;
    let name = name.trim();
    if name.is_empty() {
        return Err(format!("invalid header {h:?}: empty name"));
    }
    Ok((name.to_string(), value.trim().to_string()))
}

fn existing_state_files(dir: &Path) -> HashSet<String> {
    std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter_map(|e| e.file_name().into_string().ok())
                .filter(|n| n.ends_with(".thrennix"))
                .collect()
        })
        .unwrap_or_default()
}

async fn run(cli: Cli) -> ExitCode {
    match cli.command {
        Some(Command::Resume {
            state_file,
            url,
            common,
        }) => {
            let engine = Engine::new(engine_config(&common));
            let started = Instant::now();
            let handle = engine.resume_from_state(state_file, url);
            drive_inner(handle, &common, started, None).await
        }
        None => {
            let Some(url) = cli.url else {
                eprintln!("error: missing URL");
                return ExitCode::from(EXIT_ERROR);
            };
            let mut headers = Vec::new();
            for h in &cli.headers {
                match parse_header(h) {
                    Ok(kv) => headers.push(kv),
                    Err(e) => {
                        eprintln!("error: {e}");
                        return ExitCode::from(EXIT_ERROR);
                    }
                }
            }
            let dest_dir = match cli.output_dir {
                Some(d) => d,
                None => std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            };
            if let Err(e) = std::fs::create_dir_all(&dest_dir) {
                eprintln!("error: cannot create {}: {e}", dest_dir.display());
                return ExitCode::from(EXIT_ERROR);
            }
            let pre_existing = existing_state_files(&dest_dir);
            let request = DownloadRequest {
                url,
                dest_dir,
                file_name: cli.file_name,
                cookies: cli.cookie,
                referer: cli.referer,
                user_agent: cli.user_agent,
                headers,
            };
            let engine = Engine::new(engine_config(&cli.common));
            let started = Instant::now();
            let handle = engine.start(request);
            drive_inner(handle, &cli.common, started, Some(Arc::new(pre_existing))).await
        }
    }
}

/// Statistics gathered while the download runs.
struct Tracker {
    peak_speed: f64,
    max_conns: usize,
    /// Bytes already present when this session started (resume); excluded from the average.
    base: Option<u64>,
    /// `None` = explicit resume (always take the base from the first snapshot);
    /// `Some(set)` = state files that existed before start.
    pre_existing: Option<Arc<HashSet<String>>>,
}

impl Tracker {
    fn observe(&mut self, p: &Progress) {
        if p.status == Status::Downloading || p.status == Status::Completed {
            self.peak_speed = self.peak_speed.max(p.speed_bps);
            self.max_conns = self.max_conns.max(p.connections);
            if self.base.is_none() && !p.file_name.is_empty() {
                let resumed = match &self.pre_existing {
                    None => true,
                    Some(set) => set.contains(&format!("{}.thrennix", p.file_name)),
                };
                self.base = Some(if resumed { p.downloaded } else { 0 });
            }
        }
    }
}

async fn drive_inner(
    handle: DownloadHandle,
    common: &Common,
    started: Instant,
    pre_existing: Option<Arc<HashSet<String>>>,
) -> ExitCode {
    let handle = Arc::new(handle);
    let live = !common.quiet && std::io::stderr().is_terminal();
    let mut tracker = Tracker {
        peak_speed: 0.0,
        max_conns: 0,
        base: None,
        pre_existing,
    };
    let mut rx = handle.subscribe();

    let wait_handle = handle.clone();
    let mut wait = tokio::spawn(async move { wait_handle.wait().await });

    let mut tick = tokio::time::interval(Duration::from_millis(200));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut paused_by_user = false;
    let mut line_len = 0usize;

    let result = loop {
        tokio::select! {
            res = &mut wait => {
                break match res {
                    Ok(r) => r,
                    Err(e) => Err(Error::Other(format!("download task failed: {e}"))),
                };
            }
            _ = tokio::signal::ctrl_c(), if !paused_by_user => {
                paused_by_user = true;
                if live { clear_line(line_len); line_len = 0; }
                eprintln!("Pausing...");
                handle.pause();
            }
            _ = tokio::signal::ctrl_c(), if paused_by_user => {
                eprintln!("Interrupted again, exiting.");
                return ExitCode::from(EXIT_PAUSED);
            }
            changed = rx.changed() => {
                if changed.is_ok() {
                    let p = rx.borrow_and_update().clone();
                    tracker.observe(&p);
                }
            }
            _ = tick.tick() => {
                let p = handle.progress();
                tracker.observe(&p);
                if live && !paused_by_user {
                    line_len = draw_progress(&p, line_len);
                }
            }
        }
    };

    let elapsed = started.elapsed();
    let final_progress = handle.progress();
    tracker.observe(&final_progress);
    if live {
        clear_line(line_len);
    }

    match result {
        Ok(path) => {
            let size = std::fs::metadata(&path)
                .map(|m| m.len())
                .unwrap_or(final_progress.downloaded);
            let session_bytes = size.saturating_sub(tracker.base.unwrap_or(0));
            let secs = elapsed.as_secs_f64().max(1e-9);
            let avg = session_bytes as f64 / secs;
            let peak = tracker.peak_speed.max(avg);
            eprintln!(
                "Done: {}\n  size {}  time {:.2}s  avg {}/s  peak {}/s  max connections {}",
                path.display(),
                fmt_bytes(size),
                secs,
                fmt_bytes(avg as u64),
                fmt_bytes(peak as u64),
                tracker.max_conns
            );
            if common.json {
                let j = serde_json::json!({
                    "status": "completed",
                    "path": path.display().to_string(),
                    "file_name": final_progress.file_name,
                    "bytes": size,
                    "session_bytes": session_bytes,
                    "seconds": secs,
                    "avg_speed_bps": avg,
                    "avg_mib_s": avg / 1_048_576.0,
                    "peak_speed_bps": peak,
                    "max_connections": tracker.max_conns,
                    "resumable": final_progress.resumable,
                });
                println!("{j}");
            }
            ExitCode::SUCCESS
        }
        Err(Error::Paused) => {
            eprintln!(
                "Paused at {} of {} — run the same command again to resume",
                fmt_bytes(final_progress.downloaded),
                final_progress
                    .total
                    .map(fmt_bytes)
                    .unwrap_or_else(|| "?".into())
            );
            if common.json {
                let j = serde_json::json!({
                    "status": "paused",
                    "path": final_progress.path.display().to_string(),
                    "downloaded": final_progress.downloaded,
                    "total": final_progress.total,
                    "seconds": elapsed.as_secs_f64(),
                });
                println!("{j}");
            }
            ExitCode::from(EXIT_PAUSED)
        }
        Err(e) => {
            eprintln!("error: {e}");
            if common.json {
                let j = serde_json::json!({
                    "status": "error",
                    "error": e.to_string(),
                    "seconds": elapsed.as_secs_f64(),
                });
                println!("{j}");
            }
            ExitCode::from(EXIT_ERROR)
        }
    }
}

fn clear_line(len: usize) {
    if len > 0 {
        let mut err = std::io::stderr().lock();
        let _ = write!(err, "\r{}\r", " ".repeat(len));
        let _ = err.flush();
    }
}

fn draw_progress(p: &Progress, prev_len: usize) -> usize {
    let line = match &p.status {
        Status::Probing => "probing...".to_string(),
        _ => {
            let pct = match p.total {
                Some(t) if t > 0 => format!("{:5.1}%", p.downloaded as f64 * 100.0 / t as f64),
                _ => "    ?%".to_string(),
            };
            let total = p.total.map(fmt_bytes).unwrap_or_else(|| "?".into());
            let eta = p.eta_secs().map(fmt_eta).unwrap_or_else(|| "--:--".into());
            format!(
                "{pct}  {}/{}  {}/s  ETA {}  {} conns",
                fmt_bytes(p.downloaded),
                total,
                fmt_bytes(p.speed_bps as u64),
                eta,
                p.connections
            )
        }
    };
    let pad = prev_len.saturating_sub(line.len());
    let mut err = std::io::stderr().lock();
    let _ = write!(err, "\r{line}{}", " ".repeat(pad));
    let _ = err.flush();
    line.len()
}

fn fmt_bytes(b: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = b as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{b} B")
    } else {
        format!("{v:.2} {}", UNITS[u])
    }
}

fn fmt_eta(secs: f64) -> String {
    if !secs.is_finite() || secs > 359_999.0 {
        return "--:--".into();
    }
    let s = secs.round() as u64;
    let (h, m, s) = (s / 3600, (s % 3600) / 60, s % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m:02}:{s:02}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headers_parse() {
        assert_eq!(
            parse_header("X-A: b c").unwrap(),
            ("X-A".into(), "b c".into())
        );
        assert_eq!(
            parse_header("Authorization:Bearer x:y").unwrap(),
            ("Authorization".into(), "Bearer x:y".into())
        );
        assert!(parse_header("nocolon").is_err());
        assert!(parse_header(": v").is_err());
    }

    #[test]
    fn formatting() {
        assert_eq!(fmt_bytes(512), "512 B");
        assert_eq!(fmt_bytes(1536), "1.50 KiB");
        assert_eq!(fmt_eta(65.0), "01:05");
        assert_eq!(fmt_eta(3725.0), "1:02:05");
    }

    #[test]
    fn cli_parses() {
        let c = Cli::try_parse_from([
            "thrennix-cli",
            "http://x/y",
            "-o",
            "d",
            "-c",
            "4",
            "-m",
            "16",
            "-H",
            "A: b",
            "--json",
        ])
        .unwrap();
        assert_eq!(c.url.as_deref(), Some("http://x/y"));
        assert_eq!(c.common.connections, Some(4));
        assert!(c.common.json);
        let c = Cli::try_parse_from(["thrennix-cli", "resume", "f.thrennix", "--url", "http://n"])
            .unwrap();
        assert!(matches!(c.command, Some(Command::Resume { .. })));
        assert!(Cli::try_parse_from(["thrennix-cli"]).is_err());
        let cfg = engine_config(&Common {
            connections: Some(8),
            max_connections: Some(1),
            json: false,
            quiet: false,
        });
        assert_eq!((cfg.initial_connections, cfg.max_connections), (1, 1));
    }
}
