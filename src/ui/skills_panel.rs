//! 技能面板：三个 Tab —— 已安装（管理）/ 远程安装 / 本地安装。
//!
//! - 已安装：$DSH_HOME/skills 下技能列表（名称/描述/说明/来源），可删除、打开目录
//! - 远程安装：从 GitHub（默认 anthropics/skills）浏览并安装，列表展示 + 已安装标记
//! - 本地安装：选择本地 skill.md / SKILL.md 文件或技能目录导入
//!
//! 模型可在回合内用 load_skill 工具加载技能说明执行。

use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};

use egui::{RichText, ScrollArea};

use crate::core::DshEngine;
use crate::engine::skill::{Skill, DEFAULT_GITHUB_SKILLS_REPO};
use crate::ui::i18n::{tr, Lang};
use crate::ui::theme::{ChipTint, Theme};

/// 技能面板 Tab。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SkillsTab {
    Installed,
    Remote,
    Local,
}

pub struct SkillsPanel {
    engine: Arc<Mutex<DshEngine>>,
    pub lang: Lang,
    skills: Vec<Skill>,
    tab: SkillsTab,
    /// 已安装 Tab：待确认删除的技能（两击）
    pending_delete: Option<String>,
    /// 后台操作 rx（"__DONE__ ok ..." / "__DONE__ err: ..."）
    bg_rx: Option<Receiver<String>>,
    busy: bool,
    status: String,
    error: Option<String>,
}

impl SkillsPanel {
    pub fn new(engine: Arc<Mutex<DshEngine>>, lang: Lang) -> Self {
        let mut panel = Self {
            engine,
            lang,
            skills: Vec::new(),
            tab: SkillsTab::Installed,
            pending_delete: None,
            bg_rx: None,
            busy: false,
            status: String::new(),
            error: None,
        };
        panel.refresh();
        panel
    }

    /// 从引擎重新加载技能列表。
    pub fn refresh(&mut self) {
        self.skills = self.engine.lock().unwrap_or_else(|p| p.into_inner()).skills_snapshot();
    }

