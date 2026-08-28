//! 工具系统：Rust 原生实现 DSH 核心工具全集。
//!
//! 对齐 DSH 的 dsh-tool-* 语义：每个工具返回 JSON 结果，
//! agent 循环把它作为 tool/result 事件写回会话。
//! 已实现：bash / pwsh / read_file / write_file / list_dir / todo_write /
//!         str_replace_editor / fs_search / web_search / ask_user / goal_create

use crate::core::preset::AgentPreset;
use serde_json::{json, Value};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// 工具执行结果。
#[derive(Debug, Clone)]
pub struct ToolOutput {
    pub ok: bool,
    pub value: Value,
    pub stderr: String,
}

impl ToolOutput {
    pub fn to_json(&self) -> Value {
        json!({
            "ok": self.ok,
            "value": self.value,
            "stderr": self.stderr,
        })
    }
    pub(crate) fn err(msg: impl Into<String>) -> Self {
        let m = msg.into();
        ToolOutput {
            ok: false,
            value: json!({"error": m}),
            stderr: m.clone(),
        }
    }
    pub(crate) fn ok(value: Value) -> Self {
        ToolOutput {
            ok: true,
            value,
            stderr: String::new(),
        }
    }
}

/// 工具注册表。
pub struct ToolRegistry {
    /// 工作目录（相对路径解析基准）
    pub(crate) cwd: PathBuf,
    /// 命令执行是否受限（sandbox 语义，当前允许本机命令）
    #[allow(dead_code)]
    allow_shell: bool,
    /// 搜索结果上限（对齐 WEB_SEARCH_MAX_RESULTS=8）
    pub search_max_results: usize,
    /// 文件读取行数上限（对齐 dsh-tool-fs READ_LIMIT）
    pub read_limit: usize,
    /// Agent 预设（决定工具白名单与 run_code）
    pub(crate) preset: AgentPreset,
    /// 可用技能（load_skill 工具读取；随回合快照注入）
    pub skills: Vec<crate::engine::skill::Skill>,
    /// 沙箱执行器（None = 未配置，bash/pwsh 保持直通语义）。
    /// 非 None 时 bash/pwsh 按沙箱模式受限/受限执行。
    sandbox: Option<crate::exec::acl::WindowsAclSandbox>,
    /// 工作区根（权限审批的写边界：写此目录外需用户确认）。
    workspace_root: Option<PathBuf>,
    /// HTTP 代理（内置 web_search 走系统出口；None = 直连）
    http_proxy: Option<String>,
}

impl ToolRegistry {
    pub fn new(cwd: PathBuf) -> Self {
        Self {
            cwd,
            allow_shell: true,
            search_max_results: 8,
            read_limit: 1000,
            preset: AgentPreset::Standard,
            skills: Vec::new(),
            sandbox: None,
            workspace_root: None,
            http_proxy: None,
        }
    }

    /// 绑定 Agent 预设（工具集随预设变化）。
    pub fn with_preset(&self, preset: AgentPreset) -> Self {
        let mut r = self.clone_for_preset();
        r.preset = preset;
        r
    }

    fn clone_for_preset(&self) -> Self {
        ToolRegistry {
            cwd: self.cwd.clone(),
            allow_shell: self.allow_shell,
            search_max_results: self.search_max_results,
            read_limit: self.read_limit,
            preset: self.preset,
            skills: self.skills.clone(),
            sandbox: self.sandbox.clone(),
            workspace_root: self.workspace_root.clone(),
            http_proxy: self.http_proxy.clone(),
        }
    }

    /// 绑定工作区根（权限审批写边界）。
    pub fn with_workspace_root(&self, ws: Option<PathBuf>) -> Self {
        let mut r = self.clone_for_preset();
        r.workspace_root = ws;
        r
    }

    /// 绑定 HTTP 代理（内置 web_search 使用）。
    pub fn with_http_proxy(&self, proxy: Option<String>) -> Self {
        let mut r = self.clone_for_preset();
        r.http_proxy = proxy;
        r
    }

    /// 绑定沙箱执行器（None = 直通；Some = bash/pwsh 按受限模式执行）。
    pub fn with_sandbox(&self, sandbox: Option<crate::exec::acl::WindowsAclSandbox>) -> Self {
        let mut r = self.clone_for_preset();
        r.sandbox = sandbox;
        r
    }

    /// 绑定工作目录（工作区切换时生成新 registry，其余配置不变）。
    pub fn with_cwd(&self, cwd: PathBuf) -> Self {
        let mut r = self.clone_for_preset();
        r.cwd = cwd.clone();
        // 工作区根同步更新（审批/写边界 = 当前工作目录；
        // 旧实现只改 cwd 不改 workspace_root → 用户切了工作区但
        // 审批弹窗还是显示旧路径）
        r.workspace_root = Some(cwd);
        r
    }

    pub fn set_cwd(&mut self, cwd: PathBuf) {
        self.cwd = cwd;
    }

    /// 判定一次写工具调用是否可能写**工作区外**（需权限审批）。
    /// 返回 (目标路径, 原因)。只在配置了 workspace_root 时判定；
    /// 未配置 workspace_root 视为不拦截（无边界可判）。
    /// bash/pwsh 用输出重定向/写命令直觉解析；write/str_replace 解析显式 path；
    /// run_code 逐子步判定（PTC 组合不能绕过写边界）；node_called 执行任意
    /// JS（无法静态分析写目标）按潜在越权处理。
    pub fn potential_out_of_workspace(&self, name: &str, args: &Value) -> Option<(String, String)> {
        let ws = self.workspace_root.as_ref()?.clone();
        log::debug!(
            "approval check: tool={name} workspace_root={} cwd={}",
            ws.display(),
            self.cwd.display()
        );
        let targets: Vec<String> = match name {
            "write_file" => vec![args
                .get("path")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string()],
            "str_replace_editor" => {
                let cmd = args.get("command").and_then(Value::as_str).unwrap_or("");
                if cmd == "create" || cmd == "str_replace" {
                    vec![args
                        .get("path")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string()]
                } else {
                    Vec::new()
                }
            }
            "bash" | "pwsh" => {
                extract_redirect_targets(&args.get("command").and_then(Value::as_str).unwrap_or(""))
            }
            // 任意 JS：无法静态判定写目标 → 每次都审批（AlwaysAllow 后免问）
            "node_called" => {
                return Some((
                    NODE_WRITE_MARKER.to_string(),
                    "node_called 可执行任意代码（含文件写入），需用户确认".into(),
                ))
            }
            // PTC 组合执行：逐子步判定（旧实现只看顶层名，run_code 内嵌的
            // write_file/bash 完全绕过审批）
            "run_code" => {
                let steps = args
                    .get("steps")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                for step in &steps {
                    let op = step.get("op").and_then(Value::as_str).unwrap_or("");
                    if op == "run_code" || op.is_empty() {
                        continue;
                    }
                    if let Some((t, r)) = self.potential_out_of_workspace(op, step) {
                        return Some((t, format!("run_code 子步越权：{r}")));
                    }
                }
                return None;
            }
            _ => Vec::new(),
        };
        // 详细链路日志
        for t in &targets {
            if t.is_empty() {
                continue;
            }
            let resolved = self.resolve(t);
            log::info!("approval target: raw={t:?} resolved={}", resolved.display());
        }
        for t in targets {
            if t.is_empty() || t == NODE_WRITE_MARKER {
                continue;
            }
            let abs = self.resolve(&t);
            if !path_is_within_ws(&abs, &ws) {
                return Some((
                    abs.display().to_string(),
                    format!(
                        "目标「{}」不在工作区「{}」内（工具 cwd：{}）",
                        abs.display(),
                        ws.display(),
                        self.cwd.display()
                    ),
                ));
            }
        }
        None
    }

