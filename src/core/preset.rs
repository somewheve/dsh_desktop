//! Agent 预设（两种模式）。
//!
//! | id       | 模式         | 行为 |
//! |----------|--------------|------|
//! | standard | 标准模式     | 完整能力；子代理派生等自主动作需用户审批 |
//! | autoplan | 自主规划模式 | AI 自行规划/选择策略并执行（含子代理派生），不打扰用户 |
//!
//! 历史预设（PTC/code、minimal、cordis）已移除；旧会话/配置中的旧 id
//! 解析时回退 Standard。

use serde::{Deserialize, Serialize};

/// Agent 预设。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentPreset {
    /// 标准模式：完整工具集；子代理派生需审批（token 成本可见）。
    Standard,
    /// 自主规划模式：AI 自主规划与决策（自行决定何时写计划、
    /// 派生子代理、选择工具），无需用户逐步确认。
    AutoPlan,
}

impl Default for AgentPreset {
    fn default() -> Self {
        Self::Standard
    }
}

impl AgentPreset {
    pub fn id(self) -> &'static str {
        match self {
            Self::Standard => "standard",
            Self::AutoPlan => "autoplan",
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Standard => "标准模式",
            Self::AutoPlan => "自主规划",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Self::Standard => "功能完整的编码 Agent；子代理派生等自主动作需用户审批。",
            Self::AutoPlan => "AI 自行规划与决策：自主制定/更新计划、并行派生子代理、选择工具与策略，全程无需确认。",
        }
    }

    /// 从字符串解析（接受 id / 中文名；历史旧 id 回退 Standard）。
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "standard" | "标准模式" | "标准" => Some(Self::Standard),
            "autoplan" | "auto" | "自主规划" | "自主规划模式" | "自主" => Some(Self::AutoPlan),
            // 历史预设：迁移到 Standard（旧行为最接近）
            "code" | "ptc" | "minimal" | "cordis" | "creative" => Some(Self::Standard),
            _ => None,
        }
    }

    /// 全部预设。
    pub fn all() -> [AgentPreset; 2] {
        [Self::Standard, Self::AutoPlan]
    }

    /// 工具白名单（两种模式均为全部工具；保留字段供未来扩展）。
    pub fn tool_whitelist(self) -> Option<&'static [&'static str]> {
        None
    }

    /// 是否提供 run_code 组合工具（随 PTC 移除，恒 false；保留字段
    /// 避免全项目删除 dispatch 分支——历史会话重放仍可能带 run_code 步骤）。
    pub fn has_run_code(self) -> bool {
        false
    }

    /// 是否启用上下文压缩。
    pub fn has_compaction(self) -> bool {
        true
    }

    /// 子代理派生是否需要审批（标准模式 = 需要；自主规划 = AI 自行决定）。
    pub fn subagent_needs_approval(self) -> bool {
        matches!(self, Self::Standard)
    }

    /// 系统提示（persona）。
    pub fn persona(self, model: &str, cwd: &str) -> String {
        let base = format!(
            "You are a coding agent powered by the {model} model. Your working directory is {cwd}."
        );
        match self {
            Self::Standard => base,
            Self::AutoPlan => format!(
                "{base}\n\nYou operate in AUTO-PLAN mode: plan and decide autonomously. \
For complex tasks, write a plan yourself (plan_write) and keep it updated as you go, \
without waiting for user confirmation between steps. Split clearly independent work \
into parallel subagent_fork calls when it helps. Make reasonable strategy and tool \
choices yourself; only ask the user when a decision is genuinely theirs (irreversible, \
ambiguous, or preference-dependent)."
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_and_names() {
        assert_eq!(AgentPreset::Standard.id(), "standard");
        assert_eq!(AgentPreset::AutoPlan.id(), "autoplan");
        assert_eq!(AgentPreset::Standard.name(), "标准模式");
        assert_eq!(AgentPreset::AutoPlan.name(), "自主规划");
    }

    #[test]
    fn parse_variants_and_legacy_fallback() {
        assert_eq!(AgentPreset::parse("standard"), Some(AgentPreset::Standard));
        assert_eq!(AgentPreset::parse("autoplan"), Some(AgentPreset::AutoPlan));
        assert_eq!(AgentPreset::parse("自主规划"), Some(AgentPreset::AutoPlan));
        // 历史旧 id 回退 Standard（旧会话/配置不炸）
        assert_eq!(AgentPreset::parse("code"), Some(AgentPreset::Standard));
        assert_eq!(AgentPreset::parse("PTC"), Some(AgentPreset::Standard));
        assert_eq!(AgentPreset::parse("minimal"), Some(AgentPreset::Standard));
        assert_eq!(AgentPreset::parse("cordis"), Some(AgentPreset::Standard));
        assert_eq!(AgentPreset::parse("bogus"), None);
    }

    #[test]
    fn approval_and_tools_matrix() {
        // 标准模式：子代理要审批；自主规划：免审
        assert!(AgentPreset::Standard.subagent_needs_approval());
        assert!(!AgentPreset::AutoPlan.subagent_needs_approval());
        // 两模式全工具、无 run_code、均有压缩
        for p in AgentPreset::all() {
            assert!(p.tool_whitelist().is_none());
            assert!(!p.has_run_code());
            assert!(p.has_compaction());
        }
    }

    #[test]
    fn personas_differ() {
        let p1 = AgentPreset::Standard.persona("deepseek-v4", "C:/work");
        assert!(p1.contains("coding agent"));
        let p2 = AgentPreset::AutoPlan.persona("deepseek-v4", "C:/work");
        assert!(p2.contains("AUTO-PLAN"));
        assert!(p2.contains("plan_write"));
    }
}
