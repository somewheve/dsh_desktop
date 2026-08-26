//! 目标系统：create/edit/pause/resume/complete/block + 阶段 fold（对齐 dsh-goal）。
//!
//! 目标变更以 goal/change 事件写入会话日志，重放 fold 恢复状态。
//! 阶段：active / paused / blocked / complete（对齐 PHASES）。

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::core::session::{types, SessionEvent};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GoalPhase {
    Active,
    Paused,
    Blocked,
    Complete,
}

impl GoalPhase {
    pub fn as_str(&self) -> &'static str {
        match self {
            GoalPhase::Active => "active",
            GoalPhase::Paused => "paused",
            GoalPhase::Blocked => "blocked",
            GoalPhase::Complete => "complete",
        }
    }
}

/// 目标（内存模型，由 goal/change 事件 fold 而来）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Goal {
    pub id: String,
    pub objective: String,
    pub phase: GoalPhase,
    pub created_at: f64,
    pub updated_at: f64,
    pub round: u64,
}

/// 目标操作（对齐 SNAPSHOT_OPERATIONS）。
#[derive(Debug, Clone, Copy)]
pub enum GoalOp {
    Create,
    Edit,
    Pause,
    Resume,
    Complete,
    Block,
}

impl GoalOp {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "create" => Some(GoalOp::Create),
            "edit" => Some(GoalOp::Edit),
            "pause" => Some(GoalOp::Pause),
            "resume" => Some(GoalOp::Resume),
            "complete" => Some(GoalOp::Complete),
            "block" => Some(GoalOp::Block),
            _ => None,
        }
    }
    pub fn as_str(&self) -> &'static str {
        match self {
            GoalOp::Create => "create",
            GoalOp::Edit => "edit",
            GoalOp::Pause => "pause",
            GoalOp::Resume => "resume",
            GoalOp::Complete => "complete",
            GoalOp::Block => "block",
        }
    }
}

/// 目标事件数据（goal/change 的 data 形状）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoalChangeData {
    pub version: u8,
    pub id: String,
    pub op: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub objective: Option<String>,
}

/// 构造 goal/change 事件。
pub fn goal_change_event(op: GoalOp, id: &str, objective: Option<&str>) -> SessionEvent {
    SessionEvent::new(
        types::GOAL_CHANGE,
        Some(serde_json::json!({
            "version": 1,
            "id": id,
            "op": op.as_str(),
            "objective": objective,
        })),
    )
}

/// 目标管理器。
#[derive(Debug, Default)]
pub struct GoalManager {
    goals: HashMap<String, Goal>,
}

impl GoalManager {
    /// 应用一个 goal/change 事件（fold）。
    pub fn apply(&mut self, ev: &SessionEvent) -> Option<Goal> {
        if ev.r#type != types::GOAL_CHANGE {
            return None;
        }
        let data: GoalChangeData = serde_json::from_value(ev.data.clone()?).ok()?;
        let op = GoalOp::parse(&data.op)?;
        let now = ev.time;
        match op {
            GoalOp::Create => {
                if self.goals.contains_key(&data.id) {
                    log::warn!("goal create rejected: id {} 已存在", data.id);
                    return None;
                }
                let goal = Goal {
                    id: data.id,
                    objective: data.objective.unwrap_or_default(),
                    phase: GoalPhase::Active,
                    created_at: now,
                    updated_at: now,
                    round: 0,
                };
                self.goals.insert(goal.id.clone(), goal.clone());
                Some(goal)
            }
            GoalOp::Edit => {
                if let Some(g) = self.goals.get_mut(&data.id) {
                    if let Some(obj) = data.objective {
                        g.objective = obj;
                    }
                    g.updated_at = now;
                    Some(g.clone())
                } else {
                    None
                }
            }
            GoalOp::Pause => self.set_phase(&data.id, GoalPhase::Paused, now),
            GoalOp::Resume => self.set_phase(&data.id, GoalPhase::Active, now),
            GoalOp::Complete => self.set_phase(&data.id, GoalPhase::Complete, now),
            GoalOp::Block => self.set_phase(&data.id, GoalPhase::Blocked, now),
        }
    }

    fn set_phase(&mut self, id: &str, phase: GoalPhase, now: f64) -> Option<Goal> {
        if let Some(g) = self.goals.get_mut(id) {
            g.phase = phase;
            g.updated_at = now;
            Some(g.clone())
        } else {
            None
        }
    }

    pub fn get(&self, id: &str) -> Option<&Goal> {
        self.goals.get(id)
    }

    pub fn list(&self) -> Vec<&Goal> {
        let mut v: Vec<&Goal> = self.goals.values().collect();
        v.sort_by(|a, b| {
            b.updated_at
                .partial_cmp(&a.updated_at)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        v
    }

    pub fn active_count(&self) -> usize {
        self.goals
            .values()
            .filter(|g| g.phase == GoalPhase::Active)
            .count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn goal_lifecycle_fold() {
        let mut mgr = GoalManager::default();
        let ev = goal_change_event(GoalOp::Create, "g1", Some("实现全部功能"));
        mgr.apply(&ev);
        assert_eq!(mgr.get("g1").unwrap().phase, GoalPhase::Active);
        mgr.apply(&goal_change_event(GoalOp::Pause, "g1", None));
        assert_eq!(mgr.get("g1").unwrap().phase, GoalPhase::Paused);
        mgr.apply(&goal_change_event(GoalOp::Resume, "g1", None));
        assert_eq!(mgr.get("g1").unwrap().phase, GoalPhase::Active);
        mgr.apply(&goal_change_event(GoalOp::Complete, "g1", None));
        assert_eq!(mgr.get("g1").unwrap().phase, GoalPhase::Complete);
        assert_eq!(mgr.active_count(), 0);
    }

    #[test]
    fn unknown_op_ignored() {
        let mut mgr = GoalManager::default();
        let mut ev = goal_change_event(GoalOp::Create, "g1", Some("x"));
        ev.data = Some(serde_json::json!({"version": 1, "id": "g1", "op": "bogus"}));
        assert!(mgr.apply(&ev).is_none());
    }
}
