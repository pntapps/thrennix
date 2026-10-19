//! "About Thrennix" window (a separate OS window, like the details windows).
//!
//! Laid out by hand on a fixed-size canvas: a brand-blue gradient header band,
//! a 2x2 grid of publisher link cards, an open-source row and a footer bar.

use eframe::egui::{
    self, pos2, vec2, Color32, CornerRadius, CursorIcon, FontId, Galley, Mesh, Painter, Pos2, Rect,
    Response, Sense, Shape, Stroke, StrokeKind, TextureHandle, Ui, Vec2,
};
use std::sync::Arc;

use crate::brand;
use crate::theme::ui_palette;

pub const TITLE: &str = "About Thrennix";
pub const SIZE: [f32; 2] = [460.0, 452.0];

/// Spacing scale.
const PAD: f32 = 24.0;
const HEADER_H: f32 = 132.0;
const FOOTER_H: f32 = 64.0;
const CARD_H: f32 = 60.0;
const GAP: f32 = 12.0;

/// Installer side-panel gradient.
const BLUE_DEEP: Color32 = Color32::from_rgb(0x0b, 0x3d, 0x91);
const BLUE: Color32 = Color32::from_rgb(0x1a, 0x73, 0xe8);

/// The app logo as a texture (same artwork as the window icon).
pub fn logo(ctx: &egui::Context) -> TextureHandle {
    let icon = crate::app_icon();
    let image = egui::ColorImage::from_rgba_unmultiplied(
        [icon.width as usize, icon.height as usize],
        &icon.rgba,
    );
    ctx.load_texture("thrennix-logo", image, egui::TextureOptions::LINEAR)
}

#[derive(Clone, Copy)]
enum LinkIcon {
    Globe,
    People,
    Shield,
    Document,
}

/// Draw the window body; returns `true` when the user asked to close it.
pub fn show(ui: &mut Ui, logo: &TextureHandle) -> bool {
    let avail = ui.max_rect();
    let size = vec2(
        avail.width().clamp(SIZE[0], SIZE[0] * 1.5),
        avail.height().clamp(SIZE[1], SIZE[1] * 1.5),
    );
    let (full, _) = ui.allocate_exact_size(size, Sense::hover());
    let painter = ui.painter_at(full);

    header(ui, &painter, full, logo);

    let pal = ui_palette(ui);
    let text = ui.visuals().text_color();
    let left = full.left() + PAD;
    let right = full.right() - PAD;
    let mut y = full.top() + HEADER_H + 20.0;

    // "Developed by PNT Apps"            pntapps.com ↗
    let by = text_galley(&painter, "Developed by ", 13.0, pal.weak_text);
    let publisher = text_galley(&painter, brand::PUBLISHER, 13.0, text);
    let row_h = by.size().y;
    painter.galley(pos2(left, y), by.clone(), text);
    bold_galley(&painter, pos2(left + by.size().x, y), publisher, text);
    let site = short_url(brand::WEBSITE);
    text_link(
        ui,
        &painter,
        pos2(right, y + row_h / 2.0),
        &site,
        brand::WEBSITE,
    );
    y += row_h + 14.0;

    // 2x2 grid of link cards.
    let card_w = (right - left - GAP) / 2.0;
    let icons = [
        LinkIcon::Globe,
        LinkIcon::People,
        LinkIcon::Shield,
        LinkIcon::Document,
    ];
    for (i, ((label, url), icon)) in brand::LINKS.iter().zip(icons).enumerate() {
        let col = (i % 2) as f32;
        let row = (i / 2) as f32;
        let rect = Rect::from_min_size(
            pos2(left + col * (card_w + GAP), y + row * (CARD_H + GAP)),
            vec2(card_w, CARD_H),
        );
        link_card(ui, &painter, rect, icon, label, url);
    }
    y += 2.0 * CARD_H + GAP + 18.0;

    // Open source · MIT or Apache-2.0              [View on GitHub ↗]
    let oss_h = 30.0;
    let cy = y + oss_h / 2.0;
    code_icon(&painter, pos2(left + 9.0, cy), pal.weak_text);
    let oss = text_galley(
        &painter,
        "Open source · MIT or Apache-2.0",
        13.0,
        pal.weak_text,
    );
    painter.galley(
        pos2(left + 26.0, cy - oss.size().y / 2.0),
        oss,
        pal.weak_text,
    );
    github_button(ui, &painter, pos2(right, cy), oss_h);

    // Footer bar.
    let footer = Rect::from_min_max(pos2(full.left(), full.bottom() - FOOTER_H), full.max);
    let dark = ui.visuals().dark_mode;
    let footer_fill = if dark {
        Color32::from_rgb(0x19, 0x1a, 0x1d)
    } else {
        pal.sidebar
    };
    painter.rect_filled(footer, 0.0, footer_fill);
    painter.hline(
        footer.x_range(),
        footer.top(),
        Stroke::new(1.0_f32, pal.border),
    );
    let copy = text_galley(&painter, brand::COPYRIGHT, 12.0, pal.weak_text);
    painter.galley(
        pos2(left, footer.center().y - copy.size().y / 2.0),
        copy,
        pal.weak_text,
    );
    let close_rect = Rect::from_min_size(
        pos2(right - 96.0, footer.center().y - 16.0),
        vec2(96.0, 32.0),
    );
    let close = primary_button(ui, &painter, close_rect, "Close", BLUE).clicked();

    close || ui.input(|i| i.key_pressed(egui::Key::Escape))
}

