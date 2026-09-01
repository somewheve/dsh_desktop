//! 顶层应用：现代化侧边导航布局 + 引擎接线。

use std::sync::mpsc::channel;
use std::sync::{Arc, Mutex};

use eframe::egui;
use egui::{CentralPanel, Panel, RichText};
use log::{info, warn};

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
    Code,
    Settings,
}

pub struct DshDesktopApp {
    code: crate::ui::code_browser::CodeBrowser,
    cfg: AppConfig,
    tab: Tab,
    chat: ChatTab,
    skills: SkillsPanel,
    ext: ExtensionsTab,
    web_probe: crate::dsh::WebProbe,
    settings: SettingsTab,
    /// DSH web 子进程托管（启动/停止/重启；侧边栏手动控制）
    web: crate::dsh::WebHandle,
    /// dsh CLI 是否已安装（启动时探测一次；未安装隐藏 web 操作按钮）
    dsh_installed: bool,
    /// 插件变更后请求重启 web（扩展页置位，帧循环消费——使 web 进程内
    /// 的 cordis 插件配置生效；注释承诺的联动此前从未接线）
    web_restart_flag: std::sync::Arc<std::sync::atomic::AtomicBool>,
    bridge_port: Option<u16>,
    /// 桥鉴权 token（调用方需带 Authorization: Bearer <token>）
    bridge_token: Option<String>,
    engine: Arc<Mutex<DshEngine>>,
    /// 已持久化的工作区（变更时写 cfg.last_workspace）
    ws_saved: Option<String>,
    /// 侧栏"会话"导航下方的会话列表展开状态（收纳自 Chat 中央区）
    sessions_expanded: bool,
}

