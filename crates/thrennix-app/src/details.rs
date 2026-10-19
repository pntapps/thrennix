//! Per-download progress window (a separate OS window, IDM-style).

use eframe::egui::{self, vec2, Align, Layout, Rect, RichText, Sense, Stroke, StrokeKind};
use egui_extras::{Column, TableBuilder};
use thrennix_core::{SegmentInfo, Status};

use crate::fmt;
use crate::protocol::Kind;
use crate::state::RowView;
use crate::theme::{self, ui_palette};

pub const SIZE_EXPANDED: [f32; 2] = [580.0, 560.0];
pub const SIZE_COLLAPSED: [f32; 2] = [580.0, 300.0];

#[derive(Default)]
pub struct DetailsState {
    pub tab: u8,
    pub collapsed: bool,
    pub confirm_cancel: bool,
}

pub enum DetailsAction {
    Pause,
    Resume,
    /// Cancelled; `true` = also delete the partial file.
    Cancel(bool),
    Options {
        open_folder: bool,
        open_file: bool,
    },
    Resize([f32; 2]),
    Close,
}

pub fn title(r: &RowView) -> String {
    match (&r.status, r.total) {
        (Status::Completed, _) => format!("Complete · {}", r.name),
        (_, Some(t)) if t > 0 && r.running => {
            format!("{:.0}% {}", r.downloaded as f64 * 100.0 / t as f64, r.name)
        }
        _ => r.name.to_string(),
    }
}

pub fn status_line(r: &RowView) -> String {
    match &r.status {
        Status::Completed => "Complete".into(),
        Status::Failed(e) => format!("Error: {e}"),
        Status::Paused => "Paused".into(),
        _ if r.muxing => if r.kind == Kind::Ytdlp {
            "Merging video and audio…"
        } else {
            "Muxing…"
        }
        .into(),
        Status::Probing => "Connecting…".into(),
        Status::Downloading => "Receiving data…".into(),
    }
}

