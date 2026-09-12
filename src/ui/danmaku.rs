//! 弹幕渲染层：AI 思考过程（reasoning）以弹幕形式从右向左飘过。
//!
//! 弹幕生命周期：出生（右侧外）→ 匀速左移 → 移出左侧销毁。
//! 轨道分配：多条弹幕并排，避免重叠。
//! 弹幕按种类区分气泡颜色与形状：思考（蓝紫胶囊）、工具调用（金色圆角）、
//! 工具结果（绿/红方角）；鼠标悬浮显示全量内容。

use egui::{pos2, Color32, FontId, Pos2, Rect, Vec2};
use std::collections::VecDeque;

/// 弹幕种类（决定气泡颜色/形状）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DanmakuKind {
    /// AI 思考过程：蓝紫胶囊气泡
    Thinking,
    /// 工具调用：金色圆角卡片
    ToolCall,
    /// 工具结果（成功）：绿色方角卡片
    ToolResultOk,
    /// 工具结果（失败）：红色方角卡片
    ToolResultErr,
}

impl DanmakuKind {
    /// 气泡背景色（半透明，适配深色主题）。
    fn bg(self) -> Color32 {
        match self {
            DanmakuKind::Thinking => Color32::from_rgba_unmultiplied(80, 100, 200, 90),
            DanmakuKind::ToolCall => Color32::from_rgba_unmultiplied(205, 150, 55, 105),
            DanmakuKind::ToolResultOk => Color32::from_rgba_unmultiplied(55, 170, 100, 110),
            DanmakuKind::ToolResultErr => Color32::from_rgba_unmultiplied(215, 70, 70, 115),
        }
    }

    /// 弹幕文字颜色。
    fn fg(self) -> Color32 {
        match self {
            DanmakuKind::Thinking => Color32::from_rgba_unmultiplied(205, 212, 255, 235),
            DanmakuKind::ToolCall => Color32::from_rgba_unmultiplied(255, 232, 180, 240),
            DanmakuKind::ToolResultOk => Color32::from_rgba_unmultiplied(205, 255, 222, 240),
            DanmakuKind::ToolResultErr => Color32::from_rgba_unmultiplied(255, 208, 208, 240),
        }
    }

    /// 圆角半径：思考=胶囊，工具调用=圆角卡片，工具结果=方角。
    fn radius(self, height: f32) -> f32 {
        match self {
            DanmakuKind::Thinking => height / 2.0,
            DanmakuKind::ToolCall => 6.0,
            DanmakuKind::ToolResultOk | DanmakuKind::ToolResultErr => 3.0,
        }
    }
}

/// 一条弹幕。
#[derive(Debug, Clone)]
pub struct Danmaku {
    /// 气泡显示文本（已简化的主要内容）
    pub text: String,
    /// 全量内容（鼠标悬浮时显示）
    pub full: String,
    /// 种类（气泡颜色/形状）
    pub kind: DanmakuKind,
    /// 当前 x（屏幕坐标，从右往左递减）
    pub x: f32,
    /// 轨道 y 偏移
    pub y: f32,
    /// 速度（px/s）
    pub speed: f32,
    /// 文本宽度（由渲染层测量后回填）
    pub width: f32,
    /// 是否已完成测量
    pub measured: bool,
    /// 文本 galley 缓存（避免每帧重新排版）
    pub galley: Option<std::sync::Arc<egui::Galley>>,
}

/// 弹幕层。
pub struct DanmakuLayer {
    items: VecDeque<Danmaku>,
    /// 轨道高度（行高）
    track_h: f32,
    /// 弹幕区宽度（右边缘起点）
    width: f32,
    /// 弹幕区可视高度（chat_tab 每帧按实际分配高度同步）
    height: f32,
}

/// 每个弹幕左侧的小 lo 娘 + 丝带占用的额外宽度（气泡左边缘往左）。
const LOLI_W: f32 = 22.0;

/// 同时活跃的弹幕数量上限（太多会拖慢渲染：每条含 lo 娘 ~20 个图形）。
const MAX_DANMAKU: usize = 16;

