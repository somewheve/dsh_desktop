//! 会话持久化：JSONL 事件日志（对齐 DSH session-persistence-jsonl 思路）。
//!
//! 每个会话一个 .jsonl 文件，逐行追加 SessionEvent；加载时重放。

use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;

use std::collections::VecDeque;

use anyhow::{Context, Result};
use log::{info, warn};

use super::preset::AgentPreset;
use super::session::{Message, Session, SessionEvent};

pub struct SessionStore {
    pub(crate) dir: PathBuf,
}

impl SessionStore {
    pub fn new(dir: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("create session dir {}", dir.display()))?;
        Ok(Self { dir })
    }

    fn path_for(&self, session_id: &str) -> PathBuf {
        self.dir.join(format!("{session_id}.jsonl"))
    }

    /// 追加一个事件到会话日志。
    pub fn append(&self, session_id: &str, event: &SessionEvent) -> Result<()> {
        let path = self.path_for(session_id);
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("open {}", path.display()))?;
        let line = serde_json::to_string(event)?;
        writeln!(f, "{line}")?;
        Ok(())
    }

    /// 重放会话全部事件（用于恢复）。
    pub fn load_events(&self, session_id: &str) -> Vec<SessionEvent> {
        let path = self.path_for(session_id);
        let file = match std::fs::File::open(&path) {
            Ok(f) => f,
            Err(_) => return Vec::new(),
        };
        let reader = BufReader::new(file);
        let mut out = Vec::new();
        for line in reader.lines() {
            if let Ok(line) = line {
                if line.trim().is_empty() {
                    continue;
                }
                match serde_json::from_str::<SessionEvent>(&line) {
                    Ok(ev) => out.push(ev),
                    Err(e) => warn!("skip bad event line in {}: {e}", path.display()),
                }
            }
        }
        out
    }

    /// 列出所有会话文件。
    pub fn list_sessions(&self) -> Vec<String> {
        let mut ids = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&self.dir) {
            for e in entries.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                if name.ends_with(".jsonl") {
                    ids.push(name.trim_end_matches(".jsonl").to_string());
                }
            }
        }
        ids.sort();
        ids
    }

    /// 删除会话。
    pub fn delete(&self, session_id: &str) -> Result<()> {
        let path = self.path_for(session_id);
        if path.exists() {
            std::fs::remove_file(&path)?;
            info!("deleted session {session_id}");
        }
        Ok(())
    }

    /// 恢复会话（events → messages 投影）。
    ///
    /// tool/call + tool/result 按出现顺序 FIFO 配对投影为 Message::Tool，
    /// 保证重放序列满足 OpenAI 兼容约束：assistant 的 tool_calls 后必须有
    /// 对应 tool_call_id 的 tool 消息响应（否则 API 返回 400）。
    pub fn load_session(&self, session_id: &str) -> Result<Session> {
        use super::session::types::{TOOL_CALL, TOOL_RESULT};
        let events = self.load_events(session_id);
        let mut session = Session::new(session_id.to_string());
        let mut pending_calls: VecDeque<String> = VecDeque::new();
        for ev in &events {
            session.events.push(ev.clone());
            if let Some(data) = &ev.data {
                match ev.r#type.as_str() {
                    TOOL_CALL => {
                        if let Some(cid) = data.get("call_id").and_then(|v| v.as_str()) {
                            pending_calls.push_back(cid.to_string());
                        }
                    }
                    TOOL_RESULT => {
                        if let Some(cid) = pending_calls.pop_front() {
                            let content =
                                data.get("value").map(|v| v.to_string()).unwrap_or_default();
                            session.messages.push(Message::Tool {
                                tool_call_id: cid,
                                content,
                            });
                        } else {
                            warn!("tool/result 无对应 tool/call，忽略（{session_id}）");
                        }
                    }
                    _ => project_event(&mut session, &ev.r#type, data),
                }
            }
        }
        session.updated_at = events.last().map(|e| e.time).unwrap_or(session.updated_at);
        Ok(session)
    }
}

/// 事件 → session 消息投影（对齐 DSH 前端 fold 语义的核心子集）。
fn project_event(session: &mut Session, event_type: &str, data: &serde_json::Value) {
    use super::session::types::*;
    match event_type {
        SESSION_PRESET => {
            if let Some(p) = data
                .get("preset")
                .and_then(|v| v.as_str())
                .and_then(AgentPreset::parse)
            {
                session.preset = p;
            }
        }
        ASSISTANT_MESSAGE | USER_MESSAGE => {
            if let Some(content) = data.get("content").and_then(|c| c.as_str()) {
                let msg = if event_type == USER_MESSAGE {
                    super::session::Message::User {
                        content: content.to_string(),
                    }
                } else {
                    // 恢复 tool_calls（OpenAI 兼容：tool 消息前必须有带 tool_calls 的 assistant）
                    let tool_calls = data
                        .get("tool_calls")
                        .and_then(|v| {
                            serde_json::from_value::<Vec<super::session::ToolCall>>(v.clone()).ok()
                        })
                        .unwrap_or_default();
                    super::session::Message::Assistant {
                        content: content.to_string(),
                        tool_calls,
                    }
                };
                session.messages.push(msg);
            }
        }
        SESSION_TITLE => {
            if let Some(title) = data.get("title").and_then(|t| t.as_str()) {
                session.title = title.to_string();
            }
        }
        SESSION_CWD => {
            if let Some(cwd) = data.get("cwd").and_then(|c| c.as_str()) {
                session.cwd = Some(std::path::PathBuf::from(cwd));
            }
        }
        _ => {}
    }
}

/// 测试辅助：临时 store。
#[cfg(test)]
pub fn temp_store() -> (SessionStore, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::new(dir.path().to_path_buf()).unwrap();
    (store, dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::session::{types, Message};

    #[test]
    fn append_and_load_roundtrip() {
        let (store, _d) = temp_store();
        let mut s = Session::new("t1".into());
        s.push_event(SessionEvent::new(
            types::USER_MESSAGE,
            Some(serde_json::json!({"content": "hello"})),
        ));
        s.push_event(SessionEvent::new(
            types::ASSISTANT_MESSAGE,
            Some(serde_json::json!({"content": "hi"})),
        ));
        for ev in &s.events {
            store.append(&s.id, ev).unwrap();
        }
        let loaded = store.load_session("t1").unwrap();
        assert_eq!(loaded.messages.len(), 2);
        match &loaded.messages[0] {
            Message::User { content } => assert_eq!(content, "hello"),
            _ => panic!("expected user message"),
        }
        assert!(store.list_sessions().contains(&"t1".to_string()));
    }

    #[test]
    fn delete_removes() {
        let (store, _d) = temp_store();
        store
            .append("gone", &SessionEvent::new(types::TURN_START, None))
            .unwrap();
        store.delete("gone").unwrap();
        assert!(!store.list_sessions().contains(&"gone".to_string()));
    }
}
