//! 扩展引擎：编排 + 子代理 + 技能 + 反馈（对齐 DSH 各能力域）。

pub mod approval;
pub mod compaction;
pub mod feedback;
pub mod jobs;
pub mod lessons;
pub mod plan;
pub mod plugin;
pub mod schedule;
pub mod skill;
pub mod subagent;
pub mod workflow;

pub use approval::{ApprovalDecision, ApprovalRegistry, ApprovalRequest};
pub use compaction::{plan_compaction, CompactionPlan};
pub use feedback::{FeedbackKind, FeedbackStore};
pub use jobs::{Job, JobManager, JobStatus};
pub use plan::{fold_plan_mode, PlanMode};
pub use plugin::{PluginManager, PluginManifest, PluginStatus};
pub use schedule::{ScheduledTask, Scheduler};
pub use skill::{is_skill_name, Skill, SkillRegistry};
pub use subagent::{run_subagent_turn, SubagentDescriptor, SubagentManager, SubagentStatus};
pub use workflow::{WorkflowManager, WorkflowPhase, WorkflowRun};
