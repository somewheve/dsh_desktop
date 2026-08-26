//! 对话消息的 Markdown 渲染（+ 内嵌图片）。
//!
//! 解析与渲染分离：`parse_markdown` 是纯函数（可单测），`render_markdown`
//! 把解析结果上屏。支持：标题 / 段落（加粗、斜体、删除线、行内代码、
//! 链接）/ 代码块 / 有序无序列表 / 表格 / 引用 / 分隔线 / 图片。
//!
//! 图片：本地文件直接解码上屏；http(s) URL 由后台线程下载到
//! `$DSH_HOME/cache/chat-imgs/` 后显示。点击图片打开放大查看器。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Receiver;

use egui::text::{LayoutJob, TextFormat};
use egui::{Color32, FontId, Sense, Stroke};
use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};

use crate::ui::theme::Theme;

/// 行内片段（带样式）。
#[derive(Debug, Clone, Default)]
pub struct MdSpan {
    pub text: String,
    pub bold: bool,
    pub italic: bool,
    pub strike: bool,
    pub code: bool,
    pub link: Option<String>,
}

/// 块级元素。
#[derive(Debug, Clone)]
pub enum MdBlock {
    Paragraph(Vec<MdSpan>),
    Heading(u8, Vec<MdSpan>),
    CodeBlock(String),
    List(Vec<Vec<MdSpan>>, bool),
    Quote(Vec<MdSpan>),
    Table {
        head: Vec<Vec<MdSpan>>,
        rows: Vec<Vec<Vec<MdSpan>>>,
    },
    Image {
        src: String,
        alt: String,
    },
    Rule,
}

/// 放大查看器状态。
#[derive(Clone)]
pub struct ViewerState {
    pub tex: egui::TextureHandle,
    pub title: String,
    pub zoom: f32,
    pub offset: egui::Vec2,
}

/// "bold" 字体族是否已绑定（app.rs setup_fonts 注册后置 true）。
/// egui 0.36 对未绑定的 FontFamily::Name 渲染会 panic（fonts.rs:1031），
/// 测试环境与无粗体字体的机器上必须回退到 proportional。
static BOLD_BOUND: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// 标记 bold 族已注册（setup_fonts 成功加载粗体字体后调用）。
pub fn set_bold_bound(v: bool) {
    BOLD_BOUND.store(v, std::sync::atomic::Ordering::Relaxed);
}

/// 加粗字体的 FontId：bold 族未绑定时回退 proportional（避免渲染 panic）。
fn bold_font(size: f32) -> FontId {
    if BOLD_BOUND.load(std::sync::atomic::Ordering::Relaxed) {
        FontId::new(size, egui::FontFamily::Name("bold".into()))
    } else {
        FontId::proportional(size)
    }
}

/// 行内样式位。
#[derive(Debug, Clone, Default)]
struct InlineFlags {
    bold: bool,
    italic: bool,
    strike: bool,
    code: bool,
    /// 链接目标 URL（None = 非链接）
    link: Option<String>,
}

/// 当前行内 token 累积器。
#[derive(Debug, Default)]
struct LineAcc {
    spans: Vec<MdSpan>,
    stack: Vec<InlineFlags>,
}

impl LineAcc {
    fn flags(&self) -> InlineFlags {
        self.stack.last().cloned().unwrap_or_default()
    }

    fn push_text(&mut self, t: &str) {
        if t.is_empty() {
            return;
        }
        let f = self.flags();
        if let Some(last) = self.spans.last_mut() {
            if last.bold == f.bold
                && last.italic == f.italic
                && last.strike == f.strike
                && last.code == f.code
                && last.link == f.link
            {
                last.text.push_str(t);
                return;
            }
        }
        self.spans.push(MdSpan {
            text: t.to_string(),
            bold: f.bold,
            italic: f.italic,
            strike: f.strike,
            code: f.code,
            link: f.link,
        });
    }

    fn push_prefix(&mut self, t: &str) {
        self.spans.push(MdSpan {
            text: t.to_string(),
            ..Default::default()
        });
    }

    fn clear(&mut self) {
        self.spans.clear();
        self.stack.clear();
    }

    fn is_empty(&self) -> bool {
        self.spans.is_empty()
    }
}

