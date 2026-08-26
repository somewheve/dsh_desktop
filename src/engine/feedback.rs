//! 消息反馈：feedback/record 事件（对齐 dsh-message-feedback）。

use std::collections::HashMap;

use crate::core::session::{types, SessionEvent};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeedbackKind {
    Upvote,
    Downvote,
}

impl FeedbackKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            FeedbackKind::Upvote => "upvote",
            FeedbackKind::Downvote => "downvote",
        }
    }
}

#[derive(Debug, Default)]
pub struct FeedbackStore {
    /// message_id -> kind
    records: HashMap<String, FeedbackKind>,
}

impl FeedbackStore {
    pub fn record(&mut self, message_id: &str, kind: FeedbackKind) {
        self.records.insert(message_id.to_string(), kind);
    }

    pub fn get(&self, message_id: &str) -> Option<FeedbackKind> {
        self.records.get(message_id).copied()
    }

    pub fn count(&self) -> usize {
        self.records.len()
    }

    /// 构造 feedback/record 事件。
    pub fn record_event(message_id: &str, kind: FeedbackKind) -> SessionEvent {
        SessionEvent::new(
            types::FEEDBACK_RECORD,
            Some(serde_json::json!({"messageId": message_id, "kind": kind.as_str()})),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feedback_roundtrip() {
        let mut store = FeedbackStore::default();
        store.record("m1", FeedbackKind::Upvote);
        assert_eq!(store.get("m1"), Some(FeedbackKind::Upvote));
        assert_eq!(store.count(), 1);
        let ev = FeedbackStore::record_event("m1", FeedbackKind::Upvote);
        assert_eq!(ev.r#type, types::FEEDBACK_RECORD);
    }
}
