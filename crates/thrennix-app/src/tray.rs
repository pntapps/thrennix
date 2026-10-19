//! System-tray (notification area) icon: menu, live tooltip, hiding the main window.
//!
//! Everything here runs on the main (winit) thread. Tray and menu events arrive through
//! tray-icon's own hidden window, so they are handled even while the main window is hidden
//! and eframe stops calling `update()`. Showing and hiding go straight to Win32; the work
//! is deferred to a zero-delay thread timer so it never runs inside another window
//! procedure (winit's or tray-icon's).

use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use eframe::egui::{self, Key, ViewportCommand};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use thrennix_core::Status;
use tray_icon::menu::{ContextMenu, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::System::Threading::GetCurrentThreadId;
use windows_sys::Win32::UI::Shell::{DefSubclassProc, SetWindowSubclass};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    IsIconic, IsWindowVisible, KillTimer, SetForegroundWindow, SetMenuDefaultItem, SetTimer,
    ShowWindow, ShowWindowAsync, SC_MINIMIZE, SW_HIDE, SW_RESTORE, SW_SHOW, SW_SHOWMINNOACTIVE,
    WM_SYSCOMMAND,
};

use crate::fmt;
use crate::state::{RowView, Shared};

const TICK_MS: u32 = 500;
const TOOLTIP_EVERY: Duration = Duration::from_secs(1);
/// Size of the icon handed to the shell (it scales to 16-24 px; 128 px looks jagged).
const ICON_PX: u32 = 32;

/// What the tray asks the UI to do on its next frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UiRequest {
    Settings,
    About,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Cmd {
    Open,
    Hide,
    Minimize,
    PauseAll,
    ResumeAll,
    Settings,
    About,
    Quit,
}

impl Cmd {
    fn from_menu_id(id: &str) -> Option<Cmd> {
        Some(match id {
            "open" => Cmd::Open,
            "pause_all" => Cmd::PauseAll,
            "resume_all" => Cmd::ResumeAll,
            "settings" => Cmd::Settings,
            "about" => Cmd::About,
            "quit" => Cmd::Quit,
            _ => return None,
        })
    }
}

struct TrayState {
    icon: TrayIcon,
    pause_all: MenuItem,
    resume_all: MenuItem,
    tooltip: String,
}

thread_local! {
    static TRAY: RefCell<Option<TrayState>> = const { RefCell::new(None) };
}

static SHARED: OnceLock<Arc<Shared>> = OnceLock::new();
static HWND_MAIN: AtomicIsize = AtomicIsize::new(0);
static MAIN_THREAD: AtomicU32 = AtomicU32::new(0);
static PENDING: Mutex<Vec<Cmd>> = Mutex::new(Vec::new());
static UI_REQUESTS: Mutex<Vec<UiRequest>> = Mutex::new(Vec::new());
static LAST_TOOLTIP: Mutex<Option<Instant>> = Mutex::new(None);
/// Cached settings, readable from window procedures without cloning `Settings`.
static MIN_TO_TRAY: AtomicBool = AtomicBool::new(false);
static CLOSE_TO_TRAY: AtomicBool = AtomicBool::new(false);
static HINT_SHOWN: AtomicBool = AtomicBool::new(false);
/// `--minimized`: hide right after eframe has shown the window for its first frame.
static START_HIDDEN: AtomicBool = AtomicBool::new(false);
static QUITTING: AtomicBool = AtomicBool::new(false);

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn hwnd() -> HWND {
    HWND_MAIN.load(Ordering::Acquire) as HWND
}

fn on_main_thread() -> bool {
    // SAFETY: no preconditions.
    MAIN_THREAD.load(Ordering::Relaxed) == unsafe { GetCurrentThreadId() }
}

