//! cordis 风格子进程插件框架：发现 / 生命周期 / JSON-RPC 桥接。
//!
//! 插件 = $DSH_HOME/plugins/<name>/plugin.json（manifest）+ 任意可执行程序。
//! 语义对齐 cordis：apply(ctx) 生命周期（initialize → ready）、服务注册、
//! 事件总线、工具桥接（agent 回合直接调用插件工具）。

pub mod protocol;

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{channel, Receiver};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use protocol::{PluginReady, PluginToolSpec, RpcIn, RpcRequest};

/// 插件清单。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginManifest {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// 启动命令（argv）。相对路径相对插件目录解析。
    pub command: Vec<String>,
    /// 静态声明的工具（插件可在握手时动态上报更多）
    #[serde(default)]
    pub tools: Vec<PluginToolSpec>,
    /// 应用启动时自动启动（默认 true：安装即生效）
    #[serde(default = "default_true")]
    pub autostart: bool,
    /// 插件领域标签（如 "search" / "git" / "data-viz"）：核心层领域增强
    /// 插件的分类声明，UI 按领域归组展示（不经系统提示注入）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub domain: Option<String>,
    /// 插件提供的主题文件（相对插件目录的 JSON 路径，格式见 ui::theme）。
    /// 主题经 ThemeManager 合并进全局主题名单，用户在设置页选用。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub theme: Option<String>,
}

fn default_true() -> bool {
    true
}

/// 插件运行状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginStatus {
    Stopped,
    Starting,
    Running,
    Failed(String),
}

impl PluginStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            PluginStatus::Stopped => "stopped",
            PluginStatus::Starting => "starting",
            PluginStatus::Running => "running",
            PluginStatus::Failed(_) => "failed",
        }
    }
}

/// 一次工具调用的响应（reader 线程 → pending oneshot）。
pub struct PluginToolResult {
    pub ok: bool,
    pub value: Value,
    pub stderr: String,
}

/// 插件运行时（一个子进程 + 读写通道）。
pub struct PluginRuntime {
    pub manifest: PluginManifest,
    pub dir: PathBuf,
    pub status: PluginStatus,
    /// 握手上报/静态声明的工具
    pub tools: Vec<PluginToolSpec>,
    /// 握手上报的服务名
    pub services: Vec<String>,
    child: Option<Child>,
    stdin: Option<Mutex<ChildStdin>>,
    /// 响应 id → oneshot sender
    pending: Mutex<HashMap<u64, tokio::sync::oneshot::Sender<PluginToolResult>>>,
    next_id: std::sync::atomic::AtomicU64,
    /// reader 线程退出的信号（可选）
    #[allow(dead_code)]
    reader_rx: Option<Receiver<String>>,
    /// 启动时间（握手超时检测用）
    started_at: Option<std::time::Instant>,
}

/// 握手超时：启动后这么久未收到 plugin.ready → 判定 Failed。
const HANDSHAKE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

impl PluginRuntime {
    pub fn spawn(manifest: PluginManifest, dir: PathBuf) -> Self {
        Self {
            manifest,
            dir,
            status: PluginStatus::Stopped,
            tools: Vec::new(),
            services: Vec::new(),
            child: None,
            stdin: None,
            pending: Mutex::new(HashMap::new()),
            next_id: std::sync::atomic::AtomicU64::new(1),
            reader_rx: None,
            started_at: None,
        }
    }

