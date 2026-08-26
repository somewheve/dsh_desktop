# DSH 完整重写规划（Rust 原生）

> 目标：把 deepseek-ai/dsh（195 个 npm 包、约 20 万行 TS/JS）**完整重写为 Rust 原生实现**，
> 内嵌进 dsh-desktop 桌面终端。不是桥接、不是调用 Node，是真正用 Rust 重写全部能力域。

## 0 结论先行

- **范围**：DSH 全部 10 大能力域（见 §2），按 DSH 开源实现（github.com/deepseek-ai/deepseek-harness）
  的语义逐一重写，事件词汇表、持久化格式、协议信封保持一致。
- **排除**（重写范畴外，需与用户确认）：
  1. cordis 插件系统：DSH 用 JS 生态的 cordis 做插件宿主。Rust 无法加载 JS 插件，
     重写为 Rust 原生插件接口（trait + 动态加载），语义对齐（loader/group/事件）。
  2. dsh-web-frontend / dsh-client-ui-*（React 前端 32 包）：UI 层用 egui 原生重写
     对应功能（会话/对话/工具调用/设置/插件/目标/计划/子代理/jobs/技能面板），不移植 React。
  3. dsh-session-telemetry-otel（OpenTelemetry 上报）：可裁剪，默认关闭。
- **策略**：按能力域分里程碑，每个里程碑"语义对齐 + 测试 + 可运行"，不追求 1:1 文件结构。

## 1 目标架构（Rust 模块树）

    src/
    ├── main.rs / app.rs          # 桌面终端外壳（egui，现代化 UI）
    ├── core/                     # 【引擎核心】= dsh-agent + dsh-session + dsh-llm + dsh-tools
    │   ├── agent.rs              # turn/step 驱动、inbox、dispatch  （已实现核心）
    │   ├── session.rs            # 会话模型 + 事件词汇表             （已实现核心）
    │   ├── llm.rs                # OpenAI 兼容流式客户端 + retry     （已实现核心）
    │   ├── tools.rs              # 工具注册表 + 内置工具             （已实现核心）
    │   ├── storage.rs            # JSONL 持久化                     （已实现核心）
    │   └── settings.rs           # API key/model/proxy             （已实现核心）
    ├── engine/                   # 【扩展引擎】完整能力域
    │   ├── goal.rs               # goal 生命周期（dsh-goal / dsh-command-goal）
    │   ├── plan.rs               # plan mode（dsh-plan-mode）
    │   ├── subagent.rs           # 子代理（dsh-subagent*，in-process 实现）
    │   ├── workflow.rs           # workflow 编排（dsh-workflow*）
    │   ├── schedule.rs           # 定时任务（dsh-schedule）
    │   ├── compaction.rs         # 上下文压缩（dsh-compaction*）
    │   ├── skill.rs              # 技能系统（dsh-skill*）
    │   ├── jobs.rs               # 后台任务（dsh-jobs*）
    │   ├── feedback.rs           # 消息反馈（dsh-message-feedback）
    │   ├── timeout.rs            # 调用超时策略（dsh-timeout）
    │   └── title.rs              # 会话标题（dsh-session-title*）
    ├── exec/                     # 【执行环境】= dsh-bash/pwsh/sandbox/subprocess/terminal
    │   ├── shell.rs              # bash/pwsh 执行（复用 portable-pty）
    │   ├── sandbox.rs            # 沙箱策略（Windows ACL / landlock 语义）
    │   ├── subprocess.rs         # 子进程管理（dsh-subprocess*）
    │   └── code_runtime.rs       # 代码运行（dsh-code-runtime*）
    ├── state/                    # 【状态与持久化】= dsh-settings/credentials/storage/spill/fs
    │   ├── settings.rs           # 设置命名空间（dsh-settings*，多命名空间）
    │   ├── credentials.rs        # 凭据管理（dsh-credentials*）
    │   ├── storage.rs            # 通用存储（dsh-storage-json / atomic-write）
    │   ├── spill.rs              # 断线容灾（dsh-spill*）
    │   └── fs.rs                 # 文件系统抽象（dsh-fs*，observation-policy）
    ├── service/                  # 【服务层】= dsh-host-webserver + api-gateway + typert
    │   ├── bridge.rs             # xitca-web 本地服务（已实现：/api/sessions 等）
    │   ├── rpc.rs                # Typert RPC 信封（对齐 dsh-typert-protocol）
    │   └── events.rs             # 事件流推送（SSE/WS，对齐 events.mux/host）
    ├── tools/                    # 【工具全集】= dsh-tool-*（20 个）
    │   ├── bash.rs / pwsh.rs     # 命令执行
    │   ├── fs.rs / fs_search.rs  # 文件读写/搜索
    │   ├── todo.rs               # 任务清单
    │   ├── web.rs                # 网页抓取
    │   ├── ralph.rs              # ralph 循环
    │   ├── ask_user.rs           # 用户提问
    │   ├── str_replace.rs        # 文本编辑
    │   ├── goal.rs / jobs.rs / skill.rs / workflow.rs  # 调用引擎能力
    │   └── subagent*.rs          # 子代理工具
    ├── ui/                       # 【原生 UI】= dsh-client-ui-* 的功能重写
    │   ├── chat_tab.rs           # 会话/对话（已实现）
    │   ├── terminal_tab.rs       # 终端（已实现）
    │   ├── plugins_tab.rs        # 插件管理（已实现）
    │   ├── settings_tab.rs       # 设置（已实现）
    │   ├── goal_panel.rs         # 目标面板
    │   ├── plan_panel.rs         # 计划面板
    │   ├── jobs_panel.rs         # 任务面板
    │   ├── skill_panel.rs        # 技能面板
    │   └── subagent_panel.rs     # 子代理面板
    ├── plugin/                   # 【插件系统】= cordis 语义的 Rust 版
    │   ├── registry.rs           # 插件注册表（loader）
    │   ├── group.rs              # 插件组
    │   └── events.rs             # 插件事件总线
    └── bridge.rs                 # xitca-web 访问交互层（已实现）

