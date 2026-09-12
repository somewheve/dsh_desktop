//! 定时任务：schedule/change 事件 + 到期触发（对齐 dsh-schedule）。

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::core::session::{types, SessionEvent};

#[derive(Debug, Clone)]
pub struct ScheduledTask {
    pub id: String,
    pub name: String,
    /// 触发模式："interval"（循环间隔秒）| "daily"（每天 HH:MM）|
    /// "once"（指定时刻一次性）
    pub kind: String,
    /// interval 模式：间隔秒；daily/once 不用
    pub interval_secs: u64,
    /// daily/once 模式：本地时刻（时/分）
    pub at_hour: u32,
    pub at_min: u32,
    /// once 模式：目标日期（unix 秒，当天本地 00:00 基准）；daily 不用
    pub at_date: i64,
    /// 到期时发送到绑定会话的提示词（name 的完整版）
    pub prompt: String,
    /// 绑定会话（到期在此会话发起回合）
    pub session_id: String,
    /// 下次触发时间戳（unix 秒）
    pub next_at: f64,
    pub enabled: bool,
}

impl ScheduledTask {
    /// 一次性任务已触发（once + 禁用 + 时刻已过）。
    pub fn fired_once(&self) -> bool {
        self.kind == "once" && !self.enabled
    }
}

/// daily/once 的本地时刻 → 下一次触发的 unix 秒。
/// `day_offset`：0=今天（时刻已过则自动顺延到明天），N=从今天起 N 天后
/// （once 用具体日期）。
pub fn arm_at(hour: u32, min: u32, day_offset: i64, date: Option<i64>) -> Option<f64> {
    use chrono::{Duration, Local, TimeZone};
    let base = match date {
        Some(d) => Local
            .timestamp_opt(d, 0)
            .single()?
            .date_naive(),
        None => Local::now().date_naive() + Duration::days(day_offset),
    };
    // daily（无 date）：今天已过顺延到明天；once（有 date）：只看当天，
    // 已过即过期（返回 None 由调用方按禁用插入）
    let span = if date.is_some() { 1 } else { 2 };
    for off in 0..span {
        let day = base + Duration::days(off);
        let naive = day.and_hms_opt(hour, min, 0)?;
        let local = Local
            .from_local_datetime(&naive)
            .single()?;
        let ts = local.timestamp() as f64;
        if ts > now_ts() {
            return Some(ts);
        }
    }
    None
}

#[derive(Debug, Default)]
pub struct Scheduler {
    tasks: HashMap<String, ScheduledTask>,
}

impl Scheduler {
    pub fn add(
        &mut self,
        id: &str,
        name: &str,
        interval_secs: u64,
        prompt: &str,
        session_id: &str,
    ) {
        self.add_full(ScheduledTask {
            id: id.to_string(),
            name: name.to_string(),
            kind: "interval".into(),
            interval_secs,
            at_hour: 0,
            at_min: 0,
            at_date: 0,
            prompt: prompt.to_string(),
            session_id: session_id.to_string(),
            next_at: 0.0,
            enabled: true,
        });
    }

    /// 完整参数添加：按模式计算首次触发时刻。
    /// once 的目标时刻已过 → 仍插入但禁用（UI 显示"已过期"由用户改期）。
    pub fn add_full(&mut self, t: ScheduledTask) {
        let next = match t.kind.as_str() {
            "interval" => {
                if t.interval_secs == 0 {
                    log::warn!("schedule add rejected: interval_secs must be > 0");
                    return;
                }
                now_ts() + t.interval_secs as f64
            }
            "daily" => match arm_at(t.at_hour, t.at_min, 0, None) {
                Some(ts) => ts,
                None => return,
            },
            "once" => {
                match arm_at(t.at_hour, t.at_min, 0, Some(t.at_date)) {
                    Some(ts) => ts,
                    // 指定时刻已过：插入为禁用态（用户可见可改期）
                    None => {
                        let mut t = t;
                        t.enabled = false;
                        t.next_at = 0.0;
                        self.tasks.insert(t.id.clone(), t);
                        return;
                    }
                }
            }
            other => {
                log::warn!("schedule add rejected: unknown kind {other}");
                return;
            }
        };
        let mut t = t;
        t.next_at = next;
        self.tasks.insert(t.id.clone(), t);
    }

