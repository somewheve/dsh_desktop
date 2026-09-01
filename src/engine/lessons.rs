//! 工具调用教训库（tool lessons）：失败自动记忆 → 修正配对 → 下次直跑修正版。
//!
//! 机制（三层）：
//! 1. **记录**：任何工具调用失败（ok=false）→ 记录 (工具, 规范化参数, 错误)。
//! 2. **配对**：之后同工具的**相似**成功调用 → 把成功参数记为该教训的
//!    "已验证修正方案"（相似度 = 规范化参数的 token Jaccard ≥ 0.6）。
//! 3. **复用**：
//!    - 每回合把当前工作区的活跃教训注入系统提示（AI 直接跑修正版）；
//!    - 分发前拦截：与已失败教训**完全相同**的调用且失败 ≥2 次（或已有
//!      修正方案）→ 不再执行，直接返回"之前失败过 + 修正方案"的提示
//!      （省一次注定失败的执行，也打断死循环）。
//!
//! 持久化到 `<data_dir>/tool_lessons.json`（原子写），按工作区路径分域
//! （不同项目的路径/环境教训互不污染）。单工作区上限 50 条（LRU 淘汰）。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// 单条教训。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Lesson {
    /// 工作区根（分域键）
    pub workspace: String,
    pub tool: String,
    /// 规范化参数（递归排序 key 的紧凑 JSON）——精确匹配拦截用
    pub args_canon: String,
    /// 参数预览（截断，给人/模型看）
    pub args_preview: String,
    /// 最近一次错误摘要
    pub error: String,
    /// 已验证可用的修正参数（相似成功调用配对而来）
    pub fix: Option<String>,
    /// 相同调用失败次数
    pub count: u32,
    /// 最近时间戳（unix 秒）
    pub last_ts: f64,
}

/// 是否为治理类错误（权限拦截 / 审批拒绝 / 预设白名单）。
/// 这类"失败"不是工具用法错误——用户切换权限模式或审批后同一调用是合法的，
/// 记入教训库会在 blocked_hint 里把它永久拦死（逻辑链缺陷）。
pub fn is_governance_error(err: &str) -> bool {
    let e = err.trim_start();
    e.starts_with("工作区外路径禁止访问")
        || e.starts_with("仅工作区模式下拒绝")
        || e.starts_with("已拒绝权限审批")
        || e.starts_with("权限审批失败")
        || e.starts_with("sandbox:")
        || e.starts_with("沙箱（")
        || e.starts_with("在当前预设")
        || e.contains("在当前预设（")
}

const MAX_LESSONS_PER_WS: usize = 50;
const PREVIEW_MAX: usize = 160;
const ERROR_MAX: usize = 200;
/// 配对相似度阈值（token Jaccard）
const PAIR_SIM: f64 = 0.6;

/// 教训库。线程安全由外层 `Arc<Mutex<LessonStore>>` 保证。
#[derive(Debug, Default)]
pub struct LessonStore {
    path: Option<PathBuf>,
    lessons: Vec<Lesson>,
}

/// 递归把 JSON 对象 key 排序（BTreeMap 天然有序），得到与书写顺序无关的
/// 规范化串——同一调用的参数顺序不同也算"完全相同"。
fn canonicalize(v: &serde_json::Value) -> serde_json::Value {
    use serde_json::Value;
    match v {
        Value::Object(map) => {
            let sorted: BTreeMap<String, Value> = map
                .iter()
                .map(|(k, v)| (k.clone(), canonicalize(v)))
                .collect();
            Value::Object(sorted.into_iter().collect())
        }
        Value::Array(a) => Value::Array(a.iter().map(canonicalize).collect()),
        other => other.clone(),
    }
}

pub fn canonical_args(args: &serde_json::Value) -> String {
    canonicalize(args).to_string()
}

fn preview(canon: &str) -> String {
    let mut p: String = canon.chars().take(PREVIEW_MAX).collect();
    if canon.chars().count() > PREVIEW_MAX {
        p.push('…');
    }
    p
}

fn clip(s: &str, max: usize) -> String {
    let mut out: String = s.chars().take(max).collect();
    if s.chars().count() > max {
        out.push('…');
    }
    out
}