## 2 DSH 能力域 → 重写映射（195 包归并为 10 域）

| 能力域 | DSH 包 | Rust 模块 | 状态 |
|---|---|---|---|
| 1. Agent 引擎 | dsh-agent, dsh-agent-loop, dsh-agent-instructions, dsh-agent-presets, dsh-agent-default-model, dsh-agent-tool-presentation | core/agent.rs | 核心已实现（turn/step 待扩展 inbox/dispatch 全语义） |
| 2. 会话系统 | dsh-session(16 包): persistence/projection/query/reference/stats/title/checkpoint/log-export | core/session.rs + engine/title.rs | 核心已实现；projection/query/title-llm 待补 |
| 3. LLM | dsh-llm, dsh-llm-deepseek, dsh-llm-pi-ai, dsh-llm-retry, dsh-token-meter | core/llm.rs | 核心已实现；retry 策略、多 provider、token 计量待补 |
| 4. 工具系统 | dsh-tools + dsh-tool-*(20) | core/tools.rs + tools/ | bash/fs/todo 已实现；17 个待补 |
| 5. 执行环境 | dsh-bash-local/sandbox, dsh-pwsh-*, dsh-sandbox*, dsh-shell*, dsh-subprocess*, dsh-terminal*, dsh-code-runtime* | exec/ | 待实现（sandbox 用 Windows ACL 语义） |
| 6. 编排 | dsh-goal, dsh-plan-mode, dsh-workflow, dsh-schedule, dsh-compaction*, dsh-output-retention, dsh-timeout | engine/ | 待实现 |
| 7. 子代理 | dsh-subagent(4) | engine/subagent.rs | 待实现（in-process driver） |
| 8. 状态/存储 | dsh-settings*, dsh-credentials*, dsh-storage*, dsh-spill*, dsh-fs*, dsh-atomic-write, dsh-persona | state/ | settings/credentials 已实现；storage 域/spill 待补 |
| 9. 服务层 | dsh-host-webserver, dsh-host-apiproxy, dsh-api-gateway, dsh-typert-*, dsh-jobs* | service/ + bridge.rs | xitca-web 桥已实现；rpc 信封/事件流待补 |
| 10. 技能/反馈 | dsh-skill*, dsh-message-feedback, dsh-user-approval, dsh-user-questions, dsh-permission-presets | engine/skill.rs + service/ | 待实现 |

