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
    queued_messages: std::collections::HashMap<String, std::collections::VecDeque<String>>,
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
        })
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
                let due = {
                    let mut s = scheduler.lock().unwrap_or_else(|p| p.into_inner());
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
        let _ = self.store.append(session_id, &ev);
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
        let _ = self.store.append(session_id, &ev);
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
        let _ = self.store.append(session_id, &ev);
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
        let mut session = Session::new(id.clone());
        session.cwd = cwd;
        // 创建时的 cwd 持久化（session/cwd 事件，重启重放恢复独立工作区）
        if let Some(ref c) = session.cwd {
            let cev = SessionEvent::new(
                types::SESSION_CWD,
                Some(json!({"cwd": c.to_string_lossy().into_owned()})),
            );
            session.push_event(cev.clone());
            let _ = self.store.append(&id, &cev);
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
    pub fn open_session(&mut self, session_id: &str) -> Result<Session> {
        // 1) 共享内存（agent 线程结束时会写入完整消息 + running=false，最新）
        //    但 shared 里可能是 create 时的空版本（仅 preset/title 事件、无消息）
        //    —— 跳过，走重放，确保外部/后台写入存储的新消息能被读到
        if let Some(s) = lock_shared(&self.sessions_shared).get(session_id).cloned() {
            if !s.messages.is_empty() || s.running {
                // 共享内存的 events/messages 可能是旧快照（回合中只同步 messages，
                // 不回写 events；而 store 由 agent 线程持续追加）——用 store 的
                // 最新内容补齐，否则打开会话重放会缺 GOAL_CHANGE / PLAN_MODE /
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
                // goal/plan/subagent 事件与新消息）
                if let Ok(loaded) = self.store.load_session(session_id) {
                    if loaded.events.len() > s.events.len() {
                        s.events = loaded.events;
                        s.messages = loaded.messages;
                    }
                }
                return Ok(s);
            }
        }
        // 3) 存储重放
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

    /// UI 驱动的目标操作：goal/change 事件写入会话日志（持久化 + 通知），
    /// 与 goal_create 工具同一事件通道，重放 fold 恢复目标状态。
    pub fn goal_op(
        &mut self,
        session_id: &str,
        op: crate::engine::goal::GoalOp,
        goal_id: &str,
        objective: Option<&str>,
    ) -> Result<()> {
        let ev = crate::engine::goal::goal_change_event(op, goal_id, objective);
        // 会话已删除：不落盘（append 的 create(true) 会重建已删除会话的文件）
        if !self.sessions.contains_key(session_id)
            && !lock_shared(&self.sessions_shared).contains_key(session_id)
        {
            return Err(anyhow::anyhow!("session not found: {session_id}"));
        }
        if let Some(s) = self.sessions.get_mut(session_id) {
            s.push_event(ev.clone());
        }
        {
            let mut shared = lock_shared(&self.sessions_shared);
            if let Some(s) = shared.get_mut(session_id) {
                s.push_event(ev.clone());
            }
        }
        self.store.append(session_id, &ev)?;
        let _ = self.tx.send(EngineEvent::Event {
            session_id: session_id.to_string(),
            event: ev,
        });
        info!("goal op {} on {goal_id} for {session_id}", op.as_str());
        Ok(())
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
    pub fn enqueue_message(&mut self, session_id: &str, content: &str) -> usize {
        let q = self
            .queued_messages
            .entry(session_id.to_string())
            .or_default();
        q.push_back(content.to_string());
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
        let ready: Vec<(String, String)> = {
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
        for (sid, msg) in ready {
            if let Some(q) = self.queued_messages.get_mut(&sid) {
                q.pop_front();
                if q.is_empty() {
                    self.queued_messages.remove(&sid);
                }
            }
            if let Err(e) = self.send_message(&sid, &msg) {
                warn!("queued message dropped ({sid}): {e:#}");
            }
        }
    }

    /// 用户发送消息 → 异步 agent 回合。
    pub fn send_message(&mut self, session_id: &str, content: &str) -> Result<()> {
        log::debug!("send_message called for {session_id}: {content}");
        // 0) 先校验 API key：无 key 时报错且不改任何会话状态
        let llm = self.llm.clone().ok_or_else(|| {
            anyhow::anyhow!("API key 未配置：请在设置页填写 DeepSeek API key 后重试")
        })?;
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
            // 回合运行中 → 插话：消息立即入列并打断当前 step，
            // 旧回合在下一轮自动继续处理插话（run_turn 续跑逻辑）
            log::info!("send_message: session {session_id} running -> interject");
            return self.interject(session_id, content);
        }
        // 3) 新回合信号（整体替换：旧回合持有旧 Arc，新取消/停止不影响旧回合收尾）
        let signal = Arc::new(TurnSignal::default());
        self.turn_signals
            .insert(session_id.to_string(), signal.clone());
        // 先 clone 需要的快照，避免与持久化 borrow 冲突
        let (epoch, snapshot, session_cwd, session_preset, _sess_sb, _sess_model, _sess_effort) = {
            let session = self
                .sessions
                .get_mut(session_id)
                .ok_or_else(|| anyhow::anyhow!("session not open: {session_id}"))?;
            if session.running {
                log::info!("send_message: session {session_id} running (local) -> interject");
                return self.interject(session_id, content);
            }
            session.running = true;
            session.turn_epoch += 1;

            // user/message 事件
            let ev = SessionEvent::new(types::USER_MESSAGE, Some(json!({"content": content})));
            session.push_event(ev.clone());
            session.messages.push(Message::User {
                content: content.to_string(),
            });
            // 同步到共享 map（agent 线程与桥都读它）
            {
                let mut shared = lock_shared(&self.sessions_shared);
                if let Some(s) = shared.get_mut(session_id) {
                    s.running = true;
                    s.turn_epoch = session.turn_epoch;
                    s.messages = session.messages.clone();
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
        // 失败必须回滚 running（否则会话永久卡 Running，后续全部变成插话）
        if let Err(e) = self.persist(
            session_id,
            &SessionEvent::new(types::USER_MESSAGE, Some(json!({"content": content}))),
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
        let tools = {
            // 快照注入技能（load_skill 工具 + persona 技能列表用）
            let mut t = self.tools.clone_handle();
            t.skills = self.skills.list().into_iter().cloned().collect();
            t
        };
        let model = self
            .llm
            .as_ref()
            .map(|l| l.model().to_string())
            .unwrap_or_default();
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
    fn interject(&mut self, session_id: &str, content: &str) -> Result<()> {
        let ev = SessionEvent::new(types::USER_MESSAGE, Some(json!({"content": content})));
        {
            let session = self
                .sessions
                .get_mut(session_id)
                .ok_or_else(|| anyhow::anyhow!("session not open: {session_id}"))?;
            session.push_event(ev.clone());
            session.messages.push(Message::User {
                content: content.to_string(),
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
        self.store.append(session_id, &ev)?;
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
        SessionStore::new(self.dir.clone()).unwrap()
    }
}

/// 权限审批结果。
enum ApprovalOutcome {
    Allowed,
    Denied { note: String },
}

/// 走一轮权限审批：登记请求 → 发 UI 事件 → await 用户决定（超时默认拒绝）。
async fn run_approval(
    tx: &Sender<EngineEvent>,
    approvals: &Arc<std::sync::Mutex<crate::engine::approval::ApprovalRegistry>>,
    target: String,
    reason: String,
    session_id: &str,
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
    let awaited = tokio::time::timeout(std::time::Duration::from_secs(300), rx).await;
    let decision = match awaited {
        Ok(Ok(d)) => d,
        Ok(Err(_)) => {
            approvals.lock().unwrap().cancel(&id);
            return Ok(ApprovalOutcome::Denied {
                note: "审批通道已关闭（未收到用户决定）".into(),
            });
        }
        Err(_) => {
            approvals.lock().unwrap().cancel(&id);
            return Ok(ApprovalOutcome::Denied {
                note: "审批超时（300s 未响应），已拒绝".into(),
            });
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
) -> Result<()> {
    let mut tools = tools;
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
            // 核心层能力增强）；同名不去重会被 API 拒绝（duplicate tool）
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
                Message::User { content } | Message::Assistant { content, .. } => content.len(),
                Message::Tool { content, .. } => content.len(),
            })
            .sum();
        const CHAR_LIMIT: usize = 80_000;
        let should_compact = preset.has_compaction()
            && (messages.len() > COMPACTION_THRESHOLD || est_chars > CHAR_LIMIT);
        if should_compact {
            let plan = crate::engine::compaction::plan_compaction(&messages, COMPACTION_KEEP_TAIL);
            if plan.removed > 0 {
                for ev in [
                    crate::engine::compaction::compaction_start_event(),
                    crate::engine::compaction::compaction_summary_event(&plan.summary),
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
            // 协作机制引导：告诉 AI 何时自动使用 Goal / Plan / Subagent / Jobs。
            persona.push_str(
                "\n\nCollaboration mechanisms (use them proactively when appropriate):\n\
                 - Goals: when the user states a long-term objective to track across turns, call goal_create so it shows on the Goals card.\n\
                 - Plan: for complex multi-step tasks, write a plan with plan_write (Plan card), then exit_plan_mode when the plan is done and start executing.\n\
                 - Subagents: split clearly independent subtasks (file analysis, isolated research) into subagent_fork calls to run in parallel; results come back as summaries.\n\
                 - Jobs: your turns and subagent runs are tracked automatically on the Jobs card; no action needed from you.\n",
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
            // 预设白名单（对所有执行路径生效：内置 / subagent_fork / 插件工具）
            let blocked_by_whitelist = preset
                .tool_whitelist()
                .map(|wl| !wl.contains(&tc.function.name.as_str()))
                .unwrap_or(false);
            let out = if blocked_by_whitelist {
                ToolOutput::err(format!(
                    "tool {} 在当前预设（{}）不可用",
                    tc.function.name,
                    preset.name()
                ))
            } else if tc.function.name == "subagent_fork" {
                // 子代理：独立单轮 LLM 问答（对齐 dsh-subagent-fork）。
                // 注册 descriptor（subagent/descriptor 事件持久化，重放可恢复）、
                // 创建子代理任务，总结返回主代理继续回合。
                let desc = parsed_args
                    .get("description")
                    .and_then(|v| v.as_str())
                    .unwrap_or("子代理任务")
                    .to_string();
                let prompt = parsed_args
                    .get("prompt")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                if prompt.trim().is_empty() {
                    ToolOutput::err("subagent_fork: prompt 不能为空")
                } else {
                    info!("subagent_fork: entering spawn for {session_id}");
                    // 注意：MutexGuard 临时值在 `match 表达式` 中会存活到整个
                    // match 结束（Rust 临时生命周期规则）。若在 match 的
                    // 分支体内再次 subagents.lock()，就是同一线程重复获取
                    // 非重入 Mutex → 死锁（历史回归：subagent_fork 调用后
                    // 回合永久卡死，descriptor/tool/result 均不落盘）。
                    // 因此先在小块内完成加锁+spawn，guard 立即释放再 match。
                    let spawn_res = {
                        let mut mgr = subagents.lock().unwrap();
                        mgr.spawn(session_id, 1, 3)
                    };
                    match spawn_res {
                        Err(e) => ToolOutput::err(format!("subagent_fork: {e:#}")),
                        Ok(sub_id) => {
                            info!("subagent_fork: spawned {sub_id}");
                            // 子代理任务（pending → running）
                            let sjid = {
                                let mut jm = jobs.lock().unwrap();
                                let id = jm.create("subagent", Some(format!("{desc}（{sub_id}）")));
                                jm.start(&id);
                                id
                            };
                            // descriptor: running
                            info!("subagent_fork: marking running {sub_id}");
                            subagents
                                .lock()
                                .unwrap()
                                .mark(&sub_id, SubagentStatus::Running);
                            if let Some(d) = subagents.lock().unwrap().get(&sub_id).cloned() {
                                let dev = SubagentManager::descriptor_event(&d);
                                info!("subagent_fork: appending descriptor {sub_id}");
                                append_alive(&store, sessions_shared, session_id, &dev)?;
                                info!("subagent_fork: descriptor appended ok");
                                let _ = tx.send(EngineEvent::Event {
                                    session_id: session_id.to_string(),
                                    event: dev,
                                });
                            }
                            // 独立 LLM 调用
                            let sys = format!(
                                "You are a subagent of a coding agent running on DeepSeek \
                                 Harness. Your parent session is {session_id}. Complete the \
                                 task below and reply with a concise summary of what you did \
                                 and the result."
                            );
                            let summary = crate::engine::subagent::run_subagent_turn(
                                llm.clone(),
                                &sys,
                                &prompt,
                                8,
                            )
                            .await;
                            info!(
                                "subagent_fork: llm turn done for {sub_id}: {:?}",
                                summary.as_ref().map(|s| s.chars().count())
                            );
                            let out = match &summary {
                                Ok(s) => {
                                    subagents.lock().unwrap().set_summary(&sub_id, s.clone());
                                    subagents
                                        .lock()
                                        .unwrap()
                                        .mark(&sub_id, SubagentStatus::Done);
                                    jobs.lock().unwrap().finish(&sjid, true, Some(s.clone()));
                                    ToolOutput::ok(json!({
                                        "subagent_id": sub_id,
                                        "summary": s,
                                        "note": "子代理已完成，总结如上",
                                    }))
                                }
                                Err(e) => {
                                    subagents
                                        .lock()
                                        .unwrap()
                                        .mark(&sub_id, SubagentStatus::Failed);
                                    jobs.lock().unwrap().finish(
                                        &sjid,
                                        false,
                                        Some(format!("{e:#}")),
                                    );
                                    ToolOutput::err(format!("subagent_fork 执行失败: {e:#}"))
                                }
                            };
                            // descriptor: done/failed（终态）
                            if let Some(d) = subagents.lock().unwrap().get(&sub_id).cloned() {
                                let dev = SubagentManager::descriptor_event(&d);
                                append_alive(&store, sessions_shared, session_id, &dev)?;
                                let _ = tx.send(EngineEvent::Event {
                                    session_id: session_id.to_string(),
                                    event: dev,
                                });
                            }
                            out
                        }
                    }
                }
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
                                match run_approval(tx, approvals, target, reason, session_id).await
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
            // goal_create：写入 goal/change 事件（goal 面板 fold 恢复）
            if tc.function.name == "goal_create" {
                if let Some(obj) = parsed_args.get("objective").and_then(|v| v.as_str()) {
                    let gid = format!("g-{}", uuid());
                    let gev = crate::engine::goal::goal_change_event(
                        crate::engine::goal::GoalOp::Create,
                        &gid,
                        Some(obj),
                    );
                    append_alive(&store, sessions_shared, session_id, &gev)?;
                    let _ = tx.send(EngineEvent::Event {
                        session_id: session_id.to_string(),
                        event: gev,
                    });
                    info!("goal created {gid}: {obj}");
                }
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
            // exit_plan_mode：退出计划模式
            if tc.function.name == "exit_plan_mode" {
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
        settings.api_key = Some("fake-key".into());
        settings.base_url = "http://127.0.0.1:1".into(); // 不可达：回合线程会快速失败
        settings.data_dir = tempfile::tempdir().unwrap().path().to_path_buf();
        // 隔离：技能/插件目录指向空临时目录（不加载用户环境，避免子进程副作用）
        settings.skills_dir = Some(tempfile::tempdir().unwrap().path().join("skills"));
        settings.plugins_dir = Some(tempfile::tempdir().unwrap().path().join("plugins"));
        let engine = DshEngine::new(settings, tx).expect("engine");
        (engine, rx)
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
                .any(|m| matches!(m, Message::User { content } if content == "插话消息")),
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
}