/// 解析 Markdown 为块列表（纯函数）。
pub fn parse_markdown(md: &str) -> Vec<MdBlock> {
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    let parser = Parser::new_ext(md, opts);

    let mut blocks: Vec<MdBlock> = Vec::new();
    let mut line = LineAcc::default();
    // 代码块缓冲
    let mut code: Option<String> = None;
    // 列表栈：(ordered, 下一项序号)
    let mut lists: Vec<(bool, u64)> = Vec::new();
    // 当前列表项收集（列表内非空 = 处于列表中）
    let mut list_items: Vec<Vec<MdSpan>> = Vec::new();
    // 表格收集
    let mut table: Option<(bool, Vec<Vec<Vec<MdSpan>>>, Vec<Vec<MdSpan>>)> = None; // (in_head, rows, cur_row)
                                                                                   // 图片
    let mut image: Option<(String, String)> = None; // (src, alt)

    let flush = |blocks: &mut Vec<MdBlock>, line: &mut LineAcc| {
        if !line.is_empty() {
            blocks.push(MdBlock::Paragraph(std::mem::take(&mut line.spans)));
        }
        line.clear();
    };

    for ev in parser {
        match ev {
            Event::Start(Tag::Heading { level, .. }) => {
                flush(&mut blocks, &mut line);
                let lvl = match level {
                    pulldown_cmark::HeadingLevel::H1 => 1,
                    pulldown_cmark::HeadingLevel::H2 => 2,
                    pulldown_cmark::HeadingLevel::H3 => 3,
                    pulldown_cmark::HeadingLevel::H4 => 4,
                    pulldown_cmark::HeadingLevel::H5 => 5,
                    pulldown_cmark::HeadingLevel::H6 => 6,
                };
                blocks.push(MdBlock::Heading(lvl, Vec::new()));
                // 用"临时块"占位：结束后替换
            }
            Event::End(TagEnd::Heading(_)) => {
                if let Some(MdBlock::Heading(_, spans)) = blocks.last_mut() {
                    *spans = std::mem::take(&mut line.spans);
                }
                line.clear();
            }
            Event::Start(Tag::Paragraph) => {}
            Event::End(TagEnd::Paragraph) => {
                flush(&mut blocks, &mut line);
            }
            Event::Start(Tag::CodeBlock(_)) => {
                flush(&mut blocks, &mut line);
                code = Some(String::new());
            }
            Event::End(TagEnd::CodeBlock) => {
                if let Some(c) = code.take() {
                    blocks.push(MdBlock::CodeBlock(c));
                }
            }
            Event::Start(Tag::List(Some(start))) => {
                flush(&mut blocks, &mut line);
                lists.push((true, start));
                list_items = Vec::new();
            }
            Event::Start(Tag::List(None)) => {
                flush(&mut blocks, &mut line);
                lists.push((false, 0));
                list_items = Vec::new();
            }
            Event::End(TagEnd::List(_)) => {
                flush(&mut blocks, &mut line);
                if let Some((ordered, _)) = lists.pop() {
                    if !list_items.is_empty() {
                        blocks.push(MdBlock::List(std::mem::take(&mut list_items), ordered));
                    }
                }
            }
            Event::Start(Tag::Item) => {
                flush(&mut blocks, &mut line);
                let prefix = match lists.last() {
                    Some((true, n)) => {
                        let s = format!("{n}. ");
                        lists.last_mut().unwrap().1 += 1;
                        s
                    }
                    _ => "• ".to_string(),
                };
                line.push_prefix(&prefix);
            }
            Event::End(TagEnd::Item) => {
                if !line.is_empty() {
                    list_items.push(std::mem::take(&mut line.spans));
                }
                line.clear();
            }
            Event::Start(Tag::BlockQuote(_)) => {
                flush(&mut blocks, &mut line);
            }
            Event::End(TagEnd::BlockQuote(_)) => {
                if !line.is_empty() {
                    blocks.push(MdBlock::Quote(std::mem::take(&mut line.spans)));
                    line.clear();
                }
            }
            Event::Start(Tag::Table(_)) => {
                flush(&mut blocks, &mut line);
                table = Some((false, Vec::new(), Vec::new()));
            }
            Event::End(TagEnd::Table) => {
                if let Some((_, mut rows, cur)) = table.take() {
                    if !cur.is_empty() {
                        rows.push(cur);
                    }
                    if rows.is_empty() {
                        continue;
                    }
                    let head = rows.remove(0);
                    blocks.push(MdBlock::Table { head, rows });
                }
            }
            Event::Start(Tag::TableHead) => {
                if let Some(t) = table.as_mut() {
                    t.0 = true;
                    t.2 = Vec::new();
                }
            }
            Event::End(TagEnd::TableHead) => {
                if let Some(t) = table.as_mut() {
                    let cur = std::mem::take(&mut t.2);
                    t.1.push(cur);
                    t.0 = false;
                }
            }
            Event::Start(Tag::TableRow) => {
                if let Some(t) = table.as_mut() {
                    t.2 = Vec::new();
                }
            }
            Event::End(TagEnd::TableRow) => {
                if let Some(t) = table.as_mut() {
                    let cur = std::mem::take(&mut t.2);
                    t.1.push(cur);
                }
            }
            Event::Start(Tag::TableCell) => {
                // 新 cell：清空 line 累积
                if table.is_some() {
                    line.clear();
                }
            }
            Event::End(TagEnd::TableCell) => {
                if let Some(t) = table.as_mut() {
                    t.2.push(std::mem::take(&mut line.spans));
                    line.clear();
                }
            }
            Event::Start(Tag::Image { dest_url, .. }) => {
                flush(&mut blocks, &mut line);
                image = Some((dest_url.into_string(), String::new()));
            }
            Event::End(TagEnd::Image) => {
                if let Some((src, alt)) = image.take() {
                    blocks.push(MdBlock::Image { src, alt });
                }
            }
            Event::Start(Tag::Strong) => line.stack.push(InlineFlags {
                bold: true,
                ..line.flags()
            }),
            Event::End(TagEnd::Strong) => {
                line.stack.pop();
            }
            Event::Start(Tag::Emphasis) => line.stack.push(InlineFlags {
                italic: true,
                ..line.flags()
            }),
            Event::End(TagEnd::Emphasis) => {
                line.stack.pop();
            }
            Event::Start(Tag::Strikethrough) => line.stack.push(InlineFlags {
                strike: true,
                ..line.flags()
            }),
            Event::End(TagEnd::Strikethrough) => {
                line.stack.pop();
            }
            Event::Start(Tag::Link { dest_url, .. }) => {
                let mut f = line.flags();
                f.link = Some(dest_url.into_string());
                line.stack.push(f);
            }
            Event::End(TagEnd::Link) => {
                line.stack.pop();
            }
            Event::Text(t) => {
                if let Some(c) = code.as_mut() {
                    c.push_str(&t);
                } else if let Some((_, alt)) = image.as_mut() {
                    alt.push_str(&t);
                } else {
                    line.push_text(&t);
                }
            }
            Event::Code(t) => {
                // 行内代码：先压 code 标记，再入文本
                line.stack.push(InlineFlags {
                    code: true,
                    ..line.flags()
                });
                line.push_text(&t);
                line.stack.pop();
            }
            Event::SoftBreak | Event::HardBreak => {
                line.push_text("\n");
            }
            Event::Rule => {
                flush(&mut blocks, &mut line);
                blocks.push(MdBlock::Rule);
            }
            Event::Html(_) | Event::InlineHtml(_) => {}
            _ => {}
        }
    }
    flush(&mut blocks, &mut line);
    blocks
}

