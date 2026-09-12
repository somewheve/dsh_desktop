//! 核心引擎：Rust 原生重写 DSH 的 agent 闭环。
//!
//! 模块划分（对齐 DSH 开源实现）：
//! - agent.rs       turn/step 驱动循环 + 工具调度（dsh-agent）
//! - llm.rs         DeepSeek API 客户端（dsh-llm）
//! - session.rs     会话模型 + 事件词汇表（dsh-session）
//! - tools.rs       核心工具（dsh-tool-bash/fs/todo）
//! - storage.rs     JSONL 事件持久化（dsh-session-persistence-jsonl）
//! - settings.rs    API key/model/proxy（dsh-settings + dsh-credentials）

pub mod agent;
pub mod diff;
pub mod llm;
pub mod preset;
pub mod session;
pub mod settings;
pub mod storage;
pub mod paper;
pub mod tools;
pub mod workspace;

pub use agent::{AgentStatus, DshEngine, EngineEvent};
pub use preset::AgentPreset;
pub use session::{Message, Session, SessionEvent, SessionSummary, ToolCall};
pub use workspace::{Workspace, WorkspaceManager};

// 沙箱模式（执行受限/直通语义；由 exec 层提供）
pub use crate::exec::SandboxMode;