    /// 启动子进程并发送 initialize。
    pub fn start(&mut self) -> Result<(), String> {
        if self.child.is_some() {
            return Ok(());
        }
        let cmd_vec = self.manifest.command.clone();
        if cmd_vec.is_empty() {
            // 空 command（用户手写 plugin.json 的常见错误）：不能 panic UI 线程
            return Err(format!(
                "插件 {} 的 command 为空（plugin.json 需要至少一个可执行程序名）",
                self.manifest.name
            ));
        }
        let cwd = self.dir.clone();
        // 相对路径按插件目录解析（manifest 文档语义；不解析会落到应用 CWD）
        let prog = std::path::Path::new(&cmd_vec[0]);
        let prog_resolved = if prog.is_relative() {
            let joined = cwd.join(prog);
            if joined.exists() {
                joined
            } else {
                // 不存在也保持原样（可能依赖 PATH），让 spawn 报可读错误
                prog.to_path_buf()
            }
        } else {
            prog.to_path_buf()
        };
        let mut cmd = Command::new(&prog_resolved);
        cmd.args(&cmd_vec[1..])
            .current_dir(&cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        crate::util::hide_console(&mut cmd);
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("启动插件 {} 失败: {e}", self.manifest.name))?;
        let stdin = child.stdin.take().ok_or("插件 stdin 不可用")?;
        let stdout = child.stdout.take().ok_or("插件 stdout 不可用")?;
        let stderr = child.stderr.take();
        self.stdin = Some(Mutex::new(stdin));
        self.status = PluginStatus::Starting;
        self.started_at = Some(std::time::Instant::now());
        // 工具初始 = 静态声明（插件可在 plugin.ready 动态补充）
        self.tools = self.manifest.tools.clone();
        // initialize 请求
        let init = RpcRequest::call(
            self.next_id(),
            "initialize",
            serde_json::json!({
                "pluginDir": self.dir.display().to_string(),
                "protocolVersion": 1,
            }),
        );
        let _ = self.send_line(&init);
        // reader 线程：stdout 行 → channel（pump 里解析）
        let name = self.manifest.name.clone();
        let (tx, rx) = channel::<String>();
        self.reader_rx = Some(rx);
        let mut reader = BufReader::new(stdout);
        std::thread::Builder::new()
            .name(format!("plugin-read-{name}"))
            .spawn(move || {
                let mut line = String::new();
                loop {
                    line.clear();
                    match reader.read_line(&mut line) {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {
                            // 行上限（历史缺陷：插件输出超长单行/二进制
                            // 可让 String 无限膨胀）。字符边界安全截断：
                            // String::truncate 在非边界时 panic → 读线程
                            // 死 → 插件假活（Running 但 IO 断）
                            if line.len() > 1_000_000 {
                                let mut cut = 1_000_000;
                                while cut > 0 && !line.is_char_boundary(cut) {
                                    cut -= 1;
                                }
                                line.truncate(cut);
                                line.push('\n');
                            }
                            let _ = tx.send(line.clone());
                        }
                    }
                }
            })
            .ok();
        // stderr 排水线程：不读会在子进程写满管道缓冲（~64KB）后阻塞
        if let Some(err) = stderr {
            let name2 = name.clone();
            std::thread::Builder::new()
                .name(format!("plugin-err-{name2}"))
                .spawn(move || {
                    let mut reader = BufReader::new(err);
                    let mut line = String::new();
                    loop {
                        line.clear();
                        match reader.read_line(&mut line) {
                            Ok(0) | Err(_) => break,
                            Ok(_) => {
                                log::debug!(
                                    "plugin {name2} stderr: {}",
                                    line.trim_end().chars().take(400).collect::<String>()
                                );
                            }
                        }
                    }
                })
                .ok();
        }
        self.child = Some(child);
        Ok(())
    }

    fn next_id(&self) -> u64 {
        self.next_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }

    fn send_line(&self, req: &RpcRequest) -> Result<(), String> {
        let line = serde_json::to_string(req).map_err(|e| e.to_string())?;
        let mut stdin = self.stdin.as_ref().ok_or("插件未启动")?.lock().unwrap();
        stdin
            .write_all(line.as_bytes())
            .and_then(|_| stdin.write_all(b"\n"))
            .and_then(|_| stdin.flush())
            .map_err(|e| format!("写入插件失败: {e}"))
    }

    /// 准备一次工具调用：注册响应通道 + 发送请求（同步、锁内执行），
    /// 返回 (id, oneshot Receiver) 供调用方在锁外 await。
    pub fn prepare_invoke(
        &self,
        name: &str,
        args: &Value,
    ) -> Result<(u64, tokio::sync::oneshot::Receiver<PluginToolResult>), String> {
        let id = self.next_id();
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        let req = RpcRequest::call(
            id,
            "tool.invoke",
            serde_json::json!({"name": name, "args": args}),
        );
        if let Err(e) = self.send_line(&req) {
            // 发送失败必须清理刚注册的通道（防 pending 泄漏）
            self.pending.lock().unwrap().remove(&id);
            return Err(e);
        }
        Ok((id, rx))
    }

    /// 清理一个 pending 响应通道（超时/完成时调用）。
    pub fn cancel_pending(&self, id: u64) {
        self.pending.lock().unwrap().remove(&id);
    }

