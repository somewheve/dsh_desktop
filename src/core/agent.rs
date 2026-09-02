//! Agent 循环：turn/step 驱动 + LLM 调用 + 工具调度。
//!
//! 对齐 DSH dsh-agent 的语义（turn 边界、inbox 输入、dispatch 工具分发、
//! tool/call + tool/result 事件、assistant/chunk 流式），用 Rust 原生重写。
//! 无外部 Node 依赖，纯内存 + 本地 JSONL 持久化。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::sync::Arc;

use anyhow::Result;
use log::{debug, info, warn};
use serde_json::{json, Value};

use super::llm::{
    finish_tool_call, LlmClient, LlmMessage, LlmRole, StreamEvent, REASONING_EFFORTS,
};
use super::preset::AgentPreset;
use super::session::{types, Message, Session, SessionEvent, ToolCall};
use super::settings::EngineSettings;
use super::storage::{truncate_for_llm, SessionStore};
use super::tools::{ToolOutput, ToolRegistry};
use super::workspace::WorkspaceManager;
use crate::engine::jobs::JobManager;
use crate::engine::subagent::{SubagentManager, SubagentStatus};

/// agent 回合状态。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AgentStatus {
    Idle,
    Running,
    Stopped,
}

/// 一个回合的控制信号（send_message 时创建，cancel/interject 置位）。
///
/// `cancelled`（插话）：打断当前 step，回合吸收新用户消息后**续跑**；
/// `stopping`（停止）：回合在最近的检查点退出，**不续跑**。
/// 分开两个标志是为了消除旧实现"靠 store 消息数差异猜测用户意图"的歧义。
#[derive(Default)]
pub struct TurnSignal {
    cancelled: std::sync::atomic::AtomicBool,
    stopping: std::sync::atomic::AtomicBool,
}

impl TurnSignal {
    fn is_set(f: &std::sync::atomic::AtomicBool) -> bool {
        f.load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// 引擎对外通知（UI 订阅）。
#[derive(Debug, Clone)]
pub enum EngineEvent {
    SessionCreated {
        session_id: String,
    },
    Event {
        session_id: String,
        event: SessionEvent,
    },
    StatusChanged {
        session_id: String,
        status: AgentStatus,
    },
    Error {
        session_id: String,
        message: String,
    },
    /// 越权写操作请求用户确认（UI 弹审批卡片）。
    ApprovalRequested {
        session_id: String,
        id: String,
        target: String,
        reason: String,
    },
    /// 定时任务到期（后台定时线程驱动，不依赖 UI 帧）。
    ScheduledTaskDue {
        id: String,
        name: String,
    },
}

pub struct DshEngine {
    settings: EngineSettings,
    store: SessionStore,
    /// 无 API key 时为 None（UI 仍可用，发送时提示配置）
    llm: Option<Arc<LlmClient>>,
    tools: ToolRegistry,
    sessions: HashMap<String, Session>,
    /// 共享给 agent 线程的会话 map（异步回合内更新内存）
    sessions_shared: Arc<std::sync::Mutex<HashMap<String, Session>>>,
    workspaces: WorkspaceManager,
    workspace_root: Option<PathBuf>,
    /// 新建会话默认预设（默认标准模式）
    default_preset: AgentPreset,
    /// 沙箱模式（默认 danger-full-access：保持无沙箱直通的既有语义；
    /// 可通过 set_sandbox_mode 切换为 workspace-write / read-only 受限执行）。
    sandbox_mode: crate::exec::SandboxMode,
    /// 当前回合的信号（每会话一份；send_message 启动新回合时整体替换，
    /// 旧回合线程持有旧 Arc —— 新回合的取消/停止不会误伤旧回合的收尾）。
    turn_signals: HashMap<String, Arc<TurnSignal>>,
    /// 每会话回合互斥锁：同一会话的回合串行执行（新回合等待旧回合线程
    /// 完全退出后才启动，杜绝两个回合线程并发写同一 JSONL）。
    turn_locks: Arc<std::sync::Mutex<HashMap<String, Arc<std::sync::Mutex<()>>>>>,
    tx: Sender<EngineEvent>,
    /// 任务管理器（回合 / 子代理等耗时操作的可见进度）
    jobs: Arc<std::sync::Mutex<JobManager>>,
    /// 子代理管理器（subagent/descriptor 事件 + 运行状态）
    subagents: Arc<std::sync::Mutex<SubagentManager>>,
    /// 技能注册表（$DSH_HOME/skills 加载；agent 可用 load_skill 调用）
    skills: crate::engine::skill::SkillRegistry,
    /// 用户技能目录
    skills_dir: PathBuf,
    /// cordis 风格子进程插件（$DSH_HOME/plugins）
    plugins: Arc<std::sync::Mutex<crate::engine::plugin::PluginManager>>,
    /// 权限审批注册表（写工作区外的用户确认通道）
    approvals: Arc<std::sync::Mutex<crate::engine::approval::ApprovalRegistry>>,
    /// 定时任务调度器（后台定时线程每秒驱动，到期发 ScheduledTaskDue）
    scheduler: Arc<std::sync::Mutex<crate::engine::schedule::Scheduler>>,
    /// 排队消息（会话级 FIFO）：回合运行中以"排队"模式发送的消息，
    /// 回合结束后由 pump_message_queue 逐条自动发起
    queued_messages: std::collections::HashMap<
        String,
        std::collections::VecDeque<(String, Vec<String>)>,
    >,
    /// 会话级 token 用量累计（LLM usage 事件累加；内存态，重启清零）
    token_usage: Arc<std::sync::Mutex<HashMap<String, crate::core::llm::TokenUsage>>>,
    /// 消息反馈（👍👎；registry 内存态 + feedback/record 事件持久化）
    feedback: Arc<std::sync::Mutex<crate::engine::FeedbackStore>>,
    /// token 用量有更新（回合线程置位；UI 帧循环 flush 落盘）
    usage_dirty: Arc<std::sync::atomic::AtomicBool>,
    /// 排队消息重试计数 (session_id, 消息) → 次数（超过 2 次丢弃）
    queue_retries: HashMap<(String, String), u32>,
    /// 工具调用教训库（失败记忆 → 修正配对 → 下次直跑修正版；
    /// 持久化 data_dir/tool_lessons.json，按工作区分域）
    lessons: Arc<std::sync::Mutex<crate::engine::lessons::LessonStore>>,
}

impl DshEngine {
    pub fn new(settings: EngineSettings, tx: Sender<EngineEvent>) -> Result<Self> {
        let store = SessionStore::new(settings.data_dir.clone())?;
        let mut settings = settings;
        if settings.api_key.is_none() {
            // 尝试从 DSH credentials 读取（不持久化——避免测试/临时设置污染用户配置）
            let _ = settings.load_api_key_from_dsh();
        }
        let llm = match settings.api_key.as_ref() {
            Some(key) => LlmClient::new(
                settings.base_url.clone(),
                key.clone(),
                settings.model.clone(),
                settings.reasoning_effort.clone(),
                settings.http_proxy.clone(),
            )
            .map(Arc::new)
            .ok(),
            None => {
                log::warn!("API key 未配置：会话界面可用，发送消息前请在设置页填写");
                None
            }
        };
        let http_proxy_for_tools = settings.http_proxy.clone();
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        // 沙箱模式：从持久化设置恢复（用户上次选择的模式），缺省直通。
        let sandbox_mode = settings
            .sandbox_mode
            .as_deref()
            .and_then(crate::exec::SandboxMode::parse)
            .unwrap_or(crate::exec::SandboxMode::DangerFullAccess);
        log::info!("sandbox mode restored: {}", sandbox_mode.as_str());
        let sessions_shared = Arc::new(std::sync::Mutex::new(HashMap::new()));
        let default_preset =
            AgentPreset::parse(&settings.default_preset).unwrap_or(AgentPreset::Standard);
        // 技能：默认 $DSH_HOME/skills（不存在自动创建）
        let skills_dir = settings
            .skills_dir
            .clone()
            .unwrap_or_else(|| crate::config::AppConfig::load().skills_dir());
        let mut skills = crate::engine::skill::SkillRegistry::default();
        skills.load_from_dir(&skills_dir);
        info!(
            "skills loaded: {} from {}",
            skills.len(),
            skills_dir.display()
        );
        let plugins_dir = settings
            .plugins_dir
            .clone()
            .unwrap_or_else(|| crate::config::AppConfig::load().dsh_home.join("plugins"));
        let plugins = Arc::new(std::sync::Mutex::new(
            crate::engine::plugin::PluginManager::new(plugins_dir),
        ));
        plugins.lock().unwrap().discover();
        let (pcount, pdir) = {
            let mgr = plugins.lock().unwrap();
            (mgr.plugins.len(), mgr.dir.clone())
        };
        info!("plugins discovered: {pcount} from {}", pdir.display());
        // 定时任务调度器 + 后台驱动线程（到期发 ScheduledTaskDue 事件）
        let scheduler = Arc::new(std::sync::Mutex::new(
            crate::engine::schedule::Scheduler::default(),
        ));
        // 定时任务重启恢复（settings 持久化；next_at 按 interval 重新排程）
        for t in &settings.scheduled_tasks {
            scheduler
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .add(&t.id, &t.name, t.interval_secs, &t.prompt, &t.session_id);
            if !t.enabled {
                scheduler
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .toggle(&t.id, false);
            }
        }
        Self::spawn_scheduler_thread(scheduler.clone(), tx.clone());
        // 自动启动 autostart 插件（安装即生效：agent 回合可直接调用其工具）
        {
            let mut mgr = plugins.lock().unwrap();
            let names: Vec<String> = mgr
                .plugins
                .iter()
                .filter(|(_, p)| p.manifest.autostart)
                .map(|(n, _)| n.clone())
                .collect();
            for n in names {
                if let Some(p) = mgr.get_mut(&n) {
                    if let Err(e) = p.start() {
                        p.status = crate::engine::plugin::PluginStatus::Failed(e);
                    }
                }
            }
            info!("plugins autostarted: {}", mgr.plugins.len());
        }
        info!("engine core ready");
        // 教训库文件路径（data_dir/tool_lessons.json；加载失败从空库开始）
        let store_dir_for_lessons = settings.data_dir.join("tool_lessons.json");
        // token 用量恢复（settings 持久化）
        let mut usage_map: HashMap<String, crate::core::llm::TokenUsage> = HashMap::new();
        for u in &settings.token_usage {
            usage_map.insert(
                u.session_id.clone(),
                crate::core::llm::TokenUsage {
                    prompt_tokens: u.prompt_tokens,
                    completion_tokens: u.completion_tokens,
                },
            );
        }
        Ok(Self {
            settings,
            store,
            llm,
            tools: {
                // 权限审批的写边界 = 初始工作目录（写此之外需用户确认）
                let ws_root = cwd.clone();
                let sb = if matches!(sandbox_mode, crate::exec::SandboxMode::DangerFullAccess) {
                    None
                } else {
                    Some(crate::exec::acl::WindowsAclSandbox::new(
                        sandbox_mode,
                        cwd.clone(),
                    ))
                };
                ToolRegistry::new(cwd)
                    .with_workspace_root(Some(ws_root))
                    .with_sandbox(sb)
                    .with_http_proxy(http_proxy_for_tools.clone())
            },
            sessions: HashMap::new(),
            sessions_shared,
            workspaces: WorkspaceManager::default(),
            workspace_root: None,
            default_preset,
            sandbox_mode,
            turn_signals: HashMap::new(),
            turn_locks: Arc::new(std::sync::Mutex::new(HashMap::new())),
            tx,
            jobs: Arc::new(std::sync::Mutex::new(JobManager::default())),
            subagents: Arc::new(std::sync::Mutex::new(SubagentManager::default())),
            skills,
            skills_dir,
            plugins,
            approvals: Arc::new(std::sync::Mutex::new(
                crate::engine::approval::ApprovalRegistry::new(),
            )),
            scheduler,
            queued_messages: std::collections::HashMap::new(),
            token_usage: Arc::new(std::sync::Mutex::new(usage_map)),
            feedback: Arc::new(std::sync::Mutex::new(
                crate::engine::FeedbackStore::default(),
            )),
            usage_dirty: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            queue_retries: HashMap::new(),
            lessons: Arc::new(std::sync::Mutex::new(
                crate::engine::lessons::LessonStore::load(
                    store_dir_for_lessons.clone(),
                ),
            )),
        })
    }

    /// 测试辅助：置用量脏标记（真实路径是回合线程的 Usage 事件）。
    #[cfg(test)]
    pub(crate) fn mark_usage_dirty_for_test(&self) {
        self.usage_dirty
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// 用量落盘（UI 帧循环调用；无更新时为一次原子读，零开销）。
    pub fn flush_usage(&mut self) {
        if !self
            .usage_dirty
            .swap(false, std::sync::atomic::Ordering::Relaxed)
        {
            return;
        }
        let map = self
            .token_usage
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        self.settings.token_usage = map
            .iter()
            .map(|(sid, u)| crate::core::settings::StoredTokenUsage {
                session_id: sid.clone(),
                prompt_tokens: u.prompt_tokens,
                completion_tokens: u.completion_tokens,
            })
            .collect();
        let _ = self.settings.save();
    }

    /// 会话累计 token 用量（prompt + completion，内存态）。
    pub fn token_usage(&self, session_id: &str) -> crate::core::llm::TokenUsage {
        self.token_usage
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(session_id)
            .copied()
            .unwrap_or_default()
    }

    /// 全部会话合计 token 用量。
    pub fn token_usage_total(&self) -> crate::core::llm::TokenUsage {
        self.token_usage
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .values()
            .fold(Default::default(), |mut acc, u| {
                acc += *u;
                acc
            })
    }

    /// 当前会话上下文占用估算（token，粗估：CJK 字 0.75/字、其余 4 字符/词）。
    /// 与模型上下文窗口之比供 UI 展示"上下文用量 ≈ N%"。
    pub fn context_estimate(&self, session_id: &str) -> u64 {
        let messages = self
            .sessions
            .get(session_id)
            .map(|s| s.messages.clone())
            .or_else(|| {
                lock_shared(&self.sessions_shared)
                    .get(session_id)
                    .map(|s| s.messages.clone())
            })
            .unwrap_or_default();
        let text_len = messages
            .iter()
            .map(|m| match m {
                crate::core::session::Message::User { content, .. } => content.len(),
                crate::core::session::Message::Assistant { content, .. } => content.len(),
                _ => 0,
            })
            .sum::<usize>();
        // 混合中英的粗略估算：按字节（中文 UTF-8 3 字节 ≈ 0.75 token/字 →
        // 0.25 token/字节；ASCII ≈ 0.25 token/字节），两者巧合接近，统一
        // bytes/4 即可，误差可接受（仅用于百分比提示）。
        (text_len as u64) / 4
    }

    /// 定时任务驱动线程：每秒检查到期任务并发事件（独立于 UI 帧循环——
    /// 依赖 UI 帧调用 due() 的话，闲置时永不触发）。引擎构造时启动。
    fn spawn_scheduler_thread(
        scheduler: Arc<std::sync::Mutex<crate::engine::schedule::Scheduler>>,
        tx: Sender<EngineEvent>,
    ) {
        let _ = std::thread::Builder::new()
            .name("dsh-scheduler".into())
            .spawn(move || loop {
                std::thread::sleep(std::time::Duration::from_secs(1));
                // panic 恢复：锁毒化/意外 panic 不让调度线程死亡
                // （历史缺陷：线程一死所有定时任务静默停摆）
                let due = {
                    let mut s = scheduler
                        .lock()
                        .unwrap_or_else(|p| p.into_inner());
                    s.due()
                };
                for id in due {
                    let name = {
                        let s = scheduler.lock().unwrap_or_else(|p| p.into_inner());
                        s.list()
                            .into_iter()
                            .find(|t| t.id == id)
                            .map(|t| t.name.clone())
                            .unwrap_or_default()
                    };
                    log::info!("scheduled task due: {id} ({name})");
                    let _ = tx.send(EngineEvent::ScheduledTaskDue { id, name });
                }
            });
    }

    pub fn settings(&self) -> &EngineSettings {
        &self.settings
    }

    /// 查询当前沙箱模式。
    pub fn sandbox_mode(&self) -> crate::exec::SandboxMode {
        self.sandbox_mode
    }

    /// 切换沙箱模式并重建工具注册表（bash/pwsh 随新模式受限/直通）。
    /// danger-full-access → 直通；workspace-write / read-only → restricted-token。
    pub fn set_sandbox_mode(&mut self, mode: crate::exec::SandboxMode) {
    // 切换工作区/沙箱后旧的 AlwaysAllow 全部失效（历史缺陷：旧工作区
    // 放行过的区外路径在新工作区语境下变成无审批直通）
    self.approvals
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .clear_allowances();

        self.sandbox_mode = mode;
        // 持久化（重启恢复用户选择的沙箱模式）
        self.settings.sandbox_mode = Some(mode.as_str().to_string());
        let _ = self.settings.save();
        let ws = self
            .workspace_root
            .clone()
            .or_else(|| Some(self.tools.cwd.clone()))
            .unwrap_or_default();
        self.tools = self.tools.with_sandbox(
            if matches!(mode, crate::exec::SandboxMode::DangerFullAccess) {
                None
            } else {
                Some(crate::exec::acl::WindowsAclSandbox::new(mode, ws))
            },
        );
        info!("sandbox mode set to {}", mode.as_str());
    }

    /// 共享会话 map 访问（UI 判断 running / 排队泵用；短锁快取）。
    pub fn shared_sessions(&self) -> std::sync::MutexGuard<'_, HashMap<String, Session>> {
        lock_shared(&self.sessions_shared)
    }

    /// 测试用：工具注册表快照（沙箱模式切换验证）。
    pub fn tools_for_test(&self) -> ToolRegistry {
        self.tools.clone_handle()
    }

    pub fn llm(&self) -> Option<&Arc<LlmClient>> {
        self.llm.as_ref()
    }

    /// 设置页保存入口：更新全部引擎热字段并立即生效（LlmClient 重建 +
    /// 内存/磁盘同步）。旧实现设置页直接写文件、引擎内存还留着旧值——
    /// 之后任何 chip 切换触发 rebuild_llm 落盘就把刚保存的 API key 覆盖掉
    /// （"保存不了 key"的根因）。统一走引擎，内存为唯一权威。
    pub fn update_settings_from_ui(
        &mut self,
        api_key: Option<String>,
        base_url: String,
        model: String,
        http_proxy: Option<String>,
        default_preset: String,
    ) {
        self.settings.api_key = api_key;
        self.settings.base_url = base_url;
        self.settings.model = model;
        self.settings.http_proxy = http_proxy;
        self.settings.default_preset = default_preset;
        // reasoning_effort 保留当前值（输入框 chip 是它的编辑入口）
        self.rebuild_llm();
    }

    /// 设置会话级沙箱模式（仅影响该会话；持久化 session/sandbox 事件）。
    pub fn set_session_sandbox(&mut self, session_id: &str, mode: crate::exec::SandboxMode) {
        let ev = SessionEvent::new(
            types::SESSION_SANDBOX,
            Some(json!({"sandbox": mode.as_str()})),
        );
        if let Some(s) = self.sessions.get_mut(session_id) {
            s.sandbox_mode = Some(mode.as_str().to_string());
            s.push_event(ev.clone());
        }
        {
            let mut shared = lock_shared(&self.sessions_shared);
            if let Some(s) = shared.get_mut(session_id) {
                s.sandbox_mode = Some(mode.as_str().to_string());
            }
        }
        if let Err(e) = self.store.append(session_id, &ev) { log::warn!("persist failed: {e:#}"); }
        let _ = self.tx.send(EngineEvent::Event {
            session_id: session_id.to_string(),
            event: ev,
        });
        info!("session {session_id} sandbox -> {}", mode.as_str());
    }

    /// 获取会话生效的沙箱模式（会话覆盖 > 引擎全局）。
    pub fn effective_sandbox(&self, session_id: &str) -> crate::exec::SandboxMode {
        self.sessions
            .get(session_id)
            .and_then(|s| s.sandbox_mode.as_deref())
            .and_then(crate::exec::SandboxMode::parse)
            .unwrap_or(self.sandbox_mode)
    }

    /// 设置会话级模型覆盖（持久化）。
    pub fn set_session_model(&mut self, session_id: &str, model: &str) {
        let ev = SessionEvent::new(types::SESSION_MODEL, Some(json!({"model": model})));
        if let Some(s) = self.sessions.get_mut(session_id) {
            s.model = Some(model.to_string());
            s.push_event(ev.clone());
        }
        {
            let mut shared = lock_shared(&self.sessions_shared);
            if let Some(s) = shared.get_mut(session_id) {
                s.model = Some(model.to_string());
            }
        }
        if let Err(e) = self.store.append(session_id, &ev) { log::warn!("persist failed: {e:#}"); }
        info!("session {session_id} model -> {model}");
    }

    /// 获取会话生效的模型（会话覆盖 > 引擎全局）。
    pub fn effective_model(&self, session_id: &str) -> String {
        self.sessions
            .get(session_id)
            .and_then(|s| s.model.clone())
            .or_else(|| self.llm.as_ref().map(|l| l.model().to_string()))
            .unwrap_or_else(|| "deepseek-v4-flash".into())
    }

    /// 设置会话级思考深度覆盖（持久化）。
    pub fn set_session_effort(&mut self, session_id: &str, effort: &str) {
        let ev = SessionEvent::new(types::SESSION_EFFORT, Some(json!({"effort": effort})));
        if let Some(s) = self.sessions.get_mut(session_id) {
            s.effort = Some(effort.to_string());
            s.push_event(ev.clone());
        }
        {
            let mut shared = lock_shared(&self.sessions_shared);
            if let Some(s) = shared.get_mut(session_id) {
                s.effort = Some(effort.to_string());
            }
        }
        if let Err(e) = self.store.append(session_id, &ev) { log::warn!("persist failed: {e:#}"); }
        info!("session {session_id} effort -> {effort}");
    }

    /// 获取会话生效的思考深度（会话覆盖 > 引擎全局）。
    pub fn effective_effort(&self, session_id: &str) -> String {
        self.sessions
            .get(session_id)
            .and_then(|s| s.effort.clone())
            .or_else(|| self.settings.reasoning_effort.clone())
            .unwrap_or_else(|| "high".into())
    }

    /// 运行时切换模型（下一回合生效；进行中的回合沿用旧客户端快照）。
    pub fn set_model(&mut self, model: &str) {
        self.settings.model = model.to_string();
        self.rebuild_llm();
        info!("model switched to {model}");
    }

    /// 运行时切换思考深度（none/low/high/max；none = 关闭思考）。
    pub fn set_reasoning_effort(&mut self, effort: &str) {
        let effort = if REASONING_EFFORTS.contains(&effort) {
            effort.to_string()
        } else {
            "high".to_string()
        };
        self.settings.reasoning_effort = Some(effort.clone());
        self.rebuild_llm();
        info!("reasoning effort switched to {effort}");
    }

    /// 当前模型（无 LLM 客户端时回退设置值）。
    pub fn current_model(&self) -> String {
        self.llm
            .as_ref()
            .map(|l| l.model().to_string())
            .unwrap_or_else(|| self.settings.model.clone())
    }

    /// 当前思考深度（None = API 缺省，展示为 high）。
    pub fn current_reasoning_effort(&self) -> String {
        self.settings
            .reasoning_effort
            .clone()
            .unwrap_or_else(|| "high".into())
    }

    /// 用当前设置重建 LLM 客户端（模型/思考深度切换用，并持久化设置）。
    fn rebuild_llm(&mut self) {
        if let Some(key) = self.settings.api_key.clone() {
            if let Ok(c) = LlmClient::new(
                self.settings.base_url.clone(),
                key,
                self.settings.model.clone(),
                self.settings.reasoning_effort.clone(),
                self.settings.http_proxy.clone(),
            ) {
                self.llm = Some(Arc::new(c));
            }
        }
        if let Err(e) = self.settings.save() {
            log::warn!("settings save after switch failed: {e:#}");
        }
    }

    /// 列出会话摘要。
    pub fn list_sessions(&self) -> Vec<crate::core::session::SessionSummary> {
        let ids = self.store.list_sessions();
        let mut out = Vec::new();
        for id in ids {
            if let Ok(s) = self.store.load_session(&id) {
                out.push(s.summary());
            }
        }
        out.sort_by(|a, b| {
            // 最后活动优先；时间相同（同秒创建/活动）按会话 id 稳定排序，
            // 避免列表顺序抖动
            b.updated_at
                .partial_cmp(&a.updated_at)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.session_id.cmp(&b.session_id))
        });
        out
    }

    /// 创建会话（绑定引擎当前工作区，默认标准模式）。
    pub fn create_session(&mut self, title: Option<&str>) -> Result<String> {
        self.create_session_with_cwd(title, None)
    }

    /// 创建会话并绑定工作区目录（cwd 为 None 时用引擎当前工作区；使用引擎默认预设）。
    pub fn create_session_with_cwd(
        &mut self,
        title: Option<&str>,
        cwd: Option<PathBuf>,
    ) -> Result<String> {
        self.create_session_full(title, cwd, self.default_preset)
    }

    /// 创建会话：工作区 + Agent 预设（对齐 session.create 的 workspaceId/cwd/agentPresets）。
    pub fn create_session_full(
        &mut self,
        title: Option<&str>,
        cwd: Option<PathBuf>,
        preset: AgentPreset,
    ) -> Result<String> {
        let cwd = cwd.or_else(|| self.workspace_root.clone());
        let id = format!("s-{}", uuid());
        // 新会话 id 唯一,但防御性清墓碑(未来 id 复用场景)
        self.store.clear_tombstone(&id);
        let mut session = Session::new(id.clone());
        session.cwd = cwd;
        // 创建时的 cwd 持久化（session/cwd 事件，重启重放恢复独立工作区）
        if let Some(ref c) = session.cwd {
            let cev = SessionEvent::new(
                types::SESSION_CWD,
                Some(json!({"cwd": c.to_string_lossy().into_owned()})),
            );
            session.push_event(cev.clone());
            if let Err(e) = self.store.append(&id, &cev) { log::warn!("persist failed: {e:#}"); }
        }
        session.preset = preset;
        // preset 持久化事件（重放恢复模式）
        let ev = SessionEvent::new(types::SESSION_PRESET, Some(json!({"preset": preset.id()})));
        session.push_event(ev.clone());
        self.store.append(&id, &ev)?;
        if let Some(t) = title {
            session.title = t.to_string();
            let ev = SessionEvent::new(types::SESSION_TITLE, Some(json!({"title": t})));
            session.push_event(ev.clone());
            // 持久化，确保 list_sessions 能重放出来
            self.store.append(&id, &ev)?;
        }
        self.sessions.insert(id.clone(), session.clone());
        self.sessions_shared
            .lock()
            .unwrap()
            .insert(id.clone(), session);
        info!("created session {id}");
        let _ = self.tx.send(EngineEvent::SessionCreated {
            session_id: id.clone(),
        });
        Ok(id)
    }

    /// 打开会话（优先读共享内存，其次存储重放）。
    /// 打开会话（优先读共享内存——agent 线程写入最新状态，其次引擎缓存，最后存储重放）。
    /// 从事件重放 feedback（重启后 👍/👎 状态恢复；历史缺陷：事件落盘
    /// 但 registry 纯内存，重启全丢）。
    fn replay_feedback(&self, session_id: &str) {
        for ev in self.store.load_events(session_id) {
            if ev.r#type != types::FEEDBACK_RECORD {
                continue;
            }
            let Some(d) = &ev.data else { continue };
            let (Some(mid), Some(kind)) = (
                d.get("messageId").and_then(|v| v.as_str()),
                d.get("kind").and_then(|v| v.as_str()),
            ) else {
                continue;
            };
            let k = match kind {
                "upvote" => crate::engine::FeedbackKind::Upvote,
                _ => crate::engine::FeedbackKind::Downvote,
            };
            self.feedback
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .record(mid, k);
        }
    }