    /// 后台操作收尾（drain rx）。
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
                } else if !l.is_empty() {
                    self.status = l;
                }
            }
            if done {
                self.busy = false;
                // 安装/导入/删除完成 → 引擎重载技能（agent 下一回合即可用）
                self.engine.lock().unwrap_or_else(|p| p.into_inner()).reload_skills();
                self.refresh();
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
        let spawn_res = std::thread::Builder::new()
            .name("skill-op".into())
            .spawn(move || {
                let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f))
                    .unwrap_or_else(|_| Err("技能操作线程 panic".into()));
                match res {
                    Ok(msg) => {
                        let _ = tx.send(format!("__DONE__ ok {msg}"));
                    }
                    Err(e) => {
                        let _ = tx.send(format!("__DONE__ err: {e}"));
                    }
                }
            });
        if spawn_res.is_err() {
            // 线程启动失败：立即解除 busy 并提示（不能 .expect panic 掉 UI 线程）
            self.busy = false;
            self.bg_rx = None;
            self.error = Some("后台线程启动失败".into());
        }
    }

    pub fn ui(&mut self, ui: &mut egui::Ui) {
        self.pump_bg();
        let lang = self.lang;
        ui.label(Theme::page_title(tr(lang, "技能", "Skills")));
        ui.add_space(3.0);
        ui.label(
            RichText::new(tr(
                lang,
                "技能是可加载的说明文档：模型在回合内用 load_skill 工具加载后按说明执行；也可手动浏览。",
                "Skills are loadable instruction docs: the model loads them with the load_skill tool during a turn; you can also browse them manually.",
            ))
            .size(11.0)
            .color(Theme::text_dim()),
        );
        ui.add_space(8.0);

        // ===== 目录行 + 全局操作（info 左 + chips 右，与扩展页同构） =====
        let dir = self.engine.lock().unwrap_or_else(|p| p.into_inner()).skills_dir();
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(format!(
                    "📁 {}: {}",
                    tr(lang, "技能目录", "Skills dir"),
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
                    self.refresh();
                }
            });
        });
        ui.add_space(8.0);

        // ===== Tab 行（分段胶囊，与聊天输入框 chips 同风格） =====
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing = egui::vec2(4.0, 0.0);
            for (tab, zh, en) in [
                (SkillsTab::Installed, "已安装", "Installed"),
                (SkillsTab::Remote, "远程安装", "Remote"),
                (SkillsTab::Local, "本地安装", "Local"),
            ] {
                let label = if tab == SkillsTab::Installed {
                    format!("{} ({})", tr(lang, zh, en), self.skills.len())
                } else {
                    tr(lang, zh, en)
                };
                if Theme::segment_tab(ui, &label, self.tab == tab)
                    .on_hover_text(match tab {
                        SkillsTab::Installed => {
                            tr(lang, "管理已安装的技能", "Manage installed skills")
                        }
                        SkillsTab::Remote => {
                            tr(lang, "从 GitHub 浏览并安装", "Browse & install from GitHub")
                        }
                        SkillsTab::Local => {
                            tr(lang, "从本地文件/目录导入", "Import from local file/dir")
                        }
                    })
                    .clicked()
                {
                    self.tab = tab;
                    self.pending_delete = None;
                }
            }
            if self.busy {
                ui.spinner();
            }
        });
        ui.add_space(8.0);

        match self.tab {
            SkillsTab::Installed => self.ui_installed(ui, lang),
            SkillsTab::Remote => self.ui_remote(ui, lang, &dir),
            SkillsTab::Local => self.ui_local(ui, lang),
        }

        // ===== 状态 / 错误 =====
        if let Some(e) = &self.error {
            ui.add_space(4.0);
            ui.colored_label(Theme::err(), format!("⚠ {e}"));
        }
        if !self.status.is_empty() {
            ui.add_space(2.0);
            ui.label(Theme::dim(&self.status));
        }
    }

    // ================= 已安装（管理） =================
    fn ui_installed(&mut self, ui: &mut egui::Ui, lang: Lang) {
        ScrollArea::vertical().show(ui, |ui| {
            let skills = self.skills.clone();
            for s in &skills {
                self.render_installed_card(ui, s, lang);
                ui.add_space(6.0);
            }
            if skills.is_empty() {
                ui.add_space(16.0);
                ui.centered_and_justified(|ui| {
                    ui.label(Theme::dim(&tr(
                        lang,
                        "暂无已安装技能。切到「远程安装」或「本地安装」添加。",
                        "No installed skills. Go to Remote or Local tab to add one.",
                    )));
                });
            }
        });
    }

    fn render_installed_card(&mut self, ui: &mut egui::Ui, skill: &Skill, lang: Lang) {
        Theme::card().show(ui, |ui| {
            // 名称 + 状态 + 删除
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(&skill.name)
                        .size(13.5)
                        .strong()
                        .color(Theme::accent_light()),
                );
                if skill.invocation.model_invocable {
                    Theme::status_pill(
                        ui,
                        tr(lang, "模型可调用", "model-invocable"),
                        Theme::ok(),
                    );
                } else {
                    Theme::status_pill(
                        ui,
                        tr(lang, "仅手动", "manual only"),
                        Theme::text_dim(),
                    );
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let is_confirm = self.pending_delete.as_deref() == Some(skill.name.as_str());
                    let (del_label, tint) = if is_confirm {
                        ("OK", ChipTint::Danger)
                    } else {
                        ("✕", ChipTint::Danger)
                    };
                    if Theme::mini_button(ui, del_label, tint)
                        .on_hover_text(tr(
                            lang,
                            "删除此技能（再次点击确认）",
                            "Delete this skill (click again to confirm)",
                        ))
                        .clicked()
                    {
                        if is_confirm {
                            let name = skill.name.clone();
                            let mut engine = self.engine.lock().unwrap_or_else(|p| p.into_inner());
                            match engine.remove_skill(&name) {
                                Ok(()) => {
                                    self.status =
                                        format!("{} {name}", tr(lang, "已删除", "Removed"));
                                    self.error = None;
                                    drop(engine);
                                    self.refresh();
                                }
                                Err(e) => self.error = Some(e),
                            }
                            self.pending_delete = None;
                        } else {
                            self.pending_delete = Some(skill.name.clone());
                        }
                    }
                });
            });
            // 描述
            if let Some(desc) = &skill.description {
                if !desc.is_empty() {
                    ui.add_space(2.0);
                    ui.label(
                        RichText::new(desc)
                            .size(12.0)
                            .color(Theme::text()),
                    );
                }
            }
            // 说明预览（可读性：截断 + 等宽字体）
            if let Some(ins) = &skill.instructions {
                if !ins.is_empty() {
                    ui.add_space(4.0);
                    let preview: String = ins.chars().take(220).collect();
                    let preview = if ins.chars().count() > 220 {
                        format!("{preview}…")
                    } else {
                        preview
                    };
                    ui.label(
                        RichText::new(preview)
                            .size(11.0)
                            .monospace()
                            .color(Theme::text_dim()),
                    );
                }
            }
        });
    }

    // ================= 远程安装 =================
    fn ui_remote(&mut self, ui: &mut egui::Ui, lang: Lang, dir: &std::path::Path) {
        let (gh_owner, gh_repo, gh_branch) = DEFAULT_GITHUB_SKILLS_REPO;
        ui.horizontal(|ui| {
            ui.label(Theme::card_section_title(&format!(
                "{}: {gh_owner}/{gh_repo}",
                tr(lang, "来源", "Source")
            )));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if Theme::primary_button(
                    ui,
                    tr(lang, "刷新列表", "Refresh list"),
                    !self.busy,
                )
                .clicked()
                {
                    let owner = gh_owner.to_string();
                    let repo = gh_repo.to_string();
                    let branch = gh_branch.to_string();
                    let skill_dir = dir.to_path_buf();
                    self.run_in_background(move || {
                        let names = crate::engine::skill::SkillRegistry::list_remote_skills(
                            &owner, &repo, &branch,
                        )?;
                        // 远程列表写入技能目录隐藏清单，UI 读取展示
                        let list_file = skill_dir.join(".remote-skills.json");
                        let _ = std::fs::write(
                            &list_file,
                            serde_json::to_string(&names).unwrap_or_else(|_| "[]".into()),
                        );
                        Ok(format!(
                            "{}: {}",
                            crate::ui::i18n::tr(
                                crate::ui::i18n::Lang::Zh,
                                "远程技能",
                                "remote skills"
                            ),
                            names.len()
                        ))
                    });
                }
            });
        });
        ui.add_space(4.0);

        // 远程列表（从 .remote-skills.json 读取）
        let remote: Vec<String> = std::fs::read_to_string(dir.join(".remote-skills.json"))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        if remote.is_empty() {
            ui.add_space(16.0);
            ui.centered_and_justified(|ui| {
                ui.label(Theme::dim(&tr(
                    lang,
                    "点击「刷新列表」从 GitHub 拉取可用技能。",
                    "Click Refresh list to fetch available skills from GitHub.",
                )));
            });
            return;
        }
        let installed: Vec<String> = self.skills.iter().map(|s| s.name.clone()).collect();
        ScrollArea::vertical().show(ui, |ui| {
            for name in remote {
                let is_installed = installed.contains(&name);
                Theme::card().show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new(&name)
                                .size(13.0)
                                .strong()
                                .color(Theme::accent_light()),
                        );
                        if is_installed {
                            Theme::status_pill(
                                ui,
                                tr(lang, "已安装", "installed"),
                                Theme::ok(),
                            );
                        }
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if is_installed {
                                ui.label(Theme::dim(&tr(lang, "已就绪", "ready")));
                            } else {
                                if Theme::mini_button(
                                    ui,
                                    tr(lang, "安装", "Install"),
                                    ChipTint::Accent,
                                )
                                .on_hover_text(tr(
                                    lang,
                                    "下载 SKILL.md 到本地技能目录",
                                    "Download SKILL.md into the local skills dir",
                                ))
                                .clicked()
                                {
                                    let name2 = name.clone();
                                    let owner = gh_owner.to_string();
                                    let repo = gh_repo.to_string();
                                    let branch = gh_branch.to_string();
                                    let skill_dir = dir.to_path_buf();
                                    self.run_in_background(move || {
                                        let reg = crate::engine::skill::SkillRegistry::default();
                                        let p = reg.install_from_github(
                                            &skill_dir, &owner, &repo, &branch, &name2,
                                        )?;
                                        Ok(format!(
                                            "{} {name2} → {}",
                                            crate::ui::i18n::tr(
                                                crate::ui::i18n::Lang::Zh,
                                                "已安装",
                                                "installed"
                                            ),
                                            p.display()
                                        ))
                                    });
                                }
                            }
                        });
                    });
                });
                ui.add_space(4.0);
            }
        });
    }

    // ================= 本地安装 =================
    fn ui_local(&mut self, ui: &mut egui::Ui, lang: Lang) {
        // 使用说明（可读性）
        ui.label(
            RichText::new(tr(
                lang,
                "从本地导入技能。支持两种方式：",
                "Import a skill from your local disk. Two ways:",
            ))
            .color(Theme::text()),
        );
        ui.add_space(2.0);
        ui.label(
            RichText::new(tr(
                lang,
                "① 选择 skill.md / SKILL.md 文件（技能名取 front-matter 的 name，或所在目录名）",
                "① Pick a skill.md / SKILL.md file (name from front-matter, or its folder name)",
            ))
            .size(12.0)
            .color(Theme::text_dim()),
        );
        ui.label(
            RichText::new(tr(
                lang,
                "② 选择技能目录（目录内须含 skill.md 或 SKILL.md，目录名即技能名）",
                "② Pick a skill folder (must contain skill.md or SKILL.md; folder name = skill name)",
            ))
            .size(12.0)
            .color(Theme::text_dim()),
        );
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            let pick_file = Theme::primary_button(
                ui,
                tr(lang, "📄 选择文件导入", "📄 Import from file"),
                true,
            )
            .clicked();
            let pick_dir = Theme::mini_button(
                ui,
                tr(lang, "📁 选择目录导入", "📁 Import from folder"),
                ChipTint::Accent,
            )
            .clicked();
            if pick_file {
                if let Some(path) = rfd::FileDialog::new()
                    .add_filter("Skill 文件", &["md"])
                    .set_title(tr(
                        lang,
                        "选择 skill.md / SKILL.md",
                        "Pick skill.md / SKILL.md",
                    ))
                    .pick_file()
                {
                    let mut engine = self.engine.lock().unwrap_or_else(|p| p.into_inner());
                    match engine.import_skill_file(&path) {
                        Ok(name) => {
                            self.status = format!("{} {name}", tr(lang, "已导入", "Imported"));
                            self.error = None;
                            drop(engine);
                            self.refresh();
                        }
                        Err(e) => self.error = Some(e),
                    }
                }
            }
            if pick_dir {
                if let Some(path) = rfd::FileDialog::new()
                    .set_title(tr(lang, "选择技能目录", "Pick skill folder"))
                    .pick_folder()
                {
                    let mut engine = self.engine.lock().unwrap_or_else(|p| p.into_inner());
                    match engine.import_skill_dir(&path) {
                        Ok(name) => {
                            self.status = format!("{} {name}", tr(lang, "已导入", "Imported"));
                            self.error = None;
                            drop(engine);
                            self.refresh();
                        }
                        Err(e) => self.error = Some(e),
                    }
                }
            }
        });
        ui.add_space(8.0);
        // 导入格式说明卡片
        Theme::card().show(ui, |ui| {
            ui.label(Theme::card_section_title(&tr(
                lang,
                "文件格式（front-matter + 说明正文）:",
                "File format (front-matter + instructions):",
            )));
            ui.add_space(2.0);
            ui.label(
                RichText::new("---\nname: my-skill\ndescription: 技能描述\nmodelInvocable: true\n---\n\n技能说明正文…")
                    .monospace()
                    .size(11.0)
                    .color(Theme::text_dim()),
            );
        });
    }
}
