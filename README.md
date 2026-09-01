# dsh-desktop

DeepSeek Harness 桌面终端 — **Rust 原生**、高性能、可扩展。

## 功能

- **Rust 原生 DSH 引擎**：agent 闭环（turn/step、LLM 流式、工具调度、JSONL 持久化），
  重写自 DSH 开开实现，不桥接外部 Node 服务。
- **访问交互层用 xitca-web**：本地桥暴露 `/api/sessions`、`/api/sessions/{id}/send`、`/api/settings`
  （随机 token 鉴权，`X-DSH-Token` 头必带——端口本机可读，无鉴权等于开放命令执行）。
- **DSH 集成（可选）**：自动发现 `~/.dsh` 与 profiles；列表 / 导入 / 移除 / 启停插件
  （经由 `dsh plugin` CLI，与官方行为一致）。
- **沙箱（纯 Rust）**：`bash`/`pwsh` 可切到 Windows ACL restricted-token 受限执行
  （read-only / workspace-write，写隔离、特权全删、最小环境块、Job Object 树杀；默认 danger-full-access 直通）。
- **权限审批钩子**：写工作区外弹确认卡片（A/B/C），队列跨会话可见；系统根目录
  （C:\Windows 等）不参与同盘路径自动纠正，越权写不静默放行。
- **模型系统**：DeepSeek V4 系模型（flash / pro / vision-exp + legacy），
  思考深度（关/低/高/最高）输入框工具行就地切换；
  **图片附件走 vision 多模态**（气泡内缩略图，点击放大）。
- **改动可审阅**：`write_file` / `str_replace_editor` 的结果携带 **diff 卡片**
  （红删绿增、持久化、历史回放可见）；助手消息可展开 **💭 思考时间线**。
- **用量可见**：会话标题行显示累计 token 与上下文占用估算（持久化，重启保留）。
- **斜杠命令**：`/plan` `/review` `/fix` `/test` `/continue` 高频任务提示词模板，
  输入 `/` 浮出命令面板。
- **会话管理**：自动命名（首条消息提炼）、**⑂ 分叉**（含工具配对的完整副本）、
  **⬇ 导出 Markdown**、跨会话**全文搜索**（命中片段直跳）。
- **项目记忆**：工作区根 `AGENTS.md` 自动注入系统提示（32KB 截断；用户与 AI 共同维护）。
- **定时任务**：Jobs 卡片内配置（提示词 + 间隔），到期自动在绑定会话发起 AI 回合；
  持久化重启恢复。
- **消息反馈**：助手消息 👍/👎，`feedback/record` 事件持久化。
- **首启引导**：无 API key 自动落到设置页；新会话空态提供可点示例。
- **主题系统**：16 槽语义调色盘，内置 dark / graphite / paper-light / cursor-dark；
  `$DSH_HOME/themes/*.json` 与插件 manifest `theme` 字段可发布主题。
- **插件（cordis 风格 + 核心层增强）**：任意语言子进程插件提供工具，**可覆盖内置工具**
  （如提供真正的 web_search 实现）；`domain` 字段声明领域，扩展页归组展示。
- **UI（简约）**：无框导航 + 分段 Tab + chips 设计语言；会话列表收纳进侧栏；
  计划/任务/子代理/目标为浮动卡片；ZCode 风格输入框；双语（中/英）。
- **性能**：egui GPU 文本、按行 LayoutJob 合并、渲染快照按需重建、无输出不重绘。

## 构建

```bash
cd dsh-desktop
cargo build --release
```

> 直连 crates.io 拉取依赖（无需本地代理；`scripts/crates_proxy.py` 仅为历史沙箱环境保留）。

## 运行

```bash
$env:RUST_LOG="dsh_desktop=debug" ; cargo run --release
```

首次运行在设置页选择模型 / 填写 DeepSeek API key（或从 `~/.dsh` 凭据自动读取）；
输入框工具行可随时切换模型、思考深度、权限与 Agent 模式。

## 主题格式

`$DSH_HOME/themes/<name>.json`（未指定槽位回退 dark）：

```json
{ "name": "my-theme", "bg": "#0f1116", "accent": "#E06060", "user_bubble": "#3355ff" }
```

## 插件清单（核心层增强字段）

```json
{
  "name": "my-search",
  "command": ["node", "plugin.js"],
  "tools": [{ "name": "web_search", "description": "真正的搜索", "parameters": {} }],
  "domain": "search",
  "theme": "themes/search-dark.json"
}
```

同名工具**覆盖**内置实现（web_search 内置是占位）；`domain` 声明领域；
`theme` 随插件发布主题。

## 文档

- [ARCHITECTURE.md](ARCHITECTURE.md) — 架构设计
- [agentic.md](agentic.md) — 项目约束（AI agent 与开发者共同遵守）
