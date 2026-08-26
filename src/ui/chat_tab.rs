//! 会话标签页：弹幕式思考流 + 对话界面
//!
//! 布局（人类阅读习惯）
//! ┌──────────────────────────────────────
//!  🎯 弹幕区（AI 思考过程从右向左飘过）   danmaku layer
//! ├──────────────────────────────────────
//!  💬 对话消息（气泡）                   ScrollArea
//!                                     
//! ├──────────────────────────────────────
//!
//! │  [输入框...........]  [发送]        │
//! └──────────────────────────────────────┘
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};

use eframe::egui;
use egui::{vec2, Color32, FontId, RichText, ScrollArea, TextEdit};
use log::info;

use crate::core::preset::AgentPreset;
use crate::core::session::types;
use crate::core::{DshEngine, EngineEvent, Message, SandboxMode, Session, SessionSummary};

use super::danmaku::DanmakuLayer;
use super::i18n::{tr, Lang};
use super::markdown::ViewerState;
use super::theme::Theme;

pub struct ChatTab {
    /// 界面语言（app 每帧同步
    pub lang: Lang,
    engine: Arc<Mutex<DshEngine>>,
    event_rx: Receiver<EngineEvent>,
    sessions: Vec<SessionSummary>,
    current: Option<String>,
    current_session: Option<Session>,
    input: String,
    /// 弹幕层（AI 思考过程）
    danmaku: DanmakuLayer,
    /// markdown 解析结果缓存（key=消息内容；弹幕动画期间整窗 30fps 重绘，
    /// 无缓存时所有 markdown 消息每帧重新 parse，是性能热点）
    md_parse_cache:
        std::collections::HashMap<String, std::sync::Arc<Vec<crate::ui::markdown::MdBlock>>>,
    /// 用户消息宽度缓存（避免每帧 layout_no_wrap 测量）
    user_width_cache: std::collections::HashMap<String, f32>,
    /// 会话列表刷新限频（list_sessions 读磁盘，流式 chunk 高频到达时防抖）
    last_sessions_refresh: Option<std::time::Instant>,
    /// 正在重命名的会话 id（双击会话项进入；None = 不在重命名）
    renaming: Option<String>,
    /// 重命名输入框内容
    rename_input: String,
    /// 重命名输入框是否等待首帧焦点请求
    rename_focus_pending: bool,
    /// 最后一条用户消息的渲染矩形（切会话定位用：让用户消息在视口内）
    last_user_msg_rect: Option<egui::Rect>,
    /// 流式文本缓冲（按会话
    stream_buf: std::collections::HashMap<String, String>,
    /// 吸底开关（用户上滚后关闭，回到底部自动恢复
    stick_bottom: bool,
    /// 上一帧滚动偏移（用户滚动检测）
    scroll_offset: f32,
    /// 内容高度（吸底恢复检测）
    content_h: f32,
    /// 上一帧消息区高度（窗口缩放检测）
    msg_h_prev: f32,
    /// 上一帧消息数（新消息检测）
    msg_count_prev: usize,
    /// 打开会话后首次吸底定位到"最后一个用户消

    /// （重放历史时视口默认停在 assistant 尾巴，用户自己的消息全在折叠上方
    /// 发送时能实时看到自己的消息，重启后却看不到——首次吸底定位到用户消息修复此落差）
    user_scroll_pending: bool,
    /// 待用户回答的问题（ask_user 交互：AI 提问 UI 渲染选项按钮
    pending_question: Option<PendingQuestion>,
    /// 待用户确认的越权写操作（权限审批：串行，一次一个）
    pending_approval: Option<PendingApproval>,
    /// 待确认删除的会话 id（第一次点🗑 进入确认态，再点执行
    pending_delete: Option<String>,
    /// 标题行卡片（单选：同一时间只开一张）
    card: CardKind,
    /// 计划模式状态（fold plan/mode 事件
    plan_mode: crate::engine::plan::PlanMode,
    /// 最近计划内容（plan_write 写入
    plan_content: String,
    /// 目标（fold goal/change 事件
    goals: crate::engine::goal::GoalManager,
    /// 子代理（fold 自 subagent/descriptor 事件）
    subs: std::collections::HashMap<String, crate::engine::subagent::SubagentDescriptor>,
    /// 目标卡片的新目标输入
    goal_input: String,
    status: String,
    /// 顶部错误提示（app 侧也可写入，web 启动失败
    pub error: Option<String>,
    /// 工作区路径输
    ws_input: String,
    /// 当前工作区（规范化路径显示）
    ws_current: Option<String>,
    /// 消息内嵌图片纹理缓存（路径/URL → 纹理）
    img_cache: std::collections::HashMap<String, egui::TextureHandle>,
    /// 图片放大查看器
    viewer: Option<ViewerState>,
    /// 网络图片下载队列（URL 下载结果接收端）
    http_imgs: Vec<(String, Receiver<Option<String>>)>,
}

impl ChatTab {
    pub fn new(engine: Arc<Mutex<DshEngine>>, event_rx: Receiver<EngineEvent>) -> Self {
        let ws_current = engine
            .lock()
            .unwrap()
            .workspace_root()
            .map(|p| p.to_string_lossy().into_owned());
        let mut tab = Self {
            lang: Lang::Zh,
            engine,
            event_rx,
            sessions: Vec::new(),
            current: None,
            current_session: None,
            input: String::new(),
            danmaku: DanmakuLayer::new(),
            md_parse_cache: std::collections::HashMap::new(),
            user_width_cache: std::collections::HashMap::new(),
            last_sessions_refresh: None,
            renaming: None,
            rename_input: String::new(),
            rename_focus_pending: false,
            last_user_msg_rect: None,
            stream_buf: Default::default(),
            stick_bottom: true,
            scroll_offset: 0.0,
            content_h: 0.0,
            msg_h_prev: 0.0,
            msg_count_prev: 0,
            user_scroll_pending: false,
            pending_question: None,
            pending_approval: None,
            pending_delete: None,
            card: CardKind::None,
            plan_mode: crate::engine::plan::PlanMode::Inactive,
            plan_content: String::new(),
            goals: crate::engine::goal::GoalManager::default(),
            subs: std::collections::HashMap::new(),
            goal_input: String::new(),
            status: String::new(),
            error: None,
            ws_input: ws_current.clone().unwrap_or_default(),
            ws_current,
            img_cache: std::collections::HashMap::new(),
            viewer: None,
            http_imgs: Vec::new(),
        };
        tab.refresh_sessions();
        tab
    }

    pub fn refresh_sessions(&mut self) {
        let engine = self.engine.lock().unwrap();
        self.sessions = engine.list_sessions();
    }