    /// 找出所有到期的任务并推进：interval 加一间隔；daily 跳到下一个
    /// 未来 HH:MM；once 触发后禁用（保留记录，UI 标"已触发"）。
    pub fn due(&mut self) -> Vec<String> {
        let now = now_ts();
        let mut out = Vec::new();
        for t in self.tasks.values_mut() {
            if !(t.enabled && t.next_at <= now) {
                continue;
            }
            out.push(t.id.clone());
            match t.kind.as_str() {
                "interval" => t.next_at = now + t.interval_secs as f64,
                "daily" => {
                    t.next_at = arm_at(t.at_hour, t.at_min, 1, None)
                        .unwrap_or(now + 86400.0);
                }
                "once" => {
                    t.enabled = false;
                }
                _ => {}
            }
        }
        out
    }

    /// 测试访问器。
    fn task(&self, id: &str) -> &ScheduledTask {
        self.tasks.get(id).expect("task")
    }
    fn task_mut(&mut self, id: &str) -> &mut ScheduledTask {
        self.tasks.get_mut(id).expect("task")
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

        /// once：过期插入为禁用；到期触发后禁用且不重复。
    #[test]
    fn once_fires_then_disables() {
        let mut sch = Scheduler::default();
        let mut t = task("once", 0, 0, 0, chrono::Local::now().timestamp() - 3600);
        sch.add_full(t.clone());
        assert!(!sch.list()[0].enabled, "过期 once 应插入为禁用");

        t.at_date = chrono::Local::now().timestamp() + 86400 * 2;
        t.enabled = true;
        sch.add_full(t);
        assert!(sch.list()[0].next_at > now_ts(), "未来 once 应布防");

        {
            let tm = sch.task_mut("t1");
            tm.next_at = now_ts() - 1.0;
        }
        let due = sch.due();
        assert_eq!(due, vec!["t1".to_string()]);
        assert!(!sch.task("t1").enabled, "触发后应禁用");
        assert!(sch.due().is_empty(), "禁用后不再触发");
    }

    /// daily：布防到下一个未来 HH:MM；到期后跳到下一天同一时刻且保持启用。
    #[test]
    fn daily_arms_next_occurrence() {
        let mut sch = Scheduler::default();
        sch.add_full(task("daily", 0, 23, 59, 0));
        assert!(sch.list()[0].next_at > now_ts());

        {
            let tm = sch.task_mut("t1");
            tm.next_at = now_ts() - 1.0;
        }
        assert_eq!(sch.due().len(), 1);
        let next = sch.task("t1").next_at;
        assert!(next > now_ts(), "到期后应重布防");
        assert!(next - now_ts() <= 86400.0 * 2.0, "下一天同一时刻");
        assert!(sch.task("t1").enabled, "daily 保持启用");
    }

    /// 旧配置兼容：无 kind/at_* 字段 → interval 模式。
    #[test]
    fn stored_backward_compat() {
        let json = r#"{"id":"a","name":"n","interval_secs":60,"prompt":"p","session_id":"s","enabled":true}"#;
        let t: crate::core::settings::StoredScheduledTask = serde_json::from_str(json).unwrap();
        assert_eq!(t.kind, "interval");
        assert_eq!(t.at_hour, 0);
    }

    fn task(kind: &str, interval: u64, hour: u32, min: u32, date: i64) -> ScheduledTask {
        ScheduledTask {
            id: "t1".into(),
            name: "n".into(),
            kind: kind.into(),
            interval_secs: interval,
            at_hour: hour,
            at_min: min,
            at_date: date,
            prompt: "p".into(),
            session_id: "s".into(),
            next_at: 0.0,
            enabled: true,
        }
    }

    #[test]
    fn scheduler_due() {
        use std::time::Duration;
        let mut s = Scheduler::default();
        s.add("t1", "job1", 1, "检查构建", "s-1");
        thread::sleep(Duration::from_millis(1200));
        let due = s.due();
        assert_eq!(due, vec!["t1".to_string()]);
        assert!(s.due().is_empty(), "推进后不应再到期");
    }
}
