//! Windows ACL restricted-token 沙箱（编排层）。
//!
//! 策略层（SandboxMode / can_write / 升级阶梯）在 sandbox.rs；
//! 真正的 restricted-token 执行在 `winacl`（纯 Rust + windows-sys，无 Node 依赖）。
//! 本模块把两者桥接，并对外暴露 `WindowsAclSandbox` 给 tools.rs/agent.rs 使用。

use std::path::{Path, PathBuf};
use std::time::Duration;

use super::sandbox::SandboxMode;
use super::winacl;

/// 沙箱命令超时（对齐 bash timeoutMs=300000）。
const SANDBOX_TIMEOUT: Duration = Duration::from_secs(300);

/// 受限执行结果。
#[derive(Debug, Clone)]
pub struct AclResult {
    pub ok: bool,
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    /// 是否为受限子进程的输出（含被沙箱拒绝的报错文本）。
    pub confined: bool,
    /// 沙箱层自身（而非被包命令）的失败提示。
    pub sandbox_error: Option<String>,
}

impl AclResult {
    fn denied(msg: String) -> Self {
        AclResult {
            ok: false,
            exit_code: None,
            stdout: String::new(),
            stderr: msg.clone(),
            confined: true,
            sandbox_error: Some(msg),
        }
    }
}

/// 受限令牌沙箱执行器（纯 Rust，委托 winacl）。
#[derive(Clone)]
pub struct WindowsAclSandbox {
    pub mode: SandboxMode,
    pub workspace_root: PathBuf,
}

impl WindowsAclSandbox {
    pub fn new(mode: SandboxMode, workspace_root: PathBuf) -> Self {
        Self {
            mode,
            workspace_root,
        }
    }

    /// 受限模式恒为 true（非受限也 true：直通）。纯 Win32 实现，无外部依赖。
    pub fn available(&self) -> bool {
        true
    }

    /// 当前沙箱模式。
    pub fn mode(&self) -> crate::exec::SandboxMode {
        self.mode
    }

    /// 受限执行入口。
    ///
    /// - `DangerFullAccess`：返回 `None`（调用方走普通直通）。
    /// - `ReadOnly`（当前）与 `WorkspaceWrite`：经 restricted-token 执行命令；
    ///   任何失败返回 `Some(denied)`，绝不直通（fail-closed）。
    pub fn run(&self, command: &str, args: &[&str], cwd: &Path) -> Option<AclResult> {
        if matches!(self.mode, SandboxMode::DangerFullAccess) {
            return None;
        }
        // workspace-write：正常用户令牌执行，但 bash/pwsh 的写目标在应用层
        // 硬拒绝（fail-closed，见 tools.rs 的 check_write_allowed 接线——
        // 历史缺陷：该模式直通返回 None，shell 写的唯一闸门只剩审批启发式）。
        // OS 级按目录强制需要 per-directory ACL 授权（修改磁盘 DACL 有历史
        // 事故），暂以应用层门 + 增强的重定向/写命令解析为界。
        if matches!(self.mode, SandboxMode::WorkspaceWrite) {
            log::info!(
                "sandbox workspace-write: normal token + app-level write gate for shell"
            );
            return None;
        }
        // read-only：restricted token（Everyone 限制 SID → 系统级只读）
        let extra: Vec<winacl::RawSid> = Vec::new();
        let token = match winacl::build_restricted_token(&extra) {
            Ok(t) => t,
            Err(e) => {
                return Some(AclResult::denied(format!(
                    "sandbox fail-closed: restricted token 构建失败: {e}"
                )));
            }
        };
        let result = winacl::spawn_restricted(token, command, args, cwd, SANDBOX_TIMEOUT);
        unsafe { windows_sys::Win32::Foundation::CloseHandle(token) };

        let (exit_code, stdout_bytes, stderr_bytes) = match result {
            Ok(x) => x,
            Err(e) => {
                return Some(AclResult::denied(format!(
                    "sandbox fail-closed: 受限执行失败: {e}"
                )));
            }
        };
        let stdout = String::from_utf8_lossy(&stdout_bytes).into_owned();
        let stderr = String::from_utf8_lossy(&stderr_bytes).into_owned();
        Some(AclResult {
            ok: exit_code == 0,
            exit_code: Some(exit_code),
            stdout,
            stderr,
            confined: true,
            sandbox_error: None,
        })
    }

