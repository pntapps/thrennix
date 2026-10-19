//! Publisher details and links, shared by the About window, status bar and Settings.

pub const PUBLISHER: &str = "PNT Apps";
pub const TAGLINE: &str = "A very fast, simple download manager for Windows";
pub const COPYRIGHT: &str = "© 2026 PNT Apps";
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub const WEBSITE: &str = "https://www.pntapps.com/";
pub const ABOUT: &str = "https://www.pntapps.com/about";
pub const PRIVACY: &str = "https://www.pntapps.com/privacy";
pub const TERMS: &str = "https://www.pntapps.com/terms";
pub const SOURCE: &str = "https://github.com/pntapps/thrennix";

/// (label, url) for the publisher links, in display order.
pub const LINKS: [(&str, &str); 4] = [
    ("Website", WEBSITE),
    ("About PNT Apps", ABOUT),
    ("Privacy Policy", PRIVACY),
    ("Terms of Service", TERMS),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_are_https() {
        for (_, url) in LINKS.iter().chain(&[("Source", SOURCE)]) {
            assert!(url.starts_with("https://"), "{url}");
        }
        assert!(!VERSION.is_empty());
    }
}
