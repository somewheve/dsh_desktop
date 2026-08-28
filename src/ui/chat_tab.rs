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
    /// 当前会话（引擎 pump/open 更新；渲染用快照见 render_session）
    current_session: Option<Session>,
    /// 渲染快照（current_session 变化时重建；messages 用 Arc 共享，
    /// 避免 30fps 弹幕重绘期间每帧深拷贝全部历史消息）
    render_session: Option<RenderSession>,
    /// current_session 有变化，下一帧重建 render_session
    render_dirty: bool,
    input: String,
    /// 待发送附件（📎 选择后暂存，发送时读取内容注入消息）
    attachments: Vec<std::path::PathBuf>,
    /// 发送模式（插话 / 排队）
    send_mode: SendMode,
    /// 会话搜索输入
    session_search: String,
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
    /// 待确认删除的会话 id（第一次点🗑 进入确认态，再点执行
    pending_delete: Option<String>,
    /// 标题行卡片（单选：同一时间只开一张）
    pub card: CardKind,
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
            render_session: None,
            render_dirty: false,
            input: String::new(),
            attachments: Vec::new(),
            send_mode: SendMode::Interject,
            session_search: String::new(),
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
                // 清掉该会话上次中断回合的流式残留（防"幽灵消息"）
                self.stream_buf.remove(session_id);
                self.render_dirty = true;
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

    /// 每帧处理引擎事件（app 顶层无条件调用——任何标签页都要泵，
    /// 否则后台回合事件积压在无界通道里、状态不更新）
    pub fn pump(&mut self, ui_ctx: &egui::Context) {
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
                    session_id, target, ..
                } => {
                    // 审批卡片改由引擎注册表快照驱动（render_main 每帧读取）：
                    // 旧实现只在"恰好当前会话"时捕获，切走会话/多审批并发时
                    // 卡片丢失，引擎侧死等 300s 超时。这里仅记日志（触发重绘）。
                    info!("approval requested for {session_id}: {target}");
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
                            self.render_dirty = true;
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
                        self.render_dirty = true;
                    }
                    // 回合结束（空闲/停止）：清掉流式缓冲残留——中断/失败的回合
                    // 不会有 ASSISTANT_MESSAGE 来清它，残留文本会在下一回合
                    // 混入流式气泡变成"幽灵消息"
                    if status != crate::core::AgentStatus::Running {
                        self.stream_buf.remove(&session_id);
                    }
                    self.refresh_sessions();
                }
                EngineEvent::Error { message, .. } => {
                    self.error = Some(message);
                }
                EngineEvent::ScheduledTaskDue { id, name } => {
                    // 定时任务到期（后台线程驱动）：状态栏提示（调度面板 M 后续）
                    info!("scheduled task due in UI: {id} ({name})");
                    self.status = format!("⏰ {name}");
                }
            }
        }
        // 引擎事件到达即请求重绘（LLM 静默回合结束不再冻结界面
        if any {
            ui_ctx.request_repaint();
        }
    }

    /// 会话列表（在主侧栏"会话"导航下展开渲染）：＋新建 / 列表
    /// （点击打开、双击重命名、✕ 两级删除确认）/ 运行中 ⚡ 标记。
    /// 从 Chat 中央区抽出——中央区全宽给消息区，列表收纳进左侧导航。
    /// 会话列表（文件树风格，渲染在主侧栏"会话"导航下）：
    /// 紧凑行（26px）、整行点击、hover 整行浅底、选中左侧 2px 强调条、
    /// 删除 ✕ 常驻但极淡（hover 变亮；两级确认）、双击重命名、
    /// 运行中 ⚡ 右缘标记；＋ 新建在列表尾部（树追加语义）。
    pub fn ui_session_list(
        &mut self,
        ui: &mut egui::Ui,
        max_height: Option<f32>,
    ) -> Option<String> {
        const ROW_H: f32 = 26.0;
        let lang = self.lang;
        // 搜索框（有会话时才显示）
        if self.sessions.len() > 3 {
            ui.add(
                egui::TextEdit::singleline(&mut self.session_search)
                    .hint_text(
                        egui::RichText::new(tr(lang, "搜索会话…", "Search…"))
                            .size(10.5)
                            .color(Theme::text_faint()),
                    )
                    .desired_width(ui.available_width().max(60.0))
                    .font(egui::FontId::proportional(10.5)),
            );
            ui.add_space(4.0);
        }
        // 搜索过滤（标题/ID 子串匹配；空 = 全部）
        let search_lower = self.session_search.to_lowercase();
        let sessions: Vec<_> = self
            .sessions
            .iter()
            .filter(|s| {
                search_lower.is_empty()
                    || s.title.to_lowercase().contains(&search_lower)
                    || s.session_id.contains(&search_lower)
            })
            .cloned()
            .collect();
        // 本帧被点击打开/新建的会话（返回给侧栏：非 Chat 页时切回 Chat 页）
        let mut opened: Option<String> = None;
        let mut sa = ScrollArea::vertical().id_salt("session_list_scroll");
        if let Some(h) = max_height {
            sa = sa.max_height(h);
        }
        sa.show(ui, |ui| {
            let mut to_delete: Option<String> = None;
            let mut to_open: Option<String> = None;
            let full_w = ui.available_width().max(60.0);
            for s in &sessions {
                ui.horizontal(|ui| {
                    let selected = self.current.as_deref() == Some(s.session_id.as_str());
                    let row_w = full_w - 8.0; // 两侧留 4px（树缩进感）
                                              // 占位推进布局；交互响应用 interact（allocate 响应的
                                              // widget_info 在 accessibility 树中不上报，interact 稳定）
                    let (alloc, _) =
                        ui.allocate_exact_size(egui::vec2(full_w, ROW_H), egui::Sense::hover());
                    let row = egui::Rect::from_min_size(
                        egui::pos2(alloc.left() + 4.0, alloc.top()),
                        egui::vec2(row_w, ROW_H),
                    );
                    let row_resp = ui.interact(
                        row,
                        ui.id().with(("row", &s.session_id)),
                        egui::Sense::click(),
                    );
                    let del_rect = egui::Rect::from_min_max(
                        egui::pos2(row.right() - ROW_H, row.top()),
                        row.right_bottom(),
                    );
                    let del_resp = ui.interact(
                        del_rect,
                        ui.id().with(("del", &s.session_id)),
                        egui::Sense::click(),
                    );
                    let is_confirm = self.pending_delete.as_deref() == Some(s.session_id.as_str());
                    let row_lit = row_resp.hovered() || selected || is_confirm;

                    // 整行底色：hover / 选中
                    if row_lit {
                        ui.painter().rect_filled(row, 6.0, Theme::bg_hover());
                    }
                    // 选中：左侧 2px 强调条（与主导航一致）
                    if selected {
                        let bar = egui::Rect::from_min_max(
                            egui::pos2(row.left() + 2.0, row.top() + 5.0),
                            egui::pos2(row.left() + 4.0, row.bottom() - 5.0),
                        );
                        ui.painter().rect_filled(bar, 1.0, Theme::accent());
                    }

                    // 双击重命名
                    if row_resp.double_clicked() {
                        self.renaming = Some(s.session_id.clone());
                        self.rename_input = s.title.clone();
                        self.rename_focus_pending = true;
                        to_open = None;
                    }
                    let is_renaming = self.renaming.as_deref() == Some(s.session_id.as_str());
                    if is_renaming {
                        let mut edit = ui.new_child(
                            egui::UiBuilder::new().max_rect(row.shrink2(egui::vec2(6.0, 3.0))),
                        );
                        let resp = edit.add(
                            egui::TextEdit::singleline(&mut self.rename_input)
                                .font(egui::FontId::proportional(11.5))
                                .desired_width(row_w - 16.0),
                        );
                        if self.rename_focus_pending {
                            resp.request_focus();
                            self.rename_focus_pending = false;
                        }
                        let enter_pressed = ui.input(|i| i.key_pressed(egui::Key::Enter));
                        let escape_pressed = ui.input(|i| i.key_pressed(egui::Key::Escape));
                        let lost = resp.lost_focus();
                        let submitted =
                            enter_pressed || (lost && !escape_pressed && !enter_pressed);
                        if submitted || escape_pressed {
                            let id = s.session_id.clone();
                            let name = self.rename_input.trim().to_string();
                            self.renaming = None;
                            if submitted && !name.is_empty() {
                                let mut engine = self.engine.lock().unwrap();
                                match engine.rename_session(&id, &name) {
                                    Ok(()) => {
                                        drop(engine);
                                        self.refresh_sessions();
                                    }
                                    Err(e) => self.error = Some(format!("重命名失败：{e:#}")),
                                }
                            }
                        }
                    } else {
                        // 标题（截断到可用宽；右侧预留 ⚡ 与 ✕ 位）
                        let text_w = row_w - ROW_H - 46.0 - if s.running { 16.0 } else { 0.0 };
                        let text = truncate_to_width(
                            ui,
                            &s.title,
                            text_w.max(30.0),
                            &egui::FontId::proportional(11.5),
                        );
                        let text_color = if selected {
                            Theme::text()
                        } else {
                            Theme::text_dim()
                        };
                        ui.painter().text(
                            egui::pos2(row.left() + 30.0, row.center().y),
                            egui::Align2::LEFT_CENTER,
                            text.clone(),
                            egui::FontId::proportional(11.5),
                            text_color,
                        );
                        row_resp.widget_info(|| {
                            egui::WidgetInfo::labeled(
                                egui::WidgetType::Button,
                                true,
                                format!("{text}{}", if s.running { " \u{26a1}" } else { "" }),
                            )
                        });
                        // 运行中 ⚡（右缘、✕ 左侧）
                        if s.running {
                            ui.painter().text(
                                egui::pos2(del_rect.left() - 10.0, row.center().y),
                                egui::Align2::RIGHT_CENTER,
                                "\u{26a1}",
                                egui::FontId::proportional(10.0),
                                Theme::warn(),
                            );
                        }
                        if row_resp.clicked() {
                            to_open = Some(s.session_id.clone());
                        }
                    }

                    // 删除 ✕：常驻但极淡（hover 行/自身时亮起）；两级确认
                    let (del_label, del_color) = if is_confirm {
                        ("OK", Theme::err())
                    } else if del_resp.hovered() || row_resp.hovered() {
                        ("\u{2715}", Theme::text_dim())
                    } else {
                        ("\u{2715}", Theme::text_faint().gamma_multiply(0.6))
                    };
                    if del_resp.hovered() || is_confirm {
                        ui.painter()
                            .rect_filled(del_rect, 6.0, Theme::bg_elevated());
                    }
                    ui.painter().text(
                        del_rect.center(),
                        egui::Align2::CENTER_CENTER,
                        del_label,
                        egui::FontId::proportional(11.0),
                        del_color,
                    );
                    del_resp.clone().widget_info(|| {
                        egui::WidgetInfo::labeled(egui::WidgetType::Button, true, del_label)
                    });
                    let _ = del_resp.clone().on_hover_text(if is_confirm {
                        tr(lang, "再次点击确认删除", "Click again to confirm")
                    } else {
                        tr(lang, "删除会话", "Delete session")
                    });
                    if del_resp.clicked() {
                        if is_confirm {
                            to_delete = Some(s.session_id.clone());
                        } else {
                            self.pending_delete = Some(s.session_id.clone());
                        }
                    }
                });
            }
            // 删除执行 + 当前会话切换
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
                            self.render_session = None;
                            self.pending_question = None;
                        }
                        self.refresh_sessions();
                        if self.current.is_none() {
                            let first_id = self.sessions.first().map(|s| s.session_id.clone());
                            if let Some(fid) = first_id {
                                self.open(&fid);
                            }
                        }
                    }
                    Err(e) => self.error = Some(format!("删除会话失败：{e:#}")),
                }
            }
            if let Some(id) = to_open {
                self.pending_delete = None;
                self.open(&id);
                opened = Some(id);
            }
        });
        opened
    }

    pub fn ui(&mut self, ui: &mut egui::Ui) {
        // pump：app 顶层已无条件调用（所有标签页统一泵事件）；
        // 此处再泵一次是幂等的（try_recv 清空），兼容直接调 chat.ui() 的
        // 测试与独立使用场景
        self.pump(&ui.ctx().clone());
        if !self.http_imgs.is_empty() {
            crate::ui::markdown::pump_http_images(
                ui.ctx(),
                &mut self.img_cache,
                &mut self.http_imgs,
            );
        }
        // 会话列表收纳在主侧栏（app.sidebar → chat.ui_session_list）：
        // 中央区全宽给弹幕 + 消息 + 输入
        let full = ui.available_size();
        let right_rect = egui::Rect::from_min_size(ui.max_rect().min, full);
        let mut right_ui = ui.new_child(egui::UiBuilder::new().max_rect(right_rect));
        right_ui.set_clip_rect(right_rect);
        {
            // 渲染快照仅在会话状态变化时重建（messages 深拷贝是性能热点：
            // 弹幕动画 30fps 整窗重绘时每帧 clone 整个会话历史不可接受）
            if self.render_dirty {
                self.render_session = self.current_session.as_ref().map(|s| RenderSession {
                    id: s.id.clone(),
                    title: s.title.clone(),
                    running: s.running,
                    preset: s.preset,
                    messages: Arc::new(s.messages.clone()),
                });
                self.render_dirty = false;
            }
            // 拆借用：take 快照渲染后放回（render_main 需 &mut self，
            // 不能同时持有 self.render_session 的不可变借用）
            let render_session = self.render_session.take();
            match &render_session {
                Some(session) => {
                    self.render_main(&mut right_ui, session);
                }
                None => {
                    right_ui.centered_and_justified(|ui| {
                        ui.label(
                            RichText::new(tr(
                                self.lang,
                                "选择左侧会话，或点 ＋ 新建",
                                "Pick a session on the left, or click ＋ to create one",
                            ))
                            .size(12.0)
                            .color(Theme::text_dim()),
                        );
                    });
                }
            }
            self.render_session = render_session;
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
                    .color(Theme::text_dim()),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.small_button(tr(self.lang, "100%", "100%")).clicked() {
                        v.zoom = 1.0;
                        v.offset = egui::Vec2::ZERO;
                    }
                    let _ = ui.label(
                        RichText::new(format!("{:.0}%", v.zoom * 100.0))
                            .size(11.0)
                            .color(Theme::text_dim()),
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
    fn render_main(&mut self, ui: &mut egui::Ui, session: &RenderSession) {
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
            // 标题行按钮：统一紧凑内边距（全局 14px 会让按钮高低胖瘦不均）
            right_ui.spacing_mut().button_padding = egui::vec2(8.0, 4.0);
            right_ui.spacing_mut().item_spacing = egui::vec2(2.0, 0.0);
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
                            Theme::accent_light()
                        } else {
                            Theme::text_dim()
                        }))
                        .fill(if plan_active || self.card == CardKind::Plan {
                            Theme::bg_hover()
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

        // ===== 标题行卡片：浮动层（egui::Window）=====
        // 不挤占消息区布局（旧实现内联渲染，展开后把消息/输入往下推），
        // 吸附在按钮行下方右侧；再次点击按钮或点窗口 ✕ 关闭。
        if self.card != CardKind::None {
            let lang = self.lang;
            let (title, w) = match self.card {
                CardKind::Plan => (tr(lang, "📋 计划", "📋 Plan"), 460.0),
                CardKind::Goals => (tr(lang, "🎯 目标", "🎯 Goals"), 420.0),
                CardKind::Subagents => (tr(lang, "🤖 子代理", "🤖 Subagents"), 440.0),
                CardKind::Jobs => (tr(lang, "⏳ 任务", "⏳ Tasks"), 440.0),
                CardKind::None => unreachable!(),
            };
            let anchor = egui::pos2(
                (row_rect.right() - w).max(row_rect.left()),
                row_rect.bottom() + 6.0,
            );
            let mut open = true;
            egui::Window::new(title)
                .id(egui::Id::new("floating_card"))
                .fixed_pos(anchor)
                .default_size([w, 320.0])
                .resizable(true)
                .collapsible(false)
                .open(&mut open)
                .show(ui.ctx(), |ui| match self.card {
                    CardKind::Plan => self.render_plan_card(ui),
                    CardKind::Goals => self.render_goals_card(ui, session),
                    CardKind::Subagents => self.render_subagents_card(ui),
                    CardKind::Jobs => self.render_jobs_card(ui),
                    CardKind::None => {}
                });
            if !open {
                self.card = CardKind::None;
            }
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
                    Theme::accent_light()
                } else {
                    Theme::text_dim()
                }))
                .fill(if open {
                    Theme::bg_hover()
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
        card_painter.rect_filled(card_rect, 10.0, Theme::bg_elevated());
        card_painter.rect_stroke(
            card_rect,
            10.0,
            egui::Stroke::new(1.0, Theme::border()),
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
                        .color(Theme::accent_light())
                        .strong(),
                );
                let (state_zh, state_en) =
                    if self.plan_mode == crate::engine::plan::PlanMode::Active {
                        ("进行中", "active")
                    } else {
                        ("未启用", "inactive")
                    };
                let color = if self.plan_mode == crate::engine::plan::PlanMode::Active {
                    Theme::ok()
                } else {
                    Theme::text_dim()
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
                            .color(Theme::text_faint()),
                        );
                    } else {
                        let content = self.plan_content.clone();
                        self.render_rich(ui, &content, 7001u64, Theme::text());
                    }
                });
        }
        ui.add_space(4.0);
    }

    /// 目标卡片：fold goal/change 事件；操作经 engine.goal_op 持久化
    fn render_goals_card(&mut self, ui: &mut egui::Ui, session: &RenderSession) {
        let card_h = 170.0;
        let (card_rect, _) =
            ui.allocate_exact_size(vec2(ui.available_width(), card_h), egui::Sense::hover());
        let painter = ui.painter_at(card_rect);
        painter.rect_filled(card_rect, 10.0, Theme::bg_elevated());
        painter.rect_stroke(
            card_rect,
            10.0,
            egui::Stroke::new(1.0, Theme::border()),
            egui::StrokeKind::Inside,
        );
        let mut card_ui =
            ui.new_child(egui::UiBuilder::new().max_rect(card_rect.shrink2(egui::vec2(10.0, 8.0))));
        card_ui.set_clip_rect(card_rect);
        card_ui.horizontal(|ui| {
            ui.label(
                RichText::new(tr(self.lang, "🎯 目标", "🎯 Goals"))
                    .color(Theme::accent_light())
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
                    Theme::ok()
                } else {
                    Theme::text_dim()
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
                            .color(Theme::text_faint()),
                    )
                    .desired_width(w)
                    .text_color(Theme::text()),
            );
            let clicked = ui
                .add(egui::Button::new(
                    RichText::new(tr(lang, "创建", "Create")).color(Theme::accent_light()),
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
                        crate::engine::goal::GoalPhase::Active => Theme::ok(),
                        crate::engine::goal::GoalPhase::Paused => Theme::warn(),
                        crate::engine::goal::GoalPhase::Blocked => Theme::err(),
                        crate::engine::goal::GoalPhase::Complete => Theme::text_faint(),
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
        painter.rect_filled(card_rect, 10.0, Theme::bg_elevated());
        painter.rect_stroke(
            card_rect,
            10.0,
            egui::Stroke::new(1.0, Theme::border()),
            egui::StrokeKind::Inside,
        );
        let mut card_ui =
            ui.new_child(egui::UiBuilder::new().max_rect(card_rect.shrink2(egui::vec2(10.0, 8.0))));
        card_ui.set_clip_rect(card_rect);
        card_ui.horizontal(|ui| {
            ui.label(
                RichText::new(tr(self.lang, "🤖 子代理", "🤖 Subagents"))
                    .color(Theme::accent_light())
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
                        "done" => Theme::ok(),
                        "failed" => Theme::err(),
                        "running" => Theme::cyan(),
                        _ => Theme::text_dim(),
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
                                .color(Theme::text_dim()),
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
        painter.rect_filled(card_rect, 10.0, Theme::bg_elevated());
        painter.rect_stroke(
            card_rect,
            10.0,
            egui::Stroke::new(1.0, Theme::border()),
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
                    .color(Theme::accent_light())
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
                        crate::engine::jobs::JobStatus::Running => Theme::cyan(),
                        crate::engine::jobs::JobStatus::Done => Theme::ok(),
                        crate::engine::jobs::JobStatus::Failed => Theme::err(),
                        crate::engine::jobs::JobStatus::Pending => Theme::text_dim(),
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
                                .color(Theme::text_dim()),
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
    fn render_body_bottom(&mut self, ui: &mut egui::Ui, session: &RenderSession) {
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
                                .color(Theme::text_faint()),
                        )
                        .desired_width(w)
                        .text_color(Theme::text()),
                );
                let browse = ui
                    .add(egui::Button::new(
                        RichText::new(tr(lang, "📂 浏览…", "📂 Browse…")).color(Theme::text_dim()),
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
                        RichText::new(tr(lang, "打开", "Open")).color(Theme::accent_light()),
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
            egui::Stroke::new(1.0, Theme::border()),
        );

        // 弹幕标题
        let painter2 = ui.painter_at(dm_rect);
        painter2.text(
            egui::pos2(dm_rect.left() + 10.0, dm_rect.top() + 4.0),
            egui::Align2::LEFT_TOP,
            tr(self.lang, "🎯 AI 思考", "🎯 AI thinking"),
            FontId::proportional(11.0),
            Theme::text_faint(),
        );
        // 空态提示：无弹幕时补一行弱提示，避免"黑洞块"观感
        if dm_height > 0.0 && self.danmaku.is_empty() {
            let hint = tr(
                self.lang,
                "思考过程将在这里飘过…",
                "Thinking will float by here…",
            );
            painter2.text(
                egui::pos2(dm_rect.left() + 90.0, dm_rect.top() + 4.0),
                egui::Align2::LEFT_TOP,
                hint,
                FontId::proportional(11.0),
                Theme::text_dim(),
            );
        }

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
        // intersect max_rect：极矮窗下前面区域可能把 cursor 推出界，防止底部越窗
        let full = ui.available_rect_before_wrap().intersect(ui.max_rect());
        // 输入区高度三档：96（正常）/ 80（矮窗）/ 64（超矮窗，单行紧凑模式）
        let input_h = if full.height() < 140.0 {
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
                    self.last_user_msg_rect = None;
                    for (idx, msg) in session.messages.iter().enumerate() {
                        let resp = self.render_message(ui, msg, idx as u64, false);
                        last_resp = Some(resp.clone());
                        if matches!(msg, Message::User { .. }) {
                            self.last_user_msg_rect = Some(resp.rect);
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
                    // 权限审批卡片（与 ask_user 选择卡片一致：消息区可点击）。
                    // 队列来自引擎审批注册表快照——跨会话的审批也可见
                    // （旧实现只在"恰好当前会话收到事件"时渲染，切走后卡片
                    // 丢失、引擎侧死等 300s 超时）
                    let pending_approvals: Vec<
                        crate::engine::approval::ApprovalRequest,
                    > = self
                        .engine
                        .lock()
                        .map(|e| e.pending_approvals())
                        .unwrap_or_default();
                    if let Some(a) = pending_approvals.first() {
                        ui.add_space(6.0);
                        let card = egui::Frame::default()
                            .fill(Theme::bg_elevated())
                            .stroke(egui::Stroke::new(1.0, Theme::warn()))
                            .corner_radius(egui::CornerRadius::same(10))
                            .inner_margin(egui::Margin::same(12));
                        let cr = card.show(ui, |ui| {
                            ui.label(
                                RichText::new(tr(
                                    self.lang,
                                    "🔐 需要授权：写工作区外",
                                    "🔐 Authorization needed: outside workspace",
                                ))
                                .color(Theme::warn())
                                .strong(),
                            );
                            if a.session_id != session.id {
                                ui.label(
                                    RichText::new(tr(
                                        self.lang,
                                        &format!("（来自会话 {}）", a.session_id),
                                        &format!("(from session {})", a.session_id),
                                    ))
                                    .size(11.0)
                                    .color(Theme::text_dim()),
                                );
                            }
                            ui.add_space(4.0);
                            ui.label(Theme::dim(&a.reason));
                            ui.add_space(2.0);
                            ui.label(Theme::dim(a.target.as_str()));
                            if pending_approvals.len() > 1 {
                                ui.add_space(2.0);
                                ui.label(
                                    RichText::new(tr(
                                        self.lang,
                                        &format!("另有 {} 个待审批请求排队中", pending_approvals.len() - 1),
                                        &format!("{} more approvals queued", pending_approvals.len() - 1),
                                    ))
                                    .size(11.0)
                                    .color(Theme::text_dim()),
                                );
                            }
                            ui.add_space(6.0);
                            ui.horizontal_wrapped(|ui| {
                                let opts = [
                                    ("A  允许本次", crate::engine::approval::ApprovalDecision::Allow, Theme::ok()),
                                    ("B  拒绝", crate::engine::approval::ApprovalDecision::Deny, Theme::err()),
                                    ("C  总是允许", crate::engine::approval::ApprovalDecision::AlwaysAllow, Theme::accent()),
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
                                .fill(Theme::bg_elevated())
                                .stroke(egui::Stroke::new(1.0, Theme::accent()))
                                .corner_radius(egui::CornerRadius::same(10))
                                .inner_margin(egui::Margin::same(12));
                            let cr = card.show(ui, |ui| {
                                ui.label(
                                    RichText::new(tr(
                                        self.lang,
                                    "❓ 需要你的选择",
                                    "❓ Your input needed",
                                    ))
                                    .color(Theme::accent_light())
                                    .strong(),
                                );
                                if let Some(h) = &q.header {
                                    ui.add_space(2.0);
                                    ui.label(Theme::dim(h));
                                }
                                ui.add_space(4.0);
                                ui.label(RichText::new(&q.question).color(Theme::text()));
                                if !q.options.is_empty() {
                                    ui.add_space(6.0);
                                    ui.horizontal_wrapped(|ui| {
                                        for opt in &q.options {
                                            let btn = ui.add(
                                                egui::Button::new(
                                                    RichText::new(opt.clone())
                                                        .color(Color32::WHITE),
                                                )
                                                .fill(Theme::accent())
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
                                .color(Theme::text_faint()),
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
            // 处理权限审批动作（用户点了 A/B/C → 回传引擎注册表）
            if let Some((aid, decision)) = approval_action {
                let engine = self.engine.clone();
                let resolved = engine.lock().unwrap().resolve_approval(&aid, decision);
                info!("approval {aid} resolved={resolved} {:?}", decision);
            }
        }
    }

    /// 输入区：Enter 发送（16px 亮色字体 + 圆角容器 + 焦点高亮；矩形由调用方钉底）
    /// 输入区（ZCode 风格）：一个 elevated 圆角容器 = 透明多行输入（上）+
    /// 底部工具行（左：模型/思考深度/权限 chips，右：发送按钮）。
    /// 快捷切换就地完成，不挤占消息区。
    fn render_input(&mut self, ui: &mut egui::Ui, session: &RenderSession, input_rect: egui::Rect) {
        let lang = self.lang;
        let compact = input_rect.height() < 78.0;
        let running_now = session.running;

        // 容器底色先铺（后画会盖住工具行 chips——历史 bug）
        ui.painter()
            .rect_filled(input_rect, 12.0, Theme::bg_elevated());
        let inner = input_rect.shrink2(vec2(12.0, if compact { 6.0 } else { 10.0 }));
        let bar_h = if compact { 24.0 } else { 30.0 };
        let chip_h = if compact { 22.0 } else { 26.0 };
        // 附件条（有附件时占据编辑区顶部一行）
        let attach_h = if self.attachments.is_empty() {
            0.0
        } else {
            26.0
        };
        let edit_rect = egui::Rect::from_min_max(
            egui::pos2(inner.left(), inner.top() + attach_h),
            egui::pos2(inner.right(), inner.bottom() - bar_h - 6.0),
        );
        if attach_h > 0.0 {
            let att_rect = egui::Rect::from_min_max(
                inner.min,
                egui::pos2(inner.right(), inner.top() + attach_h),
            );
            let mut att_ui = ui.new_child(egui::UiBuilder::new().max_rect(att_rect));
            att_ui.set_clip_rect(att_rect);
            att_ui.spacing_mut().button_padding = egui::vec2(6.0, 3.0);
            att_ui.horizontal(|ui| {
                let mut remove: Option<usize> = None;
                for (i, path) in self.attachments.iter().enumerate() {
                    let name = path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    let short: String = name.chars().take(18).collect();
                    let text = egui::RichText::new(format!("\u{1F4CE} {short} \u{2715}"))
                        .size(10.5)
                        .color(Theme::text_dim());
                    if ui
                        .add(
                            egui::Button::new(text)
                                .fill(egui::Color32::TRANSPARENT)
                                .stroke(egui::Stroke::NONE)
                                .corner_radius(8.0)
                                .min_size(egui::vec2(0.0, 20.0)),
                        )
                        .on_hover_text(path.display().to_string())
                        .clicked()
                    {
                        remove = Some(i);
                    }
                }
                if let Some(i) = remove {
                    self.attachments.remove(i);
                }
            });
        }
        let bar_rect = egui::Rect::from_min_max(
            egui::pos2(inner.left(), inner.bottom() - bar_h),
            inner.right_bottom(),
        );

        // ===== 上：多行输入（透明背景，容器即视觉框） =====
        let enter_pressed = ui.input(|i| i.key_pressed(egui::Key::Enter) && !i.modifiers.shift);
        let mut edit_ui = ui.new_child(egui::UiBuilder::new().max_rect(edit_rect));
        edit_ui.set_clip_rect(edit_rect);
        let input_rows = if compact {
            1
        } else {
            self.input.lines().count().clamp(2, 6)
        };
        let edit = TextEdit::multiline(&mut self.input)
            .font(egui::FontId::proportional(12.0))
            .hint_text(
                RichText::new(tr(lang, "描述任务…", "Describe the task…"))
                    .color(Theme::text_faint())
                    .size(13.0),
            )
            .desired_width(f32::INFINITY)
            .desired_rows(input_rows)
            .text_color(Theme::text())
            .background_color(egui::Color32::TRANSPARENT)
            .frame(egui::Frame::NONE)
            .vertical_align(egui::Align::Center);
        let edit_resp = edit_ui.add(edit);
        let focused = edit_resp.has_focus();
        // 仅聊天框自身持有（或刚失去）焦点时的 Enter 才发送（防止其它
        // 输入框里按 Enter 误发草稿）
        let entered = enter_pressed && (edit_resp.has_focus() || edit_resp.lost_focus());

        // ===== 下：工具行（chips + 发送） =====
        let mut bar_ui = ui.new_child(egui::UiBuilder::new().max_rect(bar_rect));
        bar_ui.set_clip_rect(bar_rect);
        // 紧凑均匀的 chip 间隙（默认 8px 配上 chip 自身内边距显得松散不齐）
        bar_ui.spacing_mut().item_spacing = egui::vec2(4.0, 0.0);
        let mut send_clicked = false;
        bar_ui.horizontal(|ui| {
            // —— 左：📎 附件 + 等高图标 chips（⊞ 模型 / ✦ 思考 / 🛡 权限 / ◇ 模式）——
            {
                let n_att = self.attachments.len();
                let label = if n_att > 0 {
                    format!("\u{1F4CE} {n_att}")
                } else {
                    "\u{1F4CE}".to_string()
                };
                if chip_button(
                    ui,
                    &label,
                    Theme::text_dim(),
                    chip_h,
                    &tr(
                        lang,
                        "附加文件（发送时内容注入消息；文本全文，超大截断）",
                        "Attach files (contents injected into the message)",
                    ),
                ) {
                    if let Some(paths) = rfd::FileDialog::new()
                        .set_title(tr(lang, "选择文件", "Pick files"))
                        .pick_files()
                    {
                        self.attachments.extend(paths);
                    }
                }
            }
            // 发送模式 chip（⚡插话 / ⏳排队）：点击切换
            {
                let (icon, tip) = match self.send_mode {
                    SendMode::Interject => (
                        "\u{26A1}",
                        tr(
                            lang,
                            "插话模式：回合运行中发送会打断当前步骤并立即处理",
                            "Interject: interrupts the running turn",
                        ),
                    ),
                    SendMode::Queue => (
                        "\u{23F3}",
                        tr(
                            lang,
                            "排队模式：回合运行中发送会排队，当前回合结束后依次执行",
                            "Queue: runs after the current turn finishes (FIFO)",
                        ),
                    ),
                };
                let qn = self
                    .engine
                    .lock()
                    .map(|e| e.queued_count(&session.id))
                    .unwrap_or(0);
                let label = if qn > 0 {
                    format!("{icon} {qn}")
                } else {
                    icon.to_string()
                };
                if chip_button(
                    ui,
                    &label,
                    if self.send_mode == SendMode::Queue {
                        Theme::accent_light()
                    } else {
                        Theme::text_dim()
                    },
                    chip_h,
                    &tip,
                ) {
                    self.send_mode = match self.send_mode {
                        SendMode::Interject => SendMode::Queue,
                        SendMode::Queue => SendMode::Interject,
                    };
                }
            }
            let mut model_cur = None;
            if ui.available_width() > 150.0 {
                let current = self.engine.lock().unwrap().effective_model(&session.id);
                let short = current
                    .strip_prefix("deepseek-")
                    .unwrap_or(&current)
                    .to_string();
                let opts: Vec<(String, String)> = crate::core::llm::DEEPSEEK_MODELS
                    .iter()
                    .map(|m| (m.to_string(), m.to_string()))
                    .collect();
                let opt_refs: Vec<(&str, &str)> =
                    opts.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
                if let Some(v) = ui_icon_chip(
                    ui,
                    "model",
                    "\u{229E}",
                    &short,
                    &opt_refs,
                    &format!("{current}\n（点击切换模型，下一回合生效）"),
                    chip_h,
                ) {
                    model_cur = Some(v);
                }
            }
            if let Some(v) = model_cur {
                self.engine
                    .lock()
                    .unwrap()
                    .set_session_model(&session.id, &v);
                self.status = tr(lang, "模型已切换", "Model switched").into();
            }
            // 思考深度
            let mut effort_cur = None;
            if ui.available_width() > 110.0 {
                let current = self.engine.lock().unwrap().effective_effort(&session.id);
                let label_of = |e: &str| -> String {
                    match e {
                        "none" => "\u{2726} 关".into(),
                        "low" => "\u{2726} 低".into(),
                        "high" => "\u{2726} 高".into(),
                        _ => "\u{2726} 最高".into(),
                    }
                };
                let short_cur = match current.as_str() {
                    "none" => "关".to_string(),
                    "low" => "低".to_string(),
                    "high" => "高".to_string(),
                    _ => "最高".to_string(),
                };
                let opts: Vec<(String, String)> = crate::core::llm::REASONING_EFFORTS
                    .iter()
                    .map(|e| (label_of(e), e.to_string()))
                    .collect();
                let opt_refs: Vec<(&str, &str)> =
                    opts.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
                if let Some(v) = ui_icon_chip(
                    ui,
                    "effort",
                    "\u{2726}",
                    &short_cur,
                    &opt_refs,
                    "思考深度（reasoning_effort，下一回合生效）",
                    chip_h,
                ) {
                    effort_cur = Some(v);
                }
            }
            if let Some(v) = effort_cur {
                self.engine
                    .lock()
                    .unwrap()
                    .set_session_effort(&session.id, &v);
                self.status = tr(lang, "思考深度已切换", "Thinking effort switched").into();
            }
            // 权限（沙箱）
            let mut sb_cur: Option<SandboxMode> = None;
            {
                let current = self
                    .engine
                    .lock()
                    .map(|e| e.effective_sandbox(&session.id))
                    .unwrap_or(SandboxMode::DangerFullAccess);
                let name_of = |m: SandboxMode| -> String {
                    match m {
                        SandboxMode::DangerFullAccess => "全访问".into(),
                        SandboxMode::WorkspaceWrite => "工作区可写".into(),
                        SandboxMode::ReadOnly => "只读".into(),
                    }
                };
                let short_of = |m: SandboxMode| -> String {
                    match m {
                        SandboxMode::DangerFullAccess => "全".into(),
                        SandboxMode::WorkspaceWrite => "写".into(),
                        SandboxMode::ReadOnly => "只读".into(),
                    }
                };
                let all = [
                    SandboxMode::DangerFullAccess,
                    SandboxMode::WorkspaceWrite,
                    SandboxMode::ReadOnly,
                ];
                let opts: Vec<(String, SandboxMode)> =
                    all.iter().map(|m| (name_of(*m), *m)).collect();
                let opt_refs: Vec<(&str, &str)> =
                    opts.iter().map(|(n, m)| (n.as_str(), m.as_str())).collect();
                if let Some(v) = ui_icon_chip(
                    ui,
                    "sandbox",
                    "\u{1F6E1}",
                    &short_of(current),
                    &opt_refs,
                    "权限/沙箱：bash/pwsh 与写操作的执行边界",
                    chip_h,
                ) {
                    if let Some(m) = SandboxMode::parse(&v) {
                        sb_cur = Some(m);
                    }
                }
            }
            if let Some(m) = sb_cur {
                self.engine
                    .lock()
                    .unwrap()
                    .set_session_sandbox(&session.id, m);
                self.status = tr(lang, "权限已切换", "Permissions switched").into();
            }
            // 模式（Agent 预设，会话级）
            let mut preset_cur: Option<AgentPreset> = None;
            if ui.available_width() > 130.0 {
                let current = session.preset;
                let opts: Vec<(String, AgentPreset)> = AgentPreset::all()
                    .iter()
                    .map(|p| (p.name().to_string(), *p))
                    .collect();
                let opt_refs: Vec<(&str, &str)> =
                    opts.iter().map(|(n, p)| (n.as_str(), p.id())).collect();
                if let Some(v) = ui_icon_chip(
                    ui,
                    "preset",
                    "\u{25C7}",
                    current.name(),
                    &opt_refs,
                    "Agent 模式（会话级，下一回合生效）",
                    chip_h,
                ) {
                    if let Some(p) = AgentPreset::parse(&v) {
                        preset_cur = Some(p);
                    }
                }
            }
            if let Some(p) = preset_cur {
                let id = session.id.clone();
                let mut engine = self.engine.lock().unwrap();
                match engine.set_session_preset(&id, p) {
                    Ok(()) => {
                        drop(engine);
                        if let Some(s) = &mut self.current_session {
                            s.preset = p;
                        }
                        self.render_dirty = true;
                        self.status =
                            format!("{} {}", tr(lang, "已切换模式", "Mode switched"), p.name());
                    }
                    Err(e) => self.error = Some(format!("切换模式失败：{e:#}")),
                }
            }
            // 右侧：状态反馈 + 发送/停止（胶囊）。
            // 显式分配剩余宽度做右对齐子 ui——`ui.with_layout(right_to_left)`
            // 的子 ui 继承父 max_rect，胶囊会被推到容器右缘之外被裁
            // （实机：发送按钮半截悬在输入框外）。
            let rem = ui.available_size();
            let right_rect = egui::Rect::from_min_size(
                ui.cursor().min,
                egui::vec2(rem.x.max(70.0), bar_rect.height()),
            )
            .intersect(bar_rect);
            let (alloc_rect, _) = ui.allocate_exact_size(right_rect.size(), egui::Sense::hover());
            let mut right_ui = ui.new_child(
                egui::UiBuilder::new()
                    .max_rect(alloc_rect)
                    .layout(egui::Layout::right_to_left(egui::Align::Center)),
            );
            right_ui.set_clip_rect(alloc_rect);
            {
                let ui = &mut right_ui;
                if running_now {
                    let stop_label = if ui.available_width() < 120.0 {
                        "⏹".to_string()
                    } else {
                        tr(lang, "⏹ 停止", "⏹ Stop")
                    };
                    let stop = ui
                        .add(
                            egui::Button::new(
                                RichText::new(stop_label).size(13.0).color(Color32::WHITE),
                            )
                            .fill(Theme::err())
                            .corner_radius(if compact { 11.0 } else { 14.0 })
                            .min_size(vec2(
                                if compact { 52.0 } else { 64.0 },
                                if compact { 22.0 } else { 28.0 },
                            )),
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
                    let send_label = if ui.available_width() < 120.0 {
                        "➤".to_string()
                    } else {
                        tr(lang, "发送", "Send")
                    };
                    send_clicked = ui
                        .add_enabled(
                            !self.input.trim().is_empty(),
                            egui::Button::new(
                                RichText::new(send_label).size(13.0).color(Color32::WHITE),
                            )
                            .fill(Theme::accent())
                            .corner_radius(if compact { 11.0 } else { 14.0 })
                            .min_size(vec2(
                                if compact { 52.0 } else { 64.0 },
                                if compact { 22.0 } else { 28.0 },
                            )),
                        )
                        .clicked();
                }
                if !self.status.is_empty() {
                    ui.label(Theme::dim(&self.status));
                }
            }
        });

        // ===== 发送 =====
        if (send_clicked || entered)
            && (!self.input.trim().is_empty() || !self.attachments.is_empty())
        {
            // 附件内容注入消息文本（持久化/重放天然包含，零引擎改动）
            let base = self.input.trim().to_string();
            let content = compose_message_with_attachments(&base, &self.attachments);
            if content.is_empty() {
                self.status = tr(lang, "消息为空", "Empty message").into();
            } else {
                let id = session.id.clone();
                let mut engine = self.engine.lock().unwrap();
                // 排队模式 + 回合运行中 → 入队不打断（回合结束由 pump 逐条发出）
                let running = {
                    let shared = engine.shared_sessions();
                    shared.get(id.as_str()).map(|x| x.running).unwrap_or(false)
                };
                if running && self.send_mode == SendMode::Queue {
                    let pos = engine.enqueue_message(&id, &content);
                    self.input.clear();
                    self.attachments.clear();
                    self.status = tr(
                        lang,
                        &format!("已排队（第 {pos} 位，当前回合结束后执行）"),
                        &format!("Queued (#{pos}, runs after current turn)"),
                    );
                } else {
                    match engine.send_message(&id, &content) {
                        Ok(()) => {
                            // 发送成功才清空（失败保留草稿与附件）。不要乐观追加
                            // 用户消息——send_message 已推事件，双写 = 显示两遍。
                            self.input.clear();
                            self.attachments.clear();
                            self.status = tr(lang, "已发送", "Sent").into();
                        }
                        Err(e) => self.error = Some(format!("{e:#}")),
                    }
                }
            }
        }

        // 边框最后画（细线不遮内容）；底色已在开头铺（末尾填充会盖住 chips）
        let border = if focused {
            Theme::accent()
        } else {
            Theme::border()
        };
        ui.painter().rect_stroke(
            input_rect,
            12.0,
            egui::Stroke::new(1.0, border),
            egui::StrokeKind::Inside,
        );
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
                                FontId::proportional(12.0),
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
                        .fill(Theme::user_bubble())
                        .corner_radius(egui::CornerRadius::same(10))
                        .inner_margin(egui::Margin::symmetric(12, 8));
                    frame.show(ui, |ui| {
                        ui.set_max_width(bubble_w);
                        ui.label(RichText::new(content).size(12.0).color(bubble_text_color()));
                    });
                })
                .response
            }
            Message::Assistant { content, .. } => {
                // 纯工具调用消息（content 为空）：不渲染空气泡（历史视觉缺陷：
                // 空白框 + 头像孤零零占一行）
                if content.trim().is_empty() {
                    return ui.allocate_response(egui::vec2(0.0, 0.0), egui::Sense::hover());
                }
                let avail_w = ui.available_width();
                let max_bubble = (avail_w * 0.95).max(80.0).min(avail_w - 8.0);
                // AI 消息无气泡（ZCode/ChatGPT 风格）：深色主题下浅灰气泡底
                // 是大面积高对比色块（"丑"的主要来源）；纯文本直接落在背景上
                let frame = egui::Frame::default()
                    .fill(egui::Color32::TRANSPARENT)
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
                            ui.label(RichText::new(content).size(12.0).color(Theme::text()));
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
                                Theme::text(),
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
                // 摘要提取 stdout/content 等主字段（复用弹幕摘要逻辑），
                // 不展示原始 JSON 转义串；全量内容悬浮可看。
                let (brief, full) = summarize_tool_result(true, content);
                let frame = egui::Frame::default()
                    .fill(Theme::bg_hover())
                    .stroke(egui::Stroke::new(1.0, Theme::border()))
                    .corner_radius(egui::CornerRadius::same(8))
                    .inner_margin(egui::Margin::symmetric(10, 6));
                let resp = frame
                    .show(ui, |ui| {
                        ui.set_max_width((ui.available_width() * 0.92).max(80.0));
                        ui.label(
                            RichText::new(truncate(&brief, 160))
                                .color(Theme::text_dim())
                                .monospace(),
                        )
                    })
                    .response;
                resp.on_hover_text(full)
            }
        };
        resp
    }
}

/// 附件内容注入上限（单文件）：超出部分截断并说明。
const ATTACH_MAX_BYTES: u64 = 512 * 1024;

/// 文本类扩展名白名单（内容直接注入）。
const ATTACH_TEXT_EXTS: &[&str] = &[
    "txt",
    "md",
    "markdown",
    "rst",
    "log",
    "csv",
    "tsv",
    "json",
    "jsonl",
    "yaml",
    "yml",
    "toml",
    "ini",
    "cfg",
    "conf",
    "xml",
    "html",
    "htm",
    "css",
    "js",
    "mjs",
    "cjs",
    "jsx",
    "ts",
    "tsx",
    "py",
    "pyi",
    "rb",
    "go",
    "rs",
    "java",
    "kt",
    "kts",
    "c",
    "h",
    "cpp",
    "hpp",
    "cc",
    "cs",
    "swift",
    "m",
    "mm",
    "php",
    "sh",
    "bash",
    "zsh",
    "fish",
    "ps1",
    "bat",
    "cmd",
    "sql",
    " graphql",
    "proto",
    "gradle",
    "properties",
    "env",
    "gitignore",
    "dockerignore",
    "dockerfile",
    "makefile",
    "cmake",
    "lock",
];

/// 判定文件是否按文本注入（扩展名白名单 + 无扩展名时 UTF-8 探测）。
fn attach_is_text(path: &std::path::Path) -> bool {
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    if ext.is_empty() {
        // 无扩展名：读前 4KB 探测 UTF-8 有效性
        return match std::fs::read(path) {
            Ok(bytes) => {
                let probe = &bytes[..bytes.len().min(4096)];
                String::from_utf8_lossy(probe)
                    .chars()
                    .any(|c| !c.is_control() || c == '\n' || c == '\r' || c == '\t')
                    && std::str::from_utf8(probe).is_ok()
            }
            Err(_) => false,
        };
    }
    // 无扩展名文件名本身（Makefile/Dockerfile 等）
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    if matches!(
        name.as_str(),
        "makefile" | "dockerfile" | "license" | "readme"
    ) {
        return true;
    }
    ATTACH_TEXT_EXTS.contains(&ext.as_str())
}

/// 组装用户消息：正文 + 附件内容注入。
/// 文本类：全文注入（超 ATTACH_MAX_BYTES 截断）；二进制/读取失败：占位说明
/// （路径可见，AI 可用工具自行处理）。空正文 + 有附件 = 纯附件消息。
pub fn compose_message_with_attachments(base: &str, attachments: &[std::path::PathBuf]) -> String {
    if attachments.is_empty() {
        return base.to_string();
    }
    let mut out = base.to_string();
    if !out.is_empty() {
        out.push_str("\n\n");
    }
    for (i, path) in attachments.iter().enumerate() {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        out.push_str(&format!(
            "---\n【附件 {}/{}：{name}】（{}）\n",
            i + 1,
            attachments.len(),
            path.display()
        ));
        if !attach_is_text(path) {
            out.push_str(&format!(
                "（二进制/非文本文件未注入内容；如需处理请用工具按路径读取）\n"
            ));
            continue;
        }
        match std::fs::read(path) {
            Ok(bytes) => {
                let truncated = bytes.len() as u64 > ATTACH_MAX_BYTES;
                let slice = &bytes[..bytes.len().min(ATTACH_MAX_BYTES as usize)];
                let text = String::from_utf8_lossy(slice);
                let fence = if text.contains("```") { "~~~" } else { "```" };
                out.push_str(&format!("{fence}\n{text}\n{fence}\n"));
                if truncated {
                    out.push_str(&format!(
                        "（文件过大，仅注入前 {} KB，完整内容请用工具按路径读取）\n",
                        ATTACH_MAX_BYTES / 1024
                    ));
                }
            }
            Err(e) => out.push_str(&format!("（读取失败：{e}）\n")),
        }
    }
    out
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

/// 等高图标 chip（输入框工具行用）：ComboBox + 统一 11.5px 字号 + 图标前缀
/// （同字号 = 等高；Popup::menu 会把锚定按钮从 accessibility 树隐藏，故不用）。
/// 返回用户新选择的值（未选择返回 None）。
/// 等高图标按钮芯片：手动绘制（悬停底色 + 垂直水平居中文字）。
/// 不走全局 Button（button_padding=14px 会让纯图标按钮宽成胶囊，
/// 与选择器 chip 的紧凑节奏不一致）。
fn chip_button(ui: &mut egui::Ui, label: &str, color: egui::Color32, h: f32, tip: &str) -> bool {
    let font = egui::FontId::proportional(11.5);
    let galley = ui.painter().layout_no_wrap(label.to_string(), font, color);
    let w = (galley.size().x + 16.0).max(30.0);
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(w, h), egui::Sense::click());
    let resp = resp.on_hover_text(tip);
    if resp.hovered() || resp.highlighted() {
        ui.painter().rect_filled(rect, 8.0, Theme::bg_hover());
    }
    let pos = egui::pos2(
        rect.center().x - galley.size().x / 2.0,
        rect.center().y - galley.size().y / 2.0,
    );
    ui.painter().galley(pos, galley, color);
    resp.clicked()
}

fn ui_icon_chip(
    ui: &mut egui::Ui,
    id: &str,
    icon: &str,
    value: &str,
    options: &[(&str, &str)], // (显示文本, 值)
    hover: &str,
    h: f32,
) -> Option<String> {
    let mut picked: Option<String> = None;
    // 视觉层完全手绘（rect 自己分配、边框自己画）：不依赖 ComboBox 内部按钮
    // 的自然宽度——它含内边距+箭头+外边距且随版本变化，预估稍小就会被
    // 外层 clip 矩形吃掉右缘圆角（实机缺陷：边框"不闭合"）。
    // 下拉改用 egui::Popup（行为同 ComboBox：点击选择 / 点击外部关闭）。
    // 无障碍/点击：在 rect 内放一个透明真实 Button（文字透明、无填充），
    // 注册 a11y 节点（kittest/UI 测试按 value 查询）并承载交互。
    let text = format!("{icon} {value} \u{25BE}");
    let font = egui::FontId::proportional(11.5);
    let galley = ui.painter().layout_no_wrap(text, font, Theme::text_dim());
    let w = (galley.size().x + 20.0).max(42.0);
    let (rect, _) = ui.allocate_exact_size(egui::vec2(w, h), egui::Sense::hover());
    let mut btn_ui = ui.new_child(egui::UiBuilder::new().max_rect(rect));
    {
        let sp = btn_ui.spacing_mut();
        sp.button_padding = egui::vec2(0.0, 0.0);
        sp.interact_size.y = h;
    }
    let resp = btn_ui.add(
        egui::Button::new(
            egui::RichText::new(format!("{icon} {value}"))
                .size(11.5)
                .color(egui::Color32::TRANSPARENT),
        )
        .fill(egui::Color32::TRANSPARENT)
        .stroke(egui::Stroke::NONE)
        .min_size(rect.size()),
    );
    let resp = resp.on_hover_text(hover);
    let bg = if resp.hovered() || resp.highlighted() {
        Theme::bg_hover()
    } else {
        Theme::bg_elevated()
    };
    ui.painter().rect(
        rect,
        8.0,
        bg,
        egui::Stroke::new(1.0, Theme::border()),
        egui::StrokeKind::Inside,
    );
    let pos = egui::pos2(
        rect.center().x - galley.size().x / 2.0,
        rect.center().y - galley.size().y / 2.0,
    );
    ui.painter().galley(pos, galley, Theme::text_dim());

    let popup_id = ui.id().with(format!("chip_{id}"));
    if let Some(menu) = egui::Popup::menu(&resp).id(popup_id).show(|ui| {
        for (disp, val) in options {
            let lit = *val == value;
            if ui
                .selectable_label(
                    lit,
                    egui::RichText::new(*disp).size(12.0).color(if lit {
                        Theme::accent_light()
                    } else {
                        Theme::text()
                    }),
                )
                .clicked()
            {
                picked = Some(val.to_string());
            }
        }
    }) {
        // 菜单打开期间持续重绘（hover 高亮等即时反馈）
        menu.response.ctx.request_repaint();
    }
    picked
}

/// 渲染用会话快照：messages 以 Arc 共享，仅在会话状态变化时重建——
/// 弹幕动画期间整窗 30fps 重绘，每帧深拷贝整个消息历史是主要卡顿源。
pub struct RenderSession {
    pub id: String,
    pub title: String,
    pub running: bool,
    pub preset: AgentPreset,
    pub messages: Arc<Vec<Message>>,
}

/// 发送模式：回合运行中发送时的行为。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendMode {
    /// 插话：打断当前 step，消息并入本回合续跑（默认，最快得到回应）
    Interject,
    /// 排队：不打断；当前回合结束后作为新回合逐条执行（FIFO）
    Queue,
}

/// 标题行卡片种类（单选：同一时间只开一张）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CardKind {
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

#[cfg(test)]
mod attach_tests {
    use super::*;

    /// 文本附件全文注入 + 代码围栏。
    #[test]
    fn compose_text_attachment() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("note.md");
        std::fs::write(&f, "# hello\nworld").unwrap();
        let out = compose_message_with_attachments("看一下", &[f]);
        assert!(out.starts_with("看一下\n\n"));
        assert!(out.contains("【附件 1/1：note.md】"));
        assert!(out.contains("```\n# hello\nworld\n```"));
    }

    /// 二进制占位（不注入内容，路径可见）。
    #[test]
    fn compose_binary_placeholder() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("blob.bin");
        std::fs::write(&f, [0u8, 159, 146, 150]).unwrap();
        let out = compose_message_with_attachments("", &[f]);
        assert!(out.contains("二进制/非文本文件未注入内容"));
        assert!(!out.contains("```"));
    }

    /// 超大文本截断提示。
    #[test]
    fn compose_large_truncated() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("big.log");
        std::fs::write(&f, "x".repeat(600_000)).unwrap();
        let out = compose_message_with_attachments("", &[f]);
        assert!(out.contains("文件过大，仅注入前 512 KB"));
    }

    /// 空正文 + 纯附件 = 合法消息；含 ``` 的内容换围栏 ~~~。
    #[test]
    fn compose_fence_escape_and_attachment_only() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("code.rs");
        std::fs::write(&f, "fn main() { println!(\"```\"); }").unwrap();
        let out = compose_message_with_attachments("", &[f]);
        assert!(out.starts_with("---\n【附件 1/1：code.rs】"));
        assert!(out.contains("~~~\nfn main()"));
        assert!(out.contains("~~~\n")); // 围栏换用 ~~~ 防嵌套破坏
    }

    /// 无附件 = 原文返回。
    #[test]
    fn compose_no_attachments() {
        assert_eq!(compose_message_with_attachments("hi", &[]), "hi");
    }

    /// 文本判定：扩展名白名单 / 无扩展 UTF-8 探测 / Makefile。
    #[test]
    fn attach_text_detection() {
        let dir = tempfile::tempdir().unwrap();
        let py = dir.path().join("a.py");
        std::fs::write(&py, "x=1").unwrap();
        assert!(attach_is_text(&py));
        let mk = dir.path().join("Makefile");
        std::fs::write(&mk, "all:").unwrap();
        assert!(attach_is_text(&mk));
        let noext = dir.path().join("plain");
        std::fs::write(&noext, "你好 plain text").unwrap();
        assert!(attach_is_text(&noext));
        let bin = dir.path().join("x.exe");
        std::fs::write(&bin, [0u8; 16]).unwrap();
        assert!(!attach_is_text(&bin));
    }
}