/// Set up the tray for the main window. Call once, on the main thread, from the app's
/// creation callback.
pub fn init(cc: &eframe::CreationContext<'_>, shared: Arc<Shared>, start_hidden: bool) {
    // SAFETY: no preconditions.
    MAIN_THREAD.store(unsafe { GetCurrentThreadId() }, Ordering::Relaxed);
    if let Ok(h) = cc.window_handle() {
        if let RawWindowHandle::Win32(w) = h.as_raw() {
            let hwnd = w.hwnd.get() as HWND;
            HWND_MAIN.store(hwnd as isize, Ordering::Release);
            // SAFETY: `hwnd` is our live top-level window, subclassed on its own thread.
            unsafe { SetWindowSubclass(hwnd, Some(subclass_proc), 1, 0) };
        }
    }
    HINT_SHOWN.store(shared.settings().tray_hint_shown, Ordering::Relaxed);
    let _ = SHARED.set(shared.clone());
    TrayIconEvent::set_event_handler(Some(|e: TrayIconEvent| match e {
        TrayIconEvent::Click {
            button: MouseButton::Left,
            button_state: MouseButtonState::Up,
            ..
        }
        | TrayIconEvent::DoubleClick {
            button: MouseButton::Left,
            ..
        } => queue(Cmd::Open),
        _ => {}
    }));
    MenuEvent::set_event_handler(Some(|e: MenuEvent| {
        if let Some(cmd) = Cmd::from_menu_id(&e.id.0) {
            queue(cmd);
        }
    }));
    sync(&shared);
    START_HIDDEN.store(start_hidden, Ordering::Relaxed);
    // SAFETY: a thread timer with a callback, dispatched by winit's message loop.
    unsafe { SetTimer(std::ptr::null_mut(), 0, TICK_MS, Some(tick)) };
}

/// The tray icon exists (so hiding the window leaves a way back).
pub fn is_active() -> bool {
    // A borrow in progress means the icon is being created or removed right now.
    TRAY.with(|t| t.try_borrow().map_or(true, |t| t.is_some()))
}

/// The close-to-tray notification was shown (keeps the settings dialog from resetting it).
pub fn hint_shown() -> bool {
    HINT_SHOWN.load(Ordering::Relaxed)
}

/// Hide the main window to the tray (the "To tray" button). No-op without a tray icon.
pub fn hide_to_tray() {
    queue(Cmd::Hide);
}

/// Per-frame hook from `update()`: close-to-tray, Ctrl+H, `--minimized`, tray requests.
pub fn on_frame(ctx: &egui::Context, shared: &Arc<Shared>) -> Vec<UiRequest> {
    sync(shared);
    if START_HIDDEN.swap(false, Ordering::Relaxed) {
        // Deferred: eframe makes the window visible after this first frame is painted.
        // Without a tray icon, minimize to the taskbar instead.
        queue(if is_active() {
            Cmd::Hide
        } else {
            Cmd::Minimize
        });
    }
    let close = ctx.input(|i| i.viewport().close_requested());
    if close
        && !QUITTING.load(Ordering::Relaxed)
        && CLOSE_TO_TRAY.load(Ordering::Relaxed)
        && is_active()
    {
        ctx.send_viewport_cmd(ViewportCommand::CancelClose);
        queue(Cmd::Hide);
        first_close_hint(shared);
    }
    let hide_key =
        !ctx.wants_keyboard_input() && ctx.input(|i| i.modifiers.command && i.key_pressed(Key::H));
    if hide_key {
        queue(Cmd::Hide);
    }
    std::mem::take(&mut *lock(&UI_REQUESTS))
}

/// Bring the main window back (tray click, "Open", single-instance `show`). Any thread.
pub fn show_window() {
    let h = hwnd();
    if h.is_null() {
        return;
    }
    // SAFETY: `h` is our main window; these calls tolerate a destroyed window.
    unsafe {
        let cmd = if IsIconic(h) != 0 {
            SW_RESTORE
        } else {
            SW_SHOW
        };
        if on_main_thread() {
            ShowWindow(h, cmd);
        } else {
            // Never block another thread on the UI thread.
            ShowWindowAsync(h, cmd);
        }
        SetForegroundWindow(h);
    }
    if let Some(ctx) = SHARED.get().and_then(|s| s.ctx.get()) {
        ctx.request_repaint();
    }
}

