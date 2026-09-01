//! 行级 diff（供工具结果的"改动可视"用）：
//! `write_file` 覆盖前捕获旧内容、`str_replace_editor` 的 old/new 片段，
//! 生成 `-old/+new` 行流，UI 按前缀着色渲染成 diff 卡片。
//!
//! 算法：经典 LCS 动态规划（O(n·m)），配合规模护栏——超大文件
//! （乘积 > 4M 单元）不做逐行 diff，返回 None 由调用方降级为
//! "整文件重写 N → M 行"的摘要。编码 agent 的常规编辑都在几百行内，
//! LCS 足够快且无依赖。

/// 单条 diff 上限（行）。超出截断并追加省略行。
pub const MAX_DIFF_LINES: usize = 200;

/// LCS 规模护栏：old/new 行数乘积超过此值放弃逐行 diff。
const LCS_CELL_CAP: usize = 4_000_000;

/// 生成 `-old / +new / 空格=上下文` 的行流。
/// 无差异返回空 String；超规模返回 None。
pub fn line_diff(old: &str, new: &str) -> Option<String> {
    let a: Vec<&str> = old.lines().collect();
    let b: Vec<&str> = new.lines().collect();
    if a == b {
        return Some(String::new());
    }
    if a.len().saturating_mul(b.len()) > LCS_CELL_CAP {
        return None;
    }
    // LCS 全表（规模已被护栏限制）
    let mut table = vec![vec![0usize; b.len() + 1]; a.len() + 1];
    for i in 1..=a.len() {
        for j in 1..=b.len() {
            table[i][j] = if a[i - 1] == b[j - 1] {
                table[i - 1][j - 1] + 1
            } else {
                table[i - 1][j].max(table[i][j - 1])
            };
        }
    }
    // 回溯产出编辑脚本（从尾部，反转）
    let mut ops: Vec<(char, &str)> = Vec::new();
    let (mut i, mut j) = (a.len(), b.len());
    while i > 0 || j > 0 {
        match (i, j) {
            (0, _) => {
                j -= 1;
                ops.push(('+', b[j]));
            }
            (_, 0) => {
                i -= 1;
                ops.push(('-', a[i]));
            }
            _ => {
                if a[i - 1] == b[j - 1] {
                    i -= 1;
                    j -= 1;
                    ops.push((' ', a[i]));
                } else if table[i - 1][j] >= table[i][j - 1] {
                    i -= 1;
                    ops.push(('-', a[i]));
                } else {
                    j -= 1;
                    ops.push(('+', b[j]));
                }
            }
        }
    }
    ops.reverse();
    Some(render_ops(&ops))
}

/// 编辑脚本 → 带上下文压缩的行流（每处变更前后保留 1 行上下文，
/// 更远的未变区间折叠为 "⋯"）。截断到 [`MAX_DIFF_LINES`]。
fn render_ops(ops: &[(char, &str)]) -> String {
    let n = ops.len();
    let mut keep = vec![false; n];
    for (k, &(op, _)) in ops.iter().enumerate() {
        if op != ' ' {
            // 变更行 + 前后各 1 行上下文
            for r in k.saturating_sub(1)..=(k + 1).min(n - 1) {
                keep[r] = true;
            }
        }
    }
    let mut out = String::new();
    let mut elided = 0usize;
    let mut emitted = 0usize;
    let mut truncated = false;
    for (k, &(op, text)) in ops.iter().enumerate() {
        if !keep[k] {
            elided += 1;
            continue;
        }
        if elided > 0 {
            if emitted >= MAX_DIFF_LINES {
                truncated = true;
                break;
            }
            out.push_str("⋯\n");
            emitted += 1;
            elided = 0;
        }
        if emitted >= MAX_DIFF_LINES {
            truncated = true;
            break;
        }
        out.push(op);
        out.push_str(text);
        out.push('\n');
        emitted += 1;
    }
    if truncated {
        out.push_str(&format!("⋯（diff 超过 {MAX_DIFF_LINES} 行已截断）\n"));
    }
    out
}

/// diff 行数统计 (删, 增)。用于卡片标题 "+a −b"。
pub fn counts(diff: &str) -> (usize, usize) {
    let mut del = 0;
    let mut add = 0;
    for line in diff.lines() {
        match line.chars().next() {
            Some('-') => del += 1,
            Some('+') => add += 1,
            _ => {}
        }
    }
    (del, add)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diff_basic_edit() {
        let d = line_diff("a\nb\nc\n", "a\nB\nc\n").unwrap();
        let (del, add) = counts(&d);
        assert_eq!((del, add), (1, 1));
        assert!(d.contains("-b"));
        assert!(d.contains("+B"));
        assert!(d.contains(" a"), "上下文行保留");
        assert!(!d.contains("c\u{0}"));
    }

    #[test]
    fn diff_insert_delete_blocks() {
        // 纯插入
        let d = line_diff("x\n", "x\ny\nz\n").unwrap();
        assert_eq!(counts(&d), (0, 2));
        // 纯删除
        let d = line_diff("x\ny\nz\n", "x\n").unwrap();
        assert_eq!(counts(&d), (2, 0));
        // 空 → 内容（create 场景）
        let d = line_diff("", "fn main() {}\n").unwrap();
        assert_eq!(counts(&d), (0, 1));
    }

    #[test]
    fn diff_identical_is_empty() {
        assert_eq!(line_diff("same\nsame\n", "same\nsame\n").unwrap(), "");
    }

    #[test]
    fn diff_context_elision() {
        // 100 行未变 + 1 行修改：中间未变区折叠
        let old: String = (0..100).map(|i| format!("line{i}\n")).collect();
        let mut new = old.clone();
        new.push_str("tail\n");
        let d = line_diff(&old, &new).unwrap();
        assert!(d.contains('⋯'), "远端上下文应折叠");
        assert!(d.lines().count() < 20, "折叠后应远小于 101 行");
    }

    #[test]
    fn diff_oversize_returns_none() {
        let big: String = (0..3000).map(|i| format!("l{i}\n")).collect();
        let mut big2 = big.clone();
        big2.push_str("x\n");
        assert!(line_diff(&big, &big2).is_none(), "3000x3001 超护栏应降级");
    }

    #[test]
    fn diff_truncates_at_cap() {
        let old = String::new();
        let new: String = (0..500).map(|i| format!("n{i}\n")).collect();
        let d = line_diff(&old, &new).unwrap();
        assert!(d.contains("已截断"), "超 200 行应截断提示");
    }
}
