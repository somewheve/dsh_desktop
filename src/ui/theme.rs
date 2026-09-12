//! 主题系统：语义调色盘（Palette）+ 内置主题 + 外部主题文件 + 插件主题。
//!
//! - 所有 UI 颜色的唯一来源是 [`Palette`]（16 个语义槽位）；
//! - `Theme` 是零大小门面：`Theme::bg()` 等关联函数读取当前激活的调色盘
//!   （全局 RwLock，UI 单线程读写 + 主题切换时写入）；
//! - 主题来源：内置（dark / paper-light）→ `$DSH_HOME/themes/*.json`
//!   → 插件 `theme` 字段指向的 JSON（插件可发布自己的主题）；
//! - 主题文件格式：`{"name": "my-theme", "bg": "#0f1116", ...}`，
//!   未指定的槽位回退内置 dark（部分主题只需覆盖几个颜色）。

use eframe::egui;
use egui::{Color32, FontId, RichText, Vec2};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// `#RRGGBB` 颜色（serde 友好；也接受 `RRGGBB` / `#RRGGBBAA`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HexColor(pub Color32);

impl HexColor {
    fn to_hex(self) -> String {
        let [r, g, b, _] = self.0.to_array();
        format!("#{r:02X}{g:02X}{b:02X}")
    }
    fn parse(s: &str) -> Option<Self> {
        let s = s.trim().trim_start_matches('#');
        match s.len() {
            6 => {
                let r = u8::from_str_radix(&s[0..2], 16).ok()?;
                let g = u8::from_str_radix(&s[2..4], 16).ok()?;
                let b = u8::from_str_radix(&s[4..6], 16).ok()?;
                Some(Self(Color32::from_rgb(r, g, b)))
            }
            8 => {
                let r = u8::from_str_radix(&s[0..2], 16).ok()?;
                let g = u8::from_str_radix(&s[2..4], 16).ok()?;
                let b = u8::from_str_radix(&s[4..6], 16).ok()?;
                let a = u8::from_str_radix(&s[6..8], 16).ok()?;
                Some(Self(Color32::from_rgba_unmultiplied(r, g, b, a)))
            }
            _ => None,
        }
    }
}

impl Serialize for HexColor {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_hex())
    }
}

impl<'de> Deserialize<'de> for HexColor {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        HexColor::parse(&s)
            .ok_or_else(|| serde::de::Error::custom(format!("非法颜色（期望 #RRGGBB）: {s}")))
    }
}

/// 语义调色盘：UI 的全部颜色槽位。
/// 缺省字段在反序列化时回退内置 dark（主题文件只需覆盖想改的颜色）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Palette {
    pub name: String,
    // 背景层次
    pub bg: HexColor,
    pub bg_elevated: HexColor,
    pub bg_hover: HexColor,
    pub border: HexColor,
    // 强调
    pub accent: HexColor,
    pub accent_light: HexColor,
    pub cyan: HexColor,
    // 文本
    pub text: HexColor,
    pub text_dim: HexColor,
    pub text_faint: HexColor,
    // 语义
    pub ok: HexColor,
    pub warn: HexColor,
    pub err: HexColor,
    // 角色（对话气泡）
    pub user_bubble: HexColor,
    pub assistant_bubble: HexColor,
}

impl Default for Palette {
    fn default() -> Self {
        Self::dark()
    }
}

impl Palette {
    /// 内置主题：Indigo Dark（既有默认，视觉不变）。
    pub fn dark() -> Self {
        Self {
            name: "dark".into(),
            bg: HexColor(Color32::from_rgb(15, 17, 22)),
            bg_elevated: HexColor(Color32::from_rgb(22, 25, 32)),
            bg_hover: HexColor(Color32::from_rgb(28, 32, 40)),
            border: HexColor(Color32::from_rgb(38, 42, 52)),
            accent: HexColor(Color32::from_rgb(99, 102, 241)),
            accent_light: HexColor(Color32::from_rgb(129, 140, 248)),
            cyan: HexColor(Color32::from_rgb(34, 211, 238)),
            text: HexColor(Color32::from_rgb(226, 232, 240)),
            text_dim: HexColor(Color32::from_rgb(148, 163, 184)),
            text_faint: HexColor(Color32::from_rgb(100, 116, 139)),
            ok: HexColor(Color32::from_rgb(52, 211, 153)),
            warn: HexColor(Color32::from_rgb(251, 191, 36)),
            err: HexColor(Color32::from_rgb(248, 113, 113)),
            user_bubble: HexColor(Color32::from_rgb(37, 99, 235)),
            assistant_bubble: HexColor(Color32::from_rgb(52, 58, 72)),
        }
    }

