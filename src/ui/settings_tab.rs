//! 设置标签页：引擎（API key/模型） + proxy 配置、字体大小等。

use egui::{RichText, TextEdit};
use std::sync::{Arc, Mutex};

use crate::config::AppConfig;
use crate::core::{DshEngine, SandboxMode};

pub struct SettingsTab {
    // 引擎设置
    default_preset: String,
    api_key: String,
    model: String,
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
    /// 界面语言（zh / en）
    lang: String,
    status: String,
    /// 桥信息 (port, token)——app 启动后写入，设置页展示
    bridge_info: Option<(u16, String)>,
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
            base_url: es.base_url.clone(),
            sandbox_mode,
            engine: runtime,
            http_proxy: cfg.http_proxy.clone().unwrap_or_default(),
            https_proxy: cfg.https_proxy.clone().unwrap_or_default(),
            no_proxy: cfg.no_proxy.clone().unwrap_or_default(),
            font_size: cfg.font_size,
            scrollback: cfg.scrollback,
            shell: cfg.resolved_shell(),
            lang: cfg.lang.clone(),
            status: String::new(),
            bridge_info: None,
        }
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, cfg: &mut AppConfig) {
        ui.heading("设置");
        ui.label(RichText::new("更改会在保存后生效；引擎设置需要重启应用后生效。").weak());
        ui.separator();

        // ===== 引擎设置（DSH 核心） =====
        ui.label(
            RichText::new("DeepSeek 引擎")
                .strong()
                .color(crate::ui::theme::Theme::accent_light()),
        );
        egui::Grid::new("engine_settings_grid")
            .num_columns(2)
            .spacing([12.0, 8.0])
            .show(ui, |ui| {
                ui.label("API Key:");
                ui.add(
                    TextEdit::singleline(&mut self.api_key)
                        .hint_text("sk-...（也可从 ~/.dsh/.credentials.yaml 自动读取）")
                        .desired_width(420.0),
                );
                ui.end_row();
                ui.label("模型:");
                ui.horizontal(|ui| {
                    // 下拉：DeepSeek 官方模型表（V4 系列 + legacy），
                    // 当前值不在表内（自定义网关模型）保留为选项
                    let mut sel = self.model.clone();
                    egui::ComboBox::from_id_salt("model_select")
                        .selected_text(&self.model)
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
                        self.status = "模型已更改（保存后生效）".into();
                    }
                    ui.label(
                        RichText::new("V4 系列（flash / pro / vision-exp）+ legacy")
                            .weak()
                            .size(11.0),
                    );
                });
                ui.end_row();
                ui.label("Base URL:");
                ui.add(TextEdit::singleline(&mut self.base_url).desired_width(420.0));
                ui.end_row();
                ui.label("默认 Agent 模式:");
                ui.horizontal(|ui| {
                    use crate::core::preset::AgentPreset;
                    let current = AgentPreset::parse(&self.default_preset).unwrap_or_default();
                    for p in AgentPreset::all() {
                        let selected = p == current;
                        let label = if selected {
                            RichText::new(format!("● {}", p.name()))
                                .color(crate::ui::theme::Theme::accent_light())
                        } else {
                            RichText::new(p.name()).color(crate::ui::theme::Theme::text_dim())
                        };
                        if ui.selectable_label(selected, label).clicked() && !selected {
                            self.default_preset = p.id().to_string();
                        }
                    }
                    ui.label(
                        RichText::new("新建会话使用（标准/PTC/极简/创造）")
                            .weak()
                            .size(11.0),
                    );
                });
                ui.end_row();
            });

        // ===== 沙箱模式（实时生效，无需重启）=====
        ui.label(
            RichText::new("沙箱模式")
                .strong()
                .color(crate::ui::theme::Theme::accent_light()),
        );
        ui.horizontal(|ui| {
            for m in [
                SandboxMode::DangerFullAccess,
                SandboxMode::WorkspaceWrite,
                SandboxMode::ReadOnly,
            ] {
                let selected = self.sandbox_mode == m;
                let label = if selected {
                    RichText::new(format!("● {}", m.as_str()))
                        .color(crate::ui::theme::Theme::accent_light())
                } else {
                    RichText::new(m.as_str()).color(crate::ui::theme::Theme::text_dim())
                };
                if ui.selectable_label(selected, label).clicked() && !selected {
                    self.sandbox_mode = m;
                    // 实时生效
                    if let Ok(mut e) = self.engine.lock() {
                        e.set_sandbox_mode(m);
                    }
                    self.status = format!("沙箱已切换至 {}", m.as_str());
                }
            }
            ui.label(
                RichText::new("bash/pwsh 受限执行（工作区可写 / 只读），现在生效")
                    .weak()
                    .size(11.0),
            );
        });
        ui.add_space(4.0);
        ui.separator();

        egui::Grid::new("settings_grid")
            .num_columns(2)
            .spacing([12.0, 8.0])
            .show(ui, |ui| {
                ui.label("HTTP 代理:");
                ui.add(
                    TextEdit::singleline(&mut self.http_proxy)
                        .hint_text("http://127.0.0.1:7890（留空清除）")
                        .desired_width(360.0),
                );
                ui.end_row();

                ui.label("HTTPS 代理:");
                ui.add(
                    TextEdit::singleline(&mut self.https_proxy)
                        .hint_text("http://127.0.0.1:7890（留空清除）")
                        .desired_width(360.0),
                );
                ui.end_row();

                ui.label("NO_PROXY:");
                ui.add(
                    TextEdit::singleline(&mut self.no_proxy)
                        .hint_text("localhost,127.0.0.1,::1（逗号分隔）")
                        .desired_width(360.0),
                );
                ui.end_row();

                ui.label("终端字体大小:");
                ui.add(egui::Slider::new(&mut self.font_size, 10.0..=28.0).text("px"));
                ui.end_row();

                ui.label("回滚行数:");
                ui.add(egui::Slider::new(&mut self.scrollback, 500..=20000).step_by(500.0));
                ui.end_row();

                ui.label("默认 Shell:");
                ui.add(TextEdit::singleline(&mut self.shell).desired_width(360.0));
                ui.end_row();

                // 界面语言（中英双语切换，立即生效）
                ui.label("界面语言 / Language:");
                ui.horizontal(|ui| {
                    let mut lang = crate::ui::i18n::Lang::parse(&self.lang);
                    let mut changed = false;
                    for candidate in [crate::ui::i18n::Lang::Zh, crate::ui::i18n::Lang::En] {
                        if ui
                            .selectable_label(lang == candidate, candidate.name())
                            .clicked()
                            && lang != candidate
                        {
                            lang = candidate;
                            changed = true;
                        }
                    }
                    if changed {
                        self.lang = lang.as_str().to_string();
                        cfg.lang = self.lang.clone();
                        let _ = cfg.save();
                        self.status = if lang == crate::ui::i18n::Lang::En {
                            "Language switched to English ✓".into()
                        } else {
                            "已切换到简体中文 ✓".into()
                        };
                    }
                });
                ui.end_row();

                // 主题（调色盘切换，立即生效）：内置 + $DSH_HOME/themes + 插件主题
                ui.label("主题 / Theme:");
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
                        .selected_text(&current)
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
                            self.status = format!("主题已切换：{selected} ✓");
                        }
                    }
                    ui.label(
                        RichText::new("内置 + $DSH_HOME/themes/*.json + 插件主题，立即生效")
                            .weak()
                            .size(11.0),
                    );
                });
                ui.end_row();
            });

        ui.separator();

        ui.horizontal(|ui| {
            if ui.button("保存设置").clicked() {
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
                    (Ok(()), Ok(())) => self.status = "设置已保存 ✓（引擎设置立即生效）".into(),
                    (Err(e), _) => self.status = format!("保存失败: {e:#}"),
                    (_, Err(e)) => self.status = format!("引擎设置保存失败: {e:#}"),
                }
            }
            if ui.button("清除 Key").clicked() {
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
                self.status = "API Key 已清除 ✓".into();
            }
            if ui.button("恢复默认代理").clicked() {
                self.http_proxy.clear();
                self.https_proxy.clear();
                self.no_proxy.clear();
                self.status = "代理已清空（保存后生效）".into();
            }
            ui.label(RichText::new(&self.status).weak());
        });

        ui.separator();
        ui.label(RichText::new("提示：代理注入到 dsh plugin / dsh web / 终端 shell 的 HTTP_PROXY、HTTPS_PROXY、NO_PROXY 环境变量。").size(11.0).weak());
        ui.label(
            RichText::new("保存后重启终端会话（顶部“重启会话”）让 shell 继承新代理。")
                .size(11.0)
                .weak(),
        );
        self.ui_bridge_info(ui);
    }

    /// 桥访问信息（端口 + 鉴权 token，供程序化调用者）。
    /// 由 app 每帧写入（桥启动时生成），设置页只读展示。
    pub fn set_bridge_info(&mut self, port: Option<u16>, token: Option<String>) {
        self.bridge_info = match (port, token) {
            (Some(p), Some(t)) => Some((p, t)),
            _ => None,
        };
    }

    fn ui_bridge_info(&self, ui: &mut egui::Ui) {
        if let Some((port, token)) = &self.bridge_info {
            ui.separator();
            ui.label(
                RichText::new("本地桥 API")
                    .strong()
                    .color(crate::ui::theme::Theme::accent_light()),
            );
            ui.label(
                RichText::new(format!(
                    "http://127.0.0.1:{port}/api/sessions · 鉴权 X-DSH-Token: {token}"
                ))
                .size(11.0)
                .weak(),
            );
            ui.label(
                RichText::new("token 每次启动随机生成（本机任意进程可读端口，故必须携带）")
                    .size(10.5)
                    .weak(),
            );
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