// ==================== 渲染 ====================

/// 渲染解析后的块到 ui。
/// `max_w`：内容硬上限宽度（消息气泡宽），代码块/表格等必须 wrap 到该宽度，
/// 防止超长行把气泡撑出视口（历史回归：ScrollArea 内容超宽 → 后续消息偏移）。
#[allow(clippy::too_many_arguments)]
pub fn render_markdown(
    ui: &mut egui::Ui,
    md: &str,
    salt: u64,
    text_color: Color32,
    base_dir: Option<&Path>,
    img_cache: &mut HashMap<String, egui::TextureHandle>,
    viewer: &mut Option<ViewerState>,
    http_imgs: &mut Vec<(String, Receiver<Option<String>>)>,
    max_w: f32,
) {
    let blocks = parse_markdown(md);
    render_blocks(
        ui, &blocks, salt, text_color, base_dir, img_cache, viewer, http_imgs, max_w,
    );
}

#[allow(clippy::too_many_arguments)]
pub fn render_blocks(
    ui: &mut egui::Ui,
    blocks: &[MdBlock],
    salt: u64,
    text_color: Color32,
    base_dir: Option<&Path>,
    img_cache: &mut HashMap<String, egui::TextureHandle>,
    viewer: &mut Option<ViewerState>,
    http_imgs: &mut Vec<(String, Receiver<Option<String>>)>,
    max_w: f32,
) {
    for (i, block) in blocks.iter().enumerate() {
        match block {
            MdBlock::Paragraph(spans) => {
                ui.add_space(3.0);
                render_line(ui, spans, 14.0, text_color, max_w);
            }
            MdBlock::Heading(level, spans) => {
                ui.add_space(6.0);
                let size = match level {
                    1 => 19.0,
                    2 => 17.0,
                    3 => 15.5,
                    _ => 14.5,
                };
                render_heading(ui, spans, size, max_w);
                ui.add_space(2.0);
            }
            MdBlock::CodeBlock(code) => {
                ui.add_space(3.0);
                render_code_block(ui, code, max_w);
            }
            MdBlock::List(items, _ordered) => {
                ui.add_space(3.0);
                for item in items {
                    // 任务列表（`- [ ]`/`- [x]`）→ 勾选框；普通项走常规行
                    if !render_checkbox_item(ui, item, 14.0, text_color, max_w) {
                        render_line(ui, item, 14.0, text_color, max_w);
                    }
                    ui.add_space(2.0);
                }
            }
            MdBlock::Quote(spans) => {
                ui.add_space(3.0);
                render_quote(ui, spans, text_color, max_w);
            }
            MdBlock::Table { head, rows } => {
                ui.add_space(4.0);
                render_table(ui, head, rows, salt, text_color, max_w);
                ui.add_space(4.0);
            }
            MdBlock::Image { src, alt } => {
                ui.add_space(4.0);
                render_image(
                    ui, src, alt, base_dir, img_cache, viewer, http_imgs, salt, i, max_w,
                );
            }
            MdBlock::Rule => {
                ui.add_space(6.0);
                ui.separator();
                ui.add_space(4.0);
            }
        }
    }
}