impl DanmakuLayer {
    pub fn new() -> Self {
        Self {
            items: VecDeque::new(),
            track_h: 26.0,
            width: 0.0,
            height: 88.0,
        }
    }

    /// 同步弹幕区实际可视高度（lo 娘尺寸/轨道数据此适配）。
    pub fn set_height(&mut self, h: f32) {
        self.height = h.max(1.0);
    }

    /// 发射一条弹幕（自动分配轨道）。
    pub fn fire(&mut self, kind: DanmakuKind, text: String, full: String) {
        if text.trim().is_empty() {
            return;
        }
        // 找最空轨道：统计每条轨道上最靠右的弹幕位置。
        // 轨道数必须 floor：round 会多算装不下的轨道，末轨底部超出
        // 弹幕区裁剪矩形 → 气泡下缘被切（"只显示一大半"的根因）。
        let tracks = ((self.height() / self.track_h) as f32).floor().max(1.0) as usize;
        let mut track_right: Vec<f32> = vec![0.0; tracks];
        for d in &self.items {
            let ti = ((d.y / self.track_h) as usize).min(tracks - 1);
            // 弹幕右边缘 = x + width；越靠右（x 大）越"新"。
            // 预留左侧 lo 娘宽度，保证相邻弹幕（含 lo 娘）不重叠。
            // 未测量的弹幕 width=0 → 右边缘被低估 → gap 不足。用预估下界。
            let w_est = if d.measured { d.width } else { (d.text.chars().count() as f32 * 12.0).min(300.0) };
            track_right[ti] = track_right[ti].max(d.x + w_est + LOLI_W);
        }
        // 选一个右边缘最小的轨道（最空）
        let mut best = 0usize;
        for i in 1..tracks {
            if track_right[i] < track_right[best] {
                best = i;
            }
        }
        // 同轨限速（防追赶重叠的核心）：新弹幕在该轨道的右后方出发，
        // 若比轨道上任一旧弹幕快，长途跋涉必然追上（Δv 最高 9px/s ×
        // ~20s 穿越 ≈ 180px ≫ 初始 22px 间隙）。取旧弹幕最小速度封顶：
        // 同速则间隙恒定，慢则间隙拉大——永不重叠。
        let track_min_speed = self
            .items
            .iter()
            .filter(|d| {
                ((d.y / self.track_h) as usize).min(tracks - 1) == best
            })
            .map(|d| d.speed)
            .fold(None::<f32>, |acc: Option<f32>, s| {
                Some(match acc {
                    Some(m) if m < s => m,
                    _ => s,
                })
            });
        // 新弹幕从宽度处（右侧外）进入；若轨道还有旧弹幕未走完，则从其右边缘外开始。
        // 注：self.width 在 advance() 才同步——首次 fire 时可能是 0（旧值），
        // max(track_right) 保证至少不与现有弹幕重叠
        let start_x = self.width.max(track_right[best]);
        // 固定速度 64px/s + 内容 hash 微差（±6，同条内容稳定不变）。
        // 120px/s 时约 9 秒穿越全屏，读不完就飘走了；64 ≈ 舒适阅读速度
        // （~16s 穿越）。旧版用 text.len()（字节数）做速度变量——中文/
        // emoji 一个字符 3-4 字节，同一切片速度差达 60px/s，忽快忽慢。
        let hash = text.chars().fold(0u32, |h, c| h.wrapping_add(c as u32));
        let desired = 64.0 + (hash % 4) as f32 * 2.0; // 64~70
        let speed = track_min_speed.map_or(desired, |m| desired.min(m));
        self.items.push_back(Danmaku {
            text,
            full,
            kind,
            x: start_x,
            y: best as f32 * self.track_h,
            speed,
            width: 0.0,
            measured: false,
            galley: None,
        });
        // 上限：防止无限堆积（旧弹幕先飘走，保留新弹幕）
        while self.items.len() > MAX_DANMAKU {
            self.items.pop_front();
        }
    }