// ---------------------------------------------------------------- header

fn header(ui: &Ui, painter: &Painter, full: Rect, logo: &TextureHandle) {
    let band = Rect::from_min_size(full.min, vec2(full.width(), HEADER_H));
    let p = painter.with_clip_rect(band);

    // Diagonal gradient: deep blue top-left → brand blue bottom-right.
    let mid = lerp_color(BLUE_DEEP, BLUE, 0.5);
    let mut mesh = Mesh::default();
    mesh.colored_vertex(band.left_top(), BLUE_DEEP);
    mesh.colored_vertex(band.right_top(), mid);
    mesh.colored_vertex(band.right_bottom(), BLUE);
    mesh.colored_vertex(band.left_bottom(), mid);
    mesh.add_triangle(0, 1, 2);
    mesh.add_triangle(0, 2, 3);
    p.add(Shape::mesh(mesh));

    // Soft decorative rings on the right.
    let ring = Color32::from_white_alpha(10);
    p.circle_filled(pos2(band.right() - 10.0, band.top() - 6.0), 84.0, ring);
    p.circle_filled(pos2(band.right() + 8.0, band.bottom() + 14.0), 58.0, ring);

    // Logo on a frosted tile.
    let tile = Rect::from_min_size(
        pos2(band.left() + PAD, band.center().y - 38.0),
        vec2(76.0, 76.0),
    );
    p.rect_filled(
        tile.translate(vec2(0.0, 2.0)),
        18.0,
        Color32::from_black_alpha(30),
    );
    p.rect_filled(tile, 18.0, Color32::from_white_alpha(38));
    p.rect_stroke(
        tile,
        18.0,
        Stroke::new(1.0_f32, Color32::from_white_alpha(70)),
        StrokeKind::Inside,
    );
    egui::Image::new(logo)
        .corner_radius(CornerRadius::same(12))
        .paint_at(ui, tile.shrink(10.0));

    // Title, version chip and tagline.
    let x = tile.right() + 18.0;
    let wrap = band.right() - PAD - x;
    let title = text_galley(&p, "Thrennix", 27.0, Color32::WHITE);
    let tagline = p.layout(
        brand::TAGLINE.to_owned(),
        FontId::proportional(13.0),
        Color32::from_white_alpha(225),
        wrap,
    );
    let block_h = title.size().y + 6.0 + tagline.size().y;
    let top = band.center().y - block_h / 2.0;
    let title_size = title.size();
    bold_galley(&p, pos2(x, top), title, Color32::WHITE);

    let chip_text = text_galley(
        &p,
        &format!("Version {}", brand::VERSION),
        11.0,
        Color32::WHITE,
    );
    let chip = Rect::from_min_size(
        pos2(
            x + title_size.x + 10.0,
            top + title_size.y / 2.0 - 10.0 + 1.0,
        ),
        vec2(chip_text.size().x + 16.0, 20.0),
    );
    p.rect_filled(chip, 10.0, Color32::from_white_alpha(46));
    p.rect_stroke(
        chip,
        10.0,
        Stroke::new(1.0_f32, Color32::from_white_alpha(60)),
        StrokeKind::Inside,
    );
    let chip_text_size = chip_text.size();
    p.galley(
        chip.center() - chip_text_size / 2.0,
        chip_text,
        Color32::WHITE,
    );

    p.galley(pos2(x, top + title_size.y + 6.0), tagline, Color32::WHITE);
}

