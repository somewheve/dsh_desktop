//! 上下文压缩：compaction/start|end|summary 事件 + 基本摘要策略（对齐 dsh-compaction-basic）。
//!
//! 切分规则（修复历史孤儿问题）：
//! - 保留尾部 keep_tail 条消息，但边界**向前扩展**跨过 tool 消息直到
//!   assistant（tool 消息不能做开头——它必须跟在声明它的 assistant 之后，
//!   否则 OpenAI 兼容 API 400）；
//! - 被裁掉头部的 assistant 若带未响应的 tool_calls，to_llm_messages 会
//!   裁剪成空壳并移除（既有防御）；
//! - 摘要作为 **User 消息**插回队首（不是丢弃：模型仍能感知早期上下文）。

use crate::core::session::{types, Message, SessionEvent};

#[derive(Debug, Clone)]
pub struct CompactionPlan {
    /// 压缩后保留的消息（含摘要 User 消息）
    pub kept: Vec<Message>,
    /// 被压缩的消息数
    pub removed: usize,
    /// 摘要文本
    pub summary: String,
}

/// 简单策略：保留最近 keep_tail 条，头部压缩为摘要 User 消息。
pub fn plan_compaction(messages: &[Message], keep_tail: usize) -> CompactionPlan {
    if messages.len() <= keep_tail {
        return CompactionPlan {
            kept: messages.to_vec(),
            removed: 0,
            summary: String::new(),
        };
    }
    let mut keep_from = messages.len() - keep_tail;
    // 边界向前扩展：kept 不能以 Tool 开头（孤儿 tool → API 400）
    while keep_from > 0 && matches!(messages[keep_from], Message::Tool { .. }) {
        keep_from -= 1;
    }
    if keep_from == 0 {
        // 整段都是 tool 链（异常防御）：不动
        return CompactionPlan {
            kept: messages.to_vec(),
            removed: 0,
            summary: String::new(),
        };
    }
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
    let mut kept = Vec::with_capacity(keep_tail + 1);
    kept.push(Message::User {
        content: format!(
            "[context summary] 以下是本会话早期内容的压缩摘要，作为上下文背景：{summary}"
        ),
    });
    kept.extend_from_slice(&messages[keep_from..]);
    CompactionPlan {
        kept,
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
        // 摘要 User + 3 条尾部
        assert_eq!(plan.kept.len(), 4);
        assert!(matches!(plan.kept[0], Message::User { .. }));
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

    /// 回归：切分边界不得产生孤儿 tool 消息（kept 以 Tool 开头 → API 400）。
    #[test]
    fn boundary_never_starts_with_orphan_tool() {
        let mut msgs = vec![Message::User {
            content: "go".into(),
        }];
        for i in 0..8 {
            msgs.push(Message::Assistant {
                content: String::new(),
                tool_calls: vec![crate::core::session::ToolCall {
                    id: format!("c{i}"),
                    call_type: "function".into(),
                    function: crate::core::session::FunctionCall {
                        name: "bash".into(),
                        arguments: "{}".into(),
                    },
                }],
            });
            msgs.push(Message::Tool {
                tool_call_id: format!("c{i}"),
                content: "ok".into(),
            });
        }
        // keep_tail=3 会切在 Tool 上 → 边界必须回退到其 assistant
        let plan = plan_compaction(&msgs, 3);
        assert!(plan.removed > 0, "应发生压缩: removed={}", plan.removed);
        // kept[0] 是摘要 User；摘要后的第一条真实消息不能是孤儿 tool
        assert!(
            !matches!(plan.kept[1], Message::Tool { .. }),
            "kept 的首条真实消息不能是孤儿 tool: {:?}",
            plan.kept
                .iter()
                .map(|m| match m {
                    Message::User { .. } => 'U',
                    Message::Assistant { .. } => 'A',
                    Message::Tool { .. } => 'T',
                })
                .collect::<String>()
        );
        // kept 内每条 Tool 的 id 都能找到前置声明的 assistant（配对完整）
        let declared: Vec<String> = plan
            .kept
            .iter()
            .filter_map(|m| match m {
                Message::Assistant { tool_calls, .. } => Some(
                    tool_calls
                        .iter()
                        .map(|tc| tc.id.clone())
                        .collect::<Vec<String>>(),
                ),
                _ => None,
            })
            .collect::<Vec<Vec<String>>>()
            .concat();
        for m in &plan.kept {
            if let Message::Tool { tool_call_id, .. } = m {
                assert!(
                    declared.contains(tool_call_id),
                    "kept 内 tool {tool_call_id} 必须有前置声明"
                );
            }
        }
    }

    /// 摘要消息进入 kept（模型仍感知早期上下文，而非直接丢弃）。
    #[test]
    fn summary_injected_as_user_message() {
        let mut msgs = Vec::new();
        for i in 0..20 {
            msgs.push(Message::User {
                content: format!("msg {i}"),
            });
        }
        let plan = plan_compaction(&msgs, 5);
        match &plan.kept[0] {
            Message::User { content } => {
                assert!(
                    content.contains("[context summary]"),
                    "摘要应为 User 消息: {content}"
                );
            }
            other => panic!("kept[0] 应为摘要 User 消息: {other:?}"),
        }
        assert_eq!(plan.kept.len(), 6);
    }
}
