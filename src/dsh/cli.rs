//! dsh CLI 定位与执行（spawn，流式输出）。

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc::Sender;

use anyhow::{Context, Result};
use log::{debug, info, warn};

/// 定位 dsh CLI：PATH -> npx 缓存 .bin。
pub fn find_dsh() -> Option<PathBuf> {
    // 只认 Win32 可直接执行的形式（历史缺陷：裸 "dsh" 排首位，npm
    // 全局目录命中 Git Bash 的 sh shim → CreateProcess error 193；
    // .ps1 同样不能被 CreateProcess 直接 spawn）
    const CANDIDATES: &[&str] = &["dsh.cmd", "dsh.exe", "dsh.bat"];
    if let Ok(path_var) = std::env::var("PATH") {
        for dir in std::env::split_paths(&path_var) {
            for name in CANDIDATES {
                let candidate = dir.join(name);
                if candidate.is_file() {
                    debug!("found dsh at {}", candidate.display());
                    return Some(candidate);
                }
            }
        }
    }
    // npx 缓存兜底：扫描 npm-cache/_npx/*/node_modules/.bin（历史缺陷：
    // 兜底路径硬编码了个人用户目录，换机即失效）。多个缓存目录时取
    // 最新修改的那个。
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        let npx_root = PathBuf::from(local).join("npm-cache").join("_npx");
        if let Ok(entries) = std::fs::read_dir(&npx_root) {
            let mut bins: Vec<(std::time::SystemTime, PathBuf)> = entries
                .flatten()
                .map(|e| e.path().join("node_modules").join(".bin"))
                .filter(|p| p.is_dir())
                .filter_map(|p| {
                    let t = p
                        .metadata()
                        .and_then(|m| m.modified())
                        .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
                    Some((t, p))
                })
                .collect();
            bins.sort_by(|a, b| b.0.cmp(&a.0));
            for (_, bin) in bins {
                for name in CANDIDATES {
                    let candidate = bin.join(name);
                    if candidate.is_file() {
                        debug!("found dsh at {}", candidate.display());
                        return Some(candidate);
                    }
                }
            }
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
///
/// 超时保护：dsh CLI 挂起/交互式等待输入时不能永久阻塞调用线程
/// （默认 10 分钟——npm install 类操作可能较慢）。输出经排水线程并发读
/// （防管道缓冲写满死锁），完成后逐行推给 UI。
pub fn run_plugin_cmd_proxy(
    profile_name: &str,
    args: &[&str],
    tx: Option<&Sender<String>>,
    cfg: &crate::config::AppConfig,
) -> Result<PluginCmdResult> {
    let dsh = find_dsh().context("dsh CLI not found on PATH")?;
    info!("dsh plugin --profile {profile_name} {args:?}");
    let mut cmd = Command::new(&dsh);
    cmd.arg("plugin")
        .arg("--profile")
        .arg(profile_name)
        .args(args);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // proxy 注入（设置页配置的 HTTP/HTTPS 代理对 dsh plugin 生效）
    cfg.apply_proxy_env(&mut cmd);
    // 静默：不弹黑色控制台窗口（dsh.cmd 是批处理）
    crate::util::hide_console(&mut cmd);
    let mut child = cmd
        .spawn()
        .with_context(|| format!("spawn {}", dsh.display()))?;

    // 排水线程：并发读 stdout/stderr 防管道死锁
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

    // 有界等待（10 分钟）
    let timeout = std::time::Duration::from_secs(600);
    let started = std::time::Instant::now();
    let status = loop {
        match child.try_wait()? {
            Some(st) => break Some(st),
            None => {
                if started.elapsed() > timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }
    };
    let timed_out = status.is_none();

    let recv = |rx: &std::sync::mpsc::Receiver<Vec<u8>>| -> String {
        rx.recv_timeout(std::time::Duration::from_secs(5))
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .unwrap_or_default()
    };
    let stdout_text = recv(&so_rx);
    let mut stderr_text = recv(&se_rx);
    if timed_out {
        stderr_text.push_str(&format!(
            "\n[dsh-desktop] dsh plugin 超时（>{timeout:?}），已终止"
        ));
    }

    // 若给了 tx，把输出逐行推给 UI（收尾后再推）
    if let Some(tx) = tx {
        for line in stdout_text.lines() {
            let _ = tx.send(line.to_string());
        }
        for line in stderr_text.lines() {
            let _ = tx.send(line.to_string());
        }
    }
    let code = if timed_out {
        -1
    } else {
        status.and_then(|s| s.code()).unwrap_or(-1)
    };
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
///
/// 正确形式是 `dsh web --port <N>`（历史缺陷：写成 `dsh --profile X web
/// --port N`，父级 --profile 被 web 子命令拒绝：`web takes none of parent
/// --profile`，启动必失败 code=1——实测 `dsh web --port N` 正常 200）。
pub fn spawn_web() -> Result<std::process::Child> {
    let dsh = find_dsh().context("dsh CLI not found on PATH")?;
    info!("spawning dsh web");
    let cfg = crate::config::AppConfig::load();
    let mut cmd = Command::new(&dsh);
    cmd.arg("web");
    cmd.arg("--port").arg(cfg.web_port.to_string());
    // stderr 管道捕获：profile 不存在 / 端口占用等失败原因要能显示给用户
    //（历史缺陷：丢弃 stderr，进程秒退毫无提示，按钮弹回像什么都没发生）。
    // stdout 仍丢弃（web 常驻输出多）。
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    cfg.apply_proxy_env(&mut cmd);
    // 静默：不弹黑色控制台窗口
    crate::util::hide_console(&mut cmd);
    let child = cmd
        .spawn()
        .with_context(|| format!("spawn dsh web: {}", dsh.display()))?;
    let job = tie_child_to_job(&child);
    let _ = job; // 句柄由 WebHandle 持有（见 start），此处仅确保绑定
    Ok(child)
}

/// 把子进程绑进 Job Object（KILL_ON_JOB_CLOSE）：父进程任何形式退出
///（含 taskkill /F、崩溃）→ 内核关闭 job 句柄 → 子进程树被终止，
/// 不再产生孤儿 web。返回 job 句柄（调用方持有；None=绑定失败仅告警，
/// 退化为旧行为）。cmd.exe 包装的 node 子进程默认继承 job，整树覆盖。
fn tie_child_to_job(child: &std::process::Child) -> Option<usize> {
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW,
        JobObjectExtendedLimitInformation, SetInformationJobObject,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    };
    unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            warn!("CreateJobObject failed");
            return None;
        }
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let ok = SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const core::ffi::c_void,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        );
        if ok == 0 {
            warn!("SetInformationJobObject failed");
            return None;
        }
        use std::os::windows::io::AsRawHandle;
        if AssignProcessToJobObject(job, child.as_raw_handle() as _) == 0 {
            warn!("AssignProcessToJobObject failed");
            return None;
        }
        info!("dsh web tied to kill-on-close job");
        Some(job as usize)
    }
}

