//! 权限审批注册表：引擎 → UI 的用户确认通道（串行）。
//!
//! 写操作越权（潜在工作区外）时，工具分发方先在本注册表登记一个审批请求，
//! 拿到 `mpsc::Receiver` 后异步等用户决定（复用插件工具的通道模式）。
//! UI 侧通过 `Engine::resolve_approval(id, decision)` 回传决定。
//!
//! - `Allow`：放行本次
//! - `Deny`：拒绝并给 AI 说明原因
//! - `AlwaysAllow`：放行本次 + 记录目标路径进总是放行集（同路径不再问）

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

/// 审批决定。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ApprovalDecision {
    Allow,
    Deny,
    AlwaysAllow,
}

impl ApprovalDecision {
    pub fn as_str(&self) -> &'static str {
        match self {
            ApprovalDecision::Allow => "allow",
            ApprovalDecision::Deny => "deny",
            ApprovalDecision::AlwaysAllow => "always-allow",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "allow" => Some(ApprovalDecision::Allow),
            "deny" => Some(ApprovalDecision::Deny),
            "always-allow" => Some(ApprovalDecision::AlwaysAllow),
            _ => None,
        }
    }
}

/// 一个待用户确认的审批请求。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalRequest {
    pub id: String,
    /// 审批所属会话（跨会话队列渲染时标识归属）
    pub session_id: String,
    /// 越权目标路径（展示给用户）
    pub target: String,
    /// 原因说明
    pub reason: String,
}

/// 审批注册表（线程安全包装在 Arc<Mutex<>> 外）。
pub struct ApprovalRegistry {
    /// 待审批请求：id → (请求信息, 响应通道)
    pending: HashMap<
        String,
        (
            ApprovalRequest,
            tokio::sync::oneshot::Sender<ApprovalDecision>,
        ),
    >,
    /// 总是放行的目标（AlwaysAllow 后记录；同一 canonical 路径不再问）
    always_allow: HashSet<String>,
}

impl Default for ApprovalRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ApprovalRegistry {
    pub fn new() -> Self {
        Self {
            pending: HashMap::new(),
            always_allow: HashSet::new(),
        }
    }

    /// 登记一个待审批请求，返回 (id, 响应通道)。
    pub fn request(
        &mut self,
        id: String,
        session_id: String,
        target: String,
        reason: String,
    ) -> tokio::sync::oneshot::Receiver<ApprovalDecision> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.pending.insert(
            id.clone(),
            (
                ApprovalRequest {
                    id,
                    session_id,
                    target: canonical_target(&target),
                    reason,
                },
                tx,
            ),
        );
        rx
    }

    /// 检查目标路径是否已在"总是放行"集合（两侧都规范化：
    /// 大小写/分隔符变体不得重复弹卡）。
    pub fn should_skip(&self, target: &str) -> bool {
        self.always_allow.contains(&canonical_target(target))
    }

    /// 用户回传决定。
    pub fn resolve(&mut self, id: &str, decision: ApprovalDecision) -> bool {
        if let Some((req, tx)) = self.pending.remove(id) {
            // AlwaysAllow：记录目标路径（规范化）
            if decision == ApprovalDecision::AlwaysAllow {
                self.always_allow.insert(canonical_target(&req.target));
            }
            let _ = tx.send(decision);
            true
        } else {
            false
        }
    }

    /// 当前所有待审批请求（UI 渲染）。
    pub fn pending_snapshot(&self) -> Vec<ApprovalRequest> {
        self.pending
            .values()
            .map(|(r, _)| r.clone())
            .collect::<Vec<_>>()
    }

    /// 撤销/取消一个待审批请求（超时清理）。
    /// 清除某会话的全部挂起审批（会话删除时调用——悬卡点允许会唤醒
    /// 已删会话的旧回合执行真实写操作，历史安全缺陷）。
    pub fn cancel_session(&mut self, session_id: &str) {
        let stale: Vec<String> = self
            .pending
            .iter()
            .filter(|(_, (req, _))| req.session_id == session_id)
            .map(|(id, _)| id.clone())
            .collect();
        for id in stale {
            self.cancel(&id);
        }
    }

    /// 清空全部 AlwaysAllow（工作区/沙箱切换时——旧语境的放行不再适用）。
    pub fn clear_allowances(&mut self) {
        if !self.always_allow.is_empty() {
            log::info!(
                "cleared {} always-allow entries (workspace/sandbox switched)",
                self.always_allow.len()
            );
        }
        self.always_allow.clear();
    }

    pub fn cancel(&mut self, id: &str) {
        self.pending.remove(id);
    }

    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }
}

/// 审批目标的规范化键（路径变体统一：大小写不敏感 + 分隔符统一 +
/// 去尾部斜杠；目标可能是非路径文本，规范化失败时原样返回）。
fn canonical_target(t: &str) -> String {
    let trimmed = t.trim_end_matches(['/', '\\']);
    trimmed.replace('\\', "/").to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_resolve_flow() {
        let mut reg = ApprovalRegistry::new();
        let rx = reg.request(
            "a1".into(),
            "s-1".into(),
            "C:\\Outside".into(),
            "写工作区外".into(),
        );
        assert_eq!(reg.pending_count(), 1);
        assert!(!reg.should_skip("C:\\outside"));

        // resolve allow
        assert!(reg.resolve("a1", ApprovalDecision::Allow));
        let rt = tokio::runtime::Runtime::new().unwrap();
        let d = rt.block_on(rx).unwrap();
        assert!(matches!(d, ApprovalDecision::Allow));
        assert_eq!(reg.pending_count(), 0);
    }

    #[test]
    fn always_allow_records_target() {
        let mut reg = ApprovalRegistry::new();
        let _rx = reg.request("a2".into(), "s-1".into(), "C:\\Sys".into(), "敏感".into());
        reg.resolve("a2", ApprovalDecision::AlwaysAllow);
        // 大小写/分隔符/尾斜杠变体不再重复弹卡
        assert!(reg.should_skip("c:/sys"));
        assert!(reg.should_skip("C:\\Sys\\"));
    }

    #[test]
    fn resolve_unknown_id_false() {
        let mut reg = ApprovalRegistry::new();
        assert!(!reg.resolve("nope", ApprovalDecision::Deny));
    }

    #[test]
    fn decision_str_roundtrip() {
        assert_eq!(
            ApprovalDecision::parse("allow"),
            Some(ApprovalDecision::Allow)
        );
        assert_eq!(
            ApprovalDecision::parse("deny"),
            Some(ApprovalDecision::Deny)
        );
        assert_eq!(
            ApprovalDecision::parse("always-allow"),
            Some(ApprovalDecision::AlwaysAllow)
        );
        assert_eq!(ApprovalDecision::parse("x"), None);
    }
}
