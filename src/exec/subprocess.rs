//! 子进程管理（对齐 dsh-subprocess*）：spawn/超时/输出捕获。

use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result};

/// 子进程执行结果。
#[derive(Debug, Clone)]
pub struct ProcResult {
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
}

/// 子进程管理器。
pub struct SubprocessManager {
    /// 默认超时（对齐 dsh-timeout 语义）
    pub default_timeout: Duration,
}

impl Default for SubprocessManager {
    fn default() -> Self {
        Self {
            default_timeout: Duration::from_secs(120),
        }
    }
}

impl SubprocessManager {
    /// 同步执行命令（捕获输出，超时杀进程）。
    pub fn run(&self, program: &str, args: &[&str], cwd: Option<&Path>) -> Result<ProcResult> {
        self.run_with_timeout(program, args, cwd, self.default_timeout)
    }

    pub fn run_with_timeout(
        &self,
        program: &str,
        args: &[&str],
        cwd: Option<&Path>,
        timeout: Duration,
    ) -> Result<ProcResult> {
        let mut cmd = Command::new(program);
        cmd.args(args);
        if let Some(dir) = cwd {
            cmd.current_dir(dir);
        }
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        // 静默：不弹黑色控制台窗口
        crate::util::hide_console(&mut cmd);

        // 用线程 + 超时等待
        let mut child = cmd.spawn().with_context(|| format!("spawn {program}"))?;
        let started = std::time::Instant::now();
        loop {
            if let Some(status) = child.try_wait().context("try_wait failed")? {
                let out = child
                    .wait_with_output()
                    .context("wait_with_output failed")?;
                return Ok(ProcResult {
                    exit_code: status.code(),
                    stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
                    stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
                    timed_out: false,
                });
            }
            if started.elapsed() > timeout {
                let _ = child.kill();
                let _ = child.wait();
                return Ok(ProcResult {
                    exit_code: None,
                    stdout: String::new(),
                    stderr: format!("timed out after {:?}", timeout),
                    timed_out: true,
                });
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

use std::path::Path;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_echo() {
        let mgr = SubprocessManager::default();
        let res = mgr.run("cmd", &["/C", "echo hello_sub"], None).unwrap();
        assert!(res.stdout.contains("hello_sub"));
        assert_eq!(res.exit_code, Some(0));
        assert!(!res.timed_out);
    }

    #[test]
    fn timeout_kills() {
        let mgr = SubprocessManager::default();
        let res = mgr
            .run_with_timeout(
                "cmd",
                &["/C", "ping -n 30 127.0.0.1 > nul"],
                None,
                Duration::from_millis(300),
            )
            .unwrap();
        assert!(res.timed_out);
    }
}