    /// 内置主题：Graphite（更中性克制的深灰，简约风的第二选择）。
    pub fn graphite() -> Self {
        Self {
            name: "graphite".into(),
            bg: HexColor(Color32::from_rgb(18, 18, 20)),
            bg_elevated: HexColor(Color32::from_rgb(26, 26, 29)),
            bg_hover: HexColor(Color32::from_rgb(34, 34, 38)),
            border: HexColor(Color32::from_rgb(44, 44, 49)),
            accent: HexColor(Color32::from_rgb(212, 212, 216)),
            accent_light: HexColor(Color32::from_rgb(235, 235, 240)),
            cyan: HexColor(Color32::from_rgb(120, 190, 210)),
            text: HexColor(Color32::from_rgb(228, 228, 232)),
            text_dim: HexColor(Color32::from_rgb(150, 150, 158)),
            text_faint: HexColor(Color32::from_rgb(104, 104, 112)),
            ok: HexColor(Color32::from_rgb(96, 200, 150)),
            warn: HexColor(Color32::from_rgb(235, 180, 80)),
            err: HexColor(Color32::from_rgb(235, 110, 110)),
            user_bubble: HexColor(Color32::from_rgb(58, 58, 64)),
            assistant_bubble: HexColor(Color32::from_rgb(38, 38, 43)),
        }
    }

    /// 内置主题：Paper Light（浅色，验证主题系统不止深色）。
    pub fn paper_light() -> Self {
        Self {
            name: "paper-light".into(),
            bg: HexColor(Color32::from_rgb(248, 247, 244)),
            bg_elevated: HexColor(Color32::from_rgb(255, 255, 255)),
            bg_hover: HexColor(Color32::from_rgb(238, 236, 231)),
            border: HexColor(Color32::from_rgb(224, 221, 214)),
            accent: HexColor(Color32::from_rgb(79, 70, 229)),
            accent_light: HexColor(Color32::from_rgb(99, 91, 255)),
            cyan: HexColor(Color32::from_rgb(13, 148, 176)),
            text: HexColor(Color32::from_rgb(38, 38, 46)),
            text_dim: HexColor(Color32::from_rgb(110, 110, 122)),
            text_faint: HexColor(Color32::from_rgb(150, 150, 160)),
            ok: HexColor(Color32::from_rgb(21, 148, 106)),
            warn: HexColor(Color32::from_rgb(202, 132, 16)),
            err: HexColor(Color32::from_rgb(214, 64, 69)),
            user_bubble: HexColor(Color32::from_rgb(79, 70, 229)),
            assistant_bubble: HexColor(Color32::from_rgb(240, 238, 233)),
        }
    }

    /// 内置主题：Cursor Dark（复刻 Cursor 编辑器默认深色：VS Code 系
    /// 暖灰黑背景 + 经典蓝强调 + 语义色沿用 VS Code dark+ 色板）。
    pub fn cursor_dark() -> Self {
        Self {
            name: "cursor-dark".into(),
            bg: HexColor(Color32::from_rgb(30, 30, 30)), // 编辑器背景 #1e1e1e
            bg_elevated: HexColor(Color32::from_rgb(37, 37, 38)), // 侧栏/面板 #252526
            bg_hover: HexColor(Color32::from_rgb(42, 45, 46)), // 列表 hover #2a2d2e
            border: HexColor(Color32::from_rgb(60, 60, 60)),
            accent: HexColor(Color32::from_rgb(0, 122, 204)), // VS Code 蓝 #007acc
            accent_light: HexColor(Color32::from_rgb(79, 193, 255)), // #4fc1ff
            cyan: HexColor(Color32::from_rgb(41, 184, 219)),
            text: HexColor(Color32::from_rgb(204, 204, 204)), // #cccccc
            text_dim: HexColor(Color32::from_rgb(157, 157, 157)),
            text_faint: HexColor(Color32::from_rgb(110, 110, 110)),
            ok: HexColor(Color32::from_rgb(137, 209, 133)), // git added #89d185
            warn: HexColor(Color32::from_rgb(204, 167, 0)), // #cca700
            err: HexColor(Color32::from_rgb(241, 76, 76)),  // #f14c4c
            user_bubble: HexColor(Color32::from_rgb(14, 99, 156)), // 按钮蓝 #0e639c
            assistant_bubble: HexColor(Color32::from_rgb(45, 45, 45)),
        }
    }

