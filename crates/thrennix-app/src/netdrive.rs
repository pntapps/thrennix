//! Network drives: SMB shares such as a USB disk on a router running Samba, or a NAS.
//! Windows' own SMB client does the work; we sign in to the share (deviceless, no drive
//! letter needed), and downloads saved under it wait for that sign-in off the UI thread.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

use serde::{Deserialize, Serialize};
use thrennix_core::{DownloadRequest, Error, Progress, Status};
use tokio::runtime::Handle;
use tokio::sync::watch;

use crate::backend::{Backend, Job, WaitFuture};
use crate::protocol::Kind;

/// A saved network folder. The password is DPAPI-encrypted in `settings.json`.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct NetDrive {
    /// `\\server\share` or `\\server\share\folder`.
    pub path: String,
    /// Empty: the current Windows account (or guest, if the server allows it).
    pub user: String,
    #[serde(with = "crate::secret::opt")]
    pub password: Option<String>,
}

impl NetDrive {
    pub fn dir(&self) -> PathBuf {
        PathBuf::from(&self.path)
    }

    /// `\\server\share`, the part Windows signs in to.
    pub fn share(&self) -> String {
        share_root(&self.path).unwrap_or_else(|| self.path.clone())
    }
}

/// Accepts `\\server\share\folder`, `//server/share`, `smb://server/share/folder` and
/// `server\share`; returns `\\server\share\folder`.
pub fn parse_address(input: &str) -> Result<String, String> {
    let s = input.trim();
    let s = match s.get(..6) {
        Some(p) if p.eq_ignore_ascii_case("smb://") => &s[6..],
        _ => s,
    };
    let parts: Vec<&str> = s
        .split(['\\', '/'])
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .collect();
    if parts.len() < 2 {
        return Err(r"Enter the server and the shared folder, e.g. \\192.168.1.1\usb".into());
    }
    if parts.iter().any(|p| *p == "." || *p == "..") || parts[0].contains(':') {
        return Err(r"That doesn't look like a network folder (\\server\share).".into());
    }
    Ok(format!(r"\\{}", parts.join(r"\")))
}

/// `\\server\share` of a UNC path (`\\?\UNC\server\share\…` too), else `None`.
pub fn share_root(path: &str) -> Option<String> {
    let rest = if let Some(r) = path.strip_prefix(r"\\?\UNC\") {
        r
    } else if path.starts_with(r"\\?\") || path.starts_with(r"\\.\") {
        return None;
    } else {
        path.strip_prefix(r"\\")?
    };
    let mut parts = rest.split('\\').filter(|p| !p.is_empty());
    let (server, share) = (parts.next()?, parts.next()?);
    Some(format!(r"\\{server}\{share}"))
}

/// The saved drive a download folder lives on.
pub fn drive_for<'a>(drives: &'a [NetDrive], dir: &Path) -> Option<&'a NetDrive> {
    let root = share_root(dir.to_str()?)?;
    drives
        .iter()
        .find(|d| d.share().eq_ignore_ascii_case(&root))
}

/// Sign in to the share. Already signed in counts as success.
pub fn connect(d: &NetDrive) -> Result<(), String> {
    use windows_sys::Win32::NetworkManagement::WNet::{
        WNetAddConnection2W, NETRESOURCEW, RESOURCETYPE_DISK,
    };
    let share = d.share();
    let wide = |s: &str| s.encode_utf16().chain([0]).collect::<Vec<u16>>();
    let remote = wide(&share);
    let user = Some(d.user.trim()).filter(|u| !u.is_empty()).map(wide);
    let password = d.password.as_deref().filter(|p| !p.is_empty()).map(wide);
    // SAFETY: zeroed NETRESOURCEW is valid (all null / 0); the strings outlive the call.
    let rc = unsafe {
        let mut nr: NETRESOURCEW = std::mem::zeroed();
        nr.dwType = RESOURCETYPE_DISK;
        nr.lpRemoteName = remote.as_ptr() as *mut u16;
        WNetAddConnection2W(
            &nr,
            password.as_ref().map_or(std::ptr::null(), |p| p.as_ptr()),
            user.as_ref().map_or(std::ptr::null(), |u| u.as_ptr()),
            0,
        )
    };
    match rc {
        0 => Ok(()),
        // ERROR_SESSION_CREDENTIAL_CONFLICT: Windows is already signed in to this server
        // (maybe as someone else). Fine if the share is reachable that way.
        1219 if Path::new(&share).is_dir() => Ok(()),
        _ => Err(describe(rc)),
    }
}

fn describe(rc: u32) -> String {
    let hint = match rc {
        53 | 1203 => "Can't find the server. Check the address, and that file sharing (Samba / SMB) is turned on in the router.",
        67 => r"The server answered, but has no shared folder by that name. Check the part after the server, e.g. \\192.168.1.1\usb.",
        86 | 1326 | 2202 => "Wrong user name or password.",
        1219 => "Windows is already signed in to this server with a different user name. Close any File Explorer windows on it, or sign out of Windows and back in, then try again.",
        1272 => "Windows blocks guest access to network drives. Set a user name and password for file sharing in the router, then enter them here.",
        5 => "Access denied. Check the user name and that this user may write to the shared folder.",
        1231 | 1232 | 64 => "The server can't be reached. Is it switched on and on the same network?",
        _ => "",
    };
    let os = std::io::Error::from_raw_os_error(rc as i32);
    if hint.is_empty() {
        format!("{os}")
    } else {
        format!("{hint} (error {rc})")
    }
}

/// Sign in and make sure the folder exists.
pub fn prepare(d: &NetDrive, dir: &Path) -> Result<(), String> {
    connect(d)?;
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))
}

