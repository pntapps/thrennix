//! Named pipe `\\.\pipe\thrennix-ipc-<user SID>`: newline-delimited JSON between the native
//! host and the GUI. The pipe only grants access to the current user, and clients verify
//! that the server process runs as the current user.

use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::os::windows::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};

use crate::nm::MAX_MESSAGE;
use crate::protocol::{validate_url, Request, Response};
use crate::state::Shared;
use crate::winsec;

const ERROR_PIPE_BUSY: i32 = 231;
/// GUI-side limit for a yt-dlp probe; the host waits a little longer than this.
const YT_PROBE_TIMEOUT: Duration = Duration::from_secs(55);
/// How long the host waits for the GUI's reply.
pub const REPLY_TIMEOUT: Duration = Duration::from_secs(20);
pub const REPLY_TIMEOUT_YT: Duration = Duration::from_secs(60);

pub fn pipe_name() -> io::Result<&'static str> {
    static NAME: OnceLock<Option<String>> = OnceLock::new();
    NAME.get_or_init(|| {
        winsec::current_user_sid()
            .ok()
            .map(|sid| format!(r"\\.\pipe\thrennix-ipc-{sid}"))
    })
    .as_deref()
    .ok_or_else(|| io::Error::other("cannot determine the current user's SID"))
}

// ---------------------------------------------------------------- client (sync)

fn is_busy(e: &io::Error) -> bool {
    e.raw_os_error() == Some(ERROR_PIPE_BUSY)
}

fn try_connect() -> io::Result<File> {
    let f = OpenOptions::new()
        .read(true)
        .write(true)
        .open(pipe_name()?)?;
    winsec::verify_pipe_server_is_current_user(&f).map_err(|e| {
        io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("refusing pipe server: {e}"),
        )
    })?;
    Ok(f)
}

/// Connect to the running GUI. When `launch` is set and no GUI is running, start one
/// and keep retrying for up to 5 s.
pub fn connect(launch: bool) -> io::Result<File> {
    let first = match try_connect() {
        Ok(f) => return Ok(f),
        Err(e) => e,
    };
    let not_found = first.kind() == io::ErrorKind::NotFound;
    if !(is_busy(&first) || (not_found && launch)) {
        return Err(first);
    }
    if not_found {
        launch_gui()?;
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        std::thread::sleep(Duration::from_millis(100));
        match try_connect() {
            Ok(f) => return Ok(f),
            Err(e) if Instant::now() >= deadline => return Err(e),
            Err(e) if is_busy(&e) || e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
}

/// Send one JSON line, return the one-line reply (without the newline).
pub fn request(pipe: &File, line: &str) -> io::Result<String> {
    let mut w = pipe;
    w.write_all(line.as_bytes())?;
    w.write_all(b"\n")?;
    w.flush()?;
    let mut reply = String::new();
    let n = io::Read::take(BufReader::new(pipe), MAX_MESSAGE as u64 + 2).read_line(&mut reply)?;
    if n == 0 {
        return Err(io::ErrorKind::UnexpectedEof.into());
    }
    Ok(reply.trim_end().to_string())
}

/// [`request`] with a deadline for the reply (the pipe has no read timeout of its own).
pub fn request_timeout(pipe: File, line: String, timeout: Duration) -> io::Result<String> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(request(&pipe, &line));
    });
    rx.recv_timeout(timeout)
        .unwrap_or_else(|_| Err(io::Error::new(io::ErrorKind::TimedOut, "no reply from app")))
}