impl DshDesktopApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let cfg = AppConfig::load();
        // 主题：内置 + $DSH_HOME/themes/*.json + 插件主题（合并去重），
        // 激活配置保存的主题名（找不到回退 dark）。插件主题在引擎构造后
        // 二次发现（autostart 插件此时已可见），首帧先用已知名激活。
        {
            let mut theme_files: Vec<std::path::PathBuf> = Vec::new();
            let themes_dir = cfg.dsh_home.join("themes");
            if let Ok(entries) = std::fs::read_dir(&themes_dir) {
                for e in entries.flatten() {
                    let p = e.path();
                    if p.extension().map(|x| x == "json").unwrap_or(false) {
                        theme_files.push(p);
                    }
                }
            }
            let mgr = crate::ui::theme::ThemeManager::discover(&theme_files);
            let palette = mgr
                .get(&cfg.theme)
                .cloned()
                .unwrap_or_else(crate::ui::theme::Palette::dark);
            crate::ui::theme::Theme::set_palette(&palette);
            Theme::apply(&cc.egui_ctx);
            // 字体大小（全局缩放）：16=1.0x；用户保存的偏好在此生效
            cc.egui_ctx.set_zoom_factor(crate::config::ui_zoom(cfg.font_size));
        }
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

        // 桥（xitca-web 访问交互层；带随机 token 鉴权）
        info!("app: before bridge");
        let (bridge_port, bridge_token) = start_bridge(engine.clone())
            .map(|(p, t)| {
                info!("bridge on port {p} (token auth enabled)");
                (Some(p), Some(t))
            })
            .unwrap_or((None, None));
        info!("app: after bridge");

        // 恢复上次打开的工作区（工具根目录 / 新会话 / 终端直接可用）
        if let Some(ws) = &cfg.last_workspace {
            if ws.is_dir() {
                match engine.lock().unwrap_or_else(|p| p.into_inner()).set_workspace(ws) {
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
        let mut ext = ExtensionsTab::new(engine.clone(), lang);
        let code = crate::ui::code_browser::CodeBrowser::new(engine.clone(), lang);
        let mut chat = ChatTab::new(engine.clone(), event_rx);
        chat.lang = lang;

        let web_probe = crate::dsh::WebProbe::start(cfg.web_port);
        let ws_saved = cfg
            .last_workspace
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned());
        let web_restart_flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        ext.set_web_restart_flag(web_restart_flag.clone());
        let dsh_installed = crate::dsh::cli::find_dsh().is_some();
        if !dsh_installed {
            log::info!("startup: dsh CLI not found — DSH Web 操作按钮隐藏");
        }
        let mut app = Self {
            cfg,
            tab: Tab::Chat,
            chat,
            skills,
            ext,
            code,
            web_probe,
            settings,
            web: crate::dsh::WebHandle::new(),
            dsh_installed,
            web_restart_flag,
            bridge_port,
            bridge_token,
            engine,
            ws_saved,
            sessions_expanded: true,
        };
        // 默认不自动拉起 DSH web（用户手动在侧边栏启动）。
        if app.web_probe.is_up() {
            info!(
                "startup: dsh web already running on port {} (external)",
                app.cfg.web_port
            );
        }
        {
            let engine = app.engine.lock().unwrap_or_else(|p| p.into_inner());
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
        // 首启引导：无 API key 时直接落到设置页（否则首次发送才报错，
        // 错误又只出现在底部状态栏，发现性差）
        {
            let has_key = app
                .engine
                .lock()
                .map(|e| e.settings().api_key.is_some())
                .unwrap_or(false);
            if !has_key {
                info!("startup: no api key -> opening settings tab");
                app.tab = Tab::Settings;
                app.settings.status =
                    "请填写 DeepSeek API Key 并保存后，回到会话页开始使用".into();
            }
        }
        app
    }

    fn sidebar(&mut self, ui: &mut egui::Ui) {
        // 简约侧栏：小型标识 + 无框导航 + 弱化状态（视觉重量让位内容区）
        ui.add_space(14.0);
        ui.label(
            RichText::new("dsh · desktop")
                .size(13.0)
                .strong()
                .color(Theme::text_faint()),
        );
        ui.add_space(10.0);
        let lang = crate::ui::i18n::Lang::parse(&self.cfg.lang);
        use crate::ui::i18n::tr;
        let chat_active = self.tab == Tab::Chat;
        let chat_label = format!(
            "{} {}",
            tr(lang, "会话", "Chat"),
            if self.sessions_expanded { "▾" } else { "▸" }
        );
        // "会话"导航行：nav_button 占左侧 + ＋ 按钮右端对齐
        {
            let plus_w = 30.0;
            let avail = ui.available_width();
            let (nav_rect, _) =
                ui.allocate_exact_size(egui::vec2(avail - plus_w, 34.0), egui::Sense::hover());
            let mut nav_ui = ui.new_child(egui::UiBuilder::new().max_rect(nav_rect));
            nav_ui.set_clip_rect(nav_rect);
            if Theme::nav_button(&mut nav_ui, &chat_label, "💬", chat_active) {
                self.tab = Tab::Chat;
                self.sessions_expanded = !self.sessions_expanded;
            }
            // ＋ 按钮（右端、与导航行同高）
            let plus_rect = egui::Rect::from_min_size(
                egui::pos2(nav_rect.right() + 4.0, nav_rect.top()),
                egui::vec2(plus_w - 6.0, 34.0),
            );
            let plus_resp = ui.interact(
                plus_rect,
                ui.id().with("plus_new_session"),
                egui::Sense::click(),
            );
            if plus_resp.hovered() {
                ui.painter().rect_filled(plus_rect, 6.0, Theme::bg_hover());
            }
            ui.painter().text(
                plus_rect.center(),
                egui::Align2::CENTER_CENTER,
                "＋",
                egui::FontId::proportional(14.0),
                if plus_resp.hovered() {
                    Theme::accent_light()
                } else {
                    Theme::text_dim()
                },
            );
            plus_resp.clone().widget_info(|| {
                egui::WidgetInfo::labeled(egui::WidgetType::Button, true, "＋ 新建会话")
            });
            let _ = plus_resp
                .clone()
                .on_hover_text(tr(lang, "新建会话", "New session"));
            if plus_resp.clicked() {
                let mut engine = self.engine.lock().unwrap_or_else(|p| p.into_inner());
                if let Ok(id) = engine.create_session(None) {
                    drop(engine);
                    self.chat.refresh_sessions();
                    self.chat.open(&id);
                    self.tab = Tab::Chat;
                }
            }
        }
        if self.sessions_expanded {
            // 点击/新建会话时自动切回会话页（否则在设置/技能页点会话无反馈）
            if self.chat.ui_session_list(ui, None).is_some() {
                self.tab = Tab::Chat;
            }
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
            &tr(lang, "代码", "Code"),
            "📂",
            self.tab == Tab::Code,
        ) {
            self.tab = Tab::Code;
        }
        if Theme::nav_button(
            ui,
            &tr(lang, "设置", "Settings"),
            "⚙️",
            self.tab == Tab::Settings,
        ) {
            self.tab = Tab::Settings;
        }
        // 桥/引擎状态：两行弱化小字（信息保留，视觉降噪）
        ui.with_layout(egui::Layout::bottom_up(egui::Align::LEFT), |ui| {
            ui.add_space(6.0);
            if let Some(port) = self.bridge_port {
                ui.label(Theme::dim(&format!("bridge 127.0.0.1:{port}")));
            }
            // DSH Web：状态一行 + 极简文字按钮（不再是按钮堆）。
            // dsh CLI 未安装且 web 未运行 → 整行隐藏（启动按钮对未安装
            // 机器只会报错）；未安装但外部 web 在跑 → 只显示状态和打开。
            let web_row_visible = self.dsh_installed || self.web_probe.is_up();
            if web_row_visible {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing = egui::vec2(5.0, 0.0);
                let web_running = self.web_probe.is_up();
                let managed = self.web.managed();
                let dot = if web_running { "●" } else { "○" };
                let status = if web_running {
                    if managed {
                        tr(lang, "DSH Web", "DSH Web")
                    } else {
                        tr(lang, "DSH Web（外部）", "DSH Web (external)")
                    }
                } else {
                    tr(lang, "DSH Web", "DSH Web")
                };
                // 状态文本与"打开"链接：24px 行高内 galley 垂直居中
                //（历史缺陷：11px 文本与 24px mini_button 顶对齐不匹配）
                let status_color = if web_running {
                    Theme::ok()
                } else {
                    Theme::text_faint()
                };
                let text = format!("{dot} {status}");
                let galley = ui.fonts_mut(|f| {
                    f.layout_no_wrap(
                        text.clone(),
                        egui::FontId::proportional(11.0),
                        status_color,
                    )
                });
                let (rect, resp) = ui.allocate_exact_size(
                    egui::vec2(galley.size().x, 24.0),
                    egui::Sense::hover(),
                );
                let mb = galley.mesh_bounds;
                ui.painter().galley(
                    egui::pos2(
                        rect.left(),
                        rect.center().y - mb.height() / 2.0 - mb.min.y,
                    ),
                    galley,
                    egui::Color32::WHITE,
                );
                // 自绘文本补 a11y 标签（读屏可见）
                let a11y = text.clone();
                resp.widget_info(move || {
                    egui::WidgetInfo::labeled(
                        egui::WidgetType::Label,
                        true,
                        a11y.clone(),
                    )
                });
                if web_running && !managed {
                    // 外部 web：可按端口终止（历史缺陷:外部态无任何控制,
                    // 端口被占后用户走投无路——只能去任务管理器找 node）
                    if Theme::mini_button(
                        ui,
                        tr(lang, "停止", "stop"),
                        crate::ui::theme::ChipTint::Danger,
                    )
                    .on_hover_text(tr(
                        lang,
                        "终止占用端口的外部 web 进程（netstat 定位 PID）",
                        "Terminate the external web process holding the port (PID via netstat)",
                    ))
                    .clicked()
                    {
                        match crate::dsh::cli::kill_web_by_port(self.cfg.web_port) {
                            Ok(msg) => {
                                info!("{msg}");
                                self.settings.status = msg.clone();
                                self.chat.status = msg.clone();
                            }
                            Err(e) => {
                                warn!("kill external web failed: {e}");
                                self.settings.status = format!("停止外部 web 失败：{e}");
                                self.chat.status = format!("停止外部 web 失败：{e}");
                            }
                        }
                    }
                }
                if web_running {
                    // "打开"链接：同样 24px 行高居中（自绘 + 点击开 URL）
                    let label = tr(lang, "打开", "open");
                    let galley = ui.fonts_mut(|f| {
                        f.layout_no_wrap(
                            label.clone(),
                            egui::FontId::proportional(11.0),
                            Theme::accent_light(),
                        )
                    });
                    let w = galley.size().x;
                    let (rect, resp) = ui.allocate_exact_size(
                        egui::vec2(w, 24.0),
                        egui::Sense::click(),
                    );
                    let mb = galley.mesh_bounds;
                    ui.painter().galley(
                        egui::pos2(
                            rect.left(),
                            rect.center().y - mb.height() / 2.0 - mb.min.y,
                        ),
                        galley,
                        egui::Color32::WHITE,
                    );
                    if resp.hovered() {
                        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                    }
                    if resp.clicked() {
                        ui.ctx().open_url(egui::OpenUrl::same_tab(
                            self.cfg.web_url(),
                        ));
                    }
                } else if self.dsh_installed
                    && !managed
                    && Theme::mini_button(
                    ui,
                    &tr(lang, "启动", "start"),
                    crate::ui::theme::ChipTint::Accent,
                )
                .on_hover_text(tr(
                    lang,
                    "启动 DSH Web（插件功能的入口：web 进程承载插件）",
                    "Start DSH Web (plugins live in the web process)",
                ))
                .clicked()
                {
                    // 失败信息经 WebHandle::last_error 显示在 web 行内
                    //（spawn 失败即时可见；进程秒退由 reap 捕获 stderr 后
                    // 写入 last_error）——不再塞到聊天页（与点击位置脱节）
                    if let Err(e) = self.web.start() {
                        log::warn!("web start failed: {e:#}");
                    }
                }
                if managed {
                    if Theme::mini_button(
                        ui,
                        &tr(lang, "停止", "stop"),
                        crate::ui::theme::ChipTint::Neutral,
                    )
                    .clicked()
                    {
                        self.web.stop();
                    }
                    if Theme::mini_button(
                        ui,
                        &tr(lang, "重启", "restart"),
                        crate::ui::theme::ChipTint::Neutral,
                    )
                    .clicked()
                    {
                        self.web.restart();
                    }
                }
            });
            // 启动失败信息（spawn 失败 / 秒退 stderr）：紧跟 web 行显示，
            // 红色小字 + 悬浮全文；下次启动成功自动清除
            if let Some(err) = self.web.last_error() {
                let short: String = err.chars().take(90).collect();
                let ell = if err.chars().count() > 90 { "…" } else { "" };
                let resp = ui.label(
                    RichText::new(format!("⚠ {short}{ell}"))
                        .size(10.5)
                        .color(Theme::err()),
                );
                resp.on_hover_text(err);
            }
            }
        });
    }
}