/// Sign in, create the folder, and check we can write to it.
pub fn test(d: &NetDrive) -> Result<(), String> {
    let dir = d.dir();
    prepare(d, &dir)?;
    let probe = dir.join(format!(".thrennix-write-test-{}", std::process::id()));
    std::fs::write(&probe, b"thrennix")
        .map_err(|e| format!("Signed in, but can't write to {}: {e}", dir.display()))?;
    let _ = std::fs::remove_file(&probe);
    Ok(())
}

// ---------------------------------------------------------------- discovery

/// A shared folder found automatically.
#[derive(Debug, Clone, PartialEq)]
pub struct Found {
    /// `\\server\share`, or just `\\server` when its folders can't be listed without
    /// signing in.
    pub path: String,
    pub note: String,
}

impl Found {
    pub fn is_server_only(&self) -> bool {
        share_root(&self.path).is_none()
    }
}

/// Port of SMB (Windows file sharing / Samba).
const SMB_PORT: u16 = 445;
const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(600);

/// Shared folders Windows already knows (mapped drive letters, open connections), then
/// every device on the local network that answers on the SMB port, with the folders it
/// shares. The router is always among the devices, since all traffic goes through it.
/// Takes a few seconds: run it off the UI thread.
pub fn discover() -> Vec<Found> {
    let mut found = windows_connections();
    let hosts = lan_neighbors();
    let per_host: Vec<Vec<Found>> = std::thread::scope(|s| {
        let handles: Vec<_> = hosts
            .iter()
            .map(|ip| s.spawn(move || probe_host(&ip.to_string())))
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().unwrap_or_default())
            .collect()
    });
    for f in per_host.into_iter().flatten() {
        if !found.iter().any(|x| x.path.eq_ignore_ascii_case(&f.path)) {
            found.push(f);
        }
    }
    found
}

/// The folders a server shares, after signing in with these credentials (for servers
/// that only list them to known users).
pub fn list_shares(server: &str, user: &str, password: &str) -> Result<Vec<Found>, String> {
    let ipc = NetDrive {
        path: format!(r"\\{server}\IPC$"),
        user: user.trim().to_string(),
        password: Some(password.to_string()).filter(|p| !p.is_empty()),
    };
    connect(&ipc)?;
    let shares = shares_of(server).map_err(describe)?;
    if shares.is_empty() {
        return Err(format!(r"\\{server} doesn't share any folders."));
    }
    Ok(shares)
}