// ---------------------------------------------------------------- widgets

fn link_card(ui: &Ui, painter: &Painter, rect: Rect, icon: LinkIcon, label: &str, url: &str) {
    let pal = ui_palette(ui);
    let dark = ui.visuals().dark_mode;
    let resp = clickable(ui, rect, url);
    let hovered = resp.hovered();
    let pressed = resp.is_pointer_button_down_on();

    let (fill, stroke) = if hovered {
        (
            pal.accent.gamma_multiply(if dark { 0.16 } else { 0.08 }),
            pal.accent.gamma_multiply(if dark { 0.75 } else { 0.55 }),
        )
    } else {
        (ui.visuals().faint_bg_color, pal.border)
    };
    let rect_drawn = if pressed { rect.shrink(1.0) } else { rect };
    painter.rect_filled(rect_drawn, 8.0, ui.visuals().panel_fill);
    painter.rect_filled(rect_drawn, 8.0, fill);
    painter.rect_stroke(
        rect_drawn,
        8.0,
        Stroke::new(1.0_f32, stroke),
        StrokeKind::Inside,
    );

    // Icon badge.
    let badge_c = pos2(rect.left() + 12.0 + 17.0, rect.center().y);
    painter.circle_filled(
        badge_c,
        17.0,
        pal.accent.gamma_multiply(if dark { 0.22 } else { 0.12 }),
    );
    draw_link_icon(painter, icon, badge_c, pal.accent);

    // Label + short URL.
    let x = rect.left() + 12.0 + 34.0 + 12.0;
    let text = ui.visuals().text_color();
    let title = text_galley(painter, label, 13.5, text);
    let sub = text_galley(painter, &short_url(url), 11.0, pal.weak_text);
    let h = title.size().y + 1.0 + sub.size().y;
    let top = rect.center().y - h / 2.0;
    let title_h = title.size().y;
    painter.galley(pos2(x, top), title, text);
    painter.galley(pos2(x, top + title_h + 1.0), sub, pal.weak_text);

    // External-link arrow, top-right.
    let arrow_color = if hovered { pal.accent } else { pal.weak_text };
    external_arrow(
        painter,
        pos2(rect.right() - 14.0, rect.top() + 14.0),
        4.0,
        arrow_color,
    );
}

/// Accent-colored inline link with an external arrow, right-aligned at `anchor`.
fn text_link(ui: &Ui, painter: &Painter, anchor: Pos2, label: &str, url: &str) {
    let pal = ui_palette(ui);
    let galley = text_galley(painter, label, 12.5, pal.accent);
    let w = galley.size().x + 4.0 + 8.0;
    let rect = Rect::from_min_size(
        pos2(anchor.x - w, anchor.y - galley.size().y / 2.0),
        vec2(w, galley.size().y),
    );
    let resp = clickable(ui, rect.expand(3.0), url);
    let gsize = galley.size();
    painter.galley(rect.min, galley, pal.accent);
    if resp.hovered() {
        painter.hline(
            rect.left()..=rect.left() + gsize.x,
            rect.bottom(),
            Stroke::new(1.0_f32, pal.accent),
        );
    }
    external_arrow(painter, pos2(rect.right() - 4.0, anchor.y), 3.5, pal.accent);
}

