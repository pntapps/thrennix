//! Dedicated disk-writer thread: positional writes into the `.part` file, coalescing
//! adjacent chunks, fed by a bounded channel (backpressure when the disk is slower).

use std::fs::{File, OpenOptions};
use std::io;
use std::path::Path;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use tokio::sync::{mpsc, oneshot};

use crate::segment::Segment;

pub enum WriteCmd {
    Data {
        seg: Arc<Segment>,
        offset: u64,
        data: Bytes,
    },
    Flush(oneshot::Sender<io::Result<()>>),
}

/// First write error, shared with the controller.
pub type WriterError = Arc<Mutex<Option<(io::ErrorKind, String)>>>;

/// Open (or create) the `.part` file. When `total` is known the file is marked sparse on
/// Windows (so NTFS does not zero-fill when writing at high offsets) and sized up front.
pub fn open_part(path: &Path, create_new: bool, total: Option<u64>) -> io::Result<File> {
    let mut o = OpenOptions::new();
    o.read(true).write(true);
    if create_new {
        o.create_new(true);
    } else {
        o.create(true);
    }
    let f = o.open(path)?;
    if let Some(t) = total {
        if t > 0 {
            if let Err(e) = check_volume_limit(&f, t) {
                let empty = f.metadata().map(|m| m.len() == 0).unwrap_or(false);
                drop(f);
                if create_new || empty {
                    let _ = std::fs::remove_file(path);
                }
                return Err(e);
            }
            if set_sparse(&f) {
                f.set_len(t)?;
            } else {
                // FAT32/exFAT: no sparse files, and pre-extending would zero-fill the whole
                // file synchronously. Let the file grow as data arrives instead.
                tracing::debug!("volume does not support sparse files; not pre-allocating");
            }
        }
    }
    Ok(f)
}

/// Error message used when the destination volume cannot hold a file this large.
pub const FAT32_TOO_LARGE: &str =
    "the destination drive is formatted FAT32, which cannot store files larger than 4 GiB; choose an NTFS or exFAT drive";

#[cfg(windows)]
fn check_volume_limit(f: &File, total: u64) -> io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::GetVolumeInformationByHandleW;
    if total <= u32::MAX as u64 {
        return Ok(());
    }
    let mut fs_name = [0u16; 64];
    // SAFETY: valid handle; the buffer length passed matches the buffer.
    let ok = unsafe {
        GetVolumeInformationByHandleW(
            f.as_raw_handle() as _,
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            fs_name.as_mut_ptr(),
            fs_name.len() as u32,
        )
    };
    if ok == 0 {
        return Ok(());
    }
    let n = fs_name
        .iter()
        .position(|&c| c == 0)
        .unwrap_or(fs_name.len());
    let name = String::from_utf16_lossy(&fs_name[..n]).to_ascii_uppercase();
    if name.starts_with("FAT") && name != "EXFAT" {
        return Err(io::Error::new(io::ErrorKind::FileTooLarge, FAT32_TOO_LARGE));
    }
    Ok(())
}

#[cfg(not(windows))]
fn check_volume_limit(_f: &File, _total: u64) -> io::Result<()> {
    Ok(())
}