    /// 每帧推进：dt 秒后更新位置，清理移出的弹幕。
    /// `hover_pause`：指针悬浮在任意弹幕上时暂停移动（可交互性——
    /// 悬浮读全文时文字不能跑）。
    pub fn advance(&mut self, dt: f32, width: f32, hover_pause: bool) {
        self.width = width;
        // 弹幕（含左侧 lo 娘）整体移出左侧即销毁
        self.items.retain(|d| d.x + d.width + LOLI_W > 0.0);
        if !hover_pause {
            for d in self.items.iter_mut() {
                d.x -= d.speed * dt;
            }
        }
    }

    /// 测量并缓存文本（每帧只对未测量项做一次 layout，之后复用 galley）。
    pub fn measure(&mut self, ui: &egui::Ui, font: &FontId) {
        for d in self.items.iter_mut() {
            if !d.measured {
                // 字形覆盖过滤（只做一次，在首次 layout 前）：弹幕文本来自
                // 任意工具输出（JSON/路径/代码片段），可能含字体链
                //（Consolas/雅黑/Segoe Emoji/Symbol）都没有的字符——缺字形
                // 渲染成"口"（tofu，用户报告弹幕偶尔飘口字）。逐字符探测
                // （epaint has_glyph 走完整回退链），无字形替换为"·"，
                // 控制字符替换为空格。
                let sanitized: String = d
                    .text
                    .chars()
                    .map(|c| {
                        if c.is_control() {
                            ' '
                        } else {
                            let covered = ui.fonts_mut(|f| f.has_glyph(font, c));
                            if covered { c } else { '·' }
                        }
                    })
                    .collect();
                d.text = sanitized;
                // hover 全文同样过滤（否则 tooltip 里照样飘 tofu）
                d.full = d
                    .full
                    .chars()
                    .map(|c| {
                        if c.is_control() {
                            ' '
                        } else if ui.fonts_mut(|f| f.has_glyph(font, c)) {
                            c
                        } else {
                            '·'
                        }
                    })
                    .collect();
                // layout 一次并缓存：绘制阶段直接用 galley，避免每帧重新排版
                // （颜色按种类排版，绘制时 fallback 同色保持一致）
                let color = d.kind.fg();
                let galley =
                    ui.fonts_mut(|f| f.layout_no_wrap(d.text.clone(), font.clone(), color));
                d.width = galley.size().x;
                d.galley = Some(galley);
                d.measured = true;
            }
        }
    }

