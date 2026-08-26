//! 会话事件模型：对齐 DSH SessionEvent 词汇表。
//!
//! 参考 @deepseek-ai/dsh-session 的 KNOWN_SESSION_EVENT_TYPES（开源实现），
//! 本模块用 Rust 原生重写核心事件子集，保持 type/seq/time/data 信封形状。

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

/// 会话事件信封（对齐 DSH sessionEventSchema）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct SessionEvent {
    pub r#type: String,
    pub seq: u64,
    pub time: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

/// 事件类型常量（DSH 词汇表的 Rust 子集）。
pub mod types {
    pub const TURN_START: &str = "turn/start";
    pub const TURN_END: &str = "turn/end";
    pub const STEP_START: &str = "step/start";
    pub const STEP_END: &str = "step/end";
    pub const ASSISTANT_CHUNK: &str = "assistant/chunk";
    pub const ASSISTANT_MESSAGE: &str = "assistant/message";
    pub const TOOL_CALL: &str = "tool/call";
    pub const TOOL_RESULT: &str = "tool/result";
    pub const SESSION_TITLE: &str = "session/title";
    pub const USER_MESSAGE: &str = "user/message";
    /// ask_user：AI 向用户提问（等待用户选择/回答）
    pub const USER_QUESTION: &str = "user/question";
    pub const COMPACTION_START: &str = "compaction/start";
    pub const COMPACTION_SUMMARY: &str = "compaction/summary";
    pub const COMPACTION_END: &str = "compaction/end";
    pub const GOAL_CHANGE: &str = "goal/change";
    pub const PLAN_MODE: &str = "plan/mode";
    pub const WORKFLOW_RUN_START: &str = "tool-workflow/run-start";
    pub const WORKFLOW_RUN_END: &str = "tool-workflow/run-end";
    pub const SCHEDULE_CHANGE: &str = "schedule/change";
    pub const SUBAGENT_DESCRIPTOR: &str = "subagent/descriptor";
    pub const FEEDBACK_RECORD: &str = "feedback/record";
    pub const SESSION_PRESET: &str = "session/preset";
    /// 会话独立工作区（cwd，set_session_workspace 持久化）
    pub const SESSION_CWD: &str = "session/cwd";
}

impl SessionEvent {
    pub fn new(r#type: &str, data: Option<serde_json::Value>) -> Self {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        Self {
            r#type: r#type.to_string(),
            seq: 0,
            time: now.as_secs_f64(),
            data,
        }
    }
}

/// 会话消息（对齐 LLM 消息模型 + DSH assistant/message 语义）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "lowercase")]
pub enum Message {
    #[serde(rename = "user")]
    User { content: String },
    #[serde(rename = "assistant")]
    Assistant {
        content: String,
        /// 本轮调用的工具（OpenAI 要求 tool 消息前必须有带 tool_calls 的 assistant 消息）
        #[serde(default)]
        tool_calls: Vec<ToolCall>,
    },
    #[serde(rename = "tool")]
    Tool {
        #[serde(rename = "tool_call_id")]
        tool_call_id: String,
        content: String,
    },
}

/// 工具调用（LLM 返回的 function call）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub call_type: String,
    pub function: FunctionCall,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionCall {
    pub name: String,
    pub arguments: String,
}

/// 会话摘要（对齐 sessionSummarySchema）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSummary {
    pub session_id: String,
    pub updated_at: f64,
    pub running: bool,
    pub blank: bool,
    pub title: String,
}

/// 会话（内存模型）。
#[derive(Debug, Clone)]
pub struct Session {
    pub id: String,
    pub title: String,
    pub created_at: f64,
    pub updated_at: f64,
    pub running: bool,
    /// 绑定工作区目录（会话级 cwd，对齐 session.create 的 workspaceId/cwd）
    pub cwd: Option<PathBuf>,
    /// Agent 预设（对齐 session.create 的 agentPresets）
    pub preset: crate::core::preset::AgentPreset,
    /// 完整事件序列（持久化依据）
    pub events: Vec<SessionEvent>,
    /// 给 LLM 的消息序列（从 events 投影）
    pub messages: Vec<Message>,
}

impl Session {
    pub fn new(id: String) -> Self {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs_f64();
        Self {
            id,
            title: String::new(),
            created_at: now,
            updated_at: now,
            running: false,
            cwd: None,
            preset: crate::core::preset::AgentPreset::Standard,
            events: Vec::new(),
            messages: Vec::new(),
        }
    }

    /// 追加事件（自动编号 seq）。
    pub fn push_event(&mut self, mut ev: SessionEvent) {
        ev.seq = self.events.len() as u64;
        self.events.push(ev);
        self.updated_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs_f64();
    }

    pub fn summary(&self) -> SessionSummary {
        let title = if self.title.is_empty() {
            // 从第一条用户消息提炼标题
            self.messages
                .iter()
                .find_map(|m| match m {
                    Message::User { content } => Some(content.chars().take(24).collect()),
                    _ => None,
                })
                .unwrap_or_else(|| "新会话".into())
        } else {
            self.title.clone()
        };
        SessionSummary {
            session_id: self.id.clone(),
            updated_at: self.updated_at,
            running: self.running,
            blank: self.messages.is_empty(),
            title,
        }
    }
}