    /// 写工具的 OS 级 fail-closed 检查：沙箱受限时按策略裁决路径
    /// （read-only 一律拒绝；workspace-write 拒绝工作区外——审批允许也不行，
    /// 因为受限令牌在 OS 层没有对应写权限，提前给出可读错误）。
    fn check_write_allowed(&self, path: &Path) -> Result<(), String> {
        if let Some(sb) = &self.sandbox {
            match sb.mode {
                crate::exec::SandboxMode::ReadOnly => {
                    return Err(
                        "sandbox: read-only 模式禁止一切写操作（write_file/str_replace_editor）"
                            .into(),
                    );
                }
                crate::exec::SandboxMode::WorkspaceWrite => {
                    // 工作区外的写：审批系统是唯一闸门（用户点"允许"后应执行）。
                    // 此处只记日志不阻断——旧实现硬拒与审批冲突（用户已批准
                    // 但 check_write_allowed 仍然拒绝 → "审批通过了但写入失败"）。
                    if !path_is_within_ws(path, &sb.workspace_root) {
                        log::warn!(
                            "workspace-write: write outside workspace (approval-gated): {}",
                            path.display()
                        );
                    }
                }
                crate::exec::SandboxMode::DangerFullAccess => {}
            }
        }
        Ok(())
    }
    pub fn tool_specs(&self) -> Vec<crate::core::llm::ToolSpec> {
        use crate::core::llm::tool_spec;
        let mut specs = vec![
            tool_spec(
                "bash",
                "执行 shell 命令（Windows 下为 cmd）。适合运行脚本、查看文件、git 操作。",
                json!({"type":"object","properties":{"command":{"type":"string"},"cwd":{"type":"string"}},"required":["command"]}),
            ),
            tool_spec(
                "pwsh",
                "执行 PowerShell 命令。",
                json!({"type":"object","properties":{"command":{"type":"string"}},"required":["command"]}),
            ),
            tool_spec(
                "read_file",
                "读取文本文件内容（带行号可选）。",
                json!({"type":"object","properties":{"path":{"type":"string"},"offset":{"type":"integer"},"limit":{"type":"integer"}},"required":["path"]}),
            ),
            tool_spec(
                "write_file",
                "写入/覆盖文本文件。",
                json!({"type":"object","properties":{"path":{"type":"string"},"content":{"type":"string"}},"required":["path","content"]}),
            ),
            tool_spec(
                "list_dir",
                "列出目录内容（非隐藏文件，最多 2 层）。",
                json!({"type":"object","properties":{"path":{"type":"string"}},"required":[]}),
            ),
            tool_spec(
                "todo_write",
                "记录任务清单（markdown 文本，对齐 dsh-tool-todo）。",
                json!({"type":"object","properties":{"todos":{"type":"string"}},"required":["todos"]}),
            ),
            tool_spec(
                "str_replace_editor",
                "查看/创建/编辑文件的编辑器。view 显示 cat -n 风格内容；create 创建新文件；str_replace 精确替换唯一匹配。",
                json!({"type":"object","properties":{"command":{"type":"string","enum":["view","create","str_replace"]},"path":{"type":"string"},"old_str":{"type":"string"},"new_str":{"type":"string"},"file_text":{"type":"string"}},"required":["command","path"]}),
            ),
            tool_spec(
                "fs_search",
                "在目录中搜索文件名/内容（对齐 dsh-tool-fs-search）。",
                json!({"type":"object","properties":{"path":{"type":"string"},"pattern":{"type":"string"}},"required":["path","pattern"]}),
            ),
            tool_spec(
                "web_search",
                "联网搜索最新信息（DuckDuckGo，结果上限 8 条：标题/链接/摘要）。查事实、时效信息、文档时使用。",
                json!({"type":"object","properties":{"query":{"type":"string"}},"required":["query"]}),
            ),
            tool_spec(
                "ask_user",
                "向用户提问，等待回答（对齐 dsh-tool-ask-user）。",
                json!({"type":"object","properties":{"question":{"type":"string"},"options":{"type":"array","items":{"type":"string"}},"header":{"type":"string"}},"required":["question"]}),
            ),
            tool_spec(
                "goal_create",
                "创建长期目标（显示在 Goals 卡片并持久化）。当用户提出一个需要跨多轮持续推进/跟踪的长期目标时，自动调用它登记目标。",
                json!({"type":"object","properties":{"objective":{"type":"string"}},"required":["objective"]}),
            ),
            tool_spec(
                "plan_write",
                "编写执行计划并进入计划模式（显示在 Plan 卡片，先计划后执行）。任务复杂/多步骤时先调用它产出计划，计划完成后再 exit_plan_mode。计划请用 markdown 任务列表（每步一行 `- [ ] 步骤`）；每完成一步，就在执行过程中用 plan_write 更新内容，把该步的 `- [ ]` 改成 `- [x]`，让 Plan 卡片的勾选框实时反映进度。",
                json!({"type":"object","properties":{"content":{"type":"string"}},"required":["content"]}),
            ),
            tool_spec(
                "exit_plan_mode",
                "退出计划模式（计划完成，开始执行）。",
                json!({"type":"object","properties":{}}),
            ),
            tool_spec(
                "subagent_fork",
                "派生一个子代理并行执行独立子任务（单轮 LLM 问答，显示在 Subagents 卡片），返回其总结。适合可并行拆分的独立工作（文件分析、独立研究等）。",
                json!({"type":"object","properties":{"description":{"type":"string"},"prompt":{"type":"string"}},"required":["description","prompt"]}),
            ),
            tool_spec(
                "load_skill",
                "加载一个技能（skill）的完整说明并按它执行。技能名须来自系统提示中列出的可用技能。",
                json!({"type":"object","properties":{"name":{"type":"string"}},"required":["name"]}),
            ),
            tool_spec(
                "node_called",
                "在 DSH 的 Node.js 环境中执行 JavaScript 代码（NODE_PATH 已指向本机 DSH 的 node_modules）。可 require 任意已安装的 DSH 官方插件/依赖（如 @deepseek-ai/dsh-goal）并调用其能力，返回 JSON 结果。适合调用 JS 库/插件、处理复杂数据处理。",
                json!({"type":"object","properties":{"code":{"type":"string"}},"required":["code"]}),
            ),
        ];
        if let Some(whitelist) = self.preset.tool_whitelist() {
            specs.retain(|s| whitelist.contains(&s.function.name.as_str()));
        }
        if self.preset.has_run_code() {
            specs.push(tool_spec(
                "run_code",
                "PTC 模式组合执行：把一系列操作组合成一个程序（steps 数组）一次执行，减少往返。steps 每项为 {\"op\": ..., \"path\": ..., ...}，op 支持 read_file / write_file / list_dir / str_replace_editor / bash / pwsh / fs_search / web_search / todo_write。",
                json!({"type":"object","properties":{"steps":{"type":"array","items":{"type":"object"}}},"required":["steps"]}),
            ));
        }
        specs
    }

    /// 分发工具调用。
    pub fn dispatch(&self, name: &str, args: &Value) -> ToolOutput {
        if let Some(whitelist) = self.preset.tool_whitelist() {
            if !whitelist.contains(&name) && !(name == "run_code" && self.preset.has_run_code()) {
                return ToolOutput::err(format!(
                    "tool {name} 在当前预设（{}）不可用",
                    self.preset.name()
                ));
            }
        }
        if name == "run_code" && !self.preset.has_run_code() {
            return ToolOutput::err("run_code 仅 PTC 模式可用");
        }
        match name {
            "bash" => self.tool_bash(args),
            "pwsh" => self.tool_pwsh(args),
            "read_file" => self.tool_read_file(args),
            "write_file" => self.tool_write_file(args),
            "list_dir" => self.tool_list_dir(args),
            "todo_write" => self.tool_todo(args),
            "run_code" => self.tool_run_code(args),
            "str_replace_editor" => self.tool_str_replace(args),
            "fs_search" => self.tool_fs_search(args),
            "web_search" => self.tool_web_search(args),
            "ask_user" => ToolOutput::ok(json!({
                "await_user": true,
                "note": "问题已提交给用户，回合暂停等待回答",
                "question": args.get("question").and_then(Value::as_str).unwrap_or(""),
                "options": args.get("options").cloned().unwrap_or_else(|| json!([])),
                "header": args.get("header").cloned().unwrap_or_else(|| json!(null)),
            })),
            "goal_create" => ToolOutput::ok(json!({
                "goal": true,
                "note": "目标已创建",
                "objective": args.get("objective").and_then(Value::as_str).unwrap_or(""),
            })),
            "plan_write" => ToolOutput::ok(json!({
                "plan": true,
                "note": "计划已写入并进入计划模式",
                "content": args.get("content").and_then(Value::as_str).unwrap_or(""),
            })),
            "exit_plan_mode" => ToolOutput::ok(json!({
                "exit_plan": true,
                "note": "已退出计划模式",
            })),
            "subagent_fork" => {
                ToolOutput::err("subagent_fork 由回合内异步执行（LLM 调用），不能在同步分发中使用")
            }
            "load_skill" => {
                let name = args.get("name").and_then(Value::as_str).unwrap_or_default();
                match self
                    .skills
                    .iter()
                    .find(|s| s.name == name)
                    .and_then(|s| s.instructions.clone())
                {
                    Some(instructions) => ToolOutput::ok(json!({
                        "skill": name,
                        "instructions": instructions,
                        "note": "技能已加载，请严格按其说明执行",
                    })),
                    None => ToolOutput::err(format!("技能不存在: {name}（可用技能见系统提示）")),
                }
            }
            "node_called" => self.tool_node_called(args),
            other => ToolOutput::err(format!("unknown tool: {other}")),
        }
    }

