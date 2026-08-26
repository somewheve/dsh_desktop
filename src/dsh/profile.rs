//! profile 读写：package.json、cordis.patch.yml、dependencies。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use log::{debug, warn};
use serde::{Deserialize, Serialize};

/// profile package.json 的 DSH 相关字段。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ProfileManifest {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub private: Option<bool>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub dependencies: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dsh: Option<DshSection>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct DshSection {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile: Option<DshProfile>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct DshProfile {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub bundles: Vec<String>,
}

impl ProfileManifest {
    pub fn read(dir: &Path) -> Result<Self> {
        let path = dir.join("package.json");
        let text =
            std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        let m: Self =
            serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
        Ok(m)
    }

    pub fn write(&self, dir: &Path) -> Result<()> {
        let path = dir.join("package.json");
        let text = serde_json::to_string_pretty(self)?;
        std::fs::write(&path, text)?;
        debug!("wrote {}", path.display());
        Ok(())
    }

    /// 已安装插件名（dependencies 的键，去掉 @scope 前缀语义保留原名）。
    pub fn installed_plugins(&self) -> Vec<String> {
        self.dependencies.keys().cloned().collect()
    }
}

/// cordis.patch.yml 的一个 patch entry（loader patch 条目）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PatchEntry {
    /// 简单禁用条目
    Disable { id: String, disabled: bool },
    /// 其他形态（保留原样）
    Other(serde_yaml::Value),
}

impl PatchEntry {
    pub fn id(&self) -> Option<&str> {
        match self {
            PatchEntry::Disable { id, .. } => Some(id.as_str()),
            PatchEntry::Other(v) => v.get("id").and_then(|x| x.as_str()),
        }
    }

    pub fn is_disabled(&self) -> bool {
        match self {
            PatchEntry::Disable { disabled, .. } => *disabled,
            PatchEntry::Other(v) => v.get("disabled").and_then(|x| x.as_bool()).unwrap_or(false),
        }
    }
}

/// 读取 cordis.patch.yml（顶层是 YAML 数组）。
pub fn read_patch(dir: &Path) -> Result<Vec<PatchEntry>> {
    let path = dir.join("cordis.patch.yml");
    if !path.is_file() {
        return Ok(Vec::new());
    }
    let text = std::fs::read_to_string(&path)?;
    if text.trim().is_empty() {
        return Ok(Vec::new());
    }
    let v: serde_yaml::Value =
        serde_yaml::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
    let arr = match v {
        serde_yaml::Value::Sequence(seq) => seq,
        _ => return Ok(Vec::new()),
    };
    let mut out = Vec::new();
    for item in arr {
        if let Some(id) = item.get("id").and_then(|x| x.as_str()) {
            let disabled = item
                .get("disabled")
                .and_then(|x| x.as_bool())
                .unwrap_or(false);
            out.push(PatchEntry::Disable {
                id: id.to_string(),
                disabled,
            });
        } else {
            out.push(PatchEntry::Other(item));
        }
    }
    Ok(out)
}

/// 设置某 entry 的禁用状态（不存在则追加；存在则最小改写）。
pub fn set_patch_disabled(dir: &Path, entry_id: &str, disabled: bool) -> Result<()> {
    let mut entries = read_patch(dir)?;
    let mut found = false;
    for e in entries.iter_mut() {
        if e.id() == Some(entry_id) {
            *e = PatchEntry::Disable {
                id: entry_id.to_string(),
                disabled,
            };
            found = true;
            break;
        }
    }
    if !found {
        entries.push(PatchEntry::Disable {
            id: entry_id.to_string(),
            disabled,
        });
    }
    write_patch(dir, &entries)
}

/// 写回 cordis.patch.yml（保留其他形态条目）。
pub fn write_patch(dir: &Path, entries: &[PatchEntry]) -> Result<()> {
    let path = dir.join("cordis.patch.yml");
    let mut seq = Vec::new();
    for e in entries {
        match e {
            PatchEntry::Disable { id, disabled } => {
                seq.push(serde_yaml::Value::Mapping(serde_yaml::Mapping::from_iter(
                    vec![
                        (
                            serde_yaml::Value::String("id".into()),
                            serde_yaml::Value::String(id.clone()),
                        ),
                        (
                            serde_yaml::Value::String("disabled".into()),
                            serde_yaml::Value::Bool(*disabled),
                        ),
                    ],
                )));
            }
            PatchEntry::Other(v) => seq.push(v.clone()),
        }
    }
    let doc = serde_yaml::Value::Sequence(seq);
    let text = serde_yaml::to_string(&doc)?;
    std::fs::write(&path, text)?;
    debug!("wrote {} ({} entries)", path.display(), entries.len());
    Ok(())
}

/// profile 目录下的 node_modules 插件包根目录（用于校验是否已安装）。
pub fn installed_package_dir(profile_dir: &Path, package: &str) -> Option<PathBuf> {
    let candidate = profile_dir.join("node_modules").join(package);
    if candidate.is_dir() {
        Some(candidate)
    } else {
        None
    }
}

/// 从 dependencies 中移除插件并写回。
pub fn remove_dependency(profile_dir: &Path, package: &str) -> Result<()> {
    let mut m = ProfileManifest::read(profile_dir)?;
    if m.dependencies.remove(package).is_none() {
        warn!("dependency {package} not found; nothing to remove");
        return Ok(());
    }
    if let Some(dsh) = &mut m.dsh {
        if let Some(profile) = &mut dsh.profile {
            profile.bundles.retain(|b| b != package);
        }
    }
    m.write(profile_dir)
}

/// 追加依赖（保留版本，无版本时用通配 "*"），并写回。
pub fn add_dependency(profile_dir: &Path, package: &str, version: Option<&str>) -> Result<()> {
    let mut m = ProfileManifest::read(profile_dir)?;
    m.dependencies
        .insert(package.to_string(), version.unwrap_or("*").to_string());
    m.write(profile_dir)
}