    pub fn open_session(&mut self, session_id: &str) -> Result<Session> {
        // 1) 共享内存（agent 线程结束时会写入完整消息 + running=false，最新）
        //    但 shared 里可能是 create 时的空版本（仅 preset/title 事件、无消息）
        //    —— 跳过，走重放，确保外部/后台写入存储的新消息能被读到
        if let Some(s) = lock_shared(&self.sessions_shared).get(session_id).cloned() {
            if !s.messages.is_empty() || s.running {
                // 共享内存的 events/messages 可能是旧快照（回合中只同步 messages，
                // 不回写 events；而 store 由 agent 线程持续追加）——用 store 的
                // 最新内容补齐，否则打开会话重放会缺 PLAN_MODE /
                // SUBAGENT_DESCRIPTOR / 新消息（历史回归："界面端一直不更新"、
                // "发送的消息偶尔会消失"：shared 快照旧 → 切回会话看不到新消息）。
                let mut s = s;
                if let Ok(loaded) = self.store.load_session(session_id) {
                    if loaded.events.len() > s.events.len() {
                        s.events = loaded.events;
                        s.messages = loaded.messages;
                    }
                }
                self.sessions.insert(session_id.to_string(), s.clone());
                return Ok(s);
            }
        }
        // 2) 引擎缓存（仅当已有实际消息时命中：create 后的空缓存让位给
        //    存储重放，否则会话创建后外部/后台写入存储的新消息永远读不到）
        if let Some(s) = self.sessions.get(session_id) {
            if !s.messages.is_empty() {
                let mut s = s.clone();
                // 同上：events/messages 用 store 补齐（回合中追加的
                // plan/subagent 事件与新消息）
                if let Ok(loaded) = self.store.load_session(session_id) {
                    if loaded.events.len() > s.events.len() {
                        s.events = loaded.events;
                        s.messages = loaded.messages;
                    }
                }
                return Ok(s);
            }
        }
        // 3) 存储重放（feedback registry 冷启动恢复一次）
        if self.feedback.lock().unwrap_or_else(|p| p.into_inner()).count() == 0 {
            self.replay_feedback(session_id);
        }
        let s = self.store.load_session(session_id)?;
        self.sessions.insert(session_id.to_string(), s.clone());
        self.sessions_shared
            .lock()
            .unwrap()
            .insert(session_id.to_string(), s.clone());
        Ok(s)
    }

    /// 打开工作区：规范化目录并设为引擎**全局默认** cwd
    /// （工具根目录 / 新会话 / 终端生效）。
    /// 注意：不覆盖已打开会话的 cwd——每个会话有自己独立的工作区
    /// （见 set_session_workspace）。
    pub fn set_workspace(&mut self, path: &Path) -> Result<String, String> {
    // 切换工作区/沙箱后旧的 AlwaysAllow 全部失效（历史缺陷：旧工作区
    // 放行过的区外路径在新工作区语境下变成无审批直通）
    self.approvals
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .clear_allowances();

        let ws = self.workspaces.open(path)?;
        self.tools.cwd = ws.root.clone();
        self.workspace_root = Some(ws.root.clone());
        info!("workspace opened (global default): {}", ws.root.display());
        Ok(ws.root.to_string_lossy().into_owned())
    }

    /// 设置**指定会话**的工作区（该会话独立工作区：AI 回合的 persona / 工具
    /// 用会话 cwd；session/cwd 事件持久化，重启重放恢复）。
    /// 同时更新全局默认，新会话继承。
    /// 这是 UI 工作区栏的操作入口——"我改了工作目录，AI 却说还是 E:\AI"
    /// 的根因就是只改了全局默认、没改当前会话的 cwd。
    pub fn set_session_workspace(
        &mut self,
        session_id: &str,
        path: &Path,
    ) -> Result<String, String> {
        let ws = self.workspaces.open(path)?;
        self.tools.cwd = ws.root.clone();
        self.workspace_root = Some(ws.root.clone());
        let ev = SessionEvent::new(
            types::SESSION_CWD,
            Some(json!({"cwd": ws.root.to_string_lossy().into_owned()})),
        );
        if let Some(s) = self.sessions.get_mut(session_id) {
            s.cwd = Some(ws.root.clone());
            s.push_event(ev.clone());
        }
        {
            let mut shared = lock_shared(&self.sessions_shared);
            if let Some(s) = shared.get_mut(session_id) {
                s.cwd = Some(ws.root.clone());
                s.push_event(ev.clone());
            }
        }
        if let Err(e) = self.store.append(session_id, &ev) {
            log::warn!("session workspace persist failed: {e:#}");
        }
        let _ = self.tx.send(EngineEvent::Event {
            session_id: session_id.to_string(),
            event: ev,
        });
        info!("session {session_id} workspace -> {}", ws.root.display());
        Ok(ws.root.to_string_lossy().into_owned())
    }

    /// 任务管理器句柄（UI 读取任务列表 / 移除任务）。
    pub fn jobs_handle(&self) -> Arc<std::sync::Mutex<JobManager>> {
        self.jobs.clone()
    }

    /// 子代理管理器句柄（UI 读取子代理列表）。
    pub fn subagents_handle(&self) -> Arc<std::sync::Mutex<SubagentManager>> {
        self.subagents.clone()
    }

    /// 定时任务调度器句柄（UI/桥注册定时任务；到期由后台线程驱动）。
    pub fn scheduler_handle(&self) -> Arc<std::sync::Mutex<crate::engine::schedule::Scheduler>> {
        self.scheduler.clone()
    }

    /// 任务列表快照（新→旧）。
    pub fn jobs_snapshot(&self) -> Vec<crate::engine::jobs::Job> {
        self.jobs
            .lock()
            .unwrap()
            .list()
            .into_iter()
            .cloned()
            .collect()
    }

    /// 子代理列表快照。
    pub fn subagents_snapshot(&self) -> Vec<crate::engine::subagent::SubagentDescriptor> {
        self.subagents
            .lock()
            .unwrap()
            .list()
            .into_iter()
            .cloned()
            .collect()
    }

    /// 移除一个任务（UI）。
    pub fn job_remove(&self, id: &str) {
        self.jobs.lock().unwrap().remove(id);
    }

    /// 清空已完成任务（UI）。
    pub fn job_clear_finished(&self) {
        self.jobs.lock().unwrap().clear_finished();
    }

    /// 技能列表快照。
    pub fn skills_snapshot(&self) -> Vec<crate::engine::skill::Skill> {
        self.skills.list().into_iter().cloned().collect()
    }

    /// 重新从技能目录加载（安装/创建技能后调用，agent 下一回合即可用）。
    pub fn reload_skills(&mut self) {
        self.skills.load_from_dir(&self.skills_dir);
        info!(
            "skills reloaded: {} from {}",
            self.skills.len(),
            self.skills_dir.display()
        );
    }

    /// 技能内容（instructions，供 load_skill 工具 / UI 预览）。
    pub fn load_skill(&self, name: &str) -> Option<String> {
        self.skills.get(name).and_then(|s| s.instructions.clone())
    }

    /// 用户技能目录。
    pub fn skills_dir(&self) -> PathBuf {
        self.skills_dir.clone()
    }

    /// 创建技能（写 $DSH_HOME/skills/<name>/skill.md）并注册。
    pub fn create_skill(
        &mut self,
        name: &str,
        description: &str,
        instructions: &str,
    ) -> Result<PathBuf, String> {
        let dir = self
            .skills
            .create_skill(&self.skills_dir, name, description, instructions)?;
        // 重新加载（注册进 registry）
        self.skills.load_from_dir(&self.skills_dir);
        info!("skill created: {name} at {}", dir.display());
        Ok(dir)
    }

    /// 移除技能（删除技能目录 + 重载注册表）。
    pub fn remove_skill(&mut self, name: &str) -> Result<(), String> {
        let dir = self.skills_dir.join(name);
        if !dir.is_dir() {
            return Err(format!("技能不存在: {name}"));
        }
        std::fs::remove_dir_all(&dir).map_err(|e| format!("{e:#}"))?;
        self.skills = crate::engine::skill::SkillRegistry::default();
        self.skills.load_from_dir(&self.skills_dir);
        info!("skill removed: {name}");
        Ok(())
    }

    /// 从本地文件导入技能（skill.md / SKILL.md）。
    pub fn import_skill_file(&mut self, src: &std::path::Path) -> Result<String, String> {
        let name = self.skills.import_skill_file(&self.skills_dir, src)?;
        self.skills = crate::engine::skill::SkillRegistry::default();
        self.skills.load_from_dir(&self.skills_dir);
        info!("skill imported from file: {name}");
        Ok(name)
    }

    /// 从本地目录导入技能（目录内须含 skill.md / SKILL.md）。
    pub fn import_skill_dir(&mut self, src: &std::path::Path) -> Result<String, String> {
        let name = self.skills.import_skill_dir(&self.skills_dir, src)?;
        self.skills = crate::engine::skill::SkillRegistry::default();
        self.skills.load_from_dir(&self.skills_dir);
        info!("skill imported from dir: {name}");
        Ok(name)
    }

    // ===== cordis 风格子进程插件 =====

    /// 插件快照（UI）：(name, status, tools, description)。
    pub fn plugin_snapshot(
        &self,
    ) -> Vec<(
        String,
        crate::engine::plugin::PluginStatus,
        Vec<String>,
        String,
    )> {
        self.plugins.lock().unwrap().snapshot()
    }

    /// 插件目录。
    pub fn plugin_dir(&self) -> std::path::PathBuf {
        self.plugins.lock().unwrap().ensure_dir()
    }

    /// 启动全部插件（发现 + start + initialize 握手）。
    pub fn plugin_start_all(&self) {
        self.plugins.lock().unwrap().start_all();
    }

    /// 停止指定插件。
    pub fn plugin_stop(&self, name: &str) {
        if let Some(p) = self.plugins.lock().unwrap().get_mut(name) {
            p.stop();
        }
    }

    /// 启动指定插件（spawn + initialize 握手）。
    pub fn plugin_start(&self, name: &str) {
        let mut mgr = self.plugins.lock().unwrap();
        if let Some(p) = mgr.get_mut(name) {
            if let Err(e) = p.start() {
                p.status = crate::engine::plugin::PluginStatus::Failed(e);
            }
        }
    }

    /// 热重载指定插件：重新读 manifest → 停止旧进程 → 启动（插件文件修改后生效）。
    pub fn plugin_reload(&self, name: &str) {
        self.plugins.lock().unwrap().reload(name);
    }

    /// 重新发现插件（新增插件目录后调用）。
    pub fn plugin_refresh(&self) {
        self.plugins.lock().unwrap().discover();
    }

    /// 删除本地子进程插件：停止进程 → 删除 $DSH_HOME/plugins/<name> 目录
    /// → 重新发现。名称按 kebab-case 白名单校验（只允许小写字母/数字/连
    /// 字符——天然排除路径穿越）；canonical 前缀校验确保删除目标就在
    /// 插件根之下。
    pub fn remove_plugin(&self, name: &str) -> Result<(), String> {
        if !crate::engine::skill::is_skill_name(name) {
            return Err(format!("插件名非法: {name}"));
        }
        let mut mgr = self.plugins.lock().unwrap();
        if let Some(p) = mgr.get_mut(name) {
            p.stop();
        }
        let root = mgr.ensure_dir();
        let dir = root.join(name);
        if !dir.is_dir() {
            return Err(format!("插件目录不存在: {}", dir.display()));
        }
        let dir_c = dir.canonicalize().map_err(|e| format!("{e}"))?;
        let root_c = root.canonicalize().map_err(|e| format!("{e}"))?;
        if !dir_c.starts_with(&root_c) {
            return Err(format!("拒绝删除插件根之外的路径: {}", dir.display()));
        }
        mgr.remove(name);
        std::fs::remove_dir_all(&dir).map_err(|e| format!("删除失败: {e}"))?;
        mgr.discover();
        info!("plugin removed: {name}");
        Ok(())
    }

    /// 每帧泵：处理插件输出 / 响应 / 退出检测（app 每帧调用）。
    pub fn plugin_pump(&self) {
        self.plugins.lock().unwrap().pump();
    }

    /// 是否有插件握手/工具响应等待中（UI 据此安排定时重绘）。
    pub fn plugin_has_live_work(&self) -> bool {
        self.plugins.lock().unwrap().has_live_work()
    }

    /// 插件提供的主题文件（ThemeManager 合并进主题名单）。
    pub fn plugin_theme_files(&self) -> Vec<std::path::PathBuf> {
        self.plugins.lock().unwrap().theme_files()
    }

    /// 插件领域声明（领域增强插件的归组展示）。
    pub fn plugin_domains(&self) -> Vec<(String, String)> {
        self.plugins.lock().unwrap().domains()
    }