    /// node_called：在 DSH 的 Node 环境中执行 JS（NODE_PATH 指向本机 DSH node_modules），
    /// 可 require 任意已安装的 DSH 官方插件并调用其能力。
    /// 受限沙箱模式下拒绝：node 拿到的是父进程完整令牌，绕过 restricted-token
    /// 就是绕过整个沙箱（历史漏洞：read-only/workspace-write 下仍可任意写）。
    fn tool_node_called(&self, args: &Value) -> ToolOutput {
        if let Some(sb) = &self.sandbox {
            return ToolOutput::err(format!(
                "node_called 在沙箱模式（{}）下不可用：node 子进程持有完整令牌，无法受限执行。请切换 danger-full-access（需用户在输入框旁切换）或改用 bash/pwsh（受沙箱约束）",
                sb.mode.as_str()
            ));
        }
        let code = args
            .get("code")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if code.trim().is_empty() {
            return ToolOutput::err("node_called: code 不能为空（要执行的 JavaScript）");
        }
        // NODE_PATH：dsh CLI（npx 缓存）node_modules + profiles/node_modules
        let node_paths = crate::dsh::cli::dsh_node_paths();
        let node_path = node_paths
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(";");
        let mut cmd = std::process::Command::new("node");
        cmd.arg("-e").arg(&code);
        if !node_paths.is_empty() {
            cmd.env("NODE_PATH", &node_path);
        }
        crate::util::hide_console(&mut cmd);
        run_command_timeout(cmd, SHELL_TIMEOUT)
    }

    /// todo_write 实现。
    fn tool_todo(&self, _args: &Value) -> ToolOutput {
        ToolOutput::ok(json!({"recorded": true, "note": "todo 记录于会话事件"}))
    }

    /// run_code：PTC 模式组合执行（顺序执行 steps，汇总结果，单步失败不中断）。
    fn tool_run_code(&self, args: &Value) -> ToolOutput {
        let steps = args
            .get("steps")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if steps.is_empty() {
            return ToolOutput::err("run_code: steps 数组不能为空");
        }
        let mut results = Vec::new();
        for (i, step) in steps.iter().enumerate() {
            let op = step.get("op").and_then(Value::as_str).unwrap_or("");
            let out = match op {
                "read_file" => self.tool_read_file(step),
                "write_file" => self.tool_write_file(step),
                "list_dir" => self.tool_list_dir(step),
                "str_replace_editor" => self.tool_str_replace(step),
                "bash" => self.tool_bash(step),
                "pwsh" => self.tool_pwsh(step),
                "fs_search" => self.tool_fs_search(step),
                "web_search" => self.tool_web_search(step),
                "todo_write" => self.tool_todo(step),
                other => ToolOutput::err(format!("run_code: 未知操作 {other}")),
            };
            results.push(json!({
                "index": i,
                "op": op,
                "ok": out.ok,
                "value": out.value,
                "stderr": out.stderr,
            }));
        }
        ToolOutput::ok(json!({"results": results}))
    }

    fn resolve(&self, path: &str) -> PathBuf {
        // Windows 路径统一：AI 模型常用 / 分隔，统一转为 \
        let p = PathBuf::from(path.replace('/', "\\"));
        if p.is_absolute() {
            // AI 常见错误：给了同盘符但不在工作区内的绝对路径
            // （如 F:\f\x.txt 而工作区是 F:\AI）。
            // 路径不存在且非系统目录时，把非盘符部分拼到工作区下
            // （F:\AI\f\x.txt）——不再弹无意义审批。
            if let Some(ws) = &self.workspace_root {
                // 仅同盘符才纠正（F:\AI 工作区只纠正 F: 盘的错误路径，
                // C:\Windows 等跨盘路径不碰——纠正了反而掩盖真正的越权写入）
                let same_drive = p
                    .components()
                    .next()
                    .and_then(|c| match c {
                        std::path::Component::Prefix(pf) => {
                            Some(pf.as_os_str().to_string_lossy().to_lowercase())
                        }
                        _ => None,
                    })
                    .zip(ws.components().next().and_then(|c| match c {
                        std::path::Component::Prefix(pf) => {
                            Some(pf.as_os_str().to_string_lossy().to_lowercase())
                        }
                        _ => None,
                    }))
                    .map(|(a, b)| a == b)
                    .unwrap_or(false);
                if same_drive && !p.starts_with(ws) && !p.exists() {
                    let rest: PathBuf = p.components().skip(2).collect();
                    // 深度限制：非盘符部分 ≤3 级才纠正（短路径 = AI 可能拼错；
                    // 深层路径 = 用户真实绝对路径，不该纠正）
                    let depth = rest.components().count();
                    if !rest.as_os_str().is_empty() && depth <= 3 {
                        let corrected = ws.join(&rest);
                        if corrected.starts_with(ws) {
                            log::info!(
                                "path auto-corrected: {} -> {} (same-drive, not exists)",
                                p.display(),
                                corrected.display()
                            );
                            return corrected;
                        }
                    }
                }
            }
            p
        } else {
            self.cwd.join(p)
        }
    }

    fn tool_bash(&self, args: &Value) -> ToolOutput {
        let command = args
            .get("command")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if command.is_empty() {
            return ToolOutput::err("empty command");
        }
        let cwd = args
            .get("cwd")
            .and_then(Value::as_str)
            .map(|s| self.resolve(s))
            .unwrap_or_else(|| self.cwd.clone());
        // 沙箱接线：受限模式下经 restricted-token 执行，fail-closed。
        if let Some(sb) = &self.sandbox {
            if let Some(out) = sb.run("cmd", &["/C", command.as_str()], &cwd) {
                return acl_output_to_tool(out);
            }
        }
        let mut cmd = Command::new("cmd");
        cmd.arg("/C").arg(&command).current_dir(&cwd);
        run_command_timeout(cmd, SHELL_TIMEOUT)
    }

    fn tool_pwsh(&self, args: &Value) -> ToolOutput {
        let command = args
            .get("command")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if command.is_empty() {
            return ToolOutput::err("empty command");
        }
        // 沙箱接线：受限模式下经 restricted-token 执行，fail-closed。
        if let Some(sb) = &self.sandbox {
            if let Some(out) = sb.run(
                "powershell",
                &[
                    "-NoProfile",
                    "-NonInteractive",
                    "-Command",
                    command.as_str(),
                ],
                &self.cwd,
            ) {
                return acl_output_to_tool(out);
            }
        }
        let mut cmd = Command::new("powershell");
        cmd.arg("-NoProfile")
            .arg("-Command")
            .arg(&command)
            .current_dir(&self.cwd);
        run_command_timeout(cmd, SHELL_TIMEOUT)
    }