/// Draw the window body; returns what the user asked for.
pub fn show(ui: &mut egui::Ui, r: &RowView, st: &mut DetailsState) -> Vec<DetailsAction> {
    let mut out = Vec::new();
    let pal = ui_palette(ui);
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        theme::file_glyph(ui, r.category);
        ui.add(egui::Label::new(RichText::new(&*r.name).size(16.0).strong()).truncate());
    });
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.selectable_value(&mut st.tab, 0, "Download status");
        ui.selectable_value(&mut st.tab, 1, "Options");
    });
    ui.separator();

    if st.tab == 0 {
        egui::Grid::new("details-grid")
            .num_columns(2)
            .spacing([16.0, 6.0])
            .show(ui, |ui| {
                let weak = |t: &str| RichText::new(t).color(pal.weak_text);
                ui.label(weak("URL"));
                ui.add(egui::Label::new(&*r.url).truncate().selectable(true))
                    .on_hover_text(&*r.url);
                ui.end_row();
                ui.label(weak("Status"));
                ui.label(status_line(r));
                ui.end_row();
                ui.label(weak("File size"));
                ui.label(r.total.map_or("unknown".into(), fmt::bytes));
                ui.end_row();
                ui.label(weak("Downloaded"));
                ui.label(match r.total {
                    Some(t) if t > 0 => format!(
                        "{}  ({:.2}%)",
                        fmt::bytes(r.downloaded),
                        r.downloaded as f64 * 100.0 / t as f64
                    ),
                    _ => fmt::bytes(r.downloaded),
                });
                ui.end_row();
                let active = r.running && !r.muxing && r.status == Status::Downloading;
                ui.label(weak("Transfer rate"));
                ui.label(if active {
                    fmt::speed(r.speed_bps)
                } else {
                    "—".into()
                });
                ui.end_row();
                ui.label(weak("Time left"));
                ui.label(if active {
                    r.eta_secs.map_or("—".into(), fmt::eta)
                } else {
                    "—".into()
                });
                ui.end_row();
                ui.label(weak("Connections"));
                ui.label(if active {
                    r.connections.to_string()
                } else {
                    "—".into()
                });
                ui.end_row();
                ui.label(weak("Resume capability"));
                ui.label(match r.resumable {
                    Some(true) => "Yes",
                    Some(false) => "No",
                    None => "Unknown",
                });
                ui.end_row();
            });
    } else {
        let (mut folder, mut file) = (r.open_folder_when_done, r.open_file_when_done);
        let a = ui.checkbox(&mut folder, "Open folder when done").changed();
        let b = ui.checkbox(&mut file, "Open file when done").changed();
        if a || b {
            out.push(DetailsAction::Options {
                open_folder: folder,
                open_file: file,
            });
        }
        ui.add_space(70.0);
    }

    ui.add_space(8.0);
    let frac = match (r.total, &r.status) {
        (_, Status::Completed) => 1.0,
        (Some(t), _) if t > 0 => (r.downloaded as f64 / t as f64) as f32,
        _ => 0.0,
    };
    let fill = state_color(r, &pal);
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 20.0), Sense::hover());
    theme::thin_bar(ui.painter(), rect, frac, fill, pal.track);
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        format!("{:.1}%", frac * 100.0),
        egui::FontId::proportional(12.0),
        if frac > 0.5 {
            egui::Color32::WHITE
        } else {
            ui.visuals().text_color()
        },
    );
    ui.add_space(8.0);

    if st.confirm_cancel {
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.label("Download cancelled. Delete the partially downloaded file too?");
            ui.horizontal(|ui| {
                if ui.button("Delete partial file").clicked() {
                    out.push(DetailsAction::Cancel(true));
                }
                if ui.button("Keep it").clicked() {
                    out.push(DetailsAction::Cancel(false));
                }
            });
        });
        ui.add_space(6.0);
    }

    ui.horizontal(|ui| {
        let toggle = if st.collapsed {
            "Show details >>"
        } else {
            "<< Hide details"
        };
        if ui.button(toggle).clicked() {
            st.collapsed = !st.collapsed;
            out.push(DetailsAction::Resize(if st.collapsed {
                SIZE_COLLAPSED
            } else {
                SIZE_EXPANDED
            }));
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let done = r.status == Status::Completed;
            if done {
                if ui.button("Close").clicked() {
                    out.push(DetailsAction::Close);
                }
            } else {
                if ui.button("Cancel").clicked() {
                    out.push(DetailsAction::Pause);
                    st.confirm_cancel = true;
                }
                if r.running {
                    if ui.button("Pause").clicked() {
                        out.push(DetailsAction::Pause);
                    }
                } else if ui.button("Resume").clicked() {
                    out.push(DetailsAction::Resume);
                }
            }
        });
    });

    if !st.collapsed {
        ui.add_space(10.0);
        ui.label(RichText::new("Start positions and download progress by connections").strong());
        ui.add_space(4.0);
        if r.kind != Kind::File {
            let (rect, _) =
                ui.allocate_exact_size(vec2(ui.available_width(), 22.0), Sense::hover());
            theme::thin_bar(ui.painter(), rect, frac, fill, pal.track);
            ui.add_space(6.0);
            ui.label(
                RichText::new("N/A for stream downloads (segments are fetched as a playlist).")
                    .color(pal.weak_text),
            );
        } else {
            connections_bar(ui, &r.segments, r.total, frac, &r.status);
            ui.add_space(6.0);
            connections_table(ui, &r.segments, r.running);
        }
    }
    out
}

fn state_color(r: &RowView, pal: &theme::Palette) -> egui::Color32 {
    match &r.status {
        Status::Completed => pal.ok,
        Status::Failed(_) => pal.error,
        Status::Paused => pal.paused,
        _ => pal.accent,
    }
}