    pub fn open(&mut self, session_id: &str) {
        let mut engine = self.engine.lock().unwrap();
        match engine.open_session(session_id) {
            Ok(s) => {
                self.current = Some(session_id.to_string());
                self.current_session = Some(s.clone());
                // 从会话事件恢复计划状态（重放后计划卡片内容仍在）
                let (mode, content) = crate::engine::plan::fold_plan_state(&s.events);
                self.plan_mode = mode;
                self.plan_content = content;
                // 从会话事件恢复目+ 子代
                self.goals = crate::engine::goal::GoalManager::default();
                self.subs.clear();
                for ev in &s.events {
                    self.goals.apply(ev);
                    if ev.r#type == types::SUBAGENT_DESCRIPTOR {
                        if let Some(d) = ev.data.as_ref().and_then(|d| {
                            serde_json::from_value::<crate::engine::subagent::SubagentDescriptor>(
                                d.clone(),
                            )
                            .ok()
                        }) {
                            self.subs.insert(d.subagent_id.clone(), d);
                        }
                    }
                }
                // 打开/切换会话：重置吸底（新会话从底部看起

                // 首次吸底定位到最后一个用户消息（历史重放后自己的消息可见
                self.stick_bottom = true;
                self.user_scroll_pending = true;
                // 工作区栏显示当前会话的独立工作区（每会话独立工作区）
                self.ws_current = s.cwd.clone().map(|p| p.to_string_lossy().into_owned());
                self.ws_input = self.ws_current.clone().unwrap_or_default();
            }
            Err(e) => self.error = Some(format!("{e:#}")),
        }
    }

    /// 每帧处理引擎事件
    fn pump(&mut self, ui_ctx: &egui::Context) {
        let mut any = false;
        while let Ok(ev) = self.event_rx.try_recv() {
            any = true;
            match ev {
                EngineEvent::SessionCreated { session_id } => {
                    self.refresh_sessions();
                    if self.current.is_none() {
                        self.open(&session_id);
                    }
                }
                EngineEvent::ApprovalRequested {
                    session_id,
                    id,
                    target,
                    reason,
                } => {
                    // 权限审批：记录待确认项（串行，UI 渲染确认卡片）
                    if self.current.as_deref() == Some(session_id.as_str()) {
                        info!("approval requested for {session_id}: {target}");
                        self.pending_approval = Some(PendingApproval { id, target, reason });
                    }
                }
                EngineEvent::Event { session_id, event } => {
                    let is_reasoning = event
                        .data
                        .as_ref()
                        .and_then(|d| d.get("reasoning"))
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    if matches!(
                        event.r#type.as_str(),
                        types::USER_MESSAGE | types::ASSISTANT_MESSAGE
                    ) {
                        let cur_match = self.current.as_deref() == Some(session_id.as_str());
                        let msg_count = self
                            .current_session
                            .as_ref()
                            .map(|s| s.messages.len())
                            .unwrap_or(0);
                        info!(
                            "pump: {} current_match={} current={:?} messages={}",
                            event.r#type, cur_match, self.current, msg_count
                        );
                    }
                    if self.current.as_deref() == Some(session_id.as_str()) {
                        log::debug!(
                            "chat pump: event for current session {} ({})",
                            session_id,
                            event.r#type
                        );
                        // ask_user：AI 提问 渲染选择卡片（等待用户回答）
                        if event.r#type == types::USER_QUESTION {
                            if let Some(data) = &event.data {
                                let question = data
                                    .get("question")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or_default()
                                    .to_string();
                                let options = data
                                    .get("options")
                                    .and_then(|v| v.as_array())
                                    .map(|a| {
                                        a.iter()
                                            .filter_map(|x| x.as_str().map(String::from))
                                            .collect()
                                    })
                                    .unwrap_or_default();
                                let header = data
                                    .get("header")
                                    .and_then(|v| v.as_str())
                                    .map(String::from);
                                // 记录提问所属会话（回答必须发回该会话）
                                self.pending_question = Some(PendingQuestion {
                                    session_id: session_id.clone(),
                                    question,
                                    options,
                                    header,
                                });
                                info!(
                                    "ask_user question received: {:?}",
                                    self.pending_question
                                        .as_ref()
                                        .map(|q| (&q.question, q.options.len()))
                                );
                            }
                            self.refresh_sessions();
                            continue;
                        }
                        // 计划事件（plan_write / exit_plan_mode）：更新计划卡片状
                        if event.r#type == types::PLAN_MODE {
                            let (mode, content) =
                                crate::engine::plan::fold_plan_state(&[event.clone()]);
                            self.plan_mode = mode;
                            if !content.is_empty() {
                                self.plan_content = content;
                            }
                        }
                        if let Some(s) = &mut self.current_session {
                            s.push_event(event.clone());
                            project(&mut s.messages, &event);
                            // 重命名：本地同步标题（列表刷新依赖 store，但标题栏即时更新）
                            if event.r#type == types::SESSION_TITLE {
                                if let Some(t) = event
                                    .data
                                    .as_ref()
                                    .and_then(|d| d.get("title"))
                                    .and_then(|v| v.as_str())
                                {
                                    s.title = t.to_string();
                                }
                            }
                        }
                        // 目标事件：fold 更新目标卡片
                        if event.r#type == types::GOAL_CHANGE {
                            self.goals.apply(&event);
                        }
                        // 子代理事件：更新子代理卡片（事件含完descriptor
                        if event.r#type == types::SUBAGENT_DESCRIPTOR {
                            if let Some(d) = event.data.as_ref().and_then(|d| {
                                serde_json::from_value::<
                                        crate::engine::subagent::SubagentDescriptor,
                                    >(d.clone())
                                    .ok()
                            }) {
                                self.subs.insert(d.subagent_id.clone(), d);
                            }
                        }
                        // 流式缓冲只接assistant/chunk 增量
                        // user/message content project 投影，绝不能stream_buf
                        // （否则用户消息会被渲染成左对齐的assistant 消息
                        if event.r#type == types::ASSISTANT_CHUNK {
                            if let Some(data) = &event.data {
                                if let Some(c) = data.get("content").and_then(|v| v.as_str()) {
                                    if is_reasoning {
                                        // 思考增弹幕（分句发射，避免一条过长）
                                        for segment in split_danmaku(c) {
                                            let seg = segment.clone();
                                            self.danmaku.fire(
                                                super::danmaku::DanmakuKind::Thinking,
                                                seg,
                                                segment,
                                            );
                                        }
                                    } else {
                                        let buf =
                                            self.stream_buf.entry(session_id.clone()).or_default();
                                        buf.push_str(c);
                                    }
                                }
                            }
                        }
                        // 工具调用 弹幕（名称 + 主要参数摘要，悬浮看全量）
                        if event.r#type == types::TOOL_CALL {
                            if let Some(data) = &event.data {
                                let name = data
                                    .get("name")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("?")
                                    .to_string();
                                let args =
                                    data.get("arguments").and_then(|v| v.as_str()).unwrap_or("");
                                let (brief, full) = summarize_tool_call(&name, args);
                                self.danmaku.fire(
                                    super::danmaku::DanmakuKind::ToolCall,
                                    brief,
                                    full,
                                );
                            }
                        }
                        // 工具结果 弹幕（✅/+ 主要内容摘要，悬浮看全量）
                        if event.r#type == types::TOOL_RESULT {
                            if let Some(data) = &event.data {
                                let ok = data.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
                                let value =
                                    data.get("value").map(|v| v.to_string()).unwrap_or_default();
                                let (brief, full) = summarize_tool_result(ok, &value);
                                let kind = if ok {
                                    super::danmaku::DanmakuKind::ToolResultOk
                                } else {
                                    super::danmaku::DanmakuKind::ToolResultErr
                                };
                                self.danmaku.fire(kind, brief, full);
                            }
                        }
                        if event.r#type == types::ASSISTANT_MESSAGE {
                            self.stream_buf.remove(&session_id);
                        }
                    }
                    // 会话列表刷新限频：list_sessions 读磁盘目录，流式输出时
                    // chunk 事件高频到达，逐事件刷新 = 每秒几十次磁盘 IO（卡顿
                    // 主因，还会让消息"闪现一大段"——事件积压后批量渲染）。
                    // 只对会改变列表的事件刷新，且 500ms 内最多一次。
                    // SESSION_TITLE：重命名后立即刷新列表（否则标题不更新）。
                    let now = std::time::Instant::now();
                    let type_changes_list = matches!(
                        event.r#type.as_str(),
                        types::USER_MESSAGE
                            | types::ASSISTANT_MESSAGE
                            | types::SESSION_PRESET
                            | types::SESSION_TITLE
                    );
                    if type_changes_list
                        || self
                            .last_sessions_refresh
                            .map(|t| now.duration_since(t).as_millis() > 500)
                            .unwrap_or(true)
                    {
                        self.refresh_sessions();
                        self.last_sessions_refresh = Some(now);
                    }
                }
                EngineEvent::StatusChanged { session_id, status } => {
                    info!("session {session_id} status: {status:?}");
                    if self.current.as_deref() == Some(session_id.as_str()) {
                        if let Some(s) = &mut self.current_session {
                            s.running = status == crate::core::AgentStatus::Running;
                        }
                    }
                    self.refresh_sessions();
                }
                EngineEvent::Error { message, .. } => {
                    self.error = Some(message);
                }
            }
        }
        // 引擎事件到达即请求重绘（LLM 静默回合结束不再冻结界面
        if any {
            ui_ctx.request_repaint();
        }
    }

    pub fn ui(&mut self, ui: &mut egui::Ui) {
        self.pump(&ui.ctx().clone());
        if !self.http_imgs.is_empty() {
            crate::ui::markdown::pump_http_images(
                ui.ctx(),
                &mut self.img_cache,
                &mut self.http_imgs,
            );
        }
        let full = ui.available_size();
        let sep = 8.0;
        // 左栏随窗口收缩（140-200），保证右栏有空
        let list_width = if full.x < 480.0 {
            (full.x * 0.28).clamp(100.0, 200.0)
        } else {
            200.0
        };
        // 右栏 = 实际剩余空间=0），任何窗口宽度都不溢出
        let right_w = (full.x - list_width - sep - 2.0).max(0.0);

        // ===== 左侧：会话列=====
        let (left_rect, _) = ui.allocate_exact_size(vec2(list_width, full.y), egui::Sense::hover());
        let mut left_ui = ui.new_child(egui::UiBuilder::new().max_rect(left_rect));
        left_ui.set_clip_rect(left_rect);
        {
            left_ui.add_space(4.0);
            left_ui.horizontal(|ui| {
                ui.label(Theme::section_title(&tr(self.lang, "会话", "Chats")));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .button(RichText::new("＋").color(Theme::ACCENT_LIGHT))
                        .on_hover_text(tr(self.lang, "新建会话", "New session"))
                        .clicked()
                    {
                        let mut engine = self.engine.lock().unwrap();
                        match engine.create_session(None) {
                            Ok(id) => {
                                drop(engine);
                                self.refresh_sessions();
                                self.open(&id);
                            }
                            Err(e) => self.error = Some(format!("{e:#}")),
                        }
                    }
                });
            });
            left_ui.add_space(6.0);
            left_ui.separator();
            let sessions = self.sessions.clone();
            ScrollArea::vertical().show(&mut left_ui, |ui| {
                let mut to_delete: Option<String> = None;
                let mut to_open: Option<String> = None;
                for s in &sessions {
                    let selected = self.current.as_deref() == Some(s.session_id.as_str());
                    let running = if s.running { " ⚡" } else { "" };
                    let label = format!("{}{}", s.title, running);
                    ui.horizontal(|ui| {
                        // 会话按钮：名painter 左对+ 超长按宽度截断显".."
                        // 注意：不能用 put/scope_builder 添加 Label（其Ui 会拦
                        // allocate 的点击响应——历史回归：点击会话项无法切换）
                        // 名字painter 绘制（不产生 widget），点击allocate 响应处理
                        // accesskit 标签widget_info 补上（不参与交互）
                        let avail = ui.available_width();
                        let del_w = if avail < 120.0 { 24.0 } else { 28.0 };
                        let btn_w = (avail - del_w - 8.0).max(40.0);
                        let (btn_rect, btn_resp) =
                            ui.allocate_exact_size(vec2(btn_w, 32.0), egui::Sense::click());
                        if btn_resp.hovered() || selected {
                            ui.painter().rect_filled(btn_rect, 8.0, Theme::BG_HOVER);
                        }
                        let text_color = if selected {
                            Theme::ACCENT_LIGHT
                        } else {
                            Theme::TEXT_DIM
                        };
                        // 实际字体宽度"截断（emoji/宽字符与估算偏差会导致文本溢出，
                        // 挤占右侧删除按钮的空间）
                        let font = egui::FontId::proportional(13.0);
                        // 双击会话项：进入重命名（编辑态显示输入框替换名称）
                        if btn_resp.double_clicked() {
                            self.renaming = Some(s.session_id.clone());
                            self.rename_input = s.title.clone();
                            self.rename_focus_pending = true;
                            to_open = None;
                        }
                        let is_renaming = self.renaming.as_deref() == Some(s.session_id.as_str());
                        if is_renaming {
                            // 编辑态：输入框占按钮区域（右侧仍保留删除按钮）
                            let mut edit = ui.new_child(
                                egui::UiBuilder::new()
                                    .max_rect(btn_rect.shrink2(egui::vec2(4.0, 4.0))),
                            );
                            let resp = edit.add(
                                egui::TextEdit::singleline(&mut self.rename_input)
                                    .font(font.clone())
                                    .desired_width(btn_w - 12.0),
                            );
                            // 仅进入编辑态的首帧请求焦点（否则每帧抢焦点，
                            // 点击其他控件无法失焦提交）
                            if self.rename_focus_pending {
                                resp.request_focus();
                                self.rename_focus_pending = false;
                            }
                            // Enter 提交（singleline 按 Enter 会 lost_focus，
                            // 但不同 egui 版本时序略有差异——Enter 按下即提交更稳）
                            let submitted =
                                ui.input(|i| i.key_pressed(egui::Key::Enter)) && resp.lost_focus();
                            // Enter 提交（不依赖 lost_focus——kittest/部分平台
                            // 焦点时序不同，Enter 键按下即提交最稳）
                            let enter_pressed = ui.input(|i| i.key_pressed(egui::Key::Enter));
                            let escape_pressed = ui.input(|i| i.key_pressed(egui::Key::Escape));
                            let lost = resp.lost_focus();
                            let submitted =
                                enter_pressed || (lost && !escape_pressed && !enter_pressed);
                            let cancelled = escape_pressed;
                            if submitted || cancelled {
                                let id = s.session_id.clone();
                                let name = self.rename_input.trim().to_string();
                                self.renaming = None;
                                if submitted && !name.is_empty() {
                                    let mut engine = self.engine.lock().unwrap();
                                    match engine.rename_session(&id, &name) {
                                        Ok(()) => {
                                            // 立即刷新列表（事件限频可能跳过首帧）
                                            drop(engine);
                                            self.refresh_sessions();
                                        }
                                        Err(e) => {
                                            self.error = Some(format!("重命名失败：{e:#}"));
                                        }
                                    }
                                }
                            }
                        } else {
                            let text =
                                truncate_to_width(ui, &format!("💬 {label}"), btn_w - 24.0, &font);
                            // 画名字：clip 到按钮矩形（即使测量偏差也不会画出按钮区域）
                            ui.painter().with_clip_rect(btn_rect).text(
                                egui::pos2(btn_rect.left() + 8.0, btn_rect.center().y),
                                egui::Align2::LEFT_CENTER,
                                text.clone(),
                                font,
                                text_color,
                            );
                            btn_resp.widget_info(|| {
                                egui::WidgetInfo::labeled(
                                    egui::WidgetType::Button,
                                    true,
                                    text.clone(),
                                )
                            });
                            if btn_resp.clicked() {
                                to_open = Some(s.session_id.clone());
                            }
                        }
                        // 删除按钮：会话项 = 名字(左对齐截 + 后面跟一个淡淡的 "x"（右对齐
                        // 位置固定在按钮右端。注意：不能put/scope_builder（其 scope
                        // 推进父光标，导致每项 horizontal 可用宽度递增——名字越往下越长
                        // x 被挤出列表）。用 allocate_rect + interact + painter 手动绘制
                        // 第一次点击进入确认态（红色 "OK"），再点执行
                        let is_confirm =
                            self.pending_delete.as_deref() == Some(s.session_id.as_str());
                        let del_label = if is_confirm { "OK" } else { "x" };
                        let del_color = if is_confirm {
                            Theme::ERR
                        } else {
                            Theme::TEXT_FAINT
                        };
                        let del_rect = egui::Rect::from_min_size(
                            egui::pos2(btn_rect.right() + 8.0, btn_rect.top()),
                            vec2(del_w, 32.0),
                        );
                        let del_resp = ui.interact(
                            del_rect,
                            ui.id().with(("del", &s.session_id)),
                            egui::Sense::click(),
                        );
                        if del_resp.hovered() || is_confirm {
                            ui.painter().rect_filled(del_rect, 8.0, Theme::BG_HOVER);
                        }
                        ui.painter().text(
                            egui::pos2(del_rect.center().x, del_rect.center().y),
                            egui::Align2::CENTER_CENTER,
                            del_label,
                            egui::FontId::proportional(13.0),
                            del_color,
                        );
                        del_resp.widget_info(|| {
                            egui::WidgetInfo::labeled(
                                egui::WidgetType::Button,
                                true,
                                del_label.to_string(),
                            )
                        });
                        let del_hover = del_resp.clone();
                        if del_resp.clicked() {
                            if is_confirm {
                                to_delete = Some(s.session_id.clone());
                            } else {
                                self.pending_delete = Some(s.session_id.clone());
                            }
                        }
                        // 悬停提示（删除/确认）
                        let _ = del_hover.on_hover_text(if is_confirm {
                            "再次点击确认删除此会话"
                        } else {
                            "删除会话"
                        });
                    });
                }
                // 删除：引擎删+ 状态清理（含当前会话的切换
                if let Some(id) = to_delete {
                    self.pending_delete = None;
                    let mut engine = self.engine.lock().unwrap();
                    match engine.delete_session(&id) {
                        Ok(()) => {
                            drop(engine);
                            self.stream_buf.remove(&id);
                            if self.current.as_deref() == Some(id.as_str()) {
                                self.current = None;
                                self.current_session = None;
                                self.pending_question = None;
                            }
                            self.refresh_sessions();
                            if self.current.is_none() {
                                // 删除的是当前会话 打开剩余第一个会
                                let first_id = self.sessions.first().map(|s| s.session_id.clone());
                                if let Some(fid) = first_id {
                                    self.open(&fid);
                                }
                            }
                        }
                        Err(e) => self.error = Some(format!("删除会话失败：{e:#}")),
                    }
                }
                // 打开其他会话时取消删除确认
                if let Some(id) = to_open {
                    self.pending_delete = None;
                    self.open(&id);
                }
            });
        }

        // separator
        let sep_x = left_rect.right() + 1.0;
        ui.painter().vline(
            sep_x,
            left_rect.y_range(),
            egui::Stroke::new(1.0, Theme::BORDER),
        );

        // ===== 右侧：弹幕区 + 对话 =====
        let right_rect = egui::Rect::from_min_size(
            egui::pos2(left_rect.right() + sep, left_rect.top()),
            vec2(right_w, full.y),
        );
        let mut right_ui = ui.new_child(egui::UiBuilder::new().max_rect(right_rect));
        right_ui.set_clip_rect(right_rect);
        {
            if let Some(error) = &self.error {
                right_ui.colored_label(Theme::ERR, format!("⚠ {error}"));
                right_ui.add_space(4.0);
            }
            match &self.current_session {
                Some(session) => {
                    let session = session.clone();
                    self.render_main(&mut right_ui, &session);
                }
                None => {
                    right_ui.centered_and_justified(|ui| {
                        ui.label(
                            RichText::new(tr(
                                self.lang,
                                "select a session please",
                                "Pick a session on the left, or click ＋ to create one",
                            ))
                            .size(15.0)
                            .color(Theme::TEXT_DIM),
                        );
                    });
                }
            }
        }
        self.ui_viewer(ui.ctx());
    }

    /// 图片放大查看器（模态窗口）：滚轮缩放、拖动平移、关闭
    fn ui_viewer(&mut self, ctx: &egui::Context) {
        let Some(mut v) = self.viewer.take() else {
            return;
        };
        let mut open = true;
        let title = v.title.clone();
        let win = egui::Window::new(format!("🖼 {title}"))
            .id(egui::Id::new("img_viewer"))
            .collapsible(false)
            .resizable(false)
            .default_width(640.0)
            .default_height(520.0)
            .open(&mut open);
        win.show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(tr(
                        self.lang,
                        "滚轮缩放 · 拖动平移",
                        "Scroll to zoom · drag to pan",
                    ))
                    .size(11.0)
                    .color(Theme::TEXT_DIM),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.small_button(tr(self.lang, "100%", "100%")).clicked() {
                        v.zoom = 1.0;
                        v.offset = egui::Vec2::ZERO;
                    }
                    let _ = ui.label(
                        RichText::new(format!("{:.0}%", v.zoom * 100.0))
                            .size(11.0)
                            .color(Theme::TEXT_DIM),
                    );
                });
            });
            ui.add_space(4.0);
            let scroll = ui.input(|i| i.smooth_scroll_delta.y);
            if scroll != 0.0 {
                v.zoom = (v.zoom * (1.0 + scroll * 0.0015)).clamp(0.05, 12.0);
            }
            let drag = ui.input(|i| i.pointer.delta());
            if ui.input(|i| i.pointer.primary_down()) {
                v.offset += drag;
            }
            let tex_size = v.tex.size_vec2();
            let size = egui::vec2(tex_size.x * v.zoom, tex_size.y * v.zoom);
            let (rect, _) = ui.allocate_exact_size(size, egui::Sense::drag());
            let tex_rect = rect.translate(v.offset);
            ui.painter().image(
                v.tex.id(),
                tex_rect,
                egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                Color32::WHITE,
            );
        });
        if open {
            self.viewer = Some(v);
        }
    }

    /// 主区域：弹幕区（上）+ 对话（下）
    fn render_main(&mut self, ui: &mut egui::Ui, session: &Session) {
        // 顶部标题行：右侧"目标 / 子代理 / 任务 / 计划"按钮（点击切换卡片，单选）。
        // 注意：不能直接 ui.with_layout(right_to_left)——它会推进父布局的 x cursor，
        // 导致**后续所有消息**整体右移 ~220px（历史回归：实机 AI 消息左缘 433 而非 214）。
        // 正确做法：allocate 整行，左/右各一个独立子 ui（子 ui 内随便用 right_to_left）。
        let row_h = 30.0;
        let row_w = ui.available_width();
        let (row_rect, _) = ui.allocate_exact_size(vec2(row_w, row_h), egui::Sense::hover());
        // 左区：会话标题 + 运行指示
        {
            let mut left_ui = ui.new_child(egui::UiBuilder::new().max_rect(
                egui::Rect::from_min_size(row_rect.min, vec2((row_w * 0.45).max(120.0), row_h)),
            ));
            left_ui.set_clip_rect(left_ui.max_rect());
            left_ui.horizontal(|ui| {
                ui.label(Theme::section_title(&session.title));
                if session.running {
                    ui.spinner();
                }
            });
        }
        // 右区：卡片按钮（right_to_left 只作用于子 ui，不污染父布局）
        {
            let right_rect = egui::Rect::from_min_size(
                egui::pos2(row_rect.left() + (row_w * 0.45).max(120.0), row_rect.top()),
                vec2((row_w * 0.55).max(200.0), row_h),
            );
            let mut right_ui = ui.new_child(egui::UiBuilder::new().max_rect(right_rect));
            right_ui.set_clip_rect(right_rect);
            right_ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                // 计划按钮（中文保留"📋 计划 ▸"，历史测试依赖）
                let lang = self.lang;
                let plan_active = self.plan_mode == crate::engine::plan::PlanMode::Active;
                let label = if self.card == CardKind::Plan {
                    if lang == Lang::En {
                        "📋 Plan ▾"
                    } else {
                        "📋 计划 ▾"
                    }
                } else if lang == Lang::En {
                    "📋 Plan ▸"
                } else {
                    "📋 计划 ▸"
                };
                let btn = ui
                    .add(
                        egui::Button::new(RichText::new(label).size(12.0).color(if plan_active {
                            Theme::ACCENT_LIGHT
                        } else {
                            Theme::TEXT_DIM
                        }))
                        .fill(if plan_active || self.card == CardKind::Plan {
                            Theme::BG_HOVER
                        } else {
                            egui::Color32::TRANSPARENT
                        })
                        .corner_radius(8.0),
                    )
                    .on_hover_text(if self.card == CardKind::Plan {
                        tr(lang, "点击关闭计划卡片", "Close plan card")
                    } else {
                        tr(lang, "点击查看计划卡片", "Show plan card")
                    });
                if btn.clicked() {
                    self.card = if self.card == CardKind::Plan {
                        CardKind::None
                    } else {
                        CardKind::Plan
                    };
                }
                // 其余卡片按钮（任务 / 子代理 / 目标）
                self.card_button(ui, CardKind::Jobs, &tr(self.lang, "⏳ 任务", "⏳ Tasks"));
                self.card_button(
                    ui,
                    CardKind::Subagents,
                    &tr(self.lang, "🤖 子代理", "🤖 Subagents"),
                );
                self.card_button(ui, CardKind::Goals, &tr(self.lang, "🎯 目标", "🎯 Goals"));
            });
        }
        ui.add_space(2.0);

        // ===== 标题行卡片（单选，点击按钮切换====
        match self.card {
            CardKind::Plan => self.render_plan_card(ui),
            CardKind::Goals => self.render_goals_card(ui, session),
            CardKind::Subagents => self.render_subagents_card(ui),
            CardKind::Jobs => self.render_jobs_card(ui),
            CardKind::None => {}
        }

        // 模式选择 / 工作区栏 / 弹幕/ 消息+ 输入
        self.render_body_bottom(ui, session);
    }

    /// 标题行卡片切换按钮（单选：打开一张自动关掉其他）
    fn card_button(&mut self, ui: &mut egui::Ui, kind: CardKind, label: &str) {
        let open = self.card == kind;
        let text = if open {
            format!("{label} ▾")
        } else {
            format!("{label} ▸")
        };
        let btn = ui
            .add(
                egui::Button::new(RichText::new(text).size(12.0).color(if open {
                    Theme::ACCENT_LIGHT
                } else {
                    Theme::TEXT_DIM
                }))
                .fill(if open {
                    Theme::BG_HOVER
                } else {
                    egui::Color32::TRANSPARENT
                })
                .corner_radius(8.0),
            )
            .on_hover_text(if open {
                tr(
                    self.lang,
                    &format!("点击关闭{label}卡片"),
                    &format!("Close {label}"),
                )
            } else {
                tr(
                    self.lang,
                    &format!("点击查看{label}卡片"),
                    &format!("Show {label}"),
                )
            });
        if btn.clicked() {
            self.card = if open { CardKind::None } else { kind };
        }
    }

    /// 富文本渲染（markdown）：供卡片正文复用 chat 消息的渲染引擎。
    /// 无 markdown 特征走纯文本；有则缓存解析并按块渲染（含代码块/列表/粗体等）。
    /// 这样 plan/goals/subagents 卡片内容更新时下一帧实时重渲染。
    fn render_rich(&mut self, ui: &mut egui::Ui, content: &str, salt: u64, text_color: Color32) {
        if content.trim().is_empty() {
            return;
        }
        if !looks_like_markdown(content) {
            ui.label(RichText::new(content).color(text_color));
            return;
        }
        let blocks = self
            .md_parse_cache
            .get(content)
            .cloned()
            .unwrap_or_else(|| {
                let parsed = std::sync::Arc::new(crate::ui::markdown::parse_markdown(content));
                if self.md_parse_cache.len() > 512 {
                    self.md_parse_cache.clear();
                }
                self.md_parse_cache
                    .insert(content.to_string(), parsed.clone());
                parsed
            });
        let max_w = ui.available_width().max(60.0);
        let img_cache = &mut self.img_cache;
        let viewer = &mut self.viewer;
        let http_imgs = &mut self.http_imgs;
        crate::ui::markdown::render_blocks(
            ui, &blocks, salt, text_color,
            None, // 卡片无会话 cwd 基准（不解析相对图片路径）
            img_cache, viewer, http_imgs, max_w,
        );
    }

    /// 计划卡片（可滚动）
    fn render_plan_card(&mut self, ui: &mut egui::Ui) {
        let card_h = 130.0;
        let (card_rect, _) =
            ui.allocate_exact_size(vec2(ui.available_width(), card_h), egui::Sense::hover());
        let card_painter = ui.painter_at(card_rect);
        card_painter.rect_filled(card_rect, 10.0, Theme::BG_ELEVATED);
        card_painter.rect_stroke(
            card_rect,
            10.0,
            egui::Stroke::new(1.0, Theme::BORDER),
            egui::StrokeKind::Inside,
        );
        let mut card_ui =
            ui.new_child(egui::UiBuilder::new().max_rect(card_rect.shrink2(egui::vec2(10.0, 8.0))));
        card_ui.set_clip_rect(card_rect);
        {
            let lang = self.lang;
            card_ui.horizontal(|ui| {
                ui.label(
                    RichText::new(tr(lang, "🗺 计划", "🗺 Plan"))
                        .color(Theme::ACCENT_LIGHT)
                        .strong(),
                );
                let (state_zh, state_en) =
                    if self.plan_mode == crate::engine::plan::PlanMode::Active {
                        ("进行中", "active")
                    } else {
                        ("未启用", "inactive")
                    };
                let color = if self.plan_mode == crate::engine::plan::PlanMode::Active {
                    Theme::OK
                } else {
                    Theme::TEXT_DIM
                };
                ui.label(
                    RichText::new(format!("（{}）", tr(lang, state_zh, state_en)))
                        .size(11.0)
                        .color(color),
                );
            });
            card_ui.add_space(4.0);
            ScrollArea::vertical()
                .max_height(card_h - 44.0)
                .id_salt("plan_card_scroll")
                .show(&mut card_ui, |ui| {
                    if self.plan_content.trim().is_empty() {
                        ui.label(
                            RichText::new(tr(
                                lang,
                                "暂无计划内容。告诉 AI 进入计划模式（它会先产出计划再执行），\
                                 或等 AI 调用 plan_write 后计划会显示在这里。",
                                "No plan yet. Ask the AI to enter plan mode (it plans before \
                                 acting), or wait for plan_write output to appear here.",
                            ))
                            .color(Theme::TEXT_FAINT),
                        );
                    } else {
                        let content = self.plan_content.clone();
                        self.render_rich(ui, &content, 7001u64, Theme::TEXT);
                    }
                });
        }
        ui.add_space(4.0);
    }

    /// 目标卡片：fold goal/change 事件；操作经 engine.goal_op 持久化
    fn render_goals_card(&mut self, ui: &mut egui::Ui, session: &Session) {
        let card_h = 170.0;
        let (card_rect, _) =
            ui.allocate_exact_size(vec2(ui.available_width(), card_h), egui::Sense::hover());
        let painter = ui.painter_at(card_rect);
        painter.rect_filled(card_rect, 10.0, Theme::BG_ELEVATED);
        painter.rect_stroke(
            card_rect,
            10.0,
            egui::Stroke::new(1.0, Theme::BORDER),
            egui::StrokeKind::Inside,
        );
        let mut card_ui =
            ui.new_child(egui::UiBuilder::new().max_rect(card_rect.shrink2(egui::vec2(10.0, 8.0))));
        card_ui.set_clip_rect(card_rect);
        card_ui.horizontal(|ui| {
            ui.label(
                RichText::new(tr(self.lang, "🎯 目标", "🎯 Goals"))
                    .color(Theme::ACCENT_LIGHT)
                    .strong(),
            );
            let active = self.goals.active_count();
            ui.label(
                RichText::new(if self.lang == Lang::En {
                    format!("({active} active)")
                } else {
                    format!("（{active} 进行中）")
                })
                .size(11.0)
                .color(if active > 0 {
                    Theme::OK
                } else {
                    Theme::TEXT_DIM
                }),
            );
        });
        // 新建目标输入
        let mut create_obj: Option<String> = None;
        let lang = self.lang;
        card_ui.horizontal(|ui| {
            let w = (ui.available_width() - 64.0).max(40.0);
            let resp = ui.add(
                TextEdit::singleline(&mut self.goal_input)
                    .hint_text(
                        RichText::new(tr(lang, "新目标描述：", "New goal: "))
                            .color(Theme::TEXT_FAINT),
                    )
                    .desired_width(w)
                    .text_color(Theme::TEXT),
            );
            let clicked = ui
                .add(egui::Button::new(
                    RichText::new(tr(lang, "创建", "Create")).color(Theme::ACCENT_LIGHT),
                ))
                .clicked()
                || (resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)));
            if clicked && !self.goal_input.trim().is_empty() {
                create_obj = Some(self.goal_input.trim().to_string());
                self.goal_input.clear();
            }
        });
        card_ui.add_space(2.0);
        let goals: Vec<crate::engine::goal::Goal> =
            self.goals.list().iter().map(|g| (*g).clone()).collect();
        let mut ops: Vec<(String, crate::engine::goal::GoalOp)> = Vec::new();
        ScrollArea::vertical()
            .max_height(card_h - 64.0)
            .id_salt("goals_card_scroll")
            .show(&mut card_ui, |ui| {
                for g in &goals {
                    let color = match g.phase {
                        crate::engine::goal::GoalPhase::Active => Theme::OK,
                        crate::engine::goal::GoalPhase::Paused => Theme::WARN,
                        crate::engine::goal::GoalPhase::Blocked => Theme::ERR,
                        crate::engine::goal::GoalPhase::Complete => Theme::TEXT_FAINT,
                    };
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(format!("● {}", g.objective)).color(color));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.label(Theme::dim(g.phase.as_str()));
                            use crate::engine::goal::{GoalOp, GoalPhase};
                            if g.phase != GoalPhase::Complete
                                && ui.small_button(tr(lang, "完成", "Complete")).clicked()
                            {
                                ops.push((g.id.clone(), GoalOp::Complete));
                            }
                            if g.phase == GoalPhase::Active
                                && ui.small_button(tr(lang, "暂停", "Pause")).clicked()
                            {
                                ops.push((g.id.clone(), GoalOp::Pause));
                            }
                            if g.phase == GoalPhase::Paused
                                && ui.small_button(tr(lang, "恢复", "Resume")).clicked()
                            {
                                ops.push((g.id.clone(), GoalOp::Resume));
                            }
                            if g.phase == GoalPhase::Active
                                && ui.small_button(tr(lang, "阻塞", "Block")).clicked()
                            {
                                ops.push((g.id.clone(), GoalOp::Block));
                            }
                        });
                    });
                    ui.add_space(2.0);
                }
                if goals.is_empty() {
                    ui.label(Theme::dim(&tr(
                        lang,
                        "暂无目标。告诉 AI 创建，或在上方输入后点「创建」。",
                        "No goals. Ask the AI to create one, or enter above and click Create.",
                    )));
                }
            });
        // 执行操作（引goal_op goal/change 事件持久+ 通知，pump 回灌 fold
        if let Some(obj) = create_obj {
            let gid = format!("g-{}", crate::util::simple_id());
            let mut engine = self.engine.lock().unwrap();
            let _ = engine.goal_op(
                &session.id,
                crate::engine::goal::GoalOp::Create,
                &gid,
                Some(&obj),
            );
        }
        for (gid, op) in ops {
            let mut engine = self.engine.lock().unwrap();
            let _ = engine.goal_op(&session.id, op, &gid, None);
        }
        ui.add_space(4.0);
    }

    /// 子代理卡片：fold subagent/descriptor 事件（持久化，重放可恢复）
    fn render_subagents_card(&mut self, ui: &mut egui::Ui) {
        let card_h = 150.0;
        let (card_rect, _) =
            ui.allocate_exact_size(vec2(ui.available_width(), card_h), egui::Sense::hover());
        let painter = ui.painter_at(card_rect);
        painter.rect_filled(card_rect, 10.0, Theme::BG_ELEVATED);
        painter.rect_stroke(
            card_rect,
            10.0,
            egui::Stroke::new(1.0, Theme::BORDER),
            egui::StrokeKind::Inside,
        );
        let mut card_ui =
            ui.new_child(egui::UiBuilder::new().max_rect(card_rect.shrink2(egui::vec2(10.0, 8.0))));
        card_ui.set_clip_rect(card_rect);
        card_ui.horizontal(|ui| {
            ui.label(
                RichText::new(tr(self.lang, "🤖 子代理", "🤖 Subagents"))
                    .color(Theme::ACCENT_LIGHT)
                    .strong(),
            );
            ui.label(Theme::dim(&format!(
                "（{} {}）",
                self.subs.len(),
                tr(self.lang, "个", "active")
            )));
        });
        card_ui.add_space(4.0);
        let mut subs: Vec<_> = self.subs.values().cloned().collect();
        subs.sort_by(|a, b| b.subagent_id.cmp(&a.subagent_id));
        ScrollArea::vertical()
            .max_height(card_h - 40.0)
            .id_salt("subs_card_scroll")
            .show(&mut card_ui, |ui| {
                for d in &subs {
                    let color = match d.status.as_str() {
                        "done" => Theme::OK,
                        "failed" => Theme::ERR,
                        "running" => Theme::CYAN,
                        _ => Theme::TEXT_DIM,
                    };
                    let icon = match d.status.as_str() {
                        "running" => "⏳",
                        "done" => "✅",
                        "failed" => "❌",
                        _ => "🕐",
                    };
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new(format!("{icon} {}", d.subagent_id))
                                .monospace()
                                .color(color),
                        );
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.label(Theme::dim(&d.status));
                        });
                    });
                    if let Some(sum) = &d.summary {
                        ui.label(
                            RichText::new(truncate(sum, 200))
                                .size(11.0)
                                .color(Theme::TEXT_DIM),
                        );
                    }
                    ui.add_space(3.0);
                }
                if subs.is_empty() {
                    ui.label(Theme::dim(&tr(
                        self.lang,
                        "暂无子代理。AI 调用 subagent_fork 工具后显示在这里。",
                        "No subagents yet. They appear here when the AI calls subagent_fork.",
                    )));
                }
            });
        ui.add_space(4.0);
    }

    /// 任务卡片：实时读取引JobManager（回/ 子代理自动创建任务）
    fn render_jobs_card(&mut self, ui: &mut egui::Ui) {
        let card_h = 160.0;
        let (card_rect, _) =
            ui.allocate_exact_size(vec2(ui.available_width(), card_h), egui::Sense::hover());
        let painter = ui.painter_at(card_rect);
        painter.rect_filled(card_rect, 10.0, Theme::BG_ELEVATED);
        painter.rect_stroke(
            card_rect,
            10.0,
            egui::Stroke::new(1.0, Theme::BORDER),
            egui::StrokeKind::Inside,
        );
        let mut card_ui =
            ui.new_child(egui::UiBuilder::new().max_rect(card_rect.shrink2(egui::vec2(10.0, 8.0))));
        card_ui.set_clip_rect(card_rect);
        let mut remove: Option<String> = None;
        let lang = self.lang;
        card_ui.horizontal(|ui| {
            ui.label(
                RichText::new(tr(lang, "⏳ 任务", "⏳ Jobs"))
                    .color(Theme::ACCENT_LIGHT)
                    .strong(),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .small_button(tr(lang, "清空已完成", "Clear finished"))
                    .clicked()
                {
                    self.engine.lock().unwrap().job_clear_finished();
                }
            });
        });
        card_ui.add_space(4.0);
        let jobs = self.engine.lock().unwrap().jobs_snapshot();
        ScrollArea::vertical()
            .max_height(card_h - 44.0)
            .id_salt("jobs_card_scroll")
            .show(&mut card_ui, |ui| {
                for job in &jobs {
                    let color = match job.status {
                        crate::engine::jobs::JobStatus::Running => Theme::CYAN,
                        crate::engine::jobs::JobStatus::Done => Theme::OK,
                        crate::engine::jobs::JobStatus::Failed => Theme::ERR,
                        crate::engine::jobs::JobStatus::Pending => Theme::TEXT_DIM,
                    };
                    let icon = match job.status {
                        crate::engine::jobs::JobStatus::Running => "⏳",
                        crate::engine::jobs::JobStatus::Done => "✅",
                        crate::engine::jobs::JobStatus::Failed => "❌",
                        crate::engine::jobs::JobStatus::Pending => "🕐",
                    };
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(format!("{icon} {}", job.name)).color(color));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.label(Theme::dim(job.status.as_str()));
                            if ui.small_button(tr(lang, "移除", "Remove")).clicked() {
                                remove = Some(job.id.clone());
                            }
                        });
                    });
                    if let Some(d) = &job.detail {
                        ui.label(Theme::dim(d));
                    }
                    if let Some(r) = &job.result {
                        ui.label(
                            RichText::new(truncate(r, 140))
                                .size(11.0)
                                .color(Theme::TEXT_DIM),
                        );
                    }
                    ui.add_space(3.0);
                }
                if jobs.is_empty() {
                    ui.label(Theme::dim(&tr(
                        lang,
                        "暂无任务。agent 回合 / 子代理执行会自动创建任务。",
                        "No jobs. Agent turns and subagents create jobs automatically.",
                    )));
                }
            });
        if let Some(id) = remove {
            self.engine.lock().unwrap().job_remove(&id);
        }
        ui.add_space(4.0);
    }

    /// 标题行下方其余区域：模式选择 / 工作区栏 / 弹幕/ 消息+ 输入区
    fn render_body_bottom(&mut self, ui: &mut egui::Ui, session: &Session) {
        // ===== 模式选择（标/ PTC / 极简 / 创造，对齐 DSH agent-presets====
        // horizontal_wrapped 会污染父布局 x（子布局宽=内容宽 → 父 cursor.x
        // 推进 ~420px，后续工作区栏/弹幕/消息区整体右移 = 实机"AI 消息偏移"）。
        // 正确做法：allocate 整行 + 独立子 ui（子 ui 内随便 wrap，不污染父）。
        // 小窗口（<450px 可用高）隐藏模式行，把高度让给消息区。
        if ui.available_height() > 450.0 {
            let mode_h = 28.0;
            let (mode_rect, _) =
                ui.allocate_exact_size(vec2(ui.available_width(), mode_h), egui::Sense::hover());
            let mut mode_ui = ui.new_child(egui::UiBuilder::new().max_rect(mode_rect));
            mode_ui.set_clip_rect(mode_rect);
            mode_ui.horizontal_wrapped(|ui| {
                ui.label(Theme::dim(&tr(self.lang, "模式:", "Mode:")));
                for p in AgentPreset::all() {
                    let selected = session.preset == p;
                    let label = if selected {
                        RichText::new(format!("● {}", p.name())).color(Theme::ACCENT_LIGHT)
                    } else {
                        RichText::new(p.name()).color(Theme::TEXT_DIM)
                    };
                    if ui.selectable_label(selected, label).clicked() && !selected {
                        let id = session.id.clone();
                        let mut engine = self.engine.lock().unwrap();
                        match engine.set_session_preset(&id, p) {
                            Ok(()) => {
                                log::info!("session {} switched to {}", id, p.id());
                                drop(engine);
                                // 本地同步 preset（引擎已持久化）。不重开会话—
                                // 重开会话会把视口跳回消息区底部，模式行在顶部
                                // 滚出视野，看起来点击没反
                                if let Some(s) = &mut self.current_session {
                                    s.preset = p;
                                }
                                self.status = format!(
                                    "{} {}",
                                    tr(self.lang, "已切换模式", "Mode switched"),
                                    p.name()
                                );
                            }
                            Err(e) => self.error = Some(format!("切换模式失败：{e:#}")),
                        }
                    }
                }
                // 长描述只在右栏足够宽（按描述能排同一行，1010px）时显示
                // 否则换行独占 1-2 行挤占消息区，历史消息可见性优
                if ui.available_width() > 1050.0 {
                    ui.label(Theme::dim(session.preset.description()));
                }
            });
            ui.add_space(4.0);
            ui.separator();
            ui.add_space(2.0);
        }

        // ===== 工作区栏（窄栏时自动换行；窗口矮时隐藏腾空间====
        // horizontal_wrapped 污染父布局 x（同上），用 allocate 子 ui 隔离。
        // 小窗口（<420px）隐藏，把高度让给消息区。
        if ui.available_height() > 420.0 {
            let lang = self.lang;
            let ws_h = 30.0;
            let (ws_rect, _) =
                ui.allocate_exact_size(vec2(ui.available_width(), ws_h), egui::Sense::hover());
            let mut ws_ui = ui.new_child(egui::UiBuilder::new().max_rect(ws_rect));
            ws_ui.set_clip_rect(ws_rect);
            ws_ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new("📁").size(13.0));
                // 留足按钮/状态宽度，避免差几像素换行2 行（挤占消息区）
                let w = (ui.available_width() - 190.0).max(40.0);
                let resp = ui.add(
                    TextEdit::singleline(&mut self.ws_input)
                        .hint_text(
                            RichText::new(tr(lang, "打开工作区目录：", "Open workspace dir: "))
                                .color(Theme::TEXT_FAINT),
                        )
                        .desired_width(w)
                        .text_color(Theme::TEXT),
                );
                let browse = ui
                    .add(egui::Button::new(
                        RichText::new(tr(lang, "📂 浏览…", "📂 Browse…")).color(Theme::TEXT_DIM),
                    ))
                    .on_hover_text(tr(lang, "打开系统目录选择器", "Open system folder picker"));
                if browse.clicked() {
                    if let Some(dir) = rfd::FileDialog::new()
                        .set_title(tr(lang, "选择工作区目录", "Select workspace folder"))
                        .pick_folder()
                    {
                        let path = dir.to_string_lossy().into_owned();
                        self.ws_input = path.clone();
                        let mut engine = self.engine.lock().unwrap();
                        // 设置当前会话的独立工作区（AI 下一回合立即生效
                        match engine.set_session_workspace(&session.id, std::path::Path::new(&path))
                        {
                            Ok(root) => {
                                log::info!("workspace opened via picker: {root}");
                                self.ws_current = Some(root);
                                self.error = None;
                            }
                            Err(e) => self.error = Some(format!("工作区打开失败：{e}")),
                        }
                    }
                }
                let opened = ui
                    .add(egui::Button::new(
                        RichText::new(tr(lang, "打开", "Open")).color(Theme::ACCENT_LIGHT),
                    ))
                    .on_hover_text(tr(
                        lang,
                        "规范化目录并作为本会话工作区（AI 回合 / 工具立即生效）",
                        "Normalize and set as this session's workspace",
                    ))
                    .clicked()
                    || (resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)));
                if opened && !self.ws_input.trim().is_empty() {
                    let path = self.ws_input.trim().to_string();
                    let mut engine = self.engine.lock().unwrap();
                    match engine.set_session_workspace(&session.id, std::path::Path::new(&path)) {
                        Ok(root) => {
                            log::info!("workspace opened from chat: {root}");
                            self.ws_current = Some(root);
                            self.error = None;
                        }
                        Err(e) => self.error = Some(format!("工作区打开失败：{e}")),
                    }
                }
                if let Some(ws) = &self.ws_current {
                    // 只显示目录名（不显示 \\?\ 前缀、不显示完整路径
                    let name = std::path::Path::new(ws)
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| ws.clone());
                    ui.label(Theme::dim(&format!("✔ {}", truncate(&name, 24))));
                }
            });
        }
        ui.add_space(4.0);
        ui.separator();
        ui.add_space(4.0);

        // ===== 弹幕区（AI 思考过程；窗口矮时动态压缩，避免输入框被挤出面板====
        // 阈值提高：小窗口（<550px 可用高）隐藏弹幕区，把空间让给消息区
        // （消息区是主要阅读区域；弹幕是装饰——历史回归："看不到历史消息"）。
        let avail_before_dm = ui.available_size();
        let dm_height = if avail_before_dm.y < 550.0 {
            0.0 // 高度不足：隐藏弹幕区，保住消息区与输入框
        } else if avail_before_dm.y < 750.0 {
            60.0 // 中等高度：压缩弹幕区
        } else {
            90.0
        };
        let avail = ui.available_size();
        let dm_w = avail.x.max(60.0);
        let (dm_rect, _) = ui.allocate_exact_size(vec2(dm_w, dm_height), egui::Sense::hover());
        // 背景：半透明深色 + 顶部边框
        let painter = ui.painter_at(dm_rect);
        painter.rect_filled(
            dm_rect,
            8.0,
            Color32::from_rgba_unmultiplied(20, 24, 40, 200),
        );
        painter.hline(
            dm_rect.x_range(),
            dm_rect.bottom(),
            egui::Stroke::new(1.0, Theme::BORDER),
        );

        // 弹幕标题
        let painter2 = ui.painter_at(dm_rect);
        painter2.text(
            egui::pos2(dm_rect.left() + 10.0, dm_rect.top() + 4.0),
            egui::Align2::LEFT_TOP,
            tr(self.lang, "🎯 AI 思考", "🎯 AI thinking"),
            FontId::proportional(11.0),
            Theme::TEXT_FAINT,
        );

        // 推进 + 测量 + 绘制弹幕
        let dt = ui.input(|i| i.stable_dt.min(0.05));
        let font = FontId::monospace(15.0);
        self.danmaku.set_height(dm_height);
        self.danmaku.measure(ui, &font);
        self.danmaku.advance(dt, dm_rect.width() - 10.0);
        self.danmaku.draw(
            &painter,
            egui::pos2(dm_rect.left() + 10.0, dm_rect.top() + 22.0),
            &font,
        );
        // 有弹幕且弹幕区可见时持续重绘（动画）。
        // 20fps（50ms）已足够平滑（弹幕速度较快，帧间位移小）；
        // 更高帧率会拖累整窗重绘（消息区 markdown 每帧重排），
        // 矮窗口隐藏弹幕区（dm_height==0）时绝不请求重绘。
        if dm_height > 0.0 && !self.danmaku.is_empty() {
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(50));
        }

        if dm_height > 0.0 {
            ui.add_space(4.0);
            ui.separator();
            ui.add_space(4.0);
        }

        // ===== 消息+ 输入区：手动矩形分配 =====
        // 输入框矩形锚定面板底部（任何窗口高度都完整可见），消息区在其上方占剩
        let full = ui.available_rect_before_wrap();
        // 输入区高度三档：96（正常）/ 80（矮窗）/ 64（超矮窗，单行紧凑模式）
        let input_h = if full.height() < 70.0 {
            64.0
        } else if full.height() < 200.0 {
            80.0
        } else {
            96.0
        };
        let input_rect = egui::Rect::from_min_max(
            egui::pos2(full.left(), (full.bottom() - input_h).max(full.top())),
            full.max,
        );
        // 输入区（底部，手rect
        self.render_input(ui, session, input_rect);
        // 消息区（输入区上方剩余；高度不足时为 0，不挤压输入框）
        let msg_rect = egui::Rect::from_min_max(
            full.min,
            egui::pos2(
                full.right(),
                (full.bottom() - input_h - 8.0).max(full.top()),
            ),
        );
        if msg_rect.height() > 4.0 {
            let mut msg_ui = ui.new_child(egui::UiBuilder::new().max_rect(msg_rect));
            msg_ui.set_clip_rect(msg_rect);
            // 吸底控制：窗口缩/ 新消/ 流式输出中的帧，
            // 对最后一条消息调用官scroll_to_me 滚到底部
            // 其余帧不干预滚动（用户自由滚动）
            let streaming = !self
                .stream_buf
                .get(&session.id)
                .map(|s| s.is_empty())
                .unwrap_or(false)
                && session.running;
            let h_changed = (self.msg_h_prev - msg_rect.height()).abs() > 4.0;
            let new_msg = session.messages.len() != self.msg_count_prev;
            // 新消息（用户发回复完成）或打开会话后的待定定位（user_scroll_pending
            // 无条件滚底；流式/缩放仅在吸底状态（用户未上滚）时跟随
            // 注意：切会话时新旧会话消息数可能相同且窗口高度不变，new_msg/
            // h_changed 均为 false，此时必须靠 user_scroll_pending 强制定位
            // （否则视口停在上一会话的位置，用户消息在折叠上方不可见）
            let force_bottom = new_msg
                || self.user_scroll_pending
                || (self.stick_bottom && (h_changed || streaming));
            self.msg_h_prev = msg_rect.height();
            self.msg_count_prev = session.messages.len();
            let mut clicked_option: Option<String> = None;
            let mut approval_action: Option<(String, crate::engine::approval::ApprovalDecision)> =
                None;
            // 切会话（user_scroll_pending）后的滚动定位在 show() 之后手动
            // 写 scrolled.state.offset 完成（ScrollArea id 相同导致上一会话
            // 深 offset 复用；builder offset / scroll_to_me 在内容未布局时
            // 不可靠——历史回归："切会话后看不到历史消息"）。
            let mut scrolled = ScrollArea::vertical()
                .id_salt("chat_scroll")
                .max_height(msg_rect.height())
                .max_width(msg_rect.width())
                .auto_shrink([false, false])
                .show(&mut msg_ui, |ui| {
                    // 终极防线：内容宽度硬限**视口宽**（msg_rect.width）。
                    // 任何消息（含未知/历史超宽内容）都不得把 ScrollArea 内容撑宽
                    // ——否则后续消息的可用宽失真（实机：AI 消息左缘偏移 200px+）。
                    // 注意：不能用 ui.available_width()（内容已被撑宽时它同样失真）。
                    ui.set_max_width(msg_rect.width());
                    let stream = self
                        .stream_buf
                        .get(&session.id)
                        .cloned()
                        .unwrap_or_default();
                    // 性能：不 clone 整个 messages 列表（弹幕动画期间整窗高频
                    // 重绘，几百条消息每帧克隆 = 大量堆分配）。历史消息直接借用
                    // 迭代，流式缓冲单独作为一条追加渲染。
                    let has_stream = !stream.is_empty() && session.running;
                    let mut last_resp: Option<egui::Response> = None;
                    let mut last_user_resp: Option<egui::Response> = None;
                    self.last_user_msg_rect = None;
                    for (idx, msg) in session.messages.iter().enumerate() {
                        let resp = self.render_message(ui, msg, idx as u64, false);
                        last_resp = Some(resp.clone());
                        if matches!(msg, Message::User { .. }) {
                            self.last_user_msg_rect = Some(resp.rect);
                            last_user_resp = Some(resp);
                        }
                    }
                    // 流式缓冲：只在回合运行中显示（中断/失败 ASSISTANT_MESSAGE
                    // 事件可能不出现，残留的旧流会变成"幽灵消息"钉在底部）
                    if has_stream {
                        let idx = session.messages.len();
                        let msg = Message::Assistant {
                            content: stream,
                            tool_calls: Vec::new(),
                        };
                        let resp = self.render_message(ui, &msg, idx as u64, true);
                        last_resp = Some(resp);
                    }
                    // 权限审批卡片（与 ask_user 选择卡片一致：消息区可点击）
                    if let Some(a) = &self.pending_approval {
                        ui.add_space(6.0);
                        let card = egui::Frame::default()
                            .fill(Theme::BG_ELEVATED)
                            .stroke(egui::Stroke::new(1.0, Theme::WARN))
                            .corner_radius(egui::CornerRadius::same(10))
                            .inner_margin(egui::Margin::same(12));
                        let cr = card.show(ui, |ui| {
                            ui.label(
                                RichText::new(tr(
                                    self.lang,
                                    "🔐 需要授权：写工作区外",
                                    "🔐 Authorization needed: outside workspace",
                                ))
                                .color(Theme::WARN)
                                .strong(),
                            );
                            ui.add_space(4.0);
                            ui.label(Theme::dim(&a.reason));
                            ui.add_space(2.0);
                            ui.label(Theme::dim(a.target.as_str()));
                            ui.add_space(6.0);
                            ui.horizontal_wrapped(|ui| {
                                let opts = [
                                    ("A  允许本次", crate::engine::approval::ApprovalDecision::Allow, Theme::OK),
                                    ("B  拒绝", crate::engine::approval::ApprovalDecision::Deny, Theme::ERR),
                                    ("C  总是允许", crate::engine::approval::ApprovalDecision::AlwaysAllow, Theme::ACCENT),
                                ];
                                for (label, dec, fill) in opts {
                                    let btn = ui.add(
                                        egui::Button::new(
                                            RichText::new(label).color(Color32::WHITE),
                                        )
                                        .fill(fill)
                                        .corner_radius(8.0),
                                    );
                                    if btn
                                        .on_hover_text(tr(self.lang, "点击选择", "Click to choose"))
                                        .clicked()
                                    {
                                        approval_action = Some((a.id.clone(), dec));
                                    }
                                }
                            });
                        });
                        last_resp = Some(cr.response);
                    }
                    // 待用户回答的问题卡片（ask_user：AI 提问 选项按钮
                    // 只在提问会话显示（绑定 session_id；切走后不显示，
                    // 避免回答发到错误会话）
                    if let Some(q) = &self.pending_question {
                        if !q.question.is_empty() && q.session_id == session.id {
                            ui.add_space(6.0);
                            let card = egui::Frame::default()
                                .fill(Theme::BG_ELEVATED)
                                .stroke(egui::Stroke::new(1.0, Theme::ACCENT))
                                .corner_radius(egui::CornerRadius::same(10))
                                .inner_margin(egui::Margin::same(12));
                            let cr = card.show(ui, |ui| {
                                ui.label(
                                    RichText::new(tr(
                                        self.lang,
                                    "❓ 需要你的选择",
                                    "❓ Your input needed",
                                    ))
                                    .color(Theme::ACCENT_LIGHT)
                                    .strong(),
                                );
                                if let Some(h) = &q.header {
                                    ui.add_space(2.0);
                                    ui.label(Theme::dim(h));
                                }
                                ui.add_space(4.0);
                                ui.label(RichText::new(&q.question).color(Theme::TEXT));
                                if !q.options.is_empty() {
                                    ui.add_space(6.0);
                                    ui.horizontal_wrapped(|ui| {
                                        for opt in &q.options {
                                            let btn = ui.add(
                                                egui::Button::new(
                                                    RichText::new(opt.clone())
                                                        .color(Color32::WHITE),
                                                )
                                                .fill(Theme::ACCENT)
                                                .corner_radius(8.0),
                                            );
                                            if btn
                                                .on_hover_text(tr(
                                                    self.lang,
                                                    "点击作为回答发送",
                                                    "Click to send as answer",
                                                ))
                                                .clicked()
                                            {
                                                clicked_option = Some(opt.clone());
                                            }
                                        }
                                    });
                                }
                            });
                            last_resp = Some(cr.response);
                        }
                    }
                    if force_bottom {
                        // 打开会话后的首次吸底：定位到最后一个用户消息（历史重放
                        // 自己的消息可见，其下方的 AI 回复也保留在视口内）
                        // 若最后一条本身就是用户消息，则与正常滚底等价。
                        // 注意：切会话（user_scroll_pending）的定位由下方
                        // scroll_to_rect 完成（内容闭包内 rect 有效）；此处
                        // 只处理常规吸底（新消息/流式跟随）。
                        if !self.user_scroll_pending {
                            if let Some(resp) = &last_resp {
                                // 瞬时滚动（无动画），避免与用户滚轮竞争
                                resp.scroll_to_me_animation(
                                    Some(egui::Align::Max),
                                    egui::style::ScrollAnimation::none(),
                                );
                            }
                        }
                    } else {
                        // 用户已自行滚动定位，取消待定的用户消息定
                        self.user_scroll_pending = false;
                    }
                    // 切会话（user_scroll_pending）后的滚动定位：在内容闭包内用
                    // scroll_to_rect 把视口滚到最后一条消息（内容布局已发生，
                    // rect 有效）。show() 之后写 state.offset 只改返回副本、
                    // 不会回写 ScrollArea 持久 state（历史回归："切会话后看不到
                    // 历史消息"——ScrollArea id 复用上一会话深 offset）。
                    if session.messages.is_empty() && !has_stream {
                        ui.add_space(20.0);
                        ui.centered_and_justified(|ui| {
                            ui.label(
                                RichText::new(tr(
                                    self.lang,
                                    "开始对话 —— 输入你的问题，AI 的思考会飘过",
                                    "Start chatting — type your question; the AI's thinking floats by",
                                ))
                                .color(Theme::TEXT_FAINT),
                            );
                        });
                    }
                });
            // 用户滚动检测：offset 变小（上滚）退出吸底；回到底部 恢复吸底
            let mut new_offset = scrolled.state.offset.y;
            if self.user_scroll_pending {
                // 切会话/首次打开的强制定位：把滚动状态写回持久存储
                // （ScrollArea id 相同导致上一会话深 offset 复用；egui 的
                // scroll_to_rect/scroll_to_me 写全局 pass_state，但内容闭包
                // 内调用后会被其他 ScrollArea 的 end() 竞争吞掉——实测不生效。
                // 直接改持久 State.offset，下一帧 begin() 即生效）。
                // 定位目标：最后一条用户消息（让用户自己的消息在视口内，
                // 而不是滚到内容底部——底部可能是长 AI 回复，用户消息被
                // 挤出视口上方，看起来"用户消息不显示"）。
                let target_offset = if let Some(ur) = self.last_user_msg_rect {
                    // 用户消息顶部对齐视口顶部（留 8px 边距）
                    (ur.top() - 8.0).max(0.0)
                } else {
                    (scrolled.content_size.y - msg_rect.height()).max(0.0)
                };
                let max_offset = (scrolled.content_size.y - msg_rect.height()).max(0.0);
                scrolled.state.offset.y = target_offset.min(max_offset);
                new_offset = scrolled.state.offset.y;
                let id = scrolled.id;
                let state = scrolled.state;
                ui.ctx().data_mut(|d| d.insert_persisted(id, state));
                self.user_scroll_pending = false;
            }
            if new_offset < self.scroll_offset - 1.0 {
                self.stick_bottom = false;
            }
            if self.content_h - new_offset < msg_rect.height() + 20.0 {
                self.stick_bottom = true;
            }
            self.scroll_offset = new_offset;
            self.content_h = scrolled.content_size.y;
            // 用户点击了问题卡片选项 作为回答发送并清除卡片。
            // 回答必须发回提问的会话（卡片绑定 q.session_id；不能发到
            // 当前渲染会话——用户可能已切走，历史回归"消息回到别的会话"）。
            if let Some(ans) = clicked_option {
                let qid = self.pending_question.as_ref().map(|q| q.session_id.clone());
                self.pending_question = None;
                if let Some(id) = qid {
                    let mut engine = self.engine.lock().unwrap();
                    match engine.send_message(&id, &ans) {
                        Ok(()) => {
                            info!("question answered: {ans} -> {id}");
                        }
                        Err(e) => self.error = Some(format!("{e:#}")),
                    }
                }
            }
            // 处理权限审批动作（用户点了 A/B/C → 回传引擎）
            if let Some((aid, decision)) = approval_action {
                self.pending_approval = None;
                let engine = self.engine.clone();
                let resolved = engine.lock().unwrap().resolve_approval(&aid, decision);
                info!("approval {aid} resolved={resolved} {:?}", decision);
            }
        }
    }

    /// 输入区：Enter 发送（16px 亮色字体 + 圆角容器 + 焦点高亮；矩形由调用方钉底）
    fn render_input(&mut self, ui: &mut egui::Ui, session: &Session, input_rect: egui::Rect) {
        // 容器背景（与面板区分的深色凹槽）
        ui.painter().rect_filled(input_rect, 10.0, Theme::BG);
        // 压缩模式（输入区 <70px 高）：减小边+ 单行输入，防止内容溢出被窗口裁掉
        let compact = input_rect.height() < 70.0;
        let pad_y = if compact { 6.0 } else { 12.0 };
        let mut input_ui =
            ui.new_child(egui::UiBuilder::new().max_rect(input_rect.shrink2(vec2(14.0, pad_y))));
        input_ui.set_clip_rect(input_rect);
        let mut focused = false;
        // 用引事件流的最running（避免陈clone 导致按钮永久禁用
        let running_now = self
            .current_session
            .as_ref()
            .map(|s| s.running)
            .unwrap_or(false);
        // 沙箱切换：Arc clone 避免对 self 的额外捕获（与 TextEdit 借用互不冲突）
        let engine = self.engine.clone();
        let mut sb_mode = engine
            .lock()
            .map(|e| e.sandbox_mode())
            .unwrap_or(SandboxMode::DangerFullAccess);
        input_ui.horizontal(|ui| {
            // 沙箱模式快速切换（仅窗口足够宽时显示，避免窄窗口挤压输入/发送控件挤压出界）
            if ui.available_width() > 260.0 {
                let sb_short = match sb_mode {
                    SandboxMode::DangerFullAccess => "全",
                    SandboxMode::WorkspaceWrite => "写",
                    SandboxMode::ReadOnly => "只读",
                };
                let sb_label = RichText::new(sb_short).size(14.0).color(match sb_mode {
                    SandboxMode::DangerFullAccess => Theme::WARN,
                    SandboxMode::WorkspaceWrite => Theme::OK,
                    SandboxMode::ReadOnly => Theme::CYAN,
                });
                if ui
                    .small_button(sb_label)
                    .on_hover_text(format!("沙箱模式：{}（点击切换）", sb_mode.as_str()))
                    .clicked()
                {
                    sb_mode = match sb_mode {
                        SandboxMode::DangerFullAccess => SandboxMode::WorkspaceWrite,
                        SandboxMode::WorkspaceWrite => SandboxMode::ReadOnly,
                        SandboxMode::ReadOnly => SandboxMode::DangerFullAccess,
                    };
                    if let Ok(mut e) = engine.lock() {
                        e.set_sandbox_mode(sb_mode);
                    }
                }
                ui.add_space(2.0);
            }
            // Enter 发送（Shift+Enter 换行）；回合运行Enter = 插话（引擎侧打断当前 step 续跑
            let entered = ui.input(|i| i.key_pressed(egui::Key::Enter) && !i.modifiers.shift);
            // 宽度自适应：窄时按钮变窄，极窄时隐藏按钮（Enter 发送）
            // 保证发送控件永远不会被挤出面板
            // 按钮统一显示"发送"（无 ➤ 图标），文字在按钮内居中
            let avail_w = ui.available_width();
            let lang = self.lang;
            let (send_w, send_label) = if avail_w < 84.0 {
                (0.0, String::new()) // 极窄：隐藏按钮，仅 Enter 发送
            } else if avail_w < 160.0 {
                (52.0, tr(lang, "发送", "Send")) // 窄：紧凑按钮
            } else {
                (64.0, tr(lang, "发送", "Send")) // 正常
            };
            let edit_w = (avail_w - send_w - 8.0).max(20.0);
            let edit = TextEdit::multiline(&mut self.input)
                .font(egui::FontId::proportional(16.0))
                .hint_text(
                    // 短 hint：窄输入框下 hint 换行会撑高 TextEdit 导致溢出窗口
                    RichText::new(tr(lang, "输入消息…", "Type a message…"))
                        .color(Theme::TEXT_DIM)
                        .size(15.0),
                )
                .desired_width(edit_w)
                .desired_rows(if compact { 1 } else { 2 })
                .text_color(Color32::from_rgb(243, 246, 252))
                .background_color(Theme::BG)
                .vertical_align(egui::Align::Center);
            let edit_resp = ui.add(edit);
            focused = edit_resp.has_focus();
            let mut send_clicked = false;
            if send_w > 0.0 {
                if running_now {
                    // 运行中：发送按钮替换为"停止"按钮（中断当前回合）
                    let stop_label = if send_w < 160.0 {
                        "⏹".to_string()
                    } else {
                        tr(lang, "⏹ 停止", "⏹ Stop")
                    };
                    let stop = ui
                        .add(
                            egui::Button::new(
                                RichText::new(stop_label).size(15.0).color(Color32::WHITE),
                            )
                            .fill(Theme::ERR)
                            .corner_radius(8.0)
                            .min_size(vec2(send_w, if compact { 30.0 } else { 38.0 })),
                        )
                        .on_hover_text(tr(
                            lang,
                            "停止当前回合（不再继续执行工具）",
                            "Stop the current turn",
                        ));
                    if stop.clicked() {
                        let id = session.id.clone();
                        let mut engine = self.engine.lock().unwrap();
                        engine.cancel(&id);
                        self.status = tr(lang, "已请求停止", "Stop requested").into();
                    }
                } else {
                    send_clicked = ui
                        .add_enabled(
                            !self.input.trim().is_empty(),
                            egui::Button::new(
                                RichText::new(send_label).size(15.0).color(Color32::WHITE),
                            )
                            .fill(Theme::ACCENT)
                            .corner_radius(8.0)
                            .min_size(vec2(send_w, if compact { 30.0 } else { 38.0 })),
                        )
                        .clicked();
                }
            }
            if (send_clicked || entered) && !self.input.trim().is_empty() {
                let content = self.input.trim().to_string();
                let id = session.id.clone();
                let mut engine = self.engine.lock().unwrap();
                match engine.send_message(&id, &content) {
                    Ok(()) => {
                        // 发送成功才清空（失败保留草稿）
                        self.input.clear();
                        self.status = tr(self.lang, "已发送", "Sent").into();
                        // 注意：不要在此乐观追加用户消息/事件！
                        // send_message 内部已同步把 user/message 事件推入通道，
                        // 下一帧 pump() 会 push_event + project 投影显示。
                        // 乐观双写是"消息显示两遍"的根因（历史回归）：
                        // 事件通道 + project 无条件追加 → 同一内容渲染两次。
                    }
                    Err(e) => self.error = Some(format!("{e:#}")),
                }
            }
        });
        // 边框：聚焦时强调色高
        let border = if focused {
            Theme::ACCENT
        } else {
            Theme::BORDER
        };
        ui.painter().rect_stroke(
            input_rect,
            10.0,
            egui::Stroke::new(1.0, border),
            egui::StrokeKind::Inside,
        );
        if !self.status.is_empty() {
            input_ui.label(Theme::dim(&self.status));
        }
    }

    fn render_message(
        &mut self,
        ui: &mut egui::Ui,
        msg: &Message,
        salt: u64,
        streaming: bool,
    ) -> egui::Response {
        ui.add_space(6.0);
        let resp = match msg {
            Message::User { content } => {
                // 右对齐：气泡贴右缘。宽度基准 = **clip_rect 宽（视口宽）**——
                // 不能用 available_width()/max_rect().width()：ScrollArea 内容若被
                // 超宽元素（长代码行/大 JSON）撑宽，两者都会变成 3000+，
                // left_space 随之巨大，用户气泡被推到窗口外完全不可见
                // （历史回归："用户消息看不到"，x≈3300 而窗口仅 1100 宽）。
                // clip_rect 是 ScrollArea 视口裁剪矩形，宽度固定不受内容影响。
                let avail_w = ui.clip_rect().width().max(60.0);
                // 长消息限宽 65% 换行（气泡内 label wrap 到该宽度）
                let max_bubble = ((avail_w - 32.0) * 0.65).max(80.0).min(avail_w - 32.0);
                // 真实文本宽度：layout_no_wrap 对多行返回最长行宽。
                // 缓存按内容（弹幕动画期间整窗高频重绘，避免每帧重新排版）。
                let text_w = self
                    .user_width_cache
                    .get(content)
                    .copied()
                    .unwrap_or_else(|| {
                        let w = ui.fonts_mut(|f| {
                            f.layout_no_wrap(
                                content.to_string(),
                                FontId::proportional(14.0),
                                Color32::WHITE,
                            )
                            .size()
                            .x
                        });
                        if self.user_width_cache.len() > 512 {
                            self.user_width_cache.clear();
                        }
                        self.user_width_cache.insert(content.to_string(), w);
                        w
                    });
                let bubble_w = text_w.min(max_bubble).max(30.0);
                // horizontal + add_space：left_space 基于固定视口宽，
                // 不会撑宽父布局（left_space + 气泡宽 ≤ avail_w）
                ui.horizontal(|ui| {
                    let left_space = (avail_w - bubble_w - 24.0 - 8.0).max(4.0);
                    ui.add_space(left_space);
                    let frame = egui::Frame::default()
                        .fill(Theme::USER_BUBBLE)
                        .corner_radius(egui::CornerRadius::same(10))
                        .inner_margin(egui::Margin::symmetric(12, 8));
                    frame.show(ui, |ui| {
                        ui.set_max_width(bubble_w);
                        ui.label(RichText::new(content).size(14.0).color(bubble_text_color()));
                    });
                })
                .response
            }
            Message::Assistant { content, .. } => {
                let avail_w = ui.available_width();
                let max_bubble = (avail_w * 0.95).max(80.0).min(avail_w - 8.0);
                let frame = egui::Frame::default()
                    .fill(Theme::ASSISTANT_BUBBLE)
                    .corner_radius(egui::CornerRadius::same(10))
                    .inner_margin(egui::Margin::symmetric(12, 8));
                // 图片渲染状态（&mut self 字段在闭包外取出，避免借用冲突
                let img_cache = &mut self.img_cache;
                let viewer = &mut self.viewer;
                let http_imgs = &mut self.http_imgs;
                // 相对图片路径基于当前会话工作区解
                let base_dir = self.current_session.as_ref().and_then(|s| s.cwd.clone());
                frame
                    .show(ui, |ui| {
                        ui.set_max_width(max_bubble.max(40.0));
                        // Markdown 纯文本快速路径：走普label（与旧行为一致）。
                        // 流式中（streaming=true）强制走纯文本：流式内容每帧都在
                        // 变，markdown 缓存永远 miss → 每帧全量 parse 累积文本
                        // （严重卡顿 + 消息"闪现一大段"）；等 ASSISTANT_MESSAGE
                        // 完成后才走完整 markdown 渲染。
                        if streaming || !looks_like_markdown(content) {
                            ui.label(RichText::new(content).color(Theme::TEXT));
                        } else {
                            // 解析结果缓存：同一内容只 parse 一次（弹幕动画期间
                            // 整窗高频重绘，重复 parse 是主要 CPU 热点）。
                            let blocks =
                                self.md_parse_cache
                                    .get(content)
                                    .cloned()
                                    .unwrap_or_else(|| {
                                        let parsed = std::sync::Arc::new(
                                            crate::ui::markdown::parse_markdown(content),
                                        );
                                        // 防止无限增长：超限时整体清空（内容变化频繁，
                                        // 简单粗暴但有效；单条消息缓存常驻）
                                        if self.md_parse_cache.len() > 512 {
                                            self.md_parse_cache.clear();
                                        }
                                        self.md_parse_cache
                                            .insert(content.to_string(), parsed.clone());
                                        parsed
                                    });
                            crate::ui::markdown::render_blocks(
                                ui,
                                &blocks,
                                salt,
                                Theme::TEXT,
                                base_dir.as_deref(),
                                img_cache,
                                viewer,
                                http_imgs,
                                max_bubble,
                            );
                        }
                    })
                    .response
            }
            Message::Tool { content, .. } => {
                // 直接 frame.show（左对齐）：不用 with_layout(left_to_right)——
                // 它会推进父布局 x cursor，污染后续消息的对齐。
                let frame = egui::Frame::default()
                    .fill(Theme::BG_HOVER)
                    .stroke(egui::Stroke::new(1.0, Theme::BORDER))
                    .corner_radius(egui::CornerRadius::same(8))
                    .inner_margin(egui::Margin::symmetric(10, 6));
                frame
                    .show(ui, |ui| {
                        ui.set_max_width((ui.available_width() * 0.92).max(80.0));
                        ui.label(
                            RichText::new(format!("🔧 {}", truncate(content, 160)))
                                .color(Theme::TEXT_DIM)
                                .monospace(),
                        );
                    })
                    .response
            }
        };
        resp
    }
}

