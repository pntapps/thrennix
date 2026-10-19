//! Light/Dark palettes, file categories, and small vector icons drawn with the painter.

use eframe::egui::{
    self, pos2, vec2, Color32, CornerRadius, FontId, Painter, Pos2, Rect, Stroke, StrokeKind,
    Theme, Visuals,
};
use serde::{Deserialize, Serialize};

use crate::protocol::Kind;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemeChoice {
    #[default]
    System,
    Light,
    Dark,
}

impl ThemeChoice {
    pub fn label(self) -> &'static str {
        match self {
            ThemeChoice::System => "System",
            ThemeChoice::Light => "Light",
            ThemeChoice::Dark => "Dark",
        }
    }
}

const fn hex(rgb: u32) -> Color32 {
    Color32::from_rgb((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8)
}

/// Semantic colors used by the custom widgets.
#[derive(Clone, Copy)]
pub struct Palette {
    pub accent: Color32,
    pub ok: Color32,
    pub error: Color32,
    pub paused: Color32,
    pub track: Color32,
    pub border: Color32,
    pub sidebar: Color32,
    pub weak_text: Color32,
}

pub fn palette(dark: bool) -> Palette {
    if dark {
        Palette {
            accent: hex(0x4c8df6),
            ok: hex(0x3fb950),
            error: hex(0xf2555a),
            paused: hex(0xe3a33b),
            track: hex(0x3a3c42),
            border: hex(0x3a3c42),
            sidebar: hex(0x232428),
            weak_text: hex(0x9aa0a6),
        }
    } else {
        Palette {
            accent: hex(0x1a73e8),
            ok: hex(0x1e8e3e),
            error: hex(0xd93025),
            paused: hex(0xe37400),
            track: hex(0xe3e6ea),
            border: hex(0xdadce0),
            sidebar: hex(0xf1f3f6),
            weak_text: hex(0x5f6368),
        }
    }
}

pub fn ui_palette(ui: &egui::Ui) -> Palette {
    palette(ui.visuals().dark_mode)
}

fn visuals(dark: bool) -> Visuals {
    let p = palette(dark);
    let mut v = if dark {
        Visuals::dark()
    } else {
        Visuals::light()
    };
    let (bg, panel, window, faint, text, btn, btn_hover) = if dark {
        (
            hex(0x1e1f22),
            hex(0x1e1f22),
            hex(0x2b2d31),
            hex(0x25262a),
            hex(0xe3e5e8),
            hex(0x34363c),
            hex(0x3f4249),
        )
    } else {
        (
            hex(0xffffff),
            hex(0xffffff),
            hex(0xffffff),
            hex(0xf7f8fa),
            hex(0x202124),
            hex(0xeef0f3),
            hex(0xe2e6ec),
        )
    };
    v.panel_fill = panel;
    v.window_fill = window;
    v.extreme_bg_color = if dark { hex(0x17181a) } else { hex(0xffffff) };
    v.faint_bg_color = faint;
    v.code_bg_color = faint;
    v.hyperlink_color = p.accent;
    v.warn_fg_color = p.paused;
    v.error_fg_color = p.error;
    v.override_text_color = Some(text);
    v.window_stroke = Stroke::new(1.0_f32, p.border);
    v.window_corner_radius = CornerRadius::same(8);
    v.menu_corner_radius = CornerRadius::same(6);
    v.selection.bg_fill = p.accent.gamma_multiply(if dark { 0.35 } else { 0.18 });
    v.selection.stroke = Stroke::new(1.0_f32, p.accent);
    v.striped = true;
    let r = CornerRadius::same(4);
    v.widgets.noninteractive.bg_fill = bg;
    v.widgets.noninteractive.weak_bg_fill = bg;
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, p.border);
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0_f32, text);
    for (w, fill) in [
        (&mut v.widgets.inactive, btn),
        (&mut v.widgets.hovered, btn_hover),
        (&mut v.widgets.active, btn_hover),
        (&mut v.widgets.open, btn_hover),
    ] {
        w.bg_fill = fill;
        w.weak_bg_fill = fill;
        w.corner_radius = r;
        w.fg_stroke = Stroke::new(1.0_f32, text);
    }
    v.widgets.inactive.bg_stroke = Stroke::NONE;
    v.widgets.hovered.bg_stroke = Stroke::new(1.0_f32, p.accent.gamma_multiply(0.6));
    v.widgets.active.bg_stroke = Stroke::new(1.0_f32, p.accent);
    v
}

/// Install both palettes and select the user's choice (System follows Windows).
pub fn apply(ctx: &egui::Context, choice: ThemeChoice) {
    ctx.set_visuals_of(Theme::Light, visuals(false));
    ctx.set_visuals_of(Theme::Dark, visuals(true));
    ctx.set_theme(match choice {
        ThemeChoice::System => egui::ThemePreference::System,
        ThemeChoice::Light => egui::ThemePreference::Light,
        ThemeChoice::Dark => egui::ThemePreference::Dark,
    });
}