/// Remove the tray icon (on exit, so no ghost icon stays behind). Main thread.
pub fn remove() {
    TRAY.with(|t| t.borrow_mut().take());
}

/// Tooltip text: "Thrennix", plus a summary while downloads are running.
pub fn tooltip_text(downloading: usize, speed_bps: f64) -> String {
    if downloading == 0 {
        "Thrennix".into()
    } else {
        format!(
            "Thrennix — {downloading} downloading · {}",
            fmt::speed(speed_bps)
        )
    }
}

// ---------------------------------------------------------------- internals

fn queue(cmd: Cmd) {
    lock(&PENDING).push(cmd);
    // SAFETY: a one-shot thread timer; `run_pending` kills it.
    unsafe { SetTimer(std::ptr::null_mut(), 0, 0, Some(run_pending)) };
}

unsafe extern "system" fn run_pending(_: HWND, _: u32, id: usize, _: u32) {
    KillTimer(std::ptr::null_mut(), id);
    let cmds = std::mem::take(&mut *lock(&PENDING));
    for cmd in cmds {
        run(cmd);
    }
}

unsafe extern "system" fn tick(_: HWND, _: u32, _: usize, _: u32) {
    let Some(shared) = SHARED.get() else { return };
    if QUITTING.load(Ordering::Relaxed) {
        return;
    }
    sync(shared);
    let h = hwnd();
    if !h.is_null() && MIN_TO_TRAY.load(Ordering::Relaxed) && is_active() {
        // Fallback for minimize paths that bypass WM_SYSCOMMAND.
        if IsIconic(h) != 0 && IsWindowVisible(h) != 0 {
            ShowWindow(h, SW_HIDE);
        }
    }
    let due = {
        let mut last = lock(&LAST_TOOLTIP);
        let due = last.is_none_or(|t| t.elapsed() >= TOOLTIP_EVERY);
        if due {
            *last = Some(Instant::now());
        }
        due
    };
    if due && is_active() {
        refresh(&shared.snapshot());
    }
}

/// Our window's subclass: "minimize to tray" replaces the native minimize.
unsafe extern "system" fn subclass_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _id: usize,
    _data: usize,
) -> LRESULT {
    if msg == WM_SYSCOMMAND
        && (wparam & 0xFFF0) == SC_MINIMIZE as usize
        && MIN_TO_TRAY.load(Ordering::Relaxed)
        && is_active()
    {
        queue(Cmd::Hide);
        return 0;
    }
    DefSubclassProc(hwnd, msg, wparam, lparam)
}

fn run(cmd: Cmd) {
    let Some(shared) = SHARED.get() else { return };
    match cmd {
        Cmd::Open => show_window(),
        Cmd::Hide => {
            let h = hwnd();
            if !h.is_null() && is_active() {
                // SAFETY: our main window, on its own thread.
                unsafe { ShowWindow(h, SW_HIDE) };
            }
        }
        Cmd::Minimize => {
            let h = hwnd();
            if !h.is_null() {
                // SAFETY: our main window, on its own thread.
                unsafe { ShowWindow(h, SW_SHOWMINNOACTIVE) };
            }
        }
        Cmd::PauseAll => shared.pause_all(),
        Cmd::ResumeAll => {
            for r in shared.snapshot().iter().filter(|r| resumable(r)) {
                shared.resume(r.id);
            }
        }
        Cmd::Settings | Cmd::About => {
            lock(&UI_REQUESTS).push(if cmd == Cmd::Settings {
                UiRequest::Settings
            } else {
                UiRequest::About
            });
            show_window();
        }
        Cmd::Quit => quit(shared),
    }
    if let Some(ctx) = shared.ctx.get() {
        ctx.request_repaint();
    }
}

