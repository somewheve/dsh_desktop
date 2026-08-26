//! Workflow 编排：阶段 + 并行扇出（对齐 dsh-workflow）。
//!
//! workflow/run-start、tool-workflow/agent-start 等事件词汇对齐 DSH。

use std::collections::HashMap;

use crate::core::session::{types, SessionEvent};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkflowPhase {
    Pending,
    Running,
    Done,
    Failed,
}

impl WorkflowPhase {
    pub fn as_str(&self) -> &'static str {
        match self {
            WorkflowPhase::Pending => "pending",
            WorkflowPhase::Running => "running",
            WorkflowPhase::Done => "done",
            WorkflowPhase::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone)]
pub struct WorkflowRun {
    pub id: String,
    pub name: String,
    pub phase: WorkflowPhase,
    pub items: Vec<String>,
    pub results: HashMap<String, String>,
}

#[derive(Debug, Default)]
pub struct WorkflowManager {
    runs: HashMap<String, WorkflowRun>,
}

impl WorkflowManager {
    pub fn create(&mut self, id: &str, name: &str, items: Vec<String>) -> &WorkflowRun {
        self.runs.insert(
            id.to_string(),
            WorkflowRun {
                id: id.to_string(),
                name: name.to_string(),
                phase: WorkflowPhase::Pending,
                items,
                results: HashMap::new(),
            },
        );
        self.runs.get(id).unwrap()
    }

    pub fn start(&mut self, id: &str) {
        if let Some(r) = self.runs.get_mut(id) {
            r.phase = WorkflowPhase::Running;
        }
    }

    pub fn set_result(&mut self, id: &str, item: &str, result: String) {
        if let Some(r) = self.runs.get_mut(id) {
            r.results.insert(item.to_string(), result);
        }
    }

    pub fn finish(&mut self, id: &str, ok: bool) {
        if let Some(r) = self.runs.get_mut(id) {
            r.phase = if ok {
                WorkflowPhase::Done
            } else {
                WorkflowPhase::Failed
            };
        }
    }

    pub fn get(&self, id: &str) -> Option<&WorkflowRun> {
        self.runs.get(id)
    }

    /// 事件投影：workflow/run-start / run-end
    pub fn run_start_event(id: &str, name: &str) -> SessionEvent {
        SessionEvent::new(
            types::WORKFLOW_RUN_START,
            Some(serde_json::json!({"runId": id, "name": name})),
        )
    }
    pub fn run_end_event(id: &str, ok: bool) -> SessionEvent {
        SessionEvent::new(
            types::WORKFLOW_RUN_END,
            Some(serde_json::json!({"runId": id, "ok": ok})),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workflow_lifecycle() {
        let mut mgr = WorkflowManager::default();
        mgr.create("w1", "test", vec!["a".into(), "b".into()]);
        mgr.start("w1");
        mgr.set_result("w1", "a", "ok-a".into());
        mgr.set_result("w1", "b", "ok-b".into());
        mgr.finish("w1", true);
        let r = mgr.get("w1").unwrap();
        assert_eq!(r.phase, WorkflowPhase::Done);
        assert_eq!(r.results.len(), 2);
        assert_eq!(r.results["a"], "ok-a");
    }
}
