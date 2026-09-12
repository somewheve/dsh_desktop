//! 内置代码查看器：从 AI 消息的文件链接打开（ZCode 式跳转）。
//!
//! 无文件树——入口是聊天卡片里的路径点击；多文件以页签切换，
//! 只读（写操作归 AI，走审批/沙箱）。

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use egui::RichText;

use crate::core::DshEngine;
use crate::ui::i18n::{tr, Lang};
use crate::ui::theme::Theme;

/// 展示上限（行 / 字节）：防巨文件把 UI 拖死。
const MAX_LINES: usize = 8_000;
const MAX_BYTES: u64 = 2 * 1024 * 1024;

/// 文件视图（打开时同步读取；错误/二进制/超大都有明确形态）。
pub enum FileView {
    Text { path: PathBuf, lines: Vec<String>, truncated: bool },
    Binary { path: PathBuf, size: u64 },
    TooLarge { path: PathBuf, size: u64 },
    Error { path: PathBuf, msg: String },
}

/// 读取文件为视图（纯函数便于测试）。
pub fn read_file_view(path: &Path) -> FileView {
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(e) => {
            return FileView::Error { path: path.to_path_buf(), msg: e.to_string() };
        }
    };
    if meta.len() > MAX_BYTES {
        return FileView::TooLarge { path: path.to_path_buf(), size: meta.len() };
    }
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) => {
            return FileView::Error { path: path.to_path_buf(), msg: e.to_string() };
        }
    };
    // 二进制探测：前 8KB 含 NUL 即视为二进制
    if bytes[..bytes.len().min(8192)].contains(&0) {
        return FileView::Binary { path: path.to_path_buf(), size: meta.len() };
    }
    let text = String::from_utf8_lossy(&bytes).into_owned();
    let total = text.lines().count();
    let truncated = total > MAX_LINES;
    let lines: Vec<String> = if truncated {
        text.lines().take(MAX_LINES).map(String::from).collect()
    } else {
        text.lines().map(String::from).collect()
    };
    FileView::Text { path: path.to_path_buf(), lines, truncated }
}

pub struct CodeBrowser {
    engine: Arc<Mutex<DshEngine>>,
    pub lang: Lang,
    /// 打开的文件页签（按打开顺序）
    tabs: Vec<PathBuf>,
    /// 当前激活页签下标
    active: usize,
    view: Option<FileView>,
}

impl CodeBrowser {
    pub fn new(engine: Arc<Mutex<DshEngine>>, lang: Lang) -> Self {
        Self { engine, lang, tabs: Vec::new(), active: 0, view: None }
    }

    fn ws_root(&self) -> Option<PathBuf> {
        self.engine.lock().ok().and_then(|e| e.workspace_root().cloned())
    }

    /// 打开文件（外部入口：AI 消息路径点击跳转）。相对路径按工作区根
    /// 解析；已打开的文件只激活页签，不重复开。
    pub fn open_file(&mut self, path: &Path) {
        let abs = if path.is_absolute() {
            path.to_path_buf()
        } else {
            match self.ws_root() {
                Some(root) => root.join(path),
                None => path.to_path_buf(),
            }
        };
        if let Some(idx) = self.tabs.iter().position(|p| p == &abs) {
            self.active = idx;
        } else {
            self.tabs.push(abs.clone());
            self.active = self.tabs.len() - 1;
        }
        self.view = Some(read_file_view(&abs));
    }

    fn close_tab(&mut self, idx: usize) {
        if idx >= self.tabs.len() {
            return;
        }
        self.tabs.remove(idx);
        self.active = self.active.min(self.tabs.len().saturating_sub(1));
        self.view = self.tabs.get(self.active).map(|p| read_file_view(p));
    }