fn github_button(ui: &Ui, painter: &Painter, right_center: Pos2, h: f32) {
    let pal = ui_palette(ui);
    let dark = ui.visuals().dark_mode;
    let text = ui.visuals().text_color();
    let galley = text_galley(painter, "View on GitHub", 12.5, text);
    let w = 12.0 + galley.size().x + 8.0 + 8.0 + 12.0;
    let rect = Rect::from_min_size(
        pos2(right_center.x - w, right_center.y - h / 2.0),
        vec2(w, h),
    );
    let resp = clickable(ui, rect, brand::SOURCE);
    let (fill, stroke) = if resp.hovered() {
        (
            pal.accent.gamma_multiply(if dark { 0.16 } else { 0.08 }),
            pal.accent.gamma_multiply(0.6),
        )
    } else {
        (ui.visuals().widgets.inactive.weak_bg_fill, pal.border)
    };
    painter.rect_filled(rect, 6.0, fill);
    painter.rect_stroke(rect, 6.0, Stroke::new(1.0_f32, stroke), StrokeKind::Inside);
    let gsize = galley.size();
    painter.galley(
        pos2(rect.left() + 12.0, rect.center().y - gsize.y / 2.0),
        galley,
        text,
    );
    external_arrow(
        painter,
        pos2(rect.right() - 12.0 - 4.0, rect.center().y),
        3.5,
        if resp.hovered() {
            pal.accent
        } else {
            pal.weak_text
        },
    );
}

fn primary_button(
    ui: &Ui,
    painter: &Painter,
    rect: Rect,
    label: &str,
    accent: Color32,
) -> Response {
    let id = ui.id().with(("about-btn", label));
    let resp = ui
        .interact(rect, id, Sense::click())
        .on_hover_cursor(CursorIcon::PointingHand);
    let fill = if resp.is_pointer_button_down_on() {
        lerp_color(accent, Color32::BLACK, 0.18)
    } else if resp.hovered() {
        lerp_color(accent, Color32::WHITE, 0.1)
    } else {
        accent
    };
    painter.rect_filled(rect, 6.0, fill);
    let galley = text_galley(painter, label, 13.0, Color32::WHITE);
    let gsize = galley.size();
    painter.galley(rect.center() - gsize / 2.0, galley, Color32::WHITE);
    resp
}

/// A click target that shows a pointing hand, the URL as tooltip, and opens it.
fn clickable(ui: &Ui, rect: Rect, url: &str) -> Response {
    let resp = ui
        .interact(rect, ui.id().with(("about-link", url)), Sense::click())
        .on_hover_cursor(CursorIcon::PointingHand);
    if resp.clicked() {
        ui.ctx().open_url(egui::OpenUrl::new_tab(url));
    }
    resp.on_hover_text(url)
}

// ---------------------------------------------------------------- drawing helpers

fn text_galley(painter: &Painter, text: &str, size: f32, color: Color32) -> Arc<Galley> {
    painter.layout_no_wrap(text.to_owned(), FontId::proportional(size), color)
}

/// The bundled font has no bold face: overdraw with a sub-pixel offset.
fn bold_galley(painter: &Painter, pos: Pos2, galley: Arc<Galley>, color: Color32) {
    painter.galley(pos, galley.clone(), color);
    painter.galley(pos + vec2(0.6, 0.0), galley, color);
}

fn lerp_color(a: Color32, b: Color32, t: f32) -> Color32 {
    let l = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgb(l(a.r(), b.r()), l(a.g(), b.g()), l(a.b(), b.b()))
}

/// "https://www.pntapps.com/about/" → "pntapps.com/about".
fn short_url(url: &str) -> String {
    let s = url
        .trim_start_matches("https://")
        .trim_start_matches("http://");
    let s = s.strip_prefix("www.").unwrap_or(s);
    s.trim_end_matches('/').to_owned()
}

fn ellipse(c: Pos2, r: Vec2, from: f32, to: f32, n: usize) -> Vec<Pos2> {
    (0..=n)
        .map(|i| {
            let a = from + (to - from) * i as f32 / n as f32;
            c + vec2(r.x * a.cos(), r.y * a.sin())
        })
        .collect()
}

/// Small "↗" arrow centered at `c` with half-size `s`.
fn external_arrow(p: &Painter, c: Pos2, s: f32, color: Color32) {
    let st = Stroke::new(1.4_f32, color);
    let tip = c + vec2(s, -s);
    p.line_segment([c + vec2(-s, s), tip], st);
    p.line_segment([tip, tip + vec2(-s * 1.2, 0.0)], st);
    p.line_segment([tip, tip + vec2(0.0, s * 1.2)], st);
}