/// Really exit: pause downloads and save state (the normal shutdown path), drop the icon.
fn quit(shared: &Arc<Shared>) -> ! {
    QUITTING.store(true, Ordering::Relaxed);
    remove();
    let h = hwnd();
    if !h.is_null() {
        // SAFETY: our main window, on its own thread.
        unsafe { ShowWindow(h, SW_HIDE) };
    }
    shared.shutdown();
    std::process::exit(0);
}

fn resumable(r: &RowView) -> bool {
    !r.running && !r.remote_changed && matches!(r.status, Status::Paused | Status::Failed(_))
}

/// Mirror the settings: create/remove the icon, cache the flags.
fn sync(shared: &Shared) {
    if QUITTING.load(Ordering::Relaxed) {
        return;
    }
    let s = shared.settings();
    MIN_TO_TRAY.store(s.minimize_to_tray, Ordering::Relaxed);
    CLOSE_TO_TRAY.store(s.close_to_tray, Ordering::Relaxed);
    let removed = TRAY.with(|t| {
        let mut t = t.borrow_mut();
        match (s.tray_icon, t.is_some()) {
            (true, false) => {
                *t = build().ok();
                false
            }
            (false, true) => {
                *t = None;
                true
            }
            _ => false,
        }
    });
    if removed {
        // Never leave the window hidden without a way back.
        let h = hwnd();
        // SAFETY: our main window.
        if !h.is_null() && unsafe { IsWindowVisible(h) } == 0 {
            show_window();
        }
    }
}

fn build() -> Result<TrayState, String> {
    let icon = tray_icon_image()?;
    let menu = Menu::new();
    let open = MenuItem::with_id("open", "Open Thrennix", true, None);
    let pause_all = MenuItem::with_id("pause_all", "Pause all", false, None);
    let resume_all = MenuItem::with_id("resume_all", "Resume all", false, None);
    let settings = MenuItem::with_id("settings", "Settings…", true, None);
    let about = MenuItem::with_id("about", "About Thrennix", true, None);
    let quit = MenuItem::with_id("quit", "Quit", true, None);
    menu.append_items(&[
        &open,
        &PredefinedMenuItem::separator(),
        &pause_all,
        &resume_all,
        &PredefinedMenuItem::separator(),
        &settings,
        &about,
        &PredefinedMenuItem::separator(),
        &quit,
    ])
    .map_err(|e| e.to_string())?;
    // Bold default item, like other tray apps (what a left click does).
    // SAFETY: a valid popup menu handle owned by `menu`.
    unsafe { SetMenuDefaultItem(menu.hpopupmenu() as _, 0, 1) };
    let icon = TrayIconBuilder::new()
        .with_id("thrennix")
        .with_menu(Box::new(menu))
        .with_menu_on_left_click(false)
        .with_tooltip("Thrennix")
        .with_icon(icon)
        .build()
        .map_err(|e| e.to_string())?;
    *lock(&LAST_TOOLTIP) = None;
    Ok(TrayState {
        icon,
        pause_all,
        resume_all,
        tooltip: "Thrennix".into(),
    })
}

/// Update tooltip and menu enablement from the current downloads.
fn refresh(rows: &[RowView]) {
    let running: Vec<&RowView> = rows.iter().filter(|r| r.running).collect();
    let speed: f64 = running.iter().map(|r| r.speed_bps).sum();
    let tip = tooltip_text(running.len(), speed);
    let can_resume = rows.iter().any(resumable);
    TRAY.with(|t| {
        if let Some(t) = t.borrow_mut().as_mut() {
            if t.tooltip != tip {
                let _ = t.icon.set_tooltip(Some(&tip));
                t.tooltip = tip;
            }
            t.pause_all.set_enabled(!running.is_empty());
            t.resume_all.set_enabled(can_resume);
        }
    });
}

fn tray_icon_image() -> Result<tray_icon::Icon, String> {
    let src = crate::app_icon();
    let (rgba, w, h) = downscale(&src.rgba, src.width, src.height, ICON_PX);
    tray_icon::Icon::from_rgba(rgba, w, h).map_err(|e| e.to_string())
}