/// 粗略检测文本是否含 Markdown 语法。
/// 纯文本消息走旧 label 快速路径（渲染结果与历史完全一致，accesskit 文本
/// 也保持原样）；命中 Markdown 特征才进入解析渲染。
fn looks_like_markdown(s: &str) -> bool {
    if s.contains("```") || s.contains("![") || s.contains("\n---") {
        return true;
    }
    let mut has_table_sep = false;
    for line in s.lines() {
        let t = line.trim_start();
        if t.starts_with("# ") || t.starts_with("##") || t.starts_with("###") {
            return true;
        }
        if t.starts_with("- ") || t.starts_with("* ") || t.starts_with("+ ") {
            return true;
        }
        if t.starts_with("> ") || t.starts_with("1. ") || t.starts_with("2. ") {
            return true;
        }
        if t.starts_with('|') && t.contains("---") {
            has_table_sep = true;
        }
    }
    if has_table_sep {
        return true;
    }
    s.contains("**") || s.contains('`') || s.contains("](")
}

/// ask_user 待回答的问题（AI 提问 → UI 渲染选项按钮）。
/// 绑定提问的会话：卡片只在提问会话显示，回答发回提问会话——
/// 否则切到别的会话后卡片仍显示（全局状态），点击会把回答发到
/// 当前会话（历史回归："在 A 会话发送的消息回到了 B 会话"）。
#[derive(Debug, Clone)]
pub struct PendingQuestion {
    /// 提问所属会话（回答发回该会话）
    pub session_id: String,
    pub question: String,
    pub options: Vec<String>,
    pub header: Option<String>,
}