/// 解析 netstat -aon 输出中监听指定端口的 PID 列表（LISTENING 行）。
pub fn parse_listening_pids(netstat_out: &str, port: u16) -> Vec<u32> {
    let needle = format!(":{port}");
    netstat_out
        .lines()
        .filter_map(|l| {
            let cols: Vec<&str> = l.split_whitespace().collect();
            if cols.len() < 5 || !cols.iter().any(|c| c.eq_ignore_ascii_case("LISTENING")) {
                return None;
            }
            // 本地地址列（第 2 列）以 :port 结尾，避免误匹配远端端口
            let local = cols.get(1)?;
            if !local.ends_with(&needle) {
                return None;
            }
            cols.last().and_then(|p| p.parse::<u32>().ok())
        })
        .collect()
}

/// 终止占用指定端口的外部 web 进程（netstat 找 LISTENING PID → taskkill）。
/// 返回被终止的 PID 列表描述。
pub fn kill_web_by_port(port: u16) -> Result<String, String> {
    let out = Command::new("netstat")
        .args(["-aon"])
        .output()
        .map_err(|e| format!("netstat 执行失败: {e}"))?;
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    let pids = parse_listening_pids(&text, port);
    if pids.is_empty() {
        return Err(format!("端口 {port} 上没有监听进程"));
    }
    let mut killed = Vec::new();
    for pid in &pids {
        match Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/F"])
            .output()
        {
            Ok(o) if o.status.success() => killed.push(*pid),
            Ok(o) => warn!(
                "taskkill {pid} 失败: {}",
                String::from_utf8_lossy(&o.stderr).trim()
            ),
            Err(e) => warn!("taskkill {pid} 执行失败: {e}"),
        }
    }
    if killed.is_empty() {
        return Err(format!("终止失败（PID {:?}）", pids));
    }
    Ok(format!(
        "已终止外部 web 进程（PID {:?}，端口 {port}）",
        killed
    ))
}

