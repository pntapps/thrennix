//! Native messaging host: stdin frames -> GUI pipe -> stdout frames.
//! Nothing but frames may ever be written to stdout.

use std::io;

use crate::ipc;
use crate::nm::{self, Frame};
use crate::protocol::{Response, VERSION};

pub fn run() -> i32 {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut input = stdin.lock();
    let mut output = stdout.lock();
    loop {
        let reply = match nm::read_frame(&mut input) {
            Ok(Some(Frame::Message(bytes))) => handle(&bytes),
            Ok(Some(Frame::TooLarge(_))) => Response::error("message too large").to_json(),
            Ok(None) | Err(_) => return 0, // stdin closed: browser disconnected
        };
        if nm::write_frame(&mut output, reply.as_bytes()).is_err() {
            return 0;
        }
    }
}

fn handle(bytes: &[u8]) -> String {
    let value: serde_json::Value = match serde_json::from_slice(bytes) {
        Ok(v @ serde_json::Value::Object(_)) => v,
        _ => return Response::error("invalid message").to_json(),
    };
    let kind = value
        .get("type")
        .and_then(|t| t.as_str())
        .unwrap_or_default()
        .to_string();
    let is_ping = kind == "ping";
    // yt-dlp probing takes a few seconds.
    let timeout = if kind == "ytFormats" {
        ipc::REPLY_TIMEOUT_YT
    } else {
        ipc::REPLY_TIMEOUT
    };
    let line = value.to_string(); // compact: guaranteed single line
                                  // A ping should not launch the app just to answer it.
    let pipe = match ipc::connect(!is_ping) {
        Ok(p) => p,
        Err(_) if is_ping => {
            return Response::Pong {
                version: VERSION.to_string(),
                running: Some(false),
            }
            .to_json()
        }
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            return Response::error("app pipe is owned by another user").to_json()
        }
        Err(_) => return Response::error("app not running").to_json(),
    };
    match ipc::request_timeout(pipe, line, timeout) {
        Ok(reply) if serde_json::from_str::<serde_json::Value>(&reply).is_ok() => reply,
        Ok(_) => Response::error("invalid reply from app").to_json(),
        Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {
            Response::error("timed out waiting for the app").to_json()
        }
        Err(_) => Response::error("app not running").to_json(),
    }
}

/// Keep the browser's stdio pipes from leaking into a launched GUI process (otherwise the
/// browser would never see EOF on our stdout after we exit).
pub fn make_std_handles_uninheritable() {
    use windows_sys::Win32::Foundation::{
        SetHandleInformation, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::System::Console::{
        GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
    };
    for id in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
        // SAFETY: plain Win32 calls on our own process' standard handles.
        unsafe {
            let h = GetStdHandle(id);
            if !h.is_null() && h != INVALID_HANDLE_VALUE {
                SetHandleInformation(h, HANDLE_FLAG_INHERIT, 0);
            }
        }
    }
}
