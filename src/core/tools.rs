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
        }
    }

    /// 绑定工作区根（权限审批写边界）。
    pub fn with_workspace_root(&self, ws: Option<PathBuf>) -> Self {
        let mut r = self.clone_for_preset();
        r.workspace_root = ws;
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
        r.cwd = cwd;
        r
    }

    pub fn set_cwd(&mut self, cwd: PathBuf) {
        self.cwd = cwd;
    }

    /// 判定一次写工具调用是否可能写**工作区外**（需权限审批）。
    /// 返回 (目标路径, 原因)。只在配置了 workspace_root 时判定；
    /// 未配置 workspace_root 视为不拦截（无边界可判）。
    /// bash/pwsh 用 `>`/`>>` 输出重定向直觉解析；write/str_replace 解析显式 path。
    pub fn potential_out_of_workspace(&self, name: &str, args: &Value) -> Option<(String, String)> {
        let ws = self.workspace_root.as_ref()?.clone();
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
            _ => Vec::new(),
        };
        for t in targets {
            if t.is_empty() {
                continue;
            }
            let abs = self.resolve(&t);
            if !path_is_within_ws(&abs, &ws) {
                return Some((
                    abs.display().to_string(),
                    format!("写操作目标「{}」位于工作区之外", abs.display()),
                ));
            }
        }
        None
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
                "搜索当前信息（对齐 dsh-tool-web，结果上限 8 条）。",
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
    fn tool_node_called(&self, args: &Value) -> ToolOutput {
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
        let p = PathBuf::from(path);
        if p.is_absolute() {
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
                ToolOutput::ok(
                    json!({"path": path.display().to_string(), "content": content, "line_count": lines.len()}),
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
        // TODO(M5): 接入宿主搜索（DeepSeek 搜索 API / 本地索引）
        ToolOutput::ok(json!({
            "query": query,
            "sources": [],
            "note": "web_search 由宿主提供；当前无搜索提供商，请在设置中配置",
        }))
    }
}

/// 从 shell 命令里提取 `>`/`>>` 输出重定向的目标路径（简易解析）。
fn extract_redirect_targets(command: &str) -> Vec<String> {
    let mut out = Vec::new();
    let toks = command.split_whitespace();
    let mut prev_is_redirect = false;
    for t in toks {
        if t.starts_with(">>") || t.starts_with('>') {
            prev_is_redirect = true;
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
        // ">path" 无空格形式也抓
        if let Some(idx) = t.find('>') {
            let after = &t[idx + 1..];
            if !after.is_empty() && !after.contains('"') && !after.starts_with('&') {
                out.push(after.trim_matches('"').to_string());
            }
        }
    }
    out
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
/// 并发读 stdout/stderr 防管道死锁；超时杀进程；输出截断到 1MB 防 OOM。
fn run_command_timeout(mut cmd: Command, timeout: Duration) -> ToolOutput {
    const MAX_OUT: usize = 1 << 20; // 1MB
                                    // 静默：不弹黑色控制台窗口（cmd/powershell 子进程）
    crate::util::hide_console(&mut cmd);

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
    let stdout_th = std::thread::spawn({
        let pipe = child.stdout.take();
        move || drain(pipe)
    });
    let stderr_th = std::thread::spawn({
        let pipe = child.stderr.take();
        move || drain(pipe)
    });

    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break st,
            Ok(None) => {
                if start.elapsed() > timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    let so = stdout_th.join().unwrap_or_default();
                    let se = stderr_th.join().unwrap_or_default();
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
                let _ = child.kill();
                let _ = child.wait();
                return ToolOutput::err(format!("wait failed: {e}"));
            }
        }
    };
    let stdout = stdout_th.join().unwrap_or_default();
    let stderr = stderr_th.join().unwrap_or_default();
    ToolOutput {
        ok: status.success(),
        value: json!({
            "stdout": String::from_utf8_lossy(&stdout).into_owned(),
            "exit_code": status.code().unwrap_or(-1),
        }),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
    }
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
    /// 若本机没有 dsh 环境则跳过断言（仅验证工具不报错）。
    #[test]
    fn node_called_can_require_dsh_packages() {
        let reg = ToolRegistry::new(std::env::temp_dir());
        let out = reg.tool_node_called(&json!({
            "code": "try { const p = require('@deepseek-ai/dsh-base'); console.log('ok:' + typeof p); } catch (e) { console.log('norequire:' + e.message); }"
        }));
        let stdout = out
            .value
            .get("stdout")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        assert!(
            !stdout.contains("norequire") || out.stderr.contains("Cannot find"),
            "require 应成功或明确失败: stdout={stdout} stderr={}",
            out.stderr
        );
    }

    /// 权限审批：写工作区外识别（potential_out_of_workspace）。
    #[test]
    fn approval_detects_outside_workspace() {
        use std::path::PathBuf;
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
