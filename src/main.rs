#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
#![allow(linker_messages)]

// 逻辑全在 lib 里（见 src/lib.rs 的说明）；这里只负责窗口启动与 panic 报告。
use course_snatching::app::CourseApp;
use course_snatching::config::AppConfig;
use course_snatching::single_instance;
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
        // 脱敏在 write_crash_report 内部统一做，避免出现第二条未脱敏的落盘口。
        let _ = AppConfig::write_crash_report(&report);
        default_hook(info);
    }));
}

fn main() -> eframe::Result<()> {
    // 单实例守护要在建窗口、读写配置之前：第二个进程会把网络治理翻倍
    // 绕过，并与第一个进程互相覆盖配置和会话状态。
    let Some(_instance) = single_instance::acquire() else {
        single_instance::focus_existing_and_notify(APP_TITLE);
        return Ok(());
    };
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