impl eframe::App for DshDesktopApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // 语言同步：设置页切换后各面板立即生效
        let lang = crate::ui::i18n::Lang::parse(&self.cfg.lang);
        self.chat.lang = lang;
        self.skills.lang = lang;
        self.ext.lang = lang;
        self.code.lang = lang;
        self.settings.lang = lang;
        // 全局快捷键：Ctrl+N 新建会话 / Escape 关浮动卡片
        {
            let ctx = ui.ctx();
            if ctx.input(|i| i.modifiers.ctrl && i.key_pressed(egui::Key::N)) {
                let mut engine = self.engine.lock().unwrap_or_else(|p| p.into_inner());
                if let Ok(id) = engine.create_session(None) {
                    drop(engine);
                    self.chat.refresh_sessions();
                    self.chat.open(&id);
                    self.tab = Tab::Chat;
                }
            }
            if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
                self.chat.panel_expanded = crate::ui::chat_tab::PanelKind::None;
            }
        }
        // 引擎事件泵置顶（任何标签页都处理）：事件只驱动 ChatTab 状态，
        // 但泵本身必须无条件执行——否则切到技能/扩展/设置页时后台回合的
        // 事件积压在无界通道里，状态不更新、UI 冻结到下次鼠标移动
        self.chat.pump(&ui.ctx().clone());
        // 工作区变更 → 立即持久化 last_workspace（避免崩溃/异常退出丢失）
        {
            let ws = self.engine.lock().unwrap_or_else(|p| p.into_inner()).workspace_root().cloned();
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
            ui.set_width(172.0);
            self.sidebar(ui);
        });
        // 简约布局：无顶栏（页面标题由各内容区自带），底部一行弱化状态
        // 错误信息显示在底部状态栏（RUST_LOG 前）：不打断内容区布局，
        // 用户视线自然落底即可看到
        let chat_error = self.chat.error.clone();
        Panel::bottom("status").show(ui, |ui| {
            status_bar(
                ui,
                &self.cfg,
                self.web_probe.is_up(),
                lang,
                chat_error.as_deref(),
            )
        });

        // AI 消息里的文件路径点击 → 代码浏览器打开并切页（ZCode 式跳转）
        if let Some(p) = self.chat.open_file_req.take() {
            self.code.open_file(&p);
            self.tab = Tab::Code;
        }
        CentralPanel::default().show(ui, |ui| match self.tab {
            Tab::Chat => self.chat.ui(ui),
            Tab::Skills => self.skills.ui(ui),
            Tab::Extensions => {
                self.ext.lang = lang;
                self.ext.ui(ui);
            }
            Tab::Code => self.code.ui(ui),
            Tab::Settings => self.settings.ui(ui, &mut self.cfg),
        });
        // 桥访问信息（端口/token）注入设置页展示
        if let (Some(p), Some(t)) = (self.bridge_port, self.bridge_token.as_deref()) {
            self.settings.set_bridge_info(Some(p), Some(t.to_string()));
        }
        // web 子进程 reap（外部退出后清托管态）+ 插件变更重启信号
        self.web.reap();
        if self.web_restart_flag.swap(false, std::sync::atomic::Ordering::Relaxed)
            && self.web.managed()
        {
            info!("plugin change → restarting dsh web");
            self.web.restart();
        }
        // 排队消息泵：回合结束（idle）的会话自动取队首发起（FIFO）
        {
            let mut engine = self.engine.lock().unwrap_or_else(|p| p.into_inner());
            engine.pump_message_queue();
            // token 用量脏时落盘（无更新时零开销）
            engine.flush_usage();
        }
        // 插件（子进程扩展）输出泵：每帧处理响应/握手/退出（引擎短锁）
        let plugin_busy = {
            let engine = self.engine.lock().unwrap_or_else(|p| p.into_inner());
            engine.plugin_pump();
            engine.plugin_has_live_work()
        };
        if plugin_busy {
            // 有握手/工具响应等待中：定时重绘，响应到达不被"无鼠标输入不重绘"卡住
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(100));
        }
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

