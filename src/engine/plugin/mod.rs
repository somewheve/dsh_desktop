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
        let cwd = self.dir.clone();
        let mut cmd = Command::new(&cmd_vec[0]);
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
                            let _ = tx.send(line.clone());
                        }
                    }
                }
            })
            .ok();
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
        self.send_line(&req)?;
        Ok((id, rx))
    }

    /// 清理一个 pending 响应通道（超时/完成时调用）。
    pub fn cancel_pending(&self, id: u64) {
        self.pending.lock().unwrap().remove(&id);
    }

    /// 调用插件工具（异步：发请求 → 等响应）。
    pub async fn invoke_tool(&self, name: &str, args: &Value) -> Result<PluginToolResult, String> {
        let (id, rx) = self.prepare_invoke(name, args)?;
        let result = tokio::time::timeout(std::time::Duration::from_secs(120), rx)
            .await
            .map_err(|_| format!("插件工具 {name} 超时"))?
            .map_err(|_| format!("插件工具 {name} 通道关闭"))?;
        self.pending.lock().unwrap().remove(&id);
        Ok(result)
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
                if p.status != PluginStatus::Stopped && !p.is_alive() {
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
                    p.stop();
                    p.status = PluginStatus::Failed("进程退出".into());
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
}
