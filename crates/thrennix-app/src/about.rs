//! "About Thrennix" window (a separate OS window, like the details windows).

use eframe::egui::{self, vec2, Align, RichText, TextureHandle};

use crate::brand;
use crate::theme::ui_palette;

pub const TITLE: &str = "About Thrennix";
pub const SIZE: [f32; 2] = [420.0, 470.0];

/// The app logo as a texture (same artwork as the window icon).
pub fn logo(ctx: &egui::Context) -> TextureHandle {
    let icon = crate::app_icon();
    let image = egui::ColorImage::from_rgba_unmultiplied(
        [icon.width as usize, icon.height as usize],
        &icon.rgba,
    );
    ctx.load_texture("thrennix-logo", image, egui::TextureOptions::LINEAR)
}

/// Draw the window body; returns `true` when the user asked to close it.
pub fn show(ui: &mut egui::Ui, logo: &TextureHandle) -> bool {
    let pal = ui_palette(ui);
    let mut close = false;
    ui.vertical_centered(|ui| {
        ui.add_space(14.0);
        ui.add(egui::Image::new(logo).fit_to_exact_size(vec2(72.0, 72.0)));
        ui.add_space(6.0);
        ui.label(RichText::new("Thrennix").size(24.0).strong());
        ui.label(
            RichText::new(format!("Version {}", brand::VERSION))
                .small()
                .color(pal.weak_text),
        );
        ui.add_space(6.0);
        ui.label(brand::TAGLINE);
        ui.add_space(10.0);

        let mut job = egui::text::LayoutJob::default();
        let style = ui.style().clone();
        RichText::new("Developed by ")
            .color(pal.weak_text)
            .append_to(
                &mut job,
                &style,
                egui::FontSelection::Default,
                Align::Center,
            );
        RichText::new(brand::PUBLISHER)
            .strong()
            .color(pal.accent)
            .append_to(
                &mut job,
                &style,
                egui::FontSelection::Default,
                Align::Center,
            );
        ui.label(job);
        ui.add_space(8.0);

        egui::Frame::group(ui.style())
            .fill(pal.sidebar)
            .stroke(egui::Stroke::new(1.0_f32, pal.border))
            .corner_radius(egui::CornerRadius::same(8))
            .inner_margin(egui::Margin::symmetric(24, 10))
            .show(ui, |ui| {
                ui.set_min_width(220.0);
                ui.vertical_centered(|ui| {
                    ui.spacing_mut().item_spacing.y = 4.0;
                    for (label, url) in brand::LINKS {
                        ui.hyperlink_to(RichText::new(label).color(pal.accent), url)
                            .on_hover_text(url);
                    }
                });
            });

        ui.add_space(12.0);
        ui.label(RichText::new("Open source under MIT or Apache-2.0").color(pal.weak_text));
        ui.hyperlink_to(
            RichText::new("github.com/pntapps/thrennix").color(pal.accent),
            brand::SOURCE,
        )
        .on_hover_text(brand::SOURCE);
        ui.add_space(8.0);
        ui.label(RichText::new(brand::COPYRIGHT).small().color(pal.weak_text));
        ui.add_space(10.0);
        close = ui
            .add(egui::Button::new("Close").min_size(vec2(80.0, 0.0)))
            .clicked();
    });
    close || ui.input(|i| i.key_pressed(egui::Key::Escape))
}
