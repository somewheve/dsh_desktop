# dsh-desktop

DeepSeek Harness 桌面终端 — **Rust 原生**、高性能、可扩展。

## 功能

- **真终端模拟**：portable-pty (ConPTY) + vt100，支持 ANSI/VT100 颜色、光标、回滚、resize。
- **Rust 原生 DSH 引擎**：agent 闭环（turn/step、LLM 流式、工具调度、JSONL 持久化），
  重写自 DSH 开源实现，不桥接外部 Node 服务。
- **访问交互层用 xitca-web**：本地桥暴露 `/api/sessions`、`/api/sessions/{id}/send`、`/api/settings`。
- **DSH 集成（可选）**：自动发现 `~/.dsh` 与 profiles；列表 / 导入 / 移除 / 启停插件
  （经由 `dsh plugin` CLI，与官方行为一致）。
- **沙箱（纯 Rust）**：`bash`/`pwsh` 可切到 Windows ACL restricted-token 受限执行
  （read-only / workspace-write，写隔离，fail-closed；默认 danger-full-access 直通）。
  用 `windows-sys` 直接实现 CreateRestrictedToken + CreateProcessAsUserW + ACL，无 Node 依赖。
- **权限审批钩子**：写工作区外（write_file / bash/pwsh 重定向等）会在会话窗口弹确认
  卡片（A/B/C：允许本次 / 拒绝 / 总是允许），串行确认；拒绝则给 AI 说明原因。
- **性能**：egui GPU 文本、按行 LayoutJob 合并、无输出不重绘。
- **插件导入**：支持 npm 包名与本地目录两种导入方式。
- **Proxy 配置**：设置页配置 HTTP/HTTPS 代理，注入 dsh 命令与终端 shell。

## 构建

```bash
cd dsh-desktop
python scripts/crates_proxy.py 8899      # 沙箱构建依赖（开发期代理，另一终端）
cargo build --release
```

> 说明：本仓库开发机沙箱内 schannel TLS 不可用，构建期经本地 HTTP 代理拉取 crates；
> 代理仅开发期需要，运行期 dsh-desktop 不依赖它。

## 运行

```bash
$env:RUST_LOG="dsh_desktop=debug" ; cargo run --release
```

首次运行选择 DSH profile（默认自动选第一个）；终端标签页即 shell；插件标签页管理
当前 profile 的插件。

## 文档

- [ARCHITECTURE.md](ARCHITECTURE.md) — 架构设计
- [agentic.md](agentic.md) — 项目约束（AI agent 与开发者共同遵守）