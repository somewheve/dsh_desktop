//! 现代化 UI 主题与配色。

use eframe::egui;
use egui::{Color32, FontId, RichText, Vec2};

/// 深色现代主题配色（应用级）。
pub struct Theme;

impl Theme {
    // 背景层次
    pub const BG: Color32 = Color32::from_rgb(15, 17, 22); // 主背景
    pub const BG_ELEVATED: Color32 = Color32::from_rgb(22, 25, 32); // 面板
    pub const BG_HOVER: Color32 = Color32::from_rgb(28, 32, 40);
    pub const BORDER: Color32 = Color32::from_rgb(38, 42, 52);

    // 强调色（现代蓝紫渐变感）
    pub const ACCENT: Color32 = Color32::from_rgb(99, 102, 241); // indigo
    pub const ACCENT_LIGHT: Color32 = Color32::from_rgb(129, 140, 248);
    pub const CYAN: Color32 = Color32::from_rgb(34, 211, 238);

    // 文本
    pub const TEXT: Color32 = Color32::from_rgb(226, 232, 240);
    pub const TEXT_DIM: Color32 = Color32::from_rgb(148, 163, 184);
    pub const TEXT_FAINT: Color32 = Color32::from_rgb(100, 116, 139);

    // 语义色
    pub const OK: Color32 = Color32::from_rgb(52, 211, 153);
    pub const WARN: Color32 = Color32::from_rgb(251, 191, 36);
    pub const ERR: Color32 = Color32::from_rgb(248, 113, 113);

    // 角色色（对话）—— 必须一眼区分：
    // 用户消息：亮蓝色气泡（类似微信/主流聊天软件的用户气泡）
    // AI 消息：明显偏亮的灰色气泡（与背景 BG(15,17,22) 拉开差距）
    pub const USER_BUBBLE: Color32 = Color32::from_rgb(37, 99, 235);
    pub const ASSISTANT_BUBBLE: Color32 = Color32::from_rgb(52, 58, 72);

    /// 应用主题到 egui 上下文。
    ///
    /// 注意：egui 0.36 的 Options 持有两个主题槽（dark_style/light_style），
    /// ThemePreference 默认 System（跟随系统）。`set_global_style` 只写
    /// “当前生效槽”，而 App::new 时系统主题尚未到达（回退 Dark），
    /// 首帧后若 Windows 为浅色模式，生效槽切到未定制的 light_style，
    /// 整个深色主题会被忽略（白面板）。因此必须把样式写入两个槽并强制 dark。
    pub fn apply(ctx: &egui::Context) {
        let mut style = (*ctx.global_style()).clone();
        style.spacing.item_spacing = Vec2::new(8.0, 8.0);
        style.spacing.button_padding = Vec2::new(14.0, 7.0);
        style.visuals = egui::Visuals::dark();
        style.visuals.panel_fill = Self::BG;
        style.visuals.window_fill = Self::BG_ELEVATED;
        style.visuals.extreme_bg_color = Self::BG;
        style.visuals.faint_bg_color = Self::BG_HOVER;
        style.visuals.selection.bg_fill = Self::ACCENT;
        style.visuals.hyperlink_color = Self::CYAN;
        style.visuals.widgets.noninteractive.bg_fill = Self::BG_ELEVATED;
        style.visuals.widgets.noninteractive.fg_stroke.color = Self::TEXT;
        style.visuals.widgets.inactive.bg_fill = Self::BG_ELEVATED;
        style.visuals.widgets.inactive.fg_stroke.color = Self::TEXT;
        style.visuals.widgets.hovered.bg_fill = Self::BG_HOVER;
        style.visuals.widgets.hovered.fg_stroke.color = Self::TEXT;
        style.visuals.widgets.active.bg_fill = Self::ACCENT;
        style.visuals.widgets.active.fg_stroke.color = Color32::WHITE;
        // 双槽写入 + 强制 dark（否则系统浅色模式下整个主题被忽略）
        ctx.set_style_of(egui::Theme::Dark, style.clone());
        ctx.set_style_of(egui::Theme::Light, style);
        ctx.set_theme(egui::Theme::Dark);
    }

    /// 面板背景块。
    pub fn panel_frame() -> egui::Frame {
        egui::Frame::default()
            .fill(Self::BG_ELEVATED)
            .stroke(egui::Stroke::new(1.0, Self::BORDER))
            .corner_radius(egui::CornerRadius::same(10))
            .inner_margin(egui::Margin::same(14))
    }

    /// 导航项（选中高亮）。
    pub fn nav_button(ui: &mut egui::Ui, label: &str, icon: &str, selected: bool) -> bool {
        let text = format!("{icon}  {label}");
        let color = if selected {
            Self::ACCENT_LIGHT
        } else {
            Self::TEXT_DIM
        };
        let response = ui.add(
            egui::Button::new(RichText::new(text).size(14.0).color(color))
                .fill(if selected {
                    Self::BG_HOVER
                } else {
                    egui::Color32::TRANSPARENT
                })
                .corner_radius(8.0)
                .min_size(Vec2::new(150.0, 36.0)),
        );
        response.clicked()
    }

    /// 小节标题。
    pub fn section_title(text: &str) -> RichText {
        RichText::new(text).size(16.0).strong().color(Self::TEXT)
    }

    /// 弱化文本。
    pub fn dim(text: &str) -> RichText {
        RichText::new(text).size(11.0).color(Self::TEXT_DIM)
    }

    /// 输入框样式。
    pub fn text_edit(ui: &mut egui::Ui, value: &mut String, hint: &str, width: f32) {
        ui.add(
            egui::TextEdit::multiline(value)
                .hint_text(RichText::new(hint).color(Self::TEXT_FAINT))
                .desired_width(width)
                .desired_rows(2)
                .text_color(Self::TEXT)
                .background_color(Self::BG),
        );
    }

    pub fn font(size: f32) -> FontId {
        FontId::proportional(size)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 回归：egui 0.36 双主题槽机制下，自定义深色主题必须同时写入
    /// dark/light 两个槽并强制 dark。
    ///
    /// 背景：egui 0.36 的 Options 持有 dark_style/light_style 两个槽，
    /// ThemePreference 默认 System（跟随系统）。旧实现用 set_global_style
    /// 只写“当前生效槽”——App::new 时系统主题尚未到达（回退 Dark），
    /// 写进 dark_style；首帧后系统主题（Windows 浅色模式）到达，
    /// 生效槽切到未定制的 light_style → 整个应用渲染为默认浅色样式
    /// （白面板 248,248,248、浅色选择色 144,209,255），深色主题被忽略。
    #[test]
    fn apply_writes_both_theme_slots() {
        let ctx = egui::Context::default();
        Theme::apply(&ctx);
        for theme in [egui::Theme::Dark, egui::Theme::Light] {
            let style = ctx.style_of(theme);
            assert_eq!(
                style.visuals.panel_fill,
                Theme::BG,
                "{theme:?} 槽面板色必须是深色主题 BG"
            );
            assert_eq!(
                style.visuals.window_fill,
                Theme::BG_ELEVATED,
                "{theme:?} 槽窗口色必须是深色主题"
            );
            assert_eq!(
                style.visuals.selection.bg_fill,
                Theme::ACCENT,
                "{theme:?} 槽选择色必须是强调色"
            );
            assert!(style.visuals.dark_mode, "{theme:?} 槽必须为 dark_mode");
        }
        // 强制 dark：当前生效样式立即为定制深色（不依赖系统主题）
        assert_eq!(ctx.global_style().visuals.panel_fill, Theme::BG);
    }
}
