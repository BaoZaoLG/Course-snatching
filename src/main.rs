#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
#![allow(linker_messages)]

mod app;
mod config;
mod eams;
mod notify;
mod worker;

use app::CourseApp;
use config::AppConfig;
use eframe::egui;
use std::sync::Arc;

const APP_TITLE: &str = concat!("Course-snatching v", env!("CARGO_PKG_VERSION"));

fn load_icon() -> egui::IconData {
    eframe::icon_data::from_png_bytes(include_bytes!("../assets/icon.png")).expect("icon")
}

fn install_panic_report() {
    // Retention is best-effort; diagnostics must never affect application startup.
    let _ = AppConfig::retain_crash_reports();
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let report = format!(
            "Course-snatching panic report\n{}\n\n{:?}\n",
            info,
            std::backtrace::Backtrace::force_capture()
        );
        // Never let a failed diagnostics write mask or re-panic over the original failure.
        let _ = AppConfig::write_crash_report(&report);
        default_hook(info);
    }));
}

fn main() -> eframe::Result<()> {
    install_panic_report();
    let icon = load_icon();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1240.0, 780.0])
            .with_min_inner_size([1020.0, 680.0])
            .with_title(APP_TITLE)
            .with_transparent(false)
            .with_icon(Arc::new(icon)),
        ..Default::default()
    };
    eframe::run_native(
        APP_TITLE,
        options,
        Box::new(|cc| Ok(Box::new(CourseApp::new(cc)))),
    )
}
