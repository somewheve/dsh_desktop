//! 上下文压缩：compaction/start|end|summary 事件 + 基本摘要策略（对齐 dsh-compaction-basic）。

use crate::core::session::{types, Message, SessionEvent};

#[derive(Debug, Clone)]
pub struct CompactionPlan {
    /// 压缩后保留的消息
    pub kept: Vec<Message>,
    /// 被压缩的消息数
    pub removed: usize,
    /// 摘要文本
    pub summary: String,
}

/// 简单策略：保留 system + 最近 N 条消息，中间压缩为摘要。
pub fn plan_compaction(messages: &[Message], keep_tail: usize) -> CompactionPlan {
    if messages.len() <= keep_tail {
        return CompactionPlan {
            kept: messages.to_vec(),
            removed: 0,
            summary: String::new(),
        };
    }
    let keep_from = messages.len() - keep_tail;
    let removed = keep_from;
    let head = &messages[..keep_from];
    // 摘要：统计用户消息数 + 截取文本
    let user_count = head
        .iter()
        .filter(|m| matches!(m, Message::User { .. }))
        .count();
    let sample: String = head
        .iter()
        .filter_map(|m| match m {
            Message::User { content } => Some(content.chars().take(80).collect::<String>()),
            _ => None,
        })
        .take(2)
        .collect::<Vec<_>>()
        .join(" … ");
    let summary = format!(
        "（上下文已压缩：{} 条消息，其中 {} 条用户消息；早期内容：{sample}）",
        removed, user_count
    );
    CompactionPlan {
        kept: messages[keep_from..].to_vec(),
        removed,
        summary,
    }
}

/// 事件：compaction/start / end / summary。
pub fn compaction_start_event() -> SessionEvent {
    SessionEvent::new(types::COMPACTION_START, None)
}
pub fn compaction_end_event() -> SessionEvent {
    SessionEvent::new(types::COMPACTION_END, None)
}
pub fn compaction_summary_event(summary: &str) -> SessionEvent {
    SessionEvent::new(
        types::COMPACTION_SUMMARY,
        Some(serde_json::json!({"summary": summary})),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compaction_keeps_tail() {
        let mut msgs = Vec::new();
        for i in 0..10 {
            msgs.push(Message::User {
                content: format!("msg {i}"),
            });
        }
        let plan = plan_compaction(&msgs, 3);
        assert_eq!(plan.removed, 7);
        assert_eq!(plan.kept.len(), 3);
        assert!(plan.summary.contains("7 条用户消息"));
    }

    #[test]
    fn no_compaction_when_small() {
        let msgs = vec![Message::User {
            content: "hi".into(),
        }];
        let plan = plan_compaction(&msgs, 10);
        assert_eq!(plan.removed, 0);
        assert!(plan.summary.is_empty());
    }
}
