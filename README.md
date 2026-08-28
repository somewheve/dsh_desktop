# dsh-desktop

DeepSeek Harness 桌面终端 — **Rust 原生**、高性能、可扩展。

## 功能

- **真终端模拟**：portable-pty (ConPTY) + vt100，支持 ANSI/VT100 颜色、光标、回滚、resize。
- **Rust 原生 DSH 引擎**：agent 闭环（turn/step、LLM 流式、工具调度、JSONL 持久化），
  重写自 DSH 开源实现，不桥接外部 Node 服务。
- **访问交互层用 xitca-web**：本地桥暴露 `/api/sessions`、`/api/sessions/{id}/send`、`/api/settings`
  （随机 token 鉴权，`X-DSH-Token` 头必带——端口本机可读，无鉴权等于开放命令执行）。
- **DSH 集成（可选）**：自动发现 `~/.dsh` 与 profiles；列表 / 导入 / 移除 / 启停插件
  （经由 `dsh plugin` CLI，与官方行为一致）。
- **沙箱（纯 Rust）**：`bash`/`pwsh` 可切到 Windows ACL restricted-token 受限执行
  （read-only / workspace-write，写隔离、特权全删、最小环境块、Job Object 树杀；默认 danger-full-access 直通）。
- **权限审批钩子**：写工作区外（write_file / bash 重定向 / run_code 子步 / node_called）
  弹确认卡片（A/B/C），队列跨会话可见；拒绝则给 AI 说明原因。
- **模型系统**：DeepSeek V4 系模型（flash / pro / vision-exp + legacy），
  思考深度（关/低/高/最高，`reasoning_effort` + `thinking.type`）——输入框工具行就地切换，
  下一回合生效。
- **主题系统**：16 槽语义调色盘，内置 dark / graphite / paper-light；
  `$DSH_HOME/themes/*.json` 与插件 manifest `theme` 字段可发布主题，设置页切换。
- **插件（cordis 风格 + 核心层增强）**：任意语言子进程插件提供工具，**可覆盖内置工具**
  （如提供真正的 web_search 实现）——核心层直接增强，不经提示词注入；
  `domain` 字段声明领域，扩展页归组展示。
- **UI（简约）**：无框导航 + 左侧强调条；会话列表收纳进侧栏"会话"下可折叠；
  计划/任务/子代理/目标为浮动卡片（不挤占布局）；ZCode 风格输入框
  （透明多行输入 + 底部工具行：模型/思考/权限/模式 chips + 发送）；
  错误信息在底部状态栏（RUST_LOG 前）查看。
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
