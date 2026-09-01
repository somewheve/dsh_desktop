//! 扩展面板：cordis 风格子进程插件管理，三个 Tab（与技能一致）：
//! - 已安装（管理）：子进程插件列表（状态/启动/停止）+ 本地已装 DSH 插件
//! - 远程安装：从 npm registry 联网获取 DSH 官方插件清单 → 下载供 node_called 直接调用
//! - 本地安装：从本地目录导入插件（plugin.json + 可执行程序）

use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};

use egui::{RichText, ScrollArea};

use crate::core::DshEngine;
use crate::engine::plugin::PluginStatus;
use crate::ui::i18n::{tr, Lang};
use crate::ui::theme::{ChipTint, Theme};

/// 扩展面板 Tab。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExtTab {
    Installed,
    Remote,
    Local,
}

/// 渲染期收集的插件操作（渲染结束后短锁统一执行，渲染期间不持引擎锁）。
enum ExtAction {
    StartAll,
    Start(String),
    Stop(String),
    Reload(String),
    Refresh,
    /// 删除本地插件（两段式确认后触发；停进程 + 删目录 + 重发现）
    Remove(String),
}

pub struct ExtensionsTab {
    engine: Arc<Mutex<DshEngine>>,
    pub lang: Lang,
    tab: ExtTab,
    /// 网络插件清单：(name, version)
    dsh_plugins: Vec<(String, String)>,
    /// 本地已装 DSH 插件名
    local_plugins: Vec<String>,
    bg_rx: Option<Receiver<String>>,
    busy: bool,
    status: String,
    error: Option<String>,
    /// 插件变更 → 请求重启 DSH Web（app 帧循环消费；托管中才真正重启）
    web_restart: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    /// 待确认删除的插件名（两段式：第一次点 ✕ 进入确认态，再点执行）
    pending_remove: Option<String>,
    /// 本帧收集的插件操作（ui 结束后统一短锁执行）
    actions: Vec<ExtAction>,
}

impl ExtensionsTab {
    pub fn new(engine: Arc<Mutex<DshEngine>>, lang: Lang) -> Self {
        let mut tab = Self {
            engine,
            lang,
            tab: ExtTab::Installed,
            dsh_plugins: Vec::new(),
            local_plugins: Vec::new(),
            bg_rx: None,
            busy: false,
            web_restart: None,
            pending_remove: None,
            status: String::new(),
            error: None,
            actions: Vec::new(),
        };
        tab.refresh_local();
        tab
    }

    /// 注入 web 重启信号（app 构造时调用）。
    pub fn set_web_restart_flag(
        &mut self,
        flag: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) {
        self.web_restart = Some(flag);
    }

