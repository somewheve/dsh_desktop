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
    /// 已删除会话的墓碑（append 前查——check-then-write 的 TOCTOU 窗口里
    /// 回合线程的 append 会"复活"已删 .jsonl 文件）。
    /// **必须 Arc 共享**:clone_handle 产生的回合线程副本必须看到引擎
    /// 实例的墓碑,否则 TOCTOU 修复对真正要防的路径无效。
    pub(crate) tombstones: std::sync::Arc<
        std::sync::Mutex<std::collections::HashSet<String>>,
    >,
    pub(crate) dir: PathBuf,
}

impl SessionStore {
    pub fn new(dir: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("create session dir {}", dir.display()))?;
        Ok(Self {
            dir,
            tombstones: std::sync::Arc::new(std::sync::Mutex::new(
                std::collections::HashSet::new(),
            )),
        })
    }

    /// 记录墓碑（删除时调用）。
    pub fn mark_deleted(&self, session_id: &str) {
        self.tombstones
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(session_id.to_string());
    }

    /// 清除墓碑（该会话重新创建时）。
    pub fn clear_tombstone(&self, session_id: &str) {
        self.tombstones
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(session_id);
    }

    /// 会话文件是否存在（测试用）。
    pub fn path_exists(&self, session_id: &str) -> bool {
        self.path_for(session_id).exists()
    }

    fn path_for(&self, session_id: &str) -> PathBuf {
        self.dir.join(format!("{session_id}.jsonl"))
    }

    /// 追加一个事件到会话日志。
    pub fn append(&self, session_id: &str, event: &SessionEvent) -> Result<()> {
        // 墓碑检查（TOCTOU 修复:删除线程与回合线程的 append 竞态——
        // check-then-write 窗口里 create(true) 会复活已删 .jsonl）
        if self
            .tombstones
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .contains(session_id)
        {
            return Ok(()); // 静默丢弃（调用侧的 session_alive 已 warn）
        }
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
        // 先记墓碑再删文件（与回合线程 append 的原子屏障）
        self.mark_deleted(session_id);
        let path = self.path_for(session_id);
        if path.exists() {
            std::fs::remove_file(&path)?;
            info!("deleted session {session_id}");
        }
        Ok(())
    }

    /// 恢复会话（events → messages 投影）。
    ///
    /// tool/result 带 call_id 时按 id 精确配对（交错/中断回合的 FIFO 配对
    /// 会把结果错配给别的调用）；旧格式（无 call_id）退化为 FIFO。
    /// 工具结果内容按在线同样的 8000 字符上限截断（否则重启重载后完整
    /// stdout 进上下文，撑爆请求）。
    pub fn load_session(&self, session_id: &str) -> Result<Session> {
        use super::session::types::{COMPACTION_SUMMARY, TOOL_CALL, TOOL_RESULT, TURN_START};
        let events = self.load_events(session_id);
        let mut session = Session::new(session_id.to_string());
        let mut pending_calls: VecDeque<String> = VecDeque::new();
        for ev in &events {
            session.events.push(ev.clone());
            if let Some(data) = &ev.data {
                match ev.r#type.as_str() {
                    TURN_START => {
                        // 新回合开始：旧回合遗留的未执行 tool/call 不应配给
                        // 后续 result（中断回合的孤儿声明）
                        pending_calls.clear();
                    }
                    TOOL_CALL => {
                        if let Some(cid) = data.get("call_id").and_then(|v| v.as_str()) {
                            pending_calls.push_back(cid.to_string());
                        }
                    }
                    TOOL_RESULT => {
                        let declared = data.get("call_id").and_then(|v| v.as_str());
                        let cid = match declared {
                            Some(s) => {
                                // 按 id 精确取（不在队头也行；找不到 = 孤儿 result）
                                pending_calls
                                    .iter()
                                    .position(|c| c == s)
                                    .map(|pos| pending_calls.remove(pos).unwrap_or_default())
                            }
                            None => pending_calls.pop_front(), // 旧格式 FIFO
                        };
                        if let Some(cid) = cid {
                            let content = data
                                .get("value")
                                .map(|v| truncate_for_llm(&v.to_string()))
                                .unwrap_or_default();
                            session.messages.push(Message::Tool {
                                tool_call_id: cid,
                                content,
                            });
                        } else {
                            log::warn!("tool/result 无对应 tool/call，忽略（{session_id}）");
                        }
                    }
                    COMPACTION_SUMMARY => {
                        // 压缩折叠（历史缺陷：事件落盘但重放不折叠，压缩只活
                        // 一个回合）。kept_from 之前的历史消息已被压缩为摘要：
                        // 丢弃它们，插入一条摘要 User 消息作为上下文背景。
                        let summary = data.get("summary").and_then(|v| v.as_str());
                        let kept_from = data
                            .get("kept_from")
                            .and_then(|v| v.as_u64())
                            .map(|v| v as usize);
                        if let (Some(summary), Some(kf)) = (summary, kept_from) {
                            if kf != usize::MAX && kf <= session.messages.len() {
                                // 丢弃前 kf 条（保留尾部），再在头部插摘要
                                let tail: Vec<_> = session.messages.split_off(kf);
                                session.messages = tail;
                                session.messages.insert(
                                    0,
                                    super::session::Message::User {
                                        content: format!(
                                            "[context summary] 以下是本会话早期内容的压缩摘要，作为上下文背景：{summary}"
                                        ),
                                        images: None,
                                    },
                                );
                            }
                        }
                    }
                    _ => project_event(&mut session, &ev.r#type, data),
                }
            } else if ev.r#type == TURN_START {
                pending_calls.clear();
            }
        }
        session.updated_at = events.last().map(|e| e.time).unwrap_or(session.updated_at);
        Ok(session)
    }
}