/// If another GUI instance is running, ask it to show itself. Returns true if it did.
pub fn signal_existing_instance() -> bool {
    match connect(false) {
        Ok(pipe) => {
            let _ = request(&pipe, r#"{"type":"show"}"#);
            true
        }
        Err(_) => false,
    }
}

const DETACHED_PROCESS: u32 = 0x0000_0008;
const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;

/// Start the GUI (this exe, no args), detached from the browser's job object if allowed.
fn launch_gui() -> io::Result<()> {
    let exe = std::env::current_exe()?;
    crate::host::make_std_handles_uninheritable();
    let spawn = |flags: u32| {
        Command::new(&exe)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(flags)
            .spawn()
    };
    let base = DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP;
    spawn(base | CREATE_BREAKAWAY_FROM_JOB).or_else(|_| spawn(base))?;
    Ok(())
}

// ---------------------------------------------------------------- server (GUI)

/// Create the first pipe instance; fails if another GUI already owns the pipe.
/// Must be called inside the tokio runtime.
pub fn create_first() -> io::Result<NamedPipeServer> {
    create_instance(true)
}

fn create_instance(first: bool) -> io::Result<NamedPipeServer> {
    let name = pipe_name()?;
    let mut sa = winsec::user_only_security_attributes()?;
    // SAFETY: `sa` is a valid SECURITY_ATTRIBUTES whose descriptor lives for the process.
    unsafe {
        ServerOptions::new()
            .first_pipe_instance(first)
            .reject_remote_clients(true)
            .create_with_security_attributes_raw(
                name,
                (&mut sa as *mut windows_sys::Win32::Security::SECURITY_ATTRIBUTES).cast(),
            )
    }
}

pub async fn serve(mut server: NamedPipeServer, shared: Arc<Shared>) {
    loop {
        if server.connect().await.is_err() {
            match create_instance(false) {
                Ok(s) => server = s,
                Err(_) => tokio::time::sleep(Duration::from_millis(500)).await,
            }
            continue;
        }
        // Create the next instance before serving so clients never see a missing pipe.
        let next = loop {
            match create_instance(false) {
                Ok(s) => break s,
                Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
            }
        };
        let client = std::mem::replace(&mut server, next);
        tokio::spawn(handle_client(client, shared.clone()));
    }
}

async fn handle_client(client: NamedPipeServer, shared: Arc<Shared>) {
    let (r, mut w) = tokio::io::split(client);
    let mut reader = tokio::io::BufReader::new(r);
    loop {
        let mut line = String::new();
        let limit = MAX_MESSAGE as u64 + 2;
        let n = match (&mut reader).take(limit).read_line(&mut line).await {
            Ok(n) => n,
            Err(_) => break,
        };
        if n == 0 {
            break;
        }
        let oversized = !line.ends_with('\n') && n as u64 >= limit;
        let reply = if oversized {
            Response::error("message too large")
        } else if line.trim().is_empty() {
            continue;
        } else {
            handle_line(&shared, line.trim()).await
        };
        let mut out = reply.to_json();
        out.push('\n');
        if w.write_all(out.as_bytes()).await.is_err() || w.flush().await.is_err() || oversized {
            break;
        }
    }
}

pub async fn handle_line(shared: &Arc<Shared>, line: &str) -> Response {
    match serde_json::from_str::<Request>(line) {
        Err(e) => Response::error(format!("invalid message: {e}")),
        Ok(Request::Ping) => Response::pong(),
        Ok(Request::Show) => {
            shared.focus();
            Response::Ok
        }
        Ok(Request::Download(msg)) => {
            if let Err(e) = msg.check() {
                return Response::error(e);
            }
            match shared.add(msg.into(), None, true) {
                Ok(()) => Response::Ok,
                Err(e) => Response::error(e),
            }
        }
        Ok(Request::YtFormats { url }) => {
            if let Err(e) = validate_url(&url) {
                return Response::error(e);
            }
            let probe = shared.probe_yt(url);
            match tokio::time::timeout(YT_PROBE_TIMEOUT, probe).await {
                Ok(Ok(info)) => Response::YtFormats {
                    title: info.title,
                    formats: info.formats,
                },
                Ok(Err(e)) => Response::error(e),
                Err(_) => Response::error("yt-dlp took too long to answer"),
            }
        }
    }
}