/// DSH web 子进程托管：启动 / 停止 / 重启 + 是否由本应用托管。
///
/// 插件（cordis 组合）只在 dsh web 进程里生效——这是"插件功能不是摆设"的关键：
/// 桌面端负责拉起/重启 web，插件变更后重启使配置真正生效。
#[derive(Default)]
pub struct WebHandle {
    child: Option<std::process::Child>,
    /// 后台线程捕获的 web stderr（进程退出时填入；管道读线程防写满死锁）
    stderr_captured: Option<std::sync::Arc<std::sync::Mutex<Option<String>>>>,
    /// stderr 读线程完成标志（EOF 即置位——reap 等它而非固定 sleep，
    /// 消除 UI 线程 120ms 卡顿与"早读到空串"竞态）
    stderr_done: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    /// kill-on-close Job 句柄（usize 存的 *mut c_void；父进程任何形式退出
    /// → 子进程树被内核终止）
    job: Option<usize>,
    /// 最近一次失败原因（spawn 失败或进程异常退出）——UI 在 web 行内显示
    last_error: Option<String>,
    /// 用户主动停止（reap 不把这次退出当失败）
    user_stop: bool,
}

impl WebHandle {
    pub fn new() -> Self {
        Self::default()
    }

    /// 是否由本应用托管（false = 外部已启动，勿抢占端口）。
    pub fn managed(&self) -> bool {
        self.child.is_some()
    }

    pub fn pid(&self) -> Option<u32> {
        self.child.as_ref().map(|c| c.id())
    }

    /// 最近一次失败原因（None = 无失败）；UI 在 web 行内显示。
    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    /// 启动（已在托管则忽略）。spawn 失败/进程秒退的原因记入 last_error。
    pub fn start(&mut self) -> Result<()> {
        if self.child.is_some() {
            return Ok(());
        }
        self.last_error = None;
        self.user_stop = false;
        let mut child = match spawn_web() {
            Ok(c) => c,
            Err(e) => {
                let msg = format!("{e:#}");
                warn!("dsh web start failed: {msg}");
                self.last_error = Some(msg);
                return Err(e);
            }
        };
        // stderr 读线程：进程退出后填 captured（防管道写满死锁 + 秒退原因可读）
        let captured = std::sync::Arc::new(std::sync::Mutex::new(None::<String>));
        let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        if let Some(stderr) = child.stderr.take() {
            let captured = captured.clone();
            let done = done.clone();
            let _ = std::thread::Builder::new()
                .name("dsh-web-stderr".into())
                .spawn(move || {
                    use std::io::Read;
                    let mut buf = String::new();
                    let mut r = stderr;
                    let _ = r.read_to_string(&mut buf);
                    // 只留尾部 8KB（失败信息一般在末尾）
                    let tail: String = buf
                        .chars()
                        .skip(buf.chars().count().saturating_sub(8 * 1024))
                        .collect();
                    *captured.lock().unwrap() = Some(tail);
                    done.store(true, std::sync::atomic::Ordering::Release);
                });
        } else {
            done.store(true, std::sync::atomic::Ordering::Release);
        }
        self.stderr_captured = Some(captured);
        self.stderr_done = Some(done);
        self.job = tie_child_to_job(&child);
        self.child = Some(child);
        info!("dsh web started");
        Ok(())
    }

