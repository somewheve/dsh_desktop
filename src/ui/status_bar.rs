//! 状态栏（极简）：一行弱化小字 —— DSH web 状态 · profile · 日志级别。
//! 视觉原则：状态栏是"在但不见"的层，除非用户在找它。

use egui::RichText;

use crate::config::AppConfig;
use crate::ui::i18n::{tr, Lang};
use crate::ui::theme::Theme;

pub fn status_bar(ui: &mut egui::Ui, cfg: &AppConfig, web: bool, lang: Lang, error: Option<&str>) {
    ui.horizontal(|ui| {
        ui.add_space(6.0);
        // web 状态：圆点 + 文本（无分隔线堆砌）
        let dot = if web { "●" } else { "○" };
        ui.label(
            RichText::new(format!(
                "{dot} web {}",
                if web {
                    cfg.web_port.to_string()
                } else {
                    tr(lang, "未运行", "off").into()
                }
            ))
            .size(10.5)
            .color(if web {
                Theme::ok()
            } else {
                Theme::text_faint()
            }),
        );
        ui.label(RichText::new("·").size(10.5).color(Theme::text_faint()));
        let profile = cfg
            .profile
            .clone()
            .unwrap_or_else(|| tr(lang, "(未选)", "(none)").into());
        ui.label(
            RichText::new(format!("profile {profile}"))
                .size(10.5)
                .color(Theme::text_faint()),
        );

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            // 错误信息（最底部查看位）：红字 + hover 看全文（长错误截断显示）
            if let Some(err) = error {
                let shown: String = err.chars().take(120).collect();
                let ellipsis = if err.chars().count() > 120 { "…" } else { "" };
                ui.label(
                    RichText::new(format!("⚠ {shown}{ellipsis}"))
                        .size(10.5)
                        .color(Theme::err()),
                )
                .on_hover_text(err);
            }
            let level = std::env::var("RUST_LOG").unwrap_or_else(|_| "info".into());
            ui.label(
                RichText::new(format!("RUST_LOG={level}"))
                    .size(10.0)
                    .color(Theme::text_faint())
                    .weak(),
            );
            ui.add_space(6.0);
        });
    });
}