// ---------------------------------------------------------------- categories

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    Compressed,
    Documents,
    Music,
    Programs,
    Video,
    Other,
}

impl Category {
    pub const TYPES: [Category; 5] = [
        Category::Compressed,
        Category::Documents,
        Category::Music,
        Category::Programs,
        Category::Video,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Category::Compressed => "Compressed",
            Category::Documents => "Documents",
            Category::Music => "Music",
            Category::Programs => "Programs",
            Category::Video => "Video",
            Category::Other => "Other",
        }
    }

    /// Short tag + color for the file-type glyph.
    pub fn glyph(self) -> (&'static str, Color32) {
        match self {
            Category::Compressed => ("ZIP", hex(0xb8860b)),
            Category::Documents => ("DOC", hex(0x1a73e8)),
            Category::Music => ("MP3", hex(0x9334e6)),
            Category::Programs => ("EXE", hex(0x5f6368)),
            Category::Video => ("VID", hex(0xd93025)),
            Category::Other => ("FILE", hex(0x80868b)),
        }
    }
}

pub fn classify(kind: Kind, name: &str) -> Category {
    if kind.is_media() {
        return Category::Video;
    }
    let lower = name.to_ascii_lowercase();
    let ext = lower.rsplit('.').next().unwrap_or("");
    let ext = if lower.ends_with(".tar.gz") {
        "gz"
    } else {
        ext
    };
    match ext {
        "zip" | "rar" | "7z" | "tar" | "gz" | "tgz" | "bz2" | "xz" | "zst" | "cab" | "iso"
        | "img" => Category::Compressed,
        "pdf" | "doc" | "docx" | "xls" | "xlsx" | "ppt" | "pptx" | "txt" | "rtf" | "odt"
        | "ods" | "odp" | "csv" | "epub" | "md" => Category::Documents,
        "mp3" | "wav" | "flac" | "aac" | "m4a" | "ogg" | "opus" | "wma" => Category::Music,
        "exe" | "msi" | "msix" | "appx" | "bat" | "cmd" | "apk" | "dmg" | "deb" | "rpm"
        | "appimage" => Category::Programs,
        "mp4" | "mkv" | "avi" | "mov" | "webm" | "wmv" | "flv" | "m4v" | "ts" | "mpg" | "mpeg" => {
            Category::Video
        }
        _ => Category::Other,
    }
}

/// Small rounded tag like "ZIP" in front of a file name.
pub fn file_glyph(ui: &mut egui::Ui, cat: Category) {
    let (text, color) = cat.glyph();
    let (rect, _) = ui.allocate_exact_size(vec2(28.0, 15.0), egui::Sense::hover());
    let p = ui.painter();
    p.rect_filled(
        rect,
        3.0,
        color.gamma_multiply(if ui.visuals().dark_mode { 0.85 } else { 1.0 }),
    );
    p.text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        text,
        FontId::proportional(8.5),
        Color32::WHITE,
    );
}

// ---------------------------------------------------------------- icons

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Icon {
    Add,
    Resume,
    Pause,
    PauseAll,
    Delete,
    DeleteDone,
    Settings,
}

/// Draw `icon` centered in `rect` (about 22px).
pub fn draw_icon(p: &Painter, icon: Icon, rect: Rect, color: Color32, accent: Color32) {
    let c = rect.center();
    let s = rect.width().min(rect.height()) / 2.0;
    let stroke = Stroke::new(2.0_f32, color);
    match icon {
        Icon::Add => {
            p.circle_filled(c, s, accent);
            let w = Stroke::new(2.2_f32, Color32::WHITE);
            p.line_segment([c - vec2(s * 0.5, 0.0), c + vec2(s * 0.5, 0.0)], w);
            p.line_segment([c - vec2(0.0, s * 0.5), c + vec2(0.0, s * 0.5)], w);
        }
        Icon::Resume => {
            let pts = vec![
                c + vec2(-s * 0.55, -s * 0.75),
                c + vec2(s * 0.75, 0.0),
                c + vec2(-s * 0.55, s * 0.75),
            ];
            p.add(egui::Shape::convex_polygon(
                pts,
                hex(0x1e8e3e),
                Stroke::NONE,
            ));
        }
        Icon::Pause => pause_bars(p, c, s, hex(0xe37400)),
        Icon::PauseAll => {
            pause_bars(
                p,
                c + vec2(-s * 0.35, -s * 0.15),
                s * 0.8,
                hex(0xe37400).gamma_multiply(0.55),
            );
            pause_bars(p, c + vec2(s * 0.25, s * 0.15), s * 0.8, hex(0xe37400));
        }
        Icon::Delete | Icon::DeleteDone => {
            let body =
                Rect::from_min_max(c + vec2(-s * 0.55, -s * 0.45), c + vec2(s * 0.55, s * 0.85));
            p.rect_stroke(body, 2.0, stroke, StrokeKind::Inside);
            p.line_segment(
                [c + vec2(-s * 0.8, -s * 0.6), c + vec2(s * 0.8, -s * 0.6)],
                stroke,
            );
            p.line_segment(
                [
                    c + vec2(-s * 0.25, -s * 0.85),
                    c + vec2(s * 0.25, -s * 0.85),
                ],
                stroke,
            );
            if icon == Icon::Delete {
                for dx in [-0.2, 0.2] {
                    p.line_segment(
                        [c + vec2(s * dx, -s * 0.2), c + vec2(s * dx, s * 0.6)],
                        Stroke::new(1.5_f32, color),
                    );
                }
            } else {
                let g = Stroke::new(2.2_f32, hex(0x1e8e3e));
                p.line_segment(
                    [c + vec2(-s * 0.3, s * 0.2), c + vec2(-s * 0.05, s * 0.45)],
                    g,
                );
                p.line_segment(
                    [c + vec2(-s * 0.05, s * 0.45), c + vec2(s * 0.35, -s * 0.05)],
                    g,
                );
            }
        }
        Icon::Settings => {
            for i in 0..8 {
                let a = i as f32 * std::f32::consts::TAU / 8.0;
                let d = vec2(a.cos(), a.sin());
                p.line_segment(
                    [c + d * s * 0.55, c + d * s * 0.95],
                    Stroke::new(3.0_f32, color),
                );
            }
            p.circle_stroke(c, s * 0.6, Stroke::new(2.2_f32, color));
            p.circle_filled(c, s * 0.22, color);
        }
    }
}

