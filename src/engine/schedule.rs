//! 定时任务：schedule/change 事件 + 到期触发（对齐 dsh-schedule）。

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::core::session::{types, SessionEvent};

#[derive(Debug, Clone)]
pub struct ScheduledTask {
    pub id: String,
    pub name: String,
    /// 触发间隔（秒）
    pub interval_secs: u64,
    /// 下次触发时间戳
    pub next_at: f64,
    pub enabled: bool,
}

#[derive(Debug, Default)]
pub struct Scheduler {
    tasks: HashMap<String, ScheduledTask>,
}

impl Scheduler {
    pub fn add(&mut self, id: &str, name: &str, interval_secs: u64) {
        if interval_secs == 0 {
            log::warn!("schedule add rejected: interval_secs must be > 0");
            return;
        }
        let now = now_ts();
        self.tasks.insert(
            id.to_string(),
            ScheduledTask {
                id: id.to_string(),
                name: name.to_string(),
                interval_secs,
                next_at: now + interval_secs as f64,
                enabled: true,
            },
        );
    }

    /// 找出所有到期的任务，推进 next_at 并返回。
    pub fn due(&mut self) -> Vec<String> {
        let now = now_ts();
        let mut out = Vec::new();
        for t in self.tasks.values_mut() {
            if t.enabled && t.next_at <= now {
                out.push(t.id.clone());
                t.next_at = now + t.interval_secs as f64;
            }
        }
        out
    }

    pub fn remove(&mut self, id: &str) {
        self.tasks.remove(id);
    }

    pub fn toggle(&mut self, id: &str, enabled: bool) {
        if let Some(t) = self.tasks.get_mut(id) {
            t.enabled = enabled;
        }
    }

    pub fn list(&self) -> Vec<&ScheduledTask> {
        let mut v: Vec<_> = self.tasks.values().collect();
        v.sort_by(|a, b| {
            a.next_at
                .partial_cmp(&b.next_at)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        v
    }

    pub fn change_event(id: &str, enabled: bool) -> SessionEvent {
        SessionEvent::new(
            types::SCHEDULE_CHANGE,
            Some(serde_json::json!({"id": id, "enabled": enabled})),
        )
    }
}

fn now_ts() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn scheduler_due() {
        use std::time::Duration;
        let mut s = Scheduler::default();
        s.add("t1", "job1", 1);
        thread::sleep(Duration::from_millis(1200));
        let due = s.due();
        assert_eq!(due, vec!["t1".to_string()]);
        assert!(s.due().is_empty(), "推进后不应再到期");
    }
}
