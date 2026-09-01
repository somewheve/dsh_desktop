//! 会话标签页：弹幕式思考流 + 对话界面
//!
//! 布局（人类阅读习惯）
//! ┌──────────────────────────────────────
//!  🎯 弹幕区（AI 工具调用信息从右向左飘过）  danmaku layer
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
use super::theme::{ChipTint, Theme};

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
    /// 弹幕层（AI 工具调用信息）
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
    pub panel_expanded: PanelKind,
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
    /// 复制成功的内联反馈（消息 id + 点击时刻；按钮旁短暂显示"已复制 ✓"）
    copy_flash: Option<(String, std::time::Instant)>,
    /// 状态方块按钮行上一帧实测宽（两遍测宽：右缘对齐用）
    chips_row_w: f32,
    /// 展开面板上一帧实测宽（下标 = PanelKind 判别值；内容自适应宽）
    panel_widths: [f32; 5],
    pub(crate) status: String,
    /// 顶部错误提示（app 侧也可写入，web 启动失败
    pub error: Option<String>,
    /// 工作区路径输
    ws_input: String,
    /// 跨页打开文件请求（diff 卡标题点击 → app 消费切到代码浏览器）
    pub open_file_req: Option<std::path::PathBuf>,
    /// 全文搜索结果缓存 (查询, 时间, 结果)——防抖用
    search_hits: Option<(String, std::time::Instant, Vec<(SessionSummary, String)>)>,
    /// 定时任务表单（Jobs 卡片）
    sched_name: String,
    sched_interval: String,
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
            panel_expanded: PanelKind::None,
            plan_mode: crate::engine::plan::PlanMode::Inactive,
            plan_content: String::new(),
            goals: crate::engine::goal::GoalManager::default(),
            subs: std::collections::HashMap::new(),
            goal_input: String::new(),
            copy_flash: None,
            chips_row_w: 0.0,
            panel_widths: [0.0; 5],
            status: String::new(),
            error: None,
            ws_input: ws_current.clone().unwrap_or_default(),
            open_file_req: None,
            search_hits: None,
            sched_name: String::new(),
            sched_interval: String::from("30"),
            ws_current,
            img_cache: std::collections::HashMap::new(),
            viewer: None,
            http_imgs: Vec::new(),
        };
        tab.refresh_sessions();
        tab
    }

    pub fn refresh_sessions(&mut self) {
        let engine = self.engine.lock().unwrap_or_else(|p| p.into_inner());
        self.sessions = engine.list_sessions();
    }

    pub fn open(&mut self, session_id: &str) {
        let mut engine = self.engine.lock().unwrap_or_else(|p| p.into_inner());
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
                    if self.current.as_deref() == Some(session_id.as_str()) {
                        log::debug!(
                            "chat pump: event for current session {} ({})",
                            session_id,
                            event.r#type
                        );
                        // 自动命名 / 重命名：刷新侧栏会话列表标题
                        if event.r#type == types::SESSION_TITLE {
                            self.refresh_sessions();
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
                                        // 思考增量不再上弹幕（用户要求：弹幕只
                                        // 显示工具调用信息）；直接丢弃即可，思考
                                        // 全文已随 assistant/message 事件持久化，
                                        // 消息区折叠面板仍可回看
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
                    // 到期即重绘（闲置窗口无输入时不重绘 → 事件滞留通道，
                    // 用户动鼠标才 fire 的历史缺陷）
                    ui_ctx.request_repaint();
                    // 定时任务到期：把 prompt 发到绑定会话发起回合
                    info!("scheduled task due in UI: {id} ({name})");
                    if let Ok(mut e) = self.engine.lock() {
                        e.fire_scheduled(&id);
                    }
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
    /// 紧凑行（26px）、整行点击、hover 整行半透明浅底、
    /// 选中 = accent 淡底 + 亮字（无强调条，靠底色区分）、
    /// 删除 ✕ 常驻但极淡（hover 变亮；两级确认）、双击重命名、
    /// 运行中 ⚡ 与相对时间在右缘；＋ 新建在列表尾部（树追加语义）。
    pub fn ui_session_list(
        &mut self,
        ui: &mut egui::Ui,
        max_height: Option<f32>,
    ) -> Option<String> {
        const ROW_H: f32 = 26.0;
        /// 右缘相对时间占位（"3分" / "2时" / "8/12"）
        const TIME_W: f32 = 34.0;
        let lang = self.lang;
        // 搜索框（有会话时才显示）：深底圆角胶囊，与输入框 chips 同族
        if self.sessions.len() > 3 {
            ui.add(
                egui::TextEdit::singleline(&mut self.session_search)
                    .hint_text(
                        egui::RichText::new(tr(lang, "🔍 搜索会话…", "🔍 Search…"))
                            .size(10.5)
                            .color(Theme::text_faint()),
                    )
                    .desired_width(ui.available_width().max(60.0))
                    .font(egui::FontId::proportional(10.5))
                    .text_color(Theme::text_dim())
                    .frame(
                        egui::Frame::default()
                            .fill(Theme::bg())
                            .stroke(egui::Stroke::new(1.0, Theme::border()))
                            .corner_radius(egui::CornerRadius::same(7))
                            .inner_margin(egui::Margin::symmetric(8, 4)),
                    ),
            );
            ui.add_space(5.0);
        }
        // 搜索过滤：空 = 全部；非空 = 跨会话全文搜索（防抖 400ms——
        // 历史缺陷：每帧全盘重放所有会话 JSONL，鼠标划过侧栏即连续磁盘
        // 全扫且持引擎锁阻塞回合线程）
        let search_lower = self.session_search.trim().to_lowercase();
        let sessions: Vec<(SessionSummary, String)> = if search_lower.is_empty() {
            self.search_hits = None;
            self.sessions
                .iter()
                .cloned()
                .map(|s| (s, String::new()))
                .collect()
        } else {
            let now = std::time::Instant::now();
            let cached = self
                .search_hits
                .as_ref()
                .map(|(q, t, _)| q == &search_lower && now.duration_since(*t) < std::time::Duration::from_millis(600))
                .unwrap_or(false);
            if cached {
                // 固定 TTL（历史缺陷:命中刷新时间戳 = 滑动窗口,持续重绘
                // 下同一查询永不过期,侧栏搜索结果无限陈旧）
                self.search_hits
                    .as_ref()
                    .map(|(_, _, v)| v.clone())
                    .unwrap_or_default()
            } else if search_lower.chars().count() < 2 {
                // 少于 2 字符不做全盘扫（避免每敲一键重放全部会话）
                Vec::new()
            } else {
                let hits = match self.engine.lock() {
                    Ok(e) => e.search_sessions(&search_lower, 30),
                    Err(_) => Vec::new(),
                };
                self.search_hits = Some((search_lower.clone(), now, hits.clone()));
                hits
            }
        };
        // 本帧被点击打开/新建的会话（返回给侧栏：非 Chat 页时切回 Chat 页）
        let mut opened: Option<String> = None;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or_default();
        let mut sa = ScrollArea::vertical().id_salt("session_list_scroll");
        if let Some(h) = max_height {
            sa = sa.max_height(h);
        }
        sa.show(ui, |ui| {
            // 紧凑行距（默认 8px 显得松散，列表应是连续条目）
            ui.spacing_mut().item_spacing.y = 2.0;
            let mut to_delete: Option<String> = None;
            let mut to_open: Option<String> = None;
            let full_w = ui.available_width().max(60.0);
            for (s, snip) in &sessions {
                ui.horizontal(|ui| {
                    let row_h = if snip.is_empty() { ROW_H } else { ROW_H + 14.0 };
                    let selected = self.current.as_deref() == Some(s.session_id.as_str());
                    let row_w = full_w - 8.0; // 两侧留 4px（树缩进感）
                                              // 占位推进布局；交互响应用 interact（allocate 响应的
                                              // widget_info 在 accessibility 树中不上报，interact 稳定）
                    let (alloc, _) =
                        ui.allocate_exact_size(egui::vec2(full_w, row_h), egui::Sense::hover());
                    let row = egui::Rect::from_min_size(
                        egui::pos2(alloc.left() + 4.0, alloc.top()),
                        egui::vec2(row_w, row_h),
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
                    let hovered = row_resp.hovered() || del_resp.hovered();

                    // 整行底色三态：确认删除（危险色）> 选中（accent 淡底）> hover（半透明）。
                    // 无左侧强调条；选中靠 accent 淡底 + 亮字表达。
                    if is_confirm {
                        ui.painter().rect_filled(
                            row,
                            6.0,
                            Theme::err().gamma_multiply(0.12),
                        );
                    } else if selected {
                        ui.painter().rect_filled(
                            row,
                            6.0,
                            Theme::accent().gamma_multiply(0.16),
                        );
                    } else if hovered {
                        ui.painter().rect_filled(
                            row,
                            6.0,
                            Theme::bg_hover().gamma_multiply(0.5),
                        );
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
                                let mut engine = self.engine.lock().unwrap_or_else(|p| p.into_inner());
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
                        // 标题（截断到可用宽；右侧预留 时间 / ⚡ / ✕ 位）
                        let text_w = row_w
                            - ROW_H
                            - TIME_W
                            - 8.0
                            - if s.running { 16.0 } else { 0.0 };
                        let text = truncate_to_width(
                            ui,
                            &s.title,
                            text_w.max(30.0),
                            &egui::FontId::proportional(11.5),
                        );
                        // 层次：选中 = 亮字；空白会话（未发过消息）= 极弱；一般 = 弱
                        let text_color = if selected {
                            Theme::text()
                        } else if s.blank {
                            Theme::text_faint()
                        } else {
                            Theme::text_dim()
                        };
                        let title_y = if snip.is_empty() {
                            row.center().y
                        } else {
                            row.top() + 11.0
                        };
                        ui.painter().text(
                            egui::pos2(row.left() + 20.0, title_y),
                            egui::Align2::LEFT_CENTER,
                            text.clone(),
                            egui::FontId::proportional(11.5),
                            text_color,
                        );
                        // 命中片段（全文搜索结果第二行，弱化灰）
                        if !snip.is_empty() {
                            let snip_w = row_w - 44.0 - TIME_W;
                            let short = truncate_to_width(
                                ui,
                                snip,
                                snip_w.max(40.0),
                                &egui::FontId::proportional(10.0),
                            );
                            ui.painter().text(
                                egui::pos2(row.left() + 20.0, row.top() + 24.0),
                                egui::Align2::LEFT_CENTER,
                                short,
                                egui::FontId::proportional(10.0),
                                Theme::text_faint().gamma_multiply(0.8),
                            );
                        }
                        row_resp.widget_info(|| {
                            egui::WidgetInfo::labeled(
                                egui::WidgetType::Button,
                                true,
                                format!("{text}{}", if s.running { " \u{26a1}" } else { "" }),
                            )
                        });
                        // 相对时间（右缘、✕/⚡ 左侧；极弱，选中时略亮）
                        if s.updated_at > 0.0 {
                            let rel = relative_time(now - s.updated_at, lang);
                            let time_x =
                                del_rect.left() - if s.running { 18.0 } else { 6.0 };
                            ui.painter().text(
                                egui::pos2(time_x, row.center().y),
                                egui::Align2::RIGHT_CENTER,
                                rel,
                                egui::FontId::proportional(9.5),
                                if selected {
                                    Theme::text_faint()
                                } else {
                                    Theme::text_faint().gamma_multiply(0.7)
                                },
                            );
                        }
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
            // 空列表 / 搜索无结果
            if sessions.is_empty() {
                ui.add_space(10.0);
                let hint = if self.sessions.is_empty() {
                    tr(lang, "暂无会话 · 点 ＋ 新建", "No sessions · press ＋")
                } else {
                    tr(lang, "无匹配会话", "No matching sessions")
                };
                ui.label(
                    egui::RichText::new(hint)
                        .size(10.5)
                        .color(Theme::text_faint()),
                );
            }
            // 删除执行 + 当前会话切换
            if let Some(id) = to_delete {
                self.pending_delete = None;
                let mut engine = self.engine.lock().unwrap_or_else(|p| p.into_inner());
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
                // token 用量 + 上下文占用估算（引擎 usage 累计；弱化呈现，
                // 用户在意时可见）。上下文按字节/4 粗估，仅作参考。
                let (usage, ctx, model) = {
                    match self.engine.lock() {
                        Ok(e) => (
                            e.token_usage(&session.id),
                            e.context_estimate(&session.id),
                            e.effective_model(&session.id),
                        ),
                        Err(_) => (Default::default(), 0, String::new()),
                    }
                };
                if usage.total() > 0 {
                    let ctx_cap = crate::core::llm::context_window(&model);
                    let pct = if ctx_cap > 0 {
                        (ctx as f64 / ctx_cap as f64 * 100.0).clamp(0.0, 999.0)
                    } else {
                        0.0
                    };
                    ui.label(
                        RichText::new(format!(
                            "⚡ {:.1}k tok · {} ≈{pct:.0}%",
                            usage.total() as f64 / 1000.0,
                            tr(self.lang, "上下文", "ctx"),
                        ))
                        .size(10.0)
                        .color(Theme::text_faint()),
                    )
                    .on_hover_text(tr(
                        self.lang,
                        "本会话累计 token 消耗（输入+输出）与当前上下文占用估算",
                        "Session token usage (in+out) and estimated context occupancy",
                    ));
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
                // 会话分支（最右）：复制当前进度到新会话——探索性改动的安全副本
                {
                    let lang = self.lang;
                    if Theme::mini_button(ui, "⑂", ChipTint::Neutral)
                        .on_hover_text(tr(
                            lang,
                            "从当前进度分叉出新会话（原会话保留不动）",
                            "Fork a new session from here (original kept intact)",
                        ))
                        .clicked()
                    {
                        let sid = session.id.clone();
                        // map_err 立即丢弃 PoisonError（其持有 guard，会让
                        // 引擎锁的借用活到 match 末尾，与后续 &mut self 冲突）
                        let forked = self
                            .engine
                            .lock()
                            .map(|mut e| e.fork_session(&sid))
                            .map_err(|_| "engine lock poisoned");
                        match forked {
                            Ok(Ok(new_id)) => {
                                self.refresh_sessions();
                                self.open(&new_id);
                                self.status = tr(lang, "已分叉到新会话", "Forked to new session").into();
                            }
                            Ok(Err(e)) => {
                                self.error = Some(format!("分叉失败：{e:#}"));
                            }
                            Err(e) => {
                                self.error = Some(e.to_string());
                            }
                        }
                    }
                }
                // 导出 Markdown（最右第二）：当前会话存为 .md（归档/分享）
                {
                    let lang = self.lang;
                    if Theme::mini_button(ui, "⬇", ChipTint::Neutral)
                        .on_hover_text(tr(
                            lang,
                            "导出当前会话为 Markdown 文件",
                            "Export this session as Markdown",
                        ))
                        .clicked()
                    {
                        let sid = session.id.clone();
                        let md = self
                            .engine
                            .lock()
                            .ok()
                            .and_then(|e| e.export_session_markdown(&sid).ok());
                        match md {
                            Some(text) => {
                                let safe: String = session
                                    .title
                                    .chars()
                                    .filter(|c| c.is_alphanumeric() || "._- ".contains(*c))
                                    .take(40)
                                    .collect();
                                let name = if safe.trim().is_empty() {
                                    format!("{}.md", session.id)
                                } else {
                                    format!("{safe}.md")
                                };
                                if let Some(path) = rfd::FileDialog::new()
                                    .set_file_name(&name)
                                    .set_title(tr(lang, "导出会话", "Export session"))
                                    .save_file()
                                {
                                    match std::fs::write(&path, &text) {
                                        Ok(()) => {
                                            self.status = format!(
                                                "{} {}",
                                                tr(lang, "已导出", "Exported"),
                                                path.display()
                                            );
                                        }
                                        Err(e) => {
                                            self.error =
                                                Some(format!("{}: {e}", tr(lang, "导出失败", "Export failed")));
                                        }
                                    }
                                }
                            }
                            None => {
                                self.error =
                                    Some(tr(lang, "导出失败（会话读取错误）", "Export failed").into());
                            }
                        }
                    }
                }
            });
        }
        ui.add_space(2.0);

        // 模式选择 / 工作区栏 / 弹幕 / 状态方块 / 消息 + 输入
        self.render_body_bottom(ui, session);
    }

    /// ZCode 式状态方块按钮（弹幕区下方右缘，悬浮吸附）：每个非空域一个
    /// 紧凑方块（📋 计划 n/m、🤖 子代理、🎯 目标、⏳ 任务）；域处于活动
    /// 态（计划进行中 / 子代理运行 / 任务运行 / 目标进行中）或已展开时
    /// 按钮用强调色，点击展开/收起对应清单（单选）。
    /// 悬浮 Area（Order::Middle）不占布局流——消息区/输入框完全不受影响
    /// （历史缺陷：右上浮层卡压住 📂浏览/切换 按钮；布局流版本会挤压
    /// 消息区）。行宽与展开面板宽均两遍测宽：右缘对齐、随内容自适应。
    #[allow(clippy::too_many_lines)]
    fn render_status_chips(
        &mut self,
        ui: &mut egui::Ui,
        session: &RenderSession,
        anchor_top: f32,
        right_edge: f32,
    ) {
        let lang = self.lang;
        // ---- 数据收集（计划条目 / 子代理 / 目标 / 运行中任务）----
        let plan_active = self.plan_mode == crate::engine::plan::PlanMode::Active;
        let plan_items: Vec<(Option<bool>, String)> = self
            .plan_content
            .lines()
            .map(|l| {
                match crate::ui::markdown::checkbox_state(
                    l.trim_start_matches(['-', '*', '+', ' ']),
                ) {
                    Some((done, rest)) => (Some(done), rest.trim().to_string()),
                    None => (None, l.trim().to_string()),
                }
            })
            .filter(|(_, t)| !t.is_empty())
            .collect();
        let (plan_total, plan_done) = plan_items
            .iter()
            .filter(|(d, _)| d.is_some())
            .fold((0usize, 0usize), |(t, c), (d, _)| {
                (t + 1, c + d.unwrap_or(false) as usize)
            });
        let subs_total = self.subs.len();
        let subs_running = self
            .subs
            .values()
            .filter(|d| d.status == "running")
            .count();
        let goals_list = self.goals.list();
        let goals_active = goals_list
            .iter()
            .filter(|g| matches!(g.phase, crate::engine::goal::GoalPhase::Active))
            .count();
        let jobs_live = self
            .engine
            .lock()
            .map(|e| {
                e.jobs_snapshot()
                    .into_iter()
                    .filter(|j| {
                        matches!(
                            j.status,
                            crate::engine::jobs::JobStatus::Running
                                | crate::engine::jobs::JobStatus::Pending
                        )
                    })
                    .count()
            })
            .unwrap_or(0);

        // ---- 方块按钮行 ----
        let mut chips: Vec<(PanelKind, String, bool, String)> = Vec::new();
        if !self.plan_content.trim().is_empty() {
            let label = if plan_total > 0 {
                format!("📋 {plan_done}/{plan_total}")
            } else {
                "📋".to_string()
            };
            let tip = if plan_active {
                tr(lang, "计划 · 进行中", "Plan · active")
            } else {
                tr(lang, "计划 · 已完成", "Plan · finished")
            };
            chips.push((PanelKind::Plan, label, plan_active, tip.to_string()));
        }
        if subs_total > 0 {
            let label = if subs_running > 0 {
                format!("🤖 {subs_running}/{subs_total}")
            } else {
                format!("🤖 {subs_total}")
            };
            chips.push((
                PanelKind::Subagents,
                label,
                subs_running > 0,
                tr(lang, "子代理", "Subagents").to_string(),
            ));
        }
        if !goals_list.is_empty() {
            chips.push((
                PanelKind::Goals,
                format!("🎯 {goals_active}/{}", goals_list.len()),
                goals_active > 0,
                tr(lang, "目标", "Goals").to_string(),
            ));
        }
        if jobs_live > 0 {
            chips.push((
                PanelKind::Jobs,
                format!("⏳ {jobs_live}"),
                true,
                tr(lang, "运行中任务", "Running jobs").to_string(),
            ));
        }
        if chips.is_empty() {
            return;
        }
        // 行宽两遍测：上一帧实测宽（首帧按标签估宽），右缘对齐 right_edge
        let est: f32 = chips
            .iter()
            .map(|(_, label, _, _)| {
                ui.ctx().fonts_mut(|f| {
                    f.layout_no_wrap(
                        format!("{label} ▸"),
                        egui::FontId::proportional(11.0),
                        egui::Color32::WHITE,
                    )
                    .size()
                    .x
                        + 34.0
                })
            })
            .sum::<f32>()
            + 6.0 * chips.len().max(1) as f32;
        // 行宽两遍测：首帧用标签估宽，之后**只用实测宽**——历史缺陷：
        // max(实测, 估宽) 且估宽系统性偏大（每按钮多算 ~34px），锚点被
        // 整体左推，按钮离右缘 100+px。实测宽下一帧即精确贴住 right_edge
        //（内容变宽时首帧略溢出一帧，随后自动修正）。
        let row_w = if self.chips_row_w > 0.0 {
            self.chips_row_w
        } else {
            est
        };
        let row_resp = egui::Area::new(egui::Id::new("status_chips"))
            .order(egui::Order::Middle)
            .fixed_pos(egui::pos2(right_edge - row_w, anchor_top))
            .show(ui.ctx(), |ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing = egui::vec2(6.0, 0.0);
                    for (kind, label, working, tip) in &chips {
                        let expanded = self.panel_expanded == *kind;
                        let tint = if *working || expanded {
                            ChipTint::Accent
                        } else {
                            ChipTint::Neutral
                        };
                        let arrow = if expanded { "▾" } else { "▸" };
                        let resp = Theme::mini_button(ui, format!("{label} {arrow}"), tint)
                            .on_hover_text(tip.as_str());
                        if resp.clicked() {
                            self.panel_expanded =
                                if expanded { PanelKind::None } else { *kind };
                        }
                    }
                });
            });
        self.chips_row_w = row_resp.response.rect.width().max(0.0);
        // ---- 展开面板（单选；右缘对齐、按钮行下方；宽自适应内容）----
        let kind = self.panel_expanded;
        if kind != PanelKind::None {
            let pw = {
                let est_w = self.panel_widths[kind as usize];
                if est_w > 0.0 { est_w } else { 380.0 }
            };
            let presp = egui::Area::new(egui::Id::new(("status_panel_exp", kind as u8)))
                .order(egui::Order::Middle)
                .fixed_pos(egui::pos2(
                    right_edge - pw,
                    row_resp.response.rect.bottom() + 6.0,
                ))
                .show(ui.ctx(), |ui| {
                    egui::Frame::default()
                        .fill(Theme::bg_elevated())
                        .stroke(egui::Stroke::new(1.0, Theme::border()))
                        .corner_radius(egui::CornerRadius::same(8))
                        .inner_margin(egui::Margin::symmetric(10, 6))
                        .shadow(egui::Shadow {
                            blur: 12,
                            spread: 0,
                            offset: [0, 4],
                            color: egui::Color32::from_black_alpha(120),
                        })
                        .show(ui, |ui| {
                            // 悬浮 Area 高度无限：内部 ScrollArea 的
                            // auto_shrink(false) 会取 available 高度导致
                            // 面板撑满/空白——必须给定高度上限。
                            ui.set_max_width(560.0);
                            ui.set_max_height(460.0);
                            match kind {
                                PanelKind::Plan => self.render_plan_items(ui, &plan_items),
                                PanelKind::Subagents => self.render_subagents_card(ui),
                                PanelKind::Goals => self.render_goals_card(ui, session),
                                PanelKind::Jobs => self.render_jobs_card(ui, &session.id),
                                PanelKind::None => {}
                            }
                        });
                });
            self.panel_widths[kind as usize] = presp.response.rect.width().max(0.0);
        }
    }

    /// 计划清单条目（状态面板展开时渲染）：只显示待办项
    /// （已完成的收进头部进度 n/m，不占清单空间）；全部完成时给一行确认。
    /// 计划清单（状态面板展开时渲染）：显示**全部条目**（含已完成历史——
    /// 历史缺陷：只显示待办，计划完成后整卡只剩"全部完成"看不到做过什么）。
    /// 排版对阅读友好：进度头 + 逐条清单（待办 ☐ 高亮 / 已完成 ✓ 灰色删除线 /
    /// 说明行弱化），标签换行不截断。
    fn render_plan_items(&mut self, ui: &mut egui::Ui, items: &[(Option<bool>, String)]) {
        let lang = self.lang;
        let total = items.iter().filter(|(d, _)| d.is_some()).count();
        let done = items
            .iter()
            .filter(|(d, _)| d == &Some(true))
            .count();
        let todo = total - done;
        // ---- 进度头：n/m + 状态（一眼看总量与剩余）----
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing = egui::vec2(6.0, 0.0);
            ui.label(
                RichText::new(format!("📋 {done}/{total}"))
                    .size(12.0)
                    .strong()
                    .color(Theme::text()),
            );
            if total > 0 && todo == 0 {
                ui.label(RichText::new(tr(lang, "✓ 全部完成", "✓ all done")).size(11.0).color(Theme::ok()));
            } else {
                ui.label(
                    RichText::new(format!("· {} {}", todo, tr(lang, "项待办", "todo")))
                        .size(11.0)
                        .color(Theme::accent_light()),
                );
            }
        });
        ui.add_space(5.0);
        // ---- 清单：全部条目按原顺序（时间序）----
        egui::ScrollArea::vertical()
            .max_height(280.0)
            .id_salt("plan_panel_scroll")
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing = egui::vec2(0.0, 5.0);
                for (state, text) in items {
                    match state {
                        Some(false) => {
                            ui.horizontal(|ui| {
                                ui.label(
                                    RichText::new("☐")
                                        .size(13.0)
                                        .color(Theme::accent_light()),
                                );
                                ui.add(
                                    egui::Label::new(
                                        RichText::new(text.clone())
                                            .size(12.0)
                                            .color(Theme::text()),
                                    )
                                    .wrap(),
                                );
                            });
                        }
                        Some(true) => {
                            // 已完成：保留历史但弱化（灰色 + 删除线）
                            ui.horizontal(|ui| {
                                ui.label(RichText::new("✓").size(12.0).color(Theme::ok()));
                                ui.add(
                                    egui::Label::new(
                                        RichText::new(text.clone())
                                            .size(11.5)
                                            .strikethrough()
                                            .color(Theme::text_faint()),
                                    )
                                    .wrap(),
                                );
                            });
                        }
                        None => {
                            // 说明/标题行（非清单项）
                            ui.label(
                                RichText::new(text.clone())
                                    .size(11.0)
                                    .color(Theme::text_dim()),
                            );
                        }
                    }
                }
            });
    }

    /// 目标卡片：fold goal/change 事件；操作经 engine.goal_op 持久化
    fn render_goals_card(&mut self, ui: &mut egui::Ui, session: &RenderSession) {
        // 直接在状态卡内渲染（外层 status_panel 提供卡片边框与自适应宽度），
        // 列表限高防撑爆——不写死卡片高度。
        // 新建目标输入
        let mut create_obj: Option<String> = None;
        let lang = self.lang;
        ui.horizontal(|ui| {
            // 胶囊输入框：与 mini_button 等高（24px）同圆角/配色风格
            //（历史缺陷：默认 TextEdit 与创建按钮不等高、风格突兀）
            ui.spacing_mut().item_spacing = egui::vec2(6.0, 0.0);
            let edit = TextEdit::singleline(&mut self.goal_input)
                .hint_text(
                    RichText::new(tr(lang, "新目标描述…", "New goal…"))
                        .size(11.5)
                        .color(Theme::text_faint()),
                )
                .font(egui::FontId::proportional(11.5))
                .text_color(Theme::text())
                .frame(
                    egui::Frame::default()
                        .fill(Theme::bg_elevated())
                        .stroke(egui::Stroke::new(1.0, Theme::border()))
                        .corner_radius(egui::CornerRadius::same(8))
                        .inner_margin(egui::Margin::symmetric(10, 3)),
                );
            let resp = ui.add_sized([220.0, 24.0], edit);
            let clicked = Theme::mini_button(ui, tr(lang, "创建", "Create"), ChipTint::Accent)
                .clicked()
                || (resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)));
            if clicked && !self.goal_input.trim().is_empty() {
                create_obj = Some(self.goal_input.trim().to_string());
                self.goal_input.clear();
            }
        });
        ui.add_space(2.0);
        let goals: Vec<crate::engine::goal::Goal> =
            self.goals.list().iter().map(|g| (*g).clone()).collect();
        let mut ops: Vec<(String, crate::engine::goal::GoalOp)> = Vec::new();
        ScrollArea::vertical()
            .max_height(260.0)
            .id_salt("goals_card_scroll")
            .show(ui, |ui| {
                use crate::engine::goal::{GoalOp, GoalPhase};
                ui.spacing_mut().item_spacing = egui::vec2(0.0, 4.0);
                for g in &goals {
                    let (color, done) = match g.phase {
                        GoalPhase::Active => (Theme::ok(), false),
                        GoalPhase::Paused => (Theme::warn(), false),
                        GoalPhase::Blocked => (Theme::err(), false),
                        GoalPhase::Complete => (Theme::text_faint(), true),
                    };
                    // 目标文字独立成块（换行）——历史缺陷：与状态/按钮同一
                    // horizontal 且不 wrap，长目标把 active/完成/暂停/阻塞
                    // 挤得重叠错乱。
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("●").size(12.0).color(color));
                        let text = RichText::new(g.objective.clone()).size(12.0).color(color);
                        let text = if done { text.strikethrough() } else { text };
                        ui.add(egui::Label::new(text).wrap());
                    });
                    // 状态 + 操作按钮独立一行（左状态、右按钮，不再挤压文字）
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new(g.phase.as_str())
                                .size(10.5)
                                .color(Theme::text_faint()),
                        );
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.spacing_mut().item_spacing = egui::vec2(4.0, 0.0);
                            if g.phase != GoalPhase::Complete
                                && Theme::mini_button(ui, &tr(lang, "完成", "Complete"), ChipTint::Accent).clicked()
                            {
                                ops.push((g.id.clone(), GoalOp::Complete));
                            }
                            if g.phase == GoalPhase::Active
                                && Theme::mini_button(ui, &tr(lang, "暂停", "Pause"), ChipTint::Neutral).clicked()
                            {
                                ops.push((g.id.clone(), GoalOp::Pause));
                            }
                            if g.phase == GoalPhase::Paused
                                && Theme::mini_button(ui, &tr(lang, "恢复", "Resume"), ChipTint::Accent).clicked()
                            {
                                ops.push((g.id.clone(), GoalOp::Resume));
                            }
                            if g.phase == GoalPhase::Active
                                && Theme::mini_button(ui, &tr(lang, "阻塞", "Block"), ChipTint::Danger).clicked()
                            {
                                ops.push((g.id.clone(), GoalOp::Block));
                            }
                        });
                    });
                    ui.add_space(4.0);
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
            let mut engine = self.engine.lock().unwrap_or_else(|p| p.into_inner());
            let _ = engine.goal_op(
                &session.id,
                crate::engine::goal::GoalOp::Create,
                &gid,
                Some(&obj),
            );
        }
        for (gid, op) in ops {
            let mut engine = self.engine.lock().unwrap_or_else(|p| p.into_inner());
            let _ = engine.goal_op(&session.id, op, &gid, None);
        }
        ui.add_space(4.0);
    }

    /// 子代理卡片：fold subagent/descriptor 事件（持久化，重放可恢复）
    fn render_subagents_card(&mut self, ui: &mut egui::Ui) {
        // 直接在状态卡内渲染（外层 status_panel 已有卡片边框/自适应宽度），
        // 列表 ScrollArea 限高防撑爆，展开行内容随行自然增高——不再写死
        // 卡片高度（历史缺陷：固定 180px + clip，展开详情被裁掉）。
        let mut subs: Vec<_> = self.subs.values().cloned().collect();
        subs.sort_by(|a, b| b.subagent_id.cmp(&a.subagent_id));
        let lang = self.lang;
        ScrollArea::vertical()
            .max_height(320.0)
            .id_salt("subs_panel_scroll")
            .show(ui, |ui| {
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
                    // 可展开行：标题（状态 + id + 摘要），展开看完整任务/输出
                    //（descriptor 携带 task/result，事件持久化重放可恢复）
                    let has_detail = d.task.as_deref().map_or(false, |t| !t.is_empty())
                        || d.result.as_deref().map_or(false, |r| !r.is_empty());
                    if has_detail {
                        let header = format!(
                            "{icon} {}  {}",
                            d.subagent_id,
                            d.summary
                                .as_deref()
                                .map(|s| truncate(s, 60))
                                .unwrap_or_default()
                        );
                        egui::CollapsingHeader::new(
                            RichText::new(header)
                                .monospace()
                                .size(11.0)
                                .color(color),
                        )
                        .id_salt(egui::Id::new(&d.subagent_id))
                        .default_open(false)
                        .show(ui, |ui| {
                            egui::Frame::default()
                                .fill(Theme::bg())
                                .corner_radius(egui::CornerRadius::same(6))
                                .inner_margin(egui::Margin::symmetric(8, 6))
                                .show(ui, |ui| {
                                    ui.set_max_width(ui.available_width().max(60.0));
                                    if let Some(task) =
                                        d.task.as_deref().filter(|t| !t.is_empty())
                                    {
                                        ui.label(
                                            RichText::new(tr(
                                                lang,
                                                "任务：",
                                                "Task:",
                                            ))
                                            .size(10.5)
                                            .strong()
                                            .color(Theme::accent_light()),
                                        );
                                        ui.label(
                                            RichText::new(task)
                                                .size(10.5)
                                                .color(Theme::text_dim()),
                                        );
                                        ui.add_space(3.0);
                                    }
                                    if let Some(res) =
                                        d.result.as_deref().filter(|r| !r.is_empty())
                                    {
                                        ui.label(
                                            RichText::new(tr(
                                                lang,
                                                "完整输出：",
                                                "Full output:",
                                            ))
                                            .size(10.5)
                                            .strong()
                                            .color(Theme::accent_light()),
                                        );
                                        ScrollArea::vertical()
                                            .id_salt(ui.id().with("sub_res"))
                                            .max_height(220.0)
                                            .show(ui, |ui| {
                                                ui.add(
                                                    egui::Label::new(
                                                        RichText::new(res)
                                                            .size(10.5)
                                                            .color(Theme::text_dim()),
                                                    )
                                                    .wrap(),
                                                );
                                            });
                                    }
                                });
                        });
                    } else {
                        ui.horizontal(|ui| {
                            ui.label(
                                RichText::new(format!("{icon} {}", d.subagent_id))
                                    .monospace()
                                    .color(color),
                            );
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    ui.label(Theme::dim(&d.status));
                                },
                            );
                        });
                        if let Some(sum) = &d.summary {
                            ui.label(
                                RichText::new(truncate(sum, 200))
                                    .size(11.0)
                                    .color(Theme::text_dim()),
                            );
                        }
                    }
                    ui.add_space(3.0);
                }
                if subs.is_empty() {
                    ui.label(Theme::dim(&tr(
                        lang,
                        "暂无子代理。AI 调用 subagent_fork 工具后显示在这里。",
                        "No subagents yet. They appear here when the AI calls subagent_fork.",
                    )));
                }
            });
    }

    /// 任务卡片：实时读取引JobManager（回/ 子代理自动创建任务）
    /// 底部含 ⏱ 定时任务管理（到期把提示词发到绑定会话发起 AI 回合）。
    fn render_jobs_card(&mut self, ui: &mut egui::Ui, session_id: &str) {
        // 直接在状态卡内渲染（外层 status_panel 提供边框与自适应宽度）。
        // 整卡包一层限高滚动（历史缺陷：固定 240px + clip，定时任务区被裁）。
        let mut remove: Option<String> = None;
        let lang = self.lang;
        ScrollArea::vertical()
            .max_height(400.0)
            .id_salt("jobs_panel_scroll")
            .show(ui, |ui| {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if Theme::mini_button(
                        ui,
                        &tr(lang, "清空已完成", "Clear finished"),
                        ChipTint::Neutral,
                    )
                    .clicked()
                    {
                        self.engine
                            .lock()
                            .unwrap_or_else(|p| p.into_inner())
                            .job_clear_finished();
                    }
                });
                ui.add_space(4.0);
                let jobs =
                    self.engine.lock().unwrap_or_else(|p| p.into_inner()).jobs_snapshot();
                {
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
                    // ZCode 式紧凑行：icon + 名称（detail 并入行内）+ 状态右对齐
                    let title = match &job.detail {
                        Some(d) => format!("{icon} {} · {}", job.name, truncate(d, 40)),
                        None => format!("{icon} {}", job.name),
                    };
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(title).size(11.5).color(color));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if Theme::mini_button(
                                ui,
                                &tr(lang, "✕", "✕"),
                                ChipTint::Danger,
                            )
                            .on_hover_text(tr(lang, "移除记录", "Remove entry"))
                            .clicked()
                            {
                                remove = Some(job.id.clone());
                            }
                            ui.label(Theme::dim(job.status.as_str()));
                        });
                    });
                    if let Some(r) = &job.result {
                        ui.label(
                            RichText::new(format!("  {}", truncate(r, 100)))
                                .size(10.5)
                                .color(Theme::text_faint()),
                        );
                    }
                    ui.add_space(2.0);
                }
                if jobs.is_empty() {
                    ui.label(Theme::dim(&tr(
                        lang,
                        "暂无任务。agent 回合 / 子代理执行会自动创建任务。",
                        "No jobs. Agent turns and subagents create jobs automatically.",
                    )));
                }
                }
                if let Some(id) = remove {
                    self.engine.lock().unwrap_or_else(|p| p.into_inner()).job_remove(&id);
                }
                ui.add_space(4.0);
        // ===== ⏱ 定时任务（到期把提示词发到绑定会话，AI 自动执行）=====
        let mut sched_remove: Option<String> = None;
        let mut sched_toggle: Option<(String, bool)> = None;
        ui.separator();
        ui.label(Theme::card_section_title(&format!(
            "⏱ {}",
            tr(lang, "定时任务（当前会话）", "Scheduled (this session)")
        )));
        let tasks = self.engine.lock().map(|e| e.schedule_list()).unwrap_or_default();
        let session_tasks: Vec<_> = tasks
            .iter()
            .filter(|t| t.session_id == session_id)
            .cloned()
            .collect();
        if session_tasks.is_empty() {
            ui.label(Theme::dim(&tr(
                lang,
                "无定时任务。例如：每 30 分钟「检查构建并汇报」。",
                "No scheduled tasks. E.g. every 30m: \"check the build\".",
            )));
        }
        for t in &session_tasks {
            ui.horizontal(|ui| {
                let mins = t.interval_secs / 60;
                let state = if t.enabled { "●" } else { "○" };
                ui.label(
                    RichText::new(format!("{state} {}", t.name))
                        .size(11.5)
                        .color(if t.enabled {
                            Theme::text()
                        } else {
                            Theme::text_faint()
                        }),
                )
                .on_hover_text(&t.prompt);
                ui.label(
                    RichText::new(if mins >= 1 {
                        format!("每 {mins} 分")
                    } else {
                        format!("每 {} 秒", t.interval_secs)
                    })
                    .size(10.5)
                    .color(Theme::text_faint()),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if Theme::mini_button(ui, "✕", ChipTint::Danger)
                        .on_hover_text(tr(lang, "删除定时任务", "Delete task"))
                        .clicked()
                    {
                        sched_remove = Some(t.id.clone());
                    }
                    let (label, to) = if t.enabled {
                        (tr(lang, "暂停", "pause"), false)
                    } else {
                        (tr(lang, "启用", "enable"), true)
                    };
                    if Theme::mini_button(ui, &label, ChipTint::Neutral)
                        .on_hover_text(tr(lang, "启停定时任务", "Toggle task"))
                        .clicked()
                    {
                        sched_toggle = Some((t.id.clone(), to));
                    }
                });
            });
        }
        // 新建表单：名称（即提示词） + 间隔分钟
        ui.horizontal(|ui| {
            ui.add(
                TextEdit::singleline(&mut self.sched_name)
                    .hint_text(
                        RichText::new(tr(lang, "任务提示词，如：检查构建并汇报", "Prompt, e.g. check the build"))
                            .size(10.5)
                            .color(Theme::text_faint()),
                    )
                    .desired_width(170.0)
                    .font(FontId::proportional(10.5)),
            );
            ui.add(
                TextEdit::singleline(&mut self.sched_interval)
                    .hint_text(RichText::new(tr(lang, "分钟", "min")).size(10.5).color(Theme::text_faint()))
                    .desired_width(40.0)
                    .font(FontId::proportional(10.5)),
            );
            if Theme::mini_button(ui, &tr(lang, "添加", "Add"), ChipTint::Accent)
                .on_hover_text(tr(
                    lang,
                    "按间隔把提示词发到当前会话（AI 自动执行；可暂停/删除）",
                    "Send the prompt to this session on an interval (auto-run; pausable)",
                ))
                .clicked()
            {
                let name = self.sched_name.trim().to_string();
                let mins: u64 = self.sched_interval.trim().parse().unwrap_or(0);
                if name.is_empty() || mins == 0 {
                    self.status = tr(lang, "请填写提示词与间隔（分钟）", "Need a prompt and interval (min)").into();
                } else {
                    if let Ok(mut e) = self.engine.lock() {
                        e.schedule_add(&name, &name, mins * 60, session_id);
                    }
                    self.sched_name.clear();
                    self.status = tr(lang, "定时任务已添加", "Scheduled task added").into();
                }
            }
        });
        if let Some(id) = sched_remove {
            if let Ok(mut e) = self.engine.lock() {
                e.schedule_remove(&id);
            }
        }
        if let Some((id, on)) = sched_toggle {
            if let Ok(mut e) = self.engine.lock() {
                e.schedule_toggle(&id, on);
            }
        }
            });
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
                let w = (ui.available_width() - 210.0).max(40.0);
                let resp = ui.add(
                    TextEdit::singleline(&mut self.ws_input)
                        .hint_text(
                            RichText::new(tr(lang, "打开工作区目录：", "Open workspace dir: "))
                                .size(11.5)
                                .color(Theme::text_faint()),
                        )
                        .desired_width(w)
                        .font(FontId::proportional(11.5))
                        .text_color(Theme::text_dim())
                        .frame(
                            egui::Frame::default()
                                .fill(Theme::bg())
                                .stroke(egui::Stroke::new(1.0, Theme::border()))
                                .corner_radius(egui::CornerRadius::same(7))
                                .inner_margin(egui::Margin::symmetric(8, 4)),
                        ),
                );
                let browse = Theme::mini_button(ui, &tr(lang, "📂 浏览", "📂 Browse"), ChipTint::Neutral)
                    .on_hover_text(tr(lang, "打开系统目录选择器", "Open system folder picker"));
                if browse.clicked() {
                    if let Some(dir) = rfd::FileDialog::new()
                        .set_title(tr(lang, "选择工作区目录", "Select workspace folder"))
                        .pick_folder()
                    {
                        let path = dir.to_string_lossy().into_owned();
                        self.ws_input = path.clone();
                        let mut engine = self.engine.lock().unwrap_or_else(|p| p.into_inner());
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
                let opened = Theme::mini_button(ui, &tr(lang, "切换", "Switch"), ChipTint::Accent)
                    .on_hover_text(tr(
                        lang,
                        "规范化目录并作为本会话工作区（AI 回合 / 工具立即生效）",
                        "Normalize and set as this session's workspace",
                    ))
                    .clicked()
                    || (resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)));
                if opened && !self.ws_input.trim().is_empty() {
                    let path = self.ws_input.trim().to_string();
                    let mut engine = self.engine.lock().unwrap_or_else(|p| p.into_inner());
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

        // ===== 弹幕区（AI 工具调用信息；窗口矮时动态压缩，避免输入框被挤出面板====
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
        // dm_height 含 22px 标题条；轨道计算需排除（历史缺陷：60px 档
        // 第二轨底部超出裁剪矩形，文字半截）
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
            tr(self.lang, "🎯 工具调用", "🎯 Tool activity"),
            FontId::proportional(11.0),
            Theme::text_faint(),
        );
        // 空态提示：无弹幕时补一行弱提示，避免"黑洞块"观感
        if dm_height > 0.0 && self.danmaku.is_empty() {
            let hint = tr(
                self.lang,
                "AI 调用工具的过程将在这里飘过…",
                "Tool calls will float by here…",
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
        self.danmaku.set_height((dm_height - 22.0).max(0.0));
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

        // ===== 状态方块按钮（弹幕下方右缘，悬浮吸附；点击展开清单）=====
        // 分割线占 ~17px（space4 + line + space4）：锚点放到线下，
        // 全屏/小窗下按钮都不与分割线重合（历史缺陷：+10 压线）
        let chips_top = if dm_height > 0.0 {
            dm_rect.bottom() + 24.0
        } else {
            ui.cursor().top() + 2.0
        };
        let chips_right = ui.max_rect().right() - 6.0;
        self.render_status_chips(ui, session, chips_top, chips_right);

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
                    // 空态引导：无任何消息且不在运行时，展示能力示例
                    // （点击填入输入框，不直接发送——用户可改可换）。
                    if session.messages.is_empty() && !has_stream {
                        let avail = ui.available_size();
                        let block_h = (avail.y - 24.0).clamp(120.0, 260.0);
                        let (rect, _) = ui.allocate_exact_size(
                            vec2(avail.x.max(80.0), block_h),
                            egui::Sense::hover(),
                        );
                        let c = rect.center();
                        ui.painter().text(
                            c - egui::vec2(0.0, block_h * 0.28),
                            egui::Align2::CENTER_CENTER,
                            tr(self.lang, "描述任务开始，或试试：", "Describe a task, or try:"),
                            egui::FontId::proportional(12.5),
                            Theme::text_faint(),
                        );
                        let examples: [(&str, String); 3] = [
                            (
                                "🔍",
                                tr(
                                    self.lang,
                                    "阅读这个项目的代码，总结架构与风险",
                                    "Read this project and summarize architecture & risks",
                                ),
                            ),
                            (
                                "🛠",
                                tr(
                                    self.lang,
                                    "帮我实现一个功能并写测试",
                                    "Implement a feature for me with tests",
                                ),
                            ),
                            (
                                "🐞",
                                tr(
                                    self.lang,
                                    "分析这个 bug 的根因并修复",
                                    "Analyze and fix this bug",
                                ),
                            ),
                        ];
                        let chip_h = 30.0;
                        let gap = 8.0;
                        let widths: Vec<f32> = examples
                            .iter()
                            .map(|(icon, t)| {
                                ui.fonts_mut(|f| {
                                    f.layout_no_wrap(
                                        format!("{icon}  {t}"),
                                        egui::FontId::proportional(11.5),
                                        Theme::text_dim(),
                                    )
                                    .size()
                                    .x
                                        + 24.0
                                })
                            })
                            .collect();
                        let total: f32 =
                            widths.iter().sum::<f32>() + gap * (examples.len() - 1) as f32;
                        let mut x = c.x - total / 2.0;
                        for ((icon, t), w) in examples.iter().zip(&widths) {
                            let chip = egui::Rect::from_min_size(
                                egui::pos2(x, c.y + block_h * 0.06),
                                vec2(*w, chip_h),
                            );
                            let resp = ui.interact(
                                chip,
                                ui.id().with(("example", t.as_str())),
                                egui::Sense::click(),
                            );
                            let bg = if resp.hovered() {
                                Theme::bg_hover()
                            } else {
                                Theme::bg_elevated()
                            };
                            ui.painter().rect(
                                chip,
                                15.0,
                                bg,
                                egui::Stroke::new(1.0, Theme::border()),
                                egui::StrokeKind::Inside,
                            );
                            let galley = ui.fonts_mut(|f| {
                                f.layout_no_wrap(
                                    format!("{icon}  {t}"),
                                    egui::FontId::proportional(11.5),
                                    if resp.hovered() {
                                        Theme::text()
                                    } else {
                                        Theme::text_dim()
                                    },
                                )
                            });
                            ui.painter().galley(
                                egui::pos2(
                                    chip.center().x - galley.size().x / 2.0,
                                    chip.center().y - galley.size().y / 2.0,
                                ),
                                galley,
                                egui::Color32::WHITE,
                            );
                            resp.widget_info(|| {
                                egui::WidgetInfo::labeled(
                                    egui::WidgetType::Button,
                                    true,
                                    format!("{icon} {t}"),
                                )
                            });
                            if resp.clicked() {
                                self.input = t.clone();
                            }
                            x += w + gap;
                        }
                    }
                    let mut last_resp: Option<egui::Response> = None;
                    self.last_user_msg_rect = None;
                    for (idx, msg) in session.messages.iter().enumerate() {
                        // 消息首行顶（含附件时首行是正文气泡，render_message
                        // 返回的 resp 是最末行——直接用会把气泡定位到视口外）
                        let msg_top = ui.cursor().top();
                        let resp = self.render_message(ui, msg, idx as u64, false);
                        last_resp = Some(resp.clone());
                        if matches!(msg, Message::User { .. }) {
                            self.last_user_msg_rect = Some(egui::Rect::from_min_max(
                                egui::pos2(resp.rect.left(), msg_top),
                                resp.rect.right_bottom(),
                            ));
                        }
                    }
                    // 流式缓冲：只在回合运行中显示（中断/失败 ASSISTANT_MESSAGE
                    // 事件可能不出现，残留的旧流会变成"幽灵消息"钉在底部）
                    if has_stream {
                        let idx = session.messages.len();
                        let msg = Message::Assistant {
                            content: stream,
                            tool_calls: Vec::new(),
                            reasoning: None,
                        };
                        let resp = self.render_message(ui, &msg, idx as u64, true);
                        last_resp = Some(resp);
                    }
                    // 权限审批卡片（与 ask_user 选择卡片一致：消息区可点击）。
                    // 队列来自引擎审批注册表快照——跨会话的审批也可见
                    // （旧实现只在"恰好当前会话收到事件"时渲染，切走后卡片
                    // 丢失、引擎侧死等 300s 超时）
                    let mut pending_approvals: Vec<
                        crate::engine::approval::ApprovalRequest,
                    > = self
                        .engine
                        .lock()
                        .map(|e| e.pending_approvals())
                        .unwrap_or_default();
                    // 稳定排序（HashMap 无序 → 帧间跳变，读到的与点击
                    // resolve 的可能不是同一张；按注册序 = id 递增）
                    pending_approvals.sort_by(|a, b| a.id.cmp(&b.id));
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
                    let mut engine = self.engine.lock().unwrap_or_else(|p| p.into_inner());
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
                let resolved = engine.lock().unwrap_or_else(|p| p.into_inner()).resolve_approval(&aid, decision);
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
        // 附件条：吸附在输入框上方（悬浮 overlay，不占编辑区——历史缺陷：
        // 附件行占编辑区顶部 26px，输入框被挤下去、光标位置跳变）
        if !self.attachments.is_empty() {
            egui::Area::new(egui::Id::new("attach_strip"))
                .order(egui::Order::Middle)
                .fixed_pos(egui::pos2(inner.left(), input_rect.top() - 32.0))
                .show(ui.ctx(), |ui| {
                    egui::Frame::default()
                        .fill(Theme::bg_elevated())
                        .stroke(egui::Stroke::new(1.0, Theme::border()))
                        .corner_radius(egui::CornerRadius::same(8))
                        .inner_margin(egui::Margin::symmetric(8, 3))
                        .show(ui, |ui| {
                            ui.spacing_mut().item_spacing = egui::vec2(4.0, 0.0);
                            ui.horizontal(|ui| {
                                let mut remove: Option<usize> = None;
                                for (i, path) in self.attachments.iter().enumerate() {
                                    let name = path
                                        .file_name()
                                        .map(|n| n.to_string_lossy().into_owned())
                                        .unwrap_or_default();
                                    let short: String = name.chars().take(18).collect();
                                    let text = egui::RichText::new(format!("📎 {short} ✕"))
                                        .size(10.5)
                                        .color(Theme::text_dim());
                                    if ui
                                        .add(
                                            egui::Button::new(text)
                                                .fill(egui::Color32::TRANSPARENT)
                                                .stroke(egui::Stroke::NONE)
                                                .corner_radius(8.0)
                                                .min_size(egui::vec2(0.0, 18.0)),
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
                        });
                });
        }
        let edit_rect = egui::Rect::from_min_max(
            inner.min,
            egui::pos2(inner.right(), inner.bottom() - bar_h - 6.0),
        );
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
                    format!("+{n_att}")
                } else {
                    "+".to_string()
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
                let current = self.engine.lock().unwrap_or_else(|p| p.into_inner()).effective_model(&session.id);
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
                let current = self.engine.lock().unwrap_or_else(|p| p.into_inner()).effective_effort(&session.id);
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
                        SandboxMode::WorkspaceWrite => "仅工作区".into(),
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
                    "权限/沙箱：仅工作区=区外读写均拦截（系统基础与 ~/.dsh 除外）",
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
                let mut engine = self.engine.lock().unwrap_or_else(|p| p.into_inner());
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
            // 宽度钳制：极窄窗口下 chips 行可能已把 cursor 推过 bar 右缘，
            // 直接 intersect 会得到负宽（egui placer panic）。收缩到剩余宽度，
            // 绝不为负；空间为零时只占位不渲染内容（胶囊被裁是可接受的降级）。
            let bar_rem = (bar_rect.right() - ui.cursor().min.x).max(0.0);
            let right_w = rem.x.max(70.0).min(bar_rem);
            let right_rect = egui::Rect::from_min_size(
                ui.cursor().min,
                egui::vec2(right_w, bar_rect.height()),
            );
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
                        let mut engine = self.engine.lock().unwrap_or_else(|p| p.into_inner());
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
        // 斜杠命令：Enter 时展开为完整提示词；前缀未完整时补全（不发送
        // "/xxx" 原文）；未知命令提示可用命令。
        let mut proceed_send = true;
        if (send_clicked || entered) && self.input.trim().starts_with('/') {
            match parse_slash(&self.input.trim(), lang) {
                SlashParse::Expanded(text) => {
                    self.input = text; // 展开后的正文走下方正常发送
                }
                SlashParse::Complete(name) => {
                    self.input = format!("{name} ");
                    self.status = tr(
                        lang,
                        "已补全命令，输入参数后回车发送",
                        "Command completed; type args and press Enter",
                    )
                    .into();
                    proceed_send = false;
                }
                SlashParse::Unknown(t) => {
                    self.status = tr(
                        lang,
                        &format!("未知命令 {t}（可用：/plan /review /fix /test /continue）"),
                        &format!("Unknown command {t} (try /plan /review /fix /test /continue)"),
                    );
                    proceed_send = false;
                }
                SlashParse::NotSlash => {}
            }
        }
        if proceed_send
            && (send_clicked || entered)
            && (!self.input.trim().is_empty() || !self.attachments.is_empty())
        {
            // 附件分流：文本注入消息体；图片走 vision 多模态（路径随消息）。
            // 非 vision 模型带图发送：提示用户切换（API 会忽略图片）。
            let base = self.input.trim().to_string();
            let (text_files, images): (Vec<_>, Vec<_>) = self
                .attachments
                .iter()
                .cloned()
                .partition(|p| !is_image_path(p));
            let images: Vec<String> = images
                .iter()
                .map(|p| p.display().to_string())
                .collect();
            // 图片预检：超限（>8MB）的剔除并提示——否则发送时被静默跳过，
            // 且附件条已清空无法重试（历史链路 bug）。
            let mut images = images;
            let mut kept = Vec::new();
            for img in images.drain(..) {
                let over = std::fs::metadata(&img)
                    .map(|m| m.len() > 8 * 1024 * 1024)
                    .unwrap_or(true);
                if over {
                    let name = std::path::Path::new(&img)
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| img.clone());
                    self.status = tr(
                        lang,
                        &format!("已跳过 {name}（超过 8MB 或不可读）"),
                        &format!("Skipped {name} (over 8MB or unreadable)"),
                    );
                } else {
                    kept.push(img);
                }
            }
            let images = kept;
            let content = compose_message_with_attachments(&base, &text_files);
            if content.is_empty() && images.is_empty() {
                self.status = tr(lang, "消息为空", "Empty message").into();
            } else {
                let id = session.id.clone();
                let mut engine = self.engine.lock().unwrap_or_else(|p| p.into_inner());
                if !images.is_empty() {
                    let model = engine.effective_model(&id);
                    if !model.contains("vision") {
                        self.status = tr(
                            lang,
                            "当前模型不支持图片（已随消息保留，切换 vision 模型后生效）",
                            "Current model can't see images (kept; switch to a vision model)",
                        )
                        .into();
                    }
                }
                // 排队模式 + 回合运行中 → 入队不打断（回合结束由 pump 逐条发出）
                let running = {
                    let shared = engine.shared_sessions();
                    shared.get(id.as_str()).map(|x| x.running).unwrap_or(false)
                };
                if running && self.send_mode == SendMode::Queue {
                    let pos = engine.enqueue_message(&id, &content, images.clone());
                    self.input.clear();
                    self.attachments.clear();
                    self.status = tr(
                        lang,
                        &format!("已排队（第 {pos} 位，当前回合结束后执行）"),
                        &format!("Queued (#{pos}, runs after current turn)"),
                    );
                } else {
                    match engine.send_message_with_images(&id, &content, images) {
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

        // 斜杠命令浮层：输入 / 时在输入框上方列出匹配命令（点击补全；
        // 展开发生在发送时——见 parse_slash）。
        if self.input.starts_with('/') && !self.input.contains(' ') {
            let token = self.input.trim();
            let all = slash_commands(lang);
            let matches: Vec<&SlashCmd> =
                all.iter().filter(|c| c.name.starts_with(token)).collect();
            if !matches.is_empty() {
                let ctx = ui.ctx().clone();
                let row_h = 22.0;
                let h = matches.len() as f32 * row_h + 8.0;
                let pos = egui::pos2(input_rect.left() + 4.0, input_rect.top() - h - 6.0);
                egui::Area::new(ui.id().with("slash_popup"))
                    .order(egui::Order::Foreground)
                    .fixed_pos(pos)
                    .show(&ctx, |ui| {
                        egui::Frame::default()
                            .fill(Theme::bg_elevated())
                            .stroke(egui::Stroke::new(1.0, Theme::border()))
                            .corner_radius(egui::CornerRadius::same(8))
                            .inner_margin(egui::Margin::symmetric(8, 4))
                            .show(ui, |ui| {
                                ui.set_width((input_rect.width() * 0.7).min(440.0).max(260.0));
                                ui.spacing_mut().item_spacing.y = 0.0;
                                for c in &matches {
                                    let (rect, resp) = ui.allocate_exact_size(
                                        egui::vec2(ui.available_width(), row_h),
                                        egui::Sense::click(),
                                    );
                                    if resp.hovered() {
                                        ui.painter()
                                            .rect_filled(rect, 4.0, Theme::bg_hover());
                                    }
                                    let name_galley = ui.fonts_mut(|f| {
                                        f.layout_no_wrap(
                                            c.name.to_string(),
                                            egui::FontId::proportional(11.5),
                                            Theme::accent_light(),
                                        )
                                    });
                                    let desc_galley = ui.fonts_mut(|f| {
                                        f.layout_no_wrap(
                                            c.desc.clone(),
                                            egui::FontId::proportional(10.5),
                                            Theme::text_faint(),
                                        )
                                    });
                                    ui.painter().galley(
                                        egui::pos2(
                                            rect.left() + 6.0,
                                            rect.center().y - name_galley.size().y / 2.0,
                                        ),
                                        name_galley,
                                        Color32::WHITE,
                                    );
                                    ui.painter().galley(
                                        egui::pos2(
                                            rect.left() + 78.0,
                                            rect.center().y - desc_galley.size().y / 2.0,
                                        ),
                                        desc_galley,
                                        Color32::WHITE,
                                    );
                                    resp.widget_info(|| {
                                        egui::WidgetInfo::labeled(
                                            egui::WidgetType::Button,
                                            true,
                                            format!("{} {}", c.name, c.desc),
                                        )
                                    });
                                    if resp.clicked() {
                                        self.input = format!("{} ", c.name);
                                    }
                                }
                            });
                    });
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
            Message::User { content, images } => {
                // 右对齐：气泡贴右缘。宽度基准 = **clip_rect 宽（视口宽）**——
                // 不能用 available_width()/max_rect().width()：ScrollArea 内容若被
                // 超宽元素（长代码行/大 JSON）撑宽，两者都会变成 3000+，
                // left_space 随之巨大，用户气泡被推到窗口外完全不可见
                // （历史回归："用户消息看不到"，x≈3300 而窗口仅 1100 宽）。
                // clip_rect 是 ScrollArea 视口裁剪矩形，宽度固定不受内容影响。
                let avail_w = ui.clip_rect().width().max(60.0);
                // 长消息限宽 65% 换行（气泡内 label wrap 到该宽度）
                let max_bubble = ((avail_w - 32.0) * 0.65).max(80.0).min(avail_w - 32.0);
                // 附件折叠显示：附件块（--- + 【附件】头 + 全文围栏）
                // 在气泡里只显示附件名芯片行——全文仍随消息发给 AI
                // （compose 时注入），但气泡不再滚屏展示整个文件
                // （历史缺陷：上传 24KB 脚本，聊天窗被全文刷屏）。
                let (display, attach_names) = collapse_attachment_blocks(content);
                // 真实文本宽度：layout_no_wrap 对多行返回最长行宽。
                // 缓存按内容（弹幕动画期间整窗高频重绘，避免每帧重新排版）。
                let text_w = self
                    .user_width_cache
                    .get(content)
                    .copied()
                    .unwrap_or_else(|| {
                        let w = ui.fonts_mut(|f| {
                            f.layout_no_wrap(
                                display.clone(),
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
                // 正文气泡：附件芯片不再挤在气泡内——气泡宽度与文件名长度
                // 彻底解耦（历史缺陷：芯片被气泡宽度裁切，".sh" 等文件名
                // 尾段看不到）。纯附件（无正文）不渲染空气泡。
                // 消息响应 inner：纯附件（无正文、无芯片）时为 0 尺寸占位
                let mut inner = ui.allocate_response(egui::vec2(0.0, 0.0), egui::Sense::hover());
                if !display.trim().is_empty() {
                    // horizontal + add_space：left_space 基于固定视口宽，
                    // 不会撑宽父布局（left_space + 气泡宽 ≤ avail_w）
                    inner = ui.horizontal(|ui| {
                        let left_space = (avail_w - bubble_w - 24.0 - 8.0).max(4.0);
                        ui.add_space(left_space);
                        let frame = egui::Frame::default()
                            .fill(Theme::user_bubble())
                            .corner_radius(egui::CornerRadius::same(10))
                            .inner_margin(egui::Margin::symmetric(12, 8));
                        frame.show(ui, |ui| {
                            ui.set_max_width(bubble_w);
                            ui.label(
                                RichText::new(display).size(12.0).color(bubble_text_color()),
                            );
                        });
                    })
                    .response;
                }
                // 附件芯片：独立于气泡的"第二条消息"——一条附件一行，
                // 右对齐且与气泡右缘对齐；宽度不受气泡约束，整名可见，
                // 完整路径悬浮查看
                let chip_fill = if Theme::is_light() {
                    egui::Color32::from_black_alpha(14)
                } else {
                    egui::Color32::from_white_alpha(16)
                };
                let chip_stroke =
                    egui::Stroke::new(1.0, Theme::accent_light().gamma_multiply(0.35));
                let chip_font = egui::FontId::proportional(10.5);
                for (name, path) in &attach_names {
                    let mut short: String = name.chars().take(40).collect();
                    if name.chars().count() > 40 {
                        short.push('\u{2026}');
                    }
                    let mut label = format!("📎 {short}");
                    let mut galley = ui.painter().layout_no_wrap(
                        label.clone(),
                        chip_font.clone(),
                        Theme::accent_light(),
                    );
                    // 极窄窗口兜底：芯片右对齐后若超出视口右缘，动态收短
                    // （完整文件名悬浮可见），8 字符为下限
                    while galley.size().x + 12.0 > avail_w - 8.0
                        && short.chars().count() > 8
                    {
                        let n = short.chars().count();
                        short = short.chars().take(n - 1).collect();
                        label = format!("📎 {short}\u{2026}");
                        galley = ui.painter().layout_no_wrap(
                            label.clone(),
                            chip_font.clone(),
                            Theme::accent_light(),
                        );
                    }
                    let chip_w = galley.size().x + 12.0;
                    inner = ui.horizontal(|ui| {
                        // 芯片行没有气泡那 24px 内边距——右缘对齐气泡右缘（avail_w-8）
                        let left_space = (avail_w - chip_w - 8.0).max(4.0);
                        ui.add_space(left_space);
                        let (rect, resp) = ui.allocate_exact_size(
                            egui::vec2(chip_w, 18.0),
                            egui::Sense::hover(),
                        );
                        let resp = resp.on_hover_text(path);
                        resp.widget_info(|| {
                            egui::WidgetInfo::labeled(
                                egui::WidgetType::Label,
                                true,
                                label.clone(),
                            )
                        });
                        ui.painter().rect_filled(rect, 9.0, chip_fill);
                        ui.painter()
                            .rect_stroke(rect, 9.0, chip_stroke, egui::StrokeKind::Middle);
                        let pos = egui::pos2(
                            rect.center().x - galley.size().x / 2.0,
                            rect.center().y - galley.size().y / 2.0,
                        );
                        ui.painter().galley(pos, galley, Theme::accent_light());
                    })
                    .response;
                }
                // 附带图片：气泡下方右对齐缩略图行（点击放大复用查看器；
                // 复用 markdown 图片基建：本地路径 + http 均可）
                let resp = if let Some(imgs) = images.as_ref().filter(|v| !v.is_empty()) {
                    // 图片缓存上限（历史泄漏：无界增长）
                    if self.img_cache.len() > 256 {
                        self.img_cache.clear();
                    }
                    let img_cache = &mut self.img_cache;
                    let viewer = &mut self.viewer;
                    let http_imgs = &mut self.http_imgs;
                    let base_dir = self.current_session.as_ref().and_then(|s| s.cwd.clone());
                    ui.horizontal(|ui| {
                        // 右缘与气泡/附件芯片对齐（avail_w - 8）
                        let row_w =
                            (imgs.len() as f32 * 150.0).min(avail_w - 12.0);
                        let left_space = (avail_w - row_w - 8.0).max(4.0);
                        ui.add_space(left_space);
                        for (i, src) in imgs.iter().enumerate() {
                            crate::ui::markdown::render_image(
                                ui,
                                src,
                                "",
                                base_dir.as_deref(),
                                img_cache,
                                viewer,
                                http_imgs,
                                salt,
                                i,
                                140.0,
                            );
                        }
                    });
                    inner
                } else {
                    inner
                };
                resp
            }
            Message::Assistant {
                content,
                reasoning,
                ..
            } => {
                // 纯工具调用消息（content 为空）：不渲染空气泡（历史视觉缺陷：
                // 空白框 + 头像孤零零占一行）。但思考过程仍可回看（时间线）。
                if content.trim().is_empty() && reasoning.as_deref().map_or(true, |r| r.trim().is_empty()) {
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
                // 缓存上限（历史泄漏：无界增长 + 跨会话只增不减）
                if self.img_cache.len() > 256 {
                    self.img_cache.clear();
                }
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
                        // 思考时间线：弹幕划过后仍可回看（折叠，默认收起；
                        // id 按内容哈希——egui 记住开合状态，不占 self 字段）
                        if let Some(rs) =
                            reasoning.as_ref().filter(|r| !r.trim().is_empty())
                        {
                            ui.add_space(3.0);
                            let lang = self.lang;
                            egui::CollapsingHeader::new(
                                RichText::new(format!(
                                    "💭 {}（{} 字）",
                                    tr(lang, "思考过程", "Thinking"),
                                    rs.chars().count()
                                ))
                                .size(10.5)
                                .color(Theme::text_faint()),
                            )
                            .id_salt(egui::Id::new(rs))
                            .default_open(false)
                            .show(ui, |ui| {
                                egui::Frame::default()
                                    .fill(Theme::bg())
                                    .corner_radius(egui::CornerRadius::same(6))
                                    .inner_margin(egui::Margin::symmetric(8, 6))
                                    .show(ui, |ui| {
                                        ui.set_max_width(max_bubble.max(60.0));
                                        ui.label(
                                            RichText::new(rs)
                                                .size(10.5)
                                                .color(Theme::text_dim()),
                                        );
                                    });
                            });
                        }
                        // 消息反馈（👍/👎）：极弱图标常驻，点击高亮；记录到
                        // feedback registry + feedback/record 事件（JSONL 持久化）
                        if !streaming && !content.trim().is_empty() {
                            ui.add_space(3.0);
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing = egui::vec2(2.0, 0.0);
                                let mid = feedback_message_id(content);
                                let current = self
                                    .engine
                                    .lock()
                                    .ok()
                                    .and_then(|e| e.message_feedback(&mid));
                                let lang = self.lang;
                                // 复制整条消息（最高频需求之一）：30x20 命中区
                                //（历史缺陷 26x16 太小难点中）+ 复制后按钮旁
                                // 内联"已复制 ✓"（只写右下角状态太不显眼，
                                // 用户以为没生效）
                                {
                                    let content_owned = content.clone();
                                    let resp = ui.add_sized(
                                        [30.0, 20.0],
                                        egui::Label::new(
                                            RichText::new("⧉")
                                                .size(11.5)
                                                .color(Theme::text_faint()),
                                        ),
                                    )
                                    .interact(egui::Sense::click());
                                    let resp = resp.on_hover_text(tr(
                                        lang,
                                        "复制这条消息",
                                        "Copy this message",
                                    ));
                                    if resp.hovered() {
                                        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                                    }
                                    if resp.clicked() {
                                        ui.ctx().copy_text(content_owned);
                                        self.status = tr(lang, "已复制", "Copied").into();
                                        self.copy_flash =
                                            Some((mid.clone(), std::time::Instant::now()));
                                    }
                                }
                                for (label, kind) in [
                                    ("👍", crate::engine::FeedbackKind::Upvote),
                                    ("👎", crate::engine::FeedbackKind::Downvote),
                                ] {
                                    let active = current == Some(kind);
                                    let color = if active {
                                        match kind {
                                            crate::engine::FeedbackKind::Upvote => Theme::ok(),
                                            crate::engine::FeedbackKind::Downvote => Theme::err(),
                                        }
                                    } else {
                                        Theme::text_faint()
                                    };
                                    let resp = ui.add_sized(
                                        [22.0, 16.0],
                                        egui::Label::new(
                                            RichText::new(label).size(10.5).color(color),
                                        ),
                                    );
                                    let resp = resp.interact(egui::Sense::click());
                                    if resp.hovered() {
                                        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                                    }
                                    if resp.clicked() && !active {
                                        if let Some(sid) = self.current.clone() {
                                            if let Ok(mut e) = self.engine.lock() {
                                                let _ = e
                                                    .record_message_feedback(&sid, &mid, kind);
                                            }
                                            self.status = tr(
                                                lang,
                                                "感谢反馈",
                                                "Thanks for the feedback",
                                            )
                                            .into();
                                        }
                                    }
                                }
                                // 复制成功的内联反馈：1.5s 内在按钮旁显示
                                // "已复制 ✓"（绿色），明确告知已生效
                                if let Some((fid, t)) = &self.copy_flash {
                                    if fid == &mid && t.elapsed().as_secs_f32() < 1.5 {
                                        ui.label(
                                            RichText::new(tr(lang, "已复制 ✓", "Copied ✓"))
                                                .size(10.5)
                                                .color(Theme::ok()),
                                        );
                                    }
                                }
                            });
                        }
                    })
                    .response
            }
            Message::Tool { content, .. } => {
                // 直接 frame.show（左对齐）：不用 with_layout(left_to_right)——
                // 它会推进父布局 x cursor，污染后续消息的对齐。
                // 文件改动工具（write_file / str_replace_editor）：结果携带
                // diff → 渲染 diff 卡片（-红/+绿），用户可审阅 AI 改了什么。
                if let Some(card) = parse_diff_card(content) {
                    let mut req = self.open_file_req.take();
                    let resp = render_diff_card(ui, &card, &mut req);
                    self.open_file_req = req;
                    return resp;
                }
                // 摘要提取 stdout/content 等主字段（复用弹幕摘要逻辑），
                // 不展示原始 JSON 转义串；全量内容悬浮可看。
                // 结果带 path（read_file/list_dir/str_replace view…）→
                // 标题为可点击路径，点击在代码浏览器打开（ZCode 式跳转）。
                let path_hint: Option<String> =
                    serde_json::from_str::<serde_json::Value>(content)
                        .ok()
                        .and_then(|v| {
                            v.get("path")
                                .and_then(|p| p.as_str())
                                .map(String::from)
                        });
                let (brief, full) = summarize_tool_result(true, content);
                let frame = egui::Frame::default()
                    .fill(Theme::bg_hover())
                    .stroke(egui::Stroke::new(1.0, Theme::border()))
                    .corner_radius(egui::CornerRadius::same(8))
                    .inner_margin(egui::Margin::symmetric(10, 6));
                let lang = self.lang;
                let mut path_clicked: Option<String> = None;
                let resp = frame
                    .show(ui, |ui| {
                        ui.set_max_width((ui.available_width() * 0.92).max(80.0));
                        if let Some(path) = &path_hint {
                            let galley = ui.fonts_mut(|f| {
                                f.layout_no_wrap(
                                    format!("📄 {path}"),
                                    egui::FontId::proportional(11.0),
                                    Theme::accent_light(),
                                )
                            });
                            let (rect, t_resp) = ui.allocate_exact_size(
                                egui::vec2(galley.size().x + 8.0, 17.0),
                                egui::Sense::click(),
                            );
                            if t_resp.hovered() {
                                ui.painter()
                                    .rect_filled(rect, 4.0, Theme::bg_hover());
                                ui.ctx()
                                    .set_cursor_icon(egui::CursorIcon::PointingHand);
                            }
                            ui.painter().galley(
                                egui::pos2(
                                    rect.left() + 4.0,
                                    rect.center().y - galley.size().y / 2.0,
                                ),
                                galley,
                                Color32::WHITE,
                            );
                            let _ = t_resp.clone().on_hover_text(tr(
                                lang,
                                "点击在代码浏览器打开此文件",
                                "Click to open this file in the code browser",
                            ));
                            if t_resp.clicked() {
                                path_clicked = Some(path.clone());
                            }
                            ui.add_space(2.0);
                        }
                        ui.label(
                            RichText::new(truncate(&brief, 160))
                                .color(Theme::text_dim())
                                .monospace(),
                        )
                    })
                    .response;
                if let Some(path) = path_clicked {
                    self.open_file_req = Some(std::path::PathBuf::from(path));
                }
                resp.on_hover_text(full)
            }
        };
        resp
    }
}

/// 内置斜杠命令（高频任务的提示词模板）。`{args}` 为用户参数占位。
struct SlashCmd {
    name: &'static str,
    /// 参数缺省文案（None = 参数必填）
    default_args: Option<&'static str>,
    desc: String,
    template: String,
}

fn slash_commands(lang: Lang) -> Vec<SlashCmd> {
    vec![
        SlashCmd {
            name: "/plan",
            default_args: None,
            desc: tr(lang, "先制定分步计划再执行", "Plan first, then execute"),
            template: tr(lang,
                "请先为以下任务制定分步计划（用 plan_write 写入计划卡片，每完成一步就更新勾选状态），然后逐步执行并汇报进度：
{args}",
                "First write a step-by-step plan for the following task (use plan_write; tick off steps as you complete them), then execute it step by step and report progress:
{args}"),
        },
        SlashCmd {
            name: "/review",
            default_args: Some("当前项目"),
            desc: tr(lang, "代码审查（正确性/安全/可维护性）", "Code review"),
            template: tr(lang,
                "请审查 {args} 的代码：先读 AGENTS.md（若有）了解项目约定，重点检查正确性、安全问题与可维护性；输出按严重度排序的问题清单与具体修复建议。不要直接修改文件。",
                "Review the code of {args}: read AGENTS.md first if present; focus on correctness, security and maintainability; output a severity-ordered issue list with concrete fix suggestions. Do not modify files directly."),
        },
        SlashCmd {
            name: "/fix",
            default_args: None,
            desc: tr(lang, "定位根因并修复", "Locate root cause and fix"),
            template: tr(lang,
                "请分析并修复这个问题：{args}
先读相关代码定位根因并简述；然后用最小改动修复（str_replace_editor / write_file），最后运行相关测试验证。",
                "Analyze and fix this issue: {args}
Read the relevant code first, explain the root cause briefly; then fix it with a minimal change (str_replace_editor / write_file); finally run the relevant tests to verify."),
        },
        SlashCmd {
            name: "/test",
            default_args: Some("本次改动"),
            desc: tr(lang, "补齐/运行测试", "Add or run tests"),
            template: tr(lang,
                "请为 {args} 补充或运行测试：先了解现有测试结构，补齐关键路径用例，然后运行验证并汇报结果。",
                "Add or run tests for {args}: study the existing test layout, cover the key paths, then run them and report."),
        },
        SlashCmd {
            name: "/continue",
            default_args: Some(""),
            desc: tr(lang, "从上次中断处继续", "Continue from where it stopped"),
            template: tr(lang,
                "继续当前任务，从上次中断处推进，直到完成或需要我决策。",
                "Continue the current task from where it stopped, until done or a decision from me is needed."),
        },
    ]
}

/// 斜杠输入的解析结果。
enum SlashParse {
    NotSlash,
    /// 展开为完整提示词（可直接发送）
    Expanded(String),
    /// 前缀匹配未完整：补全到该命令（等用户补参数）
    Complete(String),
    /// 无匹配
    Unknown(String),
}

fn parse_slash(input: &str, lang: Lang) -> SlashParse {
    if !input.starts_with('/') {
        return SlashParse::NotSlash;
    }
    let token = input.split_whitespace().next().unwrap_or("/");
    let rest = input[token.len()..].trim().to_string();
    let cmds = slash_commands(lang);
    if let Some(c) = cmds.iter().find(|c| c.name == token) {
        let args = if rest.is_empty() {
            match c.default_args {
                Some(d) => d.to_string(),
                // 必填参数缺失：展开为用法提示（模型会友好回应）
                None => format!("（未提供任务描述。用法：{} <任务描述>）", c.name),
            }
        } else {
            rest
        };
        return SlashParse::Expanded(c.template.replace("{args}", &args));
    }
    let matches: Vec<&SlashCmd> = cmds.iter().filter(|c| c.name.starts_with(token)).collect();
    if let Some(first) = matches.first() {
        return SlashParse::Complete(first.name.to_string());
    }
    SlashParse::Unknown(token.to_string())
}

/// 反馈用消息标识：内容哈希（跨重启稳定；同内容去重可接受）。
fn feedback_message_id(content: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    content.hash(&mut h);
    format!("m{:016x}", h.finish())
}

/// diff 卡片数据（从工具结果 JSON 提取）。
struct DiffCard {
    path: String,
    /// `-old/+new` 行流（None = 超规模降级，用 note 摘要）
    diff: Option<String>,
    note: Option<String>,
    created: bool,
}

/// 工具结果是否为"文件改动"：带 diff / diff_note / created 字段。
fn parse_diff_card(content: &str) -> Option<DiffCard> {
    let v: serde_json::Value = serde_json::from_str(content).ok()?;
    let path = v.get("path").and_then(|p| p.as_str())?.to_string();
    let diff = v
        .get("diff")
        .and_then(|d| d.as_str())
        .filter(|d| !d.is_empty())
        .map(String::from);
    let note = v
        .get("diff_note")
        .and_then(|d| d.as_str())
        .map(String::from);
    let created = v.get("created").and_then(|c| c.as_bool()).unwrap_or(false);
    if diff.is_none() && note.is_none() && !created {
        return None;
    }
    Some(DiffCard {
        path,
        diff,
        note,
        created,
    })
}

/// diff 卡片：✎ path (+a −b) 标题 + 红/绿等宽行（滚动限高，悬停看全文）。
fn render_diff_card(
    ui: &mut egui::Ui,
    card: &DiffCard,
    open_req: &mut Option<std::path::PathBuf>,
) -> egui::Response {
    let mut title_clicked = false;
    let frame = egui::Frame::default()
        .fill(Theme::bg_elevated())
        .stroke(egui::Stroke::new(1.0, Theme::border()))
        .corner_radius(egui::CornerRadius::same(8))
        .inner_margin(egui::Margin::symmetric(10, 6));
    let resp = frame
        .show(ui, |ui| {
            ui.set_max_width((ui.available_width() * 0.92).max(80.0));
            // 标题：✎ path (+a −b) / 新建 —— 可点击：在内置代码浏览器
            // 打开该文件（AI 改完即可点路径直达源码）
            let title = if let Some(diff) = &card.diff {
                let (del, add) = crate::core::diff::counts(diff);
                format!("✎ {}  (+{add} −{del})", card.path)
            } else if card.created {
                format!("✎ {}  (新建)", card.path)
            } else {
                format!("✎ {}", card.path)
            };
            let galley = ui.fonts_mut(|f| {
                f.layout_no_wrap(
                    title.clone(),
                    egui::FontId::proportional(11.0),
                    Theme::accent_light(),
                )
            });
            let (t_rect, t_resp) = ui.allocate_exact_size(
                egui::vec2(galley.size().x + 8.0, 18.0),
                egui::Sense::click(),
            );
            if t_resp.hovered() {
                ui.painter()
                    .rect_filled(t_rect, 4.0, Theme::bg_hover());
                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            }
            ui.painter().galley(
                egui::pos2(
                    t_rect.left() + 4.0,
                    t_rect.center().y - galley.size().y / 2.0,
                ),
                galley,
                Color32::WHITE,
            );
            let _ = t_resp.clone().on_hover_text(tr(
                crate::ui::i18n::Lang::Zh,
                "点击在代码浏览器打开此文件",
                "Click to open this file in the code browser",
            ));
            t_resp.widget_info(|| {
                egui::WidgetInfo::labeled(egui::WidgetType::Button, true, title.clone())
            });
            if t_resp.clicked() {
                title_clicked = true;
            }
            if let Some(note) = &card.note {
                ui.label(RichText::new(note).size(10.5).color(Theme::text_faint()));
            }
            if let Some(diff) = &card.diff {
                ui.add_space(2.0);
                egui::ScrollArea::vertical()
                    .id_salt(ui.id().with(("diff", &card.path)))
                    .max_height(180.0)
                    .show(ui, |ui| {
                        ui.spacing_mut().item_spacing.y = 0.0;
                        for line in diff.lines() {
                            let (color, text) = match line.chars().next() {
                                Some('-') => (Theme::err(), line),
                                Some('+') => (Theme::ok(), line),
                                _ => (Theme::text_faint(), line),
                            };
                            ui.label(
                                RichText::new(text)
                                    .monospace()
                                    .size(10.5)
                                    .color(color),
                            );
                        }
                    });
            }
        })
        .response
        .on_hover_text(tr(
            crate::ui::i18n::Lang::Zh,
            "AI 的文件改动（红=删除，绿=新增）；点击标题在代码浏览器打开",
            "AI file edit (red=removed, green=added); click the title to open in the code browser",
        ));
    if title_clicked {
        *open_req = Some(std::path::PathBuf::from(&card.path));
    }
    resp
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
/// 图片附件判定（与 llm::image_mime 的扩展名集保持一致）。
fn is_image_path(p: &std::path::Path) -> bool {
    let ext = p
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();
    matches!(ext.as_str(), "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp")
}

/// 折叠用户消息里的附件块：把 compose_message_with_attachments 生成的
/// "---\\n【附件 i/n：name】（path）\\n<内容>" 块替换为空（正文保留），
/// 返回 (显示文本, [(文件名, 完整路径)])。悬浮芯片显示完整路径。
pub fn collapse_attachment_blocks(content: &str) -> (String, Vec<(String, String)>) {
    let mut names = Vec::new();
    if !content.contains("【附件") {
        return (content.to_string(), names);
    }
    let mut out = String::new();
    let mut rest = content;
    while let Some(pos) = rest.find("---\n【附件") {
        out.push_str(&rest[..pos]);
        let after = &rest[pos..];
        // 块尾 = 下一个 "---\\n【附件" 或字符串末尾
        let next = after[4..]
            .find("---\n【附件")
            .map(|o| o + 4)
            .unwrap_or(after.len());
        let block = &after[..next];
        // 解析 "【附件 i/n：name】（path）" 行
        for line in block.lines() {
            if line.starts_with("【附件") {
                let name = line
                    .find("：")
                    .and_then(|o| line[o + 3..].find("】").map(|e| line[o + 3..o + 3 + e].to_string()))
                    .unwrap_or_default();
                let path = line
                    .find("（")
                    .and_then(|o| line.find("）").map(|e| line[o + "（".len()..e].to_string()))
                    .unwrap_or_default();
                if !name.is_empty() {
                    names.push((name, path));
                }
                break;
            }
        }
        rest = &after[next..];
    }
    out.push_str(rest);
    (out.trim().to_string(), names)
}

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
pub enum PanelKind {
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
                    let images = data
                        .get("images")
                        .and_then(|v| v.as_array())
                        .map(|a| {
                            a.iter()
                                .filter_map(|x| x.as_str().map(String::from))
                                .collect::<Vec<String>>()
                        })
                        .filter(|v| !v.is_empty());
                    Message::User {
                        content: content.to_string(),
                        images,
                    }
                } else {
                    let tool_calls = data
                        .get("tool_calls")
                        .and_then(|v| {
                            serde_json::from_value::<Vec<crate::core::ToolCall>>(v.clone()).ok()
                        })
                        .unwrap_or_default();
                    let reasoning = data
                        .get("reasoning_content")
                        .and_then(|v| v.as_str())
                        .map(String::from)
                        .filter(|r| !r.is_empty());
                    Message::Assistant {
                        content: content.to_string(),
                        tool_calls,
                        reasoning,
                    }
                };
                messages.push(msg);
            }
        }
    }
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
    // 友好名称映射：内置工具用短中文标签（弹幕滚动时一眼可辨）
    const LABELS: &[(&str, &str)] = &[
        ("bash", "执行命令"),
        ("pwsh", "执行命令"),
        ("read_file", "读文件"),
        ("write_file", "写文件"),
        ("list_dir", "列目录"),
        ("str_replace_editor", "编辑文件"),
        ("fs_search", "搜索文件"),
        ("web_search", "网页搜索"),
        ("todo_write", "更新任务"),
        ("ask_user", "询问用户"),
        ("goal_create", "创建目标"),
        ("plan_write", "制定计划"),
        ("exit_plan_mode", "退出计划"),
        ("subagent_fork", "派生子代理"),
        ("load_skill", "加载技能"),
        ("node_called", "调用Node"),
        ("run_code", "组合执行"),
        ("take_screenshot", "屏幕截图"),
        ("mouse_click", "鼠标点击"),
        ("mouse_move", "移动鼠标"),
        ("mouse_drag", "鼠标拖拽"),
        ("mouse_scroll", "滚动滚轮"),
        ("key_type", "键入文本"),
        ("key_press", "按键"),
        ("read_image", "读图分析"),
        ("read_url", "读取网页"),
    ];
    let label = LABELS.iter().find(|(n, _)| *n == name).map(|(_, l)| *l).unwrap_or(name);

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
        Some(m) => (format!("🔧 {label} {}", truncate(&m, 40)), full),
        None => (format!("🔧 {label} {}", truncate(trimmed, 40)), full),
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

/// 会话列表右缘的相对时间（极简阶梯：刚刚 / N分 / N时 / 昨天 / N天 / 更早）。
/// `elapsed_secs` 是距今秒数（负值/NaN 按"刚刚"兜底）。
fn relative_time(elapsed_secs: f64, lang: crate::ui::i18n::Lang) -> String {
    use crate::ui::i18n::tr;
    let s = if elapsed_secs.is_finite() && elapsed_secs > 0.0 {
        elapsed_secs as i64
    } else {
        0
    };
    let (zh, en): (String, String) = match s {
        0..=59 => ("刚刚".into(), "now".into()),
        60..=3599 => (format!("{}分", s / 60), format!("{}m", s / 60)),
        3600..=86399 => (format!("{}时", s / 3600), format!("{}h", s / 3600)),
        86400..=172799 => ("昨天".into(), "1d".into()),
        172800..=604799 => (format!("{}天", s / 86400), format!("{}d", s / 86400)),
        _ => ("更早".into(), "old".into()),
    };
    tr(lang, &zh, &en).into()
}

#[cfg(test)]
mod tests {
    use super::*;

#[test]
fn collapse_attachment_blocks_extracts_names() {
    let base = "看一下这个脚本";
    let f = std::path::PathBuf::from("E:\\AI\\install-ada-rs.sh");
    let composed = compose_message_with_attachments(base, &[f.clone()]);
    assert!(composed.contains("【附件 1/1：install-ada-rs.sh】"));
    let (display, names) = collapse_attachment_blocks(&composed);
    assert_eq!(
        display, base,
        "正文保留,附件块(含全文围栏)整体折叠"
    );
    assert_eq!(names.len(), 1);
    assert_eq!(names[0].0, "install-ada-rs.sh");
    assert_eq!(names[0].1, "E:\\AI\\install-ada-rs.sh");
    // 无附件消息原样
    let (d2, n2) = collapse_attachment_blocks("普通消息");
    assert_eq!(d2, "普通消息");
    assert!(n2.is_empty());
    // 多附件
    let g = std::path::PathBuf::from("D:\\two.txt");
    let composed2 = compose_message_with_attachments("多附件", &[f.clone(), g]);
    let (_, names2) = collapse_attachment_blocks(&composed2);
    assert_eq!(names2.len(), 2);
    assert_eq!(names2[1].0, "two.txt");
}


    use super::*;

    

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
mod slash_tests {
    use super::*;

    /// 斜杠命令解析：展开 / 补全 / 未知 / 非命令 / 缺省参数。
    #[test]
    fn slash_parse_paths() {
        let lang = Lang::Zh;
        // 非命令
        assert!(matches!(parse_slash("普通消息", lang), SlashParse::NotSlash));
        // 完整命令 + 参数 → 展开含参数
        match parse_slash("/plan 重构登录模块", lang) {
            SlashParse::Expanded(t) => {
                assert!(t.contains("重构登录模块"), "{t}");
                assert!(t.contains("plan_write"), "模板生效");
            }
            _ => panic!("应展开"),
        }
        // 带缺省参数的命令：空参数 → 缺省文案
        match parse_slash("/review", lang) {
            SlashParse::Expanded(t) => assert!(t.contains("当前项目"), "{t}"),
            _ => panic!("应展开"),
        }
        // 必填参数缺失 → 用法提示（不 panic）
        match parse_slash("/fix", lang) {
            SlashParse::Expanded(t) => assert!(t.contains("用法"), "{t}"),
            _ => panic!("应展开为用法提示"),
        }
        // 前缀未完整 → 补全（/re → /review；/p 歧义取第一个 /plan）
        assert!(matches!(parse_slash("/re", lang), SlashParse::Complete(n) if n == "/review"));
        assert!(matches!(parse_slash("/p", lang), SlashParse::Complete(n) if n == "/plan"));
        // 完整无参命令 → 展开（不含 {args} 残留）
        match parse_slash("/continue", lang) {
            SlashParse::Expanded(t) => {
                assert!(!t.contains("{args}"), "占位符必须被替换: {t}");
            }
            _ => panic!("应展开"),
        }
        // 未知
        assert!(matches!(parse_slash("/nope", lang), SlashParse::Unknown(_)));
    }
}

#[cfg(test)]
mod diff_card_tests {
    use super::*;

    /// diff 卡片解析：带 diff / 降级 note / created / 普通工具结果。
    #[test]
    fn parse_diff_card_shapes() {
        // 带 diff → 卡片
        let json = serde_json::json!({
            "path": "src/a.rs",
            "replaced": true,
            "diff": "-old\n+new\n"
        })
        .to_string();
        let c = parse_diff_card(&json).unwrap();
        assert_eq!(c.path, "src/a.rs");
        assert_eq!(c.diff.as_deref(), Some("-old\n+new\n"));

        // 超规模降级 note → 卡片
        let c = parse_diff_card(
            r#"{"path":"big.log","diff_note":"整文件重写：100 行 → 3 行"}"#,
        )
        .unwrap();
        assert!(c.diff.is_none());
        assert!(c.note.is_some());

        // 新建（无 diff 文本）→ 卡片（标题走"新建"分支）
        let c = parse_diff_card(r#"{"path":"new.txt","created":true}"#).unwrap();
        assert!(c.created);

        // 普通工具结果（bash/read）→ 不是 diff 卡片
        assert!(parse_diff_card(r#"{"stdout":"ok"}"#).is_none());
        assert!(parse_diff_card("not json").is_none());
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