fn pause_bars(p: &Painter, c: Pos2, s: f32, color: Color32) {
    for dx in [-0.42, 0.18] {
        let r = Rect::from_min_size(pos2(c.x + s * dx, c.y - s * 0.75), vec2(s * 0.26, s * 1.5));
        p.rect_filled(r, 1.5, color);
    }
}

/// IDM-style toolbar button: icon above a small label.
pub fn toolbar_button(ui: &mut egui::Ui, icon: Icon, label: &str, enabled: bool) -> egui::Response {
    let size = vec2(68.0, 50.0);
    let sense = if enabled {
        egui::Sense::click()
    } else {
        egui::Sense::hover()
    };
    let (rect, resp) = ui.allocate_exact_size(size, sense);
    if ui.is_rect_visible(rect) {
        let pal = ui_palette(ui);
        let p = ui.painter();
        if enabled && resp.hovered() {
            p.rect_filled(rect, 6.0, pal.accent.gamma_multiply(0.12));
            p.rect_stroke(
                rect,
                6.0,
                Stroke::new(1.0_f32, pal.accent.gamma_multiply(0.4)),
                StrokeKind::Inside,
            );
        }
        let text = ui.visuals().text_color();
        let alpha = if enabled { 1.0 } else { 0.35 };
        let icon_rect =
            Rect::from_center_size(pos2(rect.center().x, rect.top() + 18.0), vec2(22.0, 22.0));
        draw_icon(
            p,
            icon,
            icon_rect,
            text.gamma_multiply(alpha),
            pal.accent.gamma_multiply(alpha),
        );
        if !enabled {
            // Dim colored icons by veiling them with the panel color.
            p.rect_filled(
                icon_rect.expand(2.0),
                0.0,
                ui.visuals().panel_fill.gamma_multiply(0.65),
            );
        }
        p.text(
            pos2(rect.center().x, rect.bottom() - 9.0),
            egui::Align2::CENTER_CENTER,
            label,
            FontId::proportional(11.5),
            text.gamma_multiply(alpha),
        );
    }
    resp
}

/// Thin progress bar (track + fill).
pub fn thin_bar(p: &Painter, rect: Rect, frac: f32, fill: Color32, track: Color32) {
    p.rect_filled(rect, 2.0, track);
    let w = rect.width() * frac.clamp(0.0, 1.0);
    if w > 0.5 {
        p.rect_filled(
            Rect::from_min_size(rect.min, vec2(w, rect.height())),
            2.0,
            fill,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn categories() {
        assert_eq!(classify(Kind::File, "a.ZIP"), Category::Compressed);
        assert_eq!(classify(Kind::File, "data.tar.gz"), Category::Compressed);
        assert_eq!(classify(Kind::File, "r.pdf"), Category::Documents);
        assert_eq!(classify(Kind::File, "s.flac"), Category::Music);
        assert_eq!(classify(Kind::File, "setup.exe"), Category::Programs);
        assert_eq!(classify(Kind::File, "m.mkv"), Category::Video);
        assert_eq!(classify(Kind::Ytdlp, "x"), Category::Video);
        assert_eq!(classify(Kind::Hls, "x.bin"), Category::Video);
        assert_eq!(classify(Kind::File, "noext"), Category::Other);
    }

    #[test]
    fn theme_serde() {
        assert_eq!(
            serde_json::to_string(&ThemeChoice::Dark).unwrap(),
            r#""dark""#
        );
        let t: ThemeChoice = serde_json::from_str(r#""system""#).unwrap();
        assert_eq!(t, ThemeChoice::System);
    }
}