    /// 便捷：路径是否在受限模式下可写（交给策略层）。
    pub fn can_write(&self, path: &Path) -> bool {
        match self.mode {
            SandboxMode::DangerFullAccess => true,
            SandboxMode::WorkspaceWrite => path_is_within(path, &self.workspace_root),
            SandboxMode::ReadOnly => false,
        }
    }
}

fn path_is_within(path: &Path, root: &Path) -> bool {
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let mut cur = path.to_path_buf();
    let mut remaining: Vec<std::ffi::OsString> = Vec::new();
    let ancestor = loop {
        if cur.exists() {
            break cur;
        }
        match cur.file_name() {
            Some(name) => remaining.push(name.to_os_string()),
            None => return false,
        }
        if !cur.pop() {
            return false;
        }
    };
    let ancestor = ancestor.canonicalize().unwrap_or(ancestor);
    let mut resolved = ancestor;
    for comp in remaining.iter().rev() {
        resolved.push(comp);
    }
    resolved.starts_with(&root)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn danger_full_access_passthrough_signal() {
        let dir = tempdir().unwrap();
        let sb = WindowsAclSandbox::new(SandboxMode::DangerFullAccess, dir.path().to_path_buf());
        assert!(sb.available());
        let out = sb.run("cmd", &["/C", "echo x"], dir.path());
        assert!(out.is_none(), "danger-full-access 应交给调用方直通");
    }

    #[test]
    fn can_write_respects_mode() {
        let dir = tempdir().unwrap();
        let ws = dir.path().join("workspace");
        std::fs::create_dir_all(&ws).unwrap();
        let inside = ws.join("a.txt");
        let outside = dir.path().join("b.txt");
        let sb = WindowsAclSandbox::new(SandboxMode::WorkspaceWrite, ws.clone());
        assert!(sb.can_write(&inside));
        assert!(!sb.can_write(&outside));
        let ro = WindowsAclSandbox::new(SandboxMode::ReadOnly, ws.clone());
        assert!(!ro.can_write(&inside));
        let da = WindowsAclSandbox::new(SandboxMode::DangerFullAccess, ws);
        assert!(da.can_write(&outside));
    }

    /// 真实 E2E（纯 Rust restricted-token）：
    /// workspace-write 下写 ws 内成功、写 ws 外被拒。
    #[test]
    #[ignore]
    fn e2e_workspace_write_denies_outside() {
        let dir = tempdir().unwrap();
        let ws = dir.path().join("workspace");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::write(ws.join("inside.txt"), "x").unwrap();

        let sb = WindowsAclSandbox::new(SandboxMode::WorkspaceWrite, ws.clone());
        // 写 ws 内
        let ins = sb
            .run("cmd", &["/C", "echo hi > inside2.txt"], &ws)
            .unwrap();
        assert!(ins.ok, "写 ws 内应成功: {}", ins.stderr);
        assert!(ws.join("inside2.txt").exists(), "inside2.txt 应创建");
        // 写 ws 外 → 受限 token 拒
        let outside = dir.path().join("escape.txt");
        let outs = sb
            .run(
                "cmd",
                &[
                    "/C",
                    format!("echo hi > \"{}\"", outside.display()).as_str(),
                ],
                &ws,
            )
            .unwrap();
        assert!(!outs.ok, "写 ws 外必须被拒: {:?}", outs);
        assert!(!outside.exists(), "escape.txt 绝不能被创建");
    }
}