/// 工具结果进 LLM 上下文的截断（防超长 JSON 撑爆上下文）。
/// 在线（agent.rs）与重放（load_session）共用同一上限。
pub fn truncate_for_llm(s: &str) -> String {
    const MAX: usize = 8000;
    if s.chars().count() <= MAX {
        return s.to_string();
    }
    let head: String = s.chars().take(MAX).collect();
    format!(
        "{head}\n…（结果过长，已截断 {} 字符）",
        s.chars().count() - MAX
    )
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
                    // 图片路径随事件恢复（多模态重放）
                    let images = data
                        .get("images")
                        .and_then(|v| v.as_array())
                        .map(|a| {
                            a.iter()
                                .filter_map(|x| x.as_str().map(String::from))
                                .collect::<Vec<String>>()
                        })
                        .filter(|v| !v.is_empty());
                    super::session::Message::User {
                        content: content.to_string(),
                        images,
                    }
                } else {
                    // 恢复 tool_calls（OpenAI 兼容：tool 消息前必须有带 tool_calls 的 assistant）
                    let tool_calls = data
                        .get("tool_calls")
                        .and_then(|v| {
                            serde_json::from_value::<Vec<super::session::ToolCall>>(v.clone()).ok()
                        })
                        .unwrap_or_default();
                    let reasoning = data
                        .get("reasoning_content")
                        .and_then(|v| v.as_str())
                        .map(String::from)
                        .filter(|r| !r.is_empty());
                    super::session::Message::Assistant {
                        content: content.to_string(),
                        tool_calls,
                        reasoning,
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
        SESSION_SANDBOX => {
            if let Some(m) = data.get("sandbox").and_then(|c| c.as_str()) {
                session.sandbox_mode = Some(m.to_string());
            }
        }
        SESSION_MODEL => {
            if let Some(m) = data.get("model").and_then(|c| c.as_str()) {
                session.model = Some(m.to_string());
            }
        }
        SESSION_EFFORT => {
            if let Some(e) = data.get("effort").and_then(|c| c.as_str()) {
                session.effort = Some(e.to_string());
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
        s.push_event(SessionEvent::new(
            types::ASSISTANT_MESSAGE,
            Some(serde_json::json!({
                "content": "answer",
                "reasoning_content": "let me think…",
                "tool_calls": []
            })),
        ));
        for ev in &s.events {
            store.append(&s.id, ev).unwrap();
        }
        let loaded = store.load_session("t1").unwrap();
        assert_eq!(loaded.messages.len(), 3);
        // 思考时间线：reasoning_content 随事件重放恢复（UI 折叠回看）
        match &loaded.messages[2] {
            Message::Assistant { reasoning, .. } => {
                assert_eq!(reasoning.as_deref(), Some("let me think…"))
            }
            _ => panic!("expected assistant"),
        }
        // 旧事件无 reasoning_content → None（向后兼容）
        match &loaded.messages[1] {
            Message::Assistant { reasoning, .. } => assert!(reasoning.is_none()),
            _ => panic!("expected assistant"),
        }
        match &loaded.messages[0] {
            Message::User { content, .. } => assert_eq!(content, "hello"),
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