/// Box-filter `rgba` (w×h) down to at most `max` px wide, by an integer factor.
fn downscale(rgba: &[u8], w: u32, h: u32, max: u32) -> (Vec<u8>, u32, u32) {
    let f = (w / max.max(1)).max(1);
    if f == 1 || !w.is_multiple_of(f) || !h.is_multiple_of(f) || rgba.len() != (w * h * 4) as usize
    {
        return (rgba.to_vec(), w, h);
    }
    let (ow, oh) = (w / f, h / f);
    let mut out = Vec::with_capacity((ow * oh * 4) as usize);
    for oy in 0..oh {
        for ox in 0..ow {
            // Premultiply so transparent pixels don't darken the edges.
            let mut acc = [0u64; 4];
            for y in oy * f..(oy + 1) * f {
                for x in ox * f..(ox + 1) * f {
                    let i = ((y * w + x) * 4) as usize;
                    let a = rgba[i + 3] as u64;
                    for c in 0..3 {
                        acc[c] += rgba[i + c] as u64 * a;
                    }
                    acc[3] += a;
                }
            }
            let n = (f * f) as u64;
            for c in 0..3 {
                out.push(acc[c].checked_div(acc[3]).unwrap_or(0) as u8);
            }
            out.push((acc[3] / n) as u8);
        }
    }
    (out, ow, oh)
}

/// First time the close button sends Thrennix to the tray, say so.
fn first_close_hint(shared: &Shared) {
    if HINT_SHOWN.swap(true, Ordering::Relaxed) {
        return;
    }
    let mut s = shared.settings();
    s.tray_hint_shown = true;
    shared.apply_settings(s);
    std::thread::spawn(|| {
        let _ = notify_rust::Notification::new()
            .appname("Thrennix")
            .summary("Thrennix is still running in the tray")
            .body("Downloads keep going. Click the tray icon to open Thrennix, or right-click it to quit.")
            .show();
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tooltip() {
        assert_eq!(tooltip_text(0, 5000.0), "Thrennix");
        assert_eq!(
            tooltip_text(2, 12.3 * 1024.0 * 1024.0),
            "Thrennix — 2 downloading · 12.3 MB/s"
        );
        assert!(tooltip_text(999, 1e15).chars().count() < 128);
    }

    #[test]
    fn settings_defaults_for_old_files() {
        // A settings.json written before the tray options existed.
        let old: crate::state::Settings =
            serde_json::from_str(r#"{"max_connections":4,"ask_where":true}"#).unwrap();
        assert!(old.tray_icon);
        assert!(!old.minimize_to_tray);
        assert!(old.close_to_tray);
        assert!(old.start_minimized);
        assert!(!old.tray_hint_shown);
        assert_eq!(old.max_connections, 4);
        // Round trip keeps the user's choices.
        let mut s = old.clone();
        s.tray_icon = false;
        s.minimize_to_tray = true;
        let back: crate::state::Settings =
            serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn menu_ids() {
        assert_eq!(Cmd::from_menu_id("open"), Some(Cmd::Open));
        assert_eq!(Cmd::from_menu_id("quit"), Some(Cmd::Quit));
        assert_eq!(Cmd::from_menu_id("nope"), None);
    }

    #[test]
    fn downscale_box() {
        // 2×2 → 1×1: one opaque red pixel, three transparent ones.
        let mut px = vec![0u8; 16];
        px[..4].copy_from_slice(&[255, 0, 0, 255]);
        let (out, w, h) = downscale(&px, 2, 2, 1);
        assert_eq!((w, h), (1, 1));
        assert_eq!(out, vec![255, 0, 0, 63]);
        // Already small enough: unchanged.
        let (out, w, _) = downscale(&px, 2, 2, 32);
        assert_eq!((out.len(), w), (16, 2));
    }
}