/// `\\server` (or `smb://server`) alone, without a shared folder.
pub fn parse_server(input: &str) -> Option<String> {
    let s = input.trim();
    let s = match s.get(..6) {
        Some(p) if p.eq_ignore_ascii_case("smb://") => &s[6..],
        _ => s,
    };
    let mut parts = s
        .split(['\\', '/'])
        .map(str::trim)
        .filter(|p| !p.is_empty());
    let server = parts.next()?;
    (parts.next().is_none() && !server.contains(':')).then(|| server.to_string())
}

/// What a device shares, if it answers on the SMB port. Lists with Windows' default
/// sign-in (current account, else guest).
fn probe_host(host: &str) -> Vec<Found> {
    use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
    let addr: Option<SocketAddr> = (host, SMB_PORT)
        .to_socket_addrs()
        .ok()
        .and_then(|mut a| a.next());
    let open = addr.is_some_and(|a| TcpStream::connect_timeout(&a, PROBE_TIMEOUT).is_ok());
    if !open {
        return Vec::new();
    }
    match shares_of(host) {
        Ok(v) if !v.is_empty() => v,
        _ => vec![Found {
            path: format!(r"\\{host}"),
            note: "file server — sign in to see its folders".into(),
        }],
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

/// SAFETY: `p` is null or a NUL-terminated UTF-16 string.
unsafe fn from_wide(p: *const u16) -> String {
    if p.is_null() {
        return String::new();
    }
    let mut n = 0;
    while *p.add(n) != 0 {
        n += 1;
    }
    String::from_utf16_lossy(std::slice::from_raw_parts(p, n))
}

/// Disk shares of a server (hidden `$` shares left out).
fn shares_of(server: &str) -> Result<Vec<Found>, u32> {
    use windows_sys::Win32::NetworkManagement::NetManagement::{
        NetApiBufferFree, MAX_PREFERRED_LENGTH,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        NetShareEnum, SHARE_INFO_1, STYPE_DISKTREE, STYPE_SPECIAL,
    };
    const ERROR_MORE_DATA: u32 = 234;
    let name = wide(&format!(r"\\{server}"));
    let (mut buf, mut read, mut total) = (std::ptr::null_mut(), 0u32, 0u32);
    // SAFETY: out-pointers are valid; the buffer is freed with NetApiBufferFree.
    let rc = unsafe {
        NetShareEnum(
            name.as_ptr(),
            1,
            &mut buf,
            MAX_PREFERRED_LENGTH,
            &mut read,
            &mut total,
            std::ptr::null_mut(),
        )
    };
    if rc != 0 && rc != ERROR_MORE_DATA {
        return Err(rc);
    }
    let mut out = Vec::new();
    if !buf.is_null() {
        // SAFETY: on success `buf` holds `read` SHARE_INFO_1 entries.
        let rows = unsafe { std::slice::from_raw_parts(buf as *const SHARE_INFO_1, read as usize) };
        for r in rows {
            let share = unsafe { from_wide(r.shi1_netname) };
            let disk = r.shi1_type & 0xff == STYPE_DISKTREE && r.shi1_type & STYPE_SPECIAL == 0;
            if disk && !share.is_empty() && !share.ends_with('$') {
                let remark = unsafe { from_wide(r.shi1_remark) };
                out.push(Found {
                    path: format!(r"\\{server}\{share}"),
                    note: if remark.trim().is_empty() {
                        format!("shared by {server}")
                    } else {
                        remark.trim().to_string()
                    },
                });
            }
        }
        unsafe { NetApiBufferFree(buf as _) };
    }
    Ok(out)
}

/// Network folders Windows is connected to or remembers (mapped drive letters).
fn windows_connections() -> Vec<Found> {
    use windows_sys::Win32::NetworkManagement::WNet::{
        WNetCloseEnum, WNetEnumResourceW, WNetOpenEnumW, NETRESOURCEW, RESOURCETYPE_DISK,
        RESOURCE_CONNECTED, RESOURCE_REMEMBERED,
    };
    const ERROR_MORE_DATA: u32 = 234;
    let mut out: Vec<Found> = Vec::new();
    for scope in [RESOURCE_CONNECTED, RESOURCE_REMEMBERED] {
        let mut h = std::ptr::null_mut();
        // SAFETY: valid out-pointer; the handle is closed below.
        if unsafe { WNetOpenEnumW(scope, RESOURCETYPE_DISK, 0, std::ptr::null(), &mut h) } != 0 {
            continue;
        }
        // u64s keep the buffer aligned for NETRESOURCEW.
        let mut buf = vec![0u64; 16 * 1024 / 8];
        loop {
            let mut count = u32::MAX;
            let mut size = (buf.len() * 8) as u32;
            // SAFETY: the buffer and size describe the same allocation.
            let rc =
                unsafe { WNetEnumResourceW(h, &mut count, buf.as_mut_ptr().cast(), &mut size) };
            if rc == ERROR_MORE_DATA {
                buf.resize((size as usize).div_ceil(8) + 1, 0);
                continue;
            }
            if rc != 0 {
                break; // ERROR_NO_MORE_ITEMS, or a real error
            }
            // SAFETY: on success the buffer starts with `count` NETRESOURCEW entries
            // whose strings live in the same buffer.
            let rows = unsafe {
                std::slice::from_raw_parts(buf.as_ptr() as *const NETRESOURCEW, count as usize)
            };
            for r in rows {
                let remote = unsafe { from_wide(r.lpRemoteName) };
                let local = unsafe { from_wide(r.lpLocalName) };
                if share_root(&remote).is_none()
                    || out.iter().any(|f| f.path.eq_ignore_ascii_case(&remote))
                {
                    continue;
                }
                out.push(Found {
                    note: if local.is_empty() {
                        "connected in Windows".into()
                    } else {
                        format!("drive {local} in Windows")
                    },
                    path: remote,
                });
            }
        }
        unsafe { WNetCloseEnum(h) };
    }
    out
}

/// Private IPv4 devices this PC has recently talked to (the neighbour / ARP table).
fn lan_neighbors() -> Vec<std::net::Ipv4Addr> {
    use windows_sys::Win32::NetworkManagement::IpHelper::{
        FreeMibTable, GetIpNetTable2, MIB_IPNET_TABLE2,
    };
    use windows_sys::Win32::Networking::WinSock::{NlnsIncomplete, NlnsUnreachable, AF_INET};
    let mut table: *mut MIB_IPNET_TABLE2 = std::ptr::null_mut();
    // SAFETY: valid out-pointer; freed with FreeMibTable.
    if unsafe { GetIpNetTable2(AF_INET, &mut table) } != 0 || table.is_null() {
        return Vec::new();
    }
    let mut ips = Vec::new();
    // SAFETY: the table holds NumEntries rows; only IPv4 was requested.
    unsafe {
        let rows =
            std::slice::from_raw_parts((*table).Table.as_ptr(), (*table).NumEntries as usize);
        for r in rows {
            if r.State == NlnsUnreachable || r.State == NlnsIncomplete {
                continue;
            }
            let ip = std::net::Ipv4Addr::from(r.Address.Ipv4.sin_addr.S_un.S_addr.to_ne_bytes());
            let last = ip.octets()[3];
            if ip.is_private() && last != 0 && last != 255 && !ips.contains(&ip) {
                ips.push(ip);
            }
        }
        FreeMibTable(table as _);
    }
    ips.truncate(256);
    ips
}

/// A download into a saved network drive. Signing in to a sleeping or unplugged server
/// can take many seconds, so it happens here, in the background; the real job starts
/// once the folder is reachable. Until then the row shows "Starting…".
pub struct NetJob {
    inner: OnceLock<Arc<dyn Job>>,
    paused: AtomicBool,
    /// `Some` once signing in is over.
    ready: watch::Receiver<Option<Result<(), String>>>,
}

impl NetJob {
    pub fn start(
        rt: &Handle,
        drive: NetDrive,
        backend: Arc<dyn Backend>,
        kind: Kind,
        req: DownloadRequest,
        format: Option<String>,
    ) -> Arc<Self> {
        let (tx, ready) = watch::channel(None);
        let job = Arc::new(NetJob {
            inner: OnceLock::new(),
            paused: AtomicBool::new(false),
            ready,
        });
        let me = job.clone();
        rt.spawn(async move {
            let dir = req.dest_dir.clone();
            let res = tokio::task::spawn_blocking(move || prepare(&drive, &dir))
                .await
                .unwrap_or_else(|e| Err(e.to_string()))
                .map_err(|e| format!("network drive: {e}"));
            if res.is_ok() && !me.paused.load(Ordering::SeqCst) {
                let inner = backend.start(kind, req, format);
                let _ = me.inner.set(inner.clone());
                // Paused while the job was being created.
                if me.paused.load(Ordering::SeqCst) {
                    inner.pause();
                }
            }
            let _ = tx.send(Some(res));
        });
        job
    }
}

impl Job for NetJob {
    fn progress(&self) -> Progress {
        match self.inner.get() {
            Some(j) => j.progress(),
            None => Progress {
                status: Status::Probing,
                file_name: String::new(),
                path: PathBuf::new(),
                downloaded: 0,
                total: None,
                speed_bps: 0.0,
                connections: 0,
                resumable: false,
                segments: Vec::new(),
            },
        }
    }

    fn pause(&self) {
        self.paused.store(true, Ordering::SeqCst);
        if let Some(j) = self.inner.get() {
            j.pause();
        }
    }

    fn wait(&self) -> WaitFuture<'_> {
        Box::pin(async move {
            let mut rx = self.ready.clone();
            let res = match rx.wait_for(Option::is_some).await {
                Ok(r) => r.clone().unwrap_or(Ok(())),
                Err(_) => Err("network drive: cancelled".into()),
            };
            match self.inner.get() {
                Some(j) => j.wait().await,
                None if self.paused.load(Ordering::SeqCst) => Err(Error::Paused),
                None => Err(Error::Other(res.err().unwrap_or_default())),
            }
        })
    }

    fn work_dir(&self) -> Option<PathBuf> {
        self.inner.get().and_then(|j| j.work_dir())
    }

    fn output_path(&self) -> Option<PathBuf> {
        self.inner.get().and_then(|j| j.output_path())
    }

    fn is_muxing(&self) -> bool {
        self.inner.get().is_some_and(|j| j.is_muxing())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses() {
        for (input, want) in [
            (r"\\192.168.1.1\usb", r"\\192.168.1.1\usb"),
            (r"\\router\usb\Downloads\", r"\\router\usb\Downloads"),
            (
                "//192.168.1.1/usb/Downloads",
                r"\\192.168.1.1\usb\Downloads",
            ),
            ("smb://192.168.1.1/usb/Movies", r"\\192.168.1.1\usb\Movies"),
            ("SMB://nas/share", r"\\nas\share"),
            (r"  router\usb  ", r"\\router\usb"),
        ] {
            assert_eq!(parse_address(input).as_deref(), Ok(want), "{input}");
        }
        for bad in [
            "",
            r"\\router",
            "smb://router/",
            r"C:\Downloads",
            r"\\r\s\..\x",
        ] {
            assert!(parse_address(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn server_only() {
        assert_eq!(
            parse_server(r"\\192.168.1.1").as_deref(),
            Some("192.168.1.1")
        );
        assert_eq!(parse_server(r"\\router\").as_deref(), Some("router"));
        assert_eq!(parse_server("smb://nas").as_deref(), Some("nas"));
        assert_eq!(parse_server(r"\\router\usb"), None);
        assert_eq!(parse_server(r"C:\"), None);
        assert_eq!(parse_server(""), None);
    }

    #[test]
    fn roots_and_lookup() {
        assert_eq!(
            share_root(r"\\router\usb\a\b").as_deref(),
            Some(r"\\router\usb")
        );
        assert_eq!(
            share_root(r"\\?\UNC\router\usb\a").as_deref(),
            Some(r"\\router\usb")
        );
        assert_eq!(share_root(r"C:\Downloads"), None);
        assert_eq!(share_root(r"\\?\C:\Downloads"), None);
        assert_eq!(share_root(r"\\router"), None);

        let drives = vec![NetDrive {
            path: r"\\Router\USB\Downloads".into(),
            ..Default::default()
        }];
        assert!(drive_for(&drives, Path::new(r"\\router\usb\Movies")).is_some());
        assert!(drive_for(&drives, Path::new(r"\\router\other")).is_none());
        assert!(drive_for(&drives, Path::new(r"C:\Downloads")).is_none());
    }
}