    /// 调用插件工具（异步：发请求 → 等响应）。
    pub async fn invoke_tool(&self, name: &str, args: &Value) -> Result<PluginToolResult, String> {
        let (id, rx) = self.prepare_invoke(name, args)?;
        let result = match tokio::time::timeout(std::time::Duration::from_secs(120), rx).await {
            Ok(Ok(r)) => Ok(r),
            Ok(Err(_)) => Err(format!("插件工具 {name} 通道关闭")),
            Err(_) => Err(format!("插件工具 {name} 超时")),
        };
        // 成功/超时/通道关闭统一清理 pending（`?` 提前返回会泄漏注册项）
        self.pending.lock().unwrap().remove(&id);
        result
    }

    /// 是否有等待中的响应或未完成的握手（UI 据此安排重绘）。
    pub fn has_live_work(&self) -> bool {
        if self.status == PluginStatus::Starting {
            return true;
        }
        !self.pending.lock().unwrap().is_empty()
    }

    /// 处理来自插件的一行 JSON。
    pub fn handle_line(&mut self, line: &str) {
        let parsed: Result<RpcIn, _> = serde_json::from_str(line);
        match parsed {
            Ok(RpcIn::Response(resp)) => {
                if let Some(tx) = self.pending.lock().unwrap().remove(&resp.id) {
                    let result = PluginToolResult {
                        ok: resp.error.is_none(),
                        value: resp.result.clone().unwrap_or_else(|| serde_json::json!({})),
                        stderr: resp
                            .error
                            .map(|e| format!("{:?}", e.message))
                            .unwrap_or_default(),
                    };
                    let _ = tx.send(result);
                }
            }
            Ok(RpcIn::Notification(notif)) => match notif.method.as_str() {
                "plugin.ready" => {
                    if let Ok(ready) = serde_json::from_value::<PluginReady>(notif.params) {
                        self.status = PluginStatus::Running;
                        if !ready.tools.is_empty() {
                            self.tools = ready.tools;
                        }
                        self.services = ready.services;
                        self.started_at = None;
                        log::info!(
                            "plugin {} ready: {} tools, {} services",
                            self.manifest.name,
                            self.tools.len(),
                            self.services.len()
                        );
                    }
                }
                "plugin.event" => {
                    log::debug!("plugin event: {:?}", notif.params);
                }
                _ => {}
            },
            Err(e) => {
                log::warn!(
                    "插件 {} 输出非 JSON 行: {e}（{}）",
                    self.manifest.name,
                    line.chars().take(80).collect::<String>()
                );
            }
        }
    }

    pub fn is_alive(&mut self) -> bool {
        match &mut self.child {
            Some(c) => c.try_wait().map(|s| s.is_none()).unwrap_or(false),
            None => false,
        }
    }

    pub fn stop(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
        self.stdin = None;
        self.status = PluginStatus::Stopped;
        self.services.clear();
        self.started_at = None;
        // 唤醒所有等待响应的调用方（drop sender → receiver 收到通道关闭错误，
        // 否则它们要等满 120s 超时）
        self.pending.lock().unwrap().clear();
    }
}

/// 插件管理器。
#[derive(Default)]
pub struct PluginManager {
    /// 插件名 → 运行时
    pub plugins: HashMap<String, PluginRuntime>,
    /// 插件目录（$DSH_HOME/plugins）
    pub dir: PathBuf,
}

impl PluginManager {
    pub fn new(dir: PathBuf) -> Self {
        Self {
            plugins: HashMap::new(),
            dir,
        }
    }