    /// 从主题 JSON 文件加载（未指定槽位回退 dark）。
    pub fn load_json_file(path: &Path) -> Result<Palette, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("读取主题失败（{}）: {e}", path.display()))?;
        let merged: Palette = serde_json::from_str(&text)
            .map_err(|e| format!("主题 JSON 解析失败（{}）: {e}", path.display()))?;
        // 回退填充：以 dark 为底、文件字段覆盖（serde(default) 已按字段回退）
        if merged.name.trim().is_empty() {
            return Err(format!("主题缺少 name（{}）", path.display()));
        }
        Ok(merged)
    }

    pub fn all_builtin() -> Vec<Palette> {
        vec![
            Self::dark(),
            Self::cursor_dark(),
            Self::graphite(),
            Self::paper_light(),
        ]
    }
}

// ===== 全局激活调色盘 =====

static ACTIVE: std::sync::LazyLock<parking_lot::RwLock<Palette>> =
    std::sync::LazyLock::new(|| parking_lot::RwLock::new(Palette::dark()));

/// 主题管理器：内置 + 目录发现（$DSH_HOME/themes）+ 插件提供，按名去重。
#[derive(Debug, Clone)]
pub struct ThemeManager {
    pub palettes: Vec<Palette>,
}

impl Default for ThemeManager {
    fn default() -> Self {
        Self {
            palettes: Palette::all_builtin(),
        }
    }
}

impl ThemeManager {
    /// 收集可用主题：内置 + `dirs` 下所有 `*.json`（+ 插件主题路径）。
    /// 坏文件跳过（不阻塞启动），同名后到者覆盖前者（插件可覆盖内置）。
    pub fn discover(extra_theme_files: &[std::path::PathBuf]) -> Self {
        let mut palettes = Palette::all_builtin();
        for f in extra_theme_files.iter() {
            match Palette::load_json_file(f) {
                Ok(p) => {
                    if let Some(slot) = palettes.iter_mut().find(|e| e.name == p.name) {
                        *slot = p;
                    } else {
                        palettes.push(p);
                    }
                }
                Err(e) => log::warn!("skip theme file: {e}"),
            }
        }
        Self { palettes }
    }

    pub fn names(&self) -> Vec<String> {
        self.palettes.iter().map(|p| p.name.clone()).collect()
    }

    pub fn get(&self, name: &str) -> Option<&Palette> {
        self.palettes.iter().find(|p| p.name == name)
    }
}

/// UI 主题门面：颜色读取当前激活调色盘。
pub struct Theme;

/// mini 按钮的语义色：中性（默认）/ 强调（主操作）/ 危险（删除等）。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ChipTint {
    Neutral,
    Accent,
    Danger,
}

impl Theme {
    // ===== 颜色槽位（读全局激活调色盘） =====
    pub fn bg() -> Color32 {
        ACTIVE.read().bg.0
    }
    pub fn bg_elevated() -> Color32 {
        ACTIVE.read().bg_elevated.0
    }
    pub fn bg_hover() -> Color32 {
        ACTIVE.read().bg_hover.0
    }
    pub fn border() -> Color32 {
        ACTIVE.read().border.0
    }
    pub fn accent() -> Color32 {
        ACTIVE.read().accent.0
    }
    pub fn accent_light() -> Color32 {
        ACTIVE.read().accent_light.0
    }
    pub fn cyan() -> Color32 {
        ACTIVE.read().cyan.0
    }
    pub fn text() -> Color32 {
        ACTIVE.read().text.0
    }
    pub fn text_dim() -> Color32 {
        ACTIVE.read().text_dim.0
    }
    pub fn text_faint() -> Color32 {
        ACTIVE.read().text_faint.0
    }
    pub fn ok() -> Color32 {
        ACTIVE.read().ok.0
    }
    pub fn warn() -> Color32 {
        ACTIVE.read().warn.0
    }
    pub fn err() -> Color32 {
        ACTIVE.read().err.0
    }
    pub fn user_bubble() -> Color32 {
        ACTIVE.read().user_bubble.0
    }
    pub fn assistant_bubble() -> Color32 {
        ACTIVE.read().assistant_bubble.0
    }