/// Returns false when the volume refused (FAT32/exFAT).
#[cfg(windows)]
fn set_sparse(f: &File) -> bool {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::System::Ioctl::FSCTL_SET_SPARSE;
    use windows_sys::Win32::System::IO::DeviceIoControl;
    let mut returned: u32 = 0;
    // SAFETY: valid open handle; no input/output buffers (NULL input = set sparse).
    let ok = unsafe {
        DeviceIoControl(
            f.as_raw_handle() as _,
            FSCTL_SET_SPARSE,
            std::ptr::null(),
            0,
            std::ptr::null_mut(),
            0,
            &mut returned,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        tracing::debug!("FSCTL_SET_SPARSE failed: {}", io::Error::last_os_error());
        return false;
    }
    true
}

#[cfg(not(windows))]
fn set_sparse(_f: &File) -> bool {
    true
}

#[cfg(windows)]
fn write_at(f: &File, mut buf: &[u8], mut off: u64) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        match f.seek_write(buf, off) {
            Ok(0) => return Err(io::Error::new(io::ErrorKind::WriteZero, "wrote 0 bytes")),
            Ok(n) => {
                buf = &buf[n..];
                off += n as u64;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

#[cfg(unix)]
fn write_at(f: &File, buf: &[u8], off: u64) -> io::Result<()> {
    use std::os::unix::fs::FileExt;
    f.write_all_at(buf, off)
}

#[cfg(not(any(windows, unix)))]
fn write_at(f: &File, buf: &[u8], off: u64) -> io::Result<()> {
    use std::io::{Seek, SeekFrom, Write};
    let mut f = f;
    f.seek(SeekFrom::Start(off))?;
    f.write_all(buf)
}

struct Batch {
    offset: u64,
    len: usize,
    parts: Vec<Bytes>,
    /// (segment, written-upto) to publish once the batch hits the disk.
    marks: Vec<(Arc<Segment>, u64)>,
    scratch: Vec<u8>,
}

impl Batch {
    fn flush(&mut self, f: &File) -> io::Result<()> {
        if self.parts.is_empty() {
            return Ok(());
        }
        let res = if self.parts.len() == 1 {
            write_at(f, &self.parts[0], self.offset)
        } else {
            self.scratch.clear();
            self.scratch.reserve(self.len);
            for p in &self.parts {
                self.scratch.extend_from_slice(p);
            }
            write_at(f, &self.scratch, self.offset)
        };
        self.parts.clear();
        self.len = 0;
        if res.is_ok() {
            for (seg, upto) in self.marks.drain(..) {
                seg.mark_written(upto);
            }
        } else {
            self.marks.clear();
        }
        res
    }
}

/// Runs on a blocking thread until every sender is dropped.
pub fn writer_loop(
    file: File,
    mut rx: mpsc::Receiver<WriteCmd>,
    batch_size: usize,
    err: WriterError,
) {
    let mut b = Batch {
        offset: 0,
        len: 0,
        parts: Vec::new(),
        marks: Vec::new(),
        scratch: Vec::new(),
    };
    let mut failed: Option<io::Error> = None;
    let mut next = rx.blocking_recv();
    while let Some(cmd) = next.take() {
        match cmd {
            WriteCmd::Data { seg, offset, data } => {
                if failed.is_none() {
                    let adjacent = !b.parts.is_empty() && b.offset + b.len as u64 == offset;
                    if !b.parts.is_empty() && (!adjacent || b.len + data.len() > batch_size) {
                        if let Err(e) = b.flush(&file) {
                            failed = Some(e);
                        }
                    }
                    if failed.is_none() {
                        if b.parts.is_empty() {
                            b.offset = offset;
                        }
                        let upto = offset + data.len() as u64;
                        b.len += data.len();
                        b.parts.push(data);
                        b.marks.push((seg, upto));
                    }
                }
            }
            WriteCmd::Flush(reply) => {
                let r = match &failed {
                    Some(e) => Err(io::Error::new(e.kind(), e.to_string())),
                    None => match b.flush(&file) {
                        Ok(()) => Ok(()),
                        Err(e) => {
                            let r = io::Error::new(e.kind(), e.to_string());
                            failed = Some(e);
                            Err(r)
                        }
                    },
                };
                let _ = reply.send(r);
            }
        }
        if let Some(e) = &failed {
            if let Ok(mut g) = err.lock() {
                if g.is_none() {
                    tracing::error!("disk write failed: {e}");
                    *g = Some((e.kind(), e.to_string()));
                }
            }
        }
        // Coalesce whatever is already queued; when the queue is empty, hit the disk.
        next = match rx.try_recv() {
            Ok(c) => Some(c),
            Err(mpsc::error::TryRecvError::Empty) => {
                if failed.is_none() {
                    if let Err(e) = b.flush(&file) {
                        if let Ok(mut g) = err.lock() {
                            g.get_or_insert((e.kind(), e.to_string()));
                        }
                        failed = Some(e);
                    }
                }
                rx.blocking_recv()
            }
            Err(mpsc::error::TryRecvError::Disconnected) => None,
        };
    }
    if failed.is_none() {
        if let Err(e) = b.flush(&file) {
            if let Ok(mut g) = err.lock() {
                g.get_or_insert((e.kind(), e.to_string()));
            }
        }
    }
}