    /// 发现并登记插件（不启动）。
    pub fn discover(&mut self) {
        if !self.dir.is_dir() {
            return;
        }
        if let Ok(entries) = std::fs::read_dir(&self.dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if !path.is_dir() {
                    continue;
                }
                let manifest_path = path.join("plugin.json");
                if !manifest_path.is_file() {
                    continue;
                }
                let Ok(text) = std::fs::read_to_string(&manifest_path) else {
                    continue;
                };
                let Ok(manifest) = serde_json::from_str::<PluginManifest>(&text) else {
                    log::warn!("插件清单解析失败: {}", manifest_path.display());
                    continue;
                };
                if !self.plugins.contains_key(&manifest.name) {
                    log::info!("plugin discovered: {} ({})", manifest.name, path.display());
                    self.plugins
                        .insert(manifest.name.clone(), PluginRuntime::spawn(manifest, path));
                }
            }
        }
    }

    /// 插件目录（不存在则创建）。
    pub fn ensure_dir(&mut self) -> PathBuf {
        if !self.dir.is_dir() {
            let _ = std::fs::create_dir_all(&self.dir);
        }
        self.dir.clone()
    }

    /// 热重载指定插件：重新读 manifest → 停止旧进程 → 替换运行时 → 重新启动。
    /// 插件文件（plugin.json / 脚本）修改后调用，无需重启应用。
    pub fn reload(&mut self, name: &str) {
        // 停止旧进程（若运行中）
        if let Some(p) = self.plugins.get_mut(name) {
            p.stop();
        }
        // 重新读 manifest
        let path = self.dir.join(name);
        let manifest_path = path.join("plugin.json");
        let Ok(text) = std::fs::read_to_string(&manifest_path) else {
            log::warn!("reload {}: 无法读取 plugin.json", name);
            return;
        };
        let manifest = match serde_json::from_str::<PluginManifest>(&text) {
            Ok(m) => m,
            Err(e) => {
                log::warn!("reload {}: manifest 解析失败: {e}", name);
                if let Some(p) = self.plugins.get_mut(name) {
                    p.status = PluginStatus::Failed(format!("manifest 解析失败: {e}"));
                }
                return;
            }
        };
        // manifest.name 可能与目录名不同：用 manifest.name 作键
        let key = manifest.name.clone();
        if let Some(p) = self.plugins.get_mut(&key) {
            p.stop();
        }
        let mut rt = PluginRuntime::spawn(manifest, path);
        if let Err(e) = rt.start() {
            rt.status = PluginStatus::Failed(e);
        }
        log::info!("plugin {key} reloaded (hot)");
        self.plugins.insert(key, rt);
    }

    /// 启动所有插件（自动发现后逐个 start + initialize）。
    pub fn start_all(&mut self) {
        self.discover();
        let names: Vec<String> = self.plugins.keys().cloned().collect();
        for name in names {
            if let Some(p) = self.plugins.get_mut(&name) {
                if let Err(e) = p.start() {
                    p.status = PluginStatus::Failed(e);
                }
            }
        }
    }

    /// 单帧泵：读取各插件输出（reader_rx），处理响应/通知，清理退出进程。
    pub fn pump(&mut self) {
        let names: Vec<String> = self.plugins.keys().cloned().collect();
        for name in names {
            let mut should_clean = false;
            // 处理 reader 线程积压的行
            let lines: Vec<String> = self
                .plugins
                .get(&name)
                .and_then(|p| p.reader_rx.as_ref())
                .map(|rx| {
                    let mut out = Vec::new();
                    while let Ok(l) = rx.try_recv() {
                        out.push(l);
                    }
                    out
                })
                .unwrap_or_default();
            if let Some(p) = self.plugins.get_mut(&name) {
                for l in &lines {
                    p.handle_line(l);
                }
                // 进程退出检测
                if p.status != PluginStatus::Stopped
                    && !matches!(p.status, PluginStatus::Failed(_))
                    && !p.is_alive()
                {
                    log::warn!("plugin {name} exited unexpectedly");
                    p.status = PluginStatus::Failed("进程退出".into());
                    should_clean = true;
                }
                // 握手超时：Starting 超过 10s 未 ready → Failed
                if p.status == PluginStatus::Starting {
                    if let Some(t) = p.started_at {
                        if t.elapsed() > HANDSHAKE_TIMEOUT {
                            log::warn!("plugin {name} handshake timeout");
                            p.status =
                                PluginStatus::Failed("握手超时（未收到 plugin.ready）".into());
                            should_clean = true;
                        }
                    }
                }
            }
            if should_clean {
                if let Some(p) = self.plugins.get_mut(&name) {
                    // 已有具体失败原因（握手超时等）时保留，不被"进程退出"覆盖
                    let reason = match &p.status {
                        PluginStatus::Failed(r) => r.clone(),
                        _ => "进程退出".into(),
                    };
                    p.stop();
                    p.status = PluginStatus::Failed(reason);
                }
            }
        }
    }

    /// 插件提供的全部工具（agent 回合合并进工具集）。
    pub fn all_tools(&self) -> Vec<PluginToolSpec> {
        self.plugins
            .values()
            .filter(|p| p.status == PluginStatus::Running)
            .flat_map(|p| p.tools.clone())
            .collect()
    }

    /// 是否有插件正在握手或等待工具响应（UI 据此安排定时重绘，
    /// 响应到达不依赖鼠标移动）。
    pub fn has_live_work(&self) -> bool {
        self.plugins.values().any(|p| p.has_live_work())
    }

    /// 插件提供的主题文件绝对路径列表（manifest.theme 相对插件目录解析）。
    /// ThemeManager 读取这些文件合并进主题名单（插件可发布主题）。
    pub fn theme_files(&self) -> Vec<std::path::PathBuf> {
        self.plugins
            .values()
            .filter_map(|p| {
                p.manifest
                    .theme
                    .as_ref()
                    .map(|rel| p.dir.join(rel))
                    .filter(|f| f.is_file())
            })
            .collect()
    }

    /// 插件领域声明列表：(插件名, 领域) —— UI 归组展示领域增强插件。
    pub fn domains(&self) -> Vec<(String, String)> {
        self.plugins
            .values()
            .filter_map(|p| {
                p.manifest
                    .domain
                    .as_ref()
                    .map(|d| (p.manifest.name.clone(), d.clone()))
            })
            .collect()
    }

    /// 找到提供指定工具的运行中插件。
    pub fn find_tool_owner(&self, name: &str) -> Option<&PluginRuntime> {
        self.plugins
            .values()
            .find(|p| p.status == PluginStatus::Running && p.tools.iter().any(|t| t.name == name))
    }

    /// 快照（UI 展示）：(name, status, tool_names, description)。
    pub fn snapshot(&self) -> Vec<(String, PluginStatus, Vec<String>, String)> {
        let mut v: Vec<_> = self
            .plugins
            .values()
            .map(|p| {
                (
                    p.manifest.name.clone(),
                    p.status.clone(),
                    p.tools.iter().map(|t| t.name.clone()).collect(),
                    p.manifest
                        .description
                        .clone()
                        .map(|d| d)
                        .unwrap_or_default(),
                )
            })
            .collect();
        v.sort_by(|a, b| a.0.cmp(&b.0));
        v
    }

    pub fn get(&self, name: &str) -> Option<&PluginRuntime> {
        self.plugins.get(name)
    }

    pub fn get_mut(&mut self, name: &str) -> Option<&mut PluginRuntime> {
        self.plugins.get_mut(name)
    }

    /// 从注册表移除（不删文件；删除本地插件用——进程停止后调用）。
    pub fn remove(&mut self, name: &str) -> Option<PluginRuntime> {
        self.plugins.remove(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 写一个最小 Python 测试插件（握手 + 一个 echo 工具）。
    fn write_py_plugin(dir: &PathBuf) -> String {
        std::fs::create_dir_all(dir.join("demo")).unwrap();
        std::fs::write(
            dir.join("demo").join("plugin.json"),
            r#"{
                "name": "demo",
                "description": "测试插件",
                "command": ["python", "plugin.py"]
            }"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("demo").join("plugin.py"),
            r#"import sys, json

def main():
    try:
        sys.stdin.reconfigure(encoding='utf-8')
        sys.stdout.reconfigure(encoding='utf-8')
    except Exception:
        pass
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            msg = json.loads(line)
        except Exception:
            continue
        method = msg.get("method", "")
        if method == "initialize":
            # 响应 + 上报 ready（工具：echo）
            out = {"jsonrpc": "2.0", "id": msg.get("id"), "result": {"ok": True}}
            sys.stdout.write(json.dumps(out) + "\n")
            ready = {"jsonrpc": "2.0", "method": "plugin.ready",
                     "params": {"tools": [{"name": "echo", "description": "echo 工具", "parameters": {"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]}}],
                                "services": ["demo.svc"]}}
            sys.stdout.write(json.dumps(ready) + "\n")
            sys.stdout.flush()
        elif method == "tool.invoke":
            params = msg.get("params", {})
            name = params.get("name", "")
            args = params.get("args", {})
            if name == "echo":
                result = {"jsonrpc": "2.0", "id": msg.get("id"),
                          "result": {"echoed": args.get("text", "")}}
            else:
                result = {"jsonrpc": "2.0", "id": msg.get("id"),
                          "error": {"code": -32601, "message": "unknown tool"}}
            sys.stdout.write(json.dumps(result) + "\n")
            sys.stdout.flush()

if __name__ == "__main__":
    main()
"#,
        )
        .unwrap();
        dir.join("demo").to_string_lossy().into_owned()
    }

    /// 端到端：发现 → 启动 → 握手（plugin.ready）→ 工具调用 echo。
    #[test]
    fn plugin_handshake_and_tool_invoke() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let td = tempfile::tempdir().unwrap();
            let dir = td.path().join("plugins");
            std::fs::create_dir_all(&dir).unwrap();
            write_py_plugin(&dir);

            let mut mgr = PluginManager::new(dir.clone());
            mgr.discover();
            assert_eq!(mgr.plugins.len(), 1);
            let name = mgr.plugins.keys().next().unwrap().clone();

            mgr.start_all();
            // 泵几轮，等 plugin.ready
            for _ in 0..100 {
                mgr.pump();
                if mgr
                    .get(&name)
                    .map(|p| p.status == PluginStatus::Running)
                    .unwrap_or(false)
                {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            let p = mgr.get(&name).unwrap();
            assert_eq!(
                p.status,
                PluginStatus::Running,
                "插件应完成握手进入 running"
            );
            assert!(
                p.tools.iter().any(|t| t.name == "echo"),
                "插件应上报 echo 工具"
            );
            assert!(p.services.contains(&"demo.svc".to_string()));

            // 工具调用：prepare（发请求）→ 主循环 pump + try_recv 轮询
            let (_id, mut rx) = {
                let p = mgr.get(&name).unwrap();
                p.prepare_invoke("echo", &serde_json::json!({"text": "你好"}))
                    .expect("prepare 应成功")
            };
            let result = loop {
                mgr.pump();
                match rx.try_recv() {
                    Ok(r) => break r,
                    Err(_) => tokio::time::sleep(std::time::Duration::from_millis(10)).await,
                }
            };
            assert!(result.ok);
            eprintln!("echo result: {:?}", result.value);
            assert_eq!(
                result.value.get("echoed").and_then(|v| v.as_str()),
                Some("你好")
            );

            // 停止
            mgr.get_mut(&name).unwrap().stop();
            assert_eq!(mgr.get(&name).unwrap().status, PluginStatus::Stopped);
        });
    }

    /// manifest 的领域（domain）与主题（theme）声明：核心层增强插件可声明
    /// 所属领域与随附主题文件，供 UI 归组展示与主题系统合并。
    #[test]
    fn manifest_domain_and_theme_parsed() {
        let dir = tempfile::tempdir().unwrap();
        let pdir = dir.path().join("enhanced");
        std::fs::create_dir_all(&pdir).unwrap();
        std::fs::write(
            pdir.join("plugin.json"),
            r##"{
                "name": "enhanced",
                "command": ["echo"],
                "domain": "search",
                "theme": "themes/search-dark.json"
            }"##,
        )
        .unwrap();
        // 主题文件存在才会被收集
        std::fs::create_dir_all(pdir.join("themes")).unwrap();
        std::fs::write(
            pdir.join("themes").join("search-dark.json"),
            r##"{"name": "search-dark", "accent": "#3B82F6"}"##,
        )
        .unwrap();
        let mut mgr = PluginManager::new(dir.path().to_path_buf());
        mgr.discover();
        // 领域声明
        let domains = mgr.domains();
        assert!(
            domains.contains(&("enhanced".to_string(), "search".to_string())),
            "domain 应解析: {domains:?}"
        );
        // 主题文件（相对插件目录解析，存在才收集）
        let themes = mgr.theme_files();
        assert_eq!(themes.len(), 1, "theme 文件应被收集: {themes:?}");
        assert!(themes[0].ends_with("search-dark.json"));
        // 无 domain/theme 的普通插件不受影响
        let plain = PluginManifest {
            name: "plain".into(),
            description: None,
            version: None,
            command: vec!["echo".into()],
            tools: Vec::new(),
            autostart: false,
            domain: None,
            theme: None,
        };
        let mut mgr2 = PluginManager::default();
        mgr2.plugins.insert(
            "plain".into(),
            PluginRuntime::spawn(plain, dir.path().to_path_buf()),
        );
        assert!(mgr2.domains().is_empty());
        assert!(mgr2.theme_files().is_empty());
    }

    /// 空命令防御：`"command": []` 返回错误而不是 panic UI 线程。
    #[test]
    fn empty_command_rejected_not_panic() {
        let mut rt = PluginRuntime::spawn(
            PluginManifest {
                name: "empty".into(),
                description: None,
                version: None,
                command: Vec::new(),
                tools: Vec::new(),
                autostart: false,
                domain: None,
                theme: None,
            },
            std::env::temp_dir(),
        );
        let r = rt.start();
        assert!(r.is_err(), "空 command 必须报错");
        assert!(r.unwrap_err().contains("command 为空"), "错误应说明原因");
    }
}