    /// 请求重启 DSH Web（插件变更后；app 帧循环消费，托管中才真正重启）
    fn request_web_restart(&self) {
        if let Some(f) = &self.web_restart {
            f.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// 刷新本地已装 DSH 插件 + 重读缓存的远程清单（不再每帧读盘）。
    fn refresh_local(&mut self) {
        let cfg = crate::config::AppConfig::load();
        self.local_plugins = crate::dsh::plugins::scan_dsh_plugins(&cfg)
            .into_iter()
            .map(|(n, _, _)| n)
            .collect();
        let cache_file = {
            let engine = self.engine.lock().unwrap_or_else(|p| p.into_inner());
            engine.skills_dir().join(".dsh-plugin-list.json")
        };
        let cached: Vec<(String, String)> = std::fs::read_to_string(&cache_file)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        if !cached.is_empty() {
            self.dsh_plugins = cached;
        }
    }

    fn pump_bg(&mut self) {
        let rx = self.bg_rx.take();
        if let Some(rx) = rx {
            let mut lines = Vec::new();
            let mut done = false;
            while let Ok(line) = rx.try_recv() {
                if line.starts_with("__DONE__") {
                    done = true;
                }
                lines.push(line);
            }
            for l in lines {
                if let Some(rest) = l.strip_prefix("__DONE__ ok") {
                    self.status = rest.trim().to_string();
                    self.error = None;
                } else if let Some(rest) = l.strip_prefix("__DONE__ err: ") {
                    self.error = Some(rest.to_string());
                    self.status.clear();
                }
            }
            if done {
                self.busy = false;
                self.refresh_local();
            } else {
                self.bg_rx = Some(rx);
            }
        }
    }

    fn run_in_background<F>(&mut self, f: F)
    where
        F: FnOnce() -> Result<String, String> + Send + 'static,
    {
        if self.busy {
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel::<String>();
        self.bg_rx = Some(rx);
        self.busy = true;
        self.status = String::new();
        self.error = None;
        std::thread::Builder::new()
            .name("ext-op".into())
            .spawn(move || {
                let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f))
                    .unwrap_or_else(|_| Err("扩展操作线程 panic".into()));
                match res {
                    Ok(msg) => {
                        let _ = tx.send(format!("__DONE__ ok {msg}"));
                    }
                    Err(e) => {
                        let _ = tx.send(format!("__DONE__ err: {e}"));
                    }
                }
            })
            .map_err(|e| e.to_string())
            .err()
            .map(|e| {
                // 线程启动失败：立即解除 busy 并提示（否则面板永久卡"操作中"）
                self.busy = false;
                self.error = Some(format!("后台线程启动失败: {e}"));
                self.bg_rx = None;
            });
    }

    pub fn ui(&mut self, ui: &mut egui::Ui) {
        self.pump_bg();
        let lang = self.lang;
        ui.label(Theme::page_title(tr(lang, "扩展", "Extensions")));
        ui.add_space(3.0);
        ui.label(
            RichText::new(tr(
                lang,
                "cordis 风格子进程插件：任意语言编写，agent 回合可直接调用插件提供的工具；DSH 官方 cordis 插件可联网下载，agent 经 node_called 直接 require 调用（无需转写）。",
                "Cordis-style subprocess plugins: write in any language; the agent can call their tools directly. DSH official cordis plugins can be fetched online and called by the agent via node_called (no transcription needed).",
            ))
            .size(11.0)
            .color(Theme::text_dim()),
        );
        ui.add_space(8.0);

        // 短锁取快照（目录/插件状态/领域声明），渲染期间不持锁：
        // ui_local 的 rfd 文件对话框会阻塞 UI（持锁 = 冻结全部引擎工作），
        // 且 ui_local 内部还会再 lock 同一非重入锁（历史死锁点）
        let (dir, snapshot, skills_dir, domains) = {
            let engine = self.engine.lock().unwrap_or_else(|p| p.into_inner());
            (
                engine.plugin_dir(),
                engine.plugin_snapshot(),
                engine.skills_dir(),
                engine.plugin_domains(),
            )
        };
        // 插件名 → 领域（核心层增强插件按 domain 声明归组展示）
        let domain_of: std::collections::HashMap<String, String> = domains.into_iter().collect();
        // 目录行（info 左 + chips 右，与技能页同构）
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(format!(
                    "📁 {}: {}",
                    tr(lang, "插件目录", "Plugin dir"),
                    dir.display()
                ))
                .size(11.0)
                .color(Theme::text_dim()),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if Theme::mini_button(
                    ui,
                    tr(lang, "打开目录", "Open folder"),
                    ChipTint::Neutral,
                )
                .clicked()
                {
                    #[cfg(windows)]
                    {
                        let _ = std::process::Command::new("explorer.exe")
                            .arg(dir.display().to_string())
                            .spawn();
                    }
                    #[cfg(not(windows))]
                    {
                        let _ = std::process::Command::new("xdg-open")
                            .arg(dir.display().to_string())
                            .spawn();
                    }
                }
                if Theme::mini_button(
                    ui,
                    tr(lang, "刷新", "Refresh"),
                    ChipTint::Neutral,
                )
                .clicked()
                {
                    self.actions.push(ExtAction::Refresh);
                    self.refresh_local();
                }
            });
        });
        ui.add_space(8.0);

        // Tab 行（分段胶囊，与聊天输入框 chips 同风格）
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing = egui::vec2(4.0, 0.0);
            for (tab, zh, en) in [
                (ExtTab::Installed, "已安装", "Installed"),
                (ExtTab::Remote, "远程安装", "Remote"),
                (ExtTab::Local, "本地安装", "Local"),
            ] {
                if Theme::segment_tab(ui, tr(lang, zh, en), self.tab == tab)
                    .on_hover_text(match tab {
                        ExtTab::Installed => {
                            tr(lang, "管理已安装的扩展", "Manage installed extensions")
                        }
                        ExtTab::Remote => tr(
                            lang,
                            "联网获取 DSH 官方插件",
                            "Fetch DSH official plugins online",
                        ),
                        ExtTab::Local => tr(
                            lang,
                            "从本地目录导入插件",
                            "Import plugin from local folder",
                        ),
                    })
                    .clicked()
                {
                    self.tab = tab;
                }
            }
            if self.busy {
                ui.spinner();
            }
        });
        ui.add_space(8.0);

        match self.tab {
            ExtTab::Installed => self.ui_installed(ui, lang, &snapshot, &skills_dir, &domain_of),
            ExtTab::Remote => self.ui_remote(ui, lang, &skills_dir),
            ExtTab::Local => self.ui_local(ui, lang),
        }
        // 渲染期收集的操作：短锁统一执行
        let actions = std::mem::take(&mut self.actions);
        if !actions.is_empty() {
            let engine = self.engine.lock().unwrap_or_else(|p| p.into_inner());
            for a in actions {
                match a {
                    ExtAction::StartAll => engine.plugin_start_all(),
                    ExtAction::Start(n) => engine.plugin_start(&n),
                    ExtAction::Stop(n) => engine.plugin_stop(&n),
                    ExtAction::Reload(n) => engine.plugin_reload(&n),
                    ExtAction::Refresh => engine.plugin_refresh(),
                    ExtAction::Remove(n) => {
                        self.pending_remove = None;
                        match engine.remove_plugin(&n) {
                            Ok(()) => {
                                self.status = format!(
                                    "{} {n}",
                                    tr(lang, "已删除插件", "Removed plugin")
                                );
                                self.error = None;
                                self.request_web_restart();
                            }
                            Err(e) => self.error = Some(e),
                        }
                    }
                }
            }
        }
        // 状态/错误
        if let Some(e) = &self.error {
            ui.add_space(2.0);
            ui.colored_label(Theme::err(), format!("⚠ {e}"));
        }
        if !self.status.is_empty() {
            ui.add_space(2.0);
            ui.label(Theme::dim(&self.status));
        }
    }

    // ================= 已安装（管理） =================
    fn ui_installed(
        &mut self,
        ui: &mut egui::Ui,
        lang: Lang,
        snapshot: &[(String, PluginStatus, Vec<String>, String)],
        skills_dir: &std::path::Path,
        domain_of: &std::collections::HashMap<String, String>,
    ) {
        ui.horizontal(|ui| {
            ui.label(Theme::card_section_title(&tr(
                lang,
                "子进程插件",
                "Subprocess plugins",
            )));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if Theme::mini_button(
                    ui,
                    tr(lang, "启动全部", "Start all"),
                    ChipTint::Accent,
                )
                .on_hover_text(tr(
                    lang,
                    "启动所有插件并完成握手（agent 可调用其工具）",
                    "Start all plugins and handshake (agent can call their tools)",
                ))
                .clicked()
                {
                    self.actions.push(ExtAction::StartAll);
                }
            });
        });
        ui.add_space(4.0);
        let snapshot = snapshot.to_vec();
        let empty = snapshot.is_empty();
        ScrollArea::vertical()
            .max_height(ui.available_height() * 0.5)
            .id_salt("ext_installed_scroll")
            .show(ui, |ui| {
                // 确认态只对存在的插件有效（插件消失即取消）
                if let Some(p) = self.pending_remove.clone() {
                    if !snapshot.iter().any(|(n, ..)| n == &p) {
                        self.pending_remove = None;
                    }
                }
                for (name, status, tool_names, desc) in snapshot {
                    Theme::card().show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.label(
                                RichText::new(&name)
                                    .size(13.5)
                                    .strong()
                                    .color(Theme::accent_light()),
                            );
                            let (status_zh, status_en, color) = match &status {
                                PluginStatus::Running => ("运行中", "running", Theme::ok()),
                                PluginStatus::Starting => ("启动中", "starting", Theme::cyan()),
                                PluginStatus::Stopped => ("已停止", "stopped", Theme::text_dim()),
                                PluginStatus::Failed(_) => ("异常", "failed", Theme::err()),
                            };
                            if let Some(dom) = domain_of.get(&name) {
                                ui.label(
                                    RichText::new(format!("◇ {dom}"))
                                        .size(10.0)
                                        .color(Theme::cyan()),
                                )
                                .on_hover_text(tr(
                                    lang,
                                    "领域增强插件（核心层工具提供者，非提示词注入）",
                                    "Domain enhancement plugin (core-level tool provider)",
                                ));
                            }
                            Theme::status_pill(
                                ui,
                                tr(lang, status_zh, status_en),
                                color,
                            );
                            if let PluginStatus::Failed(e) = &status {
                                ui.label(
                                    RichText::new(e.clone())
                                        .size(11.0)
                                        .color(Theme::err()),
                                );
                            }
                            ui.label(
                                RichText::new(format!(
                                    "{}: {}",
                                    tr(lang, "工具", "tools"),
                                    tool_names.len()
                                ))
                                .size(11.0)
                                .color(Theme::text_dim()),
                            );
                            // 工具名列表（hover 显示全名；数量多时截断显示前 6 个）
                            if !tool_names.is_empty() {
                                let names = if tool_names.len() > 6 {
                                    format!(
                                        "{} +{}",
                                        tool_names[..6].join(", "),
                                        tool_names.len() - 6
                                    )
                                } else {
                                    tool_names.join(", ")
                                };
                                ui.label(
                                    RichText::new(names.clone())
                                        .size(11.0)
                                        .color(Theme::text_faint()),
                                )
                                .on_hover_text(if tool_names.len() > 6 {
                                    tool_names.join(", ")
                                } else {
                                    names
                                });
                            }
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                ui.spacing_mut().item_spacing = egui::vec2(4.0, 0.0);
                                if Theme::mini_button(
                                    ui,
                                    tr(lang, "重载", "Reload"),
                                    ChipTint::Neutral,
                                )
                                .clicked()
                                {
                                    self.actions.push(ExtAction::Reload(name.clone()));
                                }
                                if Theme::mini_button(
                                    ui,
                                    tr(lang, "停止", "Stop"),
                                    ChipTint::Neutral,
                                )
                                .clicked()
                                {
                                    self.actions.push(ExtAction::Stop(name.clone()));
                                }
                                if Theme::mini_button(
                                    ui,
                                    tr(lang, "启动", "Start"),
                                    ChipTint::Accent,
                                )
                                .clicked()
                                {
                                    self.actions.push(ExtAction::Start(name.clone()));
                                }
                                // 删除本地插件（两段式确认：先 ✕ 再确认——
                                // 删目录不可逆，防手滑；点其他任意行自动取消）
                                let confirming = self.pending_remove.as_deref() == Some(name.as_str());
                                let label: String = if confirming {
                                    tr(lang, "确认删除?", "sure?").to_string()
                                } else {
                                    "✕".to_string()
                                };
                                if Theme::mini_button(ui, label, ChipTint::Danger)
                                    .on_hover_text(tr(
                                        lang,
                                        "删除本地插件（停止进程并移除插件目录，不可恢复）",
                                        "Delete local plugin (stop process & remove its directory, irreversible)",
                                    ))
                                    .clicked()
                                {
                                    if confirming {
                                        self.actions.push(ExtAction::Remove(name.clone()));
                                    } else {
                                        self.pending_remove = Some(name.clone());
                                    }
                                }
                            });
                        });
                        if !desc.is_empty() {
                            ui.add_space(2.0);
                            ui.label(
                                RichText::new(desc)
                                    .size(11.5)
                                    .color(Theme::text_dim()),
                            );
                        }
                    });
                    ui.add_space(4.0);
                }
                if empty {
                    ui.add_space(16.0);
                    ui.centered_and_justified(|ui| {
                        ui.label(Theme::dim(&tr(
                            lang,
                            "暂无子进程插件。切到「本地安装」导入，或在插件目录下创建 <name>/plugin.json + 可执行程序。",
                            "No subprocess plugins. Import from Local tab, or create <name>/plugin.json + an executable under the plugin dir.",
                        )));
                    });
                }
            });
        // 本地已装 DSH 官方插件（来自 profiles / dsh CLI node_modules）
        ui.add_space(6.0);
        ui.label(
            RichText::new(format!(
                "🧩 {}（{}）",
                tr(
                    lang,
                    "本地已装 DSH 官方插件",
                    "DSH official plugins (local)"
                ),
                self.local_plugins.len()
            ))
            .color(Theme::accent_light())
            .strong(),
        );
        ui.add_space(2.0);
        if self.local_plugins.is_empty() {
            ui.label(Theme::dim(&tr(
                lang,
                "未发现本地安装的 DSH 插件（profiles/node_modules）。切到「远程安装」联网获取。",
                "No locally installed DSH plugins (profiles/node_modules). Use Remote tab to fetch online.",
            )));
        } else {
            ScrollArea::vertical()
                .max_height(120.0)
                .id_salt("local_dsh_plugins")
                .show(ui, |ui| {
                    for name in self.local_plugins.clone() {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new("•").color(Theme::accent_light()));
                            ui.label(RichText::new(&name).size(12.0).color(Theme::text()));
                            // 已转写为技能标记
                            let skill_name = crate::dsh::plugins::plugin_to_skill_name(&name);
                            if skills_dir.join(&skill_name).is_dir() {
                                ui.label(
                                    RichText::new(tr(lang, "✓ 已转写", "✓ transcribed"))
                                        .size(11.0)
                                        .color(Theme::ok()),
                                );
                            }
                        });
                    }
                });
        }
    }

    // ================= 远程安装（联网） =================
    fn ui_remote(&mut self, ui: &mut egui::Ui, lang: Lang, skills_dir: &std::path::Path) {
        ui.horizontal(|ui| {
            ui.label(Theme::card_section_title(&format!(
                "🌐 {}: @deepseek-ai/dsh",
                tr(lang, "来源", "Source")
            )));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let fetch = Theme::primary_button(
                    ui,
                    tr(lang, "联网获取清单", "Fetch list"),
                    !self.busy,
                )
                .on_hover_text(tr(
                    lang,
                    "从 npm registry 读取 @deepseek-ai/dsh 的官方插件清单",
                    "Fetch the official plugin list from npm registry (@deepseek-ai/dsh)",
                ));
                if fetch.clicked() {
                    let cache_file = skills_dir.join(".dsh-plugin-list.json");
                    let engine2 = self.engine.clone();
                    self.run_in_background(move || {
                        let list = crate::dsh::plugins::fetch_dsh_plugin_list()?;
                        let _ = std::fs::write(
                            &cache_file,
                            serde_json::to_string(&list).unwrap_or_else(|_| "[]".into()),
                        );
                        engine2.lock().unwrap().plugin_refresh();
                        Ok(format!(
                            "{} {}",
                            crate::ui::i18n::tr(
                                crate::ui::i18n::Lang::Zh,
                                "已获取官方插件清单",
                                "Fetched official plugin list"
                            ),
                            list.len()
                        ))
                    });
                    self.refresh_local();
                }
            });
        });
        ui.add_space(4.0);
        // 清单已在 refresh_local() 加载（后台获取完成后刷新；不每帧读盘）
        if self.dsh_plugins.is_empty() {
            ui.add_space(16.0);
            ui.centered_and_justified(|ui| {
                ui.label(Theme::dim(&tr(
                    lang,
                    "点击「联网获取清单」从 npm registry 拉取 DSH 官方插件列表。",
                    "Click Fetch list to load DSH official plugins from npm registry.",
                )));
            });
            return;
        }
        ui.label(
            RichText::new(tr(
                lang,
                "点击「下载」：联网下载插件包到本地插件仓库。下载后 AI 即可通过 node_called 直接 require 调用，无需转写为技能。",
                "Click Download: fetch the package into the local plugin repository. After downloading, the agent can require it directly via node_called — no transcription needed.",
            ))
            .size(11.0)
            .color(Theme::text_dim()),
        );
        ui.add_space(2.0);
        let dl_nm = crate::config::AppConfig::load()
            .dsh_home
            .join("plugins-src")
            .join("node_modules");
        let mut to_download: Option<String> = None;
        ScrollArea::vertical()
            .max_height(ui.available_height() * 0.6)
            .id_salt("dsh_plugins_scroll")
            .show(ui, |ui| {
                for (name, version) in self.dsh_plugins.clone() {
                    let base = name.rsplit('/').next().unwrap_or(&name);
                    let downloaded = dl_nm.join("@deepseek-ai").join(base).is_dir();
                    let locally_installed = self.local_plugins.contains(&name);
                    Theme::card().show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.label(
                                RichText::new(&name)
                                    .size(13.0)
                                    .strong()
                                    .color(Theme::accent_light()),
                            );
                            ui.label(
                                RichText::new(version.clone())
                                    .size(11.0)
                                    .color(Theme::text_dim()),
                            );
                            if locally_installed {
                                Theme::status_pill(
                                    ui,
                                    tr(lang, "已安装", "installed"),
                                    Theme::ok(),
                                );
                            } else if downloaded {
                                Theme::status_pill(
                                    ui,
                                    tr(lang, "已下载", "downloaded"),
                                    Theme::ok(),
                                );
                            }
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if !locally_installed
                                        && Theme::mini_button(
                                            ui,
                                            tr(lang, "下载", "Download"),
                                            ChipTint::Accent,
                                        )
                                        .on_hover_text(tr(
                                            lang,
                                            "联网下载插件包（无需转写，AI 可经 node_called 直接调用）",
                                            "Download the package online (no transcription; callable by the agent via node_called)",
                                        ))
                                        .clicked()
                                    {
                                        to_download = Some(name.clone());
                                    }
                                },
                            );
                        });
                    });
                    ui.add_space(4.0);
                }
            });
        if let Some(name) = to_download {
            let cache_root = crate::config::AppConfig::load()
                .dsh_home
                .join("plugins-src");
            let engine2 = self.engine.clone();
            let restart_flag = self.web_restart.clone();
            self.run_in_background(move || {
                let pkg_dir = crate::dsh::plugins::download_dsh_plugin(&name, &cache_root)?;
                // 下载目录进入 NODE_PATH（dsh_node_paths），刷新已装列表即可被 node_called 调用
                engine2.lock().unwrap().plugin_refresh();
                // 插件集变更：请求重启 web（使 cordis 配置生效）
                if let Some(f) = restart_flag {
                    f.store(true, std::sync::atomic::Ordering::Relaxed);
                }
                Ok(format!(
                    "{} {name} → {}",
                    crate::ui::i18n::tr(
                        crate::ui::i18n::Lang::Zh,
                        "已下载（node_called 可直接调用）",
                        "Downloaded (callable via node_called)"
                    ),
                    pkg_dir.display()
                ))
            });
        }
    }

    // ================= 本地安装 =================
    fn ui_local(&mut self, ui: &mut egui::Ui, lang: Lang) {
        ui.label(
            RichText::new(tr(
                lang,
                "从本地目录导入插件。目录内须含 plugin.json（manifest），
                 插件程序任意语言（Python / Node / Rust / 批处理…）。",
                "Import a plugin from a local folder. The folder must contain plugin.json
                 (manifest); the plugin program can be in any language (Python / Node / Rust / batch…).",
            ))
            .size(11.5)
            .color(Theme::text_dim()),
        );
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            if Theme::primary_button(
                ui,
                tr(lang, "📁 选择插件目录导入", "📁 Import plugin folder"),
                true,
            )
            .clicked()
            {
                if let Some(path) = rfd::FileDialog::new()
                    .set_title(tr(
                        lang,
                        "选择插件目录（含 plugin.json）",
                        "Pick plugin folder (with plugin.json)",
                    ))
                    .pick_folder()
                {
                    let engine = self.engine.lock().unwrap_or_else(|p| p.into_inner());
                    match engine.import_plugin_dir(&path) {
                        Ok(name) => {
                            self.status = format!(
                                "{} {name}",
                                tr(lang, "已导入并启动", "Imported & started")
                            );
                            self.error = None;
                            drop(engine);
                            // 插件集变更：请求重启 web（使 cordis 配置生效）
                            self.request_web_restart();
                        }
                        Err(e) => self.error = Some(e),
                    }
                }
            }
            if self.busy {
                ui.spinner();
            }
        });
        ui.add_space(8.0);
        // 格式说明
        Theme::card().show(ui, |ui| {
            ui.label(Theme::card_section_title(&tr(
                lang,
                "plugin.json 格式：",
                "plugin.json format:",
            )));
            ui.add_space(2.0);
            ui.label(
                RichText::new(
                    "{\n  \"name\": \"my-plugin\",\n  \"description\": \"插件描述\",\n  \"command\": [\"python\", \"plugin.py\"],\n  \"tools\": [],\n  \"autostart\": true\n}",
                )
                .monospace()
                .size(11.0)
                .color(Theme::text_dim()),
            );
            ui.add_space(4.0);
            ui.label(
                RichText::new(tr(
                    lang,
                    "协议：收到 initialize 请求回 result 并发 plugin.ready（上报 tools/services）；
                     收到 tool.invoke 按 params.name 执行并回 result（JSON-RPC 2.0，stdin/stdout 行分隔）。",
                    "Protocol: reply to initialize with result and send plugin.ready (tools/services);
                     handle tool.invoke by params.name and reply with result (JSON-RPC 2.0, newline-delimited on stdin/stdout).",
                ))
                .size(11.0)
                .color(Theme::text_dim()),
            );
        });
    }
}