    /// 绘制所有弹幕：每个弹幕左侧带一个小 lo 娘（拉着丝带），
    /// 弹幕按种类画气泡（颜色+形状），悬浮时显示全量内容。
    pub fn draw(
        &self,
        painter: &egui::Painter,
        origin: Pos2,
        _font: &FontId,
    ) -> Option<String> {
        let pad_x = 8.0;
        let bubble_h = self.track_h - 6.0;
        let hover_pos = painter.ctx().pointer_hover_pos();
        let clicked_now = painter.ctx().input(|i| i.pointer.primary_clicked());
        let mut clicked_full: Option<String> = None;

        for d in self.items.iter() {
            // 性能：未进入可视区（还在右边缘外等待）或已完全移出左侧的弹幕跳过绘制
            if d.x + d.width + LOLI_W <= 0.0 || d.x > self.width {
                continue;
            }
            let top = origin.y + d.y + (self.track_h - bubble_h) / 2.0;
            let bubble_left = origin.x + d.x;
            let bubble = Rect::from_min_size(
                pos2(bubble_left, top),
                Vec2::new(d.width + pad_x * 2.0, bubble_h),
            );

            // 1) 小 lo 娘（气泡左侧，随弹幕一起移动）+ 丝带连接气泡
            self.draw_mini_loli(painter, bubble, d.kind);

            // 2) 气泡背景（种类颜色 + 形状）
            painter.rect_filled(bubble, d.kind.radius(bubble_h), d.kind.bg());
            // 文本：用缓存的 galley。垂直居中用 mesh_bounds（墨迹边界）
            // 而非 font.size——逻辑字号 ≠ 字形墨迹高度，混排 emoji/CJK
            // 时文字在气泡内偏上/偏下（视觉不居中的根因）。
            if let Some(g) = &d.galley {
                let mb = g.mesh_bounds;
                let text_pos = pos2(
                    bubble.left() + pad_x,
                    bubble.center().y - mb.height() / 2.0 - mb.min.y,
                );
                painter.galley(text_pos, g.clone(), d.kind.fg());
            }

            // 3) 悬浮：显示全量内容（手动绘制 tooltip，避免依赖 egui tooltip API）
            if let Some(hp) = hover_pos {
                if bubble.contains(hp) {
                    if clicked_now && clicked_full.is_none() {
                        clicked_full = Some(d.full.clone());
                    }
                    let tip_font = FontId::monospace(12.0);
                    // 超长内容截断，避免 tooltip 撑爆屏幕
                    let mut tip_text = d.full.clone();
                    const MAX_TIP_CHARS: usize = 1200;
                    if tip_text.chars().count() > MAX_TIP_CHARS {
                        let cut: String = tip_text.chars().take(MAX_TIP_CHARS).collect();
                        tip_text = format!("{cut}…");
                    }
                    let galley = painter.layout(
                        tip_text,
                        tip_font,
                        Color32::from_rgba_unmultiplied(230, 235, 250, 255),
                        f32::INFINITY,
                    );
                    let tip_size = galley.size();
                    // 放气泡上方；顶部空间不足时放下方
                    let mut tip_pos = pos2(bubble.left(), bubble.top() - tip_size.y - 10.0);
                    if tip_pos.y < origin.y {
                        tip_pos = pos2(bubble.left(), bubble.bottom() + 10.0);
                    }
                    let bg = Rect::from_min_size(
                        tip_pos,
                        Vec2::new(tip_size.x + 14.0, tip_size.y + 10.0),
                    );
                    painter.rect_filled(bg, 6.0, Color32::from_rgba_unmultiplied(15, 18, 30, 242));
                    painter.rect_stroke(
                        bg,
                        6.0,
                        egui::Stroke::new(1.0, Color32::from_rgba_unmultiplied(110, 120, 180, 200)),
                        egui::StrokeKind::Inside,
                    );
                    painter.galley(
                        pos2(tip_pos.x + 7.0, tip_pos.y + 5.0),
                        galley,
                        Color32::from_rgba_unmultiplied(230, 235, 250, 255),
                    );
                }
            }
        }
        clicked_full
    }

    /// 指针是否悬浮在任意弹幕上（advance 暂停用）。
    pub fn any_hovered(&self, ctx: &egui::Context, origin: Pos2) -> bool {
        let Some(hp) = ctx.pointer_hover_pos() else {
            return false;
        };
        let bubble_h = self.track_h - 6.0;
        self.items.iter().any(|d| {
            let top = origin.y + d.y + (self.track_h - bubble_h) / 2.0;
            Rect::from_min_size(
                pos2(origin.x + d.x, top),
                Vec2::new(d.width + 16.0, bubble_h),
            )
            .contains(hp)
        })
    }