/// 行内片段 → LayoutJob → 上屏。
/// `max_w`：强制 wrap 上限——父布局被超宽内容撑开时 `ui.available_width()`
/// 会失真（可能等于超宽内容宽），导致长行（URL/代码 span）继续撑大气泡。
/// 所有行内渲染必须显式限制 job 宽度。
fn line_to_job(spans: &[MdSpan], size: f32, color: Color32, max_w: f32) -> LayoutJob {
    let mut job = LayoutJob::default();
    job.wrap.max_width = max_w.max(40.0);
    let code_bg = Color32::from_rgba_unmultiplied(90, 95, 110, 60);
    for s in spans {
        let font_id = if s.code {
            FontId::monospace(size - 1.0)
        } else if s.bold {
            bold_font(size)
        } else {
            FontId::proportional(size)
        };
        let mut fmt = TextFormat {
            font_id,
            color: if s.link.is_some() {
                Theme::ACCENT_LIGHT
            } else {
                color
            },
            background: if s.code {
                code_bg
            } else {
                Color32::TRANSPARENT
            },
            italics: s.italic,
            underline: if s.link.is_some() {
                Stroke::new(1.0, Theme::ACCENT)
            } else {
                Stroke::NONE
            },
            strikethrough: if s.strike {
                Stroke::new(1.0, color.gamma_multiply(0.7))
            } else {
                Stroke::NONE
            },
            ..Default::default()
        };
        if s.code {
            fmt.expand_bg = 2.0;
        }
        job.append(&s.text, 0.0, fmt);
    }
    job
}

/// 拼接一段 MdSpan 的纯文本（供前缀检测）。
fn span_text(spans: &[MdSpan]) -> String {
    spans.iter().map(|s| s.text.as_str()).collect()
}

