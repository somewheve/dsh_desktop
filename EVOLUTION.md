# dsh-desktop 自进化（Self-Evolution）架构设计

> 目标：让 dsh-desktop 的 AI agent 能**安全地修改自身源码并从中进化**——
> 提出改进 → 改代码 → 验证 → 保留或回滚 → 沉淀教训，形成闭环。
> 本文档是 EVOLUTION 域的架构设计（铁律 #7：先架构后代码）。

## 0 一句话

agent 已具备"手"（bash / write_file / str_replace_editor / read_file / fs_search / cargo 全链路），
本项目已具备"钳制"（审批 / 沙箱 / workspace 边界 / lessons 教训库）。
自进化 = 在这两者之间加一条**受控的进化流水线**：提案 → 分支 → 修改 → 验证门 → 提交/回滚 → 评估记录。

## 1 设计原则（为什么这么设计）

| 原则 | 落地 |
|---|---|
| 可逆优先 | 每次进化先开 git 分支，失败一键丢弃；主分支只接受验证门全绿 |
| 验证是门槛不是建议 | cargo fmt/check/test 全绿是"保留"的硬前提（铁律 #1 内化到流水线） |
| 进化也要审批 | 改核心引擎（src/core、src/engine）前弹审批卡片；改 agentic.md 铁律本身一律拒绝 |
| 失败是养料 | 每次进化的失败原因进 lessons 教训库 + 进化日志，下一次直跑修正版 |
| 防失控 | 单轮进化步数上限、验证超时、只允许在工作区 git 仓库内进化、禁止删测试 |
| 记录即资产 | 进化历史落 `<data_dir>/evolution.jsonl`，可回放、可统计、可回滚到任意代 |

## 2 进化循环（EvolveLoop）状态机

```
Idle ──propose──▶ Reviewing ──approve──▶ Branching ──▶ Mutating ──▶ Verifying
 ▲                    │  reject             │              │            │ pass
 │                    ▼                     ▼              ▼            ▼
 └────────◀──────── Done ◀─── Committing ◀─┴─────────── RollingBack ◀─┤ fail
     (提交/记录/回主分支)                      (丢弃分支/回滚)
```

状态定义（`src/engine/evolve.rs`，`EvolvePhase`）：

```rust
pub enum EvolvePhase {
    Idle,        // 无进行中进化
    Reviewing,   // 提案待审批（用户确认改动范围）
    Branching,   // git 分支创建
    Mutating,    // agent 在改代码（复用现有工具）
    Verifying,   // 验证门：cargo fmt --check → cargo check --all-targets → cargo test
    RollingBack, // 验证失败：git 丢弃分支，回到基础 commit
    Committing,  // 验证通过：合并回主分支 + tag 记录代数
    Done,        // 完成（保留/回滚已定，结果已记录）
}
```

## 3 核心数据结构

```rust
/// 一次进化提案（agent 或用户发起）。
pub struct EvolutionProposal {
    pub id: String,               // evo-<ts>-<rand>
    pub objective: String,        // 进化目标（一句话）
    pub scope: EvolveScope,       // 改动范围描述（文件/模块/工具）
    pub base_commit: String,      // 起点 commit（回滚锚）
    pub branch: String,           // evolve/<id>
    pub created_at: f64,
}

/// 一次进化的完整记录（evolution.jsonl 一行）。
pub struct EvolutionRecord {
    pub id: String,
    pub objective: String,
    pub base_commit: String,
    pub files_changed: Vec<String>,
    pub verify: VerifyReport,     // fmt/check/test 结果
    pub outcome: EvolveOutcome,   // Kept | RolledBack | Rejected
    pub score: i32,               // 评估分（见 §5）
    pub note: String,             // 总结
    pub created_at: f64,
}
```

## 4 流水线各阶段与工具接入

