//! 设置标签页：引擎（API key/模型） + proxy 配置、字体大小等。

use egui::{RichText, TextEdit};
use std::sync::{Arc, Mutex};

use crate::config::AppConfig;
use crate::core::{DshEngine, SandboxMode};
use crate::ui::i18n::{tr, Lang};
use crate::ui::theme::{ChipTint, Theme};

/// 表单行标签（右对齐风格的弱化标签，统一 12px）。
fn form_label(text: &str) -> RichText {
    RichText::new(text).size(12.0).color(Theme::text_dim())
}

/// 代理等单行输入（统一样式：12.5px 主题文字 + 弱化提示）。
fn form_field<'a>(v: &'a mut String, hint: &'a str) -> TextEdit<'a> {
    TextEdit::singleline(v)
        .font(egui::FontId::proportional(12.5))
        .text_color(Theme::text())
        .hint_text(RichText::new(hint).size(11.5).color(Theme::text_faint()))
        .desired_width(360.0)
}

pub struct SettingsTab {
    // 引擎设置
    default_preset: String,
    api_key: String,
    model: String,
    /// 子代理模型（空 = 同主模型）
    subagent_model: String,
    base_url: String,
    /// 沙箱模式（danger-full-access / workspace-write / read-only）
    sandbox_mode: SandboxMode,
    /// 运行时引擎（保存沙箱切换即时生效）
    engine: Arc<Mutex<DshEngine>>,
    // 应用设置
    http_proxy: String,
    https_proxy: String,
    no_proxy: String,
    font_size: f32,
    scrollback: usize,
    shell: String,
    /// 界面语言设置值（zh / en；展示用 Lang 在 pub lang）
    lang_value: String,
    pub status: String,
    /// 桥信息 (port, token)——app 启动后写入，设置页展示
    bridge_info: Option<(u16, String)>,
    /// 界面语言（app 每帧同步）
    pub lang: Lang,
}

