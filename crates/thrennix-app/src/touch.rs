//! Touch screens (Windows tablets): finger-sized tap tolerance, press-and-hold for the
//! context menu, a roomier layout, and the on-screen keyboard for text fields.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use eframe::egui::{self, style::ScrollStyle, vec2, Align2, Event, RawInput};
use serde::{Deserialize, Serialize};

/// A fingertip wobbles far more than a mouse between press and release; with egui's
/// 6 pt default most taps were thrown away as tiny drags.
const TOUCH_CLICK_DIST: f32 = 24.0;
/// Double-tap (open a finished file) is slower than a double-click.
const TOUCH_DOUBLE_TAP: f64 = 0.45;
/// A text field focused this soon after a tap was focused by that tap.
const TAP_TO_FOCUS: Duration = Duration::from_millis(600);

/// Settings → "Touch layout".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum TouchLayout {
    /// Roomy layout when the PC has a touch screen.
    #[default]
    Auto,
    On,
    Off,
}

impl TouchLayout {
    pub fn label(self) -> &'static str {
        match self {
            TouchLayout::Auto => "Automatic",
            TouchLayout::On => "Always on",
            TouchLayout::Off => "Off",
        }
    }
}

/// The roomy layout is on (read by dialogs that take typing).
static ROOMY: AtomicBool = AtomicBool::new(false);

/// Where to put a dialog you type into: centred, or with the touch layout near the top
/// so the on-screen keyboard doesn't cover its buttons.
pub fn typing_dialog_anchor() -> (Align2, [f32; 2]) {
    if ROOMY.load(Ordering::Relaxed) {
        (Align2::CENTER_TOP, [0.0, 16.0])
    } else {
        (Align2::CENTER_CENTER, [0.0, 0.0])
    }
}

/// Whether Windows reports a touch digitizer.
pub fn has_touch_screen() -> bool {
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_MAXIMUMTOUCHES};
    // SAFETY: plain query, no pointers.
    unsafe { GetSystemMetrics(SM_MAXIMUMTOUCHES) > 0 }
}

pub struct TouchState {
    touch_screen: bool,
    /// The last pointer input came from a finger or pen (not a mouse or touchpad).
    finger: bool,
    last_touch: Option<Instant>,
    roomy: Option<bool>,
    editing: bool,
    /// We opened the on-screen keyboard, so we may close it again.
    opened_keyboard: bool,
    /// egui's own (mouse) tap tolerance and double-click delay.
    mouse: (f32, f64),
}

impl TouchState {
    pub fn new(ctx: &egui::Context) -> Self {
        let touch_screen = has_touch_screen();
        let mouse = ctx.options(|o| {
            let i = &o.input_options;
            (i.max_click_dist, i.max_double_click_delay)
        });
        let me = Self {
            touch_screen,
            finger: touch_screen,
            last_touch: None,
            roomy: None,
            editing: false,
            opened_keyboard: false,
            mouse,
        };
        me.set_input_options(ctx);
        me
    }

    /// From `App::raw_input_hook`: runs before egui reads the input, so even the first
    /// tap after using the mouse gets the finger-sized tolerance.
    pub fn on_raw_input(&mut self, ctx: &egui::Context, raw: &RawInput) {
        let touched = raw.events.iter().any(|e| matches!(e, Event::Touch { .. }));
        let mouse = raw.events.iter().any(|e| {
            matches!(
                e,
                Event::PointerMoved(_) | Event::PointerButton { .. } | Event::MouseWheel { .. }
            )
        });
        if touched {
            self.last_touch = Some(Instant::now());
        }
        // egui-winit turns touches into pointer events too, so a touch wins.
        let finger = if touched {
            true
        } else if mouse {
            false
        } else {
            self.finger
        };
        if finger != self.finger {
            self.finger = finger;
            self.set_input_options(ctx);
        }
    }

    /// Call first thing every frame. Returns whether the roomy (finger-sized) layout is on.
    pub fn frame(&mut self, ctx: &egui::Context, layout: TouchLayout) -> bool {
        let roomy = match layout {
            TouchLayout::Auto => self.touch_screen,
            TouchLayout::On => true,
            TouchLayout::Off => false,
        };
        if self.roomy != Some(roomy) {
            self.roomy = Some(roomy);
            ROOMY.store(roomy, Ordering::Relaxed);
            apply_style(ctx, roomy);
        }

        // Tapping into a text field: bring up the on-screen keyboard, like native apps.
        let editing = ctx.wants_keyboard_input();
        let tapped = self.last_touch.is_some_and(|t| t.elapsed() < TAP_TO_FOCUS);
        if editing && !self.editing && tapped {
            self.opened_keyboard = true;
            keyboard::show();
        } else if !editing && self.editing && self.opened_keyboard {
            self.opened_keyboard = false;
            keyboard::hide();
        }
        self.editing = editing;
        roomy
    }

    fn set_input_options(&self, ctx: &egui::Context) {
        let (dist, double) = if self.finger {
            (TOUCH_CLICK_DIST, TOUCH_DOUBLE_TAP)
        } else {
            self.mouse
        };
        ctx.options_mut(|o| {
            o.input_options.max_click_dist = dist;
            o.input_options.max_double_click_delay = double;
        });
    }
}

