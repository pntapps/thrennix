fn main() {
    // Embed the app icon into thrennix.exe (Explorer, taskbar, Start Menu shortcut).
    #[cfg(windows)]
    {
        println!("cargo:rerun-if-changed=assets/thrennix.ico");
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/thrennix.ico");
        if let Err(e) = res.compile() {
            println!("cargo:warning=could not embed icon: {e}");
        }
    }
}