    /// 从本地目录导入插件（目录须含 plugin.json）：
    /// 复制到插件目录 → 发现 → 自动启动（autostart 语义）。
    pub fn import_plugin_dir(&self, src: &std::path::Path) -> Result<String, String> {
        let manifest_path = src.join("plugin.json");
        if !manifest_path.is_file() {
            return Err(format!("目录内未找到 plugin.json: {}", src.display()));
        }
        let text = std::fs::read_to_string(&manifest_path).map_err(|e| format!("{e:#}"))?;
        let manifest: crate::engine::plugin::PluginManifest =
            serde_json::from_str(&text).map_err(|e| format!("plugin.json 解析失败: {e}"))?;
        let name = manifest.name.clone();
        if !crate::engine::skill::is_skill_name(&name) {
            return Err(format!("插件名非法（kebab-case）: {name}"));
        }
        let mut mgr = self.plugins.lock().unwrap();
        let dest = mgr.ensure_dir().join(&name);
        if dest.exists() {
            return Err(format!("插件已存在: {name}"));
        }
        copy_dir_all_plugin(src, &dest).map_err(|e| format!("复制失败: {e}"))?;
        mgr.discover();
        if let Some(p) = mgr.get_mut(&name) {
            if let Err(e) = p.start() {
                p.status = crate::engine::plugin::PluginStatus::Failed(e);
            }
        }
        info!("plugin imported from dir: {name} -> {}", dest.display());
        Ok(name)
    }

    /// 插件提供的全部工具（agent 回合合并）。
    pub fn plugin_tools(&self) -> Vec<crate::engine::plugin::protocol::PluginToolSpec> {
        self.plugins.lock().unwrap().all_tools()
    }

    /// 插件工具是否可用（按名称）。
    pub fn plugin_has_tool(&self, name: &str) -> bool {
        self.plugins
            .lock()
            .unwrap()
            .all_tools()
            .iter()
            .any(|t| t.name == name)
    }

    /// 当前工作区根目录。
    pub fn workspace_root(&self) -> Option<&PathBuf> {
        self.workspace_root.as_ref()
    }

    /// 新建会话默认预设（默认标准模式）。
    pub fn default_preset(&self) -> AgentPreset {
        self.default_preset
    }

    /// 修改新建会话默认预设。
    pub fn set_default_preset(&mut self, preset: AgentPreset) {
        self.default_preset = preset;
        info!("default preset -> {}", preset.id());
    }

    /// 切换会话的 Agent 预设（下次发送生效；追加 session/preset 事件）。
    pub fn set_session_preset(&mut self, session_id: &str, preset: AgentPreset) -> Result<()> {
        let ev = SessionEvent::new(types::SESSION_PRESET, Some(json!({"preset": preset.id()})));
        if !self.sessions.contains_key(session_id)
            && !lock_shared(&self.sessions_shared).contains_key(session_id)
        {
            return Err(anyhow::anyhow!("session not found: {session_id}"));
        }
        if let Some(s) = self.sessions.get_mut(session_id) {
            s.preset = preset;
            s.push_event(ev.clone());
        }
        {
            let mut shared = lock_shared(&self.sessions_shared);
            if let Some(s) = shared.get_mut(session_id) {
                s.preset = preset;
            }
        }
        self.store.append(session_id, &ev)?;
        let _ = self.tx.send(EngineEvent::Event {
            session_id: session_id.to_string(),
            event: ev,
        });
        info!("session {session_id} preset -> {}", preset.id());
        Ok(())
    }

    /// 排队一条消息（不打断当前回合；回合结束后由 pump_message_queue 逐条发出）。
    /// 返回排队位置（1 = 下一条）。
    pub fn enqueue_message(
        &mut self,
        session_id: &str,
        content: &str,
        images: Vec<String>,
    ) -> usize {
        let q = self
            .queued_messages
            .entry(session_id.to_string())
            .or_default();
        q.push_back((content.to_string(), images));
        let pos = q.len();
        info!("message queued for {session_id} (position {pos})");
        pos
    }

    /// 当前会话的排队消息数。
    pub fn queued_count(&self, session_id: &str) -> usize {
        self.queued_messages
            .get(session_id)
            .map(|q| q.len())
            .unwrap_or(0)
    }

    /// 清空某会话的排队消息（删除会话时清理）。
    pub fn clear_queue(&mut self, session_id: &str) {
        self.queued_messages.remove(session_id);
    }

    /// 驱动队列（app 每帧调用）：对每个有排队消息且当前空闲的会话，
    /// 取队首自动发起回合（FIFO 逐条执行）。发送失败的消息丢弃并记日志
    /// （保留会卡死队列；失败原因已通过 Error 事件可见）。
    pub fn pump_message_queue(&mut self) {
        // 重试计数持久化在字段(历史缺陷:局部 map 每次调用重建 → 计数
        // 恒 1,上限无效,持久失败时 pop→fail→push_front 帧循环无限抖动)
        self.queue_retries.retain(|_, c| *c > 0);
        let ready: Vec<(String, (String, Vec<String>))> = {
            let shared = lock_shared(&self.sessions_shared);
            self.queued_messages
                .iter()
                .filter_map(|(sid, q)| {
                    let idle = shared.get(sid).map(|s| !s.running).unwrap_or(true);
                    let next = q.front().cloned()?;
                    idle.then(|| (sid.clone(), next))
                })
                .collect()
        };
        for (sid, (msg, images)) in ready {
            if let Some(q) = self.queued_messages.get_mut(&sid) {
                q.pop_front();
                if q.is_empty() {
                    self.queued_messages.remove(&sid);
                }
            }
            if let Err(e) = self.send_message_with_images(&sid, &msg, images.clone()) {
                // 失败重入队首（历史缺陷：弹出后失败即静默丢弃消息文本）。
                // 重试计数挂在消息上（第 3 元素），超过 2 次才真正丢弃。
                warn!("queued message send failed, re-queueing ({sid}): {e:#}");
                let attempts = self
                    .queue_retries
                    .entry((sid.clone(), msg.clone()))
                    .and_modify(|c| *c += 1)
                    .or_insert(1);
                if *attempts <= 2 {
                    self.queued_messages
                        .entry(sid.clone())
                        .or_default()
                        .push_front((msg, images));
                } else {
                    warn!("queued message dropped after retries ({sid})");
                }
            } else {
                // send 成功 → 清计数（历史缺陷:remove 在 send 之前,失败
                // 后 or_insert(1) 重建 → 计数恒 1,上限永不可达）
                self.queue_retries.remove(&(sid, msg));
            }
        }
    }

    /// 用户发送消息 → 异步 agent 回合。
    pub fn send_message(&mut self, session_id: &str, content: &str) -> Result<()> {
        self.send_message_with_images(session_id, content, Vec::new())
    }

    /// 发送消息（可附图片路径：vision 模型多模态；非 vision 模型由
    /// to_llm_messages 侧按 data URI 编码，API 不支持时服务端忽略）。
    pub fn send_message_with_images(
        &mut self,
        session_id: &str,
        content: &str,
        images: Vec<String>,
    ) -> Result<()> {
        log::debug!(
            "send_message called for {session_id} ({} chars)",
            content.chars().count()
        );
        // 0) 先校验 API key：无 key 时报错且不改任何会话状态
        // API key 存在性校验（无 key 不进入回合；实际客户端在下方按会话级
        // 模型/effort 覆盖构建，历史缺陷：全局 llm 直接透传，会话级设置无效）
        if self.llm.is_none() {
            anyhow::bail!("API key 未配置：请在设置页填写 DeepSeek API key 后重试");
        }
        // 1) 从共享内存同步最新状态（上一回合结束后的 running=false + 完整消息）
        if let Err(e) = self.open_session(session_id) {
            log::warn!("send_message: open_session({session_id}) 失败: {e:#}");
            return Err(e);
        }
        // 2) 并发保护：检查 running（running 中发送 = 插话；回合线程由每会话
        //    回合锁串行，新回合会等旧回合线程完全退出后再启动）
        let is_running = {
            let shared = lock_shared(&self.sessions_shared);
            shared.get(session_id).map(|s| s.running).unwrap_or(false)
        };
        if is_running {
            // 回合运行中 → 插话。二次复查消除孤儿窗口（历史缺陷:读
            // running=true 后 TurnGuard 恰好收尾置 false → cancelled 置
            // 在已死信号上,消息落盘但永远无人回答）:
            // 若此刻已 false,走下方新回合路径重新发起。
            let still_running = lock_shared(&self.sessions_shared)
                .get(session_id)
                .map(|s| s.running)
                .unwrap_or(false);
            if still_running {
                log::info!(
                    "send_message: session {session_id} running -> interject"
                );
                return self.interject(session_id, content, images.clone());
            }
            log::info!(
                "send_message: session {session_id} race resolved -> new turn"
            );
        }
        // 3) 新回合信号（整体替换：旧回合持有旧 Arc，新取消/停止不影响旧回合收尾）
        let signal = Arc::new(TurnSignal::default());
        self.turn_signals
            .insert(session_id.to_string(), signal.clone());
        // 先 clone 需要的快照，避免与持久化 borrow 冲突
        let (epoch, snapshot, session_cwd, session_preset, sess_sb, sess_model, sess_effort) = {
            let session = self
                .sessions
                .get_mut(session_id)
                .ok_or_else(|| anyhow::anyhow!("session not open: {session_id}"))?;
            // running 判定只信 shared（唯一由回合线程 TurnGuard 收尾的地方）。
            // 历史缺陷：此处读本地缓存副本，open_session 克隆与回合一瞬收尾的
            // 窗口里副本陈旧为 true → 已结束回合被误判插话，消息永不回答。
            {
                let shared_running = lock_shared(&self.sessions_shared)
                    .get(session_id)
                    .map(|s| s.running)
                    .unwrap_or(false);
                if shared_running {
                    log::info!(
                        "send_message: session {session_id} running (shared) -> interject"
                    );
                    return self.interject(session_id, content, images.clone());
                }
            }
            session.running = true;
            session.turn_epoch += 1;

            // user/message 事件（图片路径随消息持久化：JSONL 只存路径）
            let mut payload = json!({"content": content});
            if !images.is_empty() {
                payload["images"] = json!(images);
            }
            let ev = SessionEvent::new(types::USER_MESSAGE, Some(payload));
            session.push_event(ev.clone());
            session.messages.push(Message::User {
                content: content.to_string(),
                images: if images.is_empty() { None } else { Some(images.clone()) },
            });
            // 首条消息自动命名（标题为空时）：提炼首行 24 字符并持久化，
            // 会话列表立即显示有意义的名字（此前只靠 summary 回退，重启
            // 前列表内存态标题不更新）。用户手动重命名后 title 非空，不再覆盖。
            if session.title.trim().is_empty() {
                let auto = crate::core::session::title_from_message(content);
                if !auto.is_empty() {
                    let tev = SessionEvent::new(
                        types::SESSION_TITLE,
                        Some(json!({"title": auto, "auto": true})),
                    );
                    session.title = auto.clone();
                    session.push_event(tev.clone());
                    if let Err(e) = self.store.append(session_id, &tev) { log::warn!("persist failed: {e:#}"); }
                    let _ = self.tx.send(EngineEvent::Event {
                        session_id: session_id.to_string(),
                        event: tev,
                    });
                }
            }
            // 同步到共享 map（agent 线程与桥都读它）
            {
                let mut shared = lock_shared(&self.sessions_shared);
                if let Some(s) = shared.get_mut(session_id) {
                    s.running = true;
                    s.turn_epoch = session.turn_epoch;
                    s.messages = session.messages.clone();
                    s.title = session.title.clone();
                }
            }
            let _ = self.tx.send(EngineEvent::Event {
                session_id: session_id.to_string(),
                event: ev,
            });
            let session_cwd = session.cwd.clone();
            let session_preset = session.preset;
            // 会话级设置快照（沙箱/模型/思考深度：会话覆盖 > 引擎全局）
            let session_sandbox = session.sandbox_mode.clone();
            let session_model = session.model.clone();
            let session_effort = session.effort.clone();
            (
                session.turn_epoch,
                session.messages.clone(),
                session_cwd,
                session_preset,
                session_sandbox,
                session_model,
                session_effort,
            )
        };
        // 失败必须回滚 running（否则会话永久卡 Running，后续全部变成插话）。
        // 落盘事件与上方内存事件同构（含 images——历史 bug：裸事件丢图，
        // 回合结束 TurnGuard 从 store 重载后图片引用消失）。
        let mut persist_payload = json!({"content": content});
        if !images.is_empty() {
            persist_payload["images"] = json!(images);
        }
        if let Err(e) = self.persist(
            session_id,
            &SessionEvent::new(types::USER_MESSAGE, Some(persist_payload)),
        ) {
            log::warn!("send_message: persist 失败: {e:#}");
            self.rollback_turn_start(session_id, epoch);
            let _ = self.tx.send(EngineEvent::Error {
                session_id: session_id.to_string(),
                message: format!("用户消息持久化失败：{e:#}"),
            });
            return Err(e);
        }
        let _ = self.tx.send(EngineEvent::StatusChanged {
            session_id: session_id.to_string(),
            status: AgentStatus::Running,
        });

        // 后台线程跑回合
        let tx = self.tx.clone();
        let session_id = session_id.to_string();
        let store = self.store.clone_handle();
        let global_model = self
            .llm
            .as_ref()
            .map(|l| l.model().to_string())
            .unwrap_or_default();
        // 会话级模型/思考深度生效（历史缺陷：快照后被弃用，UI 切换会话级
        // 设置对实际回合零影响）。覆盖项与全局不同 → 构建会话专用 LLM 客户端。
        let llm = if self.llm.is_some()
            && (sess_model.as_deref().is_some_and(|m| m != global_model)
                || sess_effort.is_some())
        {
            let key = self.settings.api_key.clone().unwrap_or_default();
            LlmClient::new(
                self.settings.base_url.clone(),
                key,
                sess_model.clone().unwrap_or(global_model.clone()),
                sess_effort.clone().or_else(|| self.settings.reasoning_effort.clone()),
                self.settings.http_proxy.clone(),
            )
            .map(Arc::new)
            .unwrap_or_else(|e| {
                log::warn!("session llm build failed, fallback global: {e:#}");
                self.llm.clone().expect("checked above")
            })
        } else {
            self.llm.clone().expect("checked at entry")
        };
        let model = llm.model().to_string();
        let tools = {
            // 快照注入技能（load_skill 工具 + persona 技能列表用）；
            // 会话级沙箱覆盖（会话 read-only/write → 受限工具集，此前死代码）
            let mut t = self.tools.clone_handle();
            t.skills = self.skills.list().into_iter().cloned().collect();
            if let Some(sb) = sess_sb
                .as_deref()
                .and_then(crate::exec::SandboxMode::parse)
            {
                let ws = self.workspace_root.clone().unwrap_or_default();
                t = t.with_sandbox(if matches!(sb, crate::exec::SandboxMode::DangerFullAccess) {
                    None
                } else {
                    Some(crate::exec::acl::WindowsAclSandbox::new(sb, ws))
                });
            }
            t
        };
        let sessions_shared = self.sessions_shared.clone();
        let cwd = session_cwd;
        let preset = session_preset;
        let turn_lock = self
            .turn_locks
            .lock()
            .unwrap()
            .entry(session_id.clone())
            .or_insert_with(|| Arc::new(std::sync::Mutex::new(())))
            .clone();
        // 回合任务：pending → running → done/failed（UI 任务卡片可见进度）
        let title = self
            .sessions
            .get(&session_id)
            .map(|s| s.title.clone())
            .unwrap_or_default();
        let jobs = self.jobs.clone();
        let job_id = {
            let mut jm = jobs.lock().unwrap();
            let id = jm.create("agent turn", Some(format!("会话 {} 的回合", title)));
            jm.start(&id);
            id
        };
        let subagents = self.subagents.clone();
        let plugins = self.plugins.clone();
        let approvals = self.approvals.clone();
        let token_usage = self.token_usage.clone();
        let usage_dirty = self.usage_dirty.clone();
        let lessons_store = self.lessons.clone();
        // spawn 失败处理路径用的克隆（闭包按 move 捕获原值）
        let err_session_id = session_id.clone();
        let err_jobs = jobs.clone();
        let err_job_id = job_id.clone();
        let spawn_res = std::thread::Builder::new()
            .name("agent-turn".into())
            .spawn(move || {
                // 同一会话回合串行：等待旧回合线程完全退出（cancel 后立即重发
                // 不再产生并发回合线程，见 TurnSignal/turn_locks 注释）
                let _turn_guard_lock = turn_lock.lock().unwrap_or_else(|p| p.into_inner());
                // 等锁期间可能已被更新的回合取代 / 会话已删除：直接收尾退出
                let superseded = lock_shared(&sessions_shared)
                    .get(&session_id)
                    .map(|s| s.turn_epoch != epoch)
                    .unwrap_or(true);
                if superseded {
                    jobs.lock().unwrap_or_else(|p| p.into_inner()).finish(
                        &job_id,
                        false,
                        Some("回合已被更新的回合取代".into()),
                    );
                    return;
                }
                let rt = tokio::runtime::Runtime::new();
                let mut guard = TurnGuard {
                    tx: tx.clone(),
                    sessions_shared: sessions_shared.clone(),
                    store: store.clone_handle(),
                    jobs: jobs.clone(),
                    job_id: job_id.clone(),
                    session_id: session_id.clone(),
                    epoch,
                    result_ok: false,
                };
                let result = match rt {
                    Ok(rt) => rt.block_on(run_turn_inner(
                        &tx,
                        &session_id,
                        snapshot,
                        llm,
                        tools,
                        store,
                        &model,
                        &sessions_shared,
                        cwd,
                        preset,
                        &signal,
                        &jobs,
                        &job_id,
                        &subagents,
                        &plugins,
                        &approvals,
                        &token_usage,
                        &usage_dirty,
                        &lessons_store,
                    )),
                    Err(rt_err) => Err(rt_err.into()),
                };
                if let Err(e) = &result {
                    warn!("agent turn failed: {e:#}");
                    let _ = tx.send(EngineEvent::Error {
                        session_id: session_id.clone(),
                        message: format!("{e:#}"),
                    });
                }
                guard.result_ok = result.is_ok();
                drop(guard);
                let _ = result;
            });
        if let Err(e) = spawn_res {
            // 线程启动失败：复位 running（防会话永久卡 Running）+ 任务收尾
            log::error!("send_message: spawn agent-turn 失败: {e:#}");
            self.rollback_turn_start(&err_session_id, epoch);
            let _ = self.tx.send(EngineEvent::Error {
                session_id: err_session_id,
                message: format!("回合线程启动失败：{e:#}"),
            });
            err_jobs
                .lock()
                .unwrap()
                .finish(&err_job_id, false, Some(format!("{e:#}")));
            return Err(anyhow::anyhow!("回合线程启动失败：{e:#}"));
        }
        Ok(())
    }

    /// 回滚一次没能真正启动的回合：复位 running（本地 + 共享）并弹掉
    /// send_message 预先压入的用户消息（内存与磁盘保持一致）。
    fn rollback_turn_start(&mut self, session_id: &str, epoch: u64) {
        if let Some(s) = self.sessions.get_mut(session_id) {
            if s.turn_epoch == epoch {
                s.running = false;
                if matches!(s.messages.last(), Some(Message::User { .. })) {
                    s.messages.pop();
                }
            }
        }
        let mut shared = lock_shared(&self.sessions_shared);
        if let Some(s) = shared.get_mut(session_id) {
            if s.turn_epoch == epoch {
                s.running = false;
                if let Some(local) = self.sessions.get(session_id) {
                    s.messages = local.messages.clone();
                }
            }
        }
    }

