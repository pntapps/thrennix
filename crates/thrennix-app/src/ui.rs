//! The main window: toolbar, category sidebar, downloads table, status bar, dialogs.

use std::collections::{BTreeMap, BTreeSet};
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use eframe::egui::{
    self, vec2, Align, Align2, Key, Layout, Rect, RichText, Sense, TextEdit, ViewportBuilder,
    ViewportClass, ViewportCommand, ViewportId,
};
use egui_extras::{Column, TableBuilder};
use thrennix_core::Status;

use crate::about;
use crate::add_url::{self, AddDialog};
use crate::brand;
use crate::details::{self, DetailsAction, DetailsState};
use crate::fmt;
use crate::protocol::{validate_url, Kind};
use crate::state::{reveal, RowView, Settings, Shared};
use crate::theme::{self, toolbar_button, ui_palette, Category, Icon, ThemeChoice};

const TOAST_TIME: Duration = Duration::from_secs(8);
const HIGHLIGHT_TIME: Duration = Duration::from_millis(2500);
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;
const ROW_HEIGHT: f32 = 26.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Filter {
    All,
    Unfinished,
    Finished,
    Type(Category),
}

impl Filter {
    fn matches(self, r: &RowView) -> bool {
        match self {
            Filter::All => true,
            Filter::Unfinished => r.status != Status::Completed,
            Filter::Finished => r.status == Status::Completed,
            Filter::Type(c) => r.category == c,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SortKey {
    Added,
    Name,
    Size,
    Status,
    TimeLeft,
    Rate,
    LastTry,
    Host,
}

/// yt-dlp availability, filled in by a background check.
#[derive(Clone)]
enum YtStatus {
    Checking,
    Found { path: PathBuf, version: String },
    Missing,
}

struct RemoveDialog {
    ids: Vec<u64>,
    label: String,
    delete: bool,
    identifiable: bool,
}

enum Action {
    Pause(u64),
    Resume(u64),
    Open(PathBuf),
    Reveal(Option<PathBuf>, PathBuf),
    CopyUrl(String),
    NewUrl(u64, String),
    Restart(u64, String),
    Details(u64),
    Remove(Vec<u64>, bool),
}

pub struct ThrennixApp {
    shared: Arc<Shared>,
    toast: Option<(String, Instant)>,
    settings_edit: Option<Settings>,
    yt_status: Arc<Mutex<YtStatus>>,
    theme: ThemeChoice,
    filter: Filter,
    sort: (SortKey, bool),
    selected: BTreeSet<u64>,
    anchor: Option<u64>,
    details: BTreeMap<u64, DetailsState>,
    add: Option<AddDialog>,
    remove: Option<RemoveDialog>,
    new_url: Option<(u64, String, String, Option<String>)>,
    confirm_restart: Option<(u64, String)>,
    scrolled_to: Option<Instant>,
    about_open: bool,
    logo: Option<egui::TextureHandle>,
}

impl ThrennixApp {
    pub fn new(cc: &eframe::CreationContext<'_>, shared: Arc<Shared>) -> Self {
        let _ = shared.ctx.set(cc.egui_ctx.clone());
        let choice = shared.settings().theme;
        theme::apply(&cc.egui_ctx, choice);
        cc.egui_ctx.all_styles_mut(|s| {
            s.spacing.item_spacing = vec2(8.0, 6.0);
            s.spacing.button_padding = vec2(10.0, 4.0);
            s.spacing.interact_size.y = 24.0;
        });
        Self {
            shared,
            toast: None,
            settings_edit: None,
            yt_status: Arc::new(Mutex::new(YtStatus::Checking)),
            theme: choice,
            filter: Filter::All,
            sort: (SortKey::Added, true),
            selected: BTreeSet::new(),
            anchor: None,
            details: BTreeMap::new(),
            add: None,
            remove: None,
            new_url: None,
            confirm_restart: None,
            scrolled_to: None,
            about_open: false,
            logo: None,
        }
    }

    /// Open the About window right away (demo `--about`, for screenshots).
    pub fn with_about(mut self, open: bool) -> Self {
        self.about_open = open;
        self
    }

    /// Open the Add URL dialog prefilled (demo `--add-yt`, for screenshots).
    pub fn with_add_url(mut self, url: Option<String>) -> Self {
        if let Some(url) = url {
            self.add = Some(AddDialog::new(url, self.shared.settings().download_dir));
        }
        self
    }

    fn toast(&mut self, msg: impl Into<String>) {
        self.toast = Some((msg.into(), Instant::now()));
    }

    fn open_add_dialog(&mut self) {
        let clip = arboard::Clipboard::new()
            .and_then(|mut c| c.get_text())
            .map(|t| t.trim().to_string())
            .ok()
            .filter(|t| !t.contains(char::is_whitespace) && validate_url(t).is_ok())
            .unwrap_or_default();
        self.add = Some(AddDialog::new(clip, self.shared.settings().download_dir));
    }

    /// Browser downloads waiting for "Ask where to save". Cancelled = kept as Paused.
    fn process_pending(&mut self) {
        if !self.shared.has_pending_ask() {
            return;
        }
        for nd in self.shared.take_pending_ask() {
            let picked = rfd::FileDialog::new()
                .set_title("Save download to…")
                .set_directory(self.shared.settings().download_dir)
                .pick_folder();
            match picked {
                Some(dir) => {
                    if let Err(e) = self.shared.add(nd, Some(dir), true) {
                        self.toast(e);
                    }
                }
                None => self.shared.add_paused(nd, None),
            }
        }
    }

    fn selected_rows<'a>(&self, rows: &'a [RowView]) -> Vec<&'a RowView> {
        rows.iter()
            .filter(|r| self.selected.contains(&r.id))
            .collect()
    }

    fn ask_remove(&mut self, rows: &[&RowView]) {
        if rows.is_empty() {
            return;
        }
        let label = if rows.len() == 1 {
            format!("Remove \"{}\" from the list?", rows[0].name)
        } else {
            format!("Remove {} downloads from the list?", rows.len())
        };
        self.remove = Some(RemoveDialog {
            ids: rows.iter().map(|r| r.id).collect(),
            label,
            delete: false,
            identifiable: rows.iter().all(|r| r.parts_identifiable),
        });
    }

    // ------------------------------------------------------------ toolbar

    fn toolbar(&mut self, ui: &mut egui::Ui, rows: &[RowView]) {
        let sel = self.selected_rows(rows);
        let can_resume = sel
            .iter()
            .any(|r| !r.running && r.status != Status::Completed);
        let can_pause = sel.iter().any(|r| r.running);
        let any_running = rows.iter().any(|r| r.running);
        let any_done = rows.iter().any(|r| r.status == Status::Completed);
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            if toolbar_button(ui, Icon::Add, "Add URL", true)
                .on_hover_text("Add a download (Ctrl+N)")
                .clicked()
            {
                self.open_add_dialog();
            }
            if toolbar_button(ui, Icon::Resume, "Resume", can_resume).clicked() {
                for r in &sel {
                    self.shared.resume(r.id);
                }
            }
            if toolbar_button(ui, Icon::Pause, "Pause", can_pause).clicked() {
                for r in &sel {
                    self.shared.pause(r.id);
                }
            }
            if toolbar_button(ui, Icon::PauseAll, "Pause all", any_running).clicked() {
                self.shared.pause_all();
            }
            ui.separator();
            if toolbar_button(ui, Icon::Delete, "Delete", !sel.is_empty()).clicked() {
                self.ask_remove(&sel);
            }
            if toolbar_button(ui, Icon::DeleteDone, "Delete compl.", any_done)
                .on_hover_text("Remove finished downloads from the list (files are kept)")
                .clicked()
            {
                self.shared.remove_completed();
            }
            ui.separator();
            if toolbar_button(ui, Icon::Settings, "Settings", true).clicked() {
                if self.settings_edit.is_some() {
                    self.close_settings();
                } else {
                    self.open_settings();
                }
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.add_space(4.0);
                if toolbar_button(ui, Icon::About, "About", true)
                    .on_hover_text("About Thrennix (F1)")
                    .clicked()
                {
                    self.about_open = !self.about_open;
                }
                let tray = crate::tray::is_active();
                if toolbar_button(ui, Icon::ToTray, "To tray", tray)
                    .on_hover_text(if tray {
                        "Hide to the system tray; downloads keep running (Ctrl+H)"
                    } else {
                        "Turn on the tray icon in Settings to use this"
                    })
                    .clicked()
                {
                    crate::tray::hide_to_tray();
                }
            });
        });
        ui.add_space(2.0);
    }

    // ------------------------------------------------------------ sidebar

    fn sidebar(&mut self, ui: &mut egui::Ui, rows: &[RowView]) {
        let pal = ui_palette(ui);
        ui.add_space(8.0);
        ui.label(RichText::new("Categories").strong().color(pal.weak_text));
        ui.add_space(4.0);
        let count = |f: Filter| rows.iter().filter(|r| f.matches(r)).count();
        let mut entry = |ui: &mut egui::Ui, f: Filter, label: &str, indent: f32| {
            let selected = self.filter == f;
            let (rect, resp) =
                ui.allocate_exact_size(vec2(ui.available_width(), 24.0), Sense::click());
            let p = ui.painter();
            if selected {
                p.rect_filled(rect, 4.0, ui.visuals().selection.bg_fill);
            } else if resp.hovered() {
                p.rect_filled(rect, 4.0, pal.accent.gamma_multiply(0.08));
            }
            let text = ui.visuals().text_color();
            p.text(
                rect.left_center() + vec2(8.0 + indent, 0.0),
                Align2::LEFT_CENTER,
                label,
                egui::FontId::proportional(13.0),
                if selected { pal.accent } else { text },
            );
            p.text(
                rect.right_center() - vec2(8.0, 0.0),
                Align2::RIGHT_CENTER,
                count(f).to_string(),
                egui::FontId::proportional(11.5),
                pal.weak_text,
            );
            if resp.clicked() {
                self.filter = f;
            }
        };
        entry(ui, Filter::All, "All downloads", 0.0);
        for c in Category::TYPES {
            entry(ui, Filter::Type(c), c.label(), 14.0);
        }
        ui.add_space(6.0);
        entry(ui, Filter::Unfinished, "Unfinished", 0.0);
        entry(ui, Filter::Finished, "Finished", 0.0);
    }

    // ------------------------------------------------------------ table

    fn sorted<'a>(&self, rows: &'a [RowView]) -> Vec<&'a RowView> {
        let mut v: Vec<&RowView> = rows.iter().filter(|r| self.filter.matches(r)).collect();
        let (key, asc) = self.sort;
        let status_rank = |r: &RowView| match &r.status {
            Status::Downloading | Status::Probing => 0,
            Status::Paused => 1,
            Status::Failed(_) => 2,
            Status::Completed => 3,
        };
        match key {
            // rows arrive newest first
            SortKey::Added => {}
            SortKey::Name => v.sort_by_key(|r| r.name.to_lowercase()),
            SortKey::Size => v.sort_by_key(|r| r.total.unwrap_or(0)),
            SortKey::Status => v.sort_by_key(|r| status_rank(r)),
            SortKey::TimeLeft => v.sort_by(|a, b| {
                let k = |r: &RowView| {
                    if r.running {
                        r.eta_secs.unwrap_or(f64::MAX)
                    } else {
                        f64::MAX
                    }
                };
                k(a).total_cmp(&k(b))
            }),
            SortKey::Rate => v.sort_by(|a, b| a.speed_bps.total_cmp(&b.speed_bps)),
            SortKey::LastTry => v.sort_by_key(|r| r.last_try.unwrap_or(0)),
            SortKey::Host => v.sort_by(|a, b| a.host.cmp(&b.host)),
        }
        if !asc {
            v.reverse();
        }
        v
    }

    fn table(
        &mut self,
        ui: &mut egui::Ui,
        rows: &[RowView],
        highlight: Option<(u64, Instant)>,
        actions: &mut Vec<Action>,
    ) {
        let view = self.sorted(rows);
        let pal = ui_palette(ui);
        let mut scroll_to = None;
        if let Some((id, at)) = highlight {
            if self.scrolled_to != Some(at) {
                self.scrolled_to = Some(at);
                if let Some(i) = view.iter().position(|r| r.id == id) {
                    scroll_to = Some(i);
                    self.selected = BTreeSet::from([id]);
                    self.anchor = Some(id);
                }
            }
        }
        let modifiers = ui.input(|i| i.modifiers);
        let mut table = TableBuilder::new(ui)
            .id_salt("downloads")
            .striped(true)
            .resizable(true)
            .sense(Sense::click())
            .auto_shrink([false, false])
            .cell_layout(Layout::left_to_right(Align::Center))
            .column(Column::initial(300.0).at_least(140.0).clip(true))
            .column(Column::initial(80.0).at_least(50.0))
            .column(Column::initial(120.0).at_least(80.0))
            .column(Column::initial(80.0).at_least(50.0))
            .column(Column::initial(95.0).at_least(60.0))
            .column(Column::initial(120.0).at_least(80.0))
            .column(Column::remainder().at_least(80.0).clip(true));
        if let Some(i) = scroll_to {
            table = table.scroll_to_row(i, Some(Align::Center));
        }
        let headers = [
            ("File Name", SortKey::Name),
            ("Size", SortKey::Size),
            ("Status", SortKey::Status),
            ("Time left", SortKey::TimeLeft),
            ("Transfer rate", SortKey::Rate),
            ("Last try", SortKey::LastTry),
            ("Description", SortKey::Host),
        ];
        let mut clicked_sort = None;
        table
            .header(24.0, |mut header| {
                for (label, key) in headers {
                    header.col(|ui| {
                        table_cell(ui);
                        let arrow = match self.sort {
                            (k, true) if k == key => " ▲",
                            (k, false) if k == key => " ▼",
                            _ => "",
                        };
                        let resp = ui.add(
                            egui::Label::new(RichText::new(format!("{label}{arrow}")).strong())
                                .sense(Sense::click()),
                        );
                        if resp.clicked() {
                            clicked_sort = Some(key);
                        }
                    });
                }
            })
            .body(|body| {
                body.rows(ROW_HEIGHT, view.len(), |mut row| {
                    let r = view[row.index()];
                    row.set_selected(self.selected.contains(&r.id));
                    row.col(|ui| {
                        table_cell(ui);
                        theme::file_glyph(ui, r.category);
                        ui.add(egui::Label::new(&*r.name).truncate().selectable(false));
                    });
                    row.col(|ui| {
                        table_cell(ui);
                        ui.label(r.total.map_or("—".into(), fmt::bytes));
                    });
                    row.col(|ui| {
                        table_cell(ui);
                        status_cell(ui, r, &pal);
                    });
                    let active = r.running && !r.muxing && r.status == Status::Downloading;
                    row.col(|ui| {
                        table_cell(ui);
                        if active {
                            ui.label(r.eta_secs.map_or("—".into(), fmt::eta));
                        }
                    });
                    row.col(|ui| {
                        table_cell(ui);
                        if active {
                            ui.label(fmt::speed(r.speed_bps));
                        }
                    });
                    row.col(|ui| {
                        table_cell(ui);
                        ui.label(r.last_try.map_or(String::new(), format_time));
                    });
                    row.col(|ui| {
                        table_cell(ui);
                        ui.add(
                            egui::Label::new(RichText::new(&*r.host).color(pal.weak_text))
                                .truncate()
                                .selectable(false),
                        );
                    });
                    let resp = row.response();
                    if resp.clicked() {
                        self.click_row(r.id, modifiers, &view);
                    }
                    if resp.double_clicked() {
                        match (&r.status, &r.path) {
                            (Status::Completed, Some(p)) => actions.push(Action::Open(p.clone())),
                            _ => actions.push(Action::Details(r.id)),
                        }
                    }
                    if resp.secondary_clicked() && !self.selected.contains(&r.id) {
                        self.selected = BTreeSet::from([r.id]);
                        self.anchor = Some(r.id);
                    }
                    resp.context_menu(|ui| self.context_menu(ui, r, rows, actions));
                });
            });
        if let Some(key) = clicked_sort {
            self.sort = if self.sort.0 == key {
                (key, !self.sort.1)
            } else {
                (key, true)
            };
        }
    }

    fn click_row(&mut self, id: u64, m: egui::Modifiers, view: &[&RowView]) {
        if m.shift {
            let pos = |x: u64| view.iter().position(|r| r.id == x);
            if let (Some(a), Some(b)) = (self.anchor.and_then(pos), pos(id)) {
                let (lo, hi) = (a.min(b), a.max(b));
                if !m.command {
                    self.selected.clear();
                }
                self.selected.extend(view[lo..=hi].iter().map(|r| r.id));
                return;
            }
        }
        if m.command {
            if !self.selected.remove(&id) {
                self.selected.insert(id);
            }
        } else {
            self.selected = BTreeSet::from([id]);
        }
        self.anchor = Some(id);
    }

    fn context_menu(
        &self,
        ui: &mut egui::Ui,
        r: &RowView,
        rows: &[RowView],
        actions: &mut Vec<Action>,
    ) {
        let targets: Vec<u64> = if self.selected.contains(&r.id) {
            rows.iter()
                .filter(|x| self.selected.contains(&x.id))
                .map(|x| x.id)
                .collect()
        } else {
            vec![r.id]
        };
        let mut item = |ui: &mut egui::Ui, label: &str, enabled: bool, a: Action| {
            if ui.add_enabled(enabled, egui::Button::new(label)).clicked() {
                actions.push(a);
                ui.close_menu();
            }
        };
        let done = r.status == Status::Completed;
        item(
            ui,
            "Open",
            done && r.path.is_some(),
            Action::Open(r.path.clone().unwrap_or_default()),
        );
        item(
            ui,
            "Open folder",
            true,
            Action::Reveal(r.path.clone(), r.dest_dir.clone()),
        );
        item(ui, "Details…", true, Action::Details(r.id));
        ui.separator();
        if r.running {
            item(ui, "Pause", true, Action::Pause(r.id));
        } else {
            item(ui, "Resume", !done, Action::Resume(r.id));
        }
        item(ui, "Copy URL", true, Action::CopyUrl(r.url.to_string()));
        if r.remote_changed {
            item(
                ui,
                "Restart from zero…",
                !r.running,
                Action::Restart(r.id, r.name.to_string()),
            );
        } else {
            item(
                ui,
                "New URL…",
                !r.running && !done,
                Action::NewUrl(r.id, r.name.to_string()),
            );
        }
        ui.separator();
        item(ui, "Remove", true, Action::Remove(targets.clone(), false));
        item(
            ui,
            "Remove + delete file",
            true,
            Action::Remove(targets, true),
        );
    }

    // ------------------------------------------------------------ status bar

    fn status_bar(&self, ui: &mut egui::Ui, rows: &[RowView]) {
        let pal = ui_palette(ui);
        let active = rows.iter().filter(|r| r.running).count();
        let waiting = rows
            .iter()
            .filter(|r| !r.running && matches!(r.status, Status::Paused | Status::Probing))
            .count();
        let speed: f64 = rows.iter().filter(|r| r.running).map(|r| r.speed_bps).sum();
        ui.horizontal(|ui| {
            let t = |s: String| RichText::new(s).small().color(pal.weak_text);
            ui.label(t(format!("↓ {}", fmt::speed(speed))));
            ui.separator();
            ui.label(t(format!("{active} active")));
            ui.separator();
            ui.label(t(format!("{waiting} paused / queued")));
            ui.separator();
            ui.label(t(format!("{} downloads", rows.len())));
            if !self.selected.is_empty() {
                ui.separator();
                ui.label(t(format!("{} selected", self.selected.len())));
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.add(egui::Hyperlink::from_label_and_url(
                    RichText::new(format!("by {}", brand::PUBLISHER))
                        .small()
                        .color(pal.weak_text),
                    brand::WEBSITE,
                ))
                .on_hover_text(brand::WEBSITE);
            });
        });
    }

    // ------------------------------------------------------------ settings

    fn open_settings(&mut self) {
        self.settings_edit = Some(self.shared.settings());
        self.recheck_ytdlp();
    }

    fn close_settings(&mut self) {
        if let Some(s) = self.settings_edit.take() {
            self.shared.apply_settings(s);
        }
    }

    fn recheck_ytdlp(&self) {
        *self.yt_status.lock().unwrap_or_else(|e| e.into_inner()) = YtStatus::Checking;
        let status = self.yt_status.clone();
        let ctx = self.shared.ctx.get().cloned();
        std::thread::spawn(move || {
            let s = match thrennix_media::ytdlp::find_ytdlp() {
                Some(path) => {
                    let version = Command::new(&path)
                        .arg("--version")
                        .creation_flags(CREATE_NO_WINDOW)
                        .output()
                        .ok()
                        .filter(|o| o.status.success())
                        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                        .unwrap_or_else(|| "unknown".into());
                    YtStatus::Found { path, version }
                }
                None => YtStatus::Missing,
            };
            *status.lock().unwrap_or_else(|e| e.into_inner()) = s;
            if let Some(ctx) = ctx {
                ctx.request_repaint();
            }
        });
    }

    fn settings_window(&mut self, ctx: &egui::Context) {
        let Some(mut s) = self.settings_edit.clone() else {
            return;
        };
        let mut open = true;
        let mut commit = false;
        let mut run: Option<&'static str> = None;
        let mut recheck = false;
        let mut show_about = false;
        let yt = self
            .yt_status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        egui::Window::new("Settings")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                egui::Grid::new("settings")
                    .num_columns(2)
                    .spacing([14.0, 10.0])
                    .show(ui, |ui| {
                        ui.label("Theme");
                        egui::ComboBox::from_id_salt("theme")
                            .selected_text(s.theme.label())
                            .show_ui(ui, |ui| {
                                for t in
                                    [ThemeChoice::System, ThemeChoice::Light, ThemeChoice::Dark]
                                {
                                    commit |=
                                        ui.selectable_value(&mut s.theme, t, t.label()).changed();
                                }
                            });
                        ui.end_row();
                        ui.label("Download folder");
                        ui.horizontal(|ui| {
                            ui.label(s.download_dir.display().to_string());
                            if ui.button("Change…").clicked() {
                                if let Some(d) = rfd::FileDialog::new()
                                    .set_directory(&s.download_dir)
                                    .pick_folder()
                                {
                                    s.download_dir = d;
                                    commit = true;
                                }
                            }
                        });
                        ui.end_row();
                        ui.label("Max connections");
                        let r = ui.add(egui::DragValue::new(&mut s.max_connections).range(1..=32));
                        commit |= r.drag_stopped() || r.lost_focus();
                        ui.end_row();
                        ui.label("Initial connections");
                        let r =
                            ui.add(egui::DragValue::new(&mut s.initial_connections).range(1..=32));
                        commit |= r.drag_stopped() || r.lost_focus();
                        ui.end_row();
                        ui.label("");
                        ui.vertical(|ui| {
                            commit |= ui
                                .checkbox(&mut s.ask_where, "Ask where to save each download")
                                .changed();
                            commit |= ui
                                .checkbox(
                                    &mut s.show_progress_window,
                                    "Show download progress window",
                                )
                                .on_hover_text(
                                    "Open the progress window when a browser download starts",
                                )
                                .changed();
                        });
                        ui.end_row();

                        ui.label("yt-dlp");
                        ui.vertical(|ui| match &yt {
                            YtStatus::Checking => {
                                ui.label(RichText::new("checking…").weak());
                            }
                            YtStatus::Found { path, version } => {
                                ui.label(format!("found at {}, version {version}", path.display()));
                                ui.horizontal(|ui| {
                                    if ui
                                        .button("Update yt-dlp")
                                        .on_hover_text("winget upgrade --id yt-dlp.yt-dlp -e")
                                        .clicked()
                                    {
                                        run = Some("winget upgrade --id yt-dlp.yt-dlp -e");
                                    }
                                    recheck |= ui.button("Re-check").clicked();
                                });
                            }
                            YtStatus::Missing => {
                                ui.label("not installed — winget install yt-dlp.yt-dlp");
                                ui.horizontal(|ui| {
                                    if ui.button("Install yt-dlp").clicked() {
                                        run = Some("winget install yt-dlp.yt-dlp");
                                    }
                                    recheck |= ui.button("Re-check").clicked();
                                });
                            }
                        });
                        ui.end_row();
                        ui.label("");
                        commit |= ui
                            .checkbox(
                                &mut s.yt_always_best,
                                "YouTube: always download the highest quality (don't ask)",
                            )
                            .on_hover_text(
                                "Add URL skips the quality list. The browser extension still                                  lets you pick.",
                            )
                            .changed();
                        ui.end_row();
                    });
                ui.add_space(4.0);
                ui.separator();
                // Window / system tray.
                ui.label(RichText::new("Window").strong());
                commit |= ui
                    .checkbox(&mut s.tray_icon, "Show icon in system tray")
                    .changed();
                ui.add_enabled_ui(s.tray_icon, |ui| {
                    ui.indent("tray_options", |ui| {
                        commit |= ui
                            .checkbox(
                                &mut s.minimize_to_tray,
                                "Minimize to tray instead of taskbar",
                            )
                            .changed();
                        commit |= ui
                            .checkbox(
                                &mut s.close_to_tray,
                                "Close button minimizes to tray (keep downloading in background)",
                            )
                            .on_hover_text("Quit from the tray icon's menu")
                            .changed();
                        commit |= ui
                            .checkbox(
                                &mut s.start_minimized,
                                "Start minimized to tray when Windows starts",
                            )
                            .on_hover_text(
                                "Applies when Thrennix starts with Windows (installer option)",
                            )
                            .changed();
                    });
                });
                s.tray_hint_shown |= crate::tray::hint_shown();
                ui.add_space(4.0);
                ui.separator();
                ui.horizontal(|ui| {
                    ui.label(RichText::new("About").strong());
                    ui.label(
                        RichText::new(format!(
                            "Thrennix {} by {}",
                            brand::VERSION,
                            brand::PUBLISHER
                        ))
                        .color(ui_palette(ui).weak_text),
                    );
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui.button("About Thrennix…").clicked() {
                            show_about = true;
                        }
                    });
                });
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing.x = 4.0;
                    for (i, (label, url)) in brand::LINKS.into_iter().enumerate() {
                        if i > 0 {
                            ui.label(RichText::new("·").color(ui_palette(ui).weak_text));
                        }
                        ui.hyperlink_to(RichText::new(label).color(ui_palette(ui).accent), url)
                            .on_hover_text(url);
                    }
                });
            });
        if show_about {
            self.about_open = true;
        }
        if let Some(cmd) = run {
            // Visible console so the user sees (and can answer) winget.
            let spawned = Command::new("cmd.exe")
                .raw_arg(format!("/k {cmd}"))
                .creation_flags(CREATE_NEW_CONSOLE)
                .spawn();
            if let Err(e) = spawned {
                self.toast(format!("cannot run winget: {e}"));
            }
        }
        if recheck {
            self.recheck_ytdlp();
        }
        if commit {
            s.normalize();
            self.shared.apply_settings(s.clone());
        }
        if s.theme != self.theme {
            self.theme = s.theme;
            theme::apply(ctx, s.theme);
        }
        self.settings_edit = Some(s);
        if !open {
            self.close_settings();
        }
    }

    // ------------------------------------------------------------ dialogs

    fn dialogs(&mut self, ctx: &egui::Context, rows: &[RowView]) {
        self.settings_window(ctx);

        if let Some(id) = self.shared.take_restart_prompt() {
            let name = rows
                .iter()
                .find(|r| r.id == id)
                .map(|r| r.name.to_string())
                .unwrap_or_default();
            self.confirm_restart = Some((id, name));
        }

        if let Some(mut d) = self.add.take() {
            if add_url::show(ctx, &mut d, &self.shared) {
                self.add = Some(d);
            }
        }

        if let Some(mut d) = self.remove.take() {
            let (mut confirm, mut cancel) = (false, false);
            egui::Window::new("Remove download")
                .collapsible(false)
                .resizable(false)
                .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    ui.label(&d.label);
                    ui.checkbox(&mut d.delete, "Also delete the file");
                    if d.delete && !d.identifiable {
                        ui.label(
                            RichText::new(
                                "yt-dlp's partial files can't be identified safely yet; \
                                 they will be left in the download folder.",
                            )
                            .small()
                            .color(ui.visuals().warn_fg_color),
                        );
                    }
                    ui.horizontal(|ui| {
                        confirm = ui.button("Remove").clicked();
                        cancel = ui.button("Cancel").clicked();
                    });
                });
            if confirm {
                for id in &d.ids {
                    self.shared.remove(*id, d.delete);
                    self.selected.remove(id);
                    self.details.remove(id);
                }
            } else if !cancel && !ctx.input(|i| i.key_pressed(Key::Escape)) {
                self.remove = Some(d);
            }
        }

        if let Some((id, name)) = self.confirm_restart.take() {
            let (mut yes, mut no) = (false, false);
            egui::Window::new("File changed on server")
                .collapsible(false)
                .resizable(false)
                .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    ui.label(format!(
                        "\"{name}\" changed on the server since the download started, so the \
                         downloaded part can't be reused."
                    ));
                    ui.label("Restart from zero?");
                    ui.horizontal(|ui| {
                        yes = ui.button("Restart from zero").clicked();
                        no = ui.button("Not now").clicked();
                    });
                });
            if yes {
                self.shared.restart_from_zero(id);
            } else if !no && !ctx.input(|i| i.key_pressed(Key::Escape)) {
                self.confirm_restart = Some((id, name));
            }
        }

        if let Some((id, name, mut url, mut err)) = self.new_url.take() {
            let (mut ok, mut cancel) = (false, false);
            egui::Window::new("New URL")
                .collapsible(false)
                .resizable(false)
                .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    ui.label(format!("Paste a fresh link for \"{name}\":"));
                    let resp = ui.add(TextEdit::singleline(&mut url).desired_width(380.0));
                    if url.is_empty() && !resp.has_focus() {
                        resp.request_focus();
                    }
                    if resp.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                        ok = true;
                    }
                    if let Some(e) = &err {
                        ui.colored_label(ui.visuals().error_fg_color, e);
                    }
                    ui.horizontal(|ui| {
                        ok |= ui.button("Continue").clicked();
                        cancel = ui.button("Cancel").clicked();
                    });
                });
            if ok {
                match self.shared.retry_with_url(id, &url) {
                    Ok(()) => return,
                    Err(e) => err = Some(e),
                }
            }
            if !cancel && !ctx.input(|i| i.key_pressed(Key::Escape)) {
                self.new_url = Some((id, name, url, err));
            }
        }
    }

    /// One OS window per open details entry.
    fn details_windows(&mut self, ctx: &egui::Context, rows: &[RowView]) {
        self.details
            .retain(|id, _| rows.iter().any(|r| r.id == *id));
        let ids: Vec<u64> = self.details.keys().copied().collect();
        for id in ids {
            let Some(r) = rows.iter().find(|r| r.id == id) else {
                continue;
            };
            let st = self.details.get_mut(&id).expect("present");
            let size = if st.collapsed {
                details::SIZE_COLLAPSED
            } else {
                details::SIZE_EXPANDED
            };
            let builder = ViewportBuilder::default()
                .with_title(details::title(r))
                .with_inner_size(size)
                .with_min_inner_size([480.0, 260.0])
                .with_icon(crate::app_icon());
            let actions = ctx.show_viewport_immediate(
                ViewportId::from_hash_of(("thrennix-details", id)),
                builder,
                |ctx, class| {
                    let mut out = Vec::new();
                    if class == ViewportClass::Embedded {
                        let mut open = true;
                        egui::Window::new(details::title(r))
                            .open(&mut open)
                            .show(ctx, |ui| {
                                out = details::show(ui, r, st);
                            });
                        if !open {
                            out.push(DetailsAction::Close);
                        }
                    } else {
                        egui::CentralPanel::default()
                            .show(ctx, |ui| out = details::show(ui, r, st));
                        if ctx.input(|i| i.viewport().close_requested()) {
                            out.push(DetailsAction::Close);
                        }
                    }
                    for a in &out {
                        if let DetailsAction::Resize(s) = a {
                            ctx.send_viewport_cmd(ViewportCommand::InnerSize(vec2(s[0], s[1])));
                        }
                    }
                    out
                },
            );
            for a in actions {
                match a {
                    DetailsAction::Pause => self.shared.pause(id),
                    DetailsAction::Resume => self.shared.resume(id),
                    DetailsAction::Cancel(delete) => {
                        self.details.remove(&id);
                        if delete {
                            self.shared.remove(id, true);
                            self.selected.remove(&id);
                        }
                    }
                    DetailsAction::Options {
                        open_folder,
                        open_file,
                    } => self.shared.set_item_options(id, open_folder, open_file),
                    DetailsAction::Resize(_) => {}
                    DetailsAction::Close => {
                        self.details.remove(&id);
                    }
                }
            }
        }
    }

    /// The About window, as its own OS window.
    fn about_window(&mut self, ctx: &egui::Context) {
        if !self.about_open {
            return;
        }
        let logo = self.logo.get_or_insert_with(|| about::logo(ctx)).clone();
        let builder = ViewportBuilder::default()
            .with_title(about::TITLE)
            .with_inner_size(about::SIZE)
            .with_resizable(false)
            .with_minimize_button(false)
            .with_maximize_button(false)
            .with_icon(crate::app_icon());
        let close = ctx.show_viewport_immediate(
            ViewportId::from_hash_of("thrennix-about"),
            builder,
            |ctx, class| {
                if class == ViewportClass::Embedded {
                    let mut open = true;
                    let mut close = false;
                    egui::Window::new(about::TITLE)
                        .open(&mut open)
                        .collapsible(false)
                        .resizable(false)
                        .show(ctx, |ui| close = about::show(ui, &logo));
                    close || !open
                } else {
                    let mut close = false;
                    // Edge-to-edge: the About page draws its own header band and padding.
                    egui::CentralPanel::default()
                        .frame(egui::Frame::central_panel(&ctx.style()).inner_margin(0))
                        .show(ctx, |ui| close = about::show(ui, &logo));
                    close || ctx.input(|i| i.viewport().close_requested())
                }
            },
        );
        if close {
            self.about_open = false;
        }
    }

    fn apply(&mut self, actions: Vec<Action>, ctx: &egui::Context, rows: &[RowView]) {
        for a in actions {
            match a {
                Action::Pause(id) => self.shared.pause(id),
                Action::Resume(id) => self.shared.resume(id),
                Action::Open(p) => {
                    if let Err(e) = opener::open(&p) {
                        self.toast(format!("Cannot open {} ({e})", p.display()));
                    }
                }
                Action::Reveal(path, dir) => {
                    if let Err(msg) = reveal(path.as_deref(), &dir) {
                        self.toast(msg);
                    }
                }
                Action::CopyUrl(u) => ctx.copy_text(u),
                Action::NewUrl(id, name) => self.new_url = Some((id, name, String::new(), None)),
                Action::Restart(id, name) => self.confirm_restart = Some((id, name)),
                Action::Details(id) => {
                    self.details.entry(id).or_default();
                }
                Action::Remove(ids, delete) => {
                    let sel: Vec<&RowView> = rows.iter().filter(|r| ids.contains(&r.id)).collect();
                    self.ask_remove(&sel);
                    if let Some(d) = &mut self.remove {
                        d.delete = delete;
                    }
                }
            }
        }
    }

    fn shortcuts(&mut self, ctx: &egui::Context, rows: &[RowView]) {
        if ctx.input(|i| i.key_pressed(Key::F1)) {
            self.about_open = true;
        }
        if ctx.wants_keyboard_input() || self.add.is_some() || self.remove.is_some() {
            return;
        }
        let (del, all, new) = ctx.input(|i| {
            (
                i.key_pressed(Key::Delete),
                i.modifiers.command && i.key_pressed(Key::A),
                i.modifiers.command && i.key_pressed(Key::N),
            )
        });
        if del {
            let sel = self.selected_rows(rows);
            self.ask_remove(&sel);
        }
        if all {
            self.selected = rows
                .iter()
                .filter(|r| self.filter.matches(r))
                .map(|r| r.id)
                .collect();
        }
        if new {
            self.open_add_dialog();
        }
    }
}

