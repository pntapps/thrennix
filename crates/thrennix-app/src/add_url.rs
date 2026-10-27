//! The "Add download" dialog, including the YouTube quality picker.
//!
//! For a YouTube URL the dialog runs a yt-dlp probe in the background ~600 ms after the
//! URL stops changing and lets the user pick a quality (same wording as the browser
//! extension). With the "always download the highest quality" setting it skips the
//! picker and uses the first (best) quality.

use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use eframe::egui::{self, vec2, Key, RichText, TextEdit};

use crate::backend::YtInfoOut;
use crate::fmt;
use crate::protocol::{sanitize_file_name, validate_url, Kind, YtFormat};
use crate::state::{NewDownload, Shared};

/// Wait this long after the last edit before probing a YouTube URL.
const DEBOUNCE: Duration = Duration::from_millis(600);
const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;
const INSTALL_CMD: &str = "winget install yt-dlp.yt-dlp";

type Slot = Arc<Mutex<Option<Result<YtInfoOut, String>>>>;

/// YouTube probe state for the URL currently in the dialog.
enum Yt {
    /// Not probed (yet).
    Idle,
    Probing {
        url: String,
        slot: Slot,
    },
    Ready {
        url: String,
        title: String,
        formats: Vec<YtFormat>,
        pick: usize,
    },
    Failed {
        url: String,
        msg: String,
        missing: bool,
    },
}

impl Yt {
    fn url(&self) -> Option<&str> {
        match self {
            Yt::Idle => None,
            Yt::Probing { url, .. } | Yt::Ready { url, .. } | Yt::Failed { url, .. } => Some(url),
        }
    }
}

pub struct AddDialog {
    url: String,
    dir: PathBuf,
    err: Option<String>,
    focus: bool,
    /// URL as of the last frame, to notice edits.
    seen_url: String,
    changed_at: Instant,
    yt: Yt,
    /// "Always highest quality": Start was pressed before the probe finished.
    start_when_ready: bool,
}

impl AddDialog {
    pub fn new(url: String, dir: PathBuf) -> Self {
        let now = Instant::now();
        Self {
            seen_url: url.trim().to_string(),
            url,
            dir,
            err: None,
            focus: true,
            // A prefilled URL (clipboard) is probed right away.
            changed_at: now.checked_sub(DEBOUNCE).unwrap_or(now),
            yt: Yt::Idle,
            start_when_ready: false,
        }
    }

    /// Reset the probe when the URL text changes.
    fn track_url(&mut self) {
        let url = self.url.trim();
        if url != self.seen_url {
            self.seen_url = url.to_string();
            self.changed_at = Instant::now();
            self.err = None;
        }
        if self.yt.url().is_some_and(|u| u != url) {
            self.yt = Yt::Idle;
            self.start_when_ready = false;
        }
    }

    /// Pick up a finished probe.
    fn poll(&mut self) {
        let Yt::Probing { url, slot } = &self.yt else {
            return;
        };
        let Some(res) = slot.lock().unwrap_or_else(|e| e.into_inner()).take() else {
            return;
        };
        let url = url.clone();
        self.yt = match res {
            Ok(info) if !info.formats.is_empty() => {
                let formats = order_formats(info.formats);
                Yt::Ready {
                    url,
                    title: info.title,
                    pick: default_pick(&formats),
                    formats,
                }
            }
            Ok(_) => Yt::Failed {
                url,
                msg: "No downloadable qualities found.".into(),
                missing: false,
            },
            Err(msg) => Yt::Failed {
                missing: is_missing_ytdlp(&msg),
                url,
                msg,
            },
        };
    }

    fn probe(&mut self, ctx: &egui::Context, shared: &Arc<Shared>, url: &str) {
        let slot: Slot = Arc::new(Mutex::new(None));
        let fut = shared.probe_yt(url.to_string());
        let (out, ctx) = (slot.clone(), ctx.clone());
        shared.rt.spawn(async move {
            let res = fut.await;
            *out.lock().unwrap_or_else(|e| e.into_inner()) = Some(res);
            ctx.request_repaint();
        });
        self.yt = Yt::Probing {
            url: url.to_string(),
            slot,
        };
    }
}

