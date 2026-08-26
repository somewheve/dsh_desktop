//! 顶层应用：现代化侧边导航布局 + 引擎接线。

use std::sync::mpsc::channel;
use std::sync::{Arc, Mutex};

use eframe::egui;
use egui::{CentralPanel, Panel, RichText};
use log::info;

use crate::bridge::start_bridge;
use crate::config::AppConfig;
use crate::core::{DshEngine, EngineEvent};
use crate::ui::chat_tab::ChatTab;
use crate::ui::extensions_tab::ExtensionsTab;
use crate::ui::settings_tab::SettingsTab;
use crate::ui::skills_panel::SkillsPanel;
use crate::ui::status_bar::status_bar;
use crate::ui::theme::Theme;
use crate::util;

#[derive(PartialEq, Clone, Copy)]
enum Tab {
    Chat,
    Skills,
    Extensions,
    Settings,
}

pub struct DshDesktopApp {
    cfg: AppConfig,
    tab: Tab,
    chat: ChatTab,
    skills: SkillsPanel,
    ext: ExtensionsTab,
    web_probe: crate::dsh::WebProbe,
    settings: SettingsTab,
    /// DSH web 子进程托管（启动/停止/重启；侧边栏手动控制）
    web: crate::dsh::WebHandle,
    bridge_port: Option<u16>,
    engine: Arc<Mutex<DshEngine>>,
    /// 已持久化的工作区（变更时写 cfg.last_workspace）
    ws_saved: Option<String>,
}

