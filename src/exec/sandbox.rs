//! 沙箱策略：模式、升级阶梯、路径裁决（对齐 dsh-sandbox + dsh-sandbox-policy）。

use std::path::{Path, PathBuf};

/// 沙箱模式（对齐 SANDBOX_MODES）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxMode {
    ReadOnly,
    WorkspaceWrite,
    DangerFullAccess,
}

impl SandboxMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            SandboxMode::ReadOnly => "read-only",
            SandboxMode::WorkspaceWrite => "workspace-write",
            SandboxMode::DangerFullAccess => "danger-full-access",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "read-only" => Some(SandboxMode::ReadOnly),
            "workspace-write" => Some(SandboxMode::WorkspaceWrite),
            "danger-full-access" => Some(SandboxMode::DangerFullAccess),
            _ => None,
        }
    }

    /// 严格更宽升级阶梯（对齐 WIDER_MODES）。
    pub fn wider_than(&self, other: &SandboxMode) -> bool {
        match (self, other) {
            (SandboxMode::WorkspaceWrite, SandboxMode::ReadOnly) => true,
            (SandboxMode::DangerFullAccess, SandboxMode::ReadOnly) => true,
            (SandboxMode::DangerFullAccess, SandboxMode::WorkspaceWrite) => true,
            _ => false,
        }
    }
}

/// 沙箱裁决结果（对齐 sandboxDenialMarker 语义）。
#[derive(Debug, Clone)]
pub enum SandboxResult {
    Allowed,
    /// 拒绝（带模型可见的提示）
    Denied {
        hint: String,
    },
}

/// 沙箱策略：模式 + 工作区根。
#[derive(Debug, Clone)]
pub struct SandboxPolicy {
    pub mode: SandboxMode,
    pub workspace_root: PathBuf,
    pub workspace_write: bool,
}

impl SandboxPolicy {
    pub fn new(mode: SandboxMode, workspace_root: PathBuf) -> Self {
        let workspace_write = match mode {
            SandboxMode::ReadOnly => false,
            SandboxMode::WorkspaceWrite => true,
            SandboxMode::DangerFullAccess => true,
        };
        Self {
            mode,
            workspace_root,
            workspace_write,
        }
    }

    /// 解析路径是否可写。
    pub fn can_write(&self, path: &Path) -> SandboxResult {
        match self.mode {
            SandboxMode::DangerFullAccess => SandboxResult::Allowed,
            SandboxMode::WorkspaceWrite => {
                if path_is_within(path, &self.workspace_root) {
                    SandboxResult::Allowed
                } else {
                    SandboxResult::Denied {
                        hint: format!(
                            "sandbox: write denied under workspace-write mode: {} 不在工作区内（{}）。如需写工作区外，请用 sandbox_permissions + justification 升级。",
                            path.display(),
                            self.workspace_root.display()
                        ),
                    }
                }
            }
            SandboxMode::ReadOnly => SandboxResult::Denied {
                hint: "sandbox: write denied under read-only mode。如需写文件，请升级到 workspace-write（需 justification）。".into(),
            },
        }
    }

    /// 请求升级（fail-closed：必须有 justification）。
    pub fn request_escalation(
        &self,
        from: SandboxMode,
        to: SandboxMode,
        justification: Option<&str>,
    ) -> Result<SandboxMode, String> {
        if !to.wider_than(&from) {
            return Err(format!("invalid escalation: {to:?} 不比 {from:?} 更宽"));
        }
        let just = justification.map(str::trim).filter(|s| !s.is_empty());
        match just {
            Some(_) => Ok(to),
            None => Err("invalid escalation: sandbox_permissions requires a justification".into()),
        }
    }
}

fn path_is_within(path: &Path, root: &Path) -> bool {
    // 防御 .. 穿越：先找"最近存在的祖先" canonicalize，再拼接剩余组件后与规范化根比较
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn mode_ladder() {
        assert!(SandboxMode::WorkspaceWrite.wider_than(&SandboxMode::ReadOnly));
        assert!(SandboxMode::DangerFullAccess.wider_than(&SandboxMode::WorkspaceWrite));
        assert!(!SandboxMode::ReadOnly.wider_than(&SandboxMode::WorkspaceWrite));
    }

    #[test]
    fn read_only_denies_write() {
        let root = tempdir().unwrap();
        let policy = SandboxPolicy::new(SandboxMode::ReadOnly, root.path().to_path_buf());
        let f = root.path().join("x.txt");
        assert!(matches!(policy.can_write(&f), SandboxResult::Denied { .. }));
    }

    #[test]
    fn workspace_write_allows_inside() {
        let root = tempdir().unwrap();
        let policy = SandboxPolicy::new(SandboxMode::WorkspaceWrite, root.path().to_path_buf());
        let f = root.path().join("x.txt");
        assert!(matches!(policy.can_write(&f), SandboxResult::Allowed));
    }

    #[test]
    fn dotdot_traversal_rejected() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("ws");
        std::fs::create_dir_all(&root).unwrap();
        // root 外已存在的文件
        let outside = dir.path().join("escape.txt");
        std::fs::write(&outside, "x").unwrap();
        let policy = SandboxPolicy::new(SandboxMode::WorkspaceWrite, root.clone());
        // ws\..\escape.txt 解析到 root 外 → 必须拒绝
        let evil = root.join("..").join("escape.txt");
        assert!(
            matches!(policy.can_write(&evil), SandboxResult::Denied { .. }),
            ".. 路径穿越应被拒绝"
        );
        // 合法路径仍放行
        let ok = root.join("inner.txt");
        assert!(matches!(policy.can_write(&ok), SandboxResult::Allowed));
    }

    #[test]
    fn escalation_requires_justification() {
        let root = tempdir().unwrap();
        let policy = SandboxPolicy::new(SandboxMode::ReadOnly, root.path().to_path_buf());
        assert!(policy
            .request_escalation(SandboxMode::ReadOnly, SandboxMode::WorkspaceWrite, None)
            .is_err());
        assert!(policy
            .request_escalation(
                SandboxMode::ReadOnly,
                SandboxMode::WorkspaceWrite,
                Some("需要写配置")
            )
            .is_ok());
    }
}