**排除域**（不重写，原因见 §0）：cordis(6)、dsh-web-frontend + dsh-client-ui-*(32)、dsh-client-*(8)、dsh-web(3)、dsh-web-app、cosmokit、schemastery、node-addon-landlock-run、dsh-host-directory-picker*(4)。

## 3 里程碑（每个里程碑 = 可运行 + 测试全绿）

| 里程碑 | 内容 | 验收 |
|---|---|---|
| M0 | 桌面终端 + 引擎核心闭环 | 终端/会话/LLM 流式/工具/持久化，37 测试全绿（已完成） |
| M1 | 工具全集（17 个）+ 事件词汇表补全 | 20 工具全部可调用；KNOWN_SESSION_EVENT_TYPES 全对齐 |
| M2 | 执行环境（sandbox/subprocess/code-runtime） | Windows ACL 沙箱可验证 |
| M3 | 编排（goal/plan/workflow/schedule/compaction/timeout） | goal 生命周期 + plan mode + workflow 跑通 |
| M4 | 子代理 + 技能 + 反馈/审批 | subagent 可 fork、skill 可加载、ask-user 可交互 |
| M5 | 服务层完整（rpc 信封/事件流/目录选择器） | 桥 API 与 DSH 前端协议兼容 |
| M6 | 插件系统（Rust 版 cordis） | 插件可注册/分组/启停 |
| M7 | UI 面板全集（goal/plan/jobs/skill/subagent） | 现代化 UI 完整 |
| M8 | 集成测试 + 文档 + 发布 | 全链路 E2E + release 构建 |
| M9 | 工作区（workspace）+ 布局重构 | 目录规范化、会话绑定 cwd、工具/终端联动、弹幕/输入比例重设计 |
| M10 | Agent 预设（标准/PTC/极简/创造） | 对齐 DSH agent-presets：模式选择 UI、工具白名单、run_code 组合执行、persona 注入、会话级持久化 |
| M11 | 全量代码审查与修复 | 3 路并行审查：状态同步（二次发送）、错误路径复位、真取消、消息裁剪孤儿、shell 超时、沙箱穿越、EOF 截断、事件重绘、TCP 探测缓存等 25+ 项修复 |

## 4 测试策略

- 每个模块单元测试；回归必须在旧代码上 FAIL 再改。
- 集成测试：真实 PTY、真实 DeepSeek API（读 ~/.dsh 凭据）、临时 DSH_HOME 隔离。
- 协议测试：事件信封 JSON 形状与 DSH KNOWN_SESSION_EVENT_TYPES 对齐断言。
- 工具测试：bash/fs 在临时目录执行（不碰真实文件）。

## 5 风险与取舍

- cordis 插件无法加载 JS → Rust 插件 trait + 配置驱动，语义对齐 loader/group。
- React 前端不移植 → egui 原生面板，功能等价（会话/对话/目标/计划/jobs/技能/设置/插件）。
- otel 上报不实现 → 默认关闭，接口预留。
- Windows 沙箱：DSH 用 landlock（Linux）+ Windows ACL；Rust 侧用 windows-sys ACL + 命令白名单。
- 完整重写预估：M1-M8 为连续迭代，每个里程碑 1-2 轮对话内可验证。

## 6 当前进度快照

- M0 全部完成（37 测试全绿、release 构建、E2E 真实 LLM 回复验证）
- 已实现：core/{agent,session,llm,tools,storage,settings}、bridge(xitca-web)、ui/{chat,terminal,plugins,settings,theme,status_bar}
- 待补：agent inbox/dispatch 全语义、projection/query、retry、17 工具、exec/、engine/、plugin/
