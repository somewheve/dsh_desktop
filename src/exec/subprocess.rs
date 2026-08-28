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
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // 静默：不弹黑色控制台窗口
        crate::util::hide_console(&mut cmd);

        let mut child = cmd.spawn().with_context(|| format!("spawn {program}"))?;
        // 排水线程并发读 stdout/stderr：子进程写满 ~64KB 管道缓冲后阻塞在
        // write 上，try_wait 永远等不到退出 → 假超时 + 输出全丢（历史死锁）
        let (so_tx, so_rx) = std::sync::mpsc::channel::<Vec<u8>>();
        let (se_tx, se_rx) = std::sync::mpsc::channel::<Vec<u8>>();
        std::thread::spawn({
            let pipe = child.stdout.take();
            move || {
                if let Some(mut r) = pipe {
                    let mut buf = Vec::new();
                    use std::io::Read;
                    let _ = r.read_to_end(&mut buf);
                    let _ = so_tx.send(buf);
                }
            }
        });
        std::thread::spawn({
            let pipe = child.stderr.take();
            move || {
                if let Some(mut r) = pipe {
                    let mut buf = Vec::new();
                    use std::io::Read;
                    let _ = r.read_to_end(&mut buf);
                    let _ = se_tx.send(buf);
                }
            }
        });

        let started = std::time::Instant::now();
        loop {
            match child.try_wait().context("try wait failed")? {
                Some(status) => {
                    let recv = |rx: &std::sync::mpsc::Receiver<Vec<u8>>| -> String {
                        rx.recv_timeout(Duration::from_secs(5))
                            .map(|b| String::from_utf8_lossy(&b).into_owned())
                            .unwrap_or_default()
                    };
                    return Ok(ProcResult {
                        exit_code: status.code(),
                        stdout: recv(&so_rx),
                        stderr: recv(&se_rx),
                        timed_out: false,
                    });
                }
                None => {
                    if started.elapsed() > timeout {
                        let _ = child.kill();
                        let _ = child.wait();
                        return Ok(ProcResult {
                            exit_code: None,
                            stdout: so_rx
                                .recv_timeout(Duration::from_secs(1))
                                .map(|b| String::from_utf8_lossy(&b).into_owned())
                                .unwrap_or_default(),
                            stderr: format!("timed out after {:?}", timeout),
                            timed_out: true,
                        });
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
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

    /// 回归：子进程输出超过管道缓冲（~64KB）不能死锁假超时——
    /// 排水线程并发读，输出完整收回且不超时。
    #[test]
    fn large_output_does_not_deadlock() {
        let mgr = SubprocessManager::default();
        // 输出 ~200KB（远超管道缓冲）
        let res = mgr
            .run_with_timeout(
                "cmd",
                &["/C", "for /L %i in (1,1,4000) do @echo 01234567890123456789012345678901234567890123456789"],
                None,
                Duration::from_secs(30),
            )
            .unwrap();
        assert!(!res.timed_out, "大输出不应触发假超时");
        assert!(
            res.stdout.len() > 100_000,
            "大输出应完整收回（{} 字节）",
            res.stdout.len()
        );
        assert_eq!(res.exit_code, Some(0));
    }
}