/// 解析任务列表项的勾选框前缀：`[x]`/`[X]` → (true, 剩余)，`[ ]` → (false, 剩余)。
/// 非勾选框项返回 None。用于 `- [ ]`/`- [x]` 计划步骤的渲染。
fn checkbox_state(text: &str) -> Option<(bool, String)> {
    let mut t = text.trim_start();
    // fnv list bullet tolerant
    for sym in ["- ", "* ", "+ ", "• "] {
        if let Some(r) = t.strip_prefix(sym) {
            t = r;
            break;
        }
    }
    let t = t.trim_start();
    let (checked, rest) = if let Some(r) = t.strip_prefix("[x]") {
        (true, r)
    } else if let Some(r) = t.strip_prefix("[X]") {
        (true, r)
    } else if let Some(r) = t.strip_prefix("[ ]") {
        (false, r)
    } else {
        return None;
    };
    Some((checked, rest.trim_start().to_string()))
}

/// 渲染任务列表项（`- [ ]`/`- [x]`）。检测到勾选框前缀时：
/// 画出 ☑/☐ 图标 + 去掉前缀的文本，返回 true；否则返回 false（非勾选框项）。
fn render_checkbox_item(
    ui: &mut egui::Ui,
    item: &[MdSpan],
    size: f32,
    color: Color32,
    max_w: f32,
) -> bool {
    let some = checkbox_state(&span_text(item));
    let Some((checked, rest_text)) = some else {
        return false;
    };
    let mut cloned: Vec<MdSpan> = Vec::new();
    if !rest_text.is_empty() {
        cloned.push(MdSpan {
            text: rest_text,
            bold: false,
            italic: false,
            strike: false,
            code: false,
            link: None,
        });
    }
    ui.horizontal(|ui| {
        if checked {
            ui.label(egui::RichText::new("☑").size(size).color(Theme::OK));
        } else {
            ui.label(egui::RichText::new("☐").size(size).color(Theme::TEXT_DIM));
        }
        ui.add_space(2.0);
        if !cloned.is_empty() {
            render_line(ui, &cloned, size, color, max_w);
        }
    });
    true
}

fn render_line(ui: &mut egui::Ui, spans: &[MdSpan], size: f32, color: Color32, max_w: f32) {
    if spans.is_empty() {
        return;
    }
    let job = line_to_job(spans, size, color, max_w);
    let mut resp = ui.label(job);
    if let Some(url) = spans.iter().find_map(|s| s.link.clone()) {
        resp = resp.on_hover_text(url);
    }
    let _ = resp;
}

fn render_heading(ui: &mut egui::Ui, spans: &[MdSpan], size: f32, max_w: f32) {
    if spans.is_empty() {
        return;
    }
    let mut job = line_to_job(spans, size, Theme::ACCENT_LIGHT, max_w);
    // 标题整行加粗族
    for sec in &mut job.sections {
        sec.format.font_id = bold_font(size);
    }
    ui.label(job);
}

fn render_code_block(ui: &mut egui::Ui, code: &str, max_w: f32) {
    let frame = egui::Frame::default()
        .fill(Theme::BG_HOVER)
        .stroke(egui::Stroke::new(1.0, Theme::BORDER))
        .corner_radius(egui::CornerRadius::same(6))
        .inner_margin(egui::Margin::symmetric(10, 8));
    frame.show(ui, |ui| {
        // 硬上限宽度：父布局被超宽内容撑开时 available_width 会失真，
        // 必须用调用方传入的 max_w 强制 wrap——否则超长代码行会把整个
        // 消息气泡撑到视口外（历史回归：AI 消息气泡宽 1394px、后续消息偏移）。
        ui.set_max_width(max_w.max(40.0));
        ui.label(
            egui::RichText::new(code.trim_end_matches('\n'))
                .monospace()
                .size(12.5)
                .color(Theme::TEXT),
        );
    });
}

fn render_quote(ui: &mut egui::Ui, spans: &[MdSpan], text_color: Color32, max_w: f32) {
    let inner = egui::Frame::default()
        .fill(Theme::BG_ELEVATED)
        .inner_margin(egui::Margin::symmetric(10, 6));
    let fr = inner.show(ui, |ui| {
        ui.set_max_width(max_w.max(40.0));
        render_line(ui, spans, 13.5, text_color.gamma_multiply(0.9), max_w);
    });
    let rect = fr.response.rect;
    ui.painter()
        .vline(rect.left(), rect.y_range(), Stroke::new(3.0, Theme::ACCENT));
}