/// 待用户确认的越权写操作（权限审批卡片）。
#[derive(Debug, Clone)]
pub struct PendingApproval {
    pub id: String,
    pub target: String,
    pub reason: String,
}

/// 标题行卡片种类（单选：同一时间只开一张）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CardKind {
    None,
    Plan,
    Goals,
    Subagents,
    Jobs,
}

/// 事件 → 消息投影。
fn project(messages: &mut Vec<Message>, ev: &crate::core::SessionEvent) {
    if ev.r#type == types::ASSISTANT_MESSAGE || ev.r#type == types::USER_MESSAGE {
        if let Some(data) = &ev.data {
            if let Some(content) = data.get("content").and_then(|v| v.as_str()) {
                let msg = if ev.r#type == types::USER_MESSAGE {
                    Message::User {
                        content: content.to_string(),
                    }
                } else {
                    let tool_calls = data
                        .get("tool_calls")
                        .and_then(|v| {
                            serde_json::from_value::<Vec<crate::core::ToolCall>>(v.clone()).ok()
                        })
                        .unwrap_or_default();
                    Message::Assistant {
                        content: content.to_string(),
                        tool_calls,
                    }
                };
                messages.push(msg);
            }
        }
    }
}

/// 把长思考文本切成弹幕句段（每段 <= 24 字符，按标点断句）。
fn split_danmaku(text: &str) -> Vec<String> {
    const MAX: usize = 24;
    let mut out = Vec::new();
    let mut current = String::new();
    for ch in text.chars() {
        current.push(ch);
        let is_break = matches!(
            ch,
            '，' | '。' | '！' | '？' | '；' | ',' | '.' | '!' | '?' | ';' | '\n' | ' '
        );
        if (is_break && current.chars().count() >= 8) || current.chars().count() >= MAX {
            out.push(current.trim().to_string());
            current.clear();
        }
    }
    if !current.trim().is_empty() {
        out.push(current.trim().to_string());
    }
    out
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max).collect();
        format!("{cut}…")
    }
}