impl eframe::App for ThrennixApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Tray: close-to-tray (cancels the close), Ctrl+H, requests from the tray menu.
        for req in crate::tray::on_frame(ctx, &self.shared) {
            match req {
                crate::tray::UiRequest::Settings if self.settings_edit.is_none() => {
                    self.open_settings()
                }
                crate::tray::UiRequest::Settings => {}
                crate::tray::UiRequest::About => self.about_open = true,
            }
        }
        self.process_pending();
        for id in self.shared.take_open_details() {
            self.details.entry(id).or_default();
        }
        let rows = self.shared.snapshot();
        self.selected.retain(|id| rows.iter().any(|r| r.id == *id));
        let mut actions = Vec::new();
        let highlight = self
            .shared
            .take_highlight()
            .filter(|(_, t)| t.elapsed() < HIGHLIGHT_TIME);
        let pal = palette_of(ctx);

        egui::TopBottomPanel::top("toolbar")
            .frame(egui::Frame::side_top_panel(&ctx.style()).fill(pal.sidebar))
            .show(ctx, |ui| self.toolbar(ui, &rows));
        egui::TopBottomPanel::bottom("status")
            .frame(egui::Frame::side_top_panel(&ctx.style()).fill(pal.sidebar))
            .show(ctx, |ui| self.status_bar(ui, &rows));
        if let Some((msg, at)) = &self.toast {
            if at.elapsed() < TOAST_TIME {
                let msg = msg.clone();
                egui::TopBottomPanel::bottom("toast").show(ctx, |ui| {
                    ui.horizontal(|ui| {
                        ui.colored_label(ui.visuals().warn_fg_color, &msg);
                        if ui.small_button("Copy").clicked() {
                            ui.ctx().copy_text(msg.clone());
                        }
                    });
                });
                ctx.request_repaint_after(Duration::from_millis(500));
            } else {
                self.toast = None;
            }
        }
        egui::SidePanel::left("categories")
            .resizable(false)
            .exact_width(180.0)
            .frame(egui::Frame::side_top_panel(&ctx.style()).fill(pal.sidebar))
            .show(ctx, |ui| self.sidebar(ui, &rows));
        egui::CentralPanel::default()
            .frame(egui::Frame::central_panel(&ctx.style()).inner_margin(egui::Margin::ZERO))
            .show(ctx, |ui| {
                if rows.iter().any(|r| self.filter.matches(r)) {
                    self.table(ui, &rows, highlight, &mut actions);
                } else {
                    ui.centered_and_justified(|ui| {
                        ui.label(
                            RichText::new(
                                "No downloads here. Use Add URL or download from the browser.",
                            )
                            .weak(),
                        );
                    });
                }
            });
        self.dialogs(ctx, &rows);
        self.apply(actions, ctx, &rows);
        self.shortcuts(ctx, &rows);
        self.details_windows(ctx, &rows);
        self.about_window(ctx);

        // Only tick while something is moving; idle uses no CPU.
        if rows.iter().any(|r| r.running) {
            ctx.request_repaint_after(Duration::from_millis(100));
        } else if highlight.is_some() {
            ctx.request_repaint_after(Duration::from_millis(250));
        }
    }
}

