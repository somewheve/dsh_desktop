//! 工具系统：Rust 原生实现 DSH 核心工具全集。
//!
//! 对齐 DSH 的 dsh-tool-* 语义：每个工具返回 JSON 结果，
//! agent 循环把它作为 tool/result 事件写回会话。
//! 已实现：bash / pwsh / read_file / write_file / list_dir / todo_write /
//!         str_replace_editor / fs_search / web_search / ask_user

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
    /// 论文搜索内置扩展配置（卸载/停用 → 工具不列出）
    pub paper_cfg: crate::core::paper::PaperSearchConfig,
    /// 编辑审批门（开启时写工具改动入待确认队列）
    pub review_edits: bool,
    /// 待确认编辑队列（引擎共享）
    pub edits_queue:
        Option<std::sync::Arc<std::sync::Mutex<Vec<crate::core::agent::PendingEdit>>>>,
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
            paper_cfg: crate::core::paper::PaperSearchConfig::default(),
            review_edits: false,
            edits_queue: None,
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
            paper_cfg: self.paper_cfg.clone(),
            review_edits: self.review_edits,
            edits_queue: self.edits_queue.clone(),
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

    /// 编辑审批门开启时入队（读原内容在写入前!由调用方保证顺序）。
    fn enqueue_edit(&self, path: &str, original: Option<String>, note: &str) {
        if !self.review_edits {
            return;
        }
        let Some(q) = &self.edits_queue else { return };
        let sid = q as *const _ as usize; // 占位;真实 sid 由 note 携带场景
        let _ = sid;
        q.lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(crate::core::agent::PendingEdit {
                id: format!("ed-{}", crate::util::simple_id()),
                path: path.to_string(),
                original,
                note: note.to_string(),
                session_id: String::new(),
            });
        log::info!("edit queued for review: {path} ({note})");
    }

    /// 绑定编辑审批门（设置开关 + 引擎队列）。
    pub fn with_review_edits(
        &self,
        on: bool,
        queue: std::sync::Arc<
            std::sync::Mutex<Vec<crate::core::agent::PendingEdit>>,
        >,
    ) -> Self {
        let mut r = self.clone_for_preset();
        r.review_edits = on;
        r.edits_queue = Some(queue);
        r
    }

    /// 绑定论文搜索扩展配置。
    pub fn with_paper_cfg(&self, cfg: crate::core::paper::PaperSearchConfig) -> Self {
        let mut r = self.clone_for_preset();
        r.paper_cfg = cfg;
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

    /// workspace-write 模式下读取仍放行的路径根（用户策略：除基础命令与
    /// DSH 自身目录外，工作区外一律不可访问——读取同样受限）：
    /// - 系统基础路径（cmd/pwsh/系统工具与运行库所在，基础命令的执行依赖）
    /// - 用户目录下的 `.dsh`（DSH 主目录：技能/插件/主题/配置——load_skill
    ///   等自身机制依赖，用户明确要求不拦截）；`DSH_HOME` 环境变量优先
    fn read_allowlist_roots() -> Vec<PathBuf> {
        let mut roots: Vec<PathBuf> = [
            r"C:\Windows",
            r"C:\Program Files",
            r"C:\Program Files (x86)",
            r"C:\ProgramData",
        ]
        .iter()
        .map(PathBuf::from)
        .collect();
        match std::env::var_os("DSH_HOME") {
            Some(h) if !h.is_empty() => roots.push(PathBuf::from(h)),
            _ => {
                if let Some(up) = std::env::var_os("USERPROFILE") {
                    roots.push(PathBuf::from(up).join(".dsh"));
                }
            }
        }
        roots
    }

    /// 路径是否在读取白名单根下（大小写无关，兼容正/反斜杠）。
    /// 额外：路径本身是白名单根的**前缀**也放行——shell 命令里带空格的
    /// 系统路径（"C:\Program Files\Google"）常因引号被拆词，提取器只能
    /// 取到 "C:\Program"（历史缺陷：不匹配任何白名单根 → 误拦）。
    fn path_in_read_allowlist(path: &Path) -> bool {
        let norm = |p: &Path| -> String {
            p.to_string_lossy().to_lowercase().replace("/", "\\")
        };
        let s = norm(path);
        if Self::read_allowlist_roots().iter().any(|r| {
            let r = norm(r);
            s == r || s.starts_with(&format!("{r}\\"))
        }) {
            return true;
        }
        // 引号拆词残段：shell 里 "C:\\Program Files\\X" 会被
        // whitespace 拆词成 "C:\\Program"。残段特征 = 无扩展名且为
        // 2 段路径（盘符+一级目录）。特征命中且该目录名与某白名单根的
        // 某级目录段前缀互含（program ↔ program files）→ 放行残段；
        // 完整路径 token 仍走上方正常判定。
        if !s.contains(".") {
            let parts: Vec<&str> = s.split("\\").collect();
            if parts.len() == 2 && parts[1].len() >= 3 {
                let dir = parts[1];
                return Self::read_allowlist_roots().iter().any(|r| {
                    norm(r)
                        .split("\\")
                        .any(|seg| seg.len() >= 3 && (seg.starts_with(dir) || dir.starts_with(seg)))
                });
            }
        }
        false
    }

    /// 读工具的边界检查：workspace-write 下，工作区与白名单（系统基础 +
    /// ~/.dsh）之外**读取也被拒绝**（历史缺陷：只拦写不拦读，工作区外
    /// 文件可随意读）。read-only / danger 模式读取不受限。
    fn check_read_allowed(&self, path: &Path) -> Result<(), String> {
        let ww = self
            .sandbox
            .as_ref()
            .map(|sb| matches!(sb.mode, crate::exec::SandboxMode::WorkspaceWrite))
            .unwrap_or(false);
        if !ww {
            return Ok(());
        }
        let ws = match &self.workspace_root {
            Some(w) => w.clone(),
            None => return Ok(()),
        };
        if path_is_within_ws(path, &ws) || Self::path_in_read_allowlist(path) {
            return Ok(());
        }
        Err(format!(
            "工作区外路径禁止访问（仅工作区模式下读取同样受限）：{}
工作区：{}；系统基础路径与 ~/.dsh 不受影响。
请改用工作区内路径，或切换权限模式。",
            path.display(),
            ws.display()
        ))
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
                "在目录中搜索：文件名模糊匹配（含通配符 * ?）或文件内容全文搜索（≤256KB 文本文件，返回匹配行）。先匹配文件名，不中则搜内容。",
                json!({"type":"object","properties":{"path":{"type":"string","description":"搜索起始目录"},"pattern":{"type":"string","description":"搜索词：文件名子串（支持 * ? 通配符）或内容关键词"}},"required":["path","pattern"]}),
            ),
            tool_spec(
                "memory_write",
                "写入一条长期记忆（跨会话持久:用户偏好、项目事实、约定）。当用户说\"记住...\"或你在回合中确认了值得跨会话保留的事实/偏好时调用。每条一行,带时间戳追加到 $DSH_HOME/memory.md,之后每个会话自动注入。",
                json!({"type":"object","properties":{"note":{"type":"string","description":"记忆内容(一行,自包含,含必要上下文)"}},"required":["note"]}),
            ),
            tool_spec(
                "web_search",
                "联网搜索最新信息（DuckDuckGo，结果上限 8 条：标题/链接/摘要）。查事实、时效信息、文档时使用。",
                json!({"type":"object","properties":{"query":{"type":"string"}},"required":["query"]}),
            ),
            tool_spec(
                "paper_search",
                "学术论文多源聚合搜索（arXiv/Crossref/PubMed/OpenAlex/Semantic Scholar，并发查询去重）。查论文、文献综述、找 DOI/引用数时使用。返回各源归一化条目：标题/作者/年份/期刊/DOI/链接/引用数/摘要片段。",
                json!({"type":"object","properties":{"query":{"type":"string","description":"检索词（标题/关键词，英文效果最佳）"},"limit":{"type":"integer","description":"每源最大条数（默认 5）"}},"required":["query"]}),
            ),
            tool_spec(
                "read_url",
                "抓取网页 URL 并返回正文文本（搜索后深入阅读、查文档时使用）。自动剥离 HTML 标签，返回纯文本（上限 32KB）。",
                json!({"type":"object","properties":{"url":{"type":"string"}},"required":["url"]}),
            ),
            tool_spec(
                "read_image",
                "读取本地图片文件并返回描述（视觉分析：截图/图表/照片的内容识别）。需要 vision 模型（deepseek-v4-flash-vision-exp）。返回图片中的文字、图表数据、界面元素等信息。",
                json!({"type":"object","properties":{"path":{"type":"string"},"question":{"type":"string","description":"想从图片中了解什么（可选，默认通用描述）"}},"required":["path"]}),
            ),
            tool_spec(
                "take_screenshot",
                "截取屏幕截图。三种模式：1) 全屏截图（不传参数）；2) 按窗口标题或进程名截图（传 target 如 \"Chrome\" / \"chrome.exe\" / \"记事本\"）；3) 列出可截图的窗口（传 list=\"true\"）。截图保存为 PNG，返回文件路径供 read_image 分析。",
                json!({"type":"object","properties":{"target":{"type":"string","description":"窗口标题或进程名（模糊匹配）"},"list":{"type":"boolean","description":"传 true 只列出窗口不截图"}}}),
            ),
            tool_spec(
                "mouse_click",
                "点击鼠标（操作真实系统界面）。坐标为屏幕物理像素，与 take_screenshot 截图坐标一致——务必先截图定位目标再点击。button 默认 left；double=true 双击。",
                json!({"type":"object","properties":{
                    "x":{"type":"integer","description":"屏幕 X（物理像素，同截图坐标）"},
                    "y":{"type":"integer","description":"屏幕 Y（物理像素，同截图坐标）"},
                    "button":{"type":"string","enum":["left","right","middle"],"description":"默认 left"},
                    "double":{"type":"boolean","description":"双击（默认 false）"}
                },"required":["x","y"]}),
            ),
            tool_spec(
                "mouse_move",
                "移动鼠标到指定屏幕坐标（物理像素，同 take_screenshot 截图坐标）。常用于 hover 或为 mouse_scroll 定位。",
                json!({"type":"object","properties":{
                    "x":{"type":"integer"},"y":{"type":"integer"}
                },"required":["x","y"]}),
            ),
            tool_spec(
                "mouse_drag",
                "按住鼠标从起点拖拽到终点（物理像素，同截图坐标）。用于拖动文件/滑块/选区等。button 默认 left。",
                json!({"type":"object","properties":{
                    "from_x":{"type":"integer"},"from_y":{"type":"integer"},
                    "to_x":{"type":"integer"},"to_y":{"type":"integer"},
                    "button":{"type":"string","enum":["left","right","middle"],"description":"默认 left"}
                },"required":["from_x","from_y","to_x","to_y"]}),
            ),
            tool_spec(
                "mouse_scroll",
                "滚动滚轮。direction 默认 down；amount 为格数（默认 3）；可选 x/y 先移动鼠标到该位置（物理像素）。",
                json!({"type":"object","properties":{
                    "direction":{"type":"string","enum":["up","down"],"description":"默认 down"},
                    "amount":{"type":"integer","description":"格数（默认 3，上限 30）"},
                    "x":{"type":"integer","description":"可选：先移动到该坐标"},
                    "y":{"type":"integer"}
                }}),
            ),
            tool_spec(
                "key_type",
                "键入文本（逐字符 Unicode 输入，支持任意语言；\\n 转为回车）。用于向已聚焦的输入框输入内容——通常先 mouse_click 聚焦目标输入框。",
                json!({"type":"object","properties":{
                    "text":{"type":"string"}
                },"required":["text"]}),
            ),
            tool_spec(
                "key_press",
                "按下按键/组合键。格式：\"ctrl+s\"、\"ctrl+shift+t\"、\"alt+f4\"、\"win+r\"，或单键 \"enter\"/\"esc\"/\"tab\"/\"backspace\"/\"delete\"/\"home\"/\"end\"/\"pgup\"/\"pgdn\"/\"up\"/\"down\"/\"left\"/\"right\"/\"space\"/\"f1\"-\"f12\"/字母/数字。",
                json!({"type":"object","properties":{
                    "combo":{"type":"string","description":"如 ctrl+s、alt+f4、enter"}
                },"required":["combo"]}),
            ),
            tool_spec(
                "ask_user",
                "向用户提问，等待回答（对齐 dsh-tool-ask-user）。",
                json!({"type":"object","properties":{"question":{"type":"string"},"options":{"type":"array","items":{"type":"string"}},"header":{"type":"string"}},"required":["question"]}),
            ),
            tool_spec(
                "plan_write",
                "编写执行计划并进入计划模式（显示在 Plan 卡片，先计划后执行）。任务复杂/多步骤时先调用它产出计划，计划完成后再 exit_plan_mode。计划请用 markdown 任务列表（每步一行 `- [ ] 步骤`）；每完成一步，就在执行过程中用 plan_write 更新内容，把该步的 `- [ ]` 改成 `- [x]`，让 Plan 卡片的勾选框实时反映进度。",
                json!({"type":"object","properties":{"content":{"type":"string"}},"required":["content"]}),
            ),
            tool_spec(
                "exit_plan_mode",
                "退出计划模式。⚠️ 调用前必须先用 plan_write 把计划中所有剩余的 `- [ ]` 改成 `- [x]`（标记全部完成），否则计划卡片的进度不会到 N/N。如果还有未完成的步骤就不应该调用本工具。",
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
                "在 DSH 的 Node.js 环境中执行 JavaScript 代码（NODE_PATH 已指向本机 DSH 的 node_modules）。可 require 任意已安装的 DSH 官方插件/依赖（如 @deepseek-ai/dsh-tool-bash）并调用其能力，返回 JSON 结果。适合调用 JS 库/插件、处理复杂数据处理。",
                json!({"type":"object","properties":{"code":{"type":"string"}},"required":["code"]}),
            ),
        ];
        // 论文搜索内置扩展：停用/卸载 → 不列出（dispatch 侧另有守卫）
        if !self.paper_cfg.any_source_on() {
            specs.retain(|s| s.function.name != "paper_search");
        }
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
            "paper_search" => self.tool_paper_search(args),
            "take_screenshot" => self.tool_take_screenshot(args),
            "mouse_click" => self.tool_mouse_click(args),
            "mouse_move" => self.tool_mouse_move(args),
            "mouse_drag" => self.tool_mouse_drag(args),
            "mouse_scroll" => self.tool_mouse_scroll(args),
            "key_type" => self.tool_key_type(args),
            "key_press" => self.tool_key_press(args),
            "read_url" => self.tool_read_url(args),
            "memory_write" => self.tool_memory_write(args),
            "read_image" => self.tool_read_image(args),
            "ask_user" => ToolOutput::ok(json!({
                "await_user": true,
                "note": "问题已提交给用户，回合暂停等待回答",
                "question": args.get("question").and_then(Value::as_str).unwrap_or(""),
                "options": args.get("options").cloned().unwrap_or_else(|| json!([])),
                "header": args.get("header").cloned().unwrap_or_else(|| json!(null)),
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
                    // 系统根目录不纠正：C:\Windows\... 等是明确的系统路径
                    // 意图（不是打错的相对路径），纠正进工作区会让越权写
                    // 静默绕过审批（工作区在 C 盘时曾发生）。
                    const SYSTEM_ROOTS: &[&str] = &[
                        "windows", "program files", "program files (x86)",
                        "programdata", "users", "perflogs", "$recycle.bin",
                    ];
                    let first_is_system = rest
                        .components()
                        .next()
                        .map(|c| {
                            let f = c.as_os_str().to_string_lossy().to_lowercase();
                            SYSTEM_ROOTS.contains(&f.as_str())
                        })
                        .unwrap_or(false);
                    // 深度限制：非盘符部分 ≤3 级才纠正（短路径 = AI 可能拼错；
                    // 深层路径 = 用户真实绝对路径，不该纠正）
                    let depth = rest.components().count();
                    if !first_is_system && !rest.as_os_str().is_empty() && depth <= 3 {
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

    /// shell 写门（workspace-write 限定）：写目标越区 → Err(拒绝文案)。
    /// read-only 由受限令牌覆盖；danger 全通。目标提取用增强解析
    /// （含 cd 穿越跟踪，见 extract_redirect_targets_ctx）。
    /// .NET 直调写的标记（目标不可静态判定，消费端按越区处理）。
    const DOTNET_WRITE_MARKER: &str = "__dotnet_write__";

    fn shell_write_gate(&self, command: &str) -> Result<(), String> {
        let ww = self
            .sandbox
            .as_ref()
            .map(|sb| matches!(sb.mode, crate::exec::SandboxMode::WorkspaceWrite))
            .unwrap_or(false);
        if !ww {
            return Ok(());
        }
        let ws = match &self.workspace_root {
            Some(w) => w.clone(),
            None => return Ok(()),
        };
        for t in extract_redirect_targets_ctx(command) {
            if t.is_flag || t.target.is_empty() {
                continue;
            }
            // .NET 直调写：目标不可判定 → 保守拒绝（fail-closed）
            if t.target == Self::DOTNET_WRITE_MARKER {
                return Err(format!(
                    "仅工作区模式下拒绝 .NET 直调写（{}）：目标路径无法静态判定。\n请改用 Set-Content/Out-File 等可解析形式，或切换权限模式。",
                    t.raw
                ));
            }
            // raw 解析（不走 resolve 的同盘自动改写——历史缺陷：改写后的
            // 区外路径被拼回工作区，gate 看到区内放行而命令实际写原路径）
            let abs = if t.target.contains(':') || t.target.starts_with('\\') {
                std::path::PathBuf::from(&t.target)
            } else {
                self.cwd.join(&t.target)
            };
            if !path_is_within_ws(&abs, &ws) {
                return Err(format!(
                    "仅工作区模式下拒绝越区写：{}（目标 {} 不在工作区 {} 内）。
请改写工作区内路径，或切换权限模式/让用户审批。",
                    t.raw, abs.display(), ws.display()
                ));
            }
        }
        Ok(())
    }

    /// shell 读边界（workspace-write）：命令中引用的绝对路径若在工作区与
    /// 读白名单之外 → 拒绝执行（用户策略：除基础命令外其他盘文件不可访问）。
    /// 与写门互补：写门拦重定向/写 cmdlet 目标，此门拦一切显式区外路径。
    fn shell_read_gate(&self, command: &str) -> Result<(), String> {
        let ww = self
            .sandbox
            .as_ref()
            .map(|sb| matches!(sb.mode, crate::exec::SandboxMode::WorkspaceWrite))
            .unwrap_or(false);
        if !ww {
            return Ok(());
        }
        let ws = match &self.workspace_root {
            Some(w) => w.clone(),
            None => return Ok(()),
        };
        for target in extract_absolute_paths(command) {
            let abs = PathBuf::from(&target);
            if path_is_within_ws(&abs, &ws) || Self::path_in_read_allowlist(&abs) {
                continue;
            }
            return Err(format!(
                "工作区外路径禁止访问（仅工作区模式）：命令引用了 {}（不在工作区 {} 内，
也不属于系统基础路径 / ~/.dsh）。请改用工作区内路径，或切换权限模式。",
                target,
                ws.display()
            ));
        }
        Ok(())
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
        // workspace-write 应用层硬门（fail-closed）：bash/pwsh 的写目标
        // 越出工作区 → 直接拒绝（历史缺陷：WW 模式 OS 层不强制，审批启发式
        // 是唯一闸门，解析漏写即无审批越区写）。read-only 已由受限令牌覆盖。
        if let Err(denied) = self.shell_write_gate(&command) {
            return ToolOutput::err(denied);
        }
        // 读边界（workspace-write）：命令引用区外绝对路径 → 拒绝
        if let Err(denied) = self.shell_read_gate(&command) {
            return ToolOutput::err(denied);
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
        if let Err(denied) = self.shell_write_gate(&command) {
            return ToolOutput::err(denied);
        }
        // 读边界（workspace-write）：命令引用区外绝对路径 → 拒绝
        if let Err(denied) = self.shell_read_gate(&command) {
            return ToolOutput::err(denied);
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
        if let Err(denied) = self.check_read_allowed(&path) {
            return ToolOutput::err(denied);
        }
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
        // 覆盖前捕获旧内容 → 结果携带 diff（UI 渲染 diff 卡片，用户可审阅改动）
        let old = std::fs::read_to_string(&path).ok();
        match std::fs::write(&path, content) {
            Ok(()) => {
                // 编辑审批门:入队(原内容备份;None=新建)
                self.enqueue_edit(
                    &path.display().to_string(),
                    old.clone(),
                    &format!("write_file {}B", content.len()),
                );
                let mut out = json!({"path": path.display().to_string(), "bytes": content.len()});
                match old {
                    Some(old) => {
                        match crate::core::diff::line_diff(&old, content) {
                            Some(d) if !d.is_empty() => {
                                out["diff"] = json!(d);
                            }
                            Some(_) => { /* 内容相同 */ }
                            None => {
                                // 超规模：降级为行数摘要
                                out["diff_note"] = json!(format!(
                                    "整文件重写：{} 行 → {} 行（过大不做逐行 diff）",
                                    old.lines().count(),
                                    content.lines().count()
                                ));
                            }
                        }
                    }
                    None => {
                        out["created"] = json!(true);
                        // 新建：全为增行（UI diff 卡片可审阅全文）
                        if let Some(d) = crate::core::diff::line_diff("", content) {
                            if !d.is_empty() {
                                out["diff"] = json!(d);
                            }
                        }
                    }
                }
                ToolOutput::ok(out)
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
        if let Err(denied) = self.check_read_allowed(&dir) {
            return ToolOutput::err(denied);
        }
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
                // 读边界只管读：view 是纯读操作；create/str_replace 是写
                // 操作，由审批系统与写门治理（历史缺陷：读门套在整个工具
                // 上，用户审批通过的区外写仍被"禁止访问"拦截）
                if let Err(denied) = self.check_read_allowed(&path) {
                    return ToolOutput::err(denied);
                }
                if path.is_dir() {
                    return self.tool_list_dir(args);
                }
                // 大小护栏（历史缺陷：view 无上限，GB 级日志整读入内存；
                // read_file 有 8MB 上限而 view 没有）
                if let Ok(m) = std::fs::metadata(&path) {
                    if m.len() > 8 * 1024 * 1024 {
                        return ToolOutput::err(format!(
                            "文件过大（{} MB > 8MB 上限），请用 read_file 的 offset/limit 分段读取",
                            m.len() / 1024 / 1024
                        ));
                    }
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
                        // 编辑审批门：新建(None=回滚即删除)
                        self.enqueue_edit(
                            &path.display().to_string(),
                            None,
                            &format!("create {}B", content.len()),
                        );
                        // 新建文件：diff = 全部为新增行（UI diff 卡片可审阅全文）
                        let mut out = json!({
                            "path": path.display().to_string(),
                            "created": true
                        });
                        if let Some(d) = crate::core::diff::line_diff("", content) {
                            if !d.is_empty() {
                                out["diff"] = json!(d);
                            }
                        }
                        ToolOutput::ok(out)
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
                if let Ok(m) = std::fs::metadata(&path) {
                    if m.len() > 8 * 1024 * 1024 {
                        return ToolOutput::err(format!(
                            "文件过大（{} MB > 8MB 上限），请用 read_file 分段读取后再改",
                            m.len() / 1024 / 1024
                        ));
                    }
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
                            Ok(()) => {
                                // 编辑审批门：原内容入队
                                self.enqueue_edit(
                                    &path.display().to_string(),
                                    Some(text.clone()),
                                    &format!("str_replace {}B→{}B", old_str.len(), new_str.len()),
                                );
                                let mut out = json!({
                                    "path": path.display().to_string(),
                                    "replaced": true,
                                    "old_len": old_str.len(),
                                    "new_len": new_str.len()
                                });
                                // 片段 diff（old_str → new_str）：改动可视
                                if let Some(d) = crate::core::diff::line_diff(old_str, new_str) {
                                    if !d.is_empty() {
                                        out["diff"] = json!(d);
                                    }
                                }
                                ToolOutput::ok(out)
                            }
                            Err(e) => ToolOutput::err(e.to_string()),
                        }
                    }
                    Err(e) => ToolOutput::err(e.to_string()),
                }
            }
            other => ToolOutput::err(format!("unknown str_replace_editor command: {other}")),
        }
    }

    /// fs_search：文件名模糊 + 文件内容全文搜索（双模式）。
    /// pattern 含通配符(*?)时按文件名 glob 匹配；否则先匹配文件名，
    /// 不中则读文件内容搜索（≤256KB 文本文件）。
    fn tool_fs_search(&self, args: &Value) -> ToolOutput {
        let dir = args
            .get("path")
            .and_then(Value::as_str)
            .map(|s| self.resolve(s))
            .unwrap_or_else(|| self.cwd.clone());
        if let Err(denied) = self.check_read_allowed(&dir) {
            return ToolOutput::err(denied);
        }
        let pattern = args
            .get("pattern")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if pattern.is_empty() {
            return ToolOutput::err("pattern 不能为空");
        }
        let pattern_lower = pattern.to_lowercase();
        let is_glob = pattern.contains('*') || pattern.contains('?');
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
            let mut dir_scanned = 0usize;
            for e in entries.flatten() {
                dir_scanned += 1;
                if dir_scanned > 2000 {
                    break;
                }
                scanned += 1;
                let name = e.file_name().to_string_lossy().into_owned();
                if name.starts_with('.') || name == "target" || name == "node_modules" {
                    continue;
                }
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                    continue;
                }
                let name_lower = name.to_lowercase();
                let name_match = if is_glob {
                    glob_match(&pattern_lower, &name_lower)
                } else {
                    name_lower.contains(&pattern_lower)
                };
                if name_match {
                    hits.push(json!({"path": p.display().to_string(), "kind": "name"}));
                    continue;
                }
                // 内容搜索：非 glob 且文件名不中时，读小文本文件搜内容
                if !is_glob && hits.len() < 50 {
                    if let Ok(meta) = std::fs::metadata(&p) {
                        if meta.len() <= 256 * 1024 {
                            if let Ok(text) = std::fs::read_to_string(&p) {
                                if text.to_lowercase().contains(&pattern_lower) {
                                    // 找到匹配行（最多 3 行上下文）
                                    let matching: Vec<&str> = text
                                        .lines()
                                        .filter(|l| l.to_lowercase().contains(&pattern_lower))
                                        .take(3)
                                        .collect();
                                    hits.push(json!({
                                        "path": p.display().to_string(),
                                        "kind": "content",
                                        "lines": matching,
                                    }));
                                }
                            }
                        }
                    }
                }
            }
        }
        hits.truncate(50);
        ToolOutput::ok(json!({"hits": hits, "count": hits.len()}))
    }

    /// take_screenshot：全屏 / 按窗口截图。
    fn tool_take_screenshot(&self, args: &Value) -> ToolOutput {
        // 模式 3：列出窗口
        if args.get("list").and_then(|v| v.as_bool()).unwrap_or(false) {
            let windows = crate::exec::capture::find_windows("");
            let list: Vec<serde_json::Value> = windows
                .iter()
                .map(|w| {
                    json!({
                        "title": w.title,
                        "pid": w.pid,
                        "process": w.process_name,
                    })
                })
                .collect();
            return ToolOutput::ok(json!({
                "windows": list,
                "count": list.len(),
                "note": "传 target 参数截图指定窗口",
            }));
        }

        let target = args.get("target").and_then(|v| v.as_str()).unwrap_or("");

        if target.is_empty() {
            // 模式 1：全屏
            match crate::exec::capture::capture_screen() {
                Ok(c) => ToolOutput::ok(json!({
                    "path": c.path.display().to_string(),
                    "width": c.width,
                    "height": c.height,
                    "source": c.source,
                    "note": "全屏截图已保存，可用 read_image 分析内容",
                })),
                Err(e) => ToolOutput::err(format!("截图失败: {e}")),
            }
        } else {
            // 模式 2：按标题/进程名
            let windows = crate::exec::capture::find_windows(target);
            if windows.is_empty() {
                return ToolOutput::err(format!(
                    "未找到匹配 \"{target}\" 的窗口（传 list=true 查看可用窗口列表）"
                ));
            }
            // 截第一个匹配的
            let w = &windows[0];
            match crate::exec::capture::capture_window(w.hwnd) {
                Ok(c) => ToolOutput::ok(json!({
                    "path": c.path.display().to_string(),
                    "width": c.width,
                    "height": c.height,
                    "source": c.source,
                    "pid": w.pid,
                    "process": w.process_name,
                    "matched": windows.len(),
                    "note": "窗口截图已保存（即使被遮挡也能截取），可用 read_image 分析",
                })),
                Err(e) => ToolOutput::err(format!("截图失败: {e}")),
            }
        }
    }

    /// GUI 输入注入（鼠标点击/移动/拖拽/滚动/键盘）的沙箱闸门：
    /// read-only / workspace-write 模式下禁止——注入输入等效于任意系统
    /// 操作（可绕过文件边界：点开资源管理器删任意盘文件）。全访问允许。
    fn check_gui_input_allowed(&self) -> Result<(), String> {
        if let Some(sb) = &self.sandbox {
            match sb.mode {
                crate::exec::SandboxMode::DangerFullAccess => {}
                _ => {
                    return Err(format!(
                        "沙箱（{}）下禁止 GUI 输入注入——点击/键入可绕过文件边界操作系统任意内容。
如需 GUI 自动化，请切换到 danger-full-access 权限模式。",
                        sb.mode.as_str()
                    ))
                }
            }
        }
        Ok(())
    }

    /// mouse_click：点击鼠标（坐标 = 截图物理像素）。
    fn tool_mouse_click(&self, args: &Value) -> ToolOutput {
        if let Err(denied) = self.check_gui_input_allowed() {
            return ToolOutput::err(denied);
        }
        let (Some(x), Some(y)) = (
            args.get("x").and_then(Value::as_i64),
            args.get("y").and_then(Value::as_i64),
        ) else {
            return ToolOutput::err("需要 x, y（屏幕物理像素，与 take_screenshot 截图坐标一致）");
        };
        let button = args
            .get("button")
            .and_then(Value::as_str)
            .and_then(crate::exec::input::MouseButton::parse)
            .unwrap_or(crate::exec::input::MouseButton::Left);
        let double = args.get("double").and_then(Value::as_bool).unwrap_or(false);
        match crate::exec::input::mouse_click(x as i32, y as i32, button, double) {
            Ok(()) => ToolOutput::ok(json!({
                "done": true, "x": x, "y": y,
                "button": format!("{button:?}").to_lowercase(),
                "double": double,
            })),
            Err(e) => ToolOutput::err(e),
        }
    }

    /// mouse_move：移动鼠标。
    fn tool_mouse_move(&self, args: &Value) -> ToolOutput {
        if let Err(denied) = self.check_gui_input_allowed() {
            return ToolOutput::err(denied);
        }
        let (Some(x), Some(y)) = (
            args.get("x").and_then(Value::as_i64),
            args.get("y").and_then(Value::as_i64),
        ) else {
            return ToolOutput::err("需要 x, y");
        };
        match crate::exec::input::mouse_move(x as i32, y as i32) {
            Ok(()) => ToolOutput::ok(json!({"done": true, "x": x, "y": y})),
            Err(e) => ToolOutput::err(e),
        }
    }

    /// mouse_drag：按住拖拽。
    fn tool_mouse_drag(&self, args: &Value) -> ToolOutput {
        if let Err(denied) = self.check_gui_input_allowed() {
            return ToolOutput::err(denied);
        }
        let nums = |k: &str| args.get(k).and_then(Value::as_i64);
        let (Some(fx), Some(fy), Some(tx), Some(ty)) = (
            nums("from_x"),
            nums("from_y"),
            nums("to_x"),
            nums("to_y"),
        ) else {
            return ToolOutput::err("需要 from_x, from_y, to_x, to_y（物理像素）");
        };
        let button = args
            .get("button")
            .and_then(Value::as_str)
            .and_then(crate::exec::input::MouseButton::parse)
            .unwrap_or(crate::exec::input::MouseButton::Left);
        match crate::exec::input::mouse_drag(
            (fx as i32, fy as i32),
            (tx as i32, ty as i32),
            button,
        ) {
            Ok(()) => ToolOutput::ok(json!({
                "done": true,
                "from": [fx, fy], "to": [tx, ty],
            })),
            Err(e) => ToolOutput::err(e),
        }
    }

    /// mouse_scroll：滚轮。
    fn tool_mouse_scroll(&self, args: &Value) -> ToolOutput {
        if let Err(denied) = self.check_gui_input_allowed() {
            return ToolOutput::err(denied);
        }
        let up = matches!(
            args.get("direction").and_then(Value::as_str),
            Some("up") | Some("UP") | Some("Up")
        );
        let amount = args
            .get("amount")
            .and_then(Value::as_i64)
            .unwrap_or(3)
            .clamp(1, 30) as i32;
        let x = args.get("x").and_then(Value::as_i64).map(|v| v as i32);
        let y = args.get("y").and_then(Value::as_i64).map(|v| v as i32);
        match crate::exec::input::mouse_scroll(x, y, amount, up) {
            Ok(()) => ToolOutput::ok(json!({
                "done": true,
                "direction": if up { "up" } else { "down" },
                "amount": amount,
            })),
            Err(e) => ToolOutput::err(e),
        }
    }

    /// key_type：键入文本（Unicode）。
    fn tool_key_type(&self, args: &Value) -> ToolOutput {
        if let Err(denied) = self.check_gui_input_allowed() {
            return ToolOutput::err(denied);
        }
        let text = args.get("text").and_then(Value::as_str).unwrap_or_default();
        if text.is_empty() {
            return ToolOutput::err("text 不能为空");
        }
        match crate::exec::input::key_type_text(text) {
            Ok(()) => ToolOutput::ok(json!({
                "done": true,
                "chars": text.chars().count(),
            })),
            Err(e) => ToolOutput::err(e),
        }
    }

    /// key_press：按键/组合键。
    fn tool_key_press(&self, args: &Value) -> ToolOutput {
        if let Err(denied) = self.check_gui_input_allowed() {
            return ToolOutput::err(denied);
        }
        let combo = args.get("combo").and_then(Value::as_str).unwrap_or_default().trim();
        if combo.is_empty() {
            return ToolOutput::err("combo 不能为空（如 ctrl+s、alt+f4、enter）");
        }
        // 先解析校验，非法组合直接报错（不产生半执行状态）
        if let Err(e) = crate::exec::input::parse_combo(combo) {
            return ToolOutput::err(e);
        }
        match crate::exec::input::key_press_combo(combo) {
            Ok(()) => ToolOutput::ok(json!({"done": true, "combo": combo})),
            Err(e) => ToolOutput::err(e),
        }
    }

    /// read_url：抓取网页正文（搜索后深入阅读）。
    fn tool_read_url(&self, args: &Value) -> ToolOutput {
        let url = args.get("url").and_then(Value::as_str).unwrap_or_default().trim().to_string();
        if url.is_empty() {
            return ToolOutput::err("url 不能为空");
        }
        if !url.starts_with("http://") && !url.starts_with("https://") {
            return ToolOutput::err("url 必须以 http:// 或 https:// 开头");
        }
        let proxy = self.http_proxy.as_deref();
        let body = crate::core::tools::fetch_url_text(&url, proxy);
        match body {
            Ok(text) => {
                const MAX: usize = 32 * 1024;
                let truncated = text.len() > MAX;
                let text = if truncated { text[..MAX].to_string() } else { text };
                ToolOutput::ok(json!({
                    "url": url,
                    "content": text,
                    "truncated": truncated,
                }))
            }
            Err(e) => ToolOutput::err(format!("抓取失败: {e}")),
        }
    }

    /// read_image：视觉分析（读取本地图片 + 可选提问）。
    /// 实现：把图片路径返回给调用方，由 vision LLM 分析——本工具
    /// 自身不调 API，而是在结果里标记"图片已就绪"，让 agent 回合
    /// 在下一轮把图片作为多模态消息发给 vision 模型。
    fn tool_read_image(&self, args: &Value) -> ToolOutput {
        let path = args.get("path").and_then(Value::as_str).unwrap_or_default();
        if path.is_empty() {
            return ToolOutput::err("path 不能为空");
        }
        let resolved = self.resolve(path);
        if let Err(denied) = self.check_read_allowed(&resolved) {
            return ToolOutput::err(denied);
        }
        // 检查文件存在 + 是图片
        let ext = resolved
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .unwrap_or_default();
        if !matches!(ext.as_str(), "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp") {
            return ToolOutput::err(format!(
                "不支持的图片格式 .{ext}（支持 png/jpg/jpeg/gif/webp/bmp）"
            ));
        }
        match std::fs::metadata(&resolved) {
            Ok(m) if m.len() > 8 * 1024 * 1024 => {
                return ToolOutput::err(format!(
                    "图片过大（{} MB > 8MB 上限）",
                    m.len() / 1024 / 1024
                ));
            }
            Err(e) => return ToolOutput::err(format!("文件不可读: {e}")),
            _ => {}
        }
        let question = args.get("question").and_then(Value::as_str).unwrap_or("");
        ToolOutput::ok(json!({
            "path": resolved.display().to_string(),
            "ready": true,
            "question": question,
            "note": "图片已就绪。下一回合把此图片路径附加到消息中，vision 模型将分析图片内容。",
        }))
    }

    /// paper_search：内置论文扩展（多源聚合；扩展页可配置/卸载）。
    fn tool_paper_search(&self, args: &Value) -> ToolOutput {
        if !self.paper_cfg.any_source_on() {
            return ToolOutput::err("论文搜索扩展已停用/卸载（扩展页可重新启用）");
        }
        let query = args
            .get("query")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim();
        if query.is_empty() {
            return ToolOutput::err("query 不能为空");
        }
        let mut cfg = self.paper_cfg.clone();
        if let Some(l) = args.get("limit").and_then(|v| v.as_u64()) {
            cfg.max_per_source = (l as usize).clamp(1, 20);
        }
        let (hits, errors) = crate::core::paper::search(query, &cfg, self.http_proxy.as_deref());
        let mut out = json!({
            "query": query,
            "count": hits.len(),
            "results": hits,
        });
        if !errors.is_empty() {
            out["source_errors"] = json!(errors
                .iter()
                .map(|(k, e)| format!("{k}: {e}"))
                .collect::<Vec<_>>());
        }
        ToolOutput::ok(out)
    }

    /// memory_write：长期记忆追加（$DSH_HOME/memory.md;回合循环侧
    /// 另有一份同路径实现——dispatch 命中时走这里,两者幂等）。
    fn tool_memory_write(&self, args: &Value) -> ToolOutput {
        let note = args
            .get("note")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim();
        if note.is_empty() {
            return ToolOutput::err("note 不能为空");
        }
        let home = std::env::var_os("DSH_HOME")
            .map(std::path::PathBuf::from)
            .or_else(|| {
                std::env::var_os("USERPROFILE")
                    .map(|p| std::path::PathBuf::from(p).join(".dsh"))
            })
            .unwrap_or_default();
        let path = home.join("memory.md");
        let write = || -> std::io::Result<()> {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            use std::io::Write as _;
            let ts = chrono::Local::now().format("%Y-%m-%d %H:%M");
            let mut f = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)?;
            f.write_all(format!("- [{ts}] {note}
").as_bytes())
        };
        match write() {
            Ok(()) => ToolOutput::ok(json!({
                "written": true,
                "note": note,
                "path": path.display().to_string(),
            })),
            Err(e) => ToolOutput::err(format!("写入失败: {e}")),
        }
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

/// 字节下标回退到字符边界（中文页面 5KB 截断防 panic）。
fn floor_char_boundary(s: &str, i: usize) -> usize {
    if i >= s.len() {
        return s.len();
    }
    let mut b = i;
    while b > 0 && !s.is_char_boundary(b) {
        b -= 1;
    }
    b
}

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
        // 块结尾：下一个 b_algo" 或 5KB 截断（防异常页面死循环）。
        // 字节截断必须落到字符边界（历史 panic：中文页面块 >5000 字节时
        // 5000 不是 UTF-8 边界 → 切片 panic，web_search 固定失败）
        let seg = &rest[block_start..];
        if seg.len() < 8 {
            break; // 恰好以 b_algo" 结尾(极端畸形)——防 seg[8..] 越界
        }
        let seg_end = seg[8..]
            .find("b_algo\"")
            .map(|i| 8 + i)
            .unwrap_or_else(|| floor_char_boundary(seg, 5000));
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
/// 写目标（带解析上下文：cd 穿越后的逻辑 cwd 与原始 token）。
#[derive(Debug)]
pub struct WriteTarget {
    /// 目标 token 原文（审计显示）
    pub raw: String,
    /// 规范化目标（绝对或相对——按逻辑 cwd 解析）
    pub target: String,
    /// 纯标志（/Y、-Force 等被跳过的 token 不算目标）
    pub is_flag: bool,
}

/// 增强版写目标提取：跟踪 `cd X &&`/`Set-Location X;`/`pushd X` 改变逻辑
/// cwd（历史缺陷：`cd C:\Windows && echo x > f` 的 f 被按工具 cwd 解析为
/// 工作区内 → 无审批越区写），并补充常见写命令（curl -o / robocopy /
/// Invoke-WebRequest -OutFile / .NET [IO.File]::Write* 直调等）。
fn extract_redirect_targets_ctx(command: &str) -> Vec<WriteTarget> {
    let mut out: Vec<WriteTarget> = Vec::new();
    let mut cwd: Vec<String> = Vec::new(); // 逻辑 cwd 段（空 = 工具 cwd）
    let toks: Vec<&str> = command.split_whitespace().collect();
    let mut i = 0usize;
    let mut prev_is_redirect = false;
    let mut in_write = false;
    let mut pending_outfile = false; // -OutFile/-o/-O 的下一个 token 是目标

    const WRITE_CMDS: &[&str] = &[
        "copy", "move", "del", "erase", "ren", "rename", "md", "mkdir", "rd",
        "cp", "mv", "rm", "tee", "install", "truncate", "shred", "xcopy",
        "robocopy", "curl", "wget", "certutil", "reg", "icacls", "expand",
        "copy-item", "move-item", "set-content", "add-content", "out-file",
        "new-item", "remove-item", "clear-content", "sc", "ni", "ac", "cpi",
    ];

    while i < toks.len() {
        let t = toks[i];
        let lower = t.to_ascii_lowercase();
        // 逻辑 cwd 跟踪（cd /d X、Set-Location X、pushd X）
        if lower == "cd" || lower == "set-location" || lower == "pushd" {
            // 跳过 /d 标志
            let mut j = i + 1;
            if j < toks.len() && (toks[j].eq_ignore_ascii_case("/d") || toks[j].starts_with('-')) {
                j += 1;
            }
            if j < toks.len() {
                cwd = vec![toks[j].trim_matches('"').to_string()];
                i = j + 1;
                continue;
            }
        }
        // .NET 直调写（[IO.File]::WriteAllText(...) / [System.IO.File]::...）
        if t.starts_with('[') && t.contains("::") {
            let l = lower;
            if l.contains("writeall") || l.contains("writebytes")
                || l.contains("appendall") || l.contains("delete")
                || l.contains("move") || l.contains("copy")
            {
                out.push(WriteTarget {
                    raw: t.to_string(),
                    // .NET 直调：标记（消费端保守拒绝）
                    target: "__dotnet_write__".to_string(),
                    is_flag: false,
                });
            }
            i += 1;
            continue;
        }
        // curl -o / wget -O / -OutFile 的参数式目标
        if pending_outfile {
            pending_outfile = false;
            out.push(WriteTarget {
                raw: t.to_string(),
                target: join_cwd(&cwd, t.trim_matches('"')),
                is_flag: false,
            });
            i += 1;
            continue;
        }
        if lower == "-o" || lower == "-outfile" || lower == "/o" {
            pending_outfile = true;
            i += 1;
            continue;
        }
        if lower == "-uri" || lower == "-url" || lower == "-path" {
            // -Path 的值也可能是目标（Set-Content -Path x）：按下一个 token 记
            let j = i + 1;
            if j < toks.len() && !toks[j].starts_with('-') {
                if in_write {
                    out.push(WriteTarget {
                        raw: toks[j].to_string(),
                        target: join_cwd(&cwd, toks[j].trim_matches('"')),
                        is_flag: false,
                    });
                }
                i = j + 1;
                continue;
            }
        }
        // 重定向形式（> >> 2> fd>）
        if let Some(rest) = strip_fd_redirect(&lower) {
            prev_is_redirect = true;
            in_write = false;
            if !rest.is_empty() {
                out.push(WriteTarget {
                    raw: t.to_string(),
                    target: join_cwd(&cwd, rest.trim_matches('"')),
                    is_flag: false,
                });
            }
            i += 1;
            continue;
        }
        if t.starts_with('>') {
            prev_is_redirect = true;
            in_write = false;
            let rest = t.trim_start_matches('>').trim();
            if !rest.is_empty() {
                out.push(WriteTarget {
                    raw: t.to_string(),
                    target: join_cwd(&cwd, rest.trim_matches('"')),
                    is_flag: false,
                });
            }
            i += 1;
            continue;
        }
        if prev_is_redirect {
            prev_is_redirect = false;
            out.push(WriteTarget {
                raw: t.to_string(),
                target: join_cwd(&cwd, t.trim_matches('"')),
                is_flag: false,
            });
            i += 1;
            continue;
        }
        // 写命令关键字
        let base = lower
            .trim_end_matches(',')
            .trim_matches(|c: char| c == '(' || c == '|');
        if WRITE_CMDS.contains(&base) {
            in_write = true;
            i += 1;
            continue;
        }
        if in_write && !t.starts_with('/') && !t.starts_with('-') {
            // URL 不是写目标（历史缺陷：curl 在 WRITE_CMDS 里，其 URL 参数
            // 被当写目标 → "仅工作区模式下拒绝越区写：https://…"，
            // 工作区内 curl 下载被误拦）。scheme 开头的 token 一律跳过。
            let tl = t.trim_matches('"').to_ascii_lowercase();
            if tl.starts_with("http://")
                || tl.starts_with("https://")
                || tl.starts_with("ftp://")
                || tl.starts_with("file://")
            {
                i += 1;
                continue;
            }
            out.push(WriteTarget {
                raw: t.to_string(),
                target: join_cwd(&cwd, t.trim_matches('"')),
                is_flag: false,
            });
            // copy/mv src dst：只取最后一个非标志 token 需要两遍——简化为
            // 全记（宁多拒不漏放：审批/门以"任一越区即拒"为准）
            i += 1;
            continue;
        }
        if in_write && (t.starts_with('/') || t.starts_with('-')) {
            i += 1;
            continue;
        }
        // 分隔符重置写上下文
        if t == "&&" || t == ";" || t == "|" || t == "&" {
            in_write = false;
            prev_is_redirect = false;
        }
        i += 1;
    }
    out
}

/// 逻辑 cwd 拼接（cwd 为空 = 相对工具 cwd；目标已绝对则原样）。
fn join_cwd(cwd: &[String], target: &str) -> String {
    if cwd.is_empty()
        || target.contains(':')
        || target.starts_with('\\')
        || target.starts_with('/')
    {
        target.to_string()
    } else {
        let sep = std::path::MAIN_SEPARATOR;
        format!("{}{sep}{}", cwd.join(&sep.to_string()), target)
    }
}

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
/// 提取命令字符串中的绝对路径（盘符形式 `X:\...` / `X:/...`）。
/// 供 shell 读边界使用：命令引用了工作区外的显式路径 → 拒绝。
/// - URL 误判防护：`https://` 的 `s:` 前一个字符是字母——要求盘符字母前
///   是边界字符（行首/空白/引号/`=`/`(`/`,` 等）
/// - token 向后延伸到空白/引号/命令分隔符（`;|&<>()[]{},=`）为止
pub(crate) fn extract_absolute_paths(cmd: &str) -> Vec<String> {
    let b: Vec<char> = cmd.chars().collect();
    let bslash: char = char::from_u32(0x5C).unwrap(); // 反斜杠（避免转义地狱）
    let stop = |c: char| c.is_whitespace() || "\"'`;|&<>()[]{}=,".contains(c);
    let mut out = Vec::new();
    let mut i = 0;
    while i + 2 < b.len() {
        let (c, next, n2) = (b[i], b[i + 1], b[i + 2]);
        if c.is_ascii_alphabetic() && next == ':' && (n2 == '\\' || n2 == '/') {
            let prev_ok = i == 0 || {
                let p = b[i - 1];
                !p.is_alphanumeric() && !"-./~_$".contains(p)
            };
            if prev_ok {
                let start = i;
                let mut j = i + 2;
                while j < b.len() && !stop(b[j]) {
                    j += 1;
                }
                let token: String = b[start..j].iter().collect();
                if token.chars().count() > 3 {
                    out.push(token);
                }
                i = j;
                continue;
            }
        } else if c == bslash && i + 1 < b.len() && b[i + 1] == bslash {
            // UNC 路径（\server\share\x）：同样按边界起步提取
            //（历史缺陷：只认盘符，UNC 静默绕过读门）
            let prev_ok = i == 0 || {
                let p = b[i - 1];
                !p.is_alphanumeric() && !"-./~_$".contains(p)
            };
            if prev_ok {
                let start = i;
                let mut j = i + 2;
                while j < b.len() && !stop(b[j]) {
                    j += 1;
                }
                let token: String = b[start..j].iter().collect();
                if token.chars().count() > 3 {
                    out.push(token);
                }
                i = j;
                continue;
            }
        }
        i += 1;
    }
    out
}

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


/// 抓取 URL 并提取正文文本（剥 HTML 标签，保留代码块内容）。
/// 30s 超时 + 1MB 上限（防超大页面 OOM）。
pub fn fetch_url_text(url: &str, proxy: Option<&str>) -> Result<String, String> {
    let mut builder = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(30));
    if let Some(p) = proxy {
        builder = builder.proxy(
            reqwest::Proxy::all(p).map_err(|e| format!("无效代理: {e}"))?,
        );
    }
    let client = builder.build().map_err(|e| format!("HTTP 客户端初始化失败: {e}"))?;
    let resp = client
        .get(url)
        .header("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36")
        .send()
        .map_err(|e| format!("请求失败: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()));
    }
    let html = resp.text().map_err(|e| format!("读取响应失败: {e}"))?;
    if html.len() > 1024 * 1024 {
        return Err("页面超过 1MB 上限".into());
    }
    Ok(strip_html_to_text(&html))
}

/// 极简 HTML→文本：剥 script/style 块、标签、多余空白。
fn strip_html_to_text(html: &str) -> String {
    let mut s: String = html.to_string();
    // 去 script/style/noscript 块
    for tag in ["script", "style", "noscript"] {
        loop {
            let Some(start) = s.find(&format!("<{tag}")) else { break };
            let Some(end_tok) = s.find(&format!("</{tag}>")) else { break };
            if end_tok <= start { break; }
            let after = s[end_tok + tag.len() + 3..].to_string();
            s = format!("{}{}", &s[..start], after);
        }
    }
    // 换行标签 → 

    for tag in ["<br", "<BR", "<p", "<P", "<div", "<DIV", "<li", "<LI", "<tr", "<TR"] {
        s = s.replace(tag, &format!("
{tag}"));
    }
    // 去所有标签
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
    // HTML 实体
    let out = out
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ");
    // 压缩空白
    let mut clean = String::with_capacity(out.len());
    let mut last_nl = false;
    for line in out.lines() {
        let t = line.trim();
        if t.is_empty() {
            if !last_nl {
                clean.push('\n');
                last_nl = true;
            }
        } else {
            clean.push_str(t);
            clean.push('\n');
            last_nl = false;
        }
    }
    clean
}


/// 极简 glob 匹配（支持 * 和 ?，大小写不敏感——调用方已 to_lowercase）。
fn glob_match(pattern: &str, text: &str) -> bool {
    fn rec(p: &[u8], t: &[u8]) -> bool {
        match (p.first(), t.first()) {
            (None, None) => true,
            (Some(b'*'), _) => {
                // * 匹配零个或多个字符
                rec(&p[1..], t) || (!t.is_empty() && rec(p, &t[1..]))
            }
            (Some(b'?'), Some(_)) => rec(&p[1..], &t[1..]),
            (Some(&pc), Some(&tc)) if pc == tc => rec(&p[1..], &t[1..]),
            _ => false,
        }
    }
    rec(pattern.as_bytes(), text.as_bytes())
}

#[cfg(test)]
mod paper_tool_tests {
    use crate::core::tools::ToolRegistry;

    /// 论文扩展：启用 → paper_search 列出；停用/卸载 → 不列出且 dispatch 拒绝。
    #[test]
    fn paper_search_spec_gated_by_extension_state() {
        let reg = ToolRegistry::new(std::path::PathBuf::from("."));
        assert!(
            reg.tool_specs().iter().any(|s| s.function.name == "paper_search"),
            "默认启用应列出"
        );
        let mut cfg = reg.paper_cfg.clone();
        cfg.uninstalled = true;
        cfg.enabled = false;
        let off = reg.with_paper_cfg(cfg);
        assert!(
            !off.tool_specs().iter().any(|s| s.function.name == "paper_search"),
            "卸载后不应列出"
        );
        let out = off.dispatch(
            "paper_search",
            &serde_json::json!({"query": "test"}),
        );
        assert!(!out.ok, "卸载后 dispatch 应拒绝: {:?}", out.value);
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

    /// workspace-write 读边界：工作区外读取被拒；工作区内 / 系统基础 /
    /// ~/.dsh 放行（用户策略：除基础命令与 DSH 目录外，区外一律不可访问）。
    #[test]
    fn workspace_write_blocks_outside_reads() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().join("workspace");
        std::fs::create_dir_all(&ws).unwrap();
        let reg = ToolRegistry::new(ws.clone())
            .with_workspace_root(Some(ws.clone()))
            .with_sandbox(Some(crate::exec::acl::WindowsAclSandbox::new(
                crate::exec::SandboxMode::WorkspaceWrite,
                ws.clone(),
            )));
        assert!(
            reg.check_read_allowed(&ws.join("a.txt")).is_ok(),
            "工作区内读取应放行"
        );
        assert!(
            reg.check_read_allowed(std::path::Path::new(
                r"C:\Windows\System32\cmd.exe"
            ))
            .is_ok(),
            "系统基础路径应放行"
        );
        let dsh = match std::env::var_os("DSH_HOME") {
            Some(h) if !h.is_empty() => std::path::PathBuf::from(h),
            _ => std::env::var_os("USERPROFILE")
                .map(|h| std::path::PathBuf::from(h).join(".dsh"))
                .unwrap(),
        };
        assert!(
            reg.check_read_allowed(&dsh.join(r"skills\demo\skill.md")).is_ok(),
            "~/.dsh 应放行: {}",
            dsh.display()
        );
        let outside = reg.check_read_allowed(std::path::Path::new(r"D:\secret.txt"));
        assert!(outside.is_err(), "区外读取必须被拒绝");
        assert!(outside.unwrap_err().contains("禁止访问"));

        let reg2 = ToolRegistry::new(ws.clone()).with_workspace_root(Some(ws));
        assert!(
            reg2.check_read_allowed(std::path::Path::new(r"D:\secret.txt")).is_ok(),
            "全访问模式读取不受限"
        );
    }

    /// read_file 工具级拦截：WW 模式下读区外路径直接报错（不触碰文件系统）。
    #[test]
    fn read_file_blocked_outside_ws() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().join("workspace");
        std::fs::create_dir_all(&ws).unwrap();
        let reg = ToolRegistry::new(ws.clone())
            .with_workspace_root(Some(ws.clone()))
            .with_sandbox(Some(crate::exec::acl::WindowsAclSandbox::new(
                crate::exec::SandboxMode::WorkspaceWrite,
                ws.clone(),
            )));
        let out = reg.tool_read_file(&json!({"path": r"D:\secret.txt"}));
        assert!(!out.ok, "区外 read_file 必须被拒绝");
        assert!(out.stderr.contains("禁止访问"), "{}", out.stderr);
    }

    /// 审批与读门不冲突：str_replace 的 create（写操作）不走读门——
    /// 审批通过后可执行（历史缺陷：读门套在整个工具上，区外写被
    /// "禁止访问"拦截，审批形同虚设）。view（纯读）仍被拦。
    #[test]
    fn str_replace_create_not_blocked_by_read_gate() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().join("workspace");
        std::fs::create_dir_all(&ws).unwrap();
        let reg = ToolRegistry::new(ws.clone())
            .with_workspace_root(Some(ws.clone()))
            .with_sandbox(Some(crate::exec::acl::WindowsAclSandbox::new(
                crate::exec::SandboxMode::WorkspaceWrite,
                ws.clone(),
            )));
        // create（写）：不得因读门报"禁止访问"
        let out = reg.tool_str_replace(&json!({
            "command": "create",
            "path": dir.path().join("out.txt").to_string_lossy(),
            "file_text": "x",
        }));
        assert!(
            !out.stderr.contains("禁止访问"),
            "create 是写操作，不应被读门拦截: {}",
            out.stderr
        );
        // view（纯读）区外：仍被拦
        let outside = dir.path().join("some.txt");
        std::fs::write(&outside, "s").unwrap();
        let out = reg.tool_str_replace(&json!({
            "command": "view",
            "path": outside.to_string_lossy(),
        }));
        assert!(!out.ok && out.stderr.contains("禁止访问"));
    }

    /// shell 读边界：命令引用区外绝对路径 → 拒绝；URL 不误判。
    #[test]
    fn shell_read_gate_paths() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().join("workspace");
        std::fs::create_dir_all(&ws).unwrap();
        let reg = ToolRegistry::new(ws.clone())
            .with_workspace_root(Some(ws.clone()))
            .with_sandbox(Some(crate::exec::acl::WindowsAclSandbox::new(
                crate::exec::SandboxMode::WorkspaceWrite,
                ws.clone(),
            )));
        assert!(reg.shell_read_gate("type D:\\secret.txt").is_err());
        assert!(
            reg.shell_read_gate(&format!(
                "Get-Content '{}'",
                dir.path().join("outside.txt").display()
            ))
            .is_err(),
            "工作区父目录也是区外"
        );
        assert!(reg.shell_read_gate("Get-Process").is_ok());
        assert!(reg.shell_read_gate("dir").is_ok());
        assert!(reg.shell_read_gate("type C:\\Windows\\win.ini").is_ok());
        assert!(reg.shell_read_gate("curl https://example.com/a:b").is_ok());

        // UNC 同样拦截（历史缺陷：只认盘符时绕过）
        assert!(reg.shell_read_gate("type \\\\nas\\\\data\\\\x.txt").is_err());
        let reg2 = ToolRegistry::new(ws).with_workspace_root(None);
        assert!(reg2.shell_read_gate("type D:\\secret.txt").is_ok());
    }

    /// 绝对路径提取：URL（https://）不得被误判为盘符路径。
    #[test]
    fn extract_absolute_paths_skips_urls() {
        let got = extract_absolute_paths("curl https://example.com and http://a.b/c");
        assert!(got.is_empty(), "URL 不应误判: {got:?}");
        let got = extract_absolute_paths("type D:\\secret.txt");
        assert_eq!(got, vec![r"D:\secret.txt".to_string()]);
        let got = extract_absolute_paths("cd E:\\AI && dir C:\\Windows\\System32");
        assert_eq!(
            got,
            vec![r"E:\AI".to_string(), r"C:\Windows\System32".to_string()]
        );
        let got = extract_absolute_paths("notepad \"D:\\notes\\a.txt\"");
        assert_eq!(got, vec![r"D:\notes\a.txt".to_string()]);

        // UNC 路径（历史缺陷：只认盘符时 UNC 静默绕过读门）
        let got = extract_absolute_paths("type \\\\nas\\\\data\\\\x.txt");
        assert_eq!(got.len(), 1, "{got:?}");
        assert!(got[0].starts_with("\\\\nas"), "{got:?}");
    }

    /// GUI 输入注入的沙箱闸门：read-only / workspace-write 拒绝,
    /// danger-full-access 放行(注入输入=任意系统操作,可绕过文件边界)。
    #[test]
    fn gui_input_gated_by_sandbox() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().join("workspace");
        std::fs::create_dir_all(&ws).unwrap();
        for mode in [
            crate::exec::SandboxMode::ReadOnly,
            crate::exec::SandboxMode::WorkspaceWrite,
        ] {
            let reg = ToolRegistry::new(ws.clone())
                .with_workspace_root(Some(ws.clone()))
                .with_sandbox(Some(crate::exec::acl::WindowsAclSandbox::new(
                    mode, ws.clone(),
                )));
            for (name, args) in [
                ("mouse_click", json!({"x": 1, "y": 1})),
                ("mouse_move", json!({"x": 1, "y": 1})),
                ("mouse_drag", json!({"from_x":1,"from_y":1,"to_x":2,"to_y":2})),
                ("mouse_scroll", json!({"direction":"down"})),
                ("key_type", json!({"text": "hi"})),
                ("key_press", json!({"combo": "enter"})),
            ] {
                let out = reg.dispatch(name, &args);
                assert!(
                    !out.ok,
                    "{name} 在 {mode:?} 沙箱下必须被拒绝"
                );
                assert!(out.stderr.contains("禁止 GUI"), "{name}: {}", out.stderr);
            }
        }
        // 全访问:放行(移动到当前鼠标位置,零副作用)
        let reg2 = ToolRegistry::new(ws)
            .with_workspace_root(None);
        let out = reg2.dispatch("mouse_move", &json!({"x": 1, "y": 1}));
        assert!(out.ok, "全访问应放行: {}", out.stderr);
    }

    /// 回归：工作区内 curl 下载不得被写门误拦（URL 被当写目标的历史缺陷，
    /// 真实案例：cd E:\AI\minecraft && curl -L -o vendor\x.js https://unpkg.com/...）。
    #[test]
    fn curl_download_in_workspace_not_blocked() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().join("workspace");
        std::fs::create_dir_all(&ws).unwrap();
        let reg = ToolRegistry::new(ws.clone())
            .with_workspace_root(Some(ws.clone()))
            .with_sandbox(Some(crate::exec::acl::WindowsAclSandbox::new(
                crate::exec::SandboxMode::WorkspaceWrite,
                ws.clone(),
            )));
        let cmd = format!(
            "cd {} && mkdir vendor && curl -L -o vendor{}x.js https://unpkg.com/three@0.160.0/build/three.module.js && echo EXIT=%errorlevel%",
            ws.display(),
            std::path::MAIN_SEPARATOR
        );
        assert!(
            reg.shell_write_gate(&cmd).is_ok(),
            "URL 不是写目标，区内 curl 下载必须放行"
        );
        assert!(reg.shell_read_gate(&cmd).is_ok());
        // 真正的区外写目标仍拦
        assert!(reg
            .shell_write_gate("curl -L -o D:\\evil.js https://a.b/c.js")
            .is_err());
    }

    /// 回归：带空格的系统路径（"C:\Program Files\Google"）在 shell 命令里
    /// 被拆词成 "C:\Program" 后不得误拦（白名单根前缀也放行）。
    #[test]
    fn quoted_system_path_prefix_not_blocked() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().join("workspace");
        std::fs::create_dir_all(&ws).unwrap();
        let reg = ToolRegistry::new(ws.clone())
            .with_workspace_root(Some(ws.clone()))
            .with_sandbox(Some(crate::exec::acl::WindowsAclSandbox::new(
                crate::exec::SandboxMode::WorkspaceWrite,
                ws,
            )));
        // 模拟 for /d %d in ("C:\Program Files\Google\...") 的拆词形态
        assert!(
            reg.shell_read_gate("dir C:\\Program").is_ok(),
            "白名单根前缀（引号拆词残段）应放行"
        );
        assert!(reg.shell_read_gate("type D:\\secret.txt").is_err());
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

#[cfg(test)]
mod diff_result_tests {
    use super::*;

    /// write_file 覆盖：结果携带 diff（-旧行/+新行）；新建：created=true
    /// 且 diff 全为新增行。通过公共 dispatch 走真实路径。
    #[test]
    fn write_file_result_carries_diff() {
        let dir = tempfile::tempdir().unwrap();
        let reg = ToolRegistry::new(dir.path().to_path_buf());

        // 新建
        let out = reg.dispatch(
            "write_file",
            &json!({"path": "a.txt", "content": "line1\nline2\n"}),
        );
        let v: serde_json::Value = serde_json::from_str(&out.value.to_string()).unwrap();
        assert_eq!(v["created"], json!(true));
        let d = v["diff"].as_str().unwrap();
        assert!(d.contains("+line1"), "新建全为增行: {d}");

        // 覆盖（改动一行）
        let out = reg.dispatch(
            "write_file",
            &json!({"path": "a.txt", "content": "line1\nCHANGED\n"}),
        );
        let v: serde_json::Value = serde_json::from_str(&out.value.to_string()).unwrap();
        assert!(v.get("created").is_none());
        let d = v["diff"].as_str().unwrap();
        assert!(d.contains("-line2"));
        assert!(d.contains("+CHANGED"));
        let (del, add) = crate::core::diff::counts(d);
        assert_eq!((del, add), (1, 1));
    }

    /// str_replace_editor：create / str_replace 结果携带片段 diff。
    #[test]
    fn str_replace_result_carries_diff() {
        let dir = tempfile::tempdir().unwrap();
        let reg = ToolRegistry::new(dir.path().to_path_buf());
        reg.dispatch(
            "str_replace_editor",
            &json!({"command": "create", "path": "s.txt", "file_text": "fn a() {\n    1\n}\n"}),
        );
        let out = reg.dispatch(
            "str_replace_editor",
            &json!({
                "command": "str_replace",
                "path": "s.txt",
                "old_str": "    1",
                "new_str": "    2"
            }),
        );
        let v: serde_json::Value = serde_json::from_str(&out.value.to_string()).unwrap();
        assert_eq!(v["replaced"], json!(true));
        let d = v["diff"].as_str().unwrap();
        assert!(d.contains("-    1"));
        assert!(d.contains("+    2"));
    }

    /// glob 通配符匹配。
    #[test]
    fn glob_matching() {
        assert!(glob_match("*.rs", "main.rs"));
        assert!(glob_match("*.rs", "lib.rs"));
        assert!(!glob_match("*.rs", "main.py"));
        assert!(glob_match("test_?.rs", "test_1.rs"));
        assert!(!glob_match("test_?.rs", "test_12.rs"));
        assert!(glob_match("*", "anything"));
        assert!(glob_match("a*c", "abc"));
        assert!(glob_match("a*c", "ac"));
        assert!(!glob_match("a*c", "abx"));
        assert!(glob_match("", ""));
    }

    /// HTML 剥离。
    #[test]
    fn html_stripping() {
        let html = r#"<html><head><script>evil()</script></head><body><h1>Title</h1><p>Para text</p></body></html>"#;
        let text = strip_html_to_text(html);
        assert!(!text.contains("evil"), "script 应被剥: {text}");
        assert!(text.contains("Title"), "{text}");
        assert!(text.contains("Para text"), "{text}");
        assert!(!text.contains("<"), "标签应被剥: {text}");
    }
}