/// YouTube video page hosts (YouTube Music is left to the default best format).
pub fn is_youtube_video_url(url: &str) -> bool {
    let Ok(u) = url::Url::parse(url.trim()) else {
        return false;
    };
    if !matches!(u.scheme(), "http" | "https") {
        return false;
    }
    let host = u.host_str().unwrap_or_default().to_ascii_lowercase();
    let h = host.strip_prefix("www.").unwrap_or(&host);
    let h = h.strip_prefix("m.").unwrap_or(h);
    matches!(h, "youtube.com" | "youtu.be" | "youtube-nocookie.com")
}

fn is_missing_ytdlp(msg: &str) -> bool {
    msg.to_ascii_lowercase().contains("yt-dlp not found")
}

/// Video qualities first (as listed), then audio — same order as the extension.
pub fn order_formats(formats: Vec<YtFormat>) -> Vec<YtFormat> {
    let (video, audio): (Vec<_>, Vec<_>) = formats
        .into_iter()
        .partition(|f| f.height.is_some_and(|h| h > 0));
    video.into_iter().chain(audio).collect()
}

/// Default selection: the highest video quality (else the first entry).
pub fn default_pick(formats: &[YtFormat]) -> usize {
    formats
        .iter()
        .enumerate()
        .filter_map(|(i, f)| f.height.filter(|h| *h > 0).map(|h| (i, h)))
        .max_by(|a, b| a.1.cmp(&b.1).then(b.0.cmp(&a.0)))
        .map_or(0, |(i, _)| i)
}

/// `MP4 file, quality 1080p HD (≈ 118 MB)` / `M4A audio file (≈ 4.0 MB)` — the wording of
/// the extension's `ytFormatLabel`.
pub fn format_label(f: &YtFormat) -> String {
    let parts: Vec<&str> = f.label.split('·').map(str::trim).collect();
    let s = match f.height.filter(|h| *h > 0) {
        Some(h) => {
            let container = match parts.as_slice() {
                [c, _, ..] if !c.is_empty() && !c.to_ascii_lowercase().contains("audio") => {
                    c.to_uppercase()
                }
                _ => "MP4".into(),
            };
            // Keep fps / 4K from yt-dlp's label ("2160p60 4K"), else build it.
            let quality = parts
                .get(1)
                .filter(|q| q.starts_with(&format!("{h}p")))
                .map(|q| q.to_string())
                .unwrap_or_else(|| format!("{h}p{}", if h >= 720 { " HD" } else { "" }));
            format!("{container} file, quality {quality}")
        }
        None => match parts.as_slice() {
            [kind, .., codec] if kind.eq_ignore_ascii_case("audio") => {
                format!("{} audio file", codec.to_uppercase())
            }
            [container, rest] => {
                format!(
                    "{} file, {} quality",
                    container.to_uppercase(),
                    rest.to_lowercase()
                )
            }
            _ if f.label.trim().is_empty() => "Audio file".into(),
            _ => f.label.trim().to_string(),
        },
    };
    match f.filesize.filter(|n| *n > 0) {
        Some(n) => format!("{s} (≈ {})", fmt::bytes(n)),
        None => s,
    }
}

fn run_in_console(cmd: &str) -> Result<(), String> {
    Command::new("cmd.exe")
        .raw_arg(format!("/k {cmd}"))
        .creation_flags(CREATE_NEW_CONSOLE)
        .spawn()
        .map(drop)
        .map_err(|e| format!("cannot run winget: {e}"))
}

