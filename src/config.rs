//! AppConfig：DSH 路径 / profile / shell / 字体 / 主题。

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// 应用配置（本地持久化，serde JSON）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    /// DSH_HOME（默认 ~/.dsh）
    pub dsh_home: PathBuf,
    /// 当前选中的 profile 名
    pub profile: Option<String>,
    /// shell 程序（默认自动探测 pwsh -> cmd）
    pub shell: Option<String>,
    /// 终端字体大小（px）
    pub font_size: f32,
    /// 回滚行数
    pub scrollback: usize,
    /// DSH web 端口（探测用）
    pub web_port: u16,
    /// HTTP 代理（如 http://127.0.0.1:7890）；为空则不设置
    pub http_proxy: Option<String>,
    /// HTTPS 代理
    pub https_proxy: Option<String>,
    /// 不走代理的主机（逗号分隔，如 localhost,127.0.0.1）
    pub no_proxy: Option<String>,
    /// 最近打开的工作区目录（启动时自动恢复，供工具/新会话/终端使用）
    pub last_workspace: Option<PathBuf>,
    /// 界面语言（"zh" 中文 / "en" English）
    pub lang: String,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            dsh_home: default_dsh_home(),
            profile: None,
            shell: None,
            font_size: 16.0,
            scrollback: 5000,
            web_port: 3081,
            http_proxy: None,
            https_proxy: None,
            no_proxy: None,
            last_workspace: None,
            lang: "zh".into(),
        }
    }
}

/// 解析 DSH_HOME：环境变量优先，否则 ~/.dsh。
pub fn default_dsh_home() -> PathBuf {
    if let Some(h) = std::env::var_os("DSH_HOME") {
        return PathBuf::from(h);
    }
    if let Some(home) = home_dir() {
        return home.join(".dsh");
    }
    PathBuf::from(".dsh")
}

/// 用户主目录。
pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
}

/// 探测默认 shell：pwsh -> powershell -> cmd。
pub fn default_shell() -> String {
    for candidate in ["pwsh", "powershell", "cmd.exe"] {
        if which(candidate) {
            return candidate.to_string();
        }
    }
    "cmd.exe".to_string()
}

/// PATH 探测可执行文件。
fn which(name: &str) -> bool {
    let path_var = match std::env::var_os("PATH") {
        Some(p) => p,
        None => return false,
    };
    let is_windows = cfg!(windows);
    std::env::split_paths(&path_var).any(|dir| {
        let full = dir.join(name);
        full.is_file()
            || (is_windows && {
                let exe = dir.join(format!("{name}.exe"));
                exe.is_file()
            })
    })
}

impl AppConfig {
    /// 配置落盘路径。
    pub fn config_file(&self) -> PathBuf {
        let base = std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        base.join("dsh-desktop").join("config.json")
    }

    pub fn load() -> Self {
        let def = Self::default();
        let path = def.config_file();
        match std::fs::read_to_string(&path) {
            Ok(text) => match serde_json::from_str(&text) {
                Ok(cfg) => {
                    log::info!("config loaded from {}", path.display());
                    cfg
                }
                Err(e) => {
                    log::warn!("config parse failed ({e}); using defaults");
                    def
                }
            },
            Err(_) => {
                log::info!("no config at {}; using defaults", path.display());
                def
            }
        }
    }

    pub fn save(&self) -> anyhow::Result<()> {
        let path = self.config_file();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let text = serde_json::to_string_pretty(self)?;
        std::fs::write(&path, text)?;
        log::info!("config saved to {}", path.display());
        Ok(())
    }

    /// 解析后的 shell 命令。
    pub fn resolved_shell(&self) -> String {
        self.shell.clone().unwrap_or_else(default_shell)
    }

    /// profile 目录。
    pub fn profile_dir(&self, name: &str) -> PathBuf {
        self.dsh_home.join("profiles").join(name)
    }

    /// 当前 profile 目录（存在才 Some）。
    pub fn current_profile_dir(&self) -> Option<PathBuf> {
        self.profile
            .as_ref()
            .map(|name| self.profile_dir(name))
            .filter(|p| p.is_dir())
    }

    /// DSH web 的候选 URL。
    pub fn web_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.web_port)
    }

    /// 路径是否存在（用于 profile 存在性判断）。
    pub fn profile_exists(&self, name: &str) -> bool {
        Path::new(&self.profile_dir(name)).is_dir()
    }

    /// 用户技能目录：$DSH_HOME/skills（不存在则自动创建）。
    pub fn skills_dir(&self) -> PathBuf {
        let dir = self.dsh_home.join("skills");
        if !dir.is_dir() {
            let _ = std::fs::create_dir_all(&dir);
            log::info!("skills dir ensured: {}", dir.display());
        }
        dir
    }

    /// 有配置代理（任一协议）。
    pub fn has_proxy(&self) -> bool {
        self.http_proxy.is_some() || self.https_proxy.is_some()
    }

    /// 设置代理（统一入口，空串视为清除）。
    pub fn set_proxy(&mut self, http: &str, https: &str, no_proxy: &str) {
        self.http_proxy = if http.trim().is_empty() {
            None
        } else {
            Some(http.trim().to_string())
        };
        self.https_proxy = if https.trim().is_empty() {
            None
        } else {
            Some(https.trim().to_string())
        };
        self.no_proxy = if no_proxy.trim().is_empty() {
            None
        } else {
            Some(no_proxy.trim().to_string())
        };
    }

    /// 应用到子进程环境：把 HTTP_PROXY/HTTPS_PROXY/NO_PROXY 写入 env。
    pub fn apply_proxy_env(&self, cmd: &mut std::process::Command) {
        if let Some(p) = &self.http_proxy {
            cmd.env("HTTP_PROXY", p);
            cmd.env("http_proxy", p);
        }
        if let Some(p) = &self.https_proxy {
            cmd.env("HTTPS_PROXY", p);
            cmd.env("https_proxy", p);
        }
        if let Some(p) = &self.no_proxy {
            cmd.env("NO_PROXY", p);
            cmd.env("no_proxy", p);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// last_workspace 可序列化往返。
    #[test]
    fn last_workspace_roundtrip() {
        let mut cfg = AppConfig::default();
        assert!(cfg.last_workspace.is_none());
        cfg.last_workspace = Some(PathBuf::from(r"C:\work\proj"));
        let text = serde_json::to_string(&cfg).unwrap();
        let back: AppConfig = serde_json::from_str(&text).unwrap();
        assert_eq!(back.last_workspace, cfg.last_workspace);
    }

    /// 旧版配置（无 last_workspace 字段）也能反序列化，默认 None。
    #[test]
    fn old_config_without_last_workspace_loads() {
        let old = r#"{"dsh_home":"C:\\dsh","profile":null,"shell":null,"font_size":16.0,"scrollback":5000,"web_port":3080,"http_proxy":null,"https_proxy":null,"no_proxy":null}"#;
        let cfg: AppConfig = serde_json::from_str(old).unwrap();
        assert!(cfg.last_workspace.is_none());
        assert_eq!(cfg.dsh_home, PathBuf::from(r"C:\dsh"));
    }
}