/// token Jaccard 相似度。按**非字母数字**切词（而非空白——规范化 JSON 是
/// 一整块无空白的串，空白切词永远 0 相似度）；路径/标识符自然拆成
/// path/src/main/rs 这样的 token，拼写修正类配对（mian→main）可达阈值。
fn similarity(a: &str, b: &str) -> f64 {
    let split = |s: &str| -> std::collections::HashSet<String> {
        s.to_lowercase()
            .split(|c: char| !c.is_alphanumeric())
            .filter(|t| !t.is_empty())
            .map(str::to_string)
            .collect()
    };
    let ta = split(a);
    let tb = split(b);
    if ta.is_empty() && tb.is_empty() {
        return 1.0;
    }
    let inter = ta.intersection(&tb).count() as f64;
    let union = ta.union(&tb).count() as f64;
    if union == 0.0 {
        0.0
    } else {
        inter / union
    }
}

fn now_ts() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

impl LessonStore {
    /// 从文件加载（不存在 → 空库；损坏 → 告警后从空库开始，不阻塞引擎）。
    pub fn load(path: PathBuf) -> Self {
        let lessons = match std::fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice::<Vec<Lesson>>(&bytes) {
                Ok(v) => v,
                Err(_) => {
                    log::warn!("tool_lessons.json 解析失败，从空教训库开始");
                    Vec::new()
                }
            },
            Err(_) => Vec::new(),
        };
        Self {
            path: Some(path),
            lessons,
        }
    }

    pub fn in_memory() -> Self {
        Self::default()
    }

    fn save(&self) {
        let Some(path) = &self.path else { return };
        let tmp = path.with_extension("json.tmp");
        match serde_json::to_vec_pretty(&self.lessons) {
            Ok(bytes) => {
                if let Err(e) = std::fs::write(&tmp, bytes)
                    .and_then(|_| std::fs::rename(&tmp, path))
                {
                    log::warn!("tool_lessons 保存失败: {e}");
                }
            }
            Err(e) => log::warn!("tool_lessons 序列化失败: {e}"),
        }
    }

    fn idx_exact(&self, ws: &str, tool: &str, canon: &str) -> Option<usize> {
        self.lessons
            .iter()
            .position(|l| l.workspace == ws && l.tool == tool && l.args_canon == canon)
    }

    /// 记录一次失败：相同调用累加 count 并刷新错误；新失败新建条目。
    pub fn record_failure(
        &mut self,
        ws: &str,
        tool: &str,
        args: &serde_json::Value,
        error: &str,
    ) {
        let canon = canonical_args(args);
        let err = clip(error.trim(), ERROR_MAX);
        match self.idx_exact(ws, tool, &canon) {
            Some(i) => {
                let l = &mut self.lessons[i];
                l.count += 1;
                l.error = err;
                l.last_ts = now_ts();
                // 失败重演说明之前的 fix 配对可能是巧合——清掉重配
                l.fix = None;
            }
            None => {
                self.lessons.push(Lesson {
                    workspace: ws.to_string(),
                    tool: tool.to_string(),
                    args_preview: preview(&canon),
                    args_canon: canon,
                    error: err,
                    fix: None,
                    count: 1,
                    last_ts: now_ts(),
                });
                self.prune(ws);
            }
        }
        self.save();
    }

    /// 记录一次成功：与已失败调用完全相同（瞬时故障自愈）→ 教训已吸收，
    /// 删除；同工具**相似**的失败教训（≥ PAIR_SIM）→ 配对为修正方案。
    pub fn record_success(
        &mut self,
        ws: &str,
        tool: &str,
        args: &serde_json::Value,
    ) {
        let canon = canonical_args(args);
        let before = self.lessons.len();
        self.lessons
            .retain(|l| !(l.workspace == ws && l.tool == tool && l.args_canon == canon));
        let mut changed = self.lessons.len() != before;
        // 对最相似且无 fix 的教训配对修正方案
        if let Some(i) = self
            .lessons
            .iter()
            .enumerate()
            .filter(|(_, l)| {
                l.workspace == ws
                    && l.tool == tool
                    && l.fix.is_none()
                    && similarity(&l.args_canon, &canon) >= PAIR_SIM
            })
            .max_by(|a, b| {
                similarity(&a.1.args_canon, &canon)
                    .partial_cmp(&similarity(&b.1.args_canon, &canon))
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .map(|(i, _)| i)
        {
            self.lessons[i].fix = Some(preview(&canon));
            changed = true;
        }
        if changed {
            self.save();
        }
    }

    /// 分发前拦截判定：与已失败教训完全相同的调用——
    /// 已有修正方案（fix）→ 拦截并给修正版（模型下一轮直跑）；
    /// 失败 ≥4 次 → 拦截（明确死循环）；
    /// 其余不拦（历史缺陷：count≥2 即拦，把"同命令修完文件重试"的合法
    /// 状态依赖型重试也拦死——bash/测试类命令参数不变、变的是文件）。
    /// 失败信息始终经 prompt_block 注入系统提示，模型有机会自主换路。
    pub fn blocked_hint(
        &self,
        ws: &str,
        tool: &str,
        args: &serde_json::Value,
    ) -> Option<String> {
        let canon = canonical_args(args);
        let l = self
            .lessons
            .iter()
            .find(|l| l.workspace == ws && l.tool == tool && l.args_canon == canon)?;
        if l.count < 4 && l.fix.is_none() {
            return None;
        }
        let mut msg = format!(
            "此调用之前已失败 {} 次，不再重复执行。上次错误：{}\n",
            l.count, l.error
        );
        if let Some(fix) = &l.fix {
            msg.push_str(&format!(
                "已验证可用的修正参数：{fix}\n请直接使用修正后的调用。"
            ));
        } else {
            msg.push_str(
                "请更换思路（修改参数/换工具/先检查目标是否存在），不要原样重试。",
            );
        }
        Some(msg)
    }

    /// 系统提示注入块：当前工作区最近 N 条教训（有修正方案的优先）。
    /// 返回 None 表示无教训。
    pub fn prompt_block(&self, ws: &str, n: usize) -> Option<String> {
        let mut items: Vec<&Lesson> = self
            .lessons
            .iter()
            .filter(|l| l.workspace == ws)
            .collect();
        if items.is_empty() {
            return None;
        }
        // 有修正的优先、其次按失败次数/新旧
        items.sort_by(|a, b| {
            b.fix.is_some()
                .cmp(&a.fix.is_some())
                .then(b.count.cmp(&a.count))
                .then(b.last_ts.partial_cmp(&a.last_ts).unwrap_or(std::cmp::Ordering::Equal))
        });
        items.truncate(n);
        let mut s = String::from(
            "Known tool pitfalls (these exact calls FAILED before — do NOT repeat them; \
             use the corrected form when given):\n",
        );
        for l in items {
            s.push_str(&format!(
                "- {} {} → failed {}x: {}",
                l.tool,
                clip(&l.args_preview, 100),
                l.count,
                clip(&l.error, 100)
            ));
            if let Some(fix) = &l.fix {
                s.push_str(&format!(" | VERIFIED FIX: {}", clip(fix, 120)));
            }
            s.push('\n');
        }
        Some(s)
    }

    /// 单工作区上限（最旧的先淘汰）。
    fn prune(&mut self, ws: &str) {
        let cnt = self.lessons.iter().filter(|l| l.workspace == ws).count();
        if cnt <= MAX_LESSONS_PER_WS {
            return;
        }
        let mut idx: Vec<(usize, f64)> = self
            .lessons
            .iter()
            .enumerate()
            .filter(|(_, l)| l.workspace == ws)
            .map(|(i, l)| (i, l.last_ts))
            .collect();
        idx.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
        let drop = cnt - MAX_LESSONS_PER_WS;
        let drop_ids: std::collections::HashSet<usize> =
            idx.into_iter().take(drop).map(|(i, _)| i).collect();
        let mut i = 0;
        self.lessons.retain(|_| {
            let keep = !drop_ids.contains(&i);
            i += 1;
            keep
        });
    }

    pub fn len(&self) -> usize {
        self.lessons.len()
    }

    pub fn is_empty(&self) -> bool {
        self.lessons.is_empty()
    }

    /// 当前工作区全部教训（测试/诊断用）。
    pub fn lessons_for(&self, ws: &str) -> Vec<&Lesson> {
        self.lessons.iter().filter(|l| l.workspace == ws).collect()
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn store() -> LessonStore {
        LessonStore::in_memory()
    }

    #[test]
    fn governance_errors_are_recognized() {
        assert!(is_governance_error("工作区外路径禁止访问（仅工作区模式）：D:/x"));
        assert!(is_governance_error("仅工作区模式下拒绝越区写：..."));
        assert!(is_governance_error("已拒绝权限审批：目标不在工作区内"));
        assert!(!is_governance_error("file not found (os error 2)"));
        assert!(!is_governance_error("命令执行失败: exit 1"));
    }

    #[test]
    fn canonical_is_order_insensitive() {
        let a = canonical_args(&json!({"path": "x.txt", "mode": "r"}));
        let b = canonical_args(&json!({"mode": "r", "path": "x.txt"}));
        assert_eq!(a, b);
        // 嵌套对象也排序
        let c = canonical_args(&json!({"o": {"b": 1, "a": 2}}));
        let d = canonical_args(&json!({"o": {"a": 2, "b": 1}}));
        assert_eq!(c, d);
    }

    #[test]
    fn failure_count_and_block() {
        let mut s = store();
        let args = json!({"path": "no-such.txt"});
        s.record_failure("ws", "read_file", &args, "file not found");
        // 失败 1 次无修正：不拦截（允许重试）
        assert!(s.blocked_hint("ws", "read_file", &args).is_none());
        s.record_failure("ws", "read_file", &args, "file not found");
        // 失败 2 次且无修正：不拦（状态依赖型重试合法——同命令可能修完文件再试）
        assert!(
            s.blocked_hint("ws", "read_file", &args).is_none(),
            "count=2 无 fix 不应拦截"
        );
        // 失败 4 次：明确死循环，拦截
        s.record_failure("ws", "read_file", &args, "file not found");
        s.record_failure("ws", "read_file", &args, "file not found");
        let hint = s.blocked_hint("ws", "read_file", &args).unwrap();
        assert!(hint.contains("4 次"), "{hint}");
        assert!(hint.contains("不要原样重试"));
        // 别的工作区不受影响
        assert!(s.blocked_hint("ws2", "read_file", &args).is_none());
    }

    #[test]
    fn success_pairs_fix_then_exact_success_clears() {
        let mut s = store();
        let bad = json!({"path": "src/mian.rs"});
        s.record_failure("ws", "read_file", &bad, "not found");
        // 相似成功（路径拼写修正）→ 配对为 fix
        let good = json!({"path": "src/main.rs"});
        s.record_success("ws", "read_file", &good);
        let l = &s.lessons_for("ws")[0];
        assert!(l.fix.as_deref().unwrap().contains("main.rs"));
        // 有修正后：完全相同的失败调用失败 1 次也拦截（直接给修正版）
        let hint = s.blocked_hint("ws", "read_file", &bad).unwrap();
        assert!(hint.contains("VERIFIED FIX") || hint.contains("修正参数"), "{hint}");
        // 提示注入包含修正
        let block = s.prompt_block("ws", 5).unwrap();
        assert!(block.contains("VERIFIED FIX"), "{block}");
        // 原失败调用之后成功了 → 教训吸收删除
        s.record_success("ws", "read_file", &bad);
        assert!(s.lessons_for("ws").is_empty());
    }

    #[test]
    fn prompt_block_empty_and_cap() {
        let s = store();
        assert!(s.prompt_block("ws", 5).is_none());
        let mut s = store();
        for i in 0..8 {
            s.record_failure(
                "ws",
                "bash",
                &json!({"command": format!("cmd-{i}")}),
                "boom",
            );
        }
        assert!(s.prompt_block("ws", 5).unwrap().lines().count() <= 6);
    }

    #[test]
    fn persist_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("tool_lessons.json");
        {
            let mut s = LessonStore::load(p.clone());
            s.record_failure("ws", "bash", &json!({"command": "ls"}), "err");
        }
        let s2 = LessonStore::load(p);
        assert_eq!(s2.len(), 1);
        assert_eq!(s2.lessons_for("ws")[0].tool, "bash");
    }

    #[test]
    fn prune_cap_per_workspace() {
        let mut s = store();
        for i in 0..(MAX_LESSONS_PER_WS + 10) {
            s.record_failure(
                "ws",
                "bash",
                &json!({"command": format!("c{i}")}),
                "e",
            );
        }
        assert_eq!(s.lessons_for("ws").len(), MAX_LESSONS_PER_WS);
        // 其他工作区不受影响
        s.record_failure("other", "bash", &json!({"command": "x"}), "e");
        assert_eq!(s.lessons_for("other").len(), 1);
    }
}
