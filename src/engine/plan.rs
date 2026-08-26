//! Plan mode：plan/mode 事件 fold + exit_plan_mode 工具语义（对齐 dsh-plan-mode）。

use crate::core::session::{types, SessionEvent};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanMode {
    Inactive,
    Active,
}

impl PlanMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            PlanMode::Inactive => "inactive",
            PlanMode::Active => "active",
        }
    }
}

/// 从会话事件 fold plan mode（最后一个 plan/mode 生效）。
pub fn fold_plan_mode(events: &[SessionEvent]) -> PlanMode {
    fold_plan_state(events).0
}

/// fold 计划状态：(模式, 最近一次计划内容)。
/// plan/mode 事件 data 支持 {"state": "active|inactive", "content": "..."}。
pub fn fold_plan_state(events: &[SessionEvent]) -> (PlanMode, String) {
    let mut mode = PlanMode::Inactive;
    let mut content = String::new();
    for ev in events {
        if ev.r#type == types::PLAN_MODE {
            if let Some(data) = &ev.data {
                if let Some(state) = data.get("state").and_then(|s| s.as_str()) {
                    mode = if state == "active" {
                        PlanMode::Active
                    } else {
                        PlanMode::Inactive
                    };
                }
                if let Some(c) = data.get("content").and_then(|c| c.as_str()) {
                    content = c.to_string();
                }
            }
        }
    }
    (mode, content)
}

/// 构造 plan/mode 事件。
pub fn plan_mode_event(state: PlanMode) -> SessionEvent {
    SessionEvent::new(
        types::PLAN_MODE,
        Some(serde_json::json!({"state": state.as_str()})),
    )
}

/// 构造携带计划内容的 plan/mode 事件（进入计划模式 + 计划文本）。
pub fn plan_content_event(content: &str) -> SessionEvent {
    SessionEvent::new(
        types::PLAN_MODE,
        Some(serde_json::json!({"state": "active", "content": content})),
    )
}

/// 构造退出计划模式事件。
pub fn plan_exit_event() -> SessionEvent {
    plan_mode_event(PlanMode::Inactive)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fold_last_wins() {
        let events = vec![
            plan_mode_event(PlanMode::Active),
            plan_mode_event(PlanMode::Inactive),
            plan_mode_event(PlanMode::Active),
        ];
        assert_eq!(fold_plan_mode(&events), PlanMode::Active);
        let events2 = vec![
            plan_mode_event(PlanMode::Active),
            plan_mode_event(PlanMode::Inactive),
        ];
        assert_eq!(fold_plan_mode(&events2), PlanMode::Inactive);
        assert_eq!(fold_plan_mode(&[]), PlanMode::Inactive);
    }

    /// 计划内容随 plan/mode 事件 fold（plan_write 后卡片可恢复）。
    #[test]
    fn fold_plan_state_keeps_content() {
        let events = vec![
            plan_content_event("1. 分析需求\n2. 实现\n3. 测试"),
            plan_mode_event(PlanMode::Inactive),
        ];
        // 退出计划模式后内容保留（卡片仍可显示最近计划）
        let (mode, content) = fold_plan_state(&events);
        assert_eq!(mode, PlanMode::Inactive);
        assert_eq!(content, "1. 分析需求\n2. 实现\n3. 测试");
        // 空事件：默认无计划
        let (m2, c2) = fold_plan_state(&[]);
        assert_eq!(m2, PlanMode::Inactive);
        assert!(c2.is_empty());
    }
}
