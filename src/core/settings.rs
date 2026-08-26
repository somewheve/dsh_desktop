//! 引擎设置：API key / model / base_url / proxy / 插件清单。
//!
//! 对应 DSH 的 dsh-settings + dsh-credentials + dsh-launch-environment。

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct EngineSettings {
    /// DeepSeek 官方 API key（读取自 DSH credentials 或本应用设置）
    pub api_key: Option<String>,
    /// 模型 id（默认 deepseek-chat）
    pub model: String,
    /// 推理强度（如 "high"）
    pub reasoning_effort: Option<String>,
    /// API base URL（默认官方；可指向兼容网关）
    pub base_url: String,
    /// HTTP 代理（可选）
    pub http_proxy: Option<String>,
    /// 会话数据目录（默认 %LOCALAPPDATA%/dsh-desktop/sessions）
    pub data_dir: PathBuf,
    /// 新建会话默认 Agent 预设（standard/code/minimal/cordis）
    pub default_preset: String,
    /// 用户技能目录（默认 $DSH_HOME/skills；测试可指向临时目录）
    pub skills_dir: Option<PathBuf>,
    /// 插件目录（默认 $DSH_HOME/plugins；测试可指向临时目录）
    pub plugins_dir: Option<PathBuf>,
}

impl Default for EngineSettings {
    fn default() -> Self {
        Self {
            api_key: None,
            model: "deepseek-chat".into(),
            reasoning_effort: None,
            base_url: "https://api.deepseek.com".into(),
            http_proxy: None,
            data_dir: default_data_dir(),
            default_preset: "standard".into(),
            skills_dir: None,
            plugins_dir: None,
        }
    }
}

pub fn default_data_dir() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("dsh-desktop").join("sessions")
}

impl EngineSettings {
    pub fn load() -> Self {
        let path = settings_file();
        let mut s: Self = match std::fs::read_to_string(&path) {
            Ok(text) => match serde_json::from_str(&text) {
                Ok(s) => s,
                Err(e) => {
                    log::warn!("engine settings parse failed: {e}");
                    Self::default()
                }
            },
            Err(_) => Self::default(),
        };
        // 防污染：data_dir 指向 tempfile 特征目录（.tmpXXXX，测试残留）时重置为默认
        if s.data_dir.to_string_lossy().contains(".tmp") {
            log::warn!(
                "engine settings data_dir 指向临时目录（疑似测试残留），已重置: {}",
                s.data_dir.display()
            );
            s.data_dir = default_data_dir();
        }
        s
    }

    pub fn save(&self) -> anyhow::Result<()> {
        let path = settings_file();
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p)?;
        }
        std::fs::write(&path, serde_json::to_string_pretty(self)?)?;
        Ok(())
    }

    /// 尝试从 DSH credentials（~/.dsh/.credentials.yaml）读取 deepseek key。
    pub fn load_api_key_from_dsh(&mut self) -> Option<String> {
        let home = std::env::var_os("DSH_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("USERPROFILE").map(|p| PathBuf::from(p).join(".dsh")))?;
        let cred = home.join(".credentials.yaml");
        let text = std::fs::read_to_string(cred).ok()?;
        // YAML 形如: deepseek-official: { apiKey: sk-xxx }
        let key = parse_credential_yaml(&text)?;
        self.api_key = Some(key.clone());
        Some(key)
    }
}

/// 极简 YAML 提取：找到 deepseek 相关行的 apiKey。
fn parse_credential_yaml(text: &str) -> Option<String> {
    let mut in_deepseek = false;
    for line in text.lines() {
        let trimmed = line.trim();
        // 格式 1：顶层 DEEPSEEK_API_KEY
        if trimmed.starts_with("DEEPSEEK_API_KEY") && trimmed.contains(':') {
            let v = trimmed
                .splitn(2, ':')
                .nth(1)?
                .trim()
                .trim_matches('"')
                .trim_matches('\'');
            if !v.is_empty() && v != "null" {
                return Some(v.to_string());
            }
        }
        // 格式 2：deepseek-official 段
        if trimmed.starts_with("deepseek") && trimmed.contains(':') {
            in_deepseek = true;
            continue;
        }
        if in_deepseek {
            if trimmed.starts_with("apiKey") || trimmed.starts_with("api_key") {
                let v = trimmed
                    .split(':')
                    .nth(1)?
                    .trim()
                    .trim_matches('"')
                    .trim_matches('\'');
                if !v.is_empty() && v != "null" {
                    return Some(v.to_string());
                }
            }
            if !trimmed.starts_with(' ')
                && !trimmed.starts_with('-')
                && trimmed.contains(':')
                && !trimmed.starts_with("apiKey")
                && !trimmed.starts_with("api_key")
            {
                in_deepseek = false;
            }
        }
    }
    None
}

fn settings_file() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("dsh-desktop").join("engine-settings.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_deepseek_key_flat() {
        let yaml = "DEEPSEEK_API_KEY: sk-flat-456\n";
        assert_eq!(parse_credential_yaml(yaml).as_deref(), Some("sk-flat-456"));
    }

    #[test]
    fn parse_deepseek_key() {
        let yaml =
            "deepseek-official:\n  apiKey: \"sk-test-123\"\n  baseUrl: https://api.deepseek.com\n";
        assert_eq!(parse_credential_yaml(yaml).as_deref(), Some("sk-test-123"));
    }

    #[test]
    fn parse_missing_key_returns_none() {
        assert_eq!(parse_credential_yaml("foo: bar\n"), None);
    }

    #[test]
    fn settings_roundtrip() {
        let mut s = EngineSettings::default();
        s.api_key = Some("sk-x".into());
        s.model = "deepseek-reasoner".into();
        let json = serde_json::to_string(&s).unwrap();
        let back: EngineSettings = serde_json::from_str(&json).unwrap();
        assert_eq!(back.api_key.as_deref(), Some("sk-x"));
        assert_eq!(back.model, "deepseek-reasoner");
    }
}