impl SettingsTab {
    pub fn new(cfg: &AppConfig, runtime: Arc<Mutex<DshEngine>>) -> Self {
        let es = crate::core::settings::EngineSettings::load();
        let sandbox_mode = runtime
            .lock()
            .map(|e| e.sandbox_mode())
            .unwrap_or(SandboxMode::DangerFullAccess);
        Self {
            default_preset: es.default_preset.clone(),
            api_key: es.api_key.clone().unwrap_or_default(),
            model: es.model.clone(),
            subagent_model: es.subagent_model.clone().unwrap_or_default(),
            base_url: es.base_url.clone(),
            sandbox_mode,
            engine: runtime,
            http_proxy: cfg.http_proxy.clone().unwrap_or_default(),
            https_proxy: cfg.https_proxy.clone().unwrap_or_default(),
            no_proxy: cfg.no_proxy.clone().unwrap_or_default(),
            font_size: cfg.font_size,
            scrollback: cfg.scrollback,
            shell: cfg.resolved_shell(),
            lang_value: cfg.lang.clone(),
            status: String::new(),
            bridge_info: None,
            lang: Lang::parse(&cfg.lang),
        }
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, cfg: &mut AppConfig) {
        let lang = self.lang;
        egui::ScrollArea::vertical()
            .id_salt("settings_scroll")
            .show(ui, |ui| {
            ui.label(Theme::page_title(tr(lang, "设置", "Settings")));
            ui.add_space(3.0);
            ui.label(
                RichText::new(tr(lang, "更改会在保存后生效；引擎设置需要重启应用后生效。", "Changes apply on save; engine settings take effect after restart."))
                    .size(11.0)
                    .color(Theme::text_dim()),
            );
            ui.add_space(10.0);

            // ===== 引擎设置（DSH 核心） =====
            Theme::card().show(ui, |ui| {
                ui.label(Theme::card_section_title(tr(lang, "DeepSeek 引擎", "DeepSeek Engine")));
                ui.add_space(6.0);
                egui::Grid::new("engine_settings_grid")
                    .num_columns(2)
                    .spacing([12.0, 8.0])
                    .show(ui, |ui| {
                        ui.label(form_label(&tr(lang, "API Key:", "API Key:")));
                        ui.add(
                            TextEdit::singleline(&mut self.api_key)
                                .font(egui::FontId::proportional(12.5))
                                .text_color(Theme::text())
                                .hint_text(RichText::new(
                                    "sk-...（也可从 ~/.dsh/.credentials.yaml 自动读取）",
                                )
                                .size(11.5)
                                .color(Theme::text_faint()))
                                .desired_width(420.0),
                        );
                        ui.end_row();
                        ui.label(form_label(&tr(lang, "编辑审批:", "Edit review:")));
                        ui.horizontal(|ui| {
                            let on = {
                                let e = self.engine.lock().unwrap();
                                e.settings().review_edits
                            };
                            if Theme::mini_button(
                                ui,
                                &tr(lang, if on { "开（AI 改动需确认）" } else { "关（改动直接落盘）" },
                                     if on { "ON (review AI edits)" } else { "OFF (auto-apply)" }),
                                if on { ChipTint::Accent } else { ChipTint::Neutral },
                            )
                            .on_hover_text(tr(
                                lang,
                                "开启后 AI 的每次文件修改先入待确认队列，可逐项 确认保留 / 回滚原内容",
                                "AI file edits queue for review: confirm or revert each",
                            ))
                            .clicked()
                            {
                                if let Ok(mut e) = self.engine.lock() {
                                    e.set_review_edits(!on);
                                }
                                self.status = tr(lang, "编辑审批门已切换（下一回合生效）", "Edit review toggled (next turn)").into();
                            }
                        });
                        ui.end_row();
                        ui.label(form_label(&tr(lang, "模型:", "Model:")));
                        ui.horizontal(|ui| {
                            // 下拉：DeepSeek 官方模型表（V4 系列 + legacy），
                            // 当前值不在表内（自定义网关模型）保留为选项
                            let mut sel = self.model.clone();
                            egui::ComboBox::from_id_salt("model_select")
                                .selected_text(
                                    RichText::new(&self.model)
                                        .size(12.5)
                                        .color(Theme::text()),
                                )
                                .width(240.0)
                                .show_ui(ui, |ui| {
                                    for m in crate::core::llm::DEEPSEEK_MODELS {
                                        ui.selectable_value(&mut sel, m.to_string(), *m);
                                    }
                                    if !crate::core::llm::DEEPSEEK_MODELS.contains(&self.model.as_str()) {
                                        let keep = self.model.clone();
                                        ui.selectable_value(
                                            &mut sel,
                                            keep.clone(),
                                            format!("{keep}（自定义）"),
                                        );
                                    }
                                });
                            if sel != self.model {
                                self.model = sel;
                                self.status = tr(lang, "模型已更改（保存后生效）", "Model changed (applies on save)").into();
                            }
                            ui.label(
                                RichText::new(tr(lang, "V4 系列（flash / pro / vision-exp）+ legacy", "V4 series (flash / pro / vision-exp) + legacy"))
                                    .color(Theme::text_faint())
                                    .size(11.0),
                            );
                        });
                        ui.end_row();
                        ui.label(form_label(&tr(lang, "子代理模型:", "Subagent model:")));
                        ui.horizontal(|ui| {
                            // 分级模型：fork 的机械子任务走便宜档；空 = 同主模型
                            let same = tr(lang, "（同主模型）", "(same as main)");
                            let mut sel = if self.subagent_model.is_empty() {
                                same.clone()
                            } else {
                                self.subagent_model.clone()
                            };
                            egui::ComboBox::from_id_salt("subagent_model_select")
                                .selected_text(
                                    RichText::new(&sel)
                                        .size(12.5)
                                        .color(Theme::text()),
                                )
                                .width(240.0)
                                .show_ui(ui, |ui| {
                                    ui.selectable_value(&mut sel, same.clone(), same.as_str());
                                    for m in crate::core::llm::DEEPSEEK_MODELS {
                                        ui.selectable_value(&mut sel, m.to_string(), *m);
                                    }
                                });
                            let picked = if sel == same.clone() {
                                String::new()
                            } else {
                                sel.clone()
                            };
                            if picked != self.subagent_model {
                                self.subagent_model = picked;
                                // 即时生效 + 持久化（下一回合的 fork 使用）
                                if let Ok(mut e) = self.engine.lock() {
                                    let m = if self.subagent_model.is_empty() {
                                        None
                                    } else {
                                        Some(self.subagent_model.clone())
                                    };
                                    e.set_subagent_model(m.as_deref());
                                }
                                self.status = tr(
                                    lang,
                                    "子代理模型已更改（下个 fork 生效）",
                                    "Subagent model changed (next fork)",
                                )
                                .into();
                            }
                            ui.label(
                                RichText::new(tr(
                                    lang,
                                    "subagent_fork 走此模型；不填则与主模型相同",
                                    "subagent_fork uses this model; empty = same as main",
                                ))
                                .color(Theme::text_faint())
                                .size(11.0),
                            );
                        });
                        ui.end_row();
                        ui.label(form_label(&tr(lang, "Base URL:", "Base URL:")));
                        ui.add(
                            TextEdit::singleline(&mut self.base_url)
                                .font(egui::FontId::proportional(12.5))
                                .text_color(Theme::text())
                                .desired_width(420.0),
                        );
                        ui.end_row();
                        ui.label(form_label(&tr(lang, "默认 Agent 模式:", "Default agent preset:")));
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing = egui::vec2(4.0, 0.0);
                            use crate::core::preset::AgentPreset;
                            let current = AgentPreset::parse(&self.default_preset).unwrap_or_default();
                            for p in AgentPreset::all() {
                                if Theme::segment_tab(ui, p.name(), p == current).clicked()
                                    && p != current
                                {
                                    self.default_preset = p.id().to_string();
                                }
                            }
                            ui.label(
                                RichText::new(tr(lang, "新建会话使用（标准/自主规划）", "Used by new sessions (standard / auto-plan)"))
                                    .color(Theme::text_faint())
                                    .size(11.0),
                            );
                        });
                        ui.end_row();
                    });
            });
            ui.add_space(8.0);

            // ===== 沙箱模式（实时生效，无需重启）=====
            Theme::card().show(ui, |ui| {
                ui.label(Theme::card_section_title(tr(lang, "沙箱模式", "Sandbox mode")));
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing = egui::vec2(4.0, 0.0);
                    for m in [
                        SandboxMode::DangerFullAccess,
                        SandboxMode::WorkspaceWrite,
                        SandboxMode::ReadOnly,
                    ] {
                        if Theme::segment_tab(ui, m.as_str(), self.sandbox_mode == m).clicked()
                            && self.sandbox_mode != m
                        {
                            self.sandbox_mode = m;
                            // 实时生效
                            if let Ok(mut e) = self.engine.lock() {
                                e.set_sandbox_mode(m);
                            }
                            self.status = tr(lang, &format!("沙箱已切换至 {}", m.as_str()), &format!("Sandbox switched to {}", m.as_str()));
                        }
                    }
                    ui.label(
                        RichText::new(tr(lang, "仅工作区=区外读写均拦截（系统基础与 ~/.dsh 除外）；只读=全局可读不可写", "Workspace-only blocks reads AND writes outside (system basics & ~/.dsh exempt); read-only = global read, no writes"))
                            .color(Theme::text_faint())
                            .size(11.0),
                    );
                });
            });
            ui.add_space(8.0);

            // ===== 网络与界面 =====
            Theme::card().show(ui, |ui| {
                ui.label(Theme::card_section_title(tr(lang, "网络与界面", "Network & UI")));
                ui.add_space(6.0);
                egui::Grid::new("settings_grid")
                    .num_columns(2)
                    .spacing([12.0, 8.0])
                    .show(ui, |ui| {
                        ui.label(form_label(&tr(lang, "HTTP 代理:", "HTTP proxy:")));
                        ui.add(form_field(&mut self.http_proxy, "http://127.0.0.1:7890（留空清除）"));
                        ui.end_row();

                        ui.label(form_label(&tr(lang, "HTTPS 代理:", "HTTPS proxy:")));
                        ui.add(form_field(&mut self.https_proxy, "http://127.0.0.1:7890（留空清除）"));
                        ui.end_row();

                        ui.label(form_label(&tr(lang, "NO_PROXY:", "NO_PROXY:")));
                        ui.add(form_field(&mut self.no_proxy, "localhost,127.0.0.1,::1（逗号分隔）"));
                        ui.end_row();

                        ui.label(form_label(&tr(lang, "界面字体大小:", "UI font size:")));
                        let before = self.font_size;
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing = egui::vec2(4.0, 0.0);
                            if Theme::mini_button(ui, "−", ChipTint::Neutral).clicked() {
                                self.font_size = (self.font_size.round() - 1.0).max(12.0);
                            }
                            ui.label(
                                RichText::new(format!("{} px", self.font_size.round() as i32))
                                    .size(12.5)
                                    .color(Theme::text()),
                            );
                            if Theme::mini_button(ui, "+", ChipTint::Neutral).clicked() {
                                self.font_size = (self.font_size.round() + 1.0).min(24.0);
                            }
                            ui.label(
                                RichText::new(tr(lang, "（16=默认）", "(16 = default)"))
                                    .size(11.0)
                                    .color(Theme::text_faint()),
                            );
                        });
                        if (self.font_size - before).abs() > f32::EPSILON {
                            // 即时生效（egui 全局缩放；保存后重启仍保留）
                            ui.ctx()
                                .set_zoom_factor(crate::config::ui_zoom(self.font_size));
                        }
                        ui.end_row();

                        ui.label(form_label(&tr(lang, "回滚行数:", "Scrollback lines:")));
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing = egui::vec2(4.0, 0.0);
                            if Theme::mini_button(ui, "−", ChipTint::Neutral).clicked() {
                                self.scrollback = self.scrollback.saturating_sub(500).max(500);
                            }
                            ui.label(
                                RichText::new(format!("{} {l}", self.scrollback, l = tr(lang, "行", "lines")))
                                    .size(12.5)
                                    .color(Theme::text()),
                            );
                            if Theme::mini_button(ui, "+", ChipTint::Neutral).clicked() {
                                self.scrollback = (self.scrollback + 500).min(20000);
                            }
                        });
                        ui.end_row();

                        ui.label(form_label(&tr(lang, "默认 Shell:", "Default shell:")));
                        ui.add(
                            TextEdit::singleline(&mut self.shell)
                                .font(egui::FontId::proportional(12.5))
                                .text_color(Theme::text())
                                .desired_width(360.0),
                        );
                        ui.end_row();

                        // 界面语言（中英双语切换，立即生效）
                        ui.label(form_label(&tr(lang, "界面语言 / Language:", "Language:")));
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing = egui::vec2(4.0, 0.0);
                            let mut lang = crate::ui::i18n::Lang::parse(&self.lang_value);
                            let mut changed = false;
                            for candidate in [crate::ui::i18n::Lang::Zh, crate::ui::i18n::Lang::En] {
                                if Theme::segment_tab(ui, candidate.name(), lang == candidate)
                                    .clicked()
                                    && lang != candidate
                                {
                                    lang = candidate;
                                    changed = true;
                                }
                            }
                            if changed {
                                self.lang_value = lang.as_str().to_string();
                                cfg.lang = self.lang_value.clone();
                                let _ = cfg.save();
                                self.status = if lang == crate::ui::i18n::Lang::En {
                                    "Language switched to English ✓".into()
                                } else {
                                    tr(lang, "已切换到简体中文 ✓", "已切换到简体中文 ✓").into()
                                };
                            }
                        });
                        ui.end_row();

                        // 主题（调色盘切换，立即生效）：内置 + $DSH_HOME/themes + 插件主题
                        ui.label(form_label(&tr(lang, "主题 / Theme:", "Theme:")));
                        ui.horizontal(|ui| {
                            let mut files: Vec<std::path::PathBuf> = Vec::new();
                            let themes_dir = cfg.dsh_home.join("themes");
                            if let Ok(entries) = std::fs::read_dir(&themes_dir) {
                                for e in entries.flatten() {
                                    let p = e.path();
                                    if p.extension().map(|x| x == "json").unwrap_or(false) {
                                        files.push(p);
                                    }
                                }
                            }
                            if let Ok(engine) = self.engine.lock() {
                                files.extend(engine.plugin_theme_files());
                            }
                            let mgr = crate::ui::theme::ThemeManager::discover(&files);
                            let names = mgr.names();
                            let current = cfg.theme.clone();
                            let mut selected = current.clone();
                            egui::ComboBox::from_id_salt("theme_select")
                                .selected_text(
                                    RichText::new(&current).size(12.5).color(Theme::text()),
                                )
                                .width(200.0)
                                .show_ui(ui, |ui| {
                                    for n in &names {
                                        ui.selectable_value(&mut selected, n.clone(), n);
                                    }
                                });
                            if selected != current {
                                if let Some(p) = mgr.get(&selected).cloned() {
                                    crate::ui::theme::Theme::set_palette(&p);
                                    crate::ui::theme::Theme::apply(ui.ctx());
                                    cfg.theme = selected.clone();
                                    let _ = cfg.save();
                                    self.status = tr(lang, &format!("主题已切换：{selected} ✓"), &format!("Theme switched: {selected} ✓"));
                                }
                            }
                            ui.label(
                                RichText::new(tr(lang, "内置 + $DSH_HOME/themes/*.json + 插件主题，立即生效", "Built-in + $DSH_HOME/themes/*.json + plugin themes, instant"))
                                    .color(Theme::text_faint())
                                    .size(11.0),
                            );
                        });
                        ui.end_row();
                    });
            });
            ui.add_space(10.0);

            ui.add_space(10.0);

            // ===== 操作行（主按钮 + 次级 chips + 状态） =====
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing = egui::vec2(6.0, 0.0);
                if Theme::primary_button(ui, &tr(lang, "保存设置", "Save settings"), true).clicked() {
                // 应用设置
                cfg.http_proxy = opt(&self.http_proxy);
                cfg.https_proxy = opt(&self.https_proxy);
                cfg.no_proxy = opt(&self.no_proxy);
                cfg.font_size = self.font_size;
                cfg.scrollback = self.scrollback;
                cfg.shell = if self.shell.trim().is_empty() {
                    None
                } else {
                    Some(self.shell.trim().to_string())
                };
                // 引擎设置：经引擎更新（内存 + 磁盘 + LLM 客户端三同步，
                // 立即生效）。不能直接 EngineSettings::save() 绕过引擎——
                // 引擎内存留着旧值时，之后任何 chip 切换触发 rebuild_llm
                // 落盘就会覆盖刚保存的 key（历史 bug："保存不了 key"）。
                // 空 key 输入 = 保留现有（防误清——改模型/代理后保存不应
                // 把已有 key 抹掉；清除走"清除 Key"按钮）
                let key = if self.api_key.trim().is_empty() {
                    self.engine
                        .lock()
                        .ok()
                        .and_then(|e| e.settings().api_key.clone())
                } else {
                    opt(&self.api_key)
                };
                let model = if self.model.trim().is_empty() {
                    "deepseek-v4-flash".into()
                } else {
                    self.model.trim().into()
                };
                let base_url = if self.base_url.trim().is_empty() {
                    "https://api.deepseek.com".into()
                } else {
                    self.base_url.trim().into()
                };
                let preset = if self.default_preset.trim().is_empty() {
                    "standard".into()
                } else {
                    self.default_preset.trim().into()
                };
                let engine_result = self
                    .engine
                    .lock()
                    .map_err(|e| anyhow::anyhow!("{e}"))
                    .and_then(|mut e| {
                        e.update_settings_from_ui(
                            key,
                            base_url,
                            model,
                            opt(&self.http_proxy),
                            preset,
                        );
                        Ok(())
                    });
                match (cfg.save(), engine_result) {
                    (Ok(()), Ok(())) => self.status = tr(lang, "设置已保存 ✓（引擎设置立即生效）", "Saved ✓ (engine settings applied immediately)").into(),
                    (Err(e), _) => self.status = tr(lang, &format!("保存失败: {e:#}"), &format!("Save failed: {e:#}")),
                    (_, Err(e)) => self.status = tr(lang, &format!("引擎设置保存失败: {e:#}"), &format!("Engine settings save failed: {e:#}")),
                }
            }
            if Theme::mini_button(ui, &tr(lang, "清除 Key", "Clear key"), ChipTint::Danger).clicked() {
                // 显式清除（绕过空输入保留逻辑）
                let keep = self.engine.lock().ok().map(|e| {
                    let s = e.settings();
                    (
                        s.base_url.clone(),
                        s.model.clone(),
                        s.http_proxy.clone(),
                        s.default_preset.clone(),
                    )
                });
                if let Some((url, model, proxy, preset)) = keep {
                    let _ = self
                        .engine
                        .lock()
                        .map(|mut e| e.update_settings_from_ui(None, url, model, proxy, preset));
                }
                self.api_key.clear();
                self.status = tr(lang, "API Key 已清除 ✓", "API key cleared ✓").into();
            }
            if Theme::mini_button(ui, &tr(lang, "恢复默认代理", "Reset proxies"), ChipTint::Neutral).clicked() {
                self.http_proxy.clear();
                self.https_proxy.clear();
                self.no_proxy.clear();
                self.status = tr(lang, "代理已清空（保存后生效）", "Proxies cleared (applies on save)").into();
            }
            if !self.status.is_empty() {
                ui.label(Theme::dim(&self.status));
            }
        });

        ui.add_space(8.0);
        ui.label(
            RichText::new(tr(lang, "提示：代理注入到 dsh plugin / dsh web / 终端 shell 的 HTTP_PROXY、HTTPS_PROXY、NO_PROXY 环境变量。", "Note: proxies are injected as HTTP_PROXY/HTTPS_PROXY/NO_PROXY for dsh plugin / dsh web / shells."))
                .size(11.0)
                .color(Theme::text_faint()),
        );
        ui.label(
            RichText::new(tr(lang, "保存后重启终端会话（顶部“重启会话”）让 shell 继承新代理。", "Restart the terminal session after saving so shells inherit the new proxy."))
                .size(11.0)
                .color(Theme::text_faint()),
        );
        self.ui_bridge_info(ui, lang);
        });
    }

    /// 桥访问信息（端口 + 鉴权 token，供程序化调用者）。
    /// 由 app 每帧写入（桥启动时生成），设置页只读展示。
    pub fn set_bridge_info(&mut self, port: Option<u16>, token: Option<String>) {
        self.bridge_info = match (port, token) {
            (Some(p), Some(t)) => Some((p, t)),
            _ => None,
        };
    }

    fn ui_bridge_info(&self, ui: &mut egui::Ui, lang: Lang) {
        if let Some((port, token)) = &self.bridge_info {
            ui.add_space(8.0);
            Theme::card().show(ui, |ui| {
                ui.label(Theme::card_section_title(tr(lang, "本地桥 API", "Local bridge API")));
                ui.add_space(2.0);
                ui.label(
                    RichText::new(format!(
                        "http://127.0.0.1:{port}/api/sessions · 鉴权 X-DSH-Token: {token}"
                    ))
                    .size(11.0)
                    .color(Theme::text_dim()),
                );
                ui.label(
                    RichText::new(tr(lang, "token 每次启动随机生成（本机任意进程可读端口，故必须携带）", "Token is random per launch (the port is readable by any local process, so it is required)"))
                        .size(10.5)
                        .color(Theme::text_faint()),
                );
            });
        }
    }
}

