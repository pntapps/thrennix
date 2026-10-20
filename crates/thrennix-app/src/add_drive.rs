//! The "Add network drive" dialog: shared folders found automatically on the network,
//! where the folder is, how to sign in, and a connect-and-write test. Searching and
//! connecting run in the background (a sleeping router can take a while to answer).

use std::sync::{Arc, Mutex};

use eframe::egui::{self, vec2, Key, RichText, TextEdit};

use crate::netdrive::{self, Found, NetDrive};
use crate::theme::ui_palette;

type Slot<T> = Arc<Mutex<Option<Result<T, String>>>>;

/// Run `f` on a thread; its result lands in the returned slot.
fn spawn<T: Send + 'static>(
    ctx: &egui::Context,
    f: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Slot<T> {
    let slot: Slot<T> = Arc::new(Mutex::new(None));
    let (out, ctx) = (slot.clone(), ctx.clone());
    std::thread::spawn(move || {
        let res = f();
        *out.lock().unwrap_or_else(|e| e.into_inner()) = Some(res);
        ctx.request_repaint();
    });
    slot
}

fn take<T>(slot: &Slot<T>) -> Option<Result<T, String>> {
    slot.lock().unwrap_or_else(|e| e.into_inner()).take()
}

pub struct AddDriveDialog {
    address: String,
    user: String,
    password: String,
    make_default: bool,
    err: Option<String>,
    /// Shared folders found so far.
    found: Vec<Found>,
    /// Network search running.
    searching: Option<Slot<Vec<Found>>>,
    searched: bool,
    /// Listing a server's folders after signing in.
    listing: Option<Slot<Vec<Found>>>,
    /// Test running for this drive.
    testing: Option<(NetDrive, Slot<()>)>,
}

pub enum Outcome {
    Open,
    Cancelled,
    Added { drive: NetDrive, make_default: bool },
}

impl AddDriveDialog {
    pub fn new() -> Self {
        Self {
            address: String::new(),
            user: String::new(),
            password: String::new(),
            make_default: true,
            err: None,
            found: Vec::new(),
            searching: None,
            searched: false,
            listing: None,
            testing: None,
        }
    }

    fn search(&mut self, ctx: &egui::Context) {
        self.searched = true;
        self.searching = Some(spawn(ctx, || Ok(netdrive::discover())));
    }

    fn connect(&mut self, ctx: &egui::Context) {
        self.err = None;
        // Just a server: sign in and list its folders to pick from.
        if let Some(server) = netdrive::parse_server(&self.address) {
            let (user, password) = (self.user.clone(), self.password.clone());
            self.listing = Some(spawn(ctx, move || {
                netdrive::list_shares(&server, &user, &password)
            }));
            return;
        }
        let path = match netdrive::parse_address(&self.address) {
            Ok(p) => p,
            Err(e) => {
                self.err = Some(e);
                return;
            }
        };
        let drive = NetDrive {
            path,
            user: self.user.trim().to_string(),
            password: Some(self.password.clone()).filter(|p| !p.is_empty()),
        };
        let d = drive.clone();
        self.testing = Some((drive, spawn(ctx, move || netdrive::test(&d))));
    }

    /// Pick up finished background work. `Some` once the drive is ready to save.
    fn poll(&mut self) -> Option<NetDrive> {
        if let Some(res) = self.searching.as_ref().and_then(take) {
            self.searching = None;
            for f in res.unwrap_or_default() {
                if !self
                    .found
                    .iter()
                    .any(|x| x.path.eq_ignore_ascii_case(&f.path))
                {
                    self.found.push(f);
                }
            }
        }
        if let Some(res) = self.listing.as_ref().and_then(take) {
            self.listing = None;
            match res {
                Ok(shares) => {
                    // The server's folders replace the server itself in the list.
                    let server = self
                        .address
                        .trim()
                        .trim_end_matches(['\\', '/'])
                        .to_string();
                    self.found
                        .retain(|f| !(f.is_server_only() && f.path.eq_ignore_ascii_case(&server)));
                    for f in shares.into_iter().rev() {
                        if !self
                            .found
                            .iter()
                            .any(|x| x.path.eq_ignore_ascii_case(&f.path))
                        {
                            self.found.insert(0, f);
                        }
                    }
                    self.err = Some("Signed in. Pick a folder above, then Connect.".into());
                }
                Err(e) => self.err = Some(e),
            }
        }
        if let Some((drive, slot)) = &self.testing {
            match take(slot) {
                Some(Ok(())) => return Some(drive.clone()),
                Some(Err(e)) => {
                    self.err = Some(e);
                    self.testing = None;
                }
                None => {}
            }
        }
        None
    }
}