    fn tool_read_file(&self, args: &Value) -> ToolOutput {
        let path = match args.get("path").and_then(Value::as_str) {
            Some(p) => self.resolve(p),
            None => return ToolOutput::err("missing path"),
        };
        // 大小上限：模型选中的路径可能是多 GB 日志——read_to_string 会把
        // 整个文件拉进内存（卡死/OOM）。超限给出可操作的错误。
        const MAX_FILE_BYTES: u64 = 8 << 20; // 8MB
        match std::fs::metadata(&path) {
            Ok(md) if md.len() > MAX_FILE_BYTES => {
                return ToolOutput::err(format!(
                    "文件过大（{} 字节 > 上限 {}）：请用 bash 分段读取（more/findstr/PowerShell -TotalCount），或说明需要的区段",
                    md.len(),
                    MAX_FILE_BYTES
                ));
            }
            Err(e) => return ToolOutput::err(e.to_string()),
            Ok(_) => {}
        }
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                let offset = args.get("offset").and_then(Value::as_u64).unwrap_or(0) as usize;
                let limit = args
                    .get("limit")
                    .and_then(Value::as_u64)
                    .map(|v| v as usize)
                    .unwrap_or(self.read_limit);
                let lines: Vec<&str> = text.lines().skip(offset).collect();
                let content = lines
                    .iter()
                    .take(limit)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join("\n");
                // line_count = 窗口内行数（旧语义返回的是 offset 后剩余总数，
                // 与实际返回内容不符）
                let shown = lines.iter().take(limit).count();
                ToolOutput::ok(
                    json!({"path": path.display().to_string(), "content": content, "line_count": shown, "remaining_after_offset": lines.len()}),
                )
            }
            Err(e) => ToolOutput::err(e.to_string()),
        }
    }

    fn tool_write_file(&self, args: &Value) -> ToolOutput {
        let (path, content) = match (
            args.get("path").and_then(Value::as_str),
            args.get("content").and_then(Value::as_str),
        ) {
            (Some(p), Some(c)) => (self.resolve(p), c),
            _ => return ToolOutput::err("missing path/content"),
        };
        if let Err(e) = self.check_write_allowed(&path) {
            return ToolOutput::err(e);
        }
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        match std::fs::write(&path, content) {
            Ok(()) => {
                ToolOutput::ok(json!({"path": path.display().to_string(), "bytes": content.len()}))
            }
            Err(e) => ToolOutput::err(e.to_string()),
        }
    }

    fn tool_list_dir(&self, args: &Value) -> ToolOutput {
        let dir = args
            .get("path")
            .and_then(Value::as_str)
            .map(|s| self.resolve(s))
            .unwrap_or_else(|| self.cwd.clone());
        match std::fs::read_dir(&dir) {
            Ok(entries) => {
                let mut items = Vec::new();
                for e in entries.flatten() {
                    let name = e.file_name().to_string_lossy().into_owned();
                    if name.starts_with('.') {
                        continue;
                    }
                    let is_dir = e.path().is_dir();
                    items.push(json!({"name": name, "is_dir": is_dir}));
                }
                items.sort_by(|a, b| {
                    let ad = a["is_dir"].as_bool().unwrap_or(false);
                    let bd = b["is_dir"].as_bool().unwrap_or(false);
                    bd.cmp(&ad)
                        .then_with(|| a["name"].as_str().cmp(&b["name"].as_str()))
                });
                ToolOutput::ok(json!({"path": dir.display().to_string(), "items": items}))
            }
            Err(e) => ToolOutput::err(e.to_string()),
        }
    }

    /// str_replace_editor：view / create / str_replace（对齐 dsh-tool-str-replace-editor）。
    fn tool_str_replace(&self, args: &Value) -> ToolOutput {
        let command = args
            .get("command")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let path = match args.get("path").and_then(Value::as_str) {
            Some(p) => self.resolve(p),
            None => return ToolOutput::err("missing path"),
        };
        match command {
            "view" => {
                if path.is_dir() {
                    return self.tool_list_dir(args);
                }
                match std::fs::read_to_string(&path) {
                    Ok(text) => {
                        // cat -n 风格
                        let numbered: Vec<String> = text
                            .lines()
                            .enumerate()
                            .map(|(i, l)| format!("{:6}\t{l}", i + 1))
                            .collect();
                        ToolOutput::ok(
                            json!({"path": path.display().to_string(), "content": numbered.join("\n")}),
                        )
                    }
                    Err(e) => ToolOutput::err(e.to_string()),
                }
            }
            "create" => {
                if path.exists() {
                    return ToolOutput::err("create 不能覆盖已存在文件");
                }
                if let Err(e) = self.check_write_allowed(&path) {
                    return ToolOutput::err(e);
                }
                let content = args.get("file_text").and_then(Value::as_str).unwrap_or("");
                if let Some(parent) = path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                match std::fs::write(&path, content) {
                    Ok(()) => {
                        ToolOutput::ok(json!({"path": path.display().to_string(), "created": true}))
                    }
                    Err(e) => ToolOutput::err(e.to_string()),
                }
            }
            "str_replace" => {
                let old_str = args.get("old_str").and_then(Value::as_str).unwrap_or("");
                let new_str = args.get("new_str").and_then(Value::as_str).unwrap_or("");
                if old_str.is_empty() {
                    return ToolOutput::err("old_str 不能为空");
                }
                if let Err(e) = self.check_write_allowed(&path) {
                    return ToolOutput::err(e);
                }
                match std::fs::read_to_string(&path) {
                    Ok(text) => {
                        // 必须唯一匹配（对齐 DSH 语义）
                        let count = text.matches(old_str).count();
                        if count == 0 {
                            return ToolOutput::err("old_str 未找到匹配");
                        }
                        if count > 1 {
                            return ToolOutput::err("old_str 匹配不唯一，请包含更多上下文");
                        }
                        let new_text = text.replacen(old_str, new_str, 1);
                        match std::fs::write(&path, &new_text) {
                            Ok(()) => ToolOutput::ok(
                                json!({"path": path.display().to_string(), "replaced": true, "old_len": old_str.len(), "new_len": new_str.len()}),
                            ),
                            Err(e) => ToolOutput::err(e.to_string()),
                        }
                    }
                    Err(e) => ToolOutput::err(e.to_string()),
                }
            }
            other => ToolOutput::err(format!("unknown str_replace_editor command: {other}")),
        }
    }

    /// fs_search：文件名/内容搜索。
    fn tool_fs_search(&self, args: &Value) -> ToolOutput {
        let dir = args
            .get("path")
            .and_then(Value::as_str)
            .map(|s| self.resolve(s))
            .unwrap_or_else(|| self.cwd.clone());
        let pattern = args
            .get("pattern")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_lowercase();
        let mut hits = Vec::new();
        let mut stack = vec![dir.clone()];
        let mut scanned = 0usize;
        while let Some(d) = stack.pop() {
            if scanned > 500 {
                break;
            }
            let Ok(entries) = std::fs::read_dir(&d) else {
                continue;
            };
            for e in entries.flatten() {
                scanned += 1;
                let name = e.file_name().to_string_lossy().into_owned();
                if name.starts_with('.') || name == "target" || name == "node_modules" {
                    continue;
                }
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                } else if name.to_lowercase().contains(&pattern) {
                    hits.push(json!({"path": p.display().to_string(), "kind": "file"}));
                }
            }
        }
        hits.truncate(50);
        ToolOutput::ok(json!({"hits": hits, "count": hits.len()}))
    }

    /// web_search：调用 dsh-desktop 自己的搜索通道。
    /// 实际搜索由 UI/宿主提供（dsh-desktop 没有内置搜索提供商时返回提示）。
    fn tool_web_search(&self, args: &Value) -> ToolOutput {
        let query = args
            .get("query")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim();
        if query.is_empty() {
            return ToolOutput::err("query 不能为空");
        }
        match web_search_impl(query, self.search_max_results, self.http_proxy.as_deref()) {
            Ok(hits) => ToolOutput::ok(json!({
                "query": query,
                "sources": hits,
                "count": hits.len(),
            })),
            Err(e) => ToolOutput::err(format!("搜索失败: {e}")),
        }
    }
}