impl DshDesktopApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let cfg = AppConfig::load();
        Theme::apply(&cc.egui_ctx);
        setup_fonts(&cc.egui_ctx);

        // 引擎（Rust 原生 DSH 核心）
        let engine_settings = crate::core::settings::EngineSettings::load();
        let (event_tx, event_rx) = channel::<EngineEvent>();
        let engine = match DshEngine::new(engine_settings, event_tx.clone()) {
            Ok(e) => Arc::new(Mutex::new(e)),
            Err(e) => {
                // 仅存储目录等问题才会失败；UI 仍启动，功能在引擎侧报错
                log::warn!("engine init failed ({e}); UI 仍可启动");
                let es = crate::core::settings::EngineSettings::default();
                Arc::new(Mutex::new(
                    DshEngine::new(es, event_tx.clone()).unwrap_or_else(|e2| {
                        // 终极兜底：内存引擎（不持久化）
                        log::error!("engine fallback failed ({e2}); using in-memory engine");
                        let es2 = crate::core::settings::EngineSettings::default();
                        let es2 = crate::core::settings::EngineSettings {
                            data_dir: std::env::temp_dir().join("dsh-desktop-sessions"),
                            ..es2
                        };
                        DshEngine::new(es2, event_tx.clone())
                            .expect("in-memory engine must succeed")
                    }),
                ))
            }
        };

        // 桥（xitca-web 访问交互层）
        info!("app: before bridge");
        let bridge_port = start_bridge(engine.clone())
            .ok()
            .inspect(|p| info!("bridge on port {p}"));
        info!("app: after bridge");

        // 恢复上次打开的工作区（工具根目录 / 新会话 / 终端直接可用）
        if let Some(ws) = &cfg.last_workspace {
            if ws.is_dir() {
                match engine.lock().unwrap().set_workspace(ws) {
                    Ok(root) => info!("startup: restored workspace {root}"),
                    Err(e) => log::warn!("startup: workspace restore failed: {e}"),
                }
            } else {
                log::warn!(
                    "startup: saved workspace no longer exists, skipped: {}",
                    ws.display()
                );
            }
        }

        let lang = crate::ui::i18n::Lang::parse(&cfg.lang);
        let settings = SettingsTab::new(&cfg, engine.clone());
        let skills = SkillsPanel::new(engine.clone(), lang);
        let ext = ExtensionsTab::new(engine.clone(), lang);
        let mut chat = ChatTab::new(engine.clone(), event_rx);
        chat.lang = lang;

        let web_probe = crate::dsh::WebProbe::start(cfg.web_port);
        let ws_saved = cfg
            .last_workspace
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned());
        let mut app = Self {
            cfg,
            tab: Tab::Chat,
            chat,
            skills,
            ext,
            web_probe,
            settings,
            web: crate::dsh::WebHandle::new(),
            bridge_port,
            engine,
            ws_saved,
        };
        // 默认不自动拉起 DSH web（用户手动在侧边栏启动）。
        if app.web_probe.is_up() {
            info!(
                "startup: dsh web already running on port {} (external)",
                app.cfg.web_port
            );
        }
        {
            let engine = app.engine.lock().unwrap();
            let sessions = engine.list_sessions();
            drop(engine);
            log::info!("startup: {} sessions found", sessions.len());
            if let Some(first) = sessions.first() {
                log::info!("startup: opening session {}", first.session_id);
                app.chat.open(&first.session_id);
            } else {
                log::info!("startup: no sessions, waiting for user to create");
            }
        }
        let dsh_home_display = app.cfg.dsh_home.display().to_string();
        info!("app started; dsh_home={dsh_home_display}");
        app
    }

    fn sidebar(&mut self, ui: &mut egui::Ui) {
        ui.add_space(10.0);
        ui.horizontal(|ui| {
            ui.label(
                RichText::new("dsh-desktop")
                    .size(18.0)
                    .strong()
                    .color(Theme::ACCENT_LIGHT),
            );
        });
        ui.add_space(16.0);
        let lang = crate::ui::i18n::Lang::parse(&self.cfg.lang);
        use crate::ui::i18n::tr;
        if Theme::nav_button(ui, &tr(lang, "会话", "Chat"), "💬", self.tab == Tab::Chat) {
            self.tab = Tab::Chat;
        }
        if Theme::nav_button(
            ui,
            &tr(lang, "技能", "Skills"),
            "📚",
            self.tab == Tab::Skills,
        ) {
            self.tab = Tab::Skills;
        }
        if Theme::nav_button(
            ui,
            &tr(lang, "扩展", "Extensions"),
            "🔌",
            self.tab == Tab::Extensions,
        ) {
            self.tab = Tab::Extensions;
        }
        if Theme::nav_button(
            ui,
            &tr(lang, "设置", "Settings"),
            "⚙️",
            self.tab == Tab::Settings,
        ) {
            self.tab = Tab::Settings;
        }
        ui.add_space(20.0);
        ui.separator();
        ui.add_space(8.0);
        if let Some(port) = self.bridge_port {
            ui.label(Theme::dim(&format!("桥: 127.0.0.1:{port}")));
        }
        {
            let engine = self.engine.lock().unwrap();
            let has_key = engine.settings().api_key.is_some();
            let status = if has_key {
                "引擎就绪 ✓".to_string()
            } else {
                tr(lang, "未配置 API key", "API key not set")
            };
            let color = if has_key { Theme::OK } else { Theme::WARN };
            ui.label(RichText::new(status).size(11.0).color(color));
        }
        ui.with_layout(egui::Layout::bottom_up(egui::Align::LEFT), |ui| {
            let web_running = self.web_probe.is_up();
            let managed = self.web.managed();
            let status = if web_running {
                if managed {
                    "DSH Web 运行中（本应用托管）"
                } else {
                    "DSH Web 运行中（外部）"
                }
            } else {
                "DSH Web 未运行"
            };
            ui.label(RichText::new(status).size(11.0).color(if web_running {
                Theme::OK
            } else {
                Theme::WARN
            }));
            ui.horizontal(|ui| {
                if web_running {
                    ui.hyperlink_to(
                        RichText::new("打开 Web").color(Theme::CYAN),
                        self.cfg.web_url(),
                    );
                } else {
                    // 未运行 → 一键启动（插件功能的入口：web 进程承载插件）
                    if ui.small_button("启动 Web").clicked() {
                        let profile = self
                            .cfg
                            .profile
                            .clone()
                            .unwrap_or_else(|| "web".to_string());
                        match self.web.start(&profile) {
                            Ok(()) => info!("web started from sidebar (profile {profile})"),
                            Err(e) => {
                                log::warn!("web start failed: {e:#}");
                                self.chat.error = Some(format!("启动 DSH Web 失败：{e:#}"));
                            }
                        }
                    }
                }
                if managed {
                    if ui.small_button("停止").clicked() {
                        self.web.stop();
                    }
                    if ui.small_button("重启").clicked() {
                        let profile = self.web.profile().to_string();
                        self.web.restart(&profile);
                    }
                }
            });
        });
    }

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        let lang = crate::ui::i18n::Lang::parse(&self.cfg.lang);
        use crate::ui::i18n::tr;
        ui.horizontal(|ui| {
            let tab_name = match self.tab {
                Tab::Chat => tr(lang, "会话", "Chat"),
                Tab::Skills => tr(lang, "技能", "Skills"),
                Tab::Extensions => tr(lang, "扩展", "Extensions"),
                Tab::Settings => tr(lang, "设置", "Settings"),
            };
            ui.label(Theme::section_title(&tab_name));
        });
        ui.separator();
    }
}