    pub fn stop(&mut self) {
        self.user_stop = true;
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
            info!("dsh web stopped");
        }
        self.close_job();
    }

    /// 关闭 kill-on-close job 句柄（进程已停，防句柄泄漏）。
    fn close_job(&mut self) {
        if let Some(j) = self.job.take() {
            unsafe {
                windows_sys::Win32::Foundation::CloseHandle(
                    j as *mut core::ffi::c_void,
                );
            }
        }
    }

    /// 重启（插件启停/导入/移除后调用，使 cordis 配置生效）。
    pub fn restart(&mut self) {
        self.stop();
        match self.start() {
            Ok(()) => info!("dsh web restarted"),
            Err(e) => warn!("dsh web restart failed: {e:#}"),
        }
    }

    /// 清理已退出的子进程：web 被外部结束/崩溃后 managed() 不再误报
    /// （历史缺陷：child 永不清退，停止/重启按钮永远操作死 PID）。
    /// 退出原因（含捕获的 stderr 尾部）写入 last_error 供 UI 显示——
    /// 除非是用户主动停止。每帧调用（try_wait 无退出时零成本）。
    pub fn reap(&mut self) {
        let exited = if let Some(child) = &mut self.child {
            match child.try_wait() {
                Ok(Some(status)) => Some(status.code()),
                Ok(None) => None,
                Err(e) => {
                    warn!("dsh web try_wait failed: {e}");
                    Some(None)
                }
            }
        } else {
            return;
        };
        if let Some(code) = exited {
            if self.user_stop {
                self.user_stop = false;
                self.child = None;
                self.close_job();
                info!("dsh web stopped by user (code {code:?})");
                return;
            }
            // stderr 读线程未收尾（EOF 未到）：本帧不判定，下帧重查
            //（历史缺陷：固定 sleep 120ms 卡 UI 且可能早读到空串）
            let drained = self
                .stderr_done
                .as_ref()
                .map(|d| d.load(std::sync::atomic::Ordering::Acquire))
                .unwrap_or(true);
            if !drained {
                return;
            }
            self.child = None;
            let stderr_tail = self
                .stderr_captured
                .as_ref()
                .and_then(|c| c.lock().ok())
                .and_then(|g| g.clone())
                .unwrap_or_default();
            let detail = stderr_tail.trim();
            let msg = if detail.is_empty() {
                format!("dsh web 启动后立即退出（code {code:?}），无错误输出")
            } else {
                format!(
                    "dsh web 启动失败（code {code:?}）：{}",
                    detail.chars().take(300).collect::<String>()
                )
            };
            warn!("{msg}");
            self.last_error = Some(msg);
            self.close_job();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// netstat 输出解析：只取本地地址以 :port 结尾的 LISTENING 行。
    #[test]
    fn parse_listening_pids_filters_port() {
        let out = "\r\n  TCP    127.0.0.1:3081    0.0.0.0:0    LISTENING    111\r\n  TCP    127.0.0.1:13081   0.0.0.0:0    LISTENING    222\r\n  TCP    127.0.0.1:9999    127.0.0.1:3081  ESTABLISHED  333\r\n  TCP    0.0.0.0:3081      0.0.0.0:0    LISTENING    444\r\n";
        let pids = parse_listening_pids(out, 3081);
        assert_eq!(pids, vec![111, 444], "匹配本地 :3081 的 LISTENING（排除 13081/远端/ESTABLISHED）");
        assert!(parse_listening_pids(out, 12345).is_empty());
    }

    /// 秒退进程：reap 应捕获退出并生成含 stderr 的失败信息。
    /// （真实链路：spawn dsh web → profile 不存在/端口占用 → 进程带
    /// 错误信息退出 → reap 写 last_error → UI 显示）
    #[test]
    fn reap_records_failure_with_stderr() {
        use std::io::Write;
        let mut child = Command::new("cmd")
            .args(["/C", "echo profile_not_found>&2 & exit /B 2"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let captured = std::sync::Arc::new(std::sync::Mutex::new(None::<String>));
        if let Some(mut stderr) = child.stderr.take() {
            let cap = captured.clone();
            std::thread::spawn(move || {
                use std::io::Read;
                let mut buf = String::new();
                let _ = stderr.read_to_string(&mut buf);
                *cap.lock().unwrap() = Some(buf);
            });
        }
        let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        done.store(true, std::sync::atomic::Ordering::Release);
        let mut h = WebHandle {
            child: Some(child),
            stderr_captured: Some(captured),
            stderr_done: Some(done),
            job: None,
            last_error: None,
            user_stop: false,
        };
        // 等进程退出 + stderr 排空
        std::thread::sleep(std::time::Duration::from_millis(600));
        h.reap();
        let err = h.last_error().expect("秒退应有失败信息");
        assert!(err.contains("退出") || err.contains("失败"), "{err}");
        assert!(h.last_error.is_some());
        // 用户主动停止不算失败
        let child2 = Command::new("cmd")
            .args(["/C", "exit /B 0"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let mut h2 = WebHandle {
            child: Some(child2),
            stderr_captured: None,
            stderr_done: None,
            job: None,
            last_error: None,
            user_stop: true,
        };
        h2.stop();
        assert!(h2.last_error.is_none(), "主动停止不应记失败");
    }
}
