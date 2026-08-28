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
            track_h: 24.0,
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
        // 找最空轨道：统计每条轨道上最靠右的弹幕位置
        let tracks = (self.height() / self.track_h).max(1.0) as usize;
        let mut track_right: Vec<f32> = vec![0.0; tracks];
        for d in &self.items {
            let ti = ((d.y / self.track_h) as usize).min(tracks - 1);
            // 弹幕右边缘 = x + width；越靠右（x 大）越"新"。
            // 预留左侧 lo 娘宽度，保证相邻弹幕（含 lo 娘）不重叠。
            track_right[ti] = track_right[ti].max(d.x + d.width + LOLI_W);
        }
        // 选一个右边缘最小的轨道（最空）
        let mut best = 0usize;
        for i in 1..tracks {
            if track_right[i] < track_right[best] {
                best = i;
            }
        }
        // 新弹幕从宽度处（右侧外）进入；若轨道还有旧弹幕未走完，则从其右边缘外开始
        let start_x = self.width.max(track_right[best]);
        // 速度加快（110~170 px/s）：弹幕更快通过，同时驻留数量更少，
        // 渲染负担更低；低帧率（20fps）下依然流畅
        let speed = 110.0 + ((text.len() % 5) as f32) * 15.0;
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
    pub fn advance(&mut self, dt: f32, width: f32) {
        self.width = width;
        // 弹幕（含左侧 lo 娘）整体移出左侧即销毁
        self.items.retain(|d| d.x + d.width + LOLI_W > 0.0);
        for d in self.items.iter_mut() {
            d.x -= d.speed * dt;
        }
    }

    /// 测量并缓存文本（每帧只对未测量项做一次 layout，之后复用 galley）。
    pub fn measure(&mut self, ui: &egui::Ui, font: &FontId) {
        for d in self.items.iter_mut() {
            if !d.measured {
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
    pub fn draw(&self, painter: &egui::Painter, origin: Pos2, font: &FontId) {
        let pad_x = 8.0;
        let bubble_h = self.track_h - 6.0;
        let hover_pos = painter.ctx().pointer_hover_pos();

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
            // 文本：用缓存的 galley（测量时 layout 一次），避免每帧重新排版
            if let Some(g) = &d.galley {
                let text_pos = pos2(
                    bubble.left() + pad_x,
                    bubble.top() + (bubble_h - font.size) / 2.0,
                );
                painter.galley(text_pos, g.clone(), d.kind.fg());
            }

            // 3) 悬浮：显示全量内容（手动绘制 tooltip，避免依赖 egui tooltip API）
            if let Some(hp) = hover_pos {
                if bubble.contains(hp) {
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
    }

    /// 气泡左侧的小 lo 娘：头发+蝴蝶结+脸+裙子+手臂，
    /// 右手牵着丝带连到气泡左缘（随弹幕一起移动）。
    fn draw_mini_loli(&self, painter: &egui::Painter, bubble: Rect, kind: DanmakuKind) {
        // 基准高度 24px 的小人，按气泡高度缩放
        let s = (bubble.height() / 24.0).clamp(0.5, 1.0);
        let cx = bubble.left() - 12.0 * s; // lo 娘中心 x（气泡左侧）
        let by = bubble.bottom() + 2.0 * s; // 底部
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
            layer.advance(0.1, 800.0);
        }
        assert!(layer.is_empty(), "弹幕应全部移出");
    }

    #[test]
    fn empty_text_ignored() {
        let mut layer = DanmakuLayer::new();
        layer.fire(DanmakuKind::Thinking, "   ".into(), "   ".into());
        assert!(layer.is_empty());
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
}
