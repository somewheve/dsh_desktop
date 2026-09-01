//! 子代理：in-process 驱动 + 递归深度预算（对齐 dsh-subagent*）。
//!
//! 子代理复用同一 LLM 客户端，独立会话事件序列（subagent/descriptor 事件
//! 记录在父会话）。深度预算防止无限递归（子代理再派生子代理受 depth 限制）。

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::core::llm::{LlmClient, LlmMessage, StreamEvent};
use crate::core::session::types;

/// 子代理状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubagentStatus {
    Pending,
    Running,
    Done,
    Failed,
}

impl SubagentStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            SubagentStatus::Pending => "pending",
            SubagentStatus::Running => "running",
            SubagentStatus::Done => "done",
            SubagentStatus::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubagentDescriptor {
    pub subagent_id: String,
    pub parent_session_id: String,
    pub status: String,
    /// 摘要（卡片行展示，截断）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// 完整任务（用户在卡片展开可见）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
    /// 完整输出（卡片展开可见；descriptor 事件持久化，重放可恢复）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
}

/// 子代理管理器。
#[derive(Debug, Default)]
pub struct SubagentManager {
    subagents: HashMap<String, SubagentDescriptor>,
}

impl SubagentManager {
    /// 派生子代理（记录 descriptor 事件，深度检查由调用方做）。
    pub fn spawn(
        &mut self,
        parent_session_id: &str,
        depth: u32,
        max_depth: u32,
        task: &str,
    ) -> Result<String> {
        if depth > max_depth {
            anyhow::bail!("subagent depth exceeded: {depth} > {max_depth}");
        }
        let id = format!("sub-{}", uuidish());
        self.subagents.insert(
            id.clone(),
            SubagentDescriptor {
                subagent_id: id.clone(),
                parent_session_id: parent_session_id.to_string(),
                status: SubagentStatus::Pending.as_str().to_string(),
                summary: None,
                task: Some(task.to_string()),
                result: None,
            },
        );
        Ok(id)
    }

    pub fn mark(&mut self, id: &str, status: SubagentStatus) {
        if let Some(s) = self.subagents.get_mut(id) {
            s.status = status.as_str().to_string();
        }
    }

    pub fn set_summary(&mut self, id: &str, summary: String) {
        if let Some(s) = self.subagents.get_mut(id) {
            s.summary = Some(summary);
        }
    }

    /// 完整输出（与摘要分开存：摘要供主代理/卡片行，全文供用户展开查看）。
    pub fn set_result(&mut self, id: &str, result: String) {
        if let Some(s) = self.subagents.get_mut(id) {
            s.result = Some(result);
        }
    }

    pub fn get(&self, id: &str) -> Option<&SubagentDescriptor> {
        self.subagents.get(id)
    }

    pub fn list(&self) -> Vec<&SubagentDescriptor> {
        let mut v: Vec<_> = self.subagents.values().collect();
        v.sort_by(|a, b| a.subagent_id.cmp(&b.subagent_id));
        v
    }

    /// 构造 subagent/descriptor 事件。
    pub fn descriptor_event(d: &SubagentDescriptor) -> crate::core::session::SessionEvent {
        crate::core::session::SessionEvent::new(
            types::SUBAGENT_DESCRIPTOR,
            Some(serde_json::to_value(d).unwrap_or_default()),
        )
    }
}

fn uuidish() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{nanos:x}")
}

/// 子代理回合：独立 LLM 调用（对齐 subagent-in-process-driver 的核心语义）。
pub async fn run_subagent_turn(
    llm: Arc<LlmClient>,
    system_prompt: &str,
    task: &str,
    max_steps: usize,
) -> Result<String> {
    run_subagent_turn_counted(llm, system_prompt, task, max_steps)
        .await
        .map(|(text, _)| text)
}

/// 带用量回传的子代理回合（子代理是 token 放大器，消耗必须记账——
/// 历史缺陷：`_ => {}` 丢弃 Usage 事件，统计缺一大块）。
pub async fn run_subagent_turn_counted(
    llm: Arc<LlmClient>,
    system_prompt: &str,
    task: &str,
    max_steps: usize,
) -> Result<(String, crate::core::llm::TokenUsage)> {
    let messages = vec![
        LlmMessage {
            role: crate::core::llm::LlmRole::System,
            content: Some(system_prompt.to_string()),
            tool_call_id: None,
            tool_calls: None,
            name: None,
            images: None,
        },
        LlmMessage {
            role: crate::core::llm::LlmRole::User,
            content: Some(task.to_string()),
            tool_call_id: None,
            tool_calls: None,
            name: None,
            images: None,
        },
    ];
    let full: std::sync::Arc<std::sync::Mutex<String>> =
        std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    let full_cb = full.clone();
    let usage = std::sync::Arc::new(std::sync::Mutex::new(
        crate::core::llm::TokenUsage::default(),
    ));
    let usage_cb = usage.clone();
    // 单轮问答（max_steps 保留在签名里以对齐 dsh-subagent 语义，当前只跑一轮）
    let _ = max_steps;
    llm.stream(&messages, None, move |evt| match evt {
        StreamEvent::Chunk(c) => full_cb.lock().unwrap().push_str(&c),
        StreamEvent::Usage(u) => {
            *usage_cb.lock().unwrap() += u;
        }
        StreamEvent::Done => {}
        _ => {}
    })
    .await?;
    let result = full.lock().unwrap().clone();
    let u = *usage.lock().unwrap();
    Ok((result, u))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spawn_and_depth() {
        let mut mgr = SubagentManager::default();
        let id = mgr.spawn("s-parent", 1, 3, "分析文件").unwrap();
        assert_eq!(mgr.get(&id).unwrap().status, "pending");
        mgr.mark(&id, SubagentStatus::Running);
        mgr.set_summary(&id, "done".into());
        mgr.set_result(&id, "完整输出正文".into());
        assert_eq!(mgr.get(&id).unwrap().result.as_deref(), Some("完整输出正文"));
        assert_eq!(mgr.get(&id).unwrap().task.as_deref(), Some("分析文件"));
        assert_eq!(mgr.get(&id).unwrap().summary.as_deref(), Some("done"));
        assert!(mgr.spawn("s-parent", 4, 3, "t").is_err(), "深度超限应失败");
    }
}