impl eframe::App for DshDesktopApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // 语言同步：设置页切换后各面板立即生效
        let lang = crate::ui::i18n::Lang::parse(&self.cfg.lang);
        self.chat.lang = lang;
        self.skills.lang = lang;
        self.ext.lang = lang;
        // 工作区变更 → 立即持久化 last_workspace（避免崩溃/异常退出丢失）
        {
            let ws = self.engine.lock().unwrap().workspace_root().cloned();
            let ws_str = ws.as_ref().map(|p| p.to_string_lossy().into_owned());
            if ws_str != self.ws_saved {
                self.ws_saved = ws_str;
                self.cfg.last_workspace = ws;
                if let Err(e) = self.cfg.save() {
                    log::warn!("config save (last_workspace) failed: {e}");
                }
            }
        }
        Panel::left("sidebar").show(ui, |ui| {
            ui.set_width(180.0);
            self.sidebar(ui);
        });
        Panel::top("topbar").show(ui, |ui| self.top_bar(ui));
        Panel::bottom("status").show(ui, |ui| {
            status_bar(ui, &self.cfg, self.web_probe.is_up(), lang)
        });

        CentralPanel::default().show(ui, |ui| match self.tab {
            Tab::Chat => self.chat.ui(ui),
            Tab::Skills => self.skills.ui(ui),
            Tab::Extensions => {
                self.ext.lang = lang;
                self.ext.ui(ui);
            }
            Tab::Settings => self.settings.ui(ui, &mut self.cfg),
        });
        // 插件（子进程扩展）输出泵：每帧处理响应/握手/退出（引擎短锁）
        self.engine.lock().unwrap().plugin_pump();
    }

    fn on_exit(&mut self) {
        info!("exiting; saving config");
        let _ = self.cfg.save();
        if let Ok(engine) = self.engine.lock() {
            let _ = engine.settings().save();
        }
        self.web.stop();
    }
}

/// 加载系统字体（等宽 + CJK 回退）到 egui。
fn setup_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    if let Some((name, bytes)) = util::load_mono_font() {
        fonts
            .font_data
            .insert(name.clone(), egui::FontData::from_owned(bytes).into());
        if let Some(mono) = fonts.families.get_mut(&egui::FontFamily::Monospace) {
            mono.insert(0, name);
        }
    }
    if let Some((name, bytes)) = util::load_cjk_font() {
        fonts
            .font_data
            .insert(name.clone(), egui::FontData::from_owned(bytes).into());
        for family in fonts.families.values_mut() {
            family.push(name.clone());
        }
    }
    // emoji 字体：插到主字体之后、egui 内置 NotoEmoji 之前。
    // 原因：NotoEmoji 单色字形被 egui 缩小到 0.81，📋 等 emoji 在 12px 下
    // 形似"口"字（用户报告的口字/tofu）；Segoe UI Emoji 字形更大更清晰。
    // 拉丁/中文等主字体已有的字符不受影响（resolve 按家族顺序取第一个
    // 有该字符的字体，主字体优先）。
    if let Some((name, bytes)) = util::load_emoji_font() {
        fonts
            .font_data
            .insert(name.clone(), egui::FontData::from_owned(bytes).into());
        for family in fonts.families.values_mut() {
            if family.is_empty() {
                family.push(name.clone());
            } else {
                family.insert(1, name.clone());
            }
        }
    }
    // 粗体字体族（Markdown **加粗**）：egui 默认 strong 只是颜色增强，
    // 这里注册真正的粗体字形族，供 chat 消息加粗 span 使用。
    if let Some((name, bytes)) = util::load_bold_font() {
        fonts
            .font_data
            .insert(name.clone(), egui::FontData::from_owned(bytes).into());
        // "bold" 族 = 粗体字体优先 + 完整回退（复用 Proportional 的 fallback 链）
        let mut bold_family: Vec<String> = fonts
            .families
            .get(&egui::FontFamily::Proportional)
            .cloned()
            .unwrap_or_default();
        if !bold_family.is_empty() {
            bold_family.remove(0); // 去掉默认比例字体（拉丁部分粗体族自己覆盖）
        }
        bold_family.insert(0, name);
        fonts
            .families
            .insert(egui::FontFamily::Name("bold".into()), bold_family);
        // 标记 markdown 渲染可安全使用 bold 族（未注册时回退 proportional，避免 egui panic）
        crate::ui::markdown::set_bold_bound(true);
    }
    ctx.set_fonts(fonts);
}