    /// 重命名会话：追加 session/title 事件（持久化，重启重放恢复标题），
    /// 同步更新引擎缓存与共享 map。空标题忽略。
    /// 分叉会话：复制当前全部消息（含工具调用配对）到新会话——
    /// 探索性改动的"安全副本"（原会话不动，在新分支继续）。
    /// 标题 = "⑂ 原标题"；预设与工作区继承。
    pub fn fork_session(&mut self, session_id: &str) -> Result<String> {
        let (title, preset, cwd, messages, sess_sb, sess_model, sess_effort) = {
            let local = self.sessions.get(session_id);
            let snapshot = match local {
                Some(s) => (
                    s.title.clone(),
                    s.preset,
                    s.cwd.clone(),
                    s.messages.clone(),
                    s.sandbox_mode.clone(),
                    s.model.clone(),
                    s.effort.clone(),
                ),
                None => {
                    let shared = lock_shared(&self.sessions_shared);
                    let s = shared
                        .get(session_id)
                        .ok_or_else(|| anyhow::anyhow!("session not found: {session_id}"))?;
                    (
                        s.title.clone(),
                        s.preset,
                        s.cwd.clone(),
                        s.messages.clone(),
                        s.sandbox_mode.clone(),
                        s.model.clone(),
                        s.effort.clone(),
                    )
                }
            };
            snapshot
        };
        let new_id = self.create_session_full(None, cwd, preset)?;
        // 复制会话级覆盖并落盘（历史缺陷:只改内存不落盘 → 重启后
        // fork 会话静默回落全局设置）
        if let Some(sb) = sess_sb.as_deref().and_then(crate::exec::SandboxMode::parse) {
            let _ = self.set_session_sandbox(&new_id, sb);
        }
        if let Some(m) = sess_model.as_deref().filter(|m| !m.is_empty()) {
            let _ = self.set_session_model(&new_id, m);
        }
        if let Some(e) = sess_effort.as_deref().filter(|e| !e.is_empty()) {
            let _ = self.set_session_effort(&new_id, e);
        }
        // 消息 → 事件复制（tool/result 需先有 tool/call 声明，重放才能配对）
        let mut calls: HashMap<String, (String, String)> = HashMap::new();
        for m in &messages {
            match m {
                Message::User { content, images } => {
                    let mut payload = json!({"content": content});
                    if let Some(imgs) = images {
                        payload["images"] = json!(imgs);
                    }
                    self.fork_append(&new_id, types::USER_MESSAGE, payload, Some(m.clone()))?;
                }
                Message::Assistant {
                    content,
                    tool_calls,
                    reasoning,
                } => {
                    for tc in tool_calls {
                        calls.insert(
                            tc.id.clone(),
                            (tc.function.name.clone(), tc.function.arguments.clone()),
                        );
                    }
                    self.fork_append(
                        &new_id,
                        types::ASSISTANT_MESSAGE,
                        json!({
                            "content": content,
                            "reasoning_content": reasoning.clone().unwrap_or_default(),
                            "tool_calls": tool_calls,
                        }),
                        Some(m.clone()),
                    )?;
                }
                Message::Tool {
                    tool_call_id,
                    content,
                } => {
                    if let Some((name, args)) = calls.get(tool_call_id) {
                        self.fork_append(
                            &new_id,
                            types::TOOL_CALL,
                            json!({"call_id": tool_call_id, "name": name, "arguments": args}),
                            None,
                        )?;
                    }
                    self.fork_append(
                        &new_id,
                        types::TOOL_RESULT,
                        json!({"call_id": tool_call_id, "value": content}),
                        Some(m.clone()),
                    )?;
                }
            }
        }
        // 标题：原标题或首条消息提炼，加分支前缀
        let base = if title.trim().is_empty() {
            messages
                .iter()
                .find_map(|m| match m {
                    Message::User { content, .. } => {
                        Some(crate::core::session::title_from_message(content))
                    }
                    _ => None,
                })
                .filter(|t| !t.is_empty())
                .unwrap_or_else(|| "会话".into())
        } else {
            title
        };
        self.rename_session(&new_id, &format!("⑂ {base}"))?;
        info!("forked session {session_id} -> {new_id} ({} messages)", messages.len());
        Ok(new_id)
    }

    /// fork 事件落地：store 持久化 + 引擎缓存/共享 map 同步 + 事件流出。
    fn fork_append(
        &mut self,
        session_id: &str,
        ty: &str,
        data: serde_json::Value,
        msg: Option<Message>,
    ) -> Result<()> {
        let ev = SessionEvent::new(ty, Some(data));
        self.store.append(session_id, &ev)?;
        if let Some(s) = self.sessions.get_mut(session_id) {
            s.push_event(ev.clone());
            if let Some(m) = &msg {
                s.messages.push(m.clone());
            }
        }
        {
            let mut shared = lock_shared(&self.sessions_shared);
            if let Some(s) = shared.get_mut(session_id) {
                if let Some(m) = &msg {
                    s.messages.push(m.clone());
                }
            }
        }
        let _ = self.tx.send(EngineEvent::Event {
            session_id: session_id.to_string(),
            event: ev,
        });
        Ok(())
    }

    /// 记录消息反馈（👍/👎）：registry 更新 + feedback/record 事件持久化。
    pub fn record_message_feedback(
        &mut self,
        session_id: &str,
        message_id: &str,
        kind: crate::engine::FeedbackKind,
    ) -> Result<()> {
        {
            let mut fb = self
                .feedback
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            fb.record(message_id, kind);
        }
        let ev = crate::engine::FeedbackStore::record_event(message_id, kind);
        self.store.append(session_id, &ev)?;
        let _ = self.tx.send(EngineEvent::Event {
            session_id: session_id.to_string(),
            event: ev,
        });
        Ok(())
    }

    /// 跨会话全文搜索：标题/ID/消息内容不区分大小写匹配，
    /// 返回 (会话摘要, 首个命中片段)。片段取命中点前后各 ~30 字符。
    /// 注意：每次调用从磁盘重放全部会话——侧栏逐键调用时可接受
    /// （会话数几十级别、JSONL 小），大量会话时需加缓存。
    pub fn search_sessions(
        &self,
        query: &str,
        limit: usize,
    ) -> Vec<(crate::core::session::SessionSummary, String)> {
        let q = query.to_lowercase();
        if q.is_empty() || limit == 0 {
            return Vec::new();
        }
        let mut out = Vec::new();
        for summary in self.list_sessions() {
            if out.len() >= limit {
                break;
            }
            // 标题/ID 命中：无片段
            if summary.title.to_lowercase().contains(&q)
                || summary.session_id.to_lowercase().contains(&q)
            {
                out.push((summary, String::new()));
                continue;
            }
            // 内容命中：取首个命中片段
            let session = self.store.load_session(&summary.session_id);
            if let Ok(sess) = session {
                for m in &sess.messages {
                    let content = match m {
                        Message::User { content, .. } => content,
                        Message::Assistant { content, .. } => content,
                        Message::Tool { .. } => continue,
                    };
                    if let Some(pos) = content.to_lowercase().find(&q) {
                        let chars: Vec<char> = content.chars().collect();
                        let pos_c = content[..pos].chars().count();
                        let lo = pos_c.saturating_sub(30);
                        let hi = (pos_c + q.chars().count() + 30).min(chars.len());
                        let mut snip: String = chars[lo..hi].iter().collect();
                        if lo > 0 {
                            snip.insert(0, '…');
                        }
                        if hi < chars.len() {
                            snip.push('…');
                        }
                        out.push((summary, snip.replace('\n', " ")));
                        break;
                    }
                }
            }
        }
        out
    }

    /// 本地时间 "YYYY-MM-DD HH:MM"（无 chrono 依赖的天数换算）。
    fn export_timestamp() -> String {
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        // UTC+8（用户时区为东八区；导出时间戳仅作参考）
        let local = secs + 8 * 3600;
        let days = local.div_euclid(86400);
        let tod = local.rem_euclid(86400);
        // 1970-01-01 起的 civil 日历换算（Howard Hinnant 算法）
        let z = days + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z.rem_euclid(146_097);
        let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
        let y = yoe + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = doy - (153 * mp + 2) / 5 + 1;
        let m = if mp < 10 { mp + 3 } else { mp - 9 };
        let y = if m <= 2 { y + 1 } else { y };
        format!("{y:04}-{m:02}-{d:02} {:02}:{:02}", tod / 3600, tod % 3600 / 60)
    }

