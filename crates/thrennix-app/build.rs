fn main() {
    // Embed the app icon and file properties into thrennix.exe (Explorer, taskbar,
    // Start Menu shortcut, Properties > Details). Versions come from Cargo.
    #[cfg(windows)]
    {
        println!("cargo:rerun-if-changed=assets/thrennix.ico");
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/thrennix.ico")
            .set("CompanyName", "PNT Apps")
            .set("ProductName", "Thrennix")
            .set("FileDescription", "Thrennix Download Manager")
            .set("LegalCopyright", "© 2026 PNT Apps")
            .set("OriginalFilename", "thrennix.exe")
            .set("InternalName", "thrennix");
        if let Err(e) = res.compile() {
            println!("cargo:warning=could not embed resources: {e}");
        }
    }
}
