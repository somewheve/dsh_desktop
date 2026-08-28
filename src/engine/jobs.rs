//! 后台任务：JobManager（对齐 dsh-jobs + dsh-jobs-local 语义）。
//!
//! 任务生命周期：pending -> running -> done | failed
//! 任务注册 agent 回合、子代理、workflow 等耗时操作。

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum JobStatus {
    Pending,
    Running,
    Done,
    Failed,
}

impl JobStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            JobStatus::Pending => "pending",
            JobStatus::Running => "running",
            JobStatus::Done => "done",
            JobStatus::Failed => "failed",
        }
    }
}

/// 一个后台任务。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    pub id: String,
    pub name: String,
    pub status: JobStatus,
    pub created_at: f64,
    pub updated_at: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
}

fn now_ts() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

/// 任务管理器。
#[derive(Debug, Default)]
pub struct JobManager {
    jobs: HashMap<String, Job>,
    order: Vec<String>,
}

impl JobManager {
    /// 创建任务，返回 id。
    pub fn create(&mut self, name: &str, detail: Option<String>) -> String {
        let id = format!("job-{}", simple_id());
        let now = now_ts();
        self.jobs.insert(
            id.clone(),
            Job {
                id: id.clone(),
                name: name.to_string(),
                status: JobStatus::Pending,
                created_at: now,
                updated_at: now,
                detail,
                result: None,
            },
        );
        self.order.push(id.clone());
        id
    }

    pub fn start(&mut self, id: &str) {
        if let Some(j) = self.jobs.get_mut(id) {
            if j.status != JobStatus::Pending {
                log::warn!("job {id} start rejected: 当前状态 {:?}", j.status);
                return;
            }
            j.status = JobStatus::Running;
            j.updated_at = now_ts();
        }
    }

    pub fn progress(&mut self, id: &str, detail: String) {
        if let Some(j) = self.jobs.get_mut(id) {
            j.detail = Some(detail);
            j.updated_at = now_ts();
        }
    }

    pub fn finish(&mut self, id: &str, ok: bool, result: Option<String>) {
        if let Some(j) = self.jobs.get_mut(id) {
            // 允许 Pending/Running 收尾（创建即完成也合法）；禁止已终态重复 finish
            if matches!(j.status, JobStatus::Done | JobStatus::Failed) {
                log::warn!("job {id} finish rejected: 当前状态 {:?}", j.status);
                return;
            }
            j.status = if ok {
                JobStatus::Done
            } else {
                JobStatus::Failed
            };
            j.result = result;
            j.updated_at = now_ts();
        }
    }

    pub fn get(&self, id: &str) -> Option<&Job> {
        self.jobs.get(id)
    }

    /// 列表（新→旧）。
    pub fn list(&self) -> Vec<&Job> {
        self.order
            .iter()
            .rev()
            .filter_map(|id| self.jobs.get(id))
            .collect()
    }

    pub fn remove(&mut self, id: &str) {
        self.jobs.remove(id);
        self.order.retain(|x| x != id);
    }

    pub fn clear_finished(&mut self) {
        let finished: Vec<String> = self
            .jobs
            .iter()
            .filter(|(_, j)| matches!(j.status, JobStatus::Done | JobStatus::Failed))
            .map(|(id, _)| id.clone())
            .collect();
        for id in finished {
            self.remove(&id);
        }
    }
}

fn simple_id() -> String {
    // 时间戳 + 单调计数器：同一时钟 tick 内连续创建也不会碰撞
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let c = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("{nanos:x}{c:x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn job_lifecycle() {
        let mut mgr = JobManager::default();
        let id = mgr.create("agent turn", None);
        assert_eq!(mgr.get(&id).unwrap().status, JobStatus::Pending);
        mgr.start(&id);
        mgr.progress(&id, "调用 LLM".into());
        mgr.finish(&id, true, Some("done".into()));
        let job = mgr.get(&id).unwrap();
        assert_eq!(job.status, JobStatus::Done);
        assert_eq!(job.result.as_deref(), Some("done"));
        // 新任务排前面
        let id2 = mgr.create("second", None);
        assert_eq!(mgr.list()[0].id, id2);
    }

    #[test]
    fn clear_finished() {
        let mut mgr = JobManager::default();
        let a = mgr.create("a", None);
        let b = mgr.create("b", None);
        mgr.finish(&a, true, None);
        mgr.clear_finished();
        assert!(mgr.get(&a).is_none());
        assert!(mgr.get(&b).is_some());
    }
}
