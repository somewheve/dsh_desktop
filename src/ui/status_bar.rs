//! 状态栏：DSH URL / profile / 日志级别。

use egui::RichText;

use crate::config::AppConfig;
use crate::ui::i18n::{tr, Lang};

pub fn status_bar(ui: &mut egui::Ui, cfg: &AppConfig, web: bool, lang: Lang) {
    ui.horizontal(|ui| {
        let web_text = if web {
            format!("DSH web: {} ✓", cfg.web_url())
        } else {
            format!(
                "DSH web: {} ({})",
                cfg.web_url(),
                tr(lang, "未运行", "not running")
            )
        };
        ui.label(RichText::new(web_text).size(11.0).color(if web {
            egui::Color32::from_rgb(80, 200, 120)
        } else {
            egui::Color32::GRAY
        }));

        ui.separator();

        let profile = cfg
            .profile
            .clone()
            .unwrap_or_else(|| tr(lang, "(未选)", "(none)").into());
        ui.label(RichText::new(format!("profile: {profile}")).size(11.0));

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let level = std::env::var("RUST_LOG").unwrap_or_else(|_| "info".into());
            ui.label(RichText::new(format!("RUST_LOG={level}")).size(10.0).weak());
        });
    });
}