fn apply_style(ctx: &egui::Context, roomy: bool) {
    ctx.all_styles_mut(|s| {
        let sp = &mut s.spacing;
        if roomy {
            sp.item_spacing = vec2(10.0, 8.0);
            sp.button_padding = vec2(14.0, 8.0);
            sp.interact_size.y = 34.0;
            sp.icon_width = 20.0;
            sp.icon_width_inner = 12.0;
            sp.icon_spacing = 8.0;
            // Floating scroll bars only appear on hover, which a finger never does.
            sp.scroll = ScrollStyle {
                bar_width: 12.0,
                ..ScrollStyle::solid()
            };
        } else {
            let d = egui::style::Spacing::default();
            sp.item_spacing = vec2(8.0, 6.0);
            sp.button_padding = vec2(10.0, 4.0);
            sp.interact_size.y = 24.0;
            sp.icon_width = d.icon_width;
            sp.icon_width_inner = d.icon_width_inner;
            sp.icon_spacing = d.icon_spacing;
            sp.scroll = d.scroll;
        }
    });
}

/// The Windows touch keyboard. There is no documented "show" call for desktop apps;
/// `ITipInvocation::Toggle` is what Explorer and most desktop apps use, and
/// `IFrameworkInputPane::Location` tells whether it is already up (so we never hide it
/// by toggling twice).
mod keyboard {
    use std::ffi::c_void;
    use std::ptr;
    use std::time::Duration;

    use windows_sys::core::{GUID, HRESULT};
    use windows_sys::Win32::Foundation::{HWND, RECT};
    use windows_sys::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER,
        CLSCTX_LOCAL_SERVER, COINIT_APARTMENTTHREADED,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::GetDesktopWindow;

    const CLSID_UIHOST_NO_LAUNCH: GUID = GUID::from_u128(0x4ce576fa_83dc_4f88_951c_9d0782b4e376);
    const IID_ITIP_INVOCATION: GUID = GUID::from_u128(0x37c994e7_432b_4834_a2f7_dce1f13b834b);
    const CLSID_FRAMEWORK_INPUT_PANE: GUID =
        GUID::from_u128(0xd5120aa3_46ba_44c5_822d_ca8092c1fc72);
    const IID_IFRAMEWORK_INPUT_PANE: GUID = GUID::from_u128(0x5752238b_24f0_495a_82f1_2fd593056796);

    #[repr(C)]
    struct TipInvocationVtbl {
        _query_interface: usize,
        _add_ref: usize,
        release: unsafe extern "system" fn(*mut c_void) -> u32,
        toggle: unsafe extern "system" fn(*mut c_void, HWND) -> HRESULT,
    }

    #[repr(C)]
    struct InputPaneVtbl {
        _query_interface: usize,
        _add_ref: usize,
        release: unsafe extern "system" fn(*mut c_void) -> u32,
        _advise: usize,
        _advise_with_hwnd: usize,
        _unadvise: usize,
        location: unsafe extern "system" fn(*mut c_void, *mut RECT) -> HRESULT,
    }

    /// A COM object pointer: the object starts with a pointer to its vtable.
    unsafe fn create<V>(clsid: &GUID, iid: &GUID, ctx: u32) -> Option<*mut *const V> {
        let mut obj: *mut c_void = ptr::null_mut();
        let hr = CoCreateInstance(clsid, ptr::null_mut(), ctx, iid, &mut obj);
        (hr >= 0 && !obj.is_null()).then_some(obj as *mut *const V)
    }

    fn is_visible() -> Option<bool> {
        // SAFETY: COM is initialised on this thread; the vtable layout matches the
        // documented IFrameworkInputPane, and the object is released after use.
        unsafe {
            let pane = create::<InputPaneVtbl>(
                &CLSID_FRAMEWORK_INPUT_PANE,
                &IID_IFRAMEWORK_INPUT_PANE,
                CLSCTX_INPROC_SERVER,
            )?;
            let mut r: RECT = std::mem::zeroed();
            let hr = ((**pane).location)(pane.cast(), &mut r);
            ((**pane).release)(pane.cast());
            (hr >= 0).then_some(r.right > r.left && r.bottom > r.top)
        }
    }

    fn toggle() -> bool {
        // SAFETY: as above, for ITipInvocation (IUnknown + Toggle).
        unsafe {
            let Some(tip) = create::<TipInvocationVtbl>(
                &CLSID_UIHOST_NO_LAUNCH,
                &IID_ITIP_INVOCATION,
                CLSCTX_INPROC_SERVER | CLSCTX_LOCAL_SERVER,
            ) else {
                return false;
            };
            let hr = ((**tip).toggle)(tip.cast(), GetDesktopWindow());
            ((**tip).release)(tip.cast());
            hr >= 0
        }
    }

    fn with_com(f: impl FnOnce() + Send + 'static) {
        std::thread::spawn(move || {
            // SAFETY: balanced with CoUninitialize below on this same thread.
            let hr = unsafe { CoInitializeEx(ptr::null(), COINIT_APARTMENTTHREADED as u32) };
            f();
            if hr >= 0 {
                unsafe { CoUninitialize() };
            }
        });
    }

    pub fn show() {
        with_com(|| {
            // Windows sometimes raises it by itself; give it a moment so we don't toggle
            // it straight back down.
            std::thread::sleep(Duration::from_millis(250));
            if is_visible() == Some(true) {
                return;
            }
            if !toggle() {
                // The keyboard host isn't running yet: starting it shows the keyboard.
                // (It needs ShellExecute; CreateProcess is refused for this exe.)
                let dir = std::env::var_os("CommonProgramFiles")
                    .map(std::path::PathBuf::from)
                    .unwrap_or_else(|| r"C:\Program Files\Common Files".into());
                let _ = opener::open(dir.join(r"microsoft shared\ink\TabTip.exe"));
            }
        });
    }

    pub fn hide() {
        with_com(|| {
            if is_visible() == Some(true) {
                toggle();
            }
        });
    }
}