fn render_table(
    ui: &mut egui::Ui,
    head: &[Vec<MdSpan>],
    rows: &[Vec<Vec<MdSpan>>],
    salt: u64,
    text_color: Color32,
    max_w: f32,
) {
    let cols = head
        .len()
        .max(rows.iter().map(|r| r.len()).max().unwrap_or(0))
        .max(1);
    let mut grid = egui::Grid::new(("md_table", salt))
        .num_columns(cols)
        .striped(true)
        .spacing(egui::vec2(14.0, 4.0))
        .min_col_width(48.0);
    // 表格总宽不得超出内容上限（超宽列撑开气泡 = 历史回归）
    let max_col = ((max_w - 20.0) / cols as f32).max(48.0);
    grid = grid.max_col_width(max_col);
    grid.show(ui, |ui| {
        for cell in head {
            let mut job = line_to_job(cell, 13.0, Theme::ACCENT_LIGHT, max_col);
            for sec in &mut job.sections {
                sec.format.font_id = bold_font(13.0);
            }
            ui.label(job);
        }
        ui.end_row();
        for row in rows {
            for cell in row {
                render_line(ui, cell, 13.0, text_color, max_col);
            }
            ui.end_row();
        }
    });
}

// ==================== 图片 ====================

/// 从文件加载纹理（内存解码，按内容检测格式）。
pub fn load_texture_from_file(ctx: &egui::Context, path: &Path) -> Option<egui::TextureHandle> {
    let bytes = std::fs::read(path).ok()?;
    let img = image::load_from_memory(&bytes).ok()?;
    let rgba = img.to_rgba8();
    let (w, h) = rgba.dimensions();
    if w == 0 || h == 0 || w > 8192 || h > 8192 {
        return None;
    }
    let color = egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], rgba.as_raw());
    Some(ctx.load_texture(path.to_string_lossy(), color, egui::TextureOptions::LINEAR))
}

fn show_image(
    ui: &mut egui::Ui,
    tex: egui::TextureHandle,
    alt: &str,
    title: String,
    viewer: &mut Option<ViewerState>,
    max_w: f32,
) {
    let tex_size = tex.size_vec2();
    // 图片宽度不得超过内容上限（窄窗口下 320px 会把气泡撑出视口）
    let max = egui::vec2(320.0f32.min(max_w.max(60.0)), 240.0);
    let scale = (max.x / tex_size.x.max(1.0))
        .min(max.y / tex_size.y.max(1.0))
        .min(1.0);
    let size = egui::vec2(tex_size.x * scale, tex_size.y * scale);
    let img = egui::Image::new(egui::load::SizedTexture::from_handle(&tex))
        .max_size(size)
        .sense(Sense::click());
    let resp = ui.add(img);
    let _ = resp.clone().on_hover_cursor(egui::CursorIcon::ZoomIn);
    if resp.clicked() {
        *viewer = Some(ViewerState {
            tex,
            title,
            zoom: 1.0,
            offset: egui::Vec2::ZERO,
        });
    }
    if !alt.is_empty() {
        ui.label(egui::RichText::new(alt).size(11.5).color(Theme::TEXT_DIM));
    }
}

#[allow(clippy::too_many_arguments)]
fn render_image(
    ui: &mut egui::Ui,
    src: &str,
    alt: &str,
    base_dir: Option<&Path>,
    img_cache: &mut HashMap<String, egui::TextureHandle>,
    viewer: &mut Option<ViewerState>,
    http_imgs: &mut Vec<(String, Receiver<Option<String>>)>,
    salt: u64,
    idx: usize,
    max_w: f32,
) {
    let _ = (salt, idx);
    let src = src.trim();
    let title = if alt.is_empty() {
        src.to_string()
    } else {
        format!("{alt} — {src}")
    };
    if src.starts_with("http://") || src.starts_with("https://") {
        if let Some(tex) = img_cache.get(src) {
            show_image(ui, tex.clone(), alt, title, viewer, max_w);
        } else {
            if !http_imgs.iter().any(|(u, _)| u == src) {
                http_imgs.push((src.to_string(), spawn_http_image_download(src)));
            }
            ui.label(
                egui::RichText::new(format!("⏳ {alt}（图片下载中…）"))
                    .size(12.0)
                    .color(Theme::TEXT_DIM),
            );
        }
        return;
    }
    // 本地文件
    let mut path = PathBuf::from(src);
    if !path.is_file() {
        if let Some(base) = base_dir {
            let cand = base.join(src);
            if cand.is_file() {
                path = cand;
            }
        }
    }
    if path.is_file() {
        if let Some(tex) = img_cache.get(src).cloned() {
            show_image(ui, tex, alt, title, viewer, max_w);
        } else if let Some(tex) = load_texture_from_file(ui.ctx(), &path) {
            img_cache.insert(src.to_string(), tex.clone());
            show_image(ui, tex, alt, title, viewer, max_w);
        } else {
            ui.label(
                egui::RichText::new(format!("🖼 {alt}（图片解码失败：{src}）"))
                    .size(12.0)
                    .color(Theme::TEXT_DIM),
            );
        }
    } else {
        ui.label(
            egui::RichText::new(format!("🖼 {alt}（找不到图片：{src}）"))
                .size(12.0)
                .color(Theme::TEXT_DIM),
        );
    }
}

