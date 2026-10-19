//! Native messaging host registration (manifests + HKCU registry keys).

use std::io;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};
use winreg::enums::HKEY_CURRENT_USER;
use winreg::RegKey;

pub const HOST_NAME: &str = "com.thrennix.host";
pub const FIREFOX_ID: &str = "thrennix@thrennix.app";
const DESCRIPTION: &str = "Thrennix download manager";
const CHROME_FILE: &str = "com.thrennix.host.chrome.json";
const FIREFOX_FILE: &str = "com.thrennix.host.firefox.json";

const CHROMIUM_KEYS: [&str; 4] = [
    r"Software\Google\Chrome\NativeMessagingHosts\com.thrennix.host",
    r"Software\Microsoft\Edge\NativeMessagingHosts\com.thrennix.host",
    r"Software\Chromium\NativeMessagingHosts\com.thrennix.host",
    r"Software\BraveSoftware\Brave-Browser\NativeMessagingHosts\com.thrennix.host",
];
const FIREFOX_KEY: &str = r"Software\Mozilla\NativeMessagingHosts\com.thrennix.host";

pub fn chrome_extension_id() -> &'static str {
    include_str!("../../../extension/extension-id.txt").trim()
}

pub fn chrome_manifest(exe: &Path, extension_id: &str) -> Value {
    json!({
        "name": HOST_NAME,
        "description": DESCRIPTION,
        "path": exe.to_string_lossy(),
        "type": "stdio",
        "allowed_origins": [format!("chrome-extension://{extension_id}/")],
    })
}

pub fn firefox_manifest(exe: &Path) -> Value {
    json!({
        "name": HOST_NAME,
        "description": DESCRIPTION,
        "path": exe.to_string_lossy(),
        "type": "stdio",
        "allowed_extensions": [FIREFOX_ID],
    })
}

/// `%LOCALAPPDATA%\Thrennix`
pub fn manifest_dir() -> io::Result<PathBuf> {
    directories::BaseDirs::new()
        .map(|b| b.data_local_dir().join("Thrennix"))
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no local app data dir"))
}

pub fn register() -> io::Result<()> {
    let exe = std::env::current_exe()?;
    let dir = manifest_dir()?;
    std::fs::create_dir_all(&dir)?;
    let chrome = dir.join(CHROME_FILE);
    let firefox = dir.join(FIREFOX_FILE);
    crate::state::write_atomic(
        &chrome,
        serde_json::to_string_pretty(&chrome_manifest(&exe, chrome_extension_id()))?.as_bytes(),
    )?;
    crate::state::write_atomic(
        &firefox,
        serde_json::to_string_pretty(&firefox_manifest(&exe))?.as_bytes(),
    )?;
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    for key in CHROMIUM_KEYS {
        let (k, _) = hkcu.create_subkey(key)?;
        k.set_value("", &chrome.to_string_lossy().as_ref())?;
    }
    let (k, _) = hkcu.create_subkey(FIREFOX_KEY)?;
    k.set_value("", &firefox.to_string_lossy().as_ref())?;
    Ok(())
}

pub fn unregister() -> io::Result<()> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    for key in CHROMIUM_KEYS.iter().chain(std::iter::once(&FIREFOX_KEY)) {
        match hkcu.delete_subkey_all(key) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    let dir = manifest_dir()?;
    for f in [CHROME_FILE, FIREFOX_FILE] {
        match std::fs::remove_file(dir.join(f)) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chrome_manifest_content() {
        let exe = Path::new(r"C:\Program Files\Thrennix\thrennix.exe");
        let m = chrome_manifest(exe, "abcdef");
        assert_eq!(
            m,
            json!({
                "name": "com.thrennix.host",
                "description": "Thrennix download manager",
                "path": r"C:\Program Files\Thrennix\thrennix.exe",
                "type": "stdio",
                "allowed_origins": ["chrome-extension://abcdef/"],
            })
        );
        // Serialized JSON escapes backslashes correctly.
        let s = serde_json::to_string(&m).unwrap();
        assert!(s.contains(r#""path":"C:\\Program Files\\Thrennix\\thrennix.exe""#));
    }

    #[test]
    fn firefox_manifest_content() {
        let m = firefox_manifest(Path::new(r"C:\v\thrennix.exe"));
        assert_eq!(m["name"], "com.thrennix.host");
        assert_eq!(m["type"], "stdio");
        assert_eq!(m["path"], r"C:\v\thrennix.exe");
        assert_eq!(m["allowed_extensions"], json!(["thrennix@thrennix.app"]));
        assert!(m.get("allowed_origins").is_none());
    }

    #[test]
    fn extension_id_is_trimmed() {
        let id = chrome_extension_id();
        assert!(!id.is_empty());
        assert_eq!(id, id.trim());
    }
}
