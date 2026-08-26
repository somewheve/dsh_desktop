//! 执行环境：shell / sandbox / subprocess（对齐 DSH dsh-sandbox* / dsh-subprocess* / dsh-terminal*）。
//!
//! 沙箱模式三档（对齐 SANDBOX_MODES）：
//! - read-only：只读文件系统（写操作被拒）
//! - workspace-write：工作区内可写
//! - danger-full-access：完全访问
//! 升级阶梯（对齐 WIDER_MODES）：read-only -> workspace-write -> danger-full-access，
//! 升级必须携带 justification 并经审批通道（fail-closed）。

pub mod acl;
pub mod sandbox;
pub mod subprocess;

pub mod winacl;
pub use acl::WindowsAclSandbox;
pub use sandbox::{SandboxMode, SandboxPolicy, SandboxResult};
pub use subprocess::SubprocessManager;