/// 后台下载网络图片到缓存目录，返回下载结果（文件路径）。
pub fn spawn_http_image_download(url: &str) -> Receiver<Option<String>> {
    let (tx, rx) = std::sync::mpsc::channel();
    let url = url.to_string();
    std::thread::Builder::new()
        .name("img-dl".into())
        .spawn(move || {
            let out = (|| -> Option<String> {
                let resp = reqwest::blocking::get(&url).ok()?;
                if !resp.status().is_success() {
                    return None;
                }
                let bytes = resp.bytes().ok()?;
                if bytes.is_empty() {
                    return None;
                }
                let dir = crate::config::AppConfig::load()
                    .dsh_home
                    .join("cache")
                    .join("chat-imgs");
                std::fs::create_dir_all(&dir).ok()?;
                let hash = {
                    use std::collections::hash_map::DefaultHasher;
                    use std::hash::{Hash, Hasher};
                    let mut h = DefaultHasher::new();
                    url.hash(&mut h);
                    format!("{:016x}", h.finish())
                };
                let path = dir.join(format!("{hash}.img"));
                std::fs::write(&path, &bytes).ok()?;
                Some(path.to_string_lossy().into_owned())
            })();
            let _ = tx.send(out);
        })
        .ok();
    rx
}

/// 轮询网络图片下载结果：完成的插入缓存。
pub fn pump_http_images(
    ctx: &egui::Context,
    img_cache: &mut HashMap<String, egui::TextureHandle>,
    http_imgs: &mut Vec<(String, Receiver<Option<String>>)>,
) {
    let mut done: Vec<usize> = Vec::new();
    let mut ready: Vec<(String, egui::TextureHandle)> = Vec::new();
    for (i, (url, rx)) in http_imgs.iter_mut().enumerate() {
        if let Ok(Some(path)) = rx.try_recv() {
            if let Some(tex) = load_texture_from_file(ctx, Path::new(&path)) {
                ready.push((url.clone(), tex));
            }
            done.push(i);
        }
    }
    for (url, tex) in ready {
        img_cache.insert(url, tex);
    }
    for i in done.into_iter().rev() {
        http_imgs.remove(i);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_of(spans: &[MdSpan]) -> String {
        spans.iter().map(|s| s.text.clone()).collect()
    }

    #[test]
    fn parse_heading_bold_code() {
        let blocks = parse_markdown("# 标题\n\n**加粗** 和 `code` 文本");
        assert_eq!(blocks.len(), 2);
        match &blocks[0] {
            MdBlock::Heading(1, spans) => assert_eq!(text_of(spans), "标题"),
            other => panic!("expected heading, got {other:?}"),
        }
        match &blocks[1] {
            MdBlock::Paragraph(spans) => {
                assert_eq!(text_of(spans), "加粗 和 code 文本");
                assert!(spans[0].bold, "加粗 span 应带 bold");
                assert!(spans[2].code, "code span 应带 code");
            }
            other => panic!("expected paragraph, got {other:?}"),
        }
    }

    #[test]
    fn parse_list_ordered_unordered() {
        let blocks = parse_markdown("- a\n- b\n\n1. one\n2. two");
        match &blocks[0] {
            MdBlock::List(items, ordered) => {
                assert!(!ordered);
                assert_eq!(items.len(), 2);
                assert!(text_of(&items[0]).starts_with("• "));
            }
            other => panic!("expected list, got {other:?}"),
        }
        match &blocks[1] {
            MdBlock::List(items, ordered) => {
                assert!(ordered);
                assert!(text_of(&items[0]).starts_with("1. "));
                assert!(text_of(&items[1]).starts_with("2. "));
            }
            other => panic!("expected ordered list, got {other:?}"),
        }
    }

    #[test]
    fn parse_code_block() {
        let blocks = parse_markdown("```\nlet x = 1;\n```");
        match &blocks[0] {
            MdBlock::CodeBlock(code) => assert_eq!(code.trim(), "let x = 1;"),
            other => panic!("expected code block, got {other:?}"),
        }
    }

    #[test]
    fn parse_table() {
        let blocks = parse_markdown("| a | b |\n|---|---|\n| 1 | 2 |");
        match &blocks[0] {
            MdBlock::Table { head, rows } => {
                assert_eq!(text_of(&head[0]), "a");
                assert_eq!(text_of(&head[1]), "b");
                assert_eq!(rows.len(), 1);
                assert_eq!(text_of(&rows[0][0]), "1");
                assert_eq!(text_of(&rows[0][1]), "2");
            }
            other => panic!("expected table, got {other:?}"),
        }
    }

    #[test]
    fn parse_image_local_and_http() {
        let blocks = parse_markdown("![示意图](C:/tmp/pic.png)\n\n![网图](https://a.b/c.png)");
        match &blocks[0] {
            MdBlock::Image { src, alt } => {
                assert_eq!(src, "C:/tmp/pic.png");
                assert_eq!(alt, "示意图");
            }
            other => panic!("expected image, got {other:?}"),
        }
        match &blocks[1] {
            MdBlock::Image { src, .. } => assert_eq!(src, "https://a.b/c.png"),
            other => panic!("expected image, got {other:?}"),
        }
    }

    #[test]
    fn parse_plain_text_keeps_newlines() {
        let blocks = parse_markdown("line1\nline2");
        assert_eq!(blocks.len(), 1);
        match &blocks[0] {
            MdBlock::Paragraph(spans) => assert_eq!(text_of(spans), "line1\nline2"),
            other => panic!("expected paragraph, got {other:?}"),
        }
    }

    /// 回归：AI 回复中的"有序列表 + 行内代码 + 加粗"（如 `1. **\`quit\` / \`quit()\`** — xxx`）
    /// 不得被解析成代码块/引用（曾出现渲染为 BG_HOVER/BORDER 纯色块的气泡断裂）。
    #[test]
    fn parse_ordered_list_with_inline_code() {
        let md = "你发了一个 `q`。这个字母有点模糊。\n\n1. **`quit` / `quit()`** — 想结束当前话题或会话\n2. **字母 Q** — 与前面对话相关\n3. **打字不小心按到**\n\n你能补充说明一下吗？";
        let blocks = parse_markdown(md);
        let mut has_code_block = false;
        let mut list_count = 0;
        for b in &blocks {
            match b {
                MdBlock::CodeBlock(_) => has_code_block = true,
                MdBlock::List(items, ordered) => {
                    list_count += 1;
                    assert!(*ordered, "应为有序列表");
                    assert_eq!(items.len(), 3, "3 个列表项");
                }
                _ => {}
            }
        }
        assert!(!has_code_block, "不得出现代码块");
        assert_eq!(list_count, 1, "应恰好一个列表");
    }

    /// 计划步骤的勾选框前缀解析。
    #[test]
    fn checkbox_state_detects_prefix() {
        // 未勾选
        let (checked, rest) = checkbox_state("[ ] 分析需求").expect("未勾选项");
        assert!(!checked);
        assert_eq!(rest, "分析需求");
        // 已勾选（含 X 大小写）
        let (c1, r1) = checkbox_state("[x] 实现功能").expect("已勾选");
        assert!(c1);
        assert_eq!(r1, "实现功能");
        let (c2, _) = checkbox_state("[X] 合并").expect("大写 X");
        assert!(c2);
        // 普通文本非勾选框
        assert!(checkbox_state("普通步骤").is_none());
        assert!(checkbox_state("1. 有序项").is_none());
        // 前导空格容忍
        let (c3, r3) = checkbox_state("  - [ ] 步骤").expect("前导空格");
        assert!(!c3);
        assert_eq!(r3, "步骤");
    }
}
