#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
//! thrennix.exe: GUI, native messaging host, and host (un)registration.

mod about;
mod args;
mod backend;
mod brand;
mod details;
mod fmt;
mod host;
mod ipc;
mod nm;
mod protocol;
mod register;
mod secret;
mod state;
mod theme;
mod ui;
mod winsec;

use std::sync::Arc;

use args::Mode;

fn main() {
    let args: Vec<String> = std::env::args_os()
        .skip(1)
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let code = match args::detect(&args) {
        Mode::NativeHost => host::run(),
        Mode::Register => report(register::register(), "registered"),
        Mode::Unregister => report(register::unregister(), "unregistered"),
        Mode::Gui { demo } => run_gui(demo),
    };
    std::process::exit(code);
}

fn report(r: std::io::Result<()>, what: &str) -> i32 {
    match r {
        Ok(()) => {
            println!("Thrennix native messaging host {what}.");
            0
        }
        Err(e) => {
            eprintln!("error: {e}");
            1
        }
    }
}

fn run_gui(demo: bool) -> i32 {
    // Single instance: hand over to the running window.
    if ipc::signal_existing_instance() {
        return 0;
    }
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("thrennix-rt")
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("error: cannot start runtime: {e}");
            return 1;
        }
    };
    let server = {
        let _g = rt.enter();
        match ipc::create_first() {
            Ok(s) => s,
            Err(_) => {
                // Lost a race with another instance starting.
                ipc::signal_existing_instance();
                return 0;
            }
        }
    };
    if !demo {
        // The exe may have moved since the last run.
        let _ = register::register();
    }

    let mut settings = state::Settings::load();
    // Demo-only extras for screenshots: --light / --dark / --details / --about (never persisted).
    let extra: Vec<String> = std::env::args().skip(2).collect();
    if demo {
        if extra.iter().any(|a| a == "--light") {
            settings.theme = theme::ThemeChoice::Light;
        } else if extra.iter().any(|a| a == "--dark") {
            settings.theme = theme::ThemeChoice::Dark;
        }
    }
    let backend: Arc<dyn backend::Backend> = if demo {
        Arc::new(backend::DemoBackend {
            rt: rt.handle().clone(),
        })
    } else {
        Arc::new(backend::RealBackend::new(
            rt.handle().clone(),
            settings.engine_config(),
        ))
    };
    let shared = Arc::new(state::Shared::new(
        rt.handle().clone(),
        backend,
        settings,
        !demo,
    ));
    rt.spawn(ipc::serve(server, shared.clone()));
    if demo {
        shared.seed_demo(extra.iter().any(|a| a == "--details"));
    }

    let title = if demo { "Thrennix (demo)" } else { "Thrennix" };
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title(title)
            .with_app_id("thrennix")
            .with_inner_size([1180.0, 640.0])
            .with_min_inner_size([760.0, 380.0])
            .with_icon(app_icon()),
        ..Default::default()
    };
    let open_about = demo && extra.iter().any(|a| a == "--about");
    let app_shared = shared.clone();
    let result = eframe::run_native(
        title,
        options,
        Box::new(move |cc| {
            Ok(Box::new(
                ui::ThrennixApp::new(cc, app_shared).with_about(open_about),
            ))
        }),
    );
    shared.shutdown();
    match result {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("error: {e}");
            1
        }
    }
}

/// Window icon (same artwork as the browser extension).
pub(crate) fn app_icon() -> Arc<eframe::egui::IconData> {
    static ICON: std::sync::OnceLock<Arc<eframe::egui::IconData>> = std::sync::OnceLock::new();
    ICON.get_or_init(|| {
        let png = include_bytes!("../../../extension/icons/icon-128.png");
        Arc::new(eframe::icon_data::from_png_bytes(png).unwrap_or_default())
    })
    .clone()
}
