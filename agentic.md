# agentic.md — dsh-desktop 项目约束清单

本文件是 dsh-desktop（DeepSeek Harness 桌面终端）的**关键约束清单**，供 AI agent 与开发者共同遵守。
违反这些约定会导致功能回归、构建环境失效或 DSH 配置损坏。

## 项目一句话

Rust 原生桌面终端 + **Rust 原生 DSH 引擎**：真终端模拟（PTY/ANSI/回滚）+
agent 闭环（LLM/工具/会话/JSONL 持久化，重写自 DSH 开源实现，不桥接外部 Node）+
访问交互层用 xitca-web。GUI 用 eframe/egui（即时模式、GPU 文本），PTY 用 portable-pty。

## 铁律（违反即事故）

1. **代码必须可编译、可测试**：`cargo fmt` → `cargo check` → `cargo test` 全绿才可交付；
   禁止带警告交付。测试必须在旧代码上 FAIL 才算回归依据。
2. **无 unsafe**：全工程安全 Rust（eframe/vt100/portable-pty 自身除外，我们不写 unsafe）。
3. **不碰真实用户数据做测试**：所有涉及 `~/.dsh` 的测试必须用 `DSH_HOME` 指向临时目录，
   严禁在测试里写真实 `C:\Users\somew\.dsh`。
4. **DSH 引擎必须 Rust 原生**：agent 循环/LLM/工具/存储不得桥接外部 Node DSH 服务；
   语义与事件词汇表对齐 DSH 开源实现（KNOWN_SESSION_EVENT_TYPES）。访问交互层用 xitca-web。
4b. **插件操作走 `dsh plugin` CLI**（pnpm 转发 + bundle reconcile），不手工改
   `cordis.patch.yml` 之外的依赖元数据；cordis.patch.yml 只能**最小改写**（保留注释与无关条目）。
4b. **proxy 配置**：AppConfig 的 http/https/no_proxy 必须注入 dsh plugin、dsh web、
   PTY shell 的环境变量；设置标签页保存；终端重启会话后生效。
5. **构建环境**：沙箱 schannel TLS 不可用 → 依赖 `scripts/crates_proxy.py` 本地代理 +
   项目 `.cargo/config.toml` 的 source 替换。代理必须常驻才能构建；构建完成后可 kill。
   代理是开发期工具，不是运行期依赖（运行期 dsh-desktop 不联网依赖）。
6. **每环节留日志**：spawn/resize/read/write/plugin 命令必须打日志；日志要能定位问题。
7. **先架构后代码**：架构变更必须先更新 ARCHITECTURE.md 再改实现；本文件与
   ARCHITECTURE.md/README.md 必须同步。
8. **性能约束**：无输出不重绘；行级 LayoutJob 合并；禁止忙等（reader 阻塞读）。
9. **不擅改上游**：不修改 @deepseek-ai/dsh 及其依赖；确需上游功能先询问用户。

## 目录结构速查

```
src/
├── main.rs            # eframe 入口、日志初始化
├── app.rs             # 顶层布局与状态（DshDesktopApp）
├── config.rs          # AppConfig（DSH 路径/profile/shell/字体/主题）
├── dsh/               # home.rs profile.rs plugins.rs cli.rs（DSH 集成）
├── term/              # session.rs screen.rs input.rs render.rs（终端）
├── ui/                # terminal_tab.rs plugins_tab.rs status_bar.rs
└── util.rs            # 公共工具（字体/日志/路径）
tests/integration.rs   # 端到端冒烟（临时 DSH_HOME）
scripts/crates_proxy.py
```

## 常用命令（必须在 dsh-desktop 目录内）

```bash
$env:RUST_LOG="dsh_desktop=debug" ; cargo run          # 开发运行
cargo fmt && cargo check && cargo test                  # 交付前必跑
cargo build --release                                   # 发布构建
python scripts/crates_proxy.py 8899                     # 手动起代理（构建需要）
```

## 错误处理约定

- 错误统一 `anyhow::Result`，UI 层转状态栏/面板文本；禁止 panic 裸奔。
- 关键路径（PTY spawn、plugin CLI）必须有日志 + 用户可见错误。