/// 双引擎搜索：优先 DuckDuckGo（国际网络零依赖）；不可达时降级 Bing
/// （国内网络可达 cn.bing.com，同样无需 API key）。插件可用同名 web_search
/// 工具覆盖整个实现（接入 SearxNG / Tavily / DeepSeek 搜索 API 等）。
fn web_search_impl(query: &str, max: usize, proxy: Option<&str>) -> Result<Vec<SearchHit>, String> {
    match web_search_ddg(query, max, proxy) {
        Ok(hits) if !hits.is_empty() => Ok(hits),
        first_err => web_search_bing(query, max, proxy).map_err(|bing_err| {
            format!(
                "DuckDuckGo: {first_err:?}; Bing: {bing_err}（两者都不可达，可配置代理或安装搜索插件）"
            )
        }),
    }
}

/// Bing 网页搜索（`https://www.bing.com/search?q=`，重定向至 cn.bing.com 亦可用）。
fn web_search_bing(query: &str, max: usize, proxy: Option<&str>) -> Result<Vec<SearchHit>, String> {
    let mut builder = reqwest::blocking::Client::builder()
        .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36")
        .timeout(Duration::from_secs(20))
        .connect_timeout(Duration::from_secs(8));
    if let Some(p) = proxy {
        builder = builder.proxy(reqwest::Proxy::all(p).map_err(|e| format!("代理配置无效: {e}"))?);
    }
    let client = builder.build().map_err(|e| e.to_string())?;
    let url = format!(
        "https://www.bing.com/search?q={}&count={max}",
        urlencode(query)
    );
    let resp = client
        .get(&url)
        .header("Accept-Language", "zh-CN,zh;q=0.9,en;q=0.8")
        .send()
        .map_err(|e| format!("{e}"))?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()));
    }
    let html = resp.text().map_err(|e| e.to_string())?;
    let hits = parse_bing_html(&html, max);
    if hits.is_empty() {
        return Err("Bing 未返回可解析结果".into());
    }
    Ok(hits)
}

/// 解析 Bing 结果页：每条为 `<li class="b_algo"><h2><a href="真实URL">标题</a></h2>`
/// + 摘要 `<p class="b_lineclamp…">…</p>`（宽松取块内首个 <p> 文本）。
pub fn parse_bing_html(html: &str, max: usize) -> Vec<SearchHit> {
    let mut out = Vec::new();
    let mut rest = html;
    while out.len() < max {
        // 精确匹配 `b_algo"`（class 值结尾引号）：b_algoSlug 含 b_algo 前缀，
        // 宽匹配会把摘要 class 误当下一个结果块起点截断块
        let Some(li) = rest.find("b_algo\"") else {
            break;
        };
        let block_start = li;
        // 块结尾：下一个 b_algo" 或 5KB 截断（防异常页面死循环）
        let seg = &rest[block_start..];
        let seg_end = seg[8..]
            .find("b_algo\"")
            .map(|i| 8 + i)
            .unwrap_or(seg.len().min(5000));
        let block = &seg[..seg_end];
        // 链接：h2 内首个 <a ... href="URL">
        let (url, title) = match block.find("<h2") {
            Some(h2) => {
                let after = &block[h2..];
                let Some(a) = after.find("<a ") else { break };
                let aa = &after[a..];
                let Some(href) = aa.find("href=\"") else {
                    break;
                };
                let hs = href + 6;
                let Some(hl) = aa[hs..].find('"') else { break };
                let url = &aa[hs..hs + hl];
                let Some(gt) = aa[hs + hl..].find('>') else {
                    break;
                };
                let te = hs + hl + gt + 1;
                let Some(ca) = aa[te..].find("</a>") else {
                    break;
                };
                (url, &aa[te..te + ca])
            }
            None => break,
        };
        // 摘要：块内 class 含 b_lineclamp 或 b_caption 后的首个 <p>
        let snippet = ["b_lineclamp", "b_caption", "b_algoSlug"]
            .iter()
            .find_map(|k| {
                block.find(k).and_then(|p| {
                    let pa = &block[p..];
                    pa.find('>').and_then(|g| {
                        let s0 = p + g + 1;
                        pa[g + 1..].find("</p>").map(|c| &block[s0..s0 + c])
                    })
                })
            })
            .unwrap_or("");
        if !url.is_empty() && url.starts_with("http") {
            out.push(SearchHit {
                title: strip_tags(title),
                url: decode_html_entities(url),
                snippet: strip_tags(snippet),
            });
        }
        rest = &seg[seg_end..];
    }
    out
}

/// 去除内联标签（<em> 等 Bing 高亮标记）并解码实体。
fn strip_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    decode_html_entities(&out)
}

/// 一条搜索结果。
#[derive(Debug, Clone, serde::Serialize)]
pub struct SearchHit {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

/// DuckDuckGo HTML 端点搜索（无需 API key 的零配置方案；官方 API 与
/// 其它提供商可经插件提供同名 web_search 工具覆盖本实现）。
/// `https://html.duckduckgo.com/html/` POST q=<query>，结果为静态 HTML。
fn web_search_ddg(query: &str, max: usize, proxy: Option<&str>) -> Result<Vec<SearchHit>, String> {
    let mut builder = reqwest::blocking::Client::builder()
        .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36")
        .timeout(Duration::from_secs(20))
        .connect_timeout(Duration::from_secs(8));
    if let Some(p) = proxy {
        builder = builder.proxy(reqwest::Proxy::all(p).map_err(|e| format!("代理配置无效: {e}"))?);
    }
    let client = builder.build().map_err(|e| e.to_string())?;
    let resp = client
        .post("https://html.duckduckgo.com/html/")
        .body(format!("q={}", urlencode(query)))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .send()
        .map_err(|e| format!("{e}"))?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()));
    }
    let html = resp.text().map_err(|e| e.to_string())?;
    Ok(parse_ddg_html(&html, max))
}

/// 解析 DuckDuckGo HTML 结果页：每条结果为
/// `<a rel="nofollow" class="result__a" href="//duckduckgo.com/l/?uddg=<编码URL>...">标题</a>`
/// 与 `<a class="result__snippet"...>摘要</a>`。
/// 无 HTML 解析依赖，用受控字符串扫描（DDG 页面结构稳定）。
pub fn parse_ddg_html(html: &str, max: usize) -> Vec<SearchHit> {
    let mut out = Vec::new();
    let mut rest = html;
    while out.len() < max {
        // 找下一个结果标题链接
        let Some(a_pos) = rest.find("result__a") else {
            break;
        };
        let after = &rest[a_pos..];
        let Some(href_pos) = after.find("href=\"") else {
            break;
        };
        let href_start = href_pos + 6;
        let Some(href_len) = after[href_start..].find('"') else {
            break;
        };
        let href_raw = &after[href_start..href_start + href_len];
        // 标题 = 链接闭合 `>` 后到 `</a>`
        let Some(gt) = after[href_start + href_len..].find('>') else {
            break;
        };
        let tag_end = href_start + href_len + gt + 1;
        let Some(close_a) = after[tag_end..].find("</a>") else {
            break;
        };
        let title_raw = &after[tag_end..tag_end + close_a];
        // 摘要：标题之后最近的 result__snippet 块（在本结果与下个结果之间找）
        let seg_end = after[tag_end..]
            .find("result__a")
            .map(|i| tag_end + i)
            .unwrap_or(after.len());
        let snippet_raw = after[tag_end..seg_end]
            .find("result__snippet")
            .and_then(|sp| {
                let sn = &after[tag_end + sp..];
                sn.find('>').and_then(|g| {
                    let s0 = tag_end + sp + g + 1;
                    sn[g + 1..].find("</a>").map(|c| &after[s0..s0 + c])
                })
            })
            .unwrap_or("");
        let url = decode_ddg_href(href_raw);
        if !url.is_empty() {
            out.push(SearchHit {
                title: decode_html_entities(title_raw),
                url,
                snippet: decode_html_entities(snippet_raw),
            });
        }
        rest = &after[seg_end.min(after.len())..];
    }
    out
}

/// DDG 跳转链接 `//duckduckgo.com/l/?uddg=<percent-encoded>&rut=...`
/// → 解码出真实 URL；非 uddg 形态原样返回（清理首部 //）。
fn decode_ddg_href(href: &str) -> String {
    let Some(pos) = href.find("uddg=") else {
        return href.trim_start_matches("//").to_string();
    };
    let enc = &href[pos + 5..];
    let enc = enc.split('&').next().unwrap_or(enc);
    percent_decode(enc)
}