    /// 当前激活调色盘的快照。
    pub fn current() -> Palette {
        ACTIVE.read().clone()
    }

    /// 切换激活主题（全局生效；UI 下一帧即用新色）。
    pub fn set_palette(p: &Palette) {
        *ACTIVE.write() = p.clone();
    }

    /// 浅色主题判定（paper-light 等浅底主题下 egui 视觉需切 light）。
    pub fn is_light() -> bool {
        let c = Self::bg().to_array();
        // 相对亮度近似（Rec.601 luma > 0.5 视为浅色）
        let luma = (c[0] as f32 * 0.299 + c[1] as f32 * 0.587 + c[2] as f32 * 0.114) / 255.0;
        luma > 0.5
    }

    /// 应用主题到 egui 上下文。
    ///
    /// 注意：egui 0.36 的 Options 持有两个主题槽（dark_style/light_style），
    /// ThemePreference 默认 System（跟随系统）。`set_global_style` 只写
    /// “当前生效槽”，而 App::new 时系统主题尚未到达（回退 Dark），
    /// 首帧后若 Windows 为浅色模式，生效槽切到未定制的 light_style，
    /// 整个自定义主题会被忽略。因此必须把样式写入两个槽。
    pub fn apply(ctx: &egui::Context) {
        let light = Self::is_light();
        let mut style = (*ctx.global_style()).clone();
        style.spacing.item_spacing = Vec2::new(8.0, 8.0);
        style.spacing.button_padding = Vec2::new(14.0, 7.0);
        style.visuals = if light {
            let mut v = egui::Visuals::light();
            // 浅色主题仍用我们自己的 panel 色，不用 egui 默认白
            v.panel_fill = Self::bg();
            v
        } else {
            egui::Visuals::dark()
        };
        style.visuals.panel_fill = Self::bg();
        style.visuals.window_fill = Self::bg_elevated();
        style.visuals.extreme_bg_color = Self::bg();
        style.visuals.faint_bg_color = Self::bg_hover();
        style.visuals.selection.bg_fill = Self::accent();
        style.visuals.hyperlink_color = Self::cyan();
        style.visuals.widgets.noninteractive.bg_fill = Self::bg_elevated();
        style.visuals.widgets.noninteractive.fg_stroke.color = Self::text();
        style.visuals.widgets.inactive.bg_fill = Self::bg_elevated();
        style.visuals.widgets.inactive.fg_stroke.color = Self::text();
        style.visuals.widgets.hovered.bg_fill = Self::bg_hover();
        style.visuals.widgets.hovered.fg_stroke.color = Self::text();
        style.visuals.widgets.active.bg_fill = Self::accent();
        // accent 底上白字在深浅主题下均可读（原 if light 分支两臂同值）
        style.visuals.widgets.active.fg_stroke.color = Color32::WHITE;
        // 双槽写入（否则系统浅色模式下整个主题被忽略）
        ctx.set_style_of(egui::Theme::Dark, style.clone());
        ctx.set_style_of(egui::Theme::Light, style);
        ctx.set_theme(if light {
            egui::Theme::Light
        } else {
            egui::Theme::Dark
        });
    }

    /// 面板背景块。
    pub fn panel_frame() -> egui::Frame {
        egui::Frame::default()
            .fill(Self::bg_elevated())
            .stroke(egui::Stroke::new(1.0, Self::border()))
            .corner_radius(egui::CornerRadius::same(10))
            .inner_margin(egui::Margin::same(14))
    }