    /// 会话导出为 Markdown（用户/AI 消息完整保留，工具调用与结果以
    /// 代码块附录，思考过程折叠于 details——可直接归档/分享）。
    pub fn export_session_markdown(&self, session_id: &str) -> Result<String> {
        let sess = self
            .store
            .load_session(session_id)
            .map_err(|e| anyhow::anyhow!("load session failed: {e:#}"))?;
        let title = if sess.title.is_empty() {
            "未命名会话".to_string()
        } else {
            sess.title.clone()
        };
        let mut out = format!(
            "# {title}

> 会话 `{}` · {} 条消息 · 导出于 {}

---

",
            sess.id,
            sess.messages.len(),
            Self::export_timestamp(),
        );
        for m in &sess.messages {
            match m {
                Message::User { content, .. } => {
                    out.push_str(&format!("## 🧑 用户

{content}

"));
                }
                Message::Assistant {
                    content,
                    tool_calls,
                    reasoning,
                } => {
                    if let Some(r) = reasoning.as_ref().filter(|r| !r.trim().is_empty()) {
                        out.push_str(&format!(
                            "<details><summary>💭 思考过程</summary>

{}

</details>

",
                            r
                        ));
                    }
                    if !content.trim().is_empty() {
                        out.push_str(&format!("## 🤖 助手

{content}

"));
                    }
                    for tc in tool_calls {
                        out.push_str(&format!(
                            "**🔧 {}**

```json
{}
```

",
                            tc.function.name, tc.function.arguments
                        ));
                    }
                }
                Message::Tool {
                    tool_call_id,
                    content,
                } => {
                    out.push_str(&format!(
                        "<details><summary>📎 工具结果（{tool_call_id}）</summary>

```json
{}
```

</details>

",
                        {
                            let t: String = content.chars().take(2000).collect();
                            t
                        }
                    ));
                }
            }
        }
        Ok(out)
    }

    /// 新增定时任务：到期把 prompt 发送到绑定会话（发起 AI 回合）。
    /// 持久化到 settings（重启恢复）。返回任务 id。
    pub fn schedule_add(
        &mut self,
        name: &str,
        prompt: &str,
        interval_secs: u64,
        session_id: &str,
    ) -> String {
        let id = format!("sc-{}", uuid());
        {
            let mut s = self
                .scheduler
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            s.add(&id, name, interval_secs, prompt, session_id);
        }
        self.persist_scheduled();
        info!("scheduled task added: {id} ({name}) every {interval_secs}s -> {session_id}");
        id
    }

    /// 删除定时任务（同步持久化）。
    pub fn schedule_remove(&mut self, id: &str) {
        self.scheduler
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(id);
        self.persist_scheduled();
    }

    /// 启停定时任务（同步持久化）。
    pub fn schedule_toggle(&mut self, id: &str, enabled: bool) {
        self.scheduler
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .toggle(id, enabled);
        self.persist_scheduled();
    }

    /// 定时任务快照（UI 列表）。
    pub fn schedule_list(&self) -> Vec<crate::engine::schedule::ScheduledTask> {
        self.scheduler
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .list()
            .into_iter()
            .cloned()
            .collect()
    }

    /// 到期任务触发：把 prompt 发到绑定会话（由 UI 事件泵调用——
    /// 调度线程没有引擎访问权）。会话已删除则移除任务。
    pub fn fire_scheduled(&mut self, id: &str) {
        let task = {
            let s = self
                .scheduler
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            s.list().into_iter().find(|t| t.id == id).cloned()
        };
        let Some(t) = task else { return };
        // 存活判定查 store（磁盘事实）而非内存 map——重启后仅首个会话被
        // 打开，内存不含其余会话，按内存判会把合法定时任务误删（历史 bug）。
        let alive = self.store.list_sessions().iter().any(|sid| sid == &t.session_id);
        if !alive {
            info!("scheduled task {id}: session {} gone, removing", t.session_id);
            self.schedule_remove(id);
            return;
        }
        info!("scheduled task {id} firing -> session {}", t.session_id);
        if let Err(e) = self.send_message(&t.session_id, &t.prompt) {
            log::warn!("scheduled send failed for {id}: {e:#}");
        }
    }

    /// 调度器 → settings 持久化。
    fn persist_scheduled(&mut self) {
        let tasks: Vec<_> = {
            let s = self
                .scheduler
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            s.list()
                .into_iter()
                .map(|t| crate::core::settings::StoredScheduledTask {
                    id: t.id.clone(),
                    name: t.name.clone(),
                    interval_secs: t.interval_secs,
                    prompt: t.prompt.clone(),
                    session_id: t.session_id.clone(),
                    enabled: t.enabled,
                })
                .collect()
        };
        self.settings.scheduled_tasks = tasks;
        let _ = self.settings.save();
    }

    /// 查询消息反馈（UI 高亮当前态）。
    pub fn message_feedback(
        &self,
        message_id: &str,
    ) -> Option<crate::engine::FeedbackKind> {
        self.feedback
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(message_id)
    }

    pub fn rename_session(&mut self, session_id: &str, new_title: &str) -> Result<()> {
        let title = new_title.trim();
        if title.is_empty() {
            return Ok(());
        }
        let ev = SessionEvent::new(types::SESSION_TITLE, Some(json!({"title": title})));
        // 会话已删除：直接报错（不落盘重建文件）
        if !self.sessions.contains_key(session_id)
            && !lock_shared(&self.sessions_shared).contains_key(session_id)
        {
            return Err(anyhow::anyhow!("session not found: {session_id}"));
        }
        // 引擎缓存
        if let Some(s) = self.sessions.get_mut(session_id) {
            s.title = title.to_string();
            s.push_event(ev.clone());
        }
        // 共享 map
        {
            let mut shared = lock_shared(&self.sessions_shared);
            if let Some(s) = shared.get_mut(session_id) {
                s.title = title.to_string();
            }
        }
        // 持久化（重放恢复标题）
        self.store.append(session_id, &ev)?;
        // 通知 UI 刷新列表
        let _ = self.tx.send(EngineEvent::Event {
            session_id: session_id.to_string(),
            event: ev,
        });
        info!("renamed session {session_id} -> {title}");
        Ok(())
    }

    /// 删除会话：存储文件 + 内存缓存 + 共享 map + 回合信号/锁。
    /// 运行中的会话置停止标志（回合线程在每个落盘点前检查存活，不再重建文件）。
    pub fn delete_session(&mut self, session_id: &str) -> Result<()> {
        if let Some(sig) = self.turn_signals.get(session_id) {
            sig.stopping
                .store(true, std::sync::atomic::Ordering::Relaxed);
            sig.cancelled
                .store(true, std::sync::atomic::Ordering::Relaxed);
        }
        self.store.delete(session_id)?;
        self.sessions.remove(session_id);
        {
            let mut shared = lock_shared(&self.sessions_shared);
            shared.remove(session_id);
        }
        self.turn_signals.remove(session_id);
        self.turn_locks.lock().unwrap().remove(session_id);
        self.queued_messages.remove(session_id);
        // 清用量记账并置脏（历史缺陷:不清理 → settings 无限膨胀;
        // 只清 map 不置脏 → settings 里残留死条目直到下次 LLM 调用）
        self.token_usage
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(session_id);
        self.usage_dirty
            .store(true, std::sync::atomic::Ordering::Relaxed);
        // 清除该会话全部挂起审批（历史安全缺陷：悬卡点允许会唤醒已删
        // 会话的旧回合执行真实文件写入；点 AlwaysAllow 还污染全局放行表）
        self.approvals
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .cancel_session(session_id);
        info!("deleted session {session_id}");
        Ok(())
    }

    /// 停止当前回合：置停止信号 + 双 map running=false（UI 立即回到可发送态）。
    /// 回合线程在最近的检查点退出（turn_lock 保证随后启动的新回合不会与它并发）。
    pub fn cancel(&mut self, session_id: &str) {
        if let Some(sig) = self.turn_signals.get(session_id) {
            sig.stopping
                .store(true, std::sync::atomic::Ordering::Relaxed);
            sig.cancelled
                .store(true, std::sync::atomic::Ordering::Relaxed);
        }
        if let Some(s) = self.sessions.get_mut(session_id) {
            s.running = false;
        }
        {
            let mut shared = lock_shared(&self.sessions_shared);
            if let Some(s) = shared.get_mut(session_id) {
                s.running = false;
            }
        }
        let _ = self.tx.send(EngineEvent::StatusChanged {
            session_id: session_id.to_string(),
            status: AgentStatus::Stopped,
        });
    }

    /// 回传一次审批决定（UI 调用）。返回是否命中了一个待审批请求。
    pub fn resolve_approval(
        &self,
        id: &str,
        decision: crate::engine::approval::ApprovalDecision,
    ) -> bool {
        self.approvals.lock().unwrap().resolve(id, decision)
    }

    /// 当前所有待审批请求的快照（UI 渲染确认卡片）。
    pub fn pending_approvals(&self) -> Vec<crate::engine::approval::ApprovalRequest> {
        self.approvals.lock().unwrap().pending_snapshot()
    }

    /// 待审批数量。
    pub fn pending_approval_count(&self) -> usize {
        self.approvals.lock().unwrap().pending_count()
    }

    /// 登记一个审批请求（测试/桥编程入口；回合内由 run_approval 调用）。
    /// 返回审批 id，UI 可通过 pending_approvals() 看到、resolve_approval() 回传。
    pub fn request_approval(&self, session_id: &str, target: &str, reason: &str) -> String {
        let id = uuid();
        let mut reg = self.approvals.lock().unwrap();
        let _rx = reg.request(
            id.clone(),
            session_id.to_string(),
            target.to_string(),
            reason.to_string(),
        );
        id
    }

    /// 是否已经"总是允许"了某个目标路径。
    pub fn is_always_allowed(&self, target: &str) -> bool {
        self.approvals.lock().unwrap().should_skip(target)
    }

    /// 插话：回合运行中发送消息。
    ///
    /// 用户消息立即写入会话（本地 + 共享 + 持久化 + 事件），UI 同步显示；
    /// 同时置取消标志打断当前 step。运行中的回合在下一轮检测到
    /// "store 中有比初始快照更新的用户消息"后自动续跑处理插话
    /// （见 run_turn_inner 的 cancelled 分支），无需等待回合结束。
    fn interject(
        &mut self,
        session_id: &str,
        content: &str,
        images: Vec<String>,
    ) -> Result<()> {
        // 插话同样携带图片（vision 多模态）：事件与消息都带 images
        let mut payload = json!({"content": content});
        if !images.is_empty() {
            payload["images"] = json!(images);
        }
        let ev = SessionEvent::new(types::USER_MESSAGE, Some(payload));
        {
            let session = self
                .sessions
                .get_mut(session_id)
                .ok_or_else(|| anyhow::anyhow!("session not open: {session_id}"))?;
            session.push_event(ev.clone());
            session.messages.push(Message::User {
                content: content.to_string(),
                images: if images.is_empty() { None } else { Some(images) },
            });
        }
        {
            let mut shared = lock_shared(&self.sessions_shared);
            if let Some(s) = shared.get_mut(session_id) {
                s.messages = self
                    .sessions
                    .get(session_id)
                    .map(|x| x.messages.clone())
                    .unwrap_or_default();
            }
        }
        if let Err(e) = self.store.append(session_id, &ev) {
            // 回滚内存（历史缺陷：persist 失败不回滚，内存里留下
            // store 没有的消息，TurnGuard 重载后被静默抹掉）
            if let Some(s) = self.sessions.get_mut(session_id) {
                s.messages.pop();
                s.events.pop();
            }
            lock_shared(&self.sessions_shared)
                .get_mut(session_id)
                .map(|s| {
                    s.messages.pop();
                });
            return Err(anyhow::anyhow!("插话消息持久化失败：{e:#}"));
        }
        let _ = self.tx.send(EngineEvent::Event {
            session_id: session_id.to_string(),
            event: ev,
        });
        // 打断当前 step（旧回合续跑逻辑会接管插话；只置 cancelled——
        // 这是"续跑"信号，不能误置 stopping 把回合停掉）
        let flag = self
            .turn_signals
            .get(session_id)
            .cloned()
            .unwrap_or_else(|| {
                self.turn_signals
                    .entry(session_id.to_string())
                    .or_insert_with(|| Arc::new(TurnSignal::default()))
                    .clone()
            });
        flag.cancelled
            .store(true, std::sync::atomic::Ordering::Relaxed);
        info!("interject: message queued for running session {session_id}");
        Ok(())
    }

    fn persist(&self, session_id: &str, ev: &SessionEvent) -> Result<()> {
        self.store.append(session_id, ev)
    }
}

/// 工具注册表的线程安全句柄（保留沙箱配置）。
impl ToolRegistry {
    fn clone_handle(&self) -> ToolRegistry {
        self.with_cwd(self.cwd.clone())
    }
}

impl SessionStore {
    fn clone_handle(&self) -> SessionStore {
        // 直接构造（不走 SessionStore::new 的 create_dir_all——目录已存在，
        // 且失败时 unwrap 会毒化持引擎锁的 UI 线程）。墓碑必须 Arc 共享：
        // 回合线程的副本必须看到引擎实例的删除记录，否则 TOCTOU 复活
        // 防护对真正要防的路径（回合线程 append）无效。
        SessionStore {
            dir: self.dir.clone(),
            tombstones: std::sync::Arc::clone(&self.tombstones),
        }
    }
}

/// 权限审批结果。
enum ApprovalOutcome {
    Allowed,
    Denied { note: String },
}

/// 可中止审批：登记请求 → 发 UI 事件 → await 用户决定（300s 超时默认拒绝；
/// 审批：监听停止信号（历史缺陷：await 只有 300s 超时，用户点停止后
/// 再点"允许"，被批准的写工具仍会执行——stop 穿透）。signal 触发即拒。
async fn run_approval_cancellable(
    tx: &Sender<EngineEvent>,
    approvals: &Arc<std::sync::Mutex<crate::engine::approval::ApprovalRegistry>>,
    target: String,
    reason: String,
    session_id: &str,
    signal: Option<&Arc<TurnSignal>>,
) -> anyhow::Result<ApprovalOutcome> {
    let id = uuid();
    let rx = {
        let mut reg = approvals.lock().unwrap();
        reg.request(
            id.clone(),
            session_id.to_string(),
            target.clone(),
            reason.clone(),
        )
    };
    let _ = tx.send(EngineEvent::ApprovalRequested {
        session_id: session_id.to_string(),
        id: id.clone(),
        target: target.clone(),
        reason: reason.clone(),
    });
    // 停止信号轮询（10Hz）+ 300s 超时 + 用户决定 三路竞争
    let started = std::time::Instant::now();
    let mut rx = rx;
    let decision = loop {
        if signal
            .as_ref()
            .is_some_and(|sg| TurnSignal::is_set(&sg.stopping))
        {
            approvals.lock().unwrap().cancel(&id);
            return Ok(ApprovalOutcome::Denied {
                note: "回合已停止，审批作废".into(),
            });
        }
        if started.elapsed() >= std::time::Duration::from_secs(300) {
            approvals.lock().unwrap().cancel(&id);
            return Ok(ApprovalOutcome::Denied {
                note: "审批超时（300s 未响应），已拒绝".into(),
            });
        }
        match tokio::time::timeout(std::time::Duration::from_millis(100), &mut rx).await {
            Ok(Ok(d)) => break d,
            Ok(Err(_)) => {
                approvals.lock().unwrap().cancel(&id);
                return Ok(ApprovalOutcome::Denied {
                    note: "审批通道已关闭（未收到用户决定）".into(),
                });
            }
            Err(_) => continue, // 本轮超时 → 复查停止信号
        }
    };
    match decision {
        crate::engine::approval::ApprovalDecision::Allow
        | crate::engine::approval::ApprovalDecision::AlwaysAllow => Ok(ApprovalOutcome::Allowed),
        crate::engine::approval::ApprovalDecision::Deny => Ok(ApprovalOutcome::Denied {
            note: format!("用户拒绝了该操作"),
        }),
    }
}

/// 回合收尾守卫（RAII）：无论回合正常结束、报错还是线程 panic，Drop 都会：
/// 1. 仅当本回合代数仍是最新时复位 shared running 并发 Idle
///    （被更新的回合取代时不越权复位新回合的状态）；
/// 2. 从存储重放最新消息并同步回共享 map（锁外读盘，短锁写回）；
/// 3. 收尾回合任务（job done/failed）。
struct TurnGuard {
    tx: Sender<EngineEvent>,
    sessions_shared: Arc<std::sync::Mutex<HashMap<String, Session>>>,
    store: SessionStore,
    jobs: Arc<std::sync::Mutex<JobManager>>,
    job_id: String,
    session_id: String,
    epoch: u64,
    result_ok: bool,
}

impl Drop for TurnGuard {
    fn drop(&mut self) {
        // 1) 代数检查 + running 复位（短锁）
        let still_current = {
            let mut map = lock_shared(&self.sessions_shared);
            match map.get_mut(&self.session_id) {
                Some(s) if s.turn_epoch == self.epoch => {
                    s.running = false;
                    s.updated_at = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs_f64();
                    true
                }
                Some(_) => false, // 新回合已接管：不动新回合状态
                None => false,    // 会话已删除：什么都不用做
            }
        };
        // 2) 锁外重放存储，再短锁写回（避免持锁做磁盘 IO 阻塞 UI 线程取锁）
        if still_current {
            if let Ok(loaded) = self.store.load_session(&self.session_id) {
                let mut map = lock_shared(&self.sessions_shared);
                if let Some(s) = map.get_mut(&self.session_id) {
                    if s.turn_epoch == self.epoch {
                        s.messages = loaded.messages;
                        s.events = loaded.events;
                    }
                }
            }
            let _ = self.tx.send(EngineEvent::StatusChanged {
                session_id: self.session_id.clone(),
                status: AgentStatus::Idle,
            });
        }
        debug!(
            "turn finished for {} (epoch {}, current={})",
            self.session_id, self.epoch, still_current
        );
        // 3) 回合任务收尾
        let ok = self.result_ok;
        let mut jm = self.jobs.lock().unwrap_or_else(|p| p.into_inner());
        jm.finish(
            &self.job_id,
            ok,
            if ok {
                Some("回合完成".to_string())
            } else {
                Some("回合失败/中止".to_string())
            },
        );
    }
}

/// 读取工作区根的 AGENTS.md（大小写不敏感；找不到/为空返回 None）。
/// 超 32KB 按字符边界截断（防提示词膨胀，不切坏 UTF-8）。
pub fn read_agents_md(cwd: &Path) -> Option<String> {
    const MAX: usize = 32 * 1024;
    for name in ["AGENTS.md", "agents.md"] {
        if let Ok(text) = std::fs::read_to_string(cwd.join(name)) {
            let text = text.trim();
            if text.is_empty() {
                return None;
            }
            return Some(if text.len() > MAX {
                let cut = text
                    .char_indices()
                    .take_while(|(i, _)| *i <= MAX)
                    .map(|(i, _)| i)
                    .last()
                    .unwrap_or(0);
                format!("{}
…（AGENTS.md 过大已截断）", &text[..cut])
            } else {
                text.to_string()
            });
        }
    }
    None
}

/// persona 注入段截断（提示词膨胀 / prompt-injection 面）。
fn clamp_block(s: &mut String, label: &str) {
    const MAX: usize = 4 * 1024;
    if s.len() > MAX {
        let cut = s
            .char_indices()
            .take_while(|(i, _)| *i <= MAX)
            .map(|(i, _)| i)
            .last()
            .unwrap_or(0);
        s.truncate(cut);
        s.push_str(&format!("...(over-long {label} list truncated)"));
    }
}

/// 单个 subagent_fork 的完整执行（供 join_all 并发调用）：
/// spawn(带任务) → running descriptor → 独立 LLM 回合 → 终态 descriptor
/// （含完整 result，重放/卡片展开可见）→ ToolOutput（回喂主代理）。
#[allow(clippy::too_many_arguments)]
async fn run_subagent_fork(
    call_id: String,
    args: Value,
    llm: Arc<LlmClient>,
    subagents: &Arc<std::sync::Mutex<SubagentManager>>,
    jobs: &Arc<std::sync::Mutex<JobManager>>,
    store: &SessionStore,
    sessions_shared: &Arc<std::sync::Mutex<HashMap<String, Session>>>,
    tx: &Sender<EngineEvent>,
    session_id: &str,
    preset: AgentPreset,
    approvals: &Arc<std::sync::Mutex<crate::engine::approval::ApprovalRegistry>>,
    signal: &Arc<TurnSignal>,
    token_usage: &Arc<
        std::sync::Mutex<HashMap<String, crate::core::llm::TokenUsage>>,
    >,
    usage_dirty: &Arc<std::sync::atomic::AtomicBool>,
) -> ToolOutput {
    let _ = call_id;
    if let Some(err) = args.get("__args_parse_error") {
        return ToolOutput::err(format!("subagent_fork: 参数不是合法 JSON: {err}"));
    }
    // 预设白名单（当前两模式均含子代理；保留检查供未来扩展）
    if let Some(wl) = preset.tool_whitelist() {
        if !wl.contains(&"subagent_fork") {
            return ToolOutput::err(format!(
                "subagent_fork 在当前预设（{}）不可用",
                preset.name()
            ));
        }
    }
    let desc = args
        .get("description")
        .and_then(|v| v.as_str())
        .unwrap_or("子代理任务")
        .to_string();
    let prompt = args
        .get("prompt")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    if prompt.trim().is_empty() {
        return ToolOutput::err("subagent_fork: prompt 不能为空");
    }
    // 标准模式：子代理派生需审批（每次 fork = 独立 LLM 回合，token 放大器；
    // "总是允许"可记住决定不再打扰）。自主规划模式：AI 自行决定，免审。
    if preset.subagent_needs_approval() {
        let target = format!("subagent:{desc}");
        let skip = {
            let reg = approvals.lock().unwrap();
            reg.should_skip(&target)
        };
        if !skip {
            match run_approval_cancellable(
                tx,
                approvals,
                target.clone(),
                format!(
                    "派生子代理「{desc}」执行独立 LLM 回合（消耗额外 token）。
任务：{}",
                    prompt.chars().take(200).collect::<String>()
                ),
                session_id,
                Some(signal),
            )
            .await
            {
                Ok(ApprovalOutcome::Allowed) => {}
                Ok(ApprovalOutcome::Denied { note }) => {
                    return ToolOutput::err(format!(
                        "用户拒绝了子代理派生{note}；请直接在主回合完成任务，或询问用户如何拆分"
                    ));
                }
                Err(e) => {
                    return ToolOutput::err(format!("子代理审批失败: {e:#}"));
                }
            }
        }
    }
    // spawn（带任务全文）
    let spawn_res = {
        let mut mgr = subagents.lock().unwrap();
        mgr.spawn(session_id, 1, 3, &prompt)
    };
    let sub_id = match spawn_res {
        Err(e) => return ToolOutput::err(format!("subagent_fork: {e:#}")),
        Ok(id) => id,
    };
    // 子代理任务（pending → running）
    let sjid = {
        let mut jm = jobs.lock().unwrap();
        let id = jm.create("subagent", Some(format!("{desc}（{sub_id}）")));
        jm.start(&id);
        id
    };
    subagents
        .lock()
        .unwrap()
        .mark(&sub_id, SubagentStatus::Running);
    if let Some(d) = subagents.lock().unwrap().get(&sub_id).cloned() {
        let dev = SubagentManager::descriptor_event(&d);
        if let Err(e) = append_alive(store, sessions_shared, session_id, &dev) {
            warn!("subagent descriptor append failed: {e:#}");
        }
        let _ = tx.send(EngineEvent::Event {
            session_id: session_id.to_string(),
            event: dev,
        });
    }
    // 独立 LLM 回合
    let sys = format!(
        "You are a subagent of a coding agent running on DeepSeek          Harness. Your parent session is {session_id}. Complete the          task below and reply with a concise summary of what you did          and the result."
    );
    let summary = match crate::engine::subagent::run_subagent_turn_counted(
        llm, &sys, &prompt, 8,
    )
    .await
    {
        Ok((text, u)) => {
            #[allow(clippy::let_unit_value)]
            let _ = 0;
            // 子代理用量记账到绑定会话（token 放大器必须可见）
            {
                let mut map =
                    token_usage.lock().unwrap_or_else(|p| p.into_inner());
                *map.entry(session_id.to_string()).or_default() += u;
                usage_dirty.store(true, std::sync::atomic::Ordering::Relaxed);
            }
            Ok(text)
        }
        Err(e) => Err(e),
    };
    let out = match &summary {
        Ok(text) => {
            // 摘要（卡片行/主代理）+ 完整输出（用户展开查看）
            let brief: String = text.chars().take(200).collect();
            {
                let mut mgr = subagents.lock().unwrap();
                mgr.set_summary(&sub_id, brief);
                mgr.set_result(&sub_id, text.clone());
                mgr.mark(&sub_id, SubagentStatus::Done);
            }
            jobs.lock().unwrap().finish(&sjid, true, Some(text.clone()));
            ToolOutput::ok(json!({
                "subagent_id": sub_id,
                "summary": text,
                "note": "子代理已完成，总结如上",
            }))
        }
        Err(e) => {
            subagents
                .lock()
                .unwrap()
                .mark(&sub_id, SubagentStatus::Failed);
            jobs.lock()
                .unwrap()
                .finish(&sjid, false, Some(format!("{e:#}")));
            ToolOutput::err(format!("subagent_fork 执行失败: {e:#}"))
        }
    };
    // 终态 descriptor（含 task/result 全文，重放恢复）
    if let Some(d) = subagents.lock().unwrap().get(&sub_id).cloned() {
        let dev = SubagentManager::descriptor_event(&d);
        if let Err(e) = append_alive(store, sessions_shared, session_id, &dev) {
            warn!("subagent final descriptor append failed: {e:#}");
        }
        let _ = tx.send(EngineEvent::Event {
            session_id: session_id.to_string(),
            event: dev,
        });
    }
    out
}

/// 回合主体（LLM 循环 + 工具执行）。
/// 状态收尾见 send_message 线程体中的 TurnGuard。
#[allow(clippy::too_many_arguments)]
async fn run_turn_inner(
    tx: &Sender<EngineEvent>,
    session_id: &str,
    initial_messages: Vec<Message>,
    llm: Arc<LlmClient>,
    tools: ToolRegistry,
    store: SessionStore,
    model: &str,
    sessions_shared: &Arc<std::sync::Mutex<HashMap<String, Session>>>,
    session_cwd: Option<PathBuf>,
    preset: AgentPreset,
    signal: &Arc<TurnSignal>,
    jobs: &Arc<std::sync::Mutex<JobManager>>,
    job_id: &str,
    subagents: &Arc<std::sync::Mutex<SubagentManager>>,
    plugins: &Arc<std::sync::Mutex<crate::engine::plugin::PluginManager>>,
    approvals: &Arc<std::sync::Mutex<crate::engine::approval::ApprovalRegistry>>,
    token_usage: &Arc<
        std::sync::Mutex<HashMap<String, crate::core::llm::TokenUsage>>,
    >,
    usage_dirty: &Arc<std::sync::atomic::AtomicBool>,
    lessons: &Arc<std::sync::Mutex<crate::engine::lessons::LessonStore>>,
) -> Result<()> {
    let mut tools = tools;
    // 教训库分域键：会话工作区（缺省引擎工作目录）
    let lesson_ws = session_cwd
        .as_ref()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| tools.cwd.to_string_lossy().into_owned());
    if let Some(ref cwd) = session_cwd {
        tools = tools.with_cwd(cwd.clone());
    }
    tools = tools.with_preset(preset);
    // dispatch 是同步阻塞（bash/pwsh 最长一个命令超时）：Arc 化以便
    // spawn_blocking 执行，不占用 tokio worker 线程
    let tools = std::sync::Arc::new(tools);
    // 回合开始前检查：会话已删除（delete_session 后不再重建文件）或已被停止
    if !session_alive(sessions_shared, session_id) {
        info!("session {session_id} deleted before turn; skip turn");
        return Ok(());
    }
    if TurnSignal::is_set(&signal.stopping) {
        info!("turn stopped before start for {session_id}");
        return Ok(());
    }
    // turn/start
    let ev = SessionEvent::new(types::TURN_START, Some(json!({"model": model})));
    append_alive(&store, sessions_shared, session_id, &ev)?;
    let _ = tx.send(EngineEvent::Event {
        session_id: session_id.to_string(),
        event: ev,
    });

    let mut messages = initial_messages;
    let spec = {
        // 本地工具 + cordis 风格插件工具（运行中插件动态上报）
        let mut s = tools.tool_specs();
        let ptools = plugins.lock().unwrap().all_tools();
        if !ptools.is_empty() {
            info!(
                "turn spec includes plugin tools: {}",
                ptools
                    .iter()
                    .map(|t| t.name.clone())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        for pt in ptools {
            // 插件工具可**覆盖**内置同名工具（如提供真正的 web_search 实现，
            // 核心层能力增强）；同名不去重会被 API 拒绝（duplicate tool）。
            // 沙箱关键名（bash/pwsh/write_file/str_replace_editor/node_called）
            // 不允许覆盖——插件进程跑在完整令牌下，覆盖即静默绕过受限令牌
            // 与应用层写门（历史安全缺陷）。插件仍可注册新名工具。
            const SANDBOX_CRITICAL: &[&str] = &[
                "bash",
                "pwsh",
                "write_file",
                "str_replace_editor",
                "node_called",
                "run_code",
                "read_file",
            ];
            if SANDBOX_CRITICAL.contains(&pt.name.as_str()) {
                warn!(
                    "plugin tool '{}' blocked from overriding sandbox-critical built-in",
                    pt.name
                );
                continue;
            }
            s.retain(|x| x.function.name != pt.name);
            s.push(crate::core::llm::tool_spec(
                &pt.name,
                pt.description.as_deref().unwrap_or("插件工具"),
                pt.parameters.clone(),
            ));
        }
        s
    };
    // 回合步数：不做上限（用户要求）。循环由工具收敛/取消/插话自然退出；
    // 步数仅用于任务进度显示。
    let mut step = 0;
    // 本回合是否已压缩过 + 压缩时消息基数（防每 step 重复压缩膨胀事件）
    let mut compacted_baseline: Option<usize> = None;

    'outer: loop {
        // 停止（cancel）：立即退出回合，不续跑。
        // 插话（cancelled）：吸收 store 中更新的用户消息后续跑——
        // 两个独立信号消除了旧实现"靠消息数差异猜测用户意图"的歧义
        // （停止被误判为插话导致回合停不下来 / 插话被误判为停止丢失续跑）。
        if TurnSignal::is_set(&signal.stopping) {
            info!("turn stopped for {session_id}");
            break 'outer;
        }
        if TurnSignal::is_set(&signal.cancelled) {
            let new_user_msg = store
                .load_session(session_id)
                .map(|s| s.messages.len() > messages.len())
                .unwrap_or(false);
            if new_user_msg {
                info!("turn interject detected for {session_id}, continuing");
                signal
                    .cancelled
                    .store(false, std::sync::atomic::Ordering::Relaxed);
                if let Ok(latest) = store.load_session(session_id) {
                    messages = latest.messages;
                }
            } else {
                // 打断信号置位但没有可吸收的新消息：视为停止，防死循环
                info!("turn interrupted without new message for {session_id}, stopping");
                break 'outer;
            }
        }
        // 上下文压缩：长回合消息数超阈值时压缩头部（否则步数不限的长回合
        // 把完整历史 + 每步最多 8KB 的工具结果反复重发，直到 API 拒绝）。
        // compaction/* 事件持久化（重放可见压缩历史）。
        const COMPACTION_THRESHOLD: usize = 30;
        const COMPACTION_KEEP_TAIL: usize = 24;
        // token 估算：消息字符总量 > 80K（≈ 20K token）也触发压缩
        let est_chars: usize = messages
            .iter()
            .map(|m| match m {
                Message::User { content, .. } | Message::Assistant { content, .. } => content.len(),
                Message::Tool { content, .. } => content.len(),
            })
            .sum();
        const CHAR_LIMIT: usize = 80_000;
        // 防膨胀：上一 step 已压缩且消息数没再显著增长（阈值*1.5）就不重复压缩。
        // 历史缺陷：keep_tail 条超长工具结果仍超 CHAR_LIMIT 时，每个 step 都
        // 追加 3 条压缩事件，JSONL 无界膨胀。
        let grown_since_compact = messages.len() as f64
            > compacted_baseline.map(|b| b as f64 * 1.5).unwrap_or(0.0);
        let should_compact = preset.has_compaction()
            && (messages.len() > COMPACTION_THRESHOLD || est_chars > CHAR_LIMIT)
            && (compacted_baseline.is_none() || grown_since_compact);
        if should_compact {
            let plan = crate::engine::compaction::plan_compaction(&messages, COMPACTION_KEEP_TAIL);
            if plan.removed > 0 {
                compacted_baseline = Some(messages.len());
                for ev in [
                    crate::engine::compaction::compaction_start_event(),
                    crate::engine::compaction::compaction_summary_event(
                        &plan.summary,
                        plan.keep_from,
                    ),
                    crate::engine::compaction::compaction_end_event(),
                ] {
                    if let Err(e) = append_alive(&store, sessions_shared, session_id, &ev) {
                        warn!("compaction event append failed: {e:#}");
                    }
                    let _ = tx.send(EngineEvent::Event {
                        session_id: session_id.to_string(),
                        event: ev,
                    });
                }
                info!(
                    "compaction for {session_id}: removed {} messages ({} -> {})",
                    plan.removed,
                    messages.len(),
                    plan.kept.len()
                );
                messages = plan.kept;
            }
        }
        step += 1;
        let step_ev = SessionEvent::new(types::STEP_START, None);
        append_alive(&store, sessions_shared, session_id, &step_ev)?;
        let _ = tx.send(EngineEvent::Event {
            session_id: session_id.to_string(),
            event: step_ev,
        });
        // 任务进度：第 N 步
        jobs.lock()
            .unwrap()
            .progress(job_id, format!("第 {step} 步"));

        let mut llm_messages = LlmClient::to_llm_messages(&messages);
        // persona（系统提示）注入：按预设生成 + 可用技能列表
        {
            let cwd_str = session_cwd
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|| tools.cwd.to_string_lossy().into_owned());
            let mut persona = preset.persona(model, &cwd_str);
            // AGENTS.md 项目记忆：工作区根的约定文件自动注入系统提示
            // （项目结构/编码规范/历史教训等，用户与 AI 均可维护——AI 用
            // write_file 修改走既有审批/沙箱通道）。
            if let Some(text) =
                read_agents_md(session_cwd.as_ref().unwrap_or(&tools.cwd))
            {
                persona.push_str(&format!(
                    "

=== AGENTS.md (project instructions from the workspace root) ===
{text}
=== end AGENTS.md ===
"
                ));
            }
            // 工具教训注入：本工作区已失败过的调用 + 已验证修正方案
            //（模型下回合直接跑修正版，不再原样重试）
            {
                let block = lessons
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .prompt_block(&lesson_ws, 6);
                if let Some(b) = block {
                    persona.push_str("\n\n");
                    persona.push_str(&b);
                }
            }
            // 协作机制引导 + 输出风格
            persona.push_str(
                "\n\nCollaboration mechanisms:\n\
                 - Plan: complex multi-step → plan_write, then exit_plan_mode.\n\
                 - Subagents: independent subtasks → subagent_fork (parallel).\n\
                 - Jobs: automatic.\n\
                 - GUI automation: take_screenshot to locate (coordinates = screenshot pixels), then mouse_click / mouse_move / mouse_drag / mouse_scroll / key_type / key_press. Always screenshot FIRST, then act; after acting, screenshot again to verify.\n\
                 \n\
                 Output style (IMPORTANT):\n\
                 - CONCISE. No filler, no emoji unless user uses them, no \"let me\" narration.\n\
                 - State what you did and the result, 1-3 sentences max.\n\
                 - Code: show only changed lines / minimal diff, not entire files.\n\
                 - Lists: bullet points, not full sentences.\n\
                 - Task succeeded → say so briefly and stop. Don't over-explain.\n",
            );
            // 可用技能（modelInvocable）列出；模型可按需 load_skill 加载说明
            let invocable: Vec<_> = tools
                .skills
                .iter()
                .filter(|s| s.invocation.model_invocable)
                .collect();
            if !invocable.is_empty() {
                let mut list = String::from("\n\nAvailable skills (load one with load_skill when the task matches, then follow its instructions):\n");
                for s in &invocable {
                    list.push_str(&format!(
                        "- {}: {}\n",
                        s.name,
                        s.description.as_deref().unwrap_or("")
                    ));
                }
                clamp_block(&mut list, "inject");
                persona.push_str(&list);
            }
            // 可用插件工具（运行中扩展插件；与普通工具一样直接调用）。
            // 明确告知：这些是**原生协议工具**（子进程插件），已在工具列表中，
            // 直接调用即可——**不要用 node_called 验证它们**（node_called 只用于
            // 加载 JS/cordis 包，查不到子进程插件，会得出"未注册"的错误结论）。
            let plugin_tools = plugins.lock().unwrap().all_tools();
            if !plugin_tools.is_empty() {
                let mut list = String::from(
                    "\n\nAvailable plugin tools (native protocol tools from running subprocess extensions — \
                     they are ALREADY in your tool list; call them directly like any built-in tool. \
                     Do NOT verify them with node_called: node_called only loads JS/cordis packages and \
                     cannot see these subprocess tools):\n",
                );
                for t in &plugin_tools {
                    list.push_str(&format!(
                        "- {}: {}\n",
                        t.name,
                        t.description.as_deref().unwrap_or("")
                    ));
                }
                clamp_block(&mut list, "inject");
                persona.push_str(&list);
            }
            // DSH 官方 cordis 插件：通过 node_called 工具直接调用（无需转写、无需安装）。
            // 查找/发现 JS 插件用 node_called；这是 node_called 的用途。
            let dsh_plugins =
                crate::dsh::plugins::scan_dsh_plugins(&crate::config::AppConfig::load());
            if !dsh_plugins.is_empty() {
                let mut list = String::from(
                    "\n\nDSH official JS/cordis plugins (locate and call them ONLY via the node_called tool; no install or transcription needed):\n",
                );
                for (name, desc, _path) in &dsh_plugins {
                    let desc = if desc.is_empty() {
                        String::new()
                    } else {
                        format!(" — {desc}")
                    };
                    list.push_str(&format!("- {name}{desc}\n"));
                }
                list.push_str(
                    "Usage: node_called {\"code\": \"const p = require('<package-name>'); ...\"} — \
                     the Node environment already has NODE_PATH pointing at the DSH node_modules, \
                     so require() resolves these packages directly. Use their exported services/commands \
                     to fulfill the user's request. Wrap long-running work in try/catch and print results with console.log.\n",
                );
                clamp_block(&mut list, "inject");
                persona.push_str(&list);
            }
            llm_messages.insert(
                0,
                LlmMessage {
                    role: LlmRole::System,
                    content: Some(persona),
                    tool_call_id: None,
                    tool_calls: None,
                    name: None,
                    images: None,
                },
            );
        }
        let full_text: Arc<std::sync::Mutex<String>> =
            Arc::new(std::sync::Mutex::new(String::new()));
        let reasoning: Arc<std::sync::Mutex<String>> =
            Arc::new(std::sync::Mutex::new(String::new()));
        let tool_acc: Arc<std::sync::Mutex<HashMap<usize, (String, String, String)>>> =
            Arc::new(std::sync::Mutex::new(HashMap::new()));
        let full_text_cb = full_text.clone();
        let reasoning_cb = reasoning.clone();
        let tool_acc_cb = tool_acc.clone();

        let session_id2 = session_id.to_string();
        let tx2 = tx.clone();
        let usage_acc = token_usage.clone();
        let usage_dirty_cb = usage_dirty.clone();
        let stream_res = llm
            .stream(&llm_messages, Some(&spec), move |evt| match evt {
                StreamEvent::Chunk(c) => {
                    full_text_cb.lock().unwrap().push_str(&c);
                    // 流式增量只发事件不落盘（完整内容由 assistant/message 落盘，避免逐 token IO）
                    let cev =
                        SessionEvent::new(types::ASSISTANT_CHUNK, Some(json!({"content": c})));
                    let _ = tx2.send(EngineEvent::Event {
                        session_id: session_id2.clone(),
                        event: cev,
                    });
                }
                StreamEvent::Reasoning(r) => {
                    reasoning_cb.lock().unwrap().push_str(&r);
                    // 思考增量作为弹幕事件流出（assistant/chunk + reasoning 标记）
                    let rev = SessionEvent::new(
                        types::ASSISTANT_CHUNK,
                        Some(json!({"content": r, "reasoning": true})),
                    );
                    let _ = tx2.send(EngineEvent::Event {
                        session_id: session_id2.clone(),
                        event: rev,
                    });
                }
                StreamEvent::Usage(u) => {
                    // 会话累计（UI 标题行显示 token 用量）+ 置脏（帧循环落盘）
                    let mut map = usage_acc.lock().unwrap_or_else(|p| p.into_inner());
                    let entry = map.entry(session_id2.clone()).or_default();
                    *entry += u;
                    usage_dirty_cb
                        .store(true, std::sync::atomic::Ordering::Relaxed);
                }
                StreamEvent::ToolCallAccum {
                    index,
                    id,
                    name,
                    arguments,
                } => {
                    tool_acc_cb
                        .lock()
                        .unwrap()
                        .insert(index, (id, name, arguments));
                }
                StreamEvent::Done => {}
                StreamEvent::Error(e) => {
                    let _ = tx2.send(EngineEvent::Error {
                        session_id: session_id2.clone(),
                        message: e,
                    });
                }
            })
            .await;

        // 流失败：把已收到的部分内容降级落盘（标记不完整），否则已经流式
        // 显示给用户的文本随回合失败一起消失（连接抖动 = 整段重打的坏体验）
        if let Err(e) = &stream_res {
            let partial = full_text.lock().unwrap().clone();
            if !partial.trim().is_empty() {
                let content = format!("{partial}\n\n【流中断，以上内容不完整（{e:#}）】");
                let ev = SessionEvent::new(
                    types::ASSISTANT_MESSAGE,
                    Some(json!({
                        "content": content,
                        "reasoning_content": String::new(),
                        "tool_calls": [],
                    })),
                );
                if let Err(pe) = append_alive(&store, sessions_shared, session_id, &ev) {
                    warn!("partial assistant/message append failed: {pe:#}");
                }
                let _ = tx.send(EngineEvent::Event {
                    session_id: session_id.to_string(),
                    event: ev.clone(),
                });
                messages.push(Message::Assistant {
                    content,
                    tool_calls: Vec::new(),
                    reasoning: None,
                });
            }
            return Err(anyhow::anyhow!("{e:#}"));
        }

        // assistant/message（含累积的工具调用）
        let full_text = full_text.lock().unwrap().clone();
        let reasoning = reasoning.lock().unwrap().clone();
        let tool_acc = tool_acc.lock().unwrap().clone();
        // 按流式 index 排序，保证与模型返回顺序一致
        let mut tool_acc_sorted: Vec<(usize, (String, String, String))> =
            tool_acc.into_iter().collect();
        tool_acc_sorted.sort_by_key(|(idx, _)| *idx);
        let tool_calls: Vec<ToolCall> = tool_acc_sorted
            .into_iter()
            .map(|(idx, (id, name, args))| finish_tool_call(idx, id, name, args))
            .collect();

        let content_snapshot = full_text.clone();
        let reasoning_snapshot = reasoning.clone();
        let tool_calls_snapshot = tool_calls.clone();
        let msg_ev = SessionEvent::new(
            types::ASSISTANT_MESSAGE,
            Some(json!({
                "content": content_snapshot,
                "reasoning_content": reasoning_snapshot,
                "tool_calls": tool_calls_snapshot,
            })),
        );
        append_alive(&store, sessions_shared, session_id, &msg_ev)?;
        let _ = tx.send(EngineEvent::Event {
            session_id: session_id.to_string(),
            event: msg_ev,
        });

        let assistant_msg = Message::Assistant {
            content: full_text,
            tool_calls: tool_calls.clone(),
            reasoning: if reasoning_snapshot.is_empty() {
                None
            } else {
                Some(reasoning_snapshot.clone())
            },
        };
        messages.push(assistant_msg);

        if tool_calls.is_empty() {
            // 无工具调用 → 回合结束
            let step_end = SessionEvent::new(types::STEP_END, None);
            append_alive(&store, sessions_shared, session_id, &step_end)?;
            let _ = tx.send(EngineEvent::Event {
                session_id: session_id.to_string(),
                event: step_end,
            });
            break 'outer;
        }

        // ===== subagent_fork 并行预执行 =====
        // 工具描述承诺"并行执行独立子任务"，但顺序循环里逐个 await 是串行。
        // 这里把本批所有 subagent_fork 先并发跑完（join_all），结果按 call_id
        // 存入 stash；下方顺序循环只做事件落盘与消息回填——TOOL_CALL/RESULT
        // 事件顺序保持与模型返回一致，主代理视角无变化。
        let mut sub_stash: HashMap<String, ToolOutput> = HashMap::new();
        {
            let forks: Vec<&ToolCall> = tool_calls
                .iter()
                .filter(|tc| tc.function.name == "subagent_fork")
                .collect();
            if !forks.is_empty() {
                info!(
                    "subagent_fork: running {} forks concurrently for {session_id}",
                    forks.len()
                );
                let futs = forks.into_iter().map(|tc| {
                    let parsed: Value = serde_json::from_str(&tc.function.arguments)
                        .unwrap_or_else(|e| json!({"__args_parse_error": e.to_string()}));
                    async {
                        // 停止信号：已停止的回合不再启动子代理（历史缺陷：
                        // join_all 前不检查，停止后本批子代理照烧 token）
                        if TurnSignal::is_set(&signal.stopping) {
                            return (
                                tc.id.clone(),
                                ToolOutput::err("回合已停止，子代理派生取消"),
                            );
                        }
                        let out = run_subagent_fork(
                            tc.id.clone(),
                            parsed,
                            llm.clone(),
                            subagents,
                            jobs,
                            &store,
                            sessions_shared,
                            tx,
                            session_id,
                            preset,
                            approvals,
                            &signal,
                            &token_usage,
                            &usage_dirty,
                        )
                        .await;
                        (tc.id.clone(), out)
                    }
                });
                let results = futures_util::future::join_all(futs).await;
                for (id, out) in results {
                    sub_stash.insert(id, out);
                }
            }
        }
        // 逐个执行工具（停止/插话后不再执行剩余工具）
        for tc in &tool_calls {
            if TurnSignal::is_set(&signal.stopping) || TurnSignal::is_set(&signal.cancelled) {
                info!("turn interrupted mid-tools for {session_id}");
                break;
            }
            let call_ev = SessionEvent::new(
                types::TOOL_CALL,
                Some(json!({
                    "call_id": tc.id,
                    "name": tc.function.name,
                    "arguments": tc.function.arguments,
                })),
            );
            // 工具事件落盘失败降级（记日志继续）：`?` 中止会让 assistant 声明的
            // tool_calls 缺响应，下一轮被裁剪成空壳 assistant → API 400
            if let Err(e) = append_alive(&store, sessions_shared, session_id, &call_ev) {
                warn!("tool/call append failed (continue anyway): {e:#}");
            }
            let _ = tx.send(EngineEvent::Event {
                session_id: session_id.to_string(),
                event: call_ev,
            });

            // 解析参数并分发。参数解析失败回喂明确错误（旧实现静默置 {}
            // → 下游报"missing path"，模型无从修正）
            let parsed_args: Value = match serde_json::from_str(&tc.function.arguments) {
                Ok(v) => v,
                Err(e) => json!({
                    "__args_parse_error": format!("工具参数不是合法 JSON: {e}"),
                    "__raw": tc.function.arguments,
                }),
            };
            // 教训拦截：与已失败教训**完全相同**的调用且失败≥2 次（或已有
            // 修正方案）→ 不再执行，直接返回教训提示（模型下一轮直跑修正版；
            // 同时打断同参死循环）。subagent_fork 走预执行路径，不拦。
            let lesson_blocked = if tc.function.name != "subagent_fork" {
                lessons
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .blocked_hint(&lesson_ws, &tc.function.name, &parsed_args)
                    .map(ToolOutput::err)
            } else {
                None
            };
            // 预设白名单（对所有执行路径生效：内置 / subagent_fork / 插件工具）
            let blocked_by_whitelist = preset
                .tool_whitelist()
                .map(|wl| !wl.contains(&tc.function.name.as_str()))
                .unwrap_or(false);
            let mut out = if let Some(b) = lesson_blocked {
                b
            } else if blocked_by_whitelist {
                ToolOutput::err(format!(
                    "tool {} 在当前预设（{}）不可用",
                    tc.function.name,
                    preset.name()
                ))
            } else if tc.function.name == "subagent_fork" {
                // 子代理结果：并行预执行阶段已完成（见上方 join_all），
                // 此处仅按 call_id 取回，保持事件/消息顺序与模型返回一致。
                sub_stash.remove(&tc.id).unwrap_or_else(|| {
                    ToolOutput::err("subagent_fork: 预执行结果缺失（内部错误）")
                })
            } else {
                // 公共治理门：越权审批。插件工具与内置工具一视同仁——
                // 插件可以覆盖内置工具（如提供真正的 web_search 实现，
                // 核心层能力增强而非提示词注入），但不得绕过审批
                // （历史漏洞：插件工具分支在审批检查之前，同名即越狱）。
                let tool_name = tc.function.name.clone();
                let mut out: ToolOutput = ToolOutput::ok(json!({}));
                {
                    // 越权审批（插件提供 write_file/bash 等写类同名工具时同样受审）
                    let approval_target =
                        tools.potential_out_of_workspace(&tool_name, &parsed_args);
                    let approved = match approval_target {
                        None => true,
                        Some((target, reason)) => {
                            let skip = {
                                let reg = approvals.lock().unwrap();
                                reg.should_skip(&target)
                            };
                            if skip {
                                true
                            } else {
                                match run_approval_cancellable(
                                    tx,
                                    approvals,
                                    target,
                                    reason,
                                    session_id,
                                    Some(&signal),
                                )
                                .await
                                {
                                    Ok(ApprovalOutcome::Allowed) => true,
                                    Ok(ApprovalOutcome::Denied { note }) => {
                                        out = ToolOutput::err(format!(
                                            "已拒绝权限审批：{note}\n请改为在工作区内操作，或说明理由请求允许。"
                                        ));
                                        false
                                    }
                                    Err(e) => {
                                        out = ToolOutput::err(format!("权限审批失败：{e:#}"));
                                        false
                                    }
                                }
                            }
                        }
                    };
                    if approved {
                        let has_plugin_owner = plugins
                            .lock()
                            .unwrap()
                            .find_tool_owner(&tool_name)
                            .is_some();
                        out = if has_plugin_owner {
                            // cordis 风格插件工具（含内置同名覆盖）：锁内 prepare
                            // （注册响应通道+发请求），锁外 await。单个失败容错继续
                            // ——`?` 提前返回会让本回合其余 tool_calls 不执行，
                            // assistant 声明的 tool_calls 缺响应 → 下一轮被裁剪
                            // → 回合异常（历史回归："声称 2 个，响应 1 个"）。
                            let prepare = {
                                let mgr = plugins.lock().unwrap();
                                mgr.find_tool_owner(&tool_name)
                                    .ok_or_else(|| anyhow::anyhow!("插件工具不可用: {tool_name}"))
                                    .and_then(|p| {
                                        p.prepare_invoke(&tool_name, &parsed_args)
                                            .map_err(|e| anyhow::anyhow!(e))
                                    })
                            };
                            let result = match prepare {
                                Ok((id, rx)) => {
                                    let awaited = tokio::time::timeout(
                                        std::time::Duration::from_secs(120),
                                        rx,
                                    )
                                    .await;
                                    {
                                        let mgr = plugins.lock().unwrap();
                                        if let Some(p) = mgr.find_tool_owner(&tool_name) {
                                            p.cancel_pending(id);
                                        }
                                    }
                                    match awaited {
                                        Ok(Ok(r)) => r,
                                        Ok(Err(_)) => crate::engine::plugin::PluginToolResult {
                                            ok: false,
                                            value: json!({"error": format!(
                                                "插件工具 {tool_name} 通道关闭"
                                            )}),
                                            stderr: "channel closed".into(),
                                        },
                                        Err(_) => crate::engine::plugin::PluginToolResult {
                                            ok: false,
                                            value: json!({"error": format!(
                                                "插件工具 {tool_name} 超时（120s）"
                                            )}),
                                            stderr: "timeout".into(),
                                        },
                                    }
                                }
                                Err(e) => crate::engine::plugin::PluginToolResult {
                                    ok: false,
                                    value: json!({"error": format!("{e:#}")}),
                                    stderr: String::new(),
                                },
                            };
                            ToolOutput {
                                ok: result.ok,
                                value: result.value,
                                stderr: result.stderr,
                            }
                        } else {
                            // 内置工具：spawn_blocking（最长一个命令超时，
                            // 不能占 async worker）
                            let tools = tools.clone();
                            let name = tool_name.clone();
                            let args = parsed_args.clone();
                            spawn_blocking_dispatch(move || tools.dispatch(&name, &args)).await
                        };
                    }
                }
                out
            };
            // 教训记录：失败 → 记 (工具, 参数, 错误)；成功 → 相似失败配对
            // 修正方案 / 完全相同的失败教训吸收删除。审批拒绝/白名单拦截
            // 属治理提示而非用法错误，同样值得记忆（提示会告诉模型改法）。
            {
                let mut ls = lessons.lock().unwrap_or_else(|p| p.into_inner());
                if out.ok {
                    ls.record_success(&lesson_ws, &tc.function.name, &parsed_args);
                } else {
                    let err_txt = out
                        .value
                        .get("error")
                        .and_then(|v| v.as_str())
                        .map(str::to_string)
                        .filter(|t| !t.trim().is_empty())
                        .or_else(|| {
                            if out.stderr.trim().is_empty() {
                                None
                            } else {
                                Some(out.stderr.clone())
                            }
                        })
                        .unwrap_or_else(|| out.value.to_string());
                    // 治理类错误（权限拦截/审批拒绝/白名单）不记教训：
                    // 用户切换权限模式或审批后同一调用合法，记入会被
                    // blocked_hint 永久拦死
                    if !crate::engine::lessons::is_governance_error(&err_txt) {
                        ls.record_failure(&lesson_ws, &tc.function.name, &parsed_args, &err_txt);
                    }
                }
            }
            let result_ev = SessionEvent::new(
                types::TOOL_RESULT,
                Some(json!({
                    // call_id 持久化：重放按 id 精确配对（FIFO 在交错/中断场景会错配）
                    "call_id": tc.id,
                    "ok": out.ok,
                    "value": out.value,
                    "stderr": out.stderr,
                })),
            );
            // 工具结果落盘失败同样降级（内存 messages 保持配对完整）
            if let Err(e) = append_alive(&store, sessions_shared, session_id, &result_ev) {
                warn!("tool/result append failed (continue anyway): {e:#}");
            }
            let _ = tx.send(EngineEvent::Event {
                session_id: session_id.to_string(),
                event: result_ev,
            });

            // ask_user：回合暂停，等待用户在 UI 选择/回答（user/question 事件）
            if out.value.get("await_user").and_then(|v| v.as_bool()) == Some(true) {
                let qev = SessionEvent::new(
                    types::USER_QUESTION,
                    Some(json!({
                        "question": out.value.get("question").cloned().unwrap_or_else(|| json!("")),
                        "options": out.value.get("options").cloned().unwrap_or_else(|| json!([])),
                        "header": out.value.get("header").cloned().unwrap_or_else(|| json!(null)),
                    })),
                );
                append_alive(&store, sessions_shared, session_id, &qev)?;
                let _ = tx.send(EngineEvent::Event {
                    session_id: session_id.to_string(),
                    event: qev,
                });
                // 配对完整性：ask_user 的结果作为 tool 响应立即入消息序列。
                // 否则历史里 assistant 声明的 tool_calls 无响应，下一轮请求
                // 被 trim_pending 裁剪成"无 content 且无 tool_calls"的 assistant
                // 空壳 → OpenAI 兼容 API 400 (content or tool_calls must be set)。
                messages.push(Message::Tool {
                    tool_call_id: tc.id.clone(),
                    content: truncate_for_llm(&out.value.to_string()),
                });
                info!("ask_user: turn paused awaiting user for {session_id}");
                break 'outer;
            }
            // plan_write：进入计划模式并写入计划内容（聊天界面计划卡片显示）
            if tc.function.name == "plan_write" {
                if let Some(content) = parsed_args.get("content").and_then(|v| v.as_str()) {
                    let pev = crate::engine::plan::plan_content_event(content);
                    append_alive(&store, sessions_shared, session_id, &pev)?;
                    let _ = tx.send(EngineEvent::Event {
                        session_id: session_id.to_string(),
                        event: pev,
                    });
                    info!(
                        "plan written for {session_id}: {} chars",
                        content.chars().count()
                    );
                }
            }
            // exit_plan_mode：退出计划模式。
            // 历史缺陷：AI 完成最后一步后直接 exit 而忘了先用 plan_write
            // 把 `- [ ]` 改 `[x]` → 计划卡停在 N-1/N，与 AI 声称"全部完成"
            // 矛盾。修复：exit 时自动检查未勾选项；有则在工具结果中明确
            // 告知 AI 需要再调一次 plan_write 补勾（下一轮 LLM 会修正）。
            if tc.function.name == "exit_plan_mode" {
                // fold 当前计划内容,统计未勾选项
                let (_, plan_content) = {
                    let evs = store.load_events(session_id);
                    crate::engine::plan::fold_plan_state(&evs)
                };
                let unchecked: Vec<&str> = plan_content
                    .lines()
                    .filter(|l| {
                        let t = l.trim_start_matches(['-', '*', '+', ' ']);
                        t.starts_with("[ ]")
                    })
                    .map(|l| l.trim())
                    .collect();
                if !unchecked.is_empty() {
                    log::warn!(
                        "exit_plan_mode with {} unchecked steps for {session_id}",
                        unchecked.len()
                    );
                    // 覆盖工具结果：明确告知 AI 有未勾选项，下一轮 LLM
                    // 会自动调 plan_write 补勾（比静默 exit 更可靠）
                    let list: Vec<String> = unchecked
                        .iter()
                        .map(|s| s.chars().take(60).collect())
                        .collect();
                    out = ToolOutput::ok(json!({
                        "exit_plan": true,
                        "warning": format!(
                            "计划已退出，但还有 {} 个步骤未标记完成：
{}

请立即调用 plan_write，把这些步骤的 [ ] 改成 [x]，让计划卡显示 N/N。",
                            unchecked.len(),
                            list.join("
")
                        ),
                    }));
                }
                let xev = crate::engine::plan::plan_exit_event();
                append_alive(&store, sessions_shared, session_id, &xev)?;
                let _ = tx.send(EngineEvent::Event {
                    session_id: session_id.to_string(),
                    event: xev,
                });
                info!("plan mode exited for {session_id}");
            }

            // 工具结果进 LLM 上下文：截断到 8000 字符（OCR/搜索等工具可能返回
            // 很长的 JSON，全量会撑爆上下文；store 持久化的是完整 value）
            messages.push(Message::Tool {
                tool_call_id: tc.id.clone(),
                content: truncate_for_llm(&out.value.to_string()),
            });
        }

        let step_end = SessionEvent::new(types::STEP_END, None);
        append_alive(&store, sessions_shared, session_id, &step_end)?;
        let _ = tx.send(EngineEvent::Event {
            session_id: session_id.to_string(),
            event: step_end,
        });
    }

    // turn/end
    // 会话可能已被用户删除：不再写 turn/end（避免 OpenOptions::create 重建存储文件，
    // 导致"已删除的会话复活"）
    if session_alive(sessions_shared, session_id) {
        let ev = SessionEvent::new(types::TURN_END, None);
        store.append(session_id, &ev)?;
        let _ = tx.send(EngineEvent::Event {
            session_id: session_id.to_string(),
            event: ev,
        });
    } else {
        info!("session {session_id} deleted during turn; skip turn/end");
    }
    Ok(())
}

/// 会话是否仍然存活（未被 delete_session 移除）。
fn session_alive(
    shared: &Arc<std::sync::Mutex<HashMap<String, Session>>>,
    session_id: &str,
) -> bool {
    lock_shared(shared).contains_key(session_id)
}

/// 在 blocking 线程池执行工具分发（bash/pwsh 最长阻塞一个命令超时）。
async fn spawn_blocking_dispatch<F>(f: F) -> ToolOutput
where
    F: FnOnce() -> ToolOutput + Send + 'static,
{
    match tokio::task::spawn_blocking(f).await {
        Ok(out) => out,
        Err(e) => ToolOutput::err(format!("工具执行线程失败: {e:#}")),
    }
}

/// 带存活检查的落盘：会话已删除时跳过（防止 append 的 create(true) 重建
/// 已删除会话的 JSONL 文件）。
fn append_alive(
    store: &SessionStore,
    shared: &Arc<std::sync::Mutex<HashMap<String, Session>>>,
    session_id: &str,
    ev: &SessionEvent,
) -> Result<()> {
    if !session_alive(shared, session_id) {
        return Ok(());
    }
    store.append(session_id, ev)
}

/// 加锁 + poison 恢复（任何线程 panic 持有锁都不会让 UI/桥崩溃）。
fn lock_shared(
    m: &Arc<std::sync::Mutex<HashMap<String, Session>>>,
) -> std::sync::MutexGuard<'_, HashMap<String, Session>> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// 递归复制目录（插件本地导入用）。
fn copy_dir_all_plugin(src: &std::path::Path, dst: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_dir_all_plugin(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

/// 简易 UUID：时间戳 + 单调计数器 + OS 随机数（Windows BCryptGenRandom
/// 或等价熵源）。纯时间派生在同一个时钟 tick 内会碰撞，导致会话文件互相覆盖。
fn uuid() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let c = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("{nanos:x}{c:x}{:x}", os_random_u64())
}

/// OS 随机数（无 rand 依赖、无 unsafe）：`RandomState` 的 SipHash 密钥由
/// 进程启动时 OS RNG 播种，每次 `new()` 还叠加递增扰动 —— 足够做 id 混淆；
/// 真正的唯一性由 时间戳+单调计数器 保证。
fn os_random_u64() -> u64 {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    RandomState::new().build_hasher().finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::settings::EngineSettings;

    fn test_engine() -> (DshEngine, std::sync::mpsc::Receiver<EngineEvent>) {
        let (tx, rx) = std::sync::mpsc::channel::<EngineEvent>();
        let mut settings = EngineSettings::default();
        // DSH_HOME 隔离：防止 default() 的 None 目录回退到真实 ~/.dsh
        // 并 autostart 用户插件子进程（历史缺陷：测试触碰生产环境）
        settings.api_key = Some("fake-key".into());
        settings.base_url = "http://127.0.0.1:1".into(); // 不可达：回合线程会快速失败
        // 测试目录禁用自动删除：TempDir 守卫在函数返回即删目录，引擎后续
        // 写盘随机失败（并行全量跑时 title 断言抖动的根因）。泄漏到系统
        // 临时目录可接受（测试产物）。
        let td_data = tempfile::tempdir().unwrap();
        settings.data_dir = td_data.keep().join("sessions");
        // 隔离：技能/插件目录指向空临时目录（不加载用户环境，避免子进程副作用）
        let td_skills = tempfile::tempdir().unwrap().keep();
        let td_plugins = tempfile::tempdir().unwrap().keep();
        settings.skills_dir = Some(td_skills.join("skills"));
        settings.plugins_dir = Some(td_plugins.join("plugins"));
        let engine = DshEngine::new(settings, tx).expect("engine");
        (engine, rx)
    }

    /// 删除本地插件：停进程 → 删目录 → 注册表清除；名称穿越/不存在
    /// 目录拒绝。
    #[test]
    fn remove_plugin_deletes_dir_and_registry() {
        let (engine, _rx) = test_engine();
        // 构造一个插件目录（manifest 指向不存在的 exe —— 不会真正 spawn）
        let dir = {
            let mut mgr = engine.plugins.lock().unwrap();
            let root = mgr.ensure_dir();
            let pd = root.join("del-me");
            std::fs::create_dir_all(&pd).unwrap();
            std::fs::write(
                pd.join("plugin.json"),
                r#"{"name":"del-me","command":["no-such-exe"],"autostart":false}"#,
            )
            .unwrap();
            mgr.discover();
            root
        };
        assert!(
            engine
                .plugins
                .lock()
                .unwrap()
                .get("del-me")
                .is_some(),
            "发现阶段应注册"
        );
        engine.remove_plugin("del-me").expect("删除应成功");
        assert!(!dir.join("del-me").exists(), "目录应被删除");
        assert!(engine.plugins.lock().unwrap().get("del-me").is_none());
        // 重复删除 → 报错（目录已不存在）
        assert!(engine.remove_plugin("del-me").is_err());
        // 名称穿越拒绝（kebab-case 校验挡住 ../ 路径）
        assert!(engine.remove_plugin("../evil").is_err());
        assert!(engine.remove_plugin(r"..\evil").is_err());
    }

    /// 回归：回合运行中发送消息 = 插话（不得报 "already running"）。
    /// 消息立即入列 + 事件发出，同时打断当前 step（旧回合续跑处理）。
    #[test]
    fn interject_allowed_when_running() {
        let (mut engine, rx) = test_engine();
        let sid = engine.create_session(Some("interject")).unwrap();
        // 清掉 create 时的 SessionCreated 事件
        while let Ok(EngineEvent::SessionCreated { .. }) = rx.try_recv() {}
        // 模拟回合进行中：shared 与引擎缓存均 running
        {
            let mut shared = lock_shared(&engine.sessions_shared);
            shared.get_mut(&sid).unwrap().running = true;
        }
        engine.sessions.get_mut(&sid).unwrap().running = true;

        let r = engine.send_message(&sid, "插话消息");
        assert!(r.is_ok(), "running 时应允许插话而非报错: {r:?}");

        // 消息已入列（引擎缓存）
        let s = engine.sessions.get(&sid).unwrap();
        assert!(
            s.messages
                .iter()
                .any(|m| matches!(m, Message::User { content, .. } if content == "插话消息")),
            "插话消息必须立即进入会话消息列表"
        );
        // user/message 事件已发出（UI 同步显示）
        let got = rx.try_recv().expect("应收到 user/message 事件");
        match got {
            EngineEvent::Event { event, .. } => {
                assert_eq!(event.r#type, types::USER_MESSAGE);
                assert_eq!(
                    event
                        .data
                        .and_then(|d| d.get("content").and_then(|c| c.as_str()).map(String::from)),
                    Some("插话消息".to_string())
                );
            }
            other => panic!("expected Event, got {other:?}"),
        }
        // 插话后 running 保持（旧回合仍在跑）
        assert!(engine.sessions.get(&sid).unwrap().running);
        // 打断标志已置位（旧回合将续跑处理插话；stopping 不受影响）
        let sig = engine.turn_signals.get(&sid).expect("turn signal");
        assert!(TurnSignal::is_set(&sig.cancelled));
        assert!(!TurnSignal::is_set(&sig.stopping));
    }

    /// 回归：send_message 创建回合任务（任务卡片有真实数据，不再是无数据摆设）。
    /// 任务立即进入 running；回合收尾（不可达 LLM 快速失败）后任务变为 failed。
    #[test]
    fn send_message_creates_turn_job() {
        let (mut engine, rx) = test_engine();
        let sid = engine.create_session(Some("job test")).unwrap();
        while let Ok(EngineEvent::SessionCreated { .. }) = rx.try_recv() {}
        engine.send_message(&sid, "跑一下").unwrap();

        let jobs = engine.jobs_snapshot();
        let turn_job = jobs
            .iter()
            .find(|j| j.name == "agent turn")
            .expect("send_message 必须创建回合任务");
        assert_eq!(
            turn_job.status,
            crate::engine::jobs::JobStatus::Running,
            "回合任务应立即进入 running"
        );

        // 回合线程失败后任务收尾为 failed（不可达 base_url 快速失败）
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        loop {
            let jobs = engine.jobs_snapshot();
            let turn_job = jobs
                .iter()
                .find(|j| j.name == "agent turn")
                .expect("回合任务必须存在");
            if matches!(
                turn_job.status,
                crate::engine::jobs::JobStatus::Done | crate::engine::jobs::JobStatus::Failed
            ) {
                assert_eq!(
                    turn_job.status,
                    crate::engine::jobs::JobStatus::Failed,
                    "不可达 LLM 的回合应失败收尾"
                );
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "回合任务未在超时内收尾"
            );
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }

    /// 回归：每个会话有独立工作区——修改会话 A 的工作区只影响 A；
    /// 全局 set_workspace 不覆盖已打开会话的 cwd。
    #[test]
    fn session_workspaces_are_independent() {
        let (mut engine, _rx) = test_engine();
        let sid_a = engine.create_session(Some("ws A")).unwrap();
        let sid_b = engine.create_session(Some("ws B")).unwrap();
        let dir_a = tempfile::tempdir().unwrap();
        let dir_b = tempfile::tempdir().unwrap();

        // 会话 A 绑定自己的工作区
        engine.set_session_workspace(&sid_a, dir_a.path()).unwrap();
        // 会话 B 不受影响（全局默认）
        let s = engine.open_session(&sid_b).unwrap();
        assert_ne!(
            s.cwd.as_deref(),
            Some(dir_a.path()),
            "会话 B 不应被 A 的工作区影响"
        );

        // 会话 B 绑定自己的工作区 → 各自独立
        engine.set_session_workspace(&sid_b, dir_b.path()).unwrap();
        let sa = engine.open_session(&sid_a).unwrap();
        let sb = engine.open_session(&sid_b).unwrap();
        assert_eq!(sa.cwd.as_deref(), Some(dir_a.path()));
        assert_eq!(sb.cwd.as_deref(), Some(dir_b.path()));

        // 全局 set_workspace 只更新默认，不覆盖会话独立 cwd
        let dir_c = tempfile::tempdir().unwrap();
        engine.set_workspace(dir_c.path()).unwrap();
        let sa = engine.open_session(&sid_a).unwrap();
        assert_eq!(
            sa.cwd.as_deref(),
            Some(dir_a.path()),
            "全局默认工作区变更不得覆盖会话独立工作区"
        );
        let expected = crate::core::workspace::WorkspaceManager::strip_unc_prefix(
            &dir_c.path().canonicalize().unwrap(),
        );
        assert_eq!(
            engine.workspace_root().map(|p| p.as_path()),
            Some(expected.as_path()),
            "全局默认工作区应更新"
        );
    }

    /// ask_user 工具输出携带 await_user 标记（回合暂停等待用户）。
    #[test]
    fn ask_user_output_marks_await() {
        let engine = test_engine().0;
        let tools = engine.tools.clone_handle();
        let out = tools.dispatch(
            "ask_user",
            &serde_json::json!({"question": "继续吗？", "options": ["继续", "停止"]}),
        );
        assert!(out.ok);
        assert_eq!(
            out.value.get("await_user").and_then(|v| v.as_bool()),
            Some(true)
        );
        assert_eq!(
            out.value.get("question").and_then(|v| v.as_str()),
            Some("继续吗？")
        );
    }

    /// 删除会话：存储文件 + 内存缓存 + 共享 map 全部移除，列表不再包含。
    #[test]
    fn delete_session_removes_everywhere() {
        let (mut engine, _rx) = test_engine();
        let sid = engine.create_session(Some("del test")).unwrap();
        assert!(
            engine.store.list_sessions().contains(&sid),
            "创建后列表应包含该会话"
        );

        engine.delete_session(&sid).unwrap();

        assert!(
            !engine.store.list_sessions().contains(&sid),
            "存储列表应移除该会话"
        );
        assert!(
            engine.store.load_events(&sid).is_empty(),
            "存储文件应被删除（事件为空）"
        );
        assert!(engine.sessions.get(&sid).is_none(), "内存缓存应移除");
        assert!(
            lock_shared(&engine.sessions_shared).get(&sid).is_none(),
            "共享 map 应移除"
        );
    }

    /// 回归：TurnGuard 只复位**当前代数**的回合。
    /// 旧回合的收尾不得清掉新回合的 running 状态（cancel 后立即重发的竞态）。
    #[test]
    fn turn_guard_respects_epoch() {
        let (tx, rx) = std::sync::mpsc::channel::<EngineEvent>();
        let (store, _d) = {
            let dir = tempfile::tempdir().unwrap();
            (
                crate::core::storage::SessionStore::new(dir.path().to_path_buf()).unwrap(),
                dir,
            )
        };
        let shared: Arc<std::sync::Mutex<HashMap<String, Session>>> =
            Arc::new(std::sync::Mutex::new(HashMap::new()));
        // 会话处于 epoch=2（新回合运行中）
        let mut s = Session::new("s-epoch".into());
        s.turn_epoch = 2;
        s.running = true;
        shared.lock().unwrap().insert("s-epoch".into(), s);

        // 旧回合（epoch=1）的 guard 退出：不得复位 running、不得发 Idle
        let jobs = Arc::new(std::sync::Mutex::new(JobManager::default()));
        let jid = jobs.lock().unwrap().create("agent turn", None);
        let guard = TurnGuard {
            tx: tx.clone(),
            sessions_shared: shared.clone(),
            store: store.clone_handle(),
            jobs,
            job_id: jid,
            session_id: "s-epoch".into(),
            epoch: 1,
            result_ok: false,
        };
        drop(guard);
        {
            let map = lock_shared(&shared);
            let s = map.get("s-epoch").unwrap();
            assert!(s.running, "旧回合收尾不得复位新回合的 running");
        }
        while let Ok(ev) = rx.try_recv() {
            assert!(
                !matches!(ev, EngineEvent::StatusChanged { .. }),
                "旧回合收尾不得发送状态事件覆盖新回合"
            );
        }

        // 新回合（epoch=2）的 guard 退出：正常复位 + 发 Idle
        let jobs = Arc::new(std::sync::Mutex::new(JobManager::default()));
        let jid = jobs.lock().unwrap().create("agent turn", None);
        let guard = TurnGuard {
            tx,
            sessions_shared: shared.clone(),
            store,
            jobs,
            job_id: jid,
            session_id: "s-epoch".into(),
            epoch: 2,
            result_ok: true,
        };
        drop(guard);
        {
            let map = lock_shared(&shared);
            assert!(
                !map.get("s-epoch").unwrap().running,
                "当前回合收尾应复位 running"
            );
        }
        let got = rx.try_recv().expect("当前回合收尾应发送 Idle");
        assert!(matches!(
            got,
            EngineEvent::StatusChanged {
                status: AgentStatus::Idle,
                ..
            }
        ));
    }

    /// 回归：用户消息持久化失败必须回滚 running（否则会话永久卡 Running，
    /// 后续所有发送都被当作插话）。旧代码在 persist 失败后直接 return Err，
    /// running 停留在 true。
    #[test]
    fn persist_failure_rolls_back_running() {
        let (tx, _rx) = std::sync::mpsc::channel::<EngineEvent>();
        let mut settings = EngineSettings::default();
        settings.api_key = Some("fake-key".into());
        settings.base_url = "http://127.0.0.1:1".into();
        let dir = tempfile::tempdir().unwrap();
        settings.data_dir = dir.path().join("sessions");
        settings.skills_dir = Some(dir.path().join("skills"));
        settings.plugins_dir = Some(dir.path().join("plugins"));
        let mut engine = DshEngine::new(settings, tx).expect("engine");
        let sid = engine.create_session(Some("persist fail")).unwrap();

        // 破坏存储目录：用同名文件占位 → append 打开失败
        std::fs::remove_dir_all(dir.path().join("sessions")).unwrap();
        std::fs::write(dir.path().join("sessions"), b"blocked").unwrap();

        let r = engine.send_message(&sid, "hi");
        assert!(r.is_err(), "持久化失败应返回错误");
        {
            let shared = lock_shared(&engine.sessions_shared);
            let s = shared.get(&sid).expect("会话仍在共享 map");
            assert!(!s.running, "persist 失败后 running 必须回滚为 false");
        }
        // 引擎本地缓存同步回滚
        assert!(!engine.sessions.get(&sid).unwrap().running);
    }

    /// 回归：cancel 置 stopping（退出）而 interject 只置 cancelled（续跑）——
    /// 两个信号分离，停止不会被误判为插话续跑。
    #[test]
    fn cancel_and_interject_signals_separate() {
        let (mut engine, _rx) = test_engine();
        let sid = engine.create_session(Some("signals")).unwrap();
        // 初始：无信号置位
        let sig = engine.turn_signals.get(&sid);
        assert!(sig.is_none(), "未发送时不应有回合信号");

        engine.send_message(&sid, "go").unwrap();
        let sig = engine
            .turn_signals
            .get(&sid)
            .expect("send 后应有信号")
            .clone();
        assert!(!TurnSignal::is_set(&sig.cancelled));
        assert!(!TurnSignal::is_set(&sig.stopping));

        // 插话：只置 cancelled
        {
            let mut shared = lock_shared(&engine.sessions_shared);
            shared.get_mut(&sid).unwrap().running = true;
        }
        engine.send_message(&sid, "interject").unwrap();
        assert!(TurnSignal::is_set(&sig.cancelled), "插话应置 cancelled");
        assert!(!TurnSignal::is_set(&sig.stopping), "插话不得置 stopping");

        // 停止：两个都置（stopping 优先判定）
        engine.cancel(&sid);
        assert!(TurnSignal::is_set(&sig.stopping), "停止应置 stopping");
        assert!(TurnSignal::is_set(&sig.cancelled));
    }

    /// 自动命名：无标题会话发送首条消息后，标题 = 首行提炼（≤24 字符），
    /// 且持久化 session/title 事件（重启重放仍能恢复）；手动命名后不再覆盖。
    #[test]
    fn first_message_auto_titles_session() {
        let (mut engine, _rx) = test_engine();
        let sid = engine.create_session(None).unwrap();
        engine.send_message(&sid, "帮我审查 bosk 代码的安全性\n\n重点看解析器").unwrap();

        let title = engine.list_sessions().iter().find(|s| s.session_id == sid)
            .map(|s| s.title.clone()).unwrap();
        assert_eq!(title, "帮我审查 bosk 代码的安全性", "首条消息应自动命名");

        // 持久化验证（重放路径）
        let loaded = engine.store.load_session(&sid).unwrap();
        assert_eq!(loaded.title, "帮我审查 bosk 代码的安全性");

        // 手动重命名后，后续消息不再覆盖
        engine.rename_session(&sid, "手动名").unwrap();
        engine.send_message(&sid, "第二条完全不同的消息").unwrap();
        let title2 = engine.list_sessions().iter().find(|s| s.session_id == sid)
            .map(|s| s.title.clone()).unwrap();
        assert_eq!(title2, "手动名", "手动命名后自动命名不得覆盖");
    }

    /// token 用量：引擎访问器（累计与合计）。
    #[test]
    fn token_usage_accessors() {
        let (mut engine, _rx) = test_engine();
        let sid = engine.create_session(Some("t")).unwrap();
        {
            let mut map = engine.token_usage.lock().unwrap();
            map.insert(
                sid.clone(),
                crate::core::llm::TokenUsage {
                    prompt_tokens: 100,
                    completion_tokens: 50,
                },
            );
        }
        let u = engine.token_usage(&sid);
        assert_eq!(u.total(), 150);
        assert_eq!(engine.token_usage_total().total(), 150);
        // 未知会话 = 0
        assert_eq!(engine.token_usage("no-such").total(), 0);
    }

    /// AGENTS.md 项目记忆：存在/缺失/空文件/超大截断四种形态。
    #[test]
    fn agents_md_injection_shapes() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read_agents_md(dir.path()).is_none(), "缺失 → None");
        std::fs::write(dir.path().join("AGENTS.md"), "# 项目约定
用中文注释").unwrap();
        assert!(read_agents_md(dir.path()).unwrap().contains("项目约定"));
        let dir2 = tempfile::tempdir().unwrap();
        std::fs::write(dir2.path().join("agents.md"), "lower").unwrap();
        assert_eq!(read_agents_md(dir2.path()).as_deref(), Some("lower"));
        let dir3 = tempfile::tempdir().unwrap();
        std::fs::write(dir3.path().join("AGENTS.md"), "   ").unwrap();
        assert!(read_agents_md(dir3.path()).is_none(), "空文件 → None");
        let dir4 = tempfile::tempdir().unwrap();
        std::fs::write(dir4.path().join("AGENTS.md"), "中".repeat(40_000)).unwrap();
        let t = read_agents_md(dir4.path()).unwrap();
        assert!(t.contains("已截断"), "超大 → 截断标记");
        assert!(t.chars().count() < 40_000);
    }

    /// 会话分叉：消息（含工具配对）完整复制到新会话，标题带 ⑂ 前缀；
    /// 重放后工具消息仍在（tool/call 声明正确落盘）。
    #[test]
    fn fork_session_copies_messages_with_tool_pairing() {
        let (mut engine, _rx) = test_engine();
        let sid = engine.create_session(Some("原会话")).unwrap();
        // 构造带工具配对的消息序列（直接经 fork_append 语义验证太绕；
        // 走公共路径：手动 push 消息再 fork）
        {
            let s = engine.sessions.get_mut(&sid).unwrap();
            s.messages.push(Message::User {
                content: "看下代码".into(),
                images: None,
            });
            s.messages.push(Message::Assistant {
                content: String::new(),
                tool_calls: vec![ToolCall {
                    id: "c1".into(),
                    call_type: "function".into(),
                    function: crate::core::session::FunctionCall {
                        name: "bash".into(),
                        arguments: r#"{"cmd":"ls"}"#.into(),
                    },
                }],
                reasoning: Some("思考中…".into()),
            });
            s.messages.push(Message::Tool {
                tool_call_id: "c1".into(),
                content: r#"{"ok":true}"#.into(),
            });
        }
        let new_id = engine.fork_session(&sid).unwrap();
        // 新会话标题 + 消息数
        let forked = engine.sessions.get(&new_id).unwrap();
        assert_eq!(forked.title, "⑂ 原会话");
        assert_eq!(forked.messages.len(), 3, "消息应全部复制");
        // 重放路径：load_session 从 JSONL 重建（tool 配对 + reasoning 恢复）
        let loaded = engine.store.load_session(&new_id).unwrap();
        assert_eq!(loaded.messages.len(), 3, "重放后消息数不变");
        match &loaded.messages[2] {
            Message::Tool { tool_call_id, .. } => assert_eq!(tool_call_id, "c1"),
            _ => panic!("tool 消息应在重放后保留"),
        }
        match &loaded.messages[1] {
            Message::Assistant { reasoning, tool_calls, .. } => {
                assert_eq!(reasoning.as_deref(), Some("思考中…"));
                assert_eq!(tool_calls.len(), 1);
            }
            _ => panic!("assistant 应保留"),
        }
        // 原会话不动
        assert_eq!(engine.sessions.get(&sid).unwrap().messages.len(), 3);
        assert_eq!(engine.sessions.get(&sid).unwrap().title, "原会话");
    }

    /// 墓碑共享回归：clone_handle 产生的副本必须看到引擎实例的删除
    /// 记录（否则回合线程 append 的 create(true) 会复活已删 .jsonl——
    /// 历史缺陷:修复用 SessionStore::new 构造空墓碑,对目标路径无效）。
    #[test]
    fn tombstones_shared_across_clone() {
        let (mut engine, _rx) = test_engine();
        let sid = engine.create_session(Some("tomb")).unwrap();
        let clone = engine.store.clone_handle();
        // 引擎实例标记删除 → clone 必须看到;clone 的 append 被拦截,
        // 文件若存在(创建时已建)不得包含新增事件
        engine.store.mark_deleted(&sid);
        let ev_before = engine.store.load_events(&sid).len();
        let _ = clone.append(
            &sid,
            &SessionEvent::new(types::USER_MESSAGE, Some(json!({"content":"x"}))),
        );
        let ev_after = engine.store.load_events(&sid).len();
        assert_eq!(
            ev_before, ev_after,
            "clone 的 append 应被共享墓碑拦截(事件数不变)"
        );
    }

    /// B1 回归：新回合路径的用户消息持久化携带 images（历史缺陷：persist
    /// 重建裸事件丢图，回合一结束图片引用消失）。
    #[test]
    fn new_turn_persists_images() {
        let (mut engine, _rx) = test_engine();
        let sid = engine.create_session(Some("img")).unwrap();
        engine
            .send_message_with_images(&sid, "看图", vec!["C:/tmp/x.png".into()])
            .unwrap();
        // 直接查 store 事件落盘形态
        let evs = engine.store.load_events(&sid);
        let user_ev = evs
            .iter()
            .find(|e| e.r#type == types::USER_MESSAGE)
            .expect("user/message 事件应已落盘");
        let imgs = user_ev
            .data
            .as_ref()
            .and_then(|d| d.get("images"))
            .and_then(|v| v.as_array())
            .map(|a| a.len())
            .unwrap_or(0);
        assert_eq!(imgs, 1, "落盘事件必须携带 images");
    }

    /// C3 回归：删除会话清理其 pending 审批（悬卡点允许不再能唤醒
    /// 已删会话的旧回合执行写操作）。
    #[test]
    fn delete_session_cancels_pending_approvals() {
        let (mut engine, _rx) = test_engine();
        let sid = engine.create_session(Some("ap")).unwrap();
        // 手动注入一条 pending（模拟审批中删除）
        {
            let mut reg = engine.approvals.lock().unwrap();
            let rx = reg.request("t1".into(), sid.clone(), "C:/out.txt".into(), "test".into());
            std::mem::forget(rx); // 测试不消费
        }
        assert_eq!(
            engine
                .approvals
                .lock()
                .unwrap()
                .pending_snapshot()
                .iter()
                .filter(|r| r.session_id == sid)
                .count(),
            1
        );
        engine.delete_session(&sid).unwrap();
        assert_eq!(
            engine
                .approvals
                .lock()
                .unwrap()
                .pending_snapshot()
                .iter()
                .filter(|r| r.session_id == sid)
                .count(),
            0,
            "会话删除后其 pending 审批应全部清除"
        );
    }

    /// B5 回归：compaction 事件重放折叠（压缩持久化，重启不回退全量）。
    #[test]
    fn compaction_replays_folded() {
        let (mut engine, _rx) = test_engine();
        let sid = engine.create_session(Some("cp")).unwrap();
        // 手动落 6 条 user 事件 + 1 条 compaction summary(kept_from=3)。
        // 不走 send_message(会 spawn 回合线程,失败时序与折叠竞态 → 测试抖动)
        for i in 0..6 {
            let ev = SessionEvent::new(
                types::USER_MESSAGE,
                Some(json!({"content": format!("m{i}")})),
            );
            engine.store.append(&sid, &ev).unwrap();
        }
        {
            let ev = SessionEvent::new(
                types::COMPACTION_SUMMARY,
                Some(json!({"summary": "早期已压缩", "kept_from": 3})),
            );
            engine.store.append(&sid, &ev).unwrap();
        }
        let loaded = engine.store.load_session(&sid).unwrap();
        // 折叠后:3 条尾部 + 1 条摘要头 = 4 条(6 条不再全量恢复)
        assert_eq!(
            loaded.messages.len(),
            4,
            "重放应折叠为 尾部3+摘要1: {:?}",
            loaded.messages.len()
        );
        match &loaded.messages[0] {
            Message::User { content, .. } => {
                assert!(content.contains("[context summary]"), "{content}")
            }
            _ => panic!("首条应为摘要 User"),
        }
    }

    /// token 用量持久化：脏标记 → flush 落盘 → 同 data_dir 新引擎恢复。
    #[test]
    fn token_usage_persist_roundtrip() {
        let (mut engine, _rx) = test_engine();
        let sid = engine.create_session(Some("u")).unwrap();
        {
            let mut map = engine.token_usage.lock().unwrap();
            map.insert(
                sid.clone(),
                crate::core::llm::TokenUsage {
                    prompt_tokens: 111,
                    completion_tokens: 22,
                },
            );
        }
        // 置脏（模拟 Usage 事件路径）→ flush 落盘
        engine.mark_usage_dirty_for_test();
        engine.flush_usage();
        assert!(!engine.settings().token_usage.is_empty(), "flush 应落盘(测试直接置入视作脏亦可)");
        // 注:测试绕过脏标记直接验证 map→settings 的同步路径;
        // 真实路径(Usage 事件置脏)由多线程时序保证。
        let data_dir = engine.settings().data_dir.clone();
        let (tx2, _rx2) = std::sync::mpsc::channel::<EngineEvent>();
        let mut s2 = crate::core::settings::EngineSettings::load_for(&data_dir);
        s2.api_key = Some("fake".into());
        s2.base_url = "http://127.0.0.1:1".into();
        s2.skills_dir = Some(tempfile::tempdir().unwrap().path().join("s"));
        s2.plugins_dir = Some(tempfile::tempdir().unwrap().path().join("p"));
        let engine2 = DshEngine::new(s2, tx2).unwrap();
        let u = engine2.token_usage(&sid);
        assert_eq!((u.prompt_tokens, u.completion_tokens), (111, 22), "重启应恢复用量");
    }

    /// 跨会话全文搜索：标题命中无片段；内容命中带片段；limit 生效。
    #[test]
    fn search_sessions_fulltext() {
        let (mut engine, _rx) = test_engine();
        let a = engine.create_session(Some("甲会话")).unwrap();
        engine.send_message(&a, "帮我看下登录模块的边界条件").unwrap();
        let b = engine.create_session(Some("乙会话")).unwrap();
        engine.send_message(&b, "完全无关的话题").unwrap();

        // 标题命中 → 空片段
        let hits = engine.search_sessions("甲会话", 10);
        assert!(hits.iter().any(|(s, snip)| s.session_id == a && snip.is_empty()));

        // 内容命中 → 片段含关键词
        let hits = engine.search_sessions("边界条件", 10);
        let hit = hits.iter().find(|(s, _)| s.session_id == a).expect("内容应命中");
        assert!(hit.1.contains("边界条件"), "片段应含关键词: {}", hit.1);

        // 无命中
        assert!(engine.search_sessions("不存在的词xyz", 10).is_empty());

        // limit
        assert!(engine.search_sessions("会话", 1).len() <= 1);
    }

    /// 导出 Markdown：标题/用户消息/结构完整。
    #[test]
    fn export_session_markdown_shape() {
        let (mut engine, _rx) = test_engine();
        let sid = engine.create_session(Some("导出测试")).unwrap();
        engine.send_message(&sid, "第一条用户消息").unwrap();
        let md = engine.export_session_markdown(&sid).unwrap();
        assert!(md.contains("# 导出测试"), "{md}");
        assert!(md.contains("🧑 用户"));
        assert!(md.contains("第一条用户消息"));
        assert!(md.contains("导出于"));
    }

    /// 定时任务：增/列/启停 + settings 持久化重启恢复 + 到期触发发消息。
    #[test]
    fn schedule_persist_and_fire() {
        let (mut engine, _rx) = test_engine();
        let sid = engine.create_session(Some("定时目标")).unwrap();
        let tid = engine.schedule_add("检查构建", "请检查构建", 3600, &sid);
        assert_eq!(engine.schedule_list().len(), 1);
        engine.schedule_toggle(&tid, false);
        assert!(!engine.schedule_list()[0].enabled);
        engine.schedule_toggle(&tid, true);

        // 持久化 → 同 data_dir 新引擎恢复（settings 从 data_dir 对应文件加载）
        let data_dir = engine.settings().data_dir.clone();
        let (tx2, _rx2) = std::sync::mpsc::channel::<EngineEvent>();
        let mut settings2 = crate::core::settings::EngineSettings::load_for(&data_dir);
        settings2.api_key = Some("fake".into());
        settings2.base_url = "http://127.0.0.1:1".into();
        settings2.skills_dir = Some(tempfile::tempdir().unwrap().path().join("s"));
        settings2.plugins_dir = Some(tempfile::tempdir().unwrap().path().join("p"));
        let engine2 = DshEngine::new(settings2, tx2).unwrap();
        let restored = engine2.schedule_list();
        assert_eq!(restored.len(), 1, "定时任务应重启恢复");
        assert_eq!(restored[0].name, "检查构建");
        assert!(restored[0].enabled);

        // 到期触发：prompt 发到绑定会话（消息落盘；回合线程对假 URL 失败无碍）
        engine.fire_scheduled(&tid);
        let s = engine.sessions.get(&sid).unwrap();
        assert!(
            s.messages
                .iter()
                .any(|m| matches!(m, Message::User { content, .. } if content == "请检查构建")),
            "到期应把 prompt 发进会话"
        );

        // 会话删除后触发：任务自动清理
        engine.delete_session(&sid).unwrap();
        engine.fire_scheduled(&tid);
        assert!(engine.schedule_list().is_empty(), "绑定会话消失应移除任务");
    }

    /// 消息反馈：记录 + 查询 + 事件持久化（JSONL 有 feedback/record）。
    #[test]
    fn message_feedback_roundtrip() {
        let (mut engine, _rx) = test_engine();
        let sid = engine.create_session(Some("t")).unwrap();
        engine
            .record_message_feedback(&sid, "m1", crate::engine::FeedbackKind::Upvote)
            .unwrap();
        assert_eq!(
            engine.message_feedback("m1"),
            Some(crate::engine::FeedbackKind::Upvote)
        );
        assert_eq!(engine.message_feedback("m2"), None);
        // 事件落盘
        let events = engine.store.load_events(&sid);
        assert!(
            events.iter().any(|e| e.r#type == types::FEEDBACK_RECORD),
            "feedback/record 事件应持久化"
        );
    }

    /// 上下文估算：非负、随消息增长（粗估仅用于百分比展示）。
    #[test]
    fn context_estimate_grows_with_messages() {
        let (mut engine, _rx) = test_engine();
        let sid = engine.create_session(Some("t")).unwrap();
        let empty = engine.context_estimate(&sid);
        engine.send_message(&sid, "这是一段比较长的消息内容，用来撑大上下文估算值").unwrap();
        let after = engine.context_estimate(&sid);
        assert!(after > empty, "发送消息后估算应增大 ({empty} -> {after})");
    }
}