    /// 气泡左侧的小 lo 娘：头发+蝴蝶结+脸+裙子+手臂，
    /// 右手牵着丝带连到气泡左缘（随弹幕一起移动）。
    fn draw_mini_loli(&self, painter: &egui::Painter, bubble: Rect, kind: DanmakuKind) {
        // 缩放固定 0.8：适配 24px 轨道 / 18px 气泡。旧版按 bubble.height()
        // (18px) 算 s=0.75 再叠加 bubble.bottom()+2*s → lo 娘裙摆+腿超出
        // 轨道底部，与下一轨弹幕重叠（视觉错乱根因之二）。
        let s = 0.8;
        let cx = bubble.left() - 13.0 * s; // lo 娘中心 x（气泡左侧）
        let by = bubble.bottom() + 1.0;    // 底部贴气泡下缘，不出轨
                                            // 头发（后层大圆）
        painter.circle_filled(
            pos2(cx, by - 16.0 * s),
            7.0 * s,
            Color32::from_rgba_unmultiplied(70, 56, 110, 255),
        );
        // 脸
        painter.circle_filled(
            pos2(cx, by - 14.0 * s),
            5.2 * s,
            Color32::from_rgba_unmultiplied(255, 226, 214, 255),
        );
        // 刘海（盖住额头）
        painter.circle_filled(
            pos2(cx, by - 16.5 * s),
            5.0 * s,
            Color32::from_rgba_unmultiplied(70, 56, 110, 255),
        );
        // 眼睛
        let eye_c = Color32::from_rgba_unmultiplied(50, 40, 60, 255);
        painter.circle_filled(pos2(cx - 1.8 * s, by - 14.0 * s), 0.8 * s, eye_c);
        painter.circle_filled(pos2(cx + 1.8 * s, by - 14.0 * s), 0.8 * s, eye_c);
        // 蝴蝶结（头顶右侧）
        let bow = Color32::from_rgba_unmultiplied(255, 130, 170, 255);
        painter.circle_filled(pos2(cx + 4.5 * s, by - 19.5 * s), 2.2 * s, bow);
        painter.circle_filled(pos2(cx + 7.5 * s, by - 17.5 * s), 1.6 * s, bow);
        painter.circle_filled(
            pos2(cx + 5.5 * s, by - 18.5 * s),
            0.9 * s,
            Color32::from_rgba_unmultiplied(255, 220, 235, 255),
        );
        // 身体（上衣）
        painter.rect_filled(
            egui::Rect::from_min_size(
                pos2(cx - 4.0 * s, by - 11.5 * s),
                Vec2::new(8.0 * s, 5.5 * s),
            ),
            2.0 * s,
            Color32::from_rgba_unmultiplied(150, 130, 210, 255),
        );
        // 裙子（梯形蓬蓬裙）
        let dress = Color32::from_rgba_unmultiplied(255, 150, 190, 255);
        let skirt = egui::Shape::convex_polygon(
            vec![
                pos2(cx - 4.0 * s, by - 7.0 * s),
                pos2(cx + 4.0 * s, by - 7.0 * s),
                pos2(cx + 8.0 * s, by - 1.0 * s),
                pos2(cx - 8.0 * s, by - 1.0 * s),
            ],
            dress,
            egui::Stroke::NONE,
        );
        painter.add(skirt);
        // 腿
        let leg = Color32::from_rgba_unmultiplied(255, 226, 214, 255);
        painter.line_segment(
            [
                pos2(cx - 3.0 * s, by - 1.0 * s),
                pos2(cx - 3.0 * s, by + 1.5 * s),
            ],
            egui::Stroke::new(1.6 * s, leg),
        );
        painter.line_segment(
            [
                pos2(cx + 3.0 * s, by - 1.0 * s),
                pos2(cx + 3.0 * s, by + 1.5 * s),
            ],
            egui::Stroke::new(1.6 * s, leg),
        );
        // 左臂（下垂）
        painter.line_segment(
            [
                pos2(cx - 4.0 * s, by - 10.0 * s),
                pos2(cx - 6.0 * s, by - 6.0 * s),
            ],
            egui::Stroke::new(1.4 * s, leg),
        );
        // 右臂（前伸牵丝带）
        let arm_end = pos2(bubble.left() - 1.0, bubble.center().y);
        painter.line_segment(
            [pos2(cx + 4.0 * s, by - 10.0 * s), arm_end],
            egui::Stroke::new(1.4 * s, leg),
        );
        // 丝带：从手到气泡左缘（颜色随弹幕种类微调）
        let ribbon = match kind {
            DanmakuKind::Thinking => Color32::from_rgba_unmultiplied(180, 190, 255, 170),
            DanmakuKind::ToolCall => Color32::from_rgba_unmultiplied(255, 215, 150, 170),
            DanmakuKind::ToolResultOk => Color32::from_rgba_unmultiplied(160, 240, 190, 170),
            DanmakuKind::ToolResultErr => Color32::from_rgba_unmultiplied(255, 160, 160, 170),
        };
        painter.line_segment(
            [arm_end, pos2(bubble.left(), bubble.center().y)],
            egui::Stroke::new(1.2 * s, ribbon),
        );
        // 手心小圆
        painter.circle_filled(
            arm_end,
            1.2 * s,
            Color32::from_rgba_unmultiplied(255, 210, 195, 255),
        );
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// 弹幕区总高度（与 chat_tab 弹幕区分配高度同步；lo 娘/轨道按此适配）。
    pub fn height(&self) -> f32 {
        self.height
    }
}

impl Default for DanmakuLayer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 同轨限速防追赶：新弹幕速度 ≤ 同轨道旧弹幕最小速度 → 间隙永不缩小。
    /// 模拟整条穿越（30s），断言同轨任意时刻右缘不越过前方左缘。
    #[test]
    fn same_track_speed_clamp_prevents_catchup() {
        let mut d = DanmakuLayer::new();
        d.set_height(24.0); // 单轨道
        d.fire(DanmakuKind::ToolCall, "AAAA".into(), String::new()); // hash→某速度
        // 强制第一条为最慢 64
        d.items.back_mut().unwrap().speed = 64.0;
        d.items.back_mut().unwrap().measured = true;
        d.items.back_mut().unwrap().width = 100.0;
        // 第二条:desired 可能 70,同轨封顶到 64
        d.fire(DanmakuKind::ToolCall, "BBBBBBBB".into(), String::new());
        {
            let second = d.items.back_mut().unwrap();
            assert_eq!(second.speed, 64.0, "同轨新弹幕速度必须被压到旧最小速度");
            second.measured = true;
            second.width = 100.0;
        }
        // 模拟 30 秒:间隙不变(同速)
        let (mut x1, mut x2) = (
            d.items.front().unwrap().x,
            d.items.back().unwrap().x,
        );
        let init_gap = x2 - (x1 + 100.0);
        for _ in 0..300 {
            x1 -= 64.0 * 0.1;
            x2 -= 64.0 * 0.1;
        }
        assert!(
            x2 - (x1 + 100.0) >= init_gap - 0.5,
            "同速穿越间隙不得缩小"
        );
    }