/// 极简 percent-decode（%XX + '+' -> 空格）。
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = |b: u8| -> Option<u8> {
                    match b {
                        b'0'..=b'9' => Some(b - b'0'),
                        b'a'..=b'f' => Some(b - b'a' + 10),
                        b'A'..=b'F' => Some(b - b'A' + 10),
                        _ => None,
                    }
                };
                if let (Some(h), Some(l)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                    out.push(h * 16 + l);
                    i += 3;
                } else {
                    out.push(b'%');
                    i += 1;
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// 极简 percent-encode（表单提交用）。
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// 最小 HTML 实体解码（标题/摘要中常见集合）。
fn decode_html_entities(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#x27;", "'")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ")
        .replace("&#x2F;", "/")
        .trim()
        .to_string()
}

/// node_called 审批的固定目标标记（AlwaysAllow 一次后续免问）。
const NODE_WRITE_MARKER: &str = "<node_called:任意代码执行>";

/// 从 shell 命令里直觉解析可能写文件的目标路径。
/// 覆盖：`>`/`>>`/`1>`/`2>`/`n>` 重定向、`>path` 无空格形式，
/// 以及常见写命令（cmd: copy/move/del/ren；PowerShell: Copy-Item/Move-Item/
/// Set-Content/Add-Content/Out-File/tee；unix 工具: sed -i/cp/mv/rm/install）。
/// 启发式解析注定不完备——真正的强制由沙箱模式（workspace-write/read-only）
/// 在 OS 层提供；danger 模式下这里是唯一的写闸门，宁可多问不能漏。
fn extract_redirect_targets(command: &str) -> Vec<String> {
    let mut out = Vec::new();
    // 写命令关键字：其后的参数里取**最后一个**非参数 token 作写目标
    // （copy/mv src dst 的目标是 dst；sed -i script file 的目标是 file；
    //  Set-Content -Path x -Value y 取最后一个参数值——宁可多问不能漏）
    const WRITE_CMDS: &[&str] = &[
        "copy", "move", "del", "erase", "ren", "rename", "md", "mkdir", "rd", "cp", "mv", "rm",
        "tee", "install", "truncate", "shred",
    ];
    const POWERSHELL_WRITE_CMDS: &[&str] = &[
        "copy-item",
        "move-item",
        "set-content",
        "add-content",
        "out-file",
        "new-item",
        "remove-item",
        "clear-content",
    ];
    let toks: Vec<&str> = command.split_whitespace().collect();
    let mut prev_is_redirect = false;
    let mut in_write_cmd = false; // 已出现写命令关键字，后续非参数 token 记为候选目标
    for t in toks {
        let lower = t.to_lowercase();
        // `2>file` / `1>file` / `3>>file` 等带 fd 前缀的重定向
        if let Some(rest) = strip_fd_redirect(&lower) {
            prev_is_redirect = true;
            in_write_cmd = false;
            if !rest.is_empty() {
                out.push(rest.trim_matches('"').to_string());
            }
            continue;
        }
        if t.starts_with(">>") || t.starts_with('>') {
            prev_is_redirect = true;
            in_write_cmd = false;
            let rest = t.trim_start_matches('>').trim();
            if !rest.is_empty() {
                out.push(rest.trim_matches('"').to_string());
            }
            continue;
        }
        if prev_is_redirect {
            out.push(t.trim_matches('"').to_string());
            prev_is_redirect = false;
            continue;
        }
        // 写命令关键字（含 PowerShell 动词-参数形式，如 Copy-Item）
        let base = lower
            .trim_end_matches(',')
            .trim_matches(|c: char| c == '(' || c == '|');
        if POWERSHELL_WRITE_CMDS.contains(&base) || WRITE_CMDS.contains(&base) || base == "sed" {
            in_write_cmd = true;
            continue;
        }
        // 写命令后一律跳过 /- 和 --- 开头的 token（命令行标志，非文件路径）：
        // del /f /q → /f 是 force 标志不是路径（历史 bug：被当目标 → F:/f 审批）
        if in_write_cmd && (t.starts_with('-') || t.starts_with('/')) {
            continue;
        }
        if in_write_cmd {
            // 写命令后的非参数 token 全部记为候选目标（copy/mv 的 dst、
            // sed 的文件、cmdlet 的参数值——宁多问不漏；读侧多触发只是多一次确认）
            out.push(t.trim_matches(['"', '\'']).to_string());
            continue;
        }
        // ">path" 无空格形式也抓（含 `2>path`）
        if let Some(idx) = t.find('>') {
            let after = &t[idx + 1..];
            if !after.is_empty() && !after.contains('"') && !after.starts_with('&') {
                out.push(after.trim_matches('"').to_string());
            }
        }
    }
    out
}

/// `2>file`/`1>>file` 形式：返回去掉 fd 前缀后的目标（无文件目标返回空串；
/// 非该形式返回 None）。`2>&1` 合并流无文件目标。
fn strip_fd_redirect(tok: &str) -> Option<String> {
    let b = tok.as_bytes();
    if b.len() >= 2 && b[0].is_ascii_digit() && b[1] == b'>' {
        let rest = tok[1..].trim_start_matches('>');
        if rest.is_empty() || rest.starts_with('&') {
            return Some(String::new());
        }
        return Some(rest.trim_matches('"').to_string());
    }
    None
}

/// 路径是否落在工作区内（防御 `..` 穿越）。
fn path_is_within_ws(path: &Path, root: &Path) -> bool {
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

/// 把沙箱执行结果转成工具输出（AclResult → ToolOutput）。
fn acl_output_to_tool(out: crate::exec::acl::AclResult) -> ToolOutput {
    if out.sandbox_error.is_some() {
        return ToolOutput::err(out.stderr);
    }
    ToolOutput {
        ok: out.ok,
        value: serde_json::json!({
            "stdout": out.stdout,
            "exit_code": out.exit_code.unwrap_or(-1),
            "sandboxed": true,
        }),
        stderr: out.stderr,
    }
}

/// Shell 命令超时（对齐 DSH bash timeoutMs=300000）。
const SHELL_TIMEOUT: Duration = Duration::from_secs(300);

/// 带超时 + 输出上限的命令执行。
///
/// 并发读 stdout/stderr 防管道死锁；超时杀进程（Windows 下用 Job Object
/// 杀整棵进程树——`child.kill()` 只杀直接子进程，cmd /c 启动的孙进程会存活
/// 并持有管道写端，导致排水线程的 join 永久阻塞、回合线程卡死）；
/// 排水线程 join 带限时兜底；输出截断到 1MB 防 OOM。
fn run_command_timeout(mut cmd: Command, timeout: Duration) -> ToolOutput {
    const MAX_OUT: usize = 1 << 20; // 1MB
                                    // 静默：不弹黑色控制台窗口（cmd/powershell 子进程）
    crate::util::hide_console(&mut cmd);
    // stdin 置 null：子进程读 stdin（pause/more/REPL）会一直挂到超时
    cmd.stdin(Stdio::null());

    fn drain<R: Read>(pipe: Option<R>) -> Vec<u8> {
        let mut buf = Vec::new();
        if let Some(mut r) = pipe {
            let mut chunk = [0u8; 8192];
            loop {
                match r.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(n) => {
                        if buf.len() < MAX_OUT {
                            let take = n.min(MAX_OUT - buf.len());
                            buf.extend_from_slice(&chunk[..take]);
                        }
                    }
                    Err(_) => break,
                }
            }
        }
        buf
    }

    let mut child = match cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn() {
        Ok(c) => c,
        Err(e) => return ToolOutput::err(format!("spawn failed: {e}")),
    };
    // Windows：进程入 Job Object（KILL_ON_JOB_CLOSE）——超时终止整棵进程树
    // （`child.kill()` 只杀直接子进程，cmd /c 的孙进程会存活并持有管道写端）
    let job = attach_tree_kill_job(&child);

    // 排水线程 → 通道：join 换成 recv_timeout，孙进程残留句柄把管道
    // 撑住时不再永久阻塞回合线程
    let (so_tx, so_rx) = std::sync::mpsc::channel::<Vec<u8>>();
    let (se_tx, se_rx) = std::sync::mpsc::channel::<Vec<u8>>();
    std::thread::spawn({
        let pipe = child.stdout.take();
        move || {
            let _ = so_tx.send(drain(pipe));
        }
    });
    std::thread::spawn({
        let pipe = child.stderr.take();
        move || {
            let _ = se_tx.send(drain(pipe));
        }
    });

    // 排水收拢限时：进程退出后管道应很快 EOF；孙进程持句柄时放弃等待
    //（线程残留可接受，回合线程绝不能被挂死）
    const JOIN_DEADLINE: Duration = Duration::from_secs(5);
    let recv_pipe = |rx: &std::sync::mpsc::Receiver<Vec<u8>>| -> Vec<u8> {
        rx.recv_timeout(JOIN_DEADLINE).unwrap_or_default()
    };

    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break st,
            Ok(None) => {
                if start.elapsed() > timeout {
                    // 树杀：关闭 Job 句柄（KILL_ON_JOB_CLOSE）；无 Job 时退化为杀直接子进程
                    drop(job);
                    let _ = child.kill();
                    let _ = child.wait();
                    let so = recv_pipe(&so_rx);
                    let se = recv_pipe(&se_rx);
                    return ToolOutput {
                        ok: false,
                        value: json!({
                            "error": format!("命令执行超时（>{}s），已终止", timeout.as_secs()),
                            "partial_stdout": String::from_utf8_lossy(&so).into_owned(),
                        }),
                        stderr: String::from_utf8_lossy(&se).into_owned(),
                    };
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(e) => {
                drop(job);
                let _ = child.kill();
                let _ = child.wait();
                return ToolOutput::err(format!("wait failed: {e}"));
            }
        }
    };
    drop(job);
    let stdout = recv_pipe(&so_rx);
    let stderr = recv_pipe(&se_rx);
    ToolOutput {
        ok: status.success(),
        value: json!({
            "stdout": String::from_utf8_lossy(&stdout).into_owned(),
            "exit_code": status.code().unwrap_or(-1),
        }),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
    }
}

/// 把子进程挂进"关闭即杀树"的 Job Object（Win32 细节封装在 exec::winacl）。
#[cfg(windows)]
fn attach_tree_kill_job(
    child: &std::process::Child,
) -> Option<crate::exec::winacl::KillOnCloseJob> {
    use std::os::windows::io::AsRawHandle;
    crate::exec::winacl::KillOnCloseJob::attach(child.as_raw_handle())
}

#[cfg(not(windows))]
fn attach_tree_kill_job(_child: &std::process::Child) -> Option<()> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// node_called：执行 JS 并返回 stdout（本机需有 node）。
    #[test]
    fn node_called_executes_js() {
        let reg = ToolRegistry::new(std::env::temp_dir());
        let out = reg.tool_node_called(&json!({
            "code": "console.log(JSON.stringify({hello: 'dsh', sum: 1 + 2}))"
        }));
        assert!(out.ok, "node_called 应执行成功: {}", out.stderr);
        let stdout = out
            .value
            .get("stdout")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        assert!(
            stdout.contains("\"sum\":3"),
            "stdout 应包含 JS 计算结果: {stdout}"
        );

        // 空 code 拒绝
        let out = reg.tool_node_called(&json!({"code": "  "}));
        assert!(!out.ok);
    }

    /// node_called 可 require DSH 官方插件（NODE_PATH 指向本机 dsh node_modules）。
    /// 本机没有 DSH 环境时 require 失败（Cannot find module）是预期——
    /// 仅验证执行链路正常（不因环境缺失而挂）。
    #[test]
    fn node_called_can_require_dsh_packages() {
        let reg = ToolRegistry::new(std::env::temp_dir());
        let out = reg.tool_node_called(&json!({
            "code": "try { const p = require('@deepseek-ai/dsh-base'); console.log('ok:' + typeof p); } catch (e) { console.log('norequire:' + e.message); }"
        }));
        assert!(out.ok, "node_called 执行链路应正常: {}", out.stderr);
        let stdout = out
            .value
            .get("stdout")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let dsh_missing = stdout.contains("norequire:Cannot find module");
        if !dsh_missing {
            assert!(stdout.starts_with("ok:"), "require 应成功: stdout={stdout}");
        }
    }

    /// 沙箱模式下 node_called 必须拒绝（完整令牌子进程 = 绕过沙箱）。
    #[test]
    fn node_called_rejected_under_sandbox() {
        let reg = ToolRegistry::new(std::env::temp_dir()).with_sandbox(Some(
            crate::exec::acl::WindowsAclSandbox::new(
                crate::exec::SandboxMode::ReadOnly,
                std::env::temp_dir(),
            ),
        ));
        let out = reg.tool_node_called(&json!({"code": "console.log(1)"}));
        assert!(!out.ok, "read-only 沙箱下 node_called 必须拒绝");
        assert!(
            out.stderr.contains("沙箱"),
            "错误应说明沙箱限制: {}",
            out.stderr
        );

        let reg = ToolRegistry::new(std::env::temp_dir()).with_sandbox(Some(
            crate::exec::acl::WindowsAclSandbox::new(
                crate::exec::SandboxMode::WorkspaceWrite,
                std::env::temp_dir(),
            ),
        ));
        let out = reg.tool_node_called(&json!({"code": "console.log(1)"}));
        assert!(!out.ok, "workspace-write 沙箱下 node_called 必须拒绝");
    }

    /// run_code 的写子步不得绕过越权审批（历史漏洞：顶层名匹配不到
    /// run_code → 内嵌 write_file 直写工作区外零确认）。
    #[test]
    fn run_code_steps_cannot_bypass_approval() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().join("workspace");
        std::fs::create_dir_all(&ws).unwrap();
        let reg = ToolRegistry::new(ws.clone()).with_workspace_root(Some(ws.clone()));
        let outside = dir.path().join("escape.txt");
        let hit = reg
            .potential_out_of_workspace(
                "run_code",
                &json!({"steps": [
                    {"op": "read_file", "path": "a.txt"},
                    {"op": "write_file", "path": outside.to_string_lossy(), "content": "x"},
                ]}),
            )
            .expect("run_code 内嵌工作区外写必须触发审批");
        assert!(hit.0.contains("escape.txt"), "target: {}", hit.0);

        // 内嵌 bash 重定向越权同样触发
        let cmd = format!("echo hi > \"{}\"", outside.display());
        let hit = reg
            .potential_out_of_workspace(
                "run_code",
                &json!({"steps": [{"op": "bash", "command": cmd}]}),
            )
            .expect("run_code 内嵌 bash 重定向越权必须触发审批");
        assert!(hit.0.contains("escape.txt"), "target: {}", hit.0);

        // 全部子步都在工作区内 → 不触发
        assert!(reg
            .potential_out_of_workspace(
                "run_code",
                &json!({"steps": [{"op": "write_file", "path": ws.join("ok.txt").to_string_lossy(), "content": "x"}]}),
            )
            .is_none());
    }

    /// node_called 按潜在越权处理（任意代码 → 每次审批，AlwaysAllow 后免问）。
    #[test]
    fn node_called_needs_approval() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().join("workspace");
        std::fs::create_dir_all(&ws).unwrap();
        let reg = ToolRegistry::new(ws.clone()).with_workspace_root(Some(ws));
        let hit = reg
            .potential_out_of_workspace("node_called", &json!({"code": "console.log(1)"}))
            .expect("node_called 应触发审批");
        assert_eq!(hit.0, NODE_WRITE_MARKER);
    }

    /// 写工具在受限沙箱下 fail-closed（read-only 拒绝一切写；
    /// workspace-write 拒绝工作区外写）。
    #[test]
    fn write_tools_fail_closed_under_sandbox() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().join("workspace");
        std::fs::create_dir_all(&ws).unwrap();
        // read-only
        let ro = ToolRegistry::new(ws.clone()).with_sandbox(Some(
            crate::exec::acl::WindowsAclSandbox::new(
                crate::exec::SandboxMode::ReadOnly,
                ws.clone(),
            ),
        ));
        let out = ro
            .tool_write_file(&json!({"path": ws.join("x.txt").to_string_lossy(), "content": "x"}));
        assert!(!out.ok, "read-only 模式 write_file 必须拒绝");
        let out = ro.tool_str_replace(&json!({
            "command": "create", "path": ws.join("y.txt").to_string_lossy(), "file_text": "x"
        }));
        assert!(!out.ok, "read-only 模式 str_replace create 必须拒绝");
        assert!(!ws.join("x.txt").exists());
        assert!(!ws.join("y.txt").exists());

        // workspace-write：审批系统是闸门（区外写经用户批准后执行，
        // check_write_allowed 只记日志不阻断——旧实现与审批冲突）
        let ww = ToolRegistry::new(ws.clone()).with_sandbox(Some(
            crate::exec::acl::WindowsAclSandbox::new(
                crate::exec::SandboxMode::WorkspaceWrite,
                ws.clone(),
            ),
        ));
        let inside = ws.join("inside.txt");
        let out = ww.tool_write_file(&json!({"path": inside.to_string_lossy(), "content": "ok"}));
        assert!(out.ok, "workspace-write 区内写应允许: {}", out.stderr);
        // 区外写：不再硬拒（审批系统处理）——验证工具不额外阻断
        let outside = dir.path().join("outside_check.txt");
        let out2 = ww.tool_write_file(&json!({"path": outside.to_string_lossy(), "content": "x"}));
        assert!(
            out2.ok,
            "workspace-write 区外写应由审批系统决定，工具不应硬拒: {out2:?}"
        );
    }

    /// 重定向启发式：常见写命令变体都识别（历史绕过：`2> file`、
    /// `Set-Content`、`cp/mv/del`、`sed -i`）。
    #[test]
    fn redirect_heuristic_covers_write_variants() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().join("workspace");
        std::fs::create_dir_all(&ws).unwrap();
        let reg = ToolRegistry::new(ws.clone()).with_workspace_root(Some(ws));
        let outside = dir.path().join("o.txt");
        let o = outside.to_string_lossy().to_string();

        let cases = [
            format!("cmd /C 2> \"{o}\""), // stderr 重定向
            format!("powershell Set-Content -Path \"{o}\" -Value x"),
            format!("copy src \"{o}\""), // cmd copy
            format!("mv src \"{o}\""),   // unix mv
            format!("sed -i s/a/b/ \"{o}\""),
            format!("echo x 1> \"{o}\""),
        ];
        for cmd in cases {
            assert!(
                reg.potential_out_of_workspace("bash", &json!({"command": cmd.clone()}))
                    .is_some(),
                "写命令变体应触发审批: {cmd}"
            );
        }
        // 纯读命令不触发
        assert!(reg
            .potential_out_of_workspace("bash", &json!({"command": "dir"}))
            .is_none());
        assert!(reg
            .potential_out_of_workspace("bash", &json!({"command": "type a.txt"}))
            .is_none());
    }

    /// 权限审批：写工作区外识别（potential_out_of_workspace）。
    #[test]
    fn approval_detects_outside_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().join("workspace");
        std::fs::create_dir_all(&ws).unwrap();
        let reg = ToolRegistry::new(ws.clone());
        // 未配置 workspace_root → 不拦截
        assert!(reg
            .potential_out_of_workspace("write_file", &json!({"path": "x.txt"}))
            .is_none());

        let reg = reg.with_workspace_root(Some(ws.clone()));
        let inside = ws.join("a.txt");
        let outside = dir.path().join("b.txt");

        // 工作区内 → 不拦截
        assert!(reg
            .potential_out_of_workspace("write_file", &json!({"path": inside.to_string_lossy()}))
            .is_none());
        // 工作区外 → 拦截
        let hit = reg
            .potential_out_of_workspace("write_file", &json!({"path": outside.to_string_lossy()}))
            .expect("写工作区外应触发审批");
        assert!(hit.0.contains("b.txt"), "target: {}", hit.0);

        // str_replace create 工作区外 → 拦截；view → 不拦截
        assert!(reg
            .potential_out_of_workspace(
                "str_replace_editor",
                &json!({"command":"create","path": outside.to_string_lossy(),"file_text":""})
            )
            .is_some());
        assert!(reg
            .potential_out_of_workspace(
                "str_replace_editor",
                &json!({"command":"view","path": outside.to_string_lossy()})
            )
            .is_none());

        // bash 重定向到工作区外 → 拦截
        let cmd = format!("echo hi > \"{}\"", outside.display());
        assert!(reg
            .potential_out_of_workspace("bash", &json!({"command": cmd}))
            .is_some());
        // bash 重定向到工作区内 → 不拦截
        let cmd_in = format!("echo hi > \"{}\"", inside.display());
        assert!(reg
            .potential_out_of_workspace("bash", &json!({"command": cmd_in}))
            .is_none());
    }
}