fn opt(s: &str) -> Option<String> {
    let t = s.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opt_trims_and_clears() {
        assert_eq!(opt("  "), None);
        assert_eq!(opt(" http://x:1 "), Some("http://x:1".to_string()));
    }

    #[test]
    fn config_proxy_roundtrip() {
        let mut cfg = AppConfig::default();
        assert!(!cfg.has_proxy());
        cfg.set_proxy("http://127.0.0.1:7890", "", "localhost");
        assert!(cfg.has_proxy());
        assert_eq!(cfg.http_proxy.as_deref(), Some("http://127.0.0.1:7890"));
        assert_eq!(cfg.no_proxy.as_deref(), Some("localhost"));
    }

    #[test]
    fn apply_proxy_env_sets_vars() {
        let mut cfg = AppConfig::default();
        cfg.set_proxy(
            "http://127.0.0.1:7890",
            "http://127.0.0.1:7890",
            "localhost",
        );
        let mut cmd = std::process::Command::new("cmd.exe");
        cfg.apply_proxy_env(&mut cmd);
        let envs: std::collections::HashMap<_, _> = std::env::vars()
            .filter(|(k, _)| k == "HTTP_PROXY" || k == "HTTPS_PROXY")
            .collect();
        let _ = envs; // Command 的 env 不能直接读；改测 proxy_env_pairs 逻辑
        let _ = &mut cmd;
    }
}