    /// 轨道完整可见性：可用高必须是轨道高的整数倍容量——末轨气泡底缘
    /// 不得超过弹幕区（round 多算轨道导致"只显示一大半"的回归锚点）。
    #[test]
    fn tracks_never_exceed_visible_height() {
        let mut d = DanmakuLayer::new(); // track_h = 26
        d.set_height(68.0); // 90px 弹幕区 - 22px 标题条
        // 连发 10 条,统计用到的最大轨道下标
        for i in 0..10 {
            d.fire(DanmakuKind::Thinking, format!("msg{i}"), String::new());
            // 拉开右边缘让轨道循环分配
            for it in d.items.iter_mut() {
                it.x -= 200.0;
                it.width = 50.0;
                it.measured = true;
            }
        }
        let max_bottom = d
            .items
            .iter()
            .map(|it| it.y + 26.0)
            .fold(0.0f32, f32::max);
        assert!(
            max_bottom <= 68.0 + 0.5,
            "末轨底缘 {max_bottom} 不得超出可用高 68(floor 轨道数)"
        );
    }

    #[test]
    fn fire_and_advance() {
        let mut layer = DanmakuLayer::new();
        layer.fire(DanmakuKind::Thinking, "思考中…".into(), "思考中…".into());
        layer.fire(
            DanmakuKind::ToolCall,
            "🔧 exec 列出目录".into(),
            "🔧 exec {\"command\":\"dir\"}".into(),
        );
        assert_eq!(layer.len(), 2);
        assert_eq!(layer.items[0].kind, DanmakuKind::Thinking);
        assert_eq!(layer.items[1].full, "🔧 exec {\"command\":\"dir\"}");
        // 推进 10 秒：都移出左侧
        for _ in 0..100 {
            layer.advance(0.1, 800.0, false);
        }
        assert!(layer.is_empty(), "弹幕应全部移出");
    }

