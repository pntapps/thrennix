//! Small Win32 security helpers: user SIDs, a user-only pipe security descriptor, and DPAPI.

use std::ffi::c_void;
use std::io;
use std::os::windows::io::AsRawHandle;
use std::sync::OnceLock;

use windows_sys::Win32::Foundation::{CloseHandle, LocalFree, HANDLE};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::Cryptography::{
    CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
};
use windows_sys::Win32::Security::{
    EqualSid, GetTokenInformation, TokenUser, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES,
    TOKEN_QUERY, TOKEN_USER,
};
use windows_sys::Win32::System::Pipes::GetNamedPipeServerProcessId;
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
};

/// Owned `TOKEN_USER` buffer for some process.
struct TokenUserBuf(Vec<u64>); // u64 for alignment

impl TokenUserBuf {
    fn sid(&self) -> *mut c_void {
        // SAFETY: the buffer was filled by GetTokenInformation(TokenUser).
        unsafe { (*(self.0.as_ptr() as *const TOKEN_USER)).User.Sid }
    }
}

struct Handle(HANDLE);
impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: we own this handle.
        unsafe { CloseHandle(self.0) };
    }
}

/// SAFETY: `process` must be a valid process handle with query rights.
unsafe fn process_user(process: HANDLE) -> io::Result<TokenUserBuf> {
    let mut token: HANDLE = std::ptr::null_mut();
    if OpenProcessToken(process, TOKEN_QUERY, &mut token) == 0 {
        return Err(io::Error::last_os_error());
    }
    let token = Handle(token);
    let mut len = 0u32;
    GetTokenInformation(token.0, TokenUser, std::ptr::null_mut(), 0, &mut len);
    if len == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut buf = vec![0u64; (len as usize).div_ceil(8)];
    if GetTokenInformation(token.0, TokenUser, buf.as_mut_ptr().cast(), len, &mut len) == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(TokenUserBuf(buf))
}

fn current_user() -> io::Result<&'static TokenUserBuf> {
    static USER: OnceLock<Option<TokenUserBuf>> = OnceLock::new();
    // SAFETY: GetCurrentProcess returns a pseudo handle that is always valid.
    USER.get_or_init(|| unsafe { process_user(GetCurrentProcess()).ok() })
        .as_ref()
        .ok_or_else(|| io::Error::other("cannot read the current user's SID"))
}

/// The current user's SID as a string, e.g. `S-1-5-21-...`.
pub fn current_user_sid() -> io::Result<&'static str> {
    static SID: OnceLock<Option<String>> = OnceLock::new();
    SID.get_or_init(|| {
        let user = current_user().ok()?;
        let mut p: *mut u16 = std::ptr::null_mut();
        // SAFETY: valid SID from our token; the returned string is freed with LocalFree.
        unsafe {
            if ConvertSidToStringSidW(user.sid(), &mut p) == 0 {
                return None;
            }
            let len = (0..).take_while(|&i| *p.add(i) != 0).count();
            let s = String::from_utf16_lossy(std::slice::from_raw_parts(p, len));
            LocalFree(p.cast());
            Some(s)
        }
    })
    .as_deref()
    .ok_or_else(|| io::Error::other("cannot format the current user's SID"))
}

/// Security attributes allowing only the current user (`D:P(A;;GA;;;<SID>)`).
/// The descriptor is created once and lives for the whole process.
pub fn user_only_security_attributes() -> io::Result<SECURITY_ATTRIBUTES> {
    static SD: OnceLock<Option<usize>> = OnceLock::new();
    let sd = SD
        .get_or_init(|| {
            let sid = current_user_sid().ok()?;
            let sddl: Vec<u16> = format!("D:P(A;;GA;;;{sid})")
                .encode_utf16()
                .chain([0])
                .collect();
            let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
            // SAFETY: valid NUL-terminated SDDL; the descriptor is intentionally never freed.
            let ok = unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    sddl.as_ptr(),
                    SDDL_REVISION_1,
                    &mut sd,
                    std::ptr::null_mut(),
                )
            };
            (ok != 0).then_some(sd as usize)
        })
        .ok_or_else(|| io::Error::other("cannot build pipe security descriptor"))?;
    Ok(SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: sd as *mut c_void,
        bInheritHandle: 0,
    })
}

/// Check that the server end of a connected pipe runs as the current user.
pub fn verify_pipe_server_is_current_user(pipe: &std::fs::File) -> io::Result<()> {
    let me = current_user()?;
    let mut pid = 0u32;
    // SAFETY: Win32 calls on a valid pipe handle / process handles we own.
    unsafe {
        if GetNamedPipeServerProcessId(pipe.as_raw_handle() as HANDLE, &mut pid) == 0 {
            return Err(io::Error::last_os_error());
        }
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if process.is_null() {
            return Err(io::Error::last_os_error());
        }
        let process = Handle(process);
        let other = process_user(process.0)?;
        if EqualSid(me.sid(), other.sid()) == 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "pipe server belongs to another user",
            ));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------- DPAPI

fn dpapi(data: &[u8], protect: bool) -> Option<Vec<u8>> {
    let input = CRYPT_INTEGER_BLOB {
        cbData: data.len() as u32,
        pbData: data.as_ptr() as *mut u8,
    };
    let mut out = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: std::ptr::null_mut(),
    };
    // SAFETY: valid input blob; output is allocated by the system and freed with LocalFree.
    unsafe {
        let ok = if protect {
            CryptProtectData(
                &input,
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut out,
            )
        } else {
            CryptUnprotectData(
                &input,
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut out,
            )
        };
        if ok == 0 {
            return None;
        }
        let v = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
        LocalFree(out.pbData.cast());
        Some(v)
    }
}

/// Encrypt for the current user (DPAPI user scope).
pub fn protect(data: &[u8]) -> Option<Vec<u8>> {
    dpapi(data, true)
}

pub fn unprotect(data: &[u8]) -> Option<Vec<u8>> {
    dpapi(data, false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sid_and_dpapi() {
        assert!(current_user_sid().unwrap().starts_with("S-1-"));
        assert!(user_only_security_attributes().is_ok());
        let enc = protect(b"a=1; b=2").unwrap();
        assert_ne!(enc, b"a=1; b=2");
        assert_eq!(unprotect(&enc).unwrap(), b"a=1; b=2");
        assert!(unprotect(b"garbage").is_none());
    }
}