fn palette_of(ctx: &egui::Context) -> theme::Palette {
    theme::palette(ctx.style().visuals.dark_mode)
}

/// Left inset of every download-table cell (header and body), measured from
/// the cell's edge; with the table's 8 px column gap this puts content ~10 px
/// right of the column divider.
const CELL_PAD: f32 = 6.0;

/// Common setup for a download-table cell: consistent left padding, and
/// non-selectable labels so text never captures clicks (or shows an I-beam)
/// — the row itself senses clicks, double-clicks and right-clicks.
fn table_cell(ui: &mut egui::Ui) {
    ui.style_mut().interaction.selectable_labels = false;
    ui.add_space(CELL_PAD);
}

/// "42.3%" / "Complete" / "Paused" / "Merging…" / "Error" with a thin bar under it.
fn status_cell(ui: &mut egui::Ui, r: &RowView, pal: &theme::Palette) {
    let frac = match (r.total, &r.status) {
        (_, Status::Completed) => 1.0,
        (Some(t), _) if t > 0 => (r.downloaded as f64 / t as f64) as f32,
        _ => 0.0,
    };
    let (text, color) = match &r.status {
        Status::Completed => ("Complete".to_string(), pal.ok),
        Status::Failed(_) if r.remote_changed => ("Changed on server".into(), pal.error),
        Status::Failed(_) => ("Error".into(), pal.error),
        Status::Paused => ("Paused".into(), pal.paused),
        _ if r.muxing => (
            if r.kind == Kind::Ytdlp {
                "Merging…"
            } else {
                "Muxing…"
            }
            .into(),
            pal.accent,
        ),
        Status::Probing => ("Starting…".into(), pal.weak_text),
        Status::Downloading if r.total.is_some() => (format!("{:.1}%", frac * 100.0), pal.accent),
        Status::Downloading => ("Downloading".into(), pal.accent),
    };
    let rect = ui.available_rect_before_wrap();
    let resp = ui.allocate_rect(rect, Sense::hover());
    let p = ui.painter();
    let show_bar = r.running || matches!(r.status, Status::Paused | Status::Failed(_));
    let text_pos = if show_bar {
        rect.left_center() - vec2(0.0, 4.0)
    } else {
        rect.left_center()
    };
    p.text(
        text_pos,
        Align2::LEFT_CENTER,
        text,
        egui::FontId::proportional(12.5),
        color,
    );
    if show_bar {
        let bar = Rect::from_min_size(
            egui::pos2(rect.left(), rect.center().y + 6.0),
            vec2((rect.width() - 6.0).max(10.0), 3.0),
        );
        theme::thin_bar(p, bar, if r.muxing { 1.0 } else { frac }, color, pal.track);
    }
    if let Status::Failed(e) = &r.status {
        resp.on_hover_text(e);
    }
}

fn format_time(secs: i64) -> String {
    use chrono::{Local, TimeZone};
    match Local.timestamp_opt(secs, 0).single() {
        Some(t) => {
            let now = Local::now();
            if t.date_naive() == now.date_naive() {
                t.format("Today %H:%M").to_string()
            } else {
                t.format("%b %d %H:%M").to_string()
            }
        }
        None => String::new(),
    }
}