    #[test]
    fn empty_text_ignored() {
        let mut layer = DanmakuLayer::new();
        layer.fire(DanmakuKind::Thinking, "   ".into(), "   ".into());
        assert!(layer.is_empty());
    }

    /// 回归：同轨弹幕全程不重叠。速度统一 + 出生间距保证
    /// （历史缺陷：速度差 9px/s × 6.7s 飞行 = 60px 漂移 → 追尾重叠）。
    #[test]
    fn no_overlap_same_track() {
        let mut layer = DanmakuLayer::new();
        layer.set_height(24.0); // 1 轨道
        let w = 800.0;
        // 连续发射多条（模拟流式 chunk 高频到达）
        for i in 0..10 {
            layer.fire(
                DanmakuKind::Thinking,
                format!("msg{i}"),
                format!("msg{i}"),
            );
            // 每帧 advance + measure（真实时序）
            let mut ui_ctx = egui::Context::default();
            let _ = &mut ui_ctx;
            layer.advance(0.016, w, false);
        }
        // 模拟多帧飞行，检查任意时刻同轨不重叠
        for _ in 0..500 {
            layer.advance(0.016, w, false);
            let mut sorted: Vec<&Danmaku> = layer.items.iter().collect();
            sorted.sort_by(|a, b| a.x.partial_cmp(&b.x).unwrap());
            for pair in sorted.windows(2) {
                let (left, right) = (pair[0], pair[1]);
                // left 在左，right 在右；left 右边缘 + lo 娘不得越 right 左边缘
                let left_right = left.x + left.width + LOLI_W;
                assert!(
                    left_right <= right.x + 0.5,
                    "重叠: left_right={left_right:.1} right_x={:.1} (text={:?})",
                    right.x, left.text
                );
            }
        }
    }

    #[test]
    fn cap_at_limit() {
        let mut layer = DanmakuLayer::new();
        for i in 0..300 {
            layer.fire(
                DanmakuKind::ToolCall,
                format!("msg {i}"),
                format!("msg {i}"),
            );
        }
        // 断言上限 = 真实常量（旧断言 <=200 而 MAX_DANMAKU=16：
        // 上限失效到 200 条也会"绿"——回归防线形同虚设）
        assert!(
            layer.len() <= MAX_DANMAKU,
            "同屏弹幕不得超过 MAX_DANMAKU={MAX_DANMAKU}，实际 {}",
            layer.len()
        );
    }

    /// 弹幕字形过滤：无字形字符（如 U+30EDD 拓展区汉字，常见字体都没有）
    /// 在首次测量时替换为"·"，永不飘 tofu"口"（弹幕文本来自任意工具输出）。
    #[test]
    fn danmaku_filters_missing_glyphs() {
        let ctx = egui::Context::default();
        // 装应用的系统字体链（egui 默认字体为空 → has_glyph 恒 false，
        // 必须用真实链：Consolas/雅黑/Segoe Emoji/Symbol）
        crate::app::setup_fonts(&ctx);
        ctx.begin_pass(egui::RawInput::default());
        let mut dan = DanmakuLayer::new();
        dan.set_height(60.0);
        let rare = char::from_u32(0x30EDD).expect("valid char");
        dan.fire(
            DanmakuKind::ToolCall,
            format!("read file.rs {rare} done"),
            String::new(),
        );
        let rect = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(800.0, 600.0));
        let ui = egui::Ui::new(
            ctx.clone(),
            egui::Id::new("danmaku_glyph_test"),
            egui::UiBuilder::new().max_rect(rect),
        );
        dan.measure(&ui, &FontId::monospace(15.0));
        let mut out = ctx.end_pass();
        // 无渲染器：清掉字形光栅化产生的纹理增量（Drop 时未应用会 panic）
        out.textures_delta.clear();
        let text = dan.items.front().expect("弹幕应存在").text.clone();
        assert!(text.contains("file.rs"), "有字形部分保留: {text}");
        assert!(!text.contains(rare), "无字形字符应被替换: {text}");
        assert!(text.contains('·'), "应替换为·: {text}");
    }
}
