//! Human-readable sizes, speeds and durations.

pub fn bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else if v >= 100.0 {
        format!("{v:.0} {}", UNITS[i])
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

pub fn speed(bps: f64) -> String {
    format!("{}/s", bytes(bps.max(0.0) as u64))
}

pub fn eta(secs: f64) -> String {
    if !secs.is_finite() || secs < 0.0 {
        return "--".into();
    }
    let s = secs.round() as u64;
    match s {
        0..=59 => format!("{s}s"),
        60..=3599 => format!("{}m {:02}s", s / 60, s % 60),
        _ if s < 100 * 3600 => format!("{}h {:02}m", s / 3600, (s % 3600) / 60),
        _ => "> 99h".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formatting() {
        assert_eq!(bytes(512), "512 B");
        assert_eq!(bytes(1536), "1.5 KB");
        assert_eq!(bytes(250 * 1024 * 1024), "250 MB");
        assert_eq!(speed(10.0 * 1024.0 * 1024.0), "10.0 MB/s");
        assert_eq!(eta(5.2), "5s");
        assert_eq!(eta(125.0), "2m 05s");
        assert_eq!(eta(3.0 * 3600.0 + 120.0), "3h 02m");
        assert_eq!(eta(f64::INFINITY), "--");
    }
}