    /// 导航项（选中高亮）。
    pub fn nav_button(ui: &mut egui::Ui, label: &str, icon: &str, selected: bool) -> bool {
        // ZCode 式导航：选中 = 高亮底色整行 + 亮字；未选中 = 弱灰字，
        // hover 极淡提亮；不用左侧强调条（与列表选中语言一致）
        let text = format!("{icon}  {label}");
        let (text_color, fill) = if selected {
            (Self::text(), Self::bg_hover().gamma_multiply(0.8))
        } else {
            (Self::text_faint(), egui::Color32::TRANSPARENT)
        };
        let size = Vec2::new(ui.available_width().max(120.0), 32.0);
        let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::click());
        // 高亮框内缩（与会话行同语言）：左 8px/右 4px 的可见边距
        let fill_rect = egui::Rect::from_min_max(
            egui::pos2(rect.left() + 8.0, rect.top() + 1.0),
            egui::pos2(rect.right() - 4.0, rect.bottom() - 1.0),
        );
        if resp.hovered() && !selected {
            ui.painter()
                .rect_filled(fill_rect, 7.0, Self::bg_hover().gamma_multiply(0.3));
        } else if selected {
            ui.painter().rect_filled(fill_rect, 7.0, fill);
        }
        let galley =
            ui.painter()
                .layout(text, FontId::proportional(13.5), text_color, f32::INFINITY);
        let mb = galley.mesh_bounds;
        let text_pos = egui::pos2(
            rect.left() + 14.0,
            rect.center().y - mb.height() / 2.0 - mb.min.y,
        );
        ui.painter().galley(text_pos, galley, egui::Color32::WHITE);
        // painter 绘制不进 accessibility 树，手动补上（屏幕阅读器可导航）
        resp.widget_info(|| {
            egui::WidgetInfo::labeled(egui::WidgetType::Button, true, format!("{icon} {label}"))
        });
        resp.clicked()
    }

    /// 小节标题。
    pub fn section_title(text: &str) -> RichText {
        RichText::new(text).size(16.0).strong().color(Self::text())
    }

    /// 弱化文本。
    pub fn dim(text: &str) -> RichText {
        RichText::new(text).size(11.0).color(Self::text_dim())
    }

    /// 输入框样式。
    pub fn text_edit(ui: &mut egui::Ui, value: &mut String, hint: &str, width: f32) {
        ui.add(
            egui::TextEdit::multiline(value)
                .hint_text(RichText::new(hint).color(Self::text_faint()))
                .desired_width(width)
                .desired_rows(2)
                .text_color(Self::text())
                .background_color(Self::bg()),
        );
    }

    pub fn font(size: f32) -> FontId {
        FontId::proportional(size)
    }

    // ===== 共享设计组件：聊天输入框（ZCode 风格）chips 推广到全部页面 =====
    //
    // 设计基准取自会话输入框：等高小尺寸 chips（8px 圆角、11.5px 字号、
    // 细边框 + hover 亮底）、实心胶囊主按钮、圆角 elevated 卡片容器。
    // 插件 / 技能 / 设置页的 tab 切换、操作按钮与卡片统一走这些组件。

    /// 页面标题（无全局顶栏，各内容区自带）。
    pub fn page_title(text: impl Into<String>) -> RichText {
        RichText::new(text).size(17.0).strong().color(Self::text())
    }

    /// 卡片容器：圆角 elevated + 细边框（与聊天输入容器同族）。
    pub fn card() -> egui::Frame {
        egui::Frame::default()
            .fill(Self::bg_elevated())
            .stroke(egui::Stroke::new(1.0, Self::border()))
            .corner_radius(egui::CornerRadius::same(10))
            .inner_margin(egui::Margin::symmetric(14, 10))
    }

    /// 卡片内小节标题。
    pub fn card_section_title(text: impl Into<String>) -> RichText {
        RichText::new(text).size(12.5).strong().color(Self::accent_light())
    }

    /// 分段 Tab（页面内 tab 切换）：等高胶囊，选中 = 亮底 + 边框 +
    /// accent 文字，悬停 = 半透明亮底。手绘 + 手动注册 a11y
    /// （nav_button 同模式；painter 绘制不进 accessibility 树）。
    pub fn segment_tab(
        ui: &mut egui::Ui,
        label: impl Into<String>,
        selected: bool,
    ) -> egui::Response {
        const H: f32 = 26.0;
        let label = label.into();
        let color =
            if selected { Self::accent_light() } else { Self::text_dim() };
        let galley = ui.painter().layout_no_wrap(
            label.clone(),
            FontId::proportional(12.0),
            color,
        );
        let w = galley.size().x + 26.0;
        let (rect, resp) =
            ui.allocate_exact_size(Vec2::new(w, H), egui::Sense::click());
        if selected {
            ui.painter().rect(
                rect,
                8.0,
                Self::bg_hover(),
                egui::Stroke::new(1.0, Self::border()),
                egui::StrokeKind::Inside,
            );
        } else if resp.hovered() {
            ui.painter().rect_filled(
                rect,
                8.0,
                Self::bg_hover().gamma_multiply(0.5),
            );
        }
        let mb = galley.mesh_bounds;
        let pos = egui::pos2(
            rect.center().x - mb.width() / 2.0 - mb.min.x,
            rect.center().y - mb.height() / 2.0 - mb.min.y,
        );
        ui.painter().galley(pos, galley, Color32::WHITE);
        resp.widget_info(|| {
            egui::WidgetInfo::labeled(
                egui::WidgetType::Button,
                true,
                label.to_owned(),
            )
        });
        resp
    }

    /// 次级小按钮（输入框 chips 同款：等高、细边框、hover 亮底）。
    pub fn mini_button(
        ui: &mut egui::Ui,
        label: impl Into<String>,
        tint: ChipTint,
    ) -> egui::Response {
        const H: f32 = 24.0;
        let label = label.into();
        let color = match tint {
            ChipTint::Neutral => Self::text_dim(),
            ChipTint::Accent => Self::accent_light(),
            ChipTint::Danger => Self::err(),
        };
        let galley = ui.painter().layout_no_wrap(
            label.clone(),
            FontId::proportional(11.5),
            color,
        );
        let w = (galley.size().x + 20.0).max(34.0);
        let (rect, resp) =
            ui.allocate_exact_size(Vec2::new(w, H), egui::Sense::click());
        let hovered = resp.hovered();
        let (bg, stroke) = match tint {
            ChipTint::Neutral => {
                let bg = if hovered {
                    Self::bg_hover()
                } else {
                    Self::bg_elevated()
                };
                let stroke = if hovered {
                    egui::Stroke::new(1.0, Self::text_faint())
                } else {
                    egui::Stroke::new(1.0, Self::border())
                };
                (bg, stroke)
            }
            ChipTint::Accent => {
                let bg = if hovered {
                    Self::accent().gamma_multiply(0.18)
                } else {
                    Self::accent().gamma_multiply(0.10)
                };
                (bg, egui::Stroke::new(1.0, Self::accent().gamma_multiply(0.45)))
            }
            ChipTint::Danger => {
                let bg = if hovered {
                    Self::err().gamma_multiply(0.20)
                } else {
                    Self::err().gamma_multiply(0.08)
                };
                (bg, egui::Stroke::new(1.0, Self::err().gamma_multiply(0.50)))
            }
        };
        ui.painter().rect(rect, 8.0, bg, stroke, egui::StrokeKind::Inside);
        // 用 mesh_bounds（实际字形墨迹边界）垂直居中而非 logical size
        // （行高含 leading，混排 emoji/CJK 时字形在行内偏移——视觉偏上/偏下）
        let mb = galley.mesh_bounds;
        let pos = egui::pos2(
            rect.center().x - mb.width() / 2.0 - mb.min.x,
            rect.center().y - mb.height() / 2.0 - mb.min.y,
        );
        ui.painter().galley(pos, galley, Color32::WHITE);
        resp.widget_info(|| {
            egui::WidgetInfo::labeled(
                egui::WidgetType::Button,
                true,
                label.to_owned(),
            )
        });
        resp
    }

    /// 主操作胶囊按钮（发送按钮同款：实心 accent + 白字 + 大圆角）。
    /// `enabled=false` 时置灰不可点（如后台任务进行中）。
    pub fn primary_button(
        ui: &mut egui::Ui,
        label: impl Into<String>,
        enabled: bool,
    ) -> egui::Response {
        ui.add_enabled(
            enabled,
            egui::Button::new(
                RichText::new(label).size(12.5).color(Color32::WHITE),
            )
            .fill(Self::accent())
            .corner_radius(13.0)
            .min_size(Vec2::new(0.0, 26.0)),
        )
    }

    /// 状态胶囊（非交互）：低饱和语义色底 + 同色文字（运行中/已安装…）。
    pub fn status_pill(ui: &mut egui::Ui, text: impl Into<String>, color: Color32) {
        let text = text.into();
        let galley = ui.painter().layout_no_wrap(
            text.clone(),
            FontId::proportional(10.5),
            color,
        );
        let (rect, resp) = ui.allocate_exact_size(
            Vec2::new(galley.size().x + 14.0, 18.0),
            egui::Sense::hover(),
        );
        ui.painter()
            .rect_filled(rect, 9.0, color.gamma_multiply(0.13));
        let mb = galley.mesh_bounds;
        let pos = egui::pos2(
            rect.center().x - mb.width() / 2.0 - mb.min.x,
            rect.center().y - mb.height() / 2.0 - mb.min.y,
        );
        ui.painter().galley(pos, galley, Color32::WHITE);
        resp.widget_info(|| {
            egui::WidgetInfo::labeled(
                egui::WidgetType::Label,
                true,
                text.to_owned(),
            )
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 全局调色盘是共享状态：涉及 set_palette 的测试必须持此锁串行
    /// （并行测试下 apply 断言会读到其它测试切换中的调色盘——历史 flake）。
    static PALETTE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// 回归：egui 0.36 双主题槽机制下，自定义主题必须同时写入
    /// dark/light 两个槽（否则系统浅色模式下整个主题被忽略）。
    #[test]
    fn apply_writes_both_theme_slots() {
        let _g = PALETTE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        Theme::set_palette(&Palette::dark());
        let ctx = egui::Context::default();
        Theme::apply(&ctx);
        for theme in [egui::Theme::Dark, egui::Theme::Light] {
            let style = ctx.style_of(theme);
            assert_eq!(
                style.visuals.panel_fill,
                Palette::dark().bg.0,
                "{theme:?} 槽面板色必须是主题 bg"
            );
            assert_eq!(
                style.visuals.selection.bg_fill,
                Palette::dark().accent.0,
                "{theme:?} 槽选择色必须是强调色"
            );
        }
    }

    /// 主题切换：调色盘全局生效，浅色主题切换 egui 主题模式。
    #[test]
    fn palette_switch_takes_effect() {
        let _g = PALETTE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        Theme::set_palette(&Palette::graphite());
        assert_eq!(Theme::bg(), Palette::graphite().bg.0);
        assert!(!Theme::is_light(), "graphite 是深色主题");

        Theme::set_palette(&Palette::paper_light());
        assert_eq!(Theme::bg(), Palette::paper_light().bg.0);
        assert!(Theme::is_light(), "paper-light 是浅色主题");

        // Cursor Dark 内置主题：编辑器背景 #1e1e1e、VS Code 蓝强调
        Theme::set_palette(&Palette::cursor_dark());
        assert_eq!(
            Theme::bg(),
            Color32::from_rgb(30, 30, 30),
            "cursor-dark 背景 = #1e1e1e"
        );
        assert!(!Theme::is_light(), "cursor-dark 是深色主题");
        assert_eq!(Theme::accent(), Color32::from_rgb(0, 122, 204));
        assert!(
            Palette::all_builtin()
                .iter()
                .any(|p| p.name == "cursor-dark"),
            "cursor-dark 应在内置主题列表"
        );
        // 还原，避免污染其它测试
        Theme::set_palette(&Palette::dark());
    }

    /// 主题文件往返：序列化 → 反序列化（未指定槽位回退 dark）。
    #[test]
    fn theme_json_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("my-theme.json");
        std::fs::write(
            &p,
            r##"{"name": "my-theme", "bg": "#101014", "accent": "#E06060"}"##,
        )
        .unwrap();
        let pal = Palette::load_json_file(&p).expect("parse");
        assert_eq!(pal.name, "my-theme");
        assert_eq!(pal.bg.0, Color32::from_rgb(16, 16, 20));
        assert_eq!(pal.accent.0, Color32::from_rgb(224, 96, 96));
        // 未指定槽位回退 dark
        assert_eq!(pal.text.0, Palette::dark().text.0);
        // 坏颜色报错
        let bad = dir.path().join("bad.json");
        std::fs::write(&bad, r##"{"name": "x", "bg": "notacolor"}"##).unwrap();
        assert!(Palette::load_json_file(&bad).is_err());
    }

    /// 主题发现：目录文件 + 内置合并，按名去重。
    #[test]
    fn theme_manager_discover() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("custom.json"),
            r##"{"name": "custom", "accent": "#123456"}"##,
        )
        .unwrap();
        let mgr = ThemeManager::discover(&[dir.path().join("custom.json")]);
        let names = mgr.names();
        assert!(names.contains(&"dark".to_string()));
        assert!(names.contains(&"custom".to_string()));
        assert!(mgr.get("custom").is_some());
        // 不存在的文件跳过不炸
        let mgr2 = ThemeManager::discover(&[dir.path().join("nope.json")]);
        assert_eq!(mgr2.names().len(), Palette::all_builtin().len());
    }
}
