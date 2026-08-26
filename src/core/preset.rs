//! Agent 预设（对齐 DSH config/agent-presets 的 4 种模式）。
//!
//! | id        | 模式     | DSH 预设目录 | 行为 |
//! |-----------|----------|--------------|------|
//! | standard  | 标准模式 | standard/    | 功能完整的编码 Agent |
//! | code      | PTC 模式 | code/        | 标准 + Code Mode（run_code 组合多步） |
//! | minimal   | 极简模式 | minimal/     | 仅 bash + str_replace_editor |
//! | cordis    | 创造模式 | cordis/      | 标准 + 运行时自省/插件实验/preset 创作 |

use serde::{Deserialize, Serialize};

/// Agent 预设。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentPreset {
    /// 标准模式：功能完整的编码 Agent（shell、文件、检索、技能、计划、目标、子代理、工作流）。
    Standard,
    /// PTC 模式：标准全部能力 + Code Mode（用 run_code 一次组合多步操作）。
    Ptc,
    /// 极简模式：仅持久 bash 与 str_replace_editor 双工具。
    Minimal,
    /// 创造模式：标准全部能力 + 运行时检查、插件实验与 preset 创作指导。
    Cordis,
}

impl Default for AgentPreset {
    fn default() -> Self {
        Self::Standard
    }
}

impl AgentPreset {
    /// preset 目录名（对齐 DSH agent-presets 目录）。
    pub fn id(self) -> &'static str {
        match self {
            Self::Standard => "standard",
            Self::Ptc => "code",
            Self::Minimal => "minimal",
            Self::Cordis => "cordis",
        }
    }

    /// 中文模式名（对齐 DSH preset.yml 的 name）。
    pub fn name(self) -> &'static str {
        match self {
            Self::Standard => "标准模式",
            Self::Ptc => "PTC 模式",
            Self::Minimal => "极简模式",
            Self::Cordis => "创造模式",
        }
    }

    /// 描述（对齐 DSH preset.yml 的 description）。
    pub fn description(self) -> &'static str {
        match self {
            Self::Standard => "功能完整的编码 Agent，支持文件编辑、Shell、文件与网页检索、Skills、计划、目标、子代理和工作流。",
            Self::Ptc => "具备标准模式的全部能力，并通过 Code Mode 呈现工具，让模型用一个程序组合多步操作。",
            Self::Minimal => "仅提供持久 bash 与 str_replace_editor 的双工具编码 Agent。",
            Self::Cordis => "用于创建自定义 Agent preset：具备标准模式的全部能力，并提供运行时检查、插件实验和 preset 创作指导。",
        }
    }

    /// 从字符串解析（接受目录名或中文名）。
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "standard" | "标准模式" | "标准" => Some(Self::Standard),
            "code" | "ptc" | "ptc 模式" | "ptc模式" => Some(Self::Ptc),
            "minimal" | "极简模式" | "极简" => Some(Self::Minimal),
            "cordis" | "creative" | "创造模式" | "创造" => Some(Self::Cordis),
            _ => None,
        }
    }

    /// 全部预设（按 DSH order）。
    pub fn all() -> [AgentPreset; 4] {
        [Self::Standard, Self::Ptc, Self::Minimal, Self::Cordis]
    }

    /// 工具白名单（None = 全部工具）。
    pub fn tool_whitelist(self) -> Option<&'static [&'static str]> {
        match self {
            Self::Minimal => Some(&["bash", "pwsh", "str_replace_editor"]),
            _ => None,
        }
    }

    /// 是否提供 run_code 组合工具（PTC 的 Code Mode）。
    pub fn has_run_code(self) -> bool {
        matches!(self, Self::Ptc)
    }

    /// 是否启用上下文压缩（minimal 无）。
    pub fn has_compaction(self) -> bool {
        !matches!(self, Self::Minimal)
    }

    /// 系统提示（persona，对齐 dsh-persona 语义）。
    pub fn persona(self, model: &str, cwd: &str) -> String {
        match self {
            Self::Minimal => "You are a helpful software engineer assistant.".to_string(),
            Self::Standard => format!(
                "You are a coding agent powered by the {model} model. Your working directory is {cwd}."
            ),
            Self::Ptc => format!(
                "You are a coding agent powered by the {model} model. Your working directory is {cwd}.

You operate in PTC (Code) mode: combine a sequence of operations into one run_code program and execute it once, so a sequence that would be five round trips becomes one. Use individual tools only when a single action is needed or run_code cannot express it."
            ),
            Self::Cordis => format!(
                "You are a coding agent powered by the {model} model, running on the DeepSeek Harness desktop build. Your working directory is {cwd}.

You can read and modify the harness you run on: every capability is a plugin in the plugin registry, and an agent preset is one such configuration mounted for a session. Load, inspect and experiment with plugins; you may author new presets. Never edit the shipped preset definitions - copy them into your own preset directory and edit the copy."
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
        assert_eq!(AgentPreset::Ptc.id(), "code");
        assert_eq!(AgentPreset::Minimal.id(), "minimal");
        assert_eq!(AgentPreset::Cordis.id(), "cordis");
        assert_eq!(AgentPreset::Standard.name(), "标准模式");
        assert_eq!(AgentPreset::Ptc.name(), "PTC 模式");
        assert_eq!(AgentPreset::Minimal.name(), "极简模式");
        assert_eq!(AgentPreset::Cordis.name(), "创造模式");
    }

    #[test]
    fn parse_variants() {
        assert_eq!(AgentPreset::parse("standard"), Some(AgentPreset::Standard));
        assert_eq!(AgentPreset::parse("code"), Some(AgentPreset::Ptc));
        assert_eq!(AgentPreset::parse("PTC"), Some(AgentPreset::Ptc));
        assert_eq!(AgentPreset::parse("极简模式"), Some(AgentPreset::Minimal));
        assert_eq!(AgentPreset::parse("创造"), Some(AgentPreset::Cordis));
        assert_eq!(AgentPreset::parse("bogus"), None);
    }

    #[test]
    fn minimal_whitelist_and_no_run_code() {
        let wl = AgentPreset::Minimal.tool_whitelist().unwrap();
        assert_eq!(wl, &["bash", "pwsh", "str_replace_editor"]);
        assert!(!AgentPreset::Minimal.has_run_code());
        assert!(AgentPreset::Ptc.has_run_code());
        assert!(AgentPreset::Standard.tool_whitelist().is_none());
        assert!(!AgentPreset::Minimal.has_compaction());
        assert!(AgentPreset::Standard.has_compaction());
    }

    #[test]
    fn personas_differ() {
        let p1 = AgentPreset::Standard.persona("deepseek-v4", "C:/work");
        assert!(p1.contains("coding agent"));
        let pm = AgentPreset::Minimal.persona("deepseek-v4", "C:/work");
        assert_eq!(pm, "You are a helpful software engineer assistant.");
        let pc = AgentPreset::Ptc.persona("deepseek-v4", "C:/work");
        assert!(pc.contains("run_code"));
        let pc2 = AgentPreset::Cordis.persona("deepseek-v4", "C:/work");
        assert!(pc2.contains("plugin"));
    }
}