/// "</>" glyph centered at `c`.
fn code_icon(p: &Painter, c: Pos2, color: Color32) {
    let st = Stroke::new(1.5_f32, color);
    p.line_segment([c + vec2(-3.5, -4.5), c + vec2(-8.0, 0.0)], st);
    p.line_segment([c + vec2(-8.0, 0.0), c + vec2(-3.5, 4.5)], st);
    p.line_segment([c + vec2(3.5, -4.5), c + vec2(8.0, 0.0)], st);
    p.line_segment([c + vec2(8.0, 0.0), c + vec2(3.5, 4.5)], st);
    p.line_segment([c + vec2(1.5, -6.0), c + vec2(-1.5, 6.0)], st);
}

fn draw_link_icon(p: &Painter, icon: LinkIcon, c: Pos2, color: Color32) {
    use std::f32::consts::{PI, TAU};
    let st = Stroke::new(1.5_f32, color);
    match icon {
        LinkIcon::Globe => {
            let r = 8.0;
            p.circle_stroke(c, r, st);
            p.add(Shape::line(ellipse(c, vec2(r * 0.42, r), 0.0, TAU, 32), st));
            p.line_segment([c - vec2(r, 0.0), c + vec2(r, 0.0)], st);
            for dy in [-0.5_f32, 0.5] {
                let half = r * (1.0 - dy * dy).sqrt() * 0.92;
                p.line_segment(
                    [c + vec2(-half, r * dy), c + vec2(half, r * dy)],
                    Stroke::new(1.1_f32, color),
                );
            }
        }
        LinkIcon::People => {
            // Front person.
            p.circle_stroke(c + vec2(-2.0, -3.5), 3.2, st);
            p.add(Shape::line(
                ellipse(c + vec2(-2.0, 7.0), vec2(6.0, 5.5), PI, TAU, 16),
                st,
            ));
            // Person behind, to the right.
            let st2 = Stroke::new(1.3_f32, color.gamma_multiply(0.75));
            p.add(Shape::line(
                ellipse(
                    c + vec2(5.0, -4.0),
                    vec2(2.6, 2.6),
                    -PI * 0.55,
                    PI * 0.75,
                    16,
                ),
                st2,
            ));
            p.add(Shape::line(
                ellipse(c + vec2(5.5, 6.0), vec2(4.5, 4.5), PI * 1.45, TAU, 12),
                st2,
            ));
        }
        LinkIcon::Shield => {
            let pts = vec![
                c + vec2(0.0, -8.5),
                c + vec2(7.0, -5.5),
                c + vec2(6.5, 1.5),
                c + vec2(3.5, 6.0),
                c + vec2(0.0, 8.5),
                c + vec2(-3.5, 6.0),
                c + vec2(-6.5, 1.5),
                c + vec2(-7.0, -5.5),
            ];
            p.add(Shape::closed_line(pts, st));
            // Check mark.
            p.line_segment([c + vec2(-3.0, 0.0), c + vec2(-0.8, 2.4)], st);
            p.line_segment([c + vec2(-0.8, 2.4), c + vec2(3.4, -2.4)], st);
        }
        LinkIcon::Document => {
            let (l, t, r, b) = (c.x - 6.0, c.y - 8.0, c.x + 6.0, c.y + 8.0);
            let fold = 4.0;
            let pts = vec![
                pos2(l, t),
                pos2(r - fold, t),
                pos2(r, t + fold),
                pos2(r, b),
                pos2(l, b),
            ];
            p.add(Shape::closed_line(pts, st));
            p.add(Shape::line(
                vec![
                    pos2(r - fold, t),
                    pos2(r - fold, t + fold),
                    pos2(r, t + fold),
                ],
                Stroke::new(1.2_f32, color),
            ));
            let thin = Stroke::new(1.2_f32, color);
            for (i, w) in [8.0_f32, 8.0, 5.0].iter().enumerate() {
                let y = c.y - 1.0 + i as f32 * 3.0;
                p.line_segment([pos2(l + 2.5, y), pos2(l + 2.5 + w * 0.9, y)], thin);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_urls() {
        assert_eq!(short_url("https://www.pntapps.com/"), "pntapps.com");
        assert_eq!(
            short_url("https://www.pntapps.com/about"),
            "pntapps.com/about"
        );
        assert_eq!(
            short_url("https://github.com/pntapps/thrennix"),
            "github.com/pntapps/thrennix"
        );
    }
}
