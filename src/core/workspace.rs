//! 工作区：目录规范化 + 会话绑定 cwd（对齐 dsh-workspace 语义）。
//!
//! - 工作区路径用 canonicalize 规范化（realpathNormalize）：尾斜杠/.. /符号链接解析
//! - 工作区必须指向已存在目录（create 的 reject 路径）
//! - 会话创建时可绑定 cwd（session.create 的 workspaceId/cwd 二选一语义）

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// 工作区记录（对齐 workspaceRecord）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Workspace {
    pub id: String,
    /// 规范化绝对路径
    pub root: PathBuf,
    pub created_at: f64,
    pub updated_at: f64,
    /// 绑定的会话 id 列表（insertSessionBefore 语义简化版）
    pub sessions: Vec<String>,
}

/// 工作区管理器。
#[derive(Debug, Default)]
pub struct WorkspaceManager {
    workspaces: HashMap<String, Workspace>,
}

impl WorkspaceManager {
    /// Windows 的 std::fs::canonicalize 返回 verbatim 形式（`\\?\C:\...`）：
    /// 对普通路径去掉该前缀，展示/持久化/身份 key 都用干净的 `C:\...` 形式。
    pub fn strip_unc_prefix(p: &Path) -> PathBuf {
        let s = p.to_string_lossy();
        if let Some(rest) = s.strip_prefix(r"\\?\") {
            PathBuf::from(rest)
        } else {
            p.to_path_buf()
        }
    }

    /// 规范化目录路径：必须存在，解析 .. / 尾斜杠 / 符号链接。
    pub fn canonicalize(path: &Path) -> std::io::Result<PathBuf> {
        let canonical = Self::strip_unc_prefix(&path.canonicalize()?);
        if !canonical.is_dir() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotADirectory,
                format!("workspace must be a directory: {}", canonical.display()),
            ));
        }
        Ok(canonical)
    }

    /// 创建/获取工作区（幂等：同路径返回已有）。
    pub fn open(&mut self, path: &Path) -> Result<Workspace, String> {
        let root = Self::canonicalize(path).map_err(|e| e.to_string())?;
        // 用规范化路径作为唯一身份（对齐 realpath 唯一性）
        let key = root.to_string_lossy().into_owned();
        if let Some(w) = self.workspaces.get(&key) {
            return Ok(w.clone());
        }
        let id = format!("ws-{}", uuidish());
        let now = now_ts();
        let ws = Workspace {
            id,
            root: root.clone(),
            created_at: now,
            updated_at: now,
            sessions: Vec::new(),
        };
        self.workspaces.insert(key, ws.clone());
        Ok(ws)
    }

    /// 获取工作区（按规范化路径，与 open 的 key 一致）。
    pub fn get(&self, path: &Path) -> Option<&Workspace> {
        let root = Self::canonicalize(path).ok()?;
        let key = root.to_string_lossy().into_owned();
        self.workspaces.get(&key)
    }

    /// 绑定会话到工作区。
    pub fn attach_session(&mut self, path: &Path, session_id: &str) -> Result<(), String> {
        let root = Self::canonicalize(path).map_err(|e| e.to_string())?;
        let key = root.to_string_lossy().into_owned();
        if let Some(w) = self.workspaces.get_mut(&key) {
            if !w.sessions.contains(&session_id.to_string()) {
                w.sessions.push(session_id.to_string());
                w.updated_at = now_ts();
            }
            Ok(())
        } else {
            Err(format!("workspace not open: {key}"))
        }
    }

    pub fn list(&self) -> Vec<&Workspace> {
        let mut v: Vec<_> = self.workspaces.values().collect();
        v.sort_by(|a, b| {
            b.updated_at
                .partial_cmp(&a.updated_at)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        v
    }

    pub fn len(&self) -> usize {
        self.workspaces.len()
    }
}

fn now_ts() -> f64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

fn uuidish() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{nanos:x}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn open_and_canonicalize() {
        let dir = tempdir().unwrap();
        let mut mgr = WorkspaceManager::default();
        // 尾斜杠 + .. 应规范化
        let messy = dir.path().join("sub/../");
        std::fs::create_dir_all(dir.path().join("sub")).unwrap();
        let ws = mgr.open(&messy).unwrap();
        // Windows canonicalize 返回 \\?\ 前缀，strip 后应与工作区 root 一致
        assert_eq!(
            ws.root,
            WorkspaceManager::strip_unc_prefix(&dir.path().canonicalize().unwrap())
        );
        assert!(
            !ws.root.to_string_lossy().starts_with(r"\\?\"),
            "root 不应带 UNC 前缀"
        );
        assert_eq!(mgr.len(), 1);
        // 同路径幂等
        let ws2 = mgr.open(dir.path()).unwrap();
        assert_eq!(ws.id, ws2.id);
    }

    #[test]
    fn nonexistent_rejected() {
        let mut mgr = WorkspaceManager::default();
        assert!(mgr.open(Path::new("C:/definitely/not/exist/xyz")).is_err());
    }

    #[test]
    fn attach_session() {
        let dir = tempdir().unwrap();
        let mut mgr = WorkspaceManager::default();
        mgr.open(dir.path()).unwrap();
        mgr.attach_session(dir.path(), "s-1").unwrap();
        let ws = mgr.get(dir.path()).unwrap();
        assert!(ws.sessions.contains(&"s-1".to_string()));
    }
}