/// Draw the dialog. Returns false once it should close.
pub fn show(ctx: &egui::Context, d: &mut AddDialog, shared: &Arc<Shared>) -> bool {
    let settings = shared.settings();
    let always_best = settings.yt_always_best;
    let drives = &settings.network_drives;
    d.track_url();
    d.poll();
    let url = d.url.trim().to_string();
    let is_yt = is_youtube_video_url(&url);
    if is_yt && matches!(d.yt, Yt::Idle) {
        let wait = DEBOUNCE.saturating_sub(d.changed_at.elapsed());
        if wait.is_zero() {
            d.probe(ctx, shared, &url);
        } else {
            ctx.request_repaint_after(wait);
        }
    }

    let (mut start, mut cancel, mut install, mut retry) = (false, false, false, false);
    let picker = is_yt && !always_best;
    let anchor = crate::touch::typing_dialog_anchor();
    egui::Window::new("Add download")
        .collapsible(false)
        .resizable(false)
        .anchor(anchor.0, anchor.1)
        .show(ctx, |ui| {
            egui::Grid::new("add")
                .num_columns(2)
                .spacing([10.0, 8.0])
                .show(ui, |ui| {
                    ui.label("URL");
                    let resp = ui.add(
                        TextEdit::singleline(&mut d.url)
                            .hint_text("https://…")
                            .desired_width(420.0),
                    );
                    if d.focus {
                        resp.request_focus();
                        d.focus = false;
                    }
                    if resp.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                        start = true;
                    }
                    ui.end_row();
                    ui.label("Save to");
                    ui.horizontal_wrapped(|ui| {
                        ui.label(d.dir.display().to_string());
                        if ui.button("Change…").clicked() {
                            if let Some(p) = rfd::FileDialog::new()
                                .set_directory(settings.picker_start(&d.dir))
                                .pick_folder()
                            {
                                d.dir = p;
                            }
                        }
                        if !drives.is_empty() {
                            // This PC first, so getting back from a network drive is one tap.
                            ui.menu_button("Save on…", |ui| {
                                let local = settings.local_dir();
                                let here = format!("This PC — {}", local.display());
                                if ui.selectable_label(d.dir == local, here).clicked() {
                                    d.dir = local;
                                    ui.close_menu();
                                }
                                for nd in drives {
                                    let on = d.dir == nd.dir();
                                    if ui.selectable_label(on, &nd.path).clicked() {
                                        d.dir = nd.dir();
                                        ui.close_menu();
                                    }
                                }
                            });
                        }
                    });
                    ui.end_row();
                });
            if is_yt {
                ui.separator();
                yt_section(ui, &mut d.yt, always_best, &mut install, &mut retry);
            }
            if let Some(e) = &d.err {
                ui.colored_label(ui.visuals().error_fg_color, e);
            }
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                let ready = matches!(d.yt, Yt::Ready { .. });
                let label = if picker && ready { "Download" } else { "Start" };
                // In picker mode there is nothing to start until the qualities are in.
                let enabled = !picker || matches!(d.yt, Yt::Idle | Yt::Ready { .. });
                start |= ui
                    .add_enabled(enabled, egui::Button::new(label).min_size(vec2(80.0, 0.0)))
                    .clicked();
                cancel = ui.button("Cancel").clicked();
            });
        });

    if install {
        if let Err(e) = run_in_console(INSTALL_CMD) {
            d.err = Some(e);
        }
    }
    if retry && is_yt {
        d.probe(ctx, shared, &url);
    }
    // "Always highest quality" pressed Start while probing: go once the probe is done.
    if d.start_when_ready && matches!(d.yt, Yt::Ready { .. } | Yt::Failed { .. }) {
        d.start_when_ready = false;
        start = true;
    }

    if start {
        let mut nd = None;
        match validate_url(&url) {
            Err(e) => d.err = Some(e),
            Ok(_) if !is_yt => {
                let kind = Kind::guess(&url);
                nd = Some(NewDownload {
                    kind,
                    format: (kind == Kind::Ytdlp).then(|| Kind::YTDLP_BEST.to_string()),
                    url: url.clone(),
                    ..Default::default()
                });
            }
            Ok(_) => match &d.yt {
                Yt::Ready {
                    title,
                    formats,
                    pick,
                    ..
                } => {
                    let i = if always_best { 0 } else { *pick };
                    let f = formats.get(i).or(formats.first());
                    nd = f.map(|f| NewDownload {
                        kind: Kind::Ytdlp,
                        format: Some(f.id.clone()),
                        file_name: sanitize_file_name(title),
                        url: url.clone(),
                        ..Default::default()
                    });
                }
                Yt::Failed { .. } if always_best => {
                    nd = Some(NewDownload {
                        kind: Kind::Ytdlp,
                        format: Some(Kind::YTDLP_BEST.to_string()),
                        url: url.clone(),
                        ..Default::default()
                    });
                }
                Yt::Idle | Yt::Failed { .. } => {
                    d.probe(ctx, shared, &url);
                    d.start_when_ready = always_best;
                }
                Yt::Probing { .. } => d.start_when_ready = always_best,
            },
        }
        if let Some(nd) = nd {
            match shared.add(nd, Some(d.dir.clone()), false) {
                Ok(()) => return false,
                Err(e) => d.err = Some(e),
            }
        }
    }
    !cancel && !ctx.input(|i| i.key_pressed(Key::Escape))
}