/// Full-width bar: every segment's span (start..end) with its downloaded part filled.
fn connections_bar(
    ui: &mut egui::Ui,
    segs: &[SegmentInfo],
    total: Option<u64>,
    frac: f32,
    status: &Status,
) {
    let pal = ui_palette(ui);
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 26.0), Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, 3.0, pal.track);
    let Some(total) = total.filter(|t| *t > 0) else {
        p.rect_filled(
            Rect::from_min_size(rect.min, vec2(rect.width() * frac, rect.height())),
            3.0,
            pal.accent,
        );
        return;
    };
    if *status == Status::Completed || segs.is_empty() {
        let color = if *status == Status::Completed {
            pal.ok
        } else {
            pal.accent.gamma_multiply(0.5)
        };
        p.rect_filled(
            Rect::from_min_size(rect.min, vec2(rect.width() * frac, rect.height())),
            3.0,
            color,
        );
    }
    let x =
        |b: u64| rect.left() + (b.min(total) as f64 / total as f64 * rect.width() as f64) as f32;
    let inner = rect.shrink2(vec2(0.0, 3.0));
    for (i, s) in segs.iter().enumerate() {
        let span = Rect::from_x_y_ranges(x(s.start)..=x(s.end), inner.y_range());
        // Hue-shifted fill per connection so neighbouring segments are distinguishable.
        let shade = if i % 2 == 0 { 1.0 } else { 0.78 };
        let color = if s.active {
            pal.accent.gamma_multiply(shade)
        } else {
            pal.ok.gamma_multiply(0.85 * shade)
        };
        let done = Rect::from_x_y_ranges(x(s.start)..=x(s.cursor.min(s.end)), inner.y_range());
        if done.width() > 0.3 {
            p.rect_filled(done, 1.0, color);
        }
        p.rect_stroke(
            span,
            1.0,
            Stroke::new(1.0_f32, pal.border),
            StrokeKind::Inside,
        );
        // Start-position marker.
        p.line_segment(
            [
                egui::pos2(span.left(), rect.top()),
                egui::pos2(span.left(), rect.bottom()),
            ],
            Stroke::new(1.5_f32, ui.visuals().text_color().gamma_multiply(0.6)),
        );
    }
    p.rect_stroke(
        rect,
        3.0,
        Stroke::new(1.0_f32, pal.border),
        StrokeKind::Inside,
    );
    resp.on_hover_text(format!(
        "{} connections",
        segs.iter().filter(|s| s.active).count()
    ));
}

fn connections_table(ui: &mut egui::Ui, segs: &[SegmentInfo], running: bool) {
    let pal = ui_palette(ui);
    if segs.is_empty() {
        ui.label(
            RichText::new(if running {
                "Waiting for connections…"
            } else {
                "No active connections."
            })
            .color(pal.weak_text),
        );
        return;
    }
    let height = ui.available_height().max(80.0);
    TableBuilder::new(ui)
        .id_salt("connections")
        .striped(true)
        .resizable(false)
        .max_scroll_height(height)
        .cell_layout(Layout::left_to_right(Align::Center))
        .column(Column::exact(40.0))
        .column(Column::initial(150.0))
        .column(Column::initial(110.0))
        .column(Column::remainder())
        .header(20.0, |mut h| {
            for t in ["N.", "Downloaded", "Speed", "Info"] {
                h.col(|ui| {
                    ui.label(RichText::new(t).strong());
                });
            }
        })
        .body(|body| {
            body.rows(20.0, segs.len(), |mut row| {
                let s = &segs[row.index()];
                let i = row.index();
                row.col(|ui| {
                    ui.label(format!("{}", i + 1));
                });
                row.col(|ui| {
                    ui.label(fmt::bytes(s.cursor.saturating_sub(s.start)));
                });
                row.col(|ui| {
                    ui.label(if s.active {
                        fmt::speed(s.speed_bps)
                    } else {
                        "—".into()
                    });
                });
                row.col(|ui| {
                    let (t, c) = if s.cursor >= s.end {
                        ("Done", pal.ok)
                    } else if s.active && s.speed_bps > 0.0 {
                        ("Receiving data…", pal.accent)
                    } else {
                        ("Idle", pal.weak_text)
                    };
                    ui.label(RichText::new(t).color(c));
                });
            });
        });
}
