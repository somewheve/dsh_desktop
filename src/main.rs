//! dsh-desktop 入口。

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use dsh_desktop::app::DshDesktopApp;
use dsh_desktop::util;
use eframe::egui;

fn main() -> eframe::Result {
    util::init_logging();
    // panic hook：崩溃时写入日志文件（否则 panic 只进 stderr，难以排查）
    std::panic::set_hook(Box::new(|info| {
        let base = std::env::var("LOCALAPPDATA").unwrap_or_else(|_| ".".into());
        let dir = format!("{base}/dsh-desktop/logs");
        let _ = std::fs::create_dir_all(&dir);
        let msg = format!(
            "PANIC: {info}
{}",
            std::backtrace::Backtrace::force_capture()
        );
        let _ = std::fs::write(format!("{dir}/panic.log"), &msg);
        log::error!("{msg}");
    }));
    log::info!("dsh-desktop starting");

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1100.0, 700.0])
            .with_min_inner_size([640.0, 400.0])
            .with_title("dsh-desktop — DeepSeek Harness 桌面终端"),
        ..Default::default()
    };

    eframe::run_native(
        "dsh-desktop",
        options,
        Box::new(|cc| Ok(Box::new(DshDesktopApp::new(cc)))),
    )
}