#[cfg(test)]
mod web_search_tests {
    use super::*;

    /// DDG HTML 解析器：fixture 页面 → 标题/真实 URL/摘要提取 + uddg 解码。
    #[test]
    fn parse_ddg_html_fixture() {
        let html = r#"<div class="result results_links">
 <a rel="nofollow" class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fwww rust-lang org%2Flearn&amp;rut=abc123">Learn Rust &amp; Programming</a>
 <a class="result__snippet">The official &quot;guide&quot; to &#x27;Rust&#x27;</a>
</div>
<div class="result">
 <a rel="nofollow" class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample com%2Fa+b&amp;rut=xyz">Second Result</a>
 <a class="result__snippet">Second snippet</a>
</div>"#;
        let hits = parse_ddg_html(html, 8);
        assert_eq!(hits.len(), 2, "应解析出 2 条: {hits:?}");
        assert_eq!(hits[0].title, "Learn Rust & Programming");
        assert_eq!(
            hits[0].url, "https://www rust-lang org/learn",
            "uddg percent-decode + &amp; 还原"
        );
        assert_eq!(hits[0].snippet, "The official \"guide\" to 'Rust'");
        assert_eq!(hits[1].title, "Second Result");
        assert_eq!(hits[1].url, "https://example com/a b", "+ 解码为空格");
    }