| 阶段 | 谁执行 | 落地方式 |
|---|---|---|
| propose | agent | 新工具 `evolve_propose(objective, scope)` → 写提案 + 发 `EvolveProposalReady` 事件 |
| review | 用户 | 复用 `ApprovalRegistry`：UI 弹"进化确认"卡片（目标/范围/文件清单），A 允许 / B 拒绝 |
| branch | 引擎 | 执行 `git rev-parse HEAD` 记 base → `git checkout -b evolve/<id>` |
| mutate | agent | **复用现有工具**（str_replace_editor / write_file / bash / read_file），不改工具层 |
| verify | 引擎 | 新增工具 `evolve_verify()`：顺序跑 fmt --check / check --all-targets / test，聚合报告 |
| decide | 引擎+agent | 全绿 → `evolve_commit(message)` 合并回主分支 + `git tag evo-<n>`；失败 → `evolve_rollback()` 丢弃分支 |
| learn | 引擎 | 失败教训自动进 lessons 库；`evolve_status()` 查询当前状态/历史 |

新增工具（进 `ToolRegistry::dispatch` 的 match，命名空间 `evolve_*`，共 5 个）：
`evolve_propose` / `evolve_verify` / `evolve_commit` / `evolve_rollback` / `evolve_status`。

## 5 评估分（score）规则

- 基础 0 分。
- +10：验证门全绿；+5：测试数量增加（新增了测试）；−5：测试数量减少。
- +5：教训库新增/更新了一条修复；−5：触发了已知教训的相同失败（说明没学到）。
- −15：回滚（改动无效，但记录本身有价值）。
- score ≥ 10 的进化自动在会话事件里打 `EVOLUTION_SUCCESS`，供 UI 面板展示进化史。

## 6 安全护栏（防失控清单）

1. **只允许在工作区 git 仓库内进化**：无 `.git` 或不在 workspace_root 内 → `evolve_propose` 拒绝。
2. **铁律不可改**：diff 涉及 `agentic.md`、`EVOLUTION.md` 的修改一律拒绝（保护约束自身）。
3. **禁删测试**：diff 中 `src/**` 测试模块或 `tests/` 只增不减（退化检测）。
4. **验证超时**：单次 `evolve_verify` 超时（默认 5 分钟）→ 按失败处理，回滚。
5. **单轮步数上限**：一次进化提案内 mutate+verify 循环 ≤ 3 轮（防死循环改代码）。
6. **并发互斥**：EvolveLoop 全局单例，进行中不允许第二个 propose（`Idle` 才接受）。
7. **审批必达**：核心引擎（src/core、src/engine）改动必须用户点"允许"才开分支；拒绝 → 直接 Done(Rejected)。
8. **提交前状态检查**：工作区有未提交杂物（如 cargo 输出文件）→ `evolve_commit` 前先确认只提交本次 diff 涉及文件。

## 7 模块落点

```
src/engine/evolve.rs        # EvolveLoop 状态机 + 提案/记录 + 评估分
src/engine/mod.rs           # pub mod evolve; 导出类型
src/core/agent.rs           # DshEngine 持有 EvolveLoop；EvolveEvent 接入 EngineEvent
src/core/tools.rs           # dispatch match 新增 evolve_* 5 个工具
src/ui/evolve_panel.rs      # UI 面板：当前进化状态 + 历史记录 + 回滚按钮（对齐其他面板风格）
data_dir/evolution.jsonl    # 进化历史持久化（原子写）
```

## 8 与现有机制的关系

- **审批**：复用 `engine::approval::ApprovalRegistry`（写工作区外审批已有；进化审批是它上面的语义层）。
- **沙箱**：mutate 阶段 bash 跑 cargo 走现有沙箱模式；验证门要求 danger 模式（否则 cargo 写 target/ 被拒，报错清晰）。
- **lessons**：验证失败的工具调用自动记教训（现有机制），进化层不重复造轮子。
- **计划/目标**：agent 可在 plan 里编排多次进化（目标 → 计划 → 多次进化 → 最终验证），进化是执行单元之一。

## 9 里程碑

| 步 | 内容 | 验收 |
|---|---|---|
| E0 | evolve.rs 骨架：状态机 + propose/verify/commit/rollback/status 工具 + 审批钩子 | 单测：假仓库验证门全绿保留、失败回滚、铁律保护拒绝 |
| E1 | evolution.jsonl 持久化 + 评估分 + lessons 联动 | 单测：记录往返、评分规则 |
| E2 | UI 面板（状态 + 历史 + 回滚） | ui_layout 测试 |
| E3 | 真实进化演练：让 agent 自己修一个已知 bug（回归测试先 FAIL） | E2E：bug 被修、验证门通过、记录落盘 |