/// 工具调用弹幕摘要：显示工具名 + 主要参数值（提取常见主字段），
/// 不展示原始 JSON 花括号；全量内容（完整参数 JSON）供悬浮显示。
fn summarize_tool_call(name: &str, args: &str) -> (String, String) {
    let full = format!("🔧 {name}\n参数: {args}");
    let trimmed = args.trim();
    // 尝试解析参数 JSON，提取第一个有意义的字符串字段
    let mut main: Option<String> = None;
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed) {
        if let Some(obj) = v.as_object() {
            // 优先展示的字段顺序：常见主内容字段
            const PREFERRED: &[&str] = &[
                "content",
                "command",
                "objective",
                "question",
                "prompt",
                "description",
                "text",
                "query",
                "path",
                "message",
                "url",
                "name",
            ];
            for key in PREFERRED {
                if let Some(val) = obj.get(*key) {
                    let s = val.as_str().unwrap_or_default();
                    let s = s.trim();
                    if !s.is_empty() {
                        main = Some(s.to_string());
                        break;
                    }
                }
            }
            // 兜底：任意非空字符串字段（取第一个）
            if main.is_none() {
                for (k, val) in obj {
                    if let Some(s) = val.as_str() {
                        let s = s.trim();
                        if !s.is_empty() {
                            main = Some(format!("{k}: {s}"));
                            break;
                        }
                    }
                }
            }
        } else if let Some(s) = v.as_str() {
            let s = s.trim();
            if !s.is_empty() {
                main = Some(s.to_string());
            }
        }
    }
    match main {
        Some(m) => (format!("🔧 {name} {}", truncate(&m, 40)), full),
        None => (format!("🔧 {name} {}", truncate(trimmed, 40)), full),
    }
}

