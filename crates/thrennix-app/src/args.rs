//! Command-line mode detection.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Gui { demo: bool },
    NativeHost,
    Register,
    Unregister,
}

/// `args` excludes the program name.
pub fn detect(args: &[String]) -> Mode {
    let Some(first) = args.first() else {
        return Mode::Gui { demo: false };
    };
    if first.starts_with("chrome-extension://") || first == "--native-host" {
        return Mode::NativeHost;
    }
    // Firefox: thrennix.exe <path to manifest .json> <extension id>
    if first.to_ascii_lowercase().ends_with(".json") && args.len() >= 2 {
        return Mode::NativeHost;
    }
    match first.as_str() {
        "--register" => Mode::Register,
        "--unregister" => Mode::Unregister,
        "--demo" => Mode::Gui { demo: true },
        _ => Mode::Gui { demo: false },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(a: &[&str]) -> Mode {
        detect(&a.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn modes() {
        assert_eq!(d(&[]), Mode::Gui { demo: false });
        assert_eq!(d(&["--demo"]), Mode::Gui { demo: true });
        assert_eq!(d(&["chrome-extension://abc/"]), Mode::NativeHost);
        assert_eq!(
            d(&["chrome-extension://abc/", "--parent-window=1234"]),
            Mode::NativeHost
        );
        assert_eq!(d(&["--native-host"]), Mode::NativeHost);
        assert_eq!(
            d(&[
                "C:\\Users\\x\\AppData\\Local\\Thrennix\\com.thrennix.host.firefox.json",
                "thrennix@thrennix.app"
            ]),
            Mode::NativeHost
        );
        // A lone .json argument is not Firefox.
        assert_eq!(d(&["foo.JSON"]), Mode::Gui { demo: false });
        assert_eq!(d(&["--register"]), Mode::Register);
        assert_eq!(d(&["--unregister"]), Mode::Unregister);
        assert_eq!(d(&["--something"]), Mode::Gui { demo: false });
    }
}