/// 加载字体（Consolas 界面主字体 + 等宽回退 + CJK 回退 + emoji 回退）到 egui。
///
/// CJK 与 emoji 回退注册前先做**度量归一**（[`util::font_tweak_to_match_primary`]）：
/// egui 按一行内每个字形所属字体自身的 ascent/row_height 摆放基线并取行高
/// 最大值，而雅黑（行高 1.32em）/Segoe Emoji 与 Consolas（1.17em）度量差异大，
/// 不归一则中英混排行高跳变、基线错位。归一后所有字体共用主字体的行高与
/// 基线，对任意字号成立。
pub(crate) fn setup_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();

    let primary = util::load_ui_font_consolas();
    let mono = util::load_mono_font();
    let cjk = util::load_cjk_font();
    let emoji = util::load_emoji_font();

    // Consolas：界面文字主字体（Windows 系统自带，含 Bold 四风格）。
    // 插到 Proportional 与 Monospace 家族首位；无中文字形 → 自动落到
    // 下方 CJK 回退链（雅黑，已归一到 Consolas 度量），emoji 落 Segoe。
    if let Some((name, bytes)) = primary.clone() {
        fonts
            .font_data
            .insert(name.clone(), egui::FontData::from_owned(bytes).into());
        for family in [
            egui::FontFamily::Proportional,
            egui::FontFamily::Monospace,
        ] {
            if let Some(list) = fonts.families.get_mut(&family) {
                list.insert(0, name.clone());
            }
        }
    }
    if let Some((name, bytes)) = mono {
        fonts
            .font_data
            .insert(name.clone(), egui::FontData::from_owned(bytes).into());
        if let Some(list) = fonts.families.get_mut(&egui::FontFamily::Monospace) {
            list.insert(1, name);
        }
    }
    // CJK 回退：度量归一（行高一致 + 基线对齐），追加到所有家族末尾。
    // 探测需要主字体存在；主字体缺失（非 Windows）时跳过归一。
    if let Some((name, bytes)) = cjk {
        let tweak = primary.as_ref().and_then(|p| {
            util::font_tweak_to_match_primary(
                p,
                &(name.clone(), bytes.clone()),
                '中',
            )
        });
        let mut data = egui::FontData::from_owned(bytes);
        if let Some(t) = tweak {
            data = data.tweak(t);
        }
        fonts.font_data.insert(name.clone(), data.into());
        for family in fonts.families.values_mut() {
            family.push(name.clone());
        }
    }
    // emoji 字体：插到主字体之后、egui 内置 NotoEmoji 之前，并同样归一。
    // 原因：NotoEmoji 单色字形被 egui 缩小到 0.81，📋 等 emoji 在 12px 下
    // 形似"口"字（用户报告的口字/tofu）；Segoe UI Emoji 字形更大更清晰。
    // 拉丁/中文等主字体已有的字符不受影响（resolve 按家族顺序取第一个
    // 有该字符的字体，主字体优先）。
    if let Some((name, bytes)) = emoji {
        let tweak = primary.as_ref().and_then(|p| {
            util::font_tweak_to_match_primary(
                p,
                &(name.clone(), bytes.clone()),
                '📋',
            )
        });
        let mut data = egui::FontData::from_owned(bytes);
        if let Some(t) = tweak {
            data = data.tweak(t);
        }
        fonts.font_data.insert(name.clone(), data.into());
        for family in fonts.families.values_mut() {
            if family.is_empty() {
                family.push(name.clone());
            } else {
                family.insert(1, name.clone());
            }
        }
    }
    // 符号字体：⧉（U+29C9 复制图标）等字形主字体/雅黑/emoji 都没有，
    // 只在 Segoe UI Symbol 中存在——注册为所有家族的最后兜底，避免
    // 渲染成 tofu"口"。放在末尾：已有字形仍由前面的字体优先提供。
    if let Some((name, bytes)) = util::load_symbol_font() {
        let tweak = primary.as_ref().and_then(|p| {
            util::font_tweak_to_match_primary(p, &(name.clone(), bytes.clone()), '⧉')
        });
        let mut data = egui::FontData::from_owned(bytes);
        if let Some(t) = tweak {
            data = data.tweak(t);
        }
        fonts.font_data.insert(name.clone(), data.into());
        for family in fonts.families.values_mut() {
            family.push(name.clone());
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