/// 工具结果弹幕摘要：提取结果里的主要内容（常见字段），
/// 不展示原始 JSON；全量内容（完整结果 JSON）供悬浮显示。
fn summarize_tool_result(ok: bool, value: &str) -> (String, String) {
    let icon = if ok { "✅" } else { "❌" };
    let full = format!("{icon} 结果: {value}");
    let trimmed = value.trim();
    let mut main: Option<String> = None;
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed) {
        if let Some(obj) = v.as_object() {
            // 优先展示的字段顺序
            const PREFERRED: &[&str] = &[
                "content", "stdout", "summary", "note", "message", "result", "output", "text",
                "error",
            ];
            for key in PREFERRED {
                if let Some(val) = obj.get(*key) {
                    if let Some(s) = val.as_str() {
                        let s = s.trim();
                        if !s.is_empty() {
                            // stdout 常带换行/转义，压成单行
                            let one = s.replace('\n', " ").replace('\r', "");
                            main = Some(one);
                            break;
                        }
                    }
                }
            }
            // 兜底：任意非空字符串字段
            if main.is_none() {
                for (k, val) in obj {
                    if let Some(s) = val.as_str() {
                        let s = s.trim();
                        if !s.is_empty() {
                            main = Some(format!("{k}: {}", s.replace('\n', " ")));
                            break;
                        }
                    }
                }
            }
        } else if let Some(s) = v.as_str() {
            let s = s.trim();
            if !s.is_empty() {
                main = Some(s.replace('\n', " "));
            }
        }
    }
    match main {
        Some(m) => (format!("{icon} {}", truncate(&m, 60)), full),
        None => (format!("{icon} {}", truncate(trimmed, 60)), full),
    }
}