fn yt_section(
    ui: &mut egui::Ui,
    yt: &mut Yt,
    always_best: bool,
    install: &mut bool,
    retry: &mut bool,
) {
    match yt {
        Yt::Idle | Yt::Probing { .. } => {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Getting qualities…");
            });
        }
        Yt::Ready {
            title,
            formats,
            pick,
            ..
        } => {
            ui.label(RichText::new(title.as_str()).strong());
            if always_best {
                let best = formats.first().map(format_label).unwrap_or_default();
                ui.label(RichText::new(format!("Highest quality: {best}")).weak());
                return;
            }
            ui.label("Choose a quality:");
            egui::ScrollArea::vertical()
                .max_height(360.0)
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    for (i, f) in formats.iter().enumerate() {
                        ui.radio_value(pick, i, format_label(f));
                    }
                });
        }
        Yt::Failed { msg, missing, .. } => {
            if *missing {
                ui.colored_label(
                    ui.visuals().warn_fg_color,
                    "Thrennix needs yt-dlp — winget install yt-dlp.yt-dlp",
                );
            } else {
                ui.colored_label(ui.visuals().error_fg_color, msg.as_str());
            }
            ui.horizontal(|ui| {
                if *missing && ui.button("Install yt-dlp").clicked() {
                    *install = true;
                }
                *retry |= ui.button("Retry").clicked();
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(id: &str, label: &str, height: Option<u32>, size: Option<u64>) -> YtFormat {
        YtFormat {
            id: id.into(),
            label: label.into(),
            height,
            filesize: size,
        }
    }

    #[test]
    fn default_is_highest_video() {
        let list = vec![
            f("a", "Audio · M4A", None, Some(4)),
            f("720", "MP4 · 720p HD", Some(720), None),
            f("2160", "MP4 · 2160p60 4K", Some(2160), None),
            f("1080", "MP4 · 1080p HD", Some(1080), None),
        ];
        assert_eq!(default_pick(&list), 2);
        let ordered = order_formats(list);
        assert_eq!(ordered[0].id, "720");
        assert_eq!(ordered.last().unwrap().id, "a");
        assert_eq!(default_pick(&ordered), 1);
        // Audio only / empty: first entry.
        assert_eq!(default_pick(&[f("a", "Audio · M4A", None, None)]), 0);
        assert_eq!(default_pick(&[]), 0);
    }

    #[test]
    fn labels_match_extension() {
        const MB: u64 = 1024 * 1024;
        assert_eq!(
            format_label(&f("x", "MP4 · 1080p HD", Some(1080), Some(118 * MB))),
            "MP4 file, quality 1080p HD (≈ 118 MB)"
        );
        assert_eq!(
            format_label(&f("x", "MP4 · 2160p60 4K", Some(2160), Some(1229 * MB))),
            "MP4 file, quality 2160p60 4K (≈ 1.2 GB)"
        );
        assert_eq!(
            format_label(&f("x", "", Some(360), None)),
            "MP4 file, quality 360p"
        );
        assert_eq!(
            format_label(&f("x", "Audio · M4A", None, Some(4 * MB))),
            "M4A audio file (≈ 4.0 MB)"
        );
        assert_eq!(
            format_label(&f("x", "MP4 · Best", None, None)),
            "MP4 file, best quality"
        );
    }

    #[test]
    fn youtube_hosts() {
        for u in [
            "https://www.youtube.com/watch?v=aqz-KE-bpKQ",
            "https://youtu.be/aqz-KE-bpKQ",
            "https://m.youtube.com/watch?v=x",
            "https://www.youtube-nocookie.com/embed/x",
        ] {
            assert!(is_youtube_video_url(u), "{u}");
        }
        for u in [
            "https://music.youtube.com/watch?v=x",
            "https://notyoutube.com/watch?v=x",
            "https://example.com/a.mp4",
            "youtube.com",
            "ftp://youtube.com/x",
        ] {
            assert!(!is_youtube_video_url(u), "{u}");
        }
    }

    #[test]
    fn missing_ytdlp_detected() {
        assert!(is_missing_ytdlp(
            "yt-dlp not found — install with: winget install yt-dlp.yt-dlp"
        ));
        assert!(is_missing_ytdlp("yt-dlp not found (demo)"));
        assert!(!is_missing_ytdlp("playlists are not supported"));
    }
}