    pub fn ui(&mut self, ui: &mut egui::Ui) {
        let lang = self.lang;
        ui.label(Theme::page_title(tr(lang, "代码", "Code")));
        ui.add_space(4.0);
        if self.tabs.is_empty() {
            ui.add_space(24.0);
            ui.centered_and_justified(|ui| {
                ui.label(Theme::dim(&tr(
                    lang,
                    "点击会话里 AI 消息的文件路径（📄 / ✎ 开头的链接）即可在此打开查看",
                    "Click a file path in AI messages (📄 / ✎ links) to open it here",
                )));
            });
            return;
        }
        // 页签栏：文件名 chip（激活亮底；点主体切换、点尾部 ✕ 关闭）
        let mut close: Option<usize> = None;
        let mut activate: Option<usize> = None;
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing = egui::vec2(4.0, 0.0);
            for (i, tab) in self.tabs.iter().enumerate() {
                let name = tab
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| tab.display().to_string());
                let is_active = i == self.active;
                let label = format!("{name} ✕");
                let galley = ui.fonts_mut(|f| {
                    f.layout_no_wrap(
                        label.clone(),
                        egui::FontId::proportional(11.0),
                        if is_active { Theme::text() } else { Theme::text_faint() },
                    )
                });
                let (rect, resp) = ui.allocate_exact_size(
                    egui::vec2(galley.size().x + 18.0, 24.0),
                    egui::Sense::click(),
                );
                if is_active {
                    ui.painter().rect(
                        rect,
                        7.0,
                        Theme::bg_hover(),
                        egui::Stroke::new(1.0, Theme::border()),
                        egui::StrokeKind::Inside,
                    );
                } else if resp.hovered() {
                    ui.painter().rect_filled(
                        rect,
                        7.0,
                        Theme::bg_hover().gamma_multiply(0.5),
                    );
                }
                ui.painter().galley(
                    egui::pos2(
                        rect.left() + 8.0,
                        rect.center().y - galley.mesh_bounds.height() / 2.0 - galley.mesh_bounds.min.y,
                    ),
                    galley,
                    egui::Color32::WHITE,
                );
                let a11y = label.clone();
                resp.widget_info(move || {
                    egui::WidgetInfo::labeled(
                        egui::WidgetType::Button,
                        true,
                        a11y.clone(),
                    )
                });
                let close_zone = rect.right() - 22.0;
                let in_close = resp
                    .clone()
                    .interact_pointer_pos()
                    .map(|p| p.x > close_zone)
                    .unwrap_or(false);
                if resp.clicked() {
                    if in_close {
                        close = Some(i);
                    } else {
                        activate = Some(i);
                    }
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if Theme::mini_button(ui, &tr(lang, "⟳", "⟳"), crate::ui::theme::ChipTint::Neutral)
                    .on_hover_text(tr(lang, "重新读取", "Reload"))
                    .clicked()
                {
                    self.view = self.tabs.get(self.active).map(|p| read_file_view(p));
                }
            });
        });
        if let Some(i) = activate {
            self.active = i;
            self.view = self.tabs.get(i).map(|p| read_file_view(p));
        }
        if let Some(i) = close {
            self.close_tab(i);
        }
        ui.add_space(6.0);
        // 内容
        egui::Frame::default()
            .fill(Theme::bg_elevated())
            .stroke(egui::Stroke::new(1.0, Theme::border()))
            .corner_radius(egui::CornerRadius::same(8))
            .inner_margin(egui::Margin::symmetric(10, 8))
            .show(ui, |ui| {
                ui.set_min_size(egui::vec2(
                    ui.available_width().max(80.0),
                    ui.available_height(),
                ));
                self.render_view(ui);
            });
    }

    /// 内容视图（行号 + 等宽；二进制/超大/错误有明确提示）。
    fn render_view(&mut self, ui: &mut egui::Ui) {
        let lang = self.lang;
        let Some(view) = &self.view else { return };
        match view {
            FileView::Text { path, lines, truncated } => {
                ui.label(
                    RichText::new(format!(
                        "📄 {}  ·  {} {}",
                        path.display(),
                        lines.len(),
                        tr(lang, "行", "lines")
                    ))
                    .size(10.5)
                    .color(Theme::accent_light()),
                );
                ui.add_space(4.0);
                egui::ScrollArea::vertical()
                    .id_salt("code_view_scroll")
                    .show(ui, |ui| {
                        ui.spacing_mut().item_spacing.y = 0.0;
                        for (i, line) in lines.iter().enumerate() {
                            ui.horizontal(|ui| {
                                ui.label(
                                    RichText::new(format!("{:>5}", i + 1))
                                        .monospace()
                                        .size(10.5)
                                        .color(Theme::text_faint().gamma_multiply(0.6)),
                                );
                                ui.add_space(6.0);
                                ui.label(
                                    RichText::new(line)
                                        .monospace()
                                        .size(10.5)
                                        .color(Theme::text()),
                                );
                            });
                        }
                    });
                if *truncated {
                    ui.label(Theme::dim(&format!(
                        "… {} {} ({MAX_LINES})",
                        tr(lang, "仅显示前", "showing first"),
                        tr(lang, "行", "lines")
                    )));
                }
            }
            FileView::Binary { path, size } => {
                ui.label(
                    RichText::new(format!("⚙ {}（{} KB）", path.display(), size / 1024))
                        .size(11.5)
                        .color(Theme::text_dim()),
                );
                ui.label(Theme::dim(&tr(
                    lang,
                    "二进制文件，不展示内容",
                    "Binary file, not displayed",
                )));
            }
            FileView::TooLarge { path, size } => {
                ui.label(
                    RichText::new(format!("📄 {}（{} KB）", path.display(), size / 1024))
                        .size(11.5)
                        .color(Theme::text_dim()),
                );
                ui.label(Theme::dim(&tr(
                    lang,
                    "文件超过 2MB，不展示（可用 AI 的 read_file 分段读）",
                    "File over 2MB (use the AI read_file tool for partial reads)",
                )));
            }
            FileView::Error { path, msg } => {
                ui.label(
                    RichText::new(format!("⚠ {}", path.display()))
                        .size(11.5)
                        .color(Theme::err()),
                );
                ui.label(Theme::dim(msg));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 视图读取：文本（截断标记）/ 二进制 / 超大 / 错误。
    #[test]
    fn file_view_shapes() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("a.txt");
        std::fs::write(&f, "line1\nline2\n").unwrap();
        match read_file_view(&f) {
            FileView::Text { lines, truncated, .. } => {
                assert_eq!(lines.len(), 2);
                assert!(!truncated);
            }
            _ => panic!("应为文本视图"),
        }
        let b = dir.path().join("b.bin");
        std::fs::write(&b, [0u8, 1, 2, 0, 3]).unwrap();
        assert!(matches!(read_file_view(&b), FileView::Binary { .. }));
        let big = dir.path().join("c.log");
        std::fs::write(&big, vec![b'x'; (MAX_BYTES + 1) as usize]).unwrap();
        assert!(matches!(read_file_view(&big), FileView::TooLarge { .. }));
        assert!(matches!(
            read_file_view(&dir.path().join("nope")),
            FileView::Error { .. }
        ));
        let many = dir.path().join("d.txt");
        let text: String = (0..MAX_LINES + 10).map(|i| format!("l{i}\n")).collect();
        std::fs::write(&many, text).unwrap();
        match read_file_view(&many) {
            FileView::Text { lines, truncated, .. } => {
                assert!(truncated);
                assert_eq!(lines.len(), MAX_LINES);
            }
            _ => panic!("应为文本视图"),
        }
    }
}