/// 按"实际字体宽度"截断文本，超宽部分以 ".." 结尾。
/// 用 egui 字体测量（而非字符数估算），避免 💬/emoji/宽字符导致
/// 文本溢出按钮区域、挤占右侧删除按钮的空间。
fn truncate_to_width(ui: &egui::Ui, s: &str, max_w: f32, font: &egui::FontId) -> String {
    let width = |t: &str| {
        ui.fonts_mut(|f| {
            f.layout_no_wrap(t.to_string(), font.clone(), egui::Color32::WHITE)
                .size()
                .x
        })
    };
    if width(s) <= max_w {
        return s.to_string();
    }
    // 预留 ".."（约 14px）后二分查找最大可容纳字符
    let budget = (max_w - 14.0).max(20.0);
    let mut lo = 1usize;
    let mut hi = s.chars().count();
    while lo < hi {
        let mid = (lo + hi + 1) / 2;
        let cut: String = s.chars().take(mid).collect();
        if width(&cut) <= budget {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    let cut: String = s.chars().take(lo).collect();
    format!("{cut}..")
}

fn bubble_text_color() -> egui::Color32 {
    egui::Color32::from_rgb(245, 245, 250)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn danmaku_splitting() {
        let segs = split_danmaku("首先我们需要分析需求，然后设计架构，最后实现并测试。");
        assert!(segs.len() >= 2);
        for s in &segs {
            assert!(s.chars().count() <= 24, "segment too long: {s}");
            assert!(!s.is_empty());
        }
    }

    #[test]
    fn short_text_single() {
        let segs = split_danmaku("好的");
        assert_eq!(segs, vec!["好的".to_string()]);
    }

    #[test]
    fn tool_call_summary_extracts_main_field() {
        // 工具调用：参数 JSON 只显示主要字段，不露花括号
        let (brief, full) = summarize_tool_call(
            "exec",
            r#"{"command":"dir E:\\AI","cwd":"E:\\AI","timeout":30}"#,
        );
        assert!(brief.contains("🔧 exec"), "brief: {brief}");
        assert!(brief.contains("dir E:\\AI"), "brief 应含主字段: {brief}");
        assert!(!brief.contains('{'), "brief 不应含 JSON 花括号: {brief}");
        assert!(full.contains("command"), "full 应保留完整参数");
        // 常见字段优先级：content 优先于 command
        let (b2, _) = summarize_tool_call("edit", r#"{"content":"修改代码","file":"a.rs"}"#);
        assert!(b2.contains("修改代码"), "b2: {b2}");
        assert!(!b2.contains('{'));
        // 非法 JSON：回退到原文截断
        let (b3, _) = summarize_tool_call("foo", "not json");
        assert!(b3.contains("not json"));
    }

    #[test]
    fn tool_result_summary_extracts_stdout() {
        // 工具结果：value 提取 stdout/内容，不露花括号
        let (brief, full) = summarize_tool_result(
            true,
            r#"{"exit_code":0,"stdout":"Hello world\n第二行","stderr":""}"#,
        );
        assert!(brief.starts_with("✅"), "brief: {brief}");
        assert!(
            brief.contains("Hello world"),
            "brief 应含 stdout 内容: {brief}"
        );
        assert!(!brief.contains('{'), "brief 不应含 JSON 花括号: {brief}");
        assert!(full.contains("exit_code"), "full 应保留完整结果");
        // 失败：❌ 前缀
        let (b2, _) = summarize_tool_result(false, r#"{"error":"boom"}"#);
        assert!(b2.starts_with("❌"));
        assert!(b2.contains("boom"));
        // 非 JSON 字符串
        let (b3, _) = summarize_tool_result(true, "plain text");
        assert!(b3.contains("plain text"));
    }
}