/// Draw the dialog.
pub fn show(ctx: &egui::Context, d: &mut AddDriveDialog) -> Outcome {
    if !d.searched {
        d.search(ctx);
    }
    if let Some(drive) = d.poll() {
        return Outcome::Added {
            drive,
            make_default: d.make_default,
        };
    }
    let busy = d.testing.is_some() || d.listing.is_some();
    let (mut connect, mut cancel, mut search) = (false, false, false);
    let anchor = crate::touch::typing_dialog_anchor();
    egui::Window::new("Add network drive")
        .collapsible(false)
        .resizable(false)
        .anchor(anchor.0, anchor.1)
        .show(ctx, |ui| {
            let weak = ui_palette(ui).weak_text;
            ui.label(
                RichText::new(
                    "Download straight to a shared folder on your router, NAS or another PC \
                     (Windows file sharing / Samba).",
                )
                .color(weak),
            );
            ui.add_space(4.0);

            // Found automatically.
            ui.horizontal(|ui| {
                ui.label(RichText::new("On your network").strong());
                if d.searching.is_some() {
                    ui.spinner();
                    ui.label(RichText::new("searching…").color(weak));
                } else {
                    search = ui.button("Search again").clicked();
                }
            });
            if d.found.is_empty() && d.searching.is_none() {
                ui.label(
                    RichText::new(
                        "Nothing found automatically. Check that file sharing (Samba / SMB) \
                         is on in the router, or type the address below.",
                    )
                    .color(weak),
                );
            }
            ui.add_enabled_ui(!busy, |ui| {
                egui::ScrollArea::vertical()
                    .max_height(200.0)
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        for f in &d.found {
                            let picked = d.address.eq_ignore_ascii_case(&f.path);
                            // Full-width, left-aligned: easy to hit with a finger.
                            let resp = ui
                                .with_layout(
                                    egui::Layout::top_down_justified(egui::Align::LEFT),
                                    |ui| {
                                        ui.selectable_label(
                                            picked,
                                            RichText::new(format!("{}   ", f.path)).strong(),
                                        )
                                    },
                                )
                                .inner;
                            ui.label(RichText::new(&f.note).small().color(weak));
                            if resp.clicked() {
                                d.address = f.path.clone();
                                d.err = f.is_server_only().then(|| {
                                    "Enter the user name and password for this server, then \
                                     Connect to see its folders."
                                        .to_string()
                                });
                            }
                        }
                    });
            });
            ui.add_space(4.0);
            ui.separator();

            ui.add_enabled_ui(!busy, |ui| {
                egui::Grid::new("add_drive")
                    .num_columns(2)
                    .spacing([10.0, 8.0])
                    .show(ui, |ui| {
                        ui.label("Folder");
                        let resp = ui.add(
                            TextEdit::singleline(&mut d.address)
                                .hint_text(r"\\192.168.1.1\usb\Downloads")
                                .desired_width(320.0),
                        );
                        connect |= resp.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                        ui.end_row();
                        ui.label("User name");
                        ui.add(
                            TextEdit::singleline(&mut d.user)
                                .hint_text("empty: guest / your Windows account")
                                .desired_width(320.0),
                        );
                        ui.end_row();
                        ui.label("Password");
                        let resp = ui.add(
                            TextEdit::singleline(&mut d.password)
                                .password(true)
                                .desired_width(320.0),
                        );
                        connect |= resp.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                        ui.end_row();
                    });
                ui.label(
                    RichText::new(
                        r"Also accepts smb://192.168.1.1/usb. Missing subfolders are created.",
                    )
                    .small()
                    .color(weak),
                );
                ui.checkbox(&mut d.make_default, "Save downloads here by default");
            });
            if let Some(e) = &d.err {
                ui.add_space(2.0);
                let color = if e.starts_with("Signed in") || e.starts_with("Enter the user") {
                    weak
                } else {
                    ui.visuals().error_fg_color
                };
                ui.colored_label(color, e);
            }
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                connect |= ui
                    .add_enabled(
                        !busy && !d.address.trim().is_empty(),
                        egui::Button::new("Connect").min_size(vec2(90.0, 0.0)),
                    )
                    .clicked();
                cancel = ui.button("Cancel").clicked();
                if busy {
                    ui.spinner();
                    ui.label("Connecting…");
                }
            });
        });
    if search && d.searching.is_none() {
        d.search(ctx);
    }
    if connect && !busy && !d.address.trim().is_empty() {
        d.connect(ctx);
    }
    // Work still running is simply forgotten.
    if cancel || ctx.input(|i| i.key_pressed(Key::Escape)) {
        return Outcome::Cancelled;
    }
    Outcome::Open
}