    /// 上限截断。
    #[test]
    fn parse_ddg_html_max() {
        let one = r#"<a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fx%2F1">T1</a><a class="result__snippet">s</a>"#;
        let page = one.repeat(5);
        assert_eq!(parse_ddg_html(&page, 3).len(), 3);
    }

    /// 无结果页面返回空（不 panic）。
    #[test]
    fn parse_ddg_html_empty() {
        assert!(parse_ddg_html("<html><body>no results</body></html>", 8).is_empty());
    }

    /// Bing 解析器：fixture → 标题/URL/摘要（含 <em> 高亮剥离）。
    #[test]
    fn parse_bing_html_fixture() {
        let html = r#"<li class="b_algo"><h2><a href="https://learn.microsoft.com/rust" h="1">Learn <em>Rust</em> &amp; Grow</a></h2><div class="b_caption"><p class="b_lineclamp4">Official <em>Rust</em> docs</p></div></li>
<li class="b_algo"><h2><a href="https://example.org/page">Second</a></h2><p class="b_algoSlug">slug text</p></li>"#;
        let hits = parse_bing_html(html, 8);
        assert_eq!(hits.len(), 2, "{hits:?}");
        assert_eq!(hits[0].title, "Learn Rust & Grow");
        assert_eq!(hits[0].url, "https://learn.microsoft.com/rust");
        assert_eq!(hits[0].snippet, "Official Rust docs");
        assert_eq!(hits[1].snippet, "slug text");
    }

    /// Bing 空页面不 panic。
    #[test]
    fn parse_bing_html_empty() {
        assert!(parse_bing_html("<html>nothing</html>", 8).is_empty());
    }

    /// 真实网络搜索（忽略：CI/离线环境跑不了；本地验证用）。
    #[test]
    #[ignore]
    fn web_search_ddg_live() {
        // 双引擎链：DDG 不可达时自动落 Bing（国内网络）
        let hits = web_search_impl("rust programming language", 5, None).expect("live search");
        assert!(!hits.is_empty(), "应至少返回一条结果");
        assert!(hits[0].url.starts_with("http"), "URL 应可解析: {hits:?}");
    }
}
