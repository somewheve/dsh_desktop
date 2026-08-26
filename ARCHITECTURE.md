# dsh-desktop 架构设计

## 0 一句话

为 DeepSeek Harness (DSH) 打造的 **Rust 原生桌面终端**：egui 即时模式 GUI + portable-pty
(ConPTY) 真终端模拟 + **Rust 原生重写的 DSH 核心引擎**（不桥接外部 Node 服务）。
目标：性能快（无 WebView、GPU 文本渲染、按需重绘）、可扩展（模块化分层）、可测试。

## 1 技术选型（为什么）

| 关注点 | 选型 | 理由 |
|---|---|---|
| GUI | eframe/egui 0.36 | 纯 Rust 即时模式，GPU 加速文本，无浏览器包袱，热更新友好 |
| PTY | portable-pty 0.9 (wezterm) | 跨平台（Windows ConPTY / unix pty），API 稳定 |
| 终端模拟 | vt100 0.16 | VT100/ANSI 解析 + 回滚，纯 Rust，无原生依赖 |
| DSH 引擎 | src/core/*（Rust 原生） | 重写自 DSH 开源实现：agent 循环/LLM/工具/会话/存储 |
| LLM | reqwest + rustls | DeepSeek 官方 API（OpenAI 兼容、流式 SSE） |
| 访问交互 | xitca-web 0.8（本地桥） | 与 ada-rs 同款框架，暴露 /api/session/* |
| 配置 | serde + serde_yaml + serde_json | DSH profile 全是 JSON/YAML，天然契合 |
| 日志 | log + env_logger | 轻量、可定位问题（约束 #9） |
| 错误 | anyhow + thiserror | 快速失败 + 类型化错误 |

## 2 分层与目录

```
dsh-desktop/
├── Cargo.toml
├── agentic.md                 # 项目约束（本文件是核心约束清单）
├── ARCHITECTURE.md            # 本文件
├── README.md                  # 使用说明
├── scripts/crates_proxy.py    # 开发期 crates.io HTTP 代理（绕过沙箱 schannel）
├── .cargo/config.toml         # 构建期 source 替换 → 本地代理
├── src/
│   ├── main.rs                # 入口：eframe 启动、日志初始化
│   ├── app.rs                 # DshDesktopApp：eframe::App 顶层布局与状态
│   ├── config.rs              # AppConfig：DSH 路径/profile/shell/字体/主题
│   ├── core/
│   │   ├── mod.rs             # Rust 原生 DSH 引擎聚合
│   │   ├── agent.rs           # turn/step 驱动循环 + 工具调度（重写自 dsh-agent）
│   │   ├── llm.rs             # DeepSeek API 客户端（重写自 dsh-llm）
│   │   ├── session.rs         # 会话模型 + 事件词汇表（重写自 dsh-session）
│   │   ├── tools.rs           # bash/fs/todo 工具（重写自 dsh-tool-*）
│   │   ├── storage.rs         # JSONL 事件持久化（重写自 dsh-session-persistence-jsonl）
│   │   └── settings.rs        # API key/model/proxy（重写自 dsh-settings/credentials）
│   ├── bridge.rs              # xitca-web 本地桥（访问交互层 /api/*）
│   ├── dsh/
│   │   ├── mod.rs             # DSH 外部集成（profiles/插件管理，可选）
│   │   ├── home.rs            # DSH_HOME 发现、profiles 枚举、web 端口探测
│   │   ├── profile.rs         # profile 读写：package.json、cordis.patch.yml、dependencies
│   │   ├── plugins.rs         # 插件：列表/导入(CLI 或本地目录)/移除/启停
│   │   └── cli.rs             # 定位并执行 dsh CLI（spawn，流式输出）
│   ├── term/
│   │   ├── mod.rs
│   │   ├── session.rs         # PTY 会话：spawn/resize/读写线程/退出
│   │   ├── screen.rs          # vt100 封装：Parser + ScreenSnapshot（脏区）
│   │   ├── input.rs           # egui 事件 → ANSI 转义字节
│   │   └── render.rs          # 屏幕快照 → egui 网格渲染（按行 LayoutJob）
│   ├── exec/
│   │   ├── mod.rs             # 执行环境聚合
│   │   ├── sandbox.rs         # 沙箱策略：模式/升级阶梯/路径裁决（can_write）
│   │   ├── winacl.rs          # 纯 Rust Win32 restricted-token（SID/token/ACL/spawn，windows-sys）
│   │   ├── acl.rs             # 沙箱编排层 WindowsAclSandbox（fail-closed，委托 winacl）
│   │   └── subprocess.rs      # 子进程管理：spawn/超时/输出捕获
│   ├── ui/
│   │   ├── mod.rs
│   │   ├── terminal_tab.rs    # 终端标签页
│   │   ├── plugins_tab.rs     # 插件管理标签页
│   │   └── status_bar.rs      # 状态栏（会话/DSH URL/日志级别）
│   └── util.rs                # 路径/字体加载/日志文件等公共工具
└── tests/
    └── integration.rs         # 端到端：PTY 冒烟、插件导入临时 DSH_HOME
```

## 3 核心数据流

### 3.0 Rust 原生 DSH 引擎（不桥接外部 Node）

```
egui UI（会话/对话/工具调用/设置/插件/终端）
   │  直接调用（Mutex<DshEngine>）
   ▼
DshEngine（src/core/agent.rs）—— 重写自 DSH 开源实现
   ├─ send_message → user/message 事件（JSONL 持久化）
   ├─ turn/start → LLM 流式调用（reqwest + SSE）
   ├─ assistant/chunk* 事件流（UI 实时渲染）
   ├─ tool/call → ToolRegistry.dispatch（bash/fs/todo）
   ├─ tool/result → 回填 LLM 继续（最多 12 步）
   └─ turn/end
   │
   ▼
本地 xitca-web 桥（src/bridge.rs，127.0.0.1:随机端口）
   GET/POST /api/sessions、/api/sessions/{id}/send、/api/settings
   —— 访问交互层，与 ada-rs 同款框架
```

关键点：
- **不依赖外部 Node DSH 服务**：agent 循环、LLM 调用、工具执行、事件持久化全部
  在 Rust 进程内完成（对齐 DSH 开源实现的语义与事件词汇表）。
- 事件词汇表（turn/start、assistant/chunk、tool/call、tool/result…）与 DSH
  KNOWN_SESSION_EVENT_TYPES 对齐，JSONL 日志格式兼容。
- 访问交互层用 xitca-web（用户指定），暴露本地 REST API，UI 直连引擎。

### 3.1 终端会话

```
UI 按键事件 ──egui──▶ input.rs 译码 ──▶ session.write(bytes)
                                        │
PTY (ConPTY) ◀──portable-pty── session.master
        │
        ▼  reader 线程（阻塞读）
   mpsc channel (bytes)
        │
        ▼  UI 帧内 drain
   screen.rs: Parser.process(bytes) ──▶ ScreenSnapshot(脏区)
        │
        ▼
   render.rs: 只重绘脏行（LayoutJob 按颜色 run 合并）→ GPU 文本
```

- reader 线程在输出到达时 `ctx.request_repaint()`，无输出不重绘（性能）。
- 行尺寸变化 → `master.resize(PtySize)` + parser.set_size()。
- 终端渲染按行构造 LayoutJob：同色连续字符合并为一个 run，背景色通过
  TextFormat.background 表达，避免逐 cell 绘制（性能）。

### 3.2 沙箱（Windows ACL restricted-token）

`src/exec/sandbox.rs` 是沙箱策略层（模式/升级阶梯/路径裁决 `can_write`）；
`src/exec/acl.rs` 是编排层（`WindowsAclSandbox`，fail-closed）；
`src/exec/winacl.rs` 是**纯 Rust Win32 实现**（windows-sys），真正落地写隔离：

- 模式三档：`read-only`（无写授予）/ `workspace-write`（工作区可写）/
  `danger-full-access`（直通，不隔离）。
- `bash` / `pwsh` 工具在非 danger 模式下经 `WindowsAclSandbox::run` 走纯 Rust 链路：
  `winacl::grant_dir_write`（SetEntriesAclW + SetNamedSecurityInfoW 给工作区目录授
  capability-SID 写 ACE）→ `winacl::build_restricted_token`（CreateRestrictedToken，
  限制 SID = logon + Everyone + 可选 workspace 能力 SID）→
  `winacl::spawn_restricted`（CreateProcessAsUserW + 匿名管道 + 超时 kill）。**无任何
  Node 依赖**（满足铁律 #4）。
- capability-SID 由工作区路径 sha256 派生（`S-1-4-<a>-<b>`，与官方 workspace-sid
  算法一致）；workspace 目录的写授权在受限 spawn 前幂等完成。
- **fail-closed**：token 构建 / 目录授权 / CreateProcessAsUserW 任一步失败即返回
  Denied，绝不悄悄直通。
- 默认模式 `danger-full-access`（保持既有语义）；`DshEngine::set_sandbox_mode` 可
  切换（设置页 / 会话窗口输入栏的沙箱按钮），`ToolRegistry::with_sandbox` /
  `clone_handle` 沿 agent 链路保持沙箱配置。
- 真实写隔离由两个 E2E 测试验证（#[ignore]，临时目录）：
  `exec::acl::tests::e2e_workspace_write_denies_outside` 与
  `exec::winacl::tests::e2e_workspace_write_isolation`：workspace 内写成功、外被拒。

### 3.3 权限审批钩子（写工作区外需用户确认）

`src/engine/approval.rs` 提供审批注册表（`ApprovalRegistry`，oneshot 通道）；
`DshEngine::resolve_approval` 供 UI 回传；`AgentEvent::ApprovalRequested` 通知 UI。

- **触发**：`ToolRegistry::potential_out_of_workspace(name, args)` 判定写类工具
  （write_file / str_replace_editor create|replace / bash/pwsh 的 `>` 重定向）目标是否
  在工作区外（`workspace_root`）。
- **流程（串行）**：越权 → 引擎 `run_approval` 登记审批 + 发 `ApprovalRequested` → UI
  `chat_tab` 弹确认卡片（A/B/C：允许本次 / 拒绝 / 总是允许）→ 用户点选 →
  `engine.resolve_approval(id, decision)` 回传 → `run_approval` await 到决定：
  允许/总是允许 → 真正 dispatch 执行；拒绝 → 返回"已拒绝，请改在工作区内"给 AI。
  always-allow 记录目标路径，同路径不再问（`should_skip`）。
- **fail-closed**：审批超时（300s 未响应）/ 通道关闭 → 默认拒绝，绝不静默放行。
- 默认 `workspace_root` = 初始工作目录（E:\AI）；未配置则不拦截（无写边界）。
- 单元测试：`engine::approval::tests`（request/resolve/always-allow/超时语义）。

### 3.4 Proxy 配置

- AppConfig 持有 http_proxy / https_proxy / no_proxy 三项。
- 设置标签页编辑并保存到 config.json。
- 应用到三条路径：dsh plugin CLI、dsh web 启动、PTY 终端 shell
  （注入 HTTP_PROXY / HTTPS_PROXY / NO_PROXY 环境变量）。
- 终端会话重启后新环境变量生效（session.rs 的 spawn_with_cfg）。

## 3.5 插件导入

```
PluginsTab ──▶ dsh/plugins.rs
   ├─ 列表：读 profile/package.json (dependencies + dsh.profile.bundles)
   ├─ 导入：
   │    ├─ CLI 路径：dsh plugin --profile <name> add <pkg>   （权威路径，pnpm 转发）
   │    └─ 本地目录：file: 绝对路径交给 CLI（anchorPathSpec 已处理相对路径）
   └─ 启停：写 cordis.patch.yml（patch 层 id 目标 disabled 条目）
```

- 不直接碰 pnpm，全部经由 `dsh plugin` 转发 → 与官方行为一致（bundle 自动 reconcile）。
- cordis.patch.yml 是用户 patch 层：以 loader entry id 为目标追加
  `{ id: <entry>, disabled: true/false }`，保留其余内容（YAML 最小改写）。

## 4 性能设计

1. **按需重绘**：PTY 无输出时 UI 不重绘；reader 线程触发 repaint。
2. **行级 LayoutJob 合并**：一行的连续同色字符合成一个 run，减少 GPU draw call。
3. **脏区**：vt100 Screen 的 changed 状态 → 只提交变化行。
4. **字体**：运行时加载系统等宽字体（Cascadia Mono/Consolas）+ 中文字体（雅黑）回退，
   避免把字体打进二进制（体积）。
5. **无 unsafe、无忙等**：reader 线程阻塞读；UI 帧 drain 非阻塞。

## 5 错误与日志策略

- `anyhow::Result` 冒泡至 UI 层，转成状态栏/面板错误文本（不 panic）。
- env_logger 写 stderr + `%LOCALAPPDATA%/dsh-desktop/logs/dsh-desktop.log`，
  每环节（spawn/resize/read/write/plugin cmd）留关键日志，便于定位。
- 会话退出时记录退出码与 stderr 尾部。

## 6 测试策略

- 单元：input.rs 按键→ANSI 译码表；screen.rs 喂 ANSI 序列断言快照；
  profile.rs 读写 package.json/cordis.patch.yml 往返；plugins.rs 临时 DSH_HOME 导入。
- 集成：tests/integration.rs 用 portable-pty 起 cmd/pwsh 冒烟 echo；
  插件导入在临时 profile 上跑（不碰真实 ~/.dsh）。
- 回归原则：修复必须先能在旧代码上 FAIL 再改（约束 #5）。

## 7 构建环境（本机沙箱特殊处理）

- 沙箱内 schannel TLS 失败（SEC_E_NO_CREDENTIALS），但 Python OpenSSL 可出网。
- 方案：scripts/crates_proxy.py 在 127.0.0.1:<port> 提供：
  - `/index/<path>`  → 转发 https://index.crates.io/<path>（sparse index）
  - `/dl/<name>/<ver>/download` → 转发 https://static.crates.io/crates/<name>/<ver>.crate
  - config.json 的 dl 指向本代理
- 项目 .cargo/config.toml：`[source.crates-io] replace-with = "local-proxy"` +
  `registry = "sparse+http://127.0.0.1:<port>/index/"`。
- 代理用 run_in_background 常驻；构建完成后 kill。