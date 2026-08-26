//! dsh CLI 定位与执行（spawn，流式输出）。

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc::Sender;

use anyhow::{Context, Result};
use log::{debug, info, warn};

/// 定位 dsh CLI：PATH -> npx 缓存 .bin。
pub fn find_dsh() -> Option<PathBuf> {
    if let Ok(path_var) = std::env::var("PATH") {
        for dir in std::env::split_paths(&path_var) {
            for name in ["dsh", "dsh.cmd", "dsh.exe", "dsh.ps1"] {
                let candidate = dir.join(name);
                if candidate.is_file() {
                    debug!("found dsh at {}", candidate.display());
                    return Some(candidate);
                }
            }
        }
    }
    // npx 缓存兜底（本机已知位置）
    let npx_cache = PathBuf::from(
        "C:\\Users\\somew\\AppData\\Local\\npm-cache\\_npx\\1e7f6d9597241db0\\node_modules\\.bin",
    );
    for name in ["dsh.cmd", "dsh.ps1", "dsh"] {
        let candidate = npx_cache.join(name);
        if candidate.is_file() {
            debug!("found dsh at {}", candidate.display());
            return Some(candidate);
        }
    }
    None
}

/// DSH 的 Node 模块解析路径（NODE_PATH）：dsh CLI（npx 缓存）node_modules +
/// profiles/node_modules + 桌面端下载插件仓库（plugins-src/node_modules）。
/// 供 node_called 工具与 cordis 桥使用。
pub fn dsh_node_paths() -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    if let Some(dsh) = find_dsh() {
        if let Some(bin) = dsh.parent() {
            if let Some(nm) = bin.parent() {
                if nm.file_name().map(|n| n == "node_modules").unwrap_or(false) {
                    out.push(nm.to_path_buf());
                }
            }
        }
    }
    let cfg = crate::config::AppConfig::load();
    out.push(cfg.dsh_home.join("profiles").join("node_modules"));
    out.push(cfg.dsh_home.join("plugins-src").join("node_modules"));
    out
}

/// 一次 dsh plugin 调用的结果。
#[derive(Debug, Clone)]
pub struct PluginCmdResult {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

/// 在 profile 目录里执行 `dsh plugin --profile <name> <args...>`。
///
/// 通过 tx 把每行输出推给 UI（可选）。
pub fn run_plugin_cmd(
    profile_name: &str,
    args: &[&str],
    tx: Option<&Sender<String>>,
) -> Result<PluginCmdResult> {
    run_plugin_cmd_proxy(profile_name, args, tx, &crate::config::AppConfig::load())
}

/// 带 proxy 环境的版本（内部实现）。
pub fn run_plugin_cmd_proxy(
    profile_name: &str,
    args: &[&str],
    tx: Option<&Sender<String>>,
    cfg: &crate::config::AppConfig,
) -> Result<PluginCmdResult> {
    let _ = cfg;
    let dsh = find_dsh().context("dsh CLI not found on PATH")?;
    info!("dsh plugin --profile {profile_name} {args:?}");
    let mut cmd = Command::new(&dsh);
    cmd.arg("plugin")
        .arg("--profile")
        .arg(profile_name)
        .args(args);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    // 静默：不弹黑色控制台窗口（dsh.cmd 是批处理）
    crate::util::hide_console(&mut cmd);
    let child = cmd
        .spawn()
        .with_context(|| format!("spawn {}", dsh.display()))?;

    // 用 wait_with_output 统一收尾，避免 child 被部分 move
    let out = child.wait_with_output()?;
    let stdout_text = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr_text = String::from_utf8_lossy(&out.stderr).into_owned();

    // 若给了 tx，把输出逐行推给 UI（收尾后再推）
    if let Some(tx) = tx {
        for line in stdout_text.lines() {
            let _ = tx.send(line.to_string());
        }
        for line in stderr_text.lines() {
            let _ = tx.send(line.to_string());
        }
    }
    let code = out.status.code().unwrap_or(-1);
    if code != 0 {
        warn!("dsh plugin exited {code}: {stderr_text}");
    }
    Ok(PluginCmdResult {
        exit_code: code,
        stdout: stdout_text,
        stderr: stderr_text,
    })
}

/// 启动 dsh web（不等待，返回子进程）。监听端口取配置（默认 3081）。
pub fn spawn_web(profile_name: &str) -> Result<std::process::Child> {
    let dsh = find_dsh().context("dsh CLI not found on PATH")?;
    info!("spawning dsh web (profile {profile_name})");
    let cfg = crate::config::AppConfig::load();
    let mut cmd = Command::new(&dsh);
    cmd.arg("--profile").arg(profile_name).arg("web");
    cmd.arg("--port").arg(cfg.web_port.to_string());
    cmd.stdout(Stdio::null()).stderr(Stdio::null());
    cfg.apply_proxy_env(&mut cmd);
    // 静默：不弹黑色控制台窗口
    crate::util::hide_console(&mut cmd);
    let child = cmd
        .spawn()
        .with_context(|| format!("spawn dsh web: {}", dsh.display()))?;
    Ok(child)
}

/// DSH web 子进程托管：启动 / 停止 / 重启 + 是否由本应用托管。
///
/// 插件（cordis 组合）只在 dsh web 进程里生效——这是"插件功能不是摆设"的关键：
/// 桌面端负责拉起/重启 web，插件变更后重启使配置真正生效。
#[derive(Default)]
pub struct WebHandle {
    child: Option<std::process::Child>,
    profile: String,
}

impl WebHandle {
    pub fn new() -> Self {
        Self::default()
    }

    /// 是否由本应用托管（false = 外部已启动，勿抢占端口）。
    pub fn managed(&self) -> bool {
        self.child.is_some()
    }

    pub fn profile(&self) -> &str {
        &self.profile
    }

    pub fn pid(&self) -> Option<u32> {
        self.child.as_ref().map(|c| c.id())
    }

    /// 启动（已在托管则忽略）。profile 不存在时 spawn 会失败并返回错误。
    pub fn start(&mut self, profile: &str) -> Result<()> {
        if self.child.is_some() {
            return Ok(());
        }
        let child = spawn_web(profile)?;
        self.profile = profile.to_string();
        self.child = Some(child);
        info!("dsh web started (profile {profile})");
        Ok(())
    }

    pub fn stop(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
            info!("dsh web stopped");
        }
    }

    /// 重启（插件启停/导入/移除后调用，使 cordis 配置生效）。
    pub fn restart(&mut self, profile: &str) {
        self.stop();
        match self.start(profile) {
            Ok(()) => info!("dsh web restarted (profile {profile})"),
            Err(e) => warn!("dsh web restart failed: {e:#}"),
        }
    }

    /// 进程是否还活着（托管中的子进程退出检测）。
    pub fn alive(&mut self) -> bool {
        match self.child.as_mut() {
            Some(c) => c.try_wait().map(|s| s.is_none()).unwrap_or(false),
            None => false,
        }
    }
}
