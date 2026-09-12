//! 插件管理：列表 / 导入 / 移除 / 启停。
//!
//! 设计原则（agentic.md #4）：插件元数据操作全部经由 `dsh plugin` CLI
//! （pnpm 转发 + bundle reconcile），避免与官方行为漂移；cordis.patch.yml
//! 只做最小改写（启停）。

use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender};

use anyhow::Result;
use log::info;

use super::cli::{run_plugin_cmd, PluginCmdResult};
use super::profile::{self, PatchEntry, ProfileManifest};

/// 一个已安装插件的展示信息。
#[derive(Debug, Clone)]
pub struct PluginInfo {
    pub name: String,
    pub version: String,
    /// 是否为 profile bundle（进入 dsh.profile.bundles 层栈）
    pub is_bundle: bool,
    /// 是否被 patch 层禁用
    pub disabled: bool,
    /// node_modules 是否真实存在（bundle 条目为 None：由部署环境解析）
    pub installed: Option<bool>,
}

/// 插件列表（package.json 的 dependencies + dsh.profile.bundles 层栈）。
///
/// 只列 dependencies 会让 web profile（bundles 在 dsh.profile.bundles、
/// dependencies 为空）永远显示"无插件"——插件页像摆设。
/// 因此 bundles 也作为条目列出（is_bundle=true）。
pub fn list_plugins(profile_dir: &Path) -> Result<Vec<PluginInfo>> {
    let manifest = ProfileManifest::read(profile_dir)?;
    let bundles = manifest
        .dsh
        .as_ref()
        .and_then(|d| d.profile.as_ref())
        .map(|p| p.bundles.clone())
        .unwrap_or_default();
    let patches = profile::read_patch(profile_dir)?;
    let mut out = Vec::new();
    // 1) bundle 层栈（真实生效的插件层，即使不在 dependencies）
    for b in &bundles {
        if !manifest.dependencies.contains_key(b) {
            let disabled = patches
                .iter()
                .any(|p| p.id() == Some(b.as_str()) && p.is_disabled());
            out.push(PluginInfo {
                name: b.clone(),
                version: "bundled".into(),
                is_bundle: true,
                disabled,
                installed: None,
            });
        }
    }
    // 2) dependencies（用户安装的插件）
    for (name, version) in manifest.dependencies.iter() {
        let disabled = patches
            .iter()
            .any(|p| p.id() == Some(name.as_str()) && p.is_disabled());
        let installed = Some(is_installed_locally(profile_dir, name));
        out.push(PluginInfo {
            name: name.clone(),
            version: version.clone(),
            is_bundle: bundles.contains(name),
            disabled,
            installed,
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// 导入插件：npm 包名（如 `@scope/pkg`）或本地路径（`file:` 前缀交给 CLI）。
///
/// 实际执行 `dsh plugin --profile <name> add <spec>`。
pub fn import_plugin(
    profile_name: &str,
    spec: &str,
    tx: Option<&Sender<String>>,
) -> Result<PluginCmdResult> {
    let spec = spec.trim();
    if spec.is_empty() {
        anyhow::bail!("plugin spec is empty");
    }
    info!("import plugin {spec} into profile {profile_name}");
    run_plugin_cmd(profile_name, &["add", spec], tx)
}

/// 移除插件：`dsh plugin --profile <name> remove <pkg>`。
pub fn remove_plugin(
    profile_name: &str,
    package: &str,
    tx: Option<&Sender<String>>,
) -> Result<PluginCmdResult> {
    info!("remove plugin {package} from profile {profile_name}");
    run_plugin_cmd(profile_name, &["remove", package], tx)
}

/// 启用 / 禁用插件（写 cordis.patch.yml）。
pub fn set_plugin_enabled(profile_dir: &Path, plugin: &str, enabled: bool) -> Result<()> {
    info!(
        "set plugin {plugin} enabled={enabled} in {}",
        profile_dir.display()
    );
    profile::set_patch_disabled(profile_dir, plugin, !enabled)
}

/// 检查插件本地是否已安装（node_modules 里存在）。
pub fn is_installed_locally(profile_dir: &Path, package: &str) -> bool {
    // 包名即 node_modules 下的相对路径（scoped @a/b 与普通包同构）
    profile_dir.join("node_modules").join(package).is_dir()
}

/// 插件 → 技能转写：把已安装插件的说明（package.json description +
/// README）转写成 SKILL.md，写入用户技能目录。
///
/// dsh-desktop 是 Rust 应用，无法直接运行 cordis JS 插件；转写让插件的
/// 使用说明进入 agent 上下文（技能机制），agent 回合中可 load_skill 加载
/// 执行——插件能力在 dsh-desktop 里真正生效。
/// 返回技能名。
pub fn transcribe_to_skill(
    profile_dir: &Path,
    package: &str,
    skills_dir: &Path,
) -> Result<String, String> {
    // node_modules 查找：profile 本级，其次上级 profiles/node_modules（部署级共享）
    let pkg_dir = [
        profile_dir.join("node_modules").join(package),
        profile_dir
            .parent()
            .map(|p| p.join("node_modules").join(package))
            .unwrap_or_default(),
    ]
    .into_iter()
    .find(|p| p.is_dir())
    .ok_or_else(|| format!("插件未安装（node_modules 缺失）: {package} —— 请先导入安装"))?;
    transcribe_pkg_to_skill(&pkg_dir, package, skills_dir)
}

/// 直接由包目录转写为技能（DSH 官方插件扫描用）。
pub fn transcribe_pkg_to_skill(
    pkg_dir: &Path,
    package: &str,
    skills_dir: &Path,
) -> Result<String, String> {
    // 插件名 → 技能名（kebab-case）
    let skill_name = plugin_to_skill_name(package);
    if !crate::engine::skill::is_skill_name(&skill_name) {
        return Err(format!("插件名无法转写为技能名: {package}"));
    }
    // 读取 package.json（description）
    let pkg_json: serde_json::Value = std::fs::read_to_string(pkg_dir.join("package.json"))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    let description = pkg_json
        .get("description")
        .and_then(|d| d.as_str())
        .unwrap_or(package)
        .to_string();
    // 读取 README（README.md / README.MD）
    let readme = ["README.md", "README.MD", "readme.md"]
        .iter()
        .find_map(|f| std::fs::read_to_string(pkg_dir.join(f)).ok())
        .unwrap_or_default();
    let readme_head: String = readme.chars().take(6000).collect();
    let truncated = readme.chars().count() > 6000;
    let body = if readme_head.trim().is_empty() {
        format!(
            "该技能由插件 {package} 转写而来。\n\n插件描述：{description}\n\n（插件未提供 README，请参阅插件目录 {} 了解详情）",
            pkg_dir.display()
        )
    } else {
        format!(
            "该技能由插件 {package} 转写而来，内容来自其 README（{}）。\n\n{}",
            if truncated { "已截断" } else { "完整" },
            readme_head
        )
    };
    // 写 SKILL.md（front-matter + 正文）
    let skill_dir = skills_dir.join(&skill_name);
    if skill_dir.is_dir() {
        return Err(format!("技能已存在: {skill_name}（如需重新转写请先移除）"));
    }
    std::fs::create_dir_all(&skill_dir).map_err(|e| format!("{e:#}"))?;
    let content = format!(
        "---\nname: {skill_name}\ndescription: {}\nmodelInvocable: true\n---\n\n{}",
        description.replace('\n', " "),
        body
    );
    std::fs::write(skill_dir.join("SKILL.md"), content).map_err(|e| format!("{e:#}"))?;
    log::info!("plugin {package} transcribed to skill {skill_name}");
    Ok(skill_name)
}

/// 扫描 DSH 官方 cordis 插件：
/// 1) $DSH_HOME/profiles/node_modules/@deepseek-ai/*（部署级 bundle 层）
/// 2) dsh CLI（npx 缓存）node_modules/@deepseek-ai/*
/// 3) $DSH_HOME/plugins-src/node_modules/@deepseek-ai/*（桌面端下载的插件）
/// 返回 (包名, 描述, 包目录)。
pub fn scan_dsh_plugins(cfg: &crate::config::AppConfig) -> Vec<(String, String, PathBuf)> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    let profiles_nm = cfg.dsh_home.join("profiles").join("node_modules");
    if profiles_nm.is_dir() {
        dirs.push(profiles_nm);
    }
    let dl_nm = cfg.dsh_home.join("plugins-src").join("node_modules");
    if dl_nm.is_dir() {
        dirs.push(dl_nm);
    }
    if let Some(dsh) = crate::dsh::cli::find_dsh() {
        // dsh CLI 所在 .bin 的上一级即 node_modules
        if let Some(bin_dir) = dsh.parent() {
            if let Some(nm) = bin_dir.parent() {
                if nm.file_name().map(|n| n == "node_modules").unwrap_or(false) {
                    dirs.push(nm.to_path_buf());
                }
            }
        }
    }
    let mut out: Vec<(String, String, PathBuf)> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for nm in dirs {
        let scope_dir = nm.join("@deepseek-ai");
        let Ok(entries) = std::fs::read_dir(&scope_dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let name = format!(
                "@deepseek-ai/{}",
                path.file_name().unwrap_or_default().to_string_lossy()
            );
            if !seen.insert(name.clone()) {
                continue;
            }
            let desc = std::fs::read_to_string(path.join("package.json"))
                .ok()
                .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
                .and_then(|v| {
                    v.get("description")
                        .and_then(|d| d.as_str())
                        .map(String::from)
                })
                .unwrap_or_default();
            out.push((name, desc, path));
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// 联网获取 DSH 官方插件清单：读取 npm registry 上 @deepseek-ai/dsh 的
/// dependencies（即官方 cordis 插件栈）。返回 (包名, 版本)。
pub fn fetch_dsh_plugin_list() -> Result<Vec<(String, String)>, String> {
    let url = "https://registry.npmjs.org/@deepseek-ai%2Fdsh";
    let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
        .user_agent("dsh-desktop-plugin-fetch")
        .build()
        .map_err(|e| format!("HTTP 客户端初始化失败: {e}"))?;
    let resp = client
        .get(url)
        .send()
        .map_err(|e| format!("请求 npm registry 失败: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("npm registry HTTP {}", resp.status().as_u16()));
    }
    let json: serde_json::Value = resp
        .json()
        .map_err(|e| format!("解析 npm registry 响应失败: {e}"))?;
    let latest = json
        .get("dist-tags")
        .and_then(|t| t.get("latest"))
        .and_then(|v| v.as_str())
        .ok_or("无法获取 @deepseek-ai/dsh 最新版本")?;
    let deps = json
        .get("versions")
        .and_then(|v| v.get(latest))
        .and_then(|v| v.get("dependencies"))
        .and_then(|v| v.as_object())
        .ok_or("无法读取 dependencies")?;
    let mut out: Vec<(String, String)> = deps
        .iter()
        .filter(|(k, _)| k.starts_with("@deepseek-ai/dsh-"))
        .map(|(k, v)| {
            (
                k.clone(),
                v.as_str().unwrap_or("").trim_start_matches('^').to_string(),
            )
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    log::info!("fetched {} dsh plugins from npm registry", out.len());
    Ok(out)
}

/// 联网下载插件包（npm tarball → 解压到本地插件仓库），返回包目录。
/// 仓库结构：`<cache_root>/node_modules/@deepseek-ai/<name>/`（标准 node_modules
/// 布局），因此 node_called 的 NODE_PATH 可直接 require 到下载的插件，
/// 无需转写为技能。
pub fn download_dsh_plugin(name: &str, cache_root: &Path) -> Result<PathBuf, String> {
    // registry 单包元数据 → tarball URL
    let meta_url = format!("https://registry.npmjs.org/{}", name.replace('/', "%2F"));
    let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
        .user_agent("dsh-desktop-plugin-fetch")
        .build()
        .map_err(|e| format!("HTTP 客户端初始化失败: {e}"))?;
    let resp = client
        .get(&meta_url)
        .send()
        .map_err(|e| format!("请求 npm registry 失败（{name}）: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!(
            "npm registry HTTP {}（{name}）",
            resp.status().as_u16()
        ));
    }
    let json: serde_json::Value = resp.json().map_err(|e| format!("解析元数据失败: {e}"))?;
    let latest = json
        .get("dist-tags")
        .and_then(|t| t.get("latest"))
        .and_then(|v| v.as_str())
        .ok_or("无法获取最新版本")?;
    let tarball = json
        .get("versions")
        .and_then(|v| v.get(latest))
        .and_then(|v| v.get("dist"))
        .and_then(|v| v.get("tarball"))
        .and_then(|v| v.as_str())
        .ok_or("无法获取 tarball 地址")?;
    // 下载 tarball
    let tgz = cache_root.join(format!("{}.tgz", name.replace('/', "-")));
    std::fs::create_dir_all(cache_root).map_err(|e| format!("{e:#}"))?;
    let bytes = client
        .get(tarball)
        .send()
        .map_err(|e| format!("下载 tarball 失败: {e}"))?
        .bytes()
        .map_err(|e| format!("读取 tarball 失败: {e}"))?;
    std::fs::write(&tgz, &bytes).map_err(|e| format!("{e:#}"))?;
    // 解压到 <cache_root>/node_modules/@deepseek-ai/<base>（标准 node_modules 布局）
    let base = name.rsplit('/').next().unwrap_or(name);
    let dest = cache_root
        .join("node_modules")
        .join("@deepseek-ai")
        .join(base);
    if dest.is_dir() {
        let _ = std::fs::remove_dir_all(&dest);
    }
    std::fs::create_dir_all(&dest).map_err(|e| format!("{e:#}"))?;
    let status = std::process::Command::new("tar")
        .args(["-xzf"])
        .arg(&tgz)
        .arg("-C")
        .arg(&dest)
        .status()
        .map_err(|e| format!("tar 执行失败: {e}"))?;
    if !status.success() {
        return Err(format!("tar 解压失败（exit {:?}）", status.code()));
    }
    // npm tarball 解压后内容在 package/ 子目录：把内容上移到包目录
    let pkg = dest.join("package");
    if pkg.is_dir() {
        for entry in std::fs::read_dir(&pkg).map_err(|e| format!("{e:#}"))? {
            let entry = entry.map_err(|e| format!("{e:#}"))?;
            let target = dest.join(entry.file_name());
            if target.exists() {
                if target.is_dir() {
                    let _ = std::fs::remove_dir_all(&target);
                } else {
                    let _ = std::fs::remove_file(&target);
                }
            }
            std::fs::rename(entry.path(), &target).map_err(|e| format!("{e:#}"))?;
        }
        let _ = std::fs::remove_dir(&pkg);
    }
    log::info!("plugin {} downloaded to {}", name, dest.display());
    Ok(dest)
}

/// 插件名 → 技能名：`@scope/pkg` → `pkg`（重名由调用方处理）。
pub fn plugin_to_skill_name(package: &str) -> String {
    let base = package
        .rsplit('/')
        .next()
        .unwrap_or(package)
        .trim_start_matches('@');
    let mut out = String::new();
    for c in base.chars() {
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            out.push(c);
        } else {
            out.push('-');
        }
    }
    // 清理连续的 '-' 与首尾 '-'（kebab-case 文法）
    let cleaned: String = out
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    if cleaned.is_empty() {
        "plugin".into()
    } else {
        cleaned
    }
}

/// 后台导入任务的通道封装：起线程跑 import/remove，逐行回传。
pub fn import_async(
    profile_name: String,
    spec: String,
) -> (
    Receiver<String>,
    std::thread::JoinHandle<Result<PluginCmdResult>>,
) {
    let (tx, rx) = channel::<String>();
    let handle = std::thread::Builder::new()
        .name("plugin-import".into())
        .spawn(move || {
            let tx_ref: Option<&Sender<String>> = Some(&tx);
            let res = import_plugin(&profile_name, &spec, tx_ref);
            let _ = tx.send(format!(
                "__DONE__ exit={}",
                res.as_ref().map(|r| r.exit_code).unwrap_or(-1)
            ));
            res
        })
        .expect("spawn plugin-import thread");
    (rx, handle)
}

/// 解析 import 结果（供 UI 显示）。
pub fn describe_result(res: &PluginCmdResult) -> String {
    let mut s = String::new();
    if !res.stdout.trim().is_empty() {
        s.push_str(&res.stdout);
    }
    if !res.stderr.trim().is_empty() {
        if !s.is_empty() {
            s.push('\n');
        }
        s.push_str(&res.stderr);
    }
    if s.trim().is_empty() {
        s = format!("exit code {}", res.exit_code);
    }
    s
}

/// 确认 profile 存在（用于 UI 提示）。
pub fn ensure_profile(profile_dir: &Path, name: &str) -> Result<()> {
    if profile_dir.is_dir() {
        Ok(())
    } else {
        anyhow::bail!("profile {name} does not exist at {}", profile_dir.display())
    }
}

/// 获取 patch 里的条目 id 列表（供 UI 展示启停状态时引用）。
pub fn patch_entry_ids(profile_dir: &Path) -> Vec<String> {
    profile::read_patch(profile_dir)
        .unwrap_or_default()
        .iter()
        .filter_map(|p: &PatchEntry| p.id().map(String::from))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn sample_profile(dir: &Path) {
        let manifest = ProfileManifest {
            name: "test-profile".into(),
            private: Some(true),
            dependencies: [
                ("@deepseek-ai/dsh-base".to_string(), "1.0.0".to_string()),
                ("@deepseek-ai/dsh-web-app".to_string(), "1.0.0".to_string()),
            ]
            .into_iter()
            .collect(),
            dsh: Some(
                serde_json::from_value(serde_json::json!({
                    "profile": { "bundles": ["@deepseek-ai/dsh-base"] }
                }))
                .unwrap(),
            ),
        };
        manifest.write(dir).unwrap();
    }

    #[test]
    fn list_plugins_parses_manifest() {
        let td = tempdir().unwrap();
        sample_profile(td.path());
        let plugins = list_plugins(td.path()).unwrap();
        assert_eq!(plugins.len(), 2);
        let base = plugins
            .iter()
            .find(|p| p.name == "@deepseek-ai/dsh-base")
            .unwrap();
        assert!(base.is_bundle);
        assert!(!base.disabled);
    }

    /// 回归：web profile 的 bundles 在 dsh.profile.bundles 而 dependencies 为空——
    /// 列表必须仍然显示 bundle 插件（否则插件页永远"无插件依赖"，像摆设）。
    #[test]
    fn list_plugins_includes_bundles_only() {
        let td = tempdir().unwrap();
        let manifest = ProfileManifest {
            name: "web".into(),
            private: Some(true),
            dependencies: Default::default(), // web profile：dependencies 为空
            dsh: Some(
                serde_json::from_value(serde_json::json!({
                    "profile": { "bundles": ["@deepseek-ai/dsh-base", "@deepseek-ai/dsh-web-app"] }
                }))
                .unwrap(),
            ),
        };
        manifest.write(td.path()).unwrap();
        let plugins = list_plugins(td.path()).unwrap();
        assert_eq!(plugins.len(), 2, "bundle 层栈必须显示: {plugins:?}");
        for p in &plugins {
            assert!(p.is_bundle, "{:?} 应为 bundle", p.name);
            assert_eq!(p.version, "bundled");
            assert_eq!(p.installed, None, "bundle 由部署环境解析，无本地安装状态");
        }
        let names: Vec<&str> = plugins.iter().map(|p| p.name.as_str()).collect();
        assert!(names.contains(&"@deepseek-ai/dsh-base"));
        assert!(names.contains(&"@deepseek-ai/dsh-web-app"));
    }

    #[test]
    fn toggle_disabled_roundtrip() {
        let td = tempdir().unwrap();
        sample_profile(td.path());
        set_plugin_enabled(td.path(), "@deepseek-ai/dsh-base", false).unwrap();
        let plugins = list_plugins(td.path()).unwrap();
        let base = plugins
            .iter()
            .find(|p| p.name == "@deepseek-ai/dsh-base")
            .unwrap();
        assert!(base.disabled);
        set_plugin_enabled(td.path(), "@deepseek-ai/dsh-base", true).unwrap();
        let plugins = list_plugins(td.path()).unwrap();
        let base = plugins
            .iter()
            .find(|p| p.name == "@deepseek-ai/dsh-base")
            .unwrap();
        assert!(!base.disabled);
    }

    #[test]
    fn import_empty_spec_rejected() {
        let err = import_plugin("web", "   ", None).unwrap_err();
        assert!(err.to_string().contains("empty"));
    }

    #[test]
    fn remove_dependency_updates_manifest() {
        let td = tempdir().unwrap();
        sample_profile(td.path());
        profile::remove_dependency(td.path(), "@deepseek-ai/dsh-web-app").unwrap();
        let m = ProfileManifest::read(td.path()).unwrap();
        assert!(!m.dependencies.contains_key("@deepseek-ai/dsh-web-app"));
    }

    #[test]
    fn is_installed_locally_detects_node_modules() {
        let td = tempdir().unwrap();
        sample_profile(td.path());
        let nm = td
            .path()
            .join("node_modules")
            .join("@deepseek-ai")
            .join("dsh-base");
        std::fs::create_dir_all(&nm).unwrap();
        assert!(is_installed_locally(td.path(), "@deepseek-ai/dsh-base"));
        assert!(!is_installed_locally(td.path(), "@deepseek-ai/nope"));
    }

    /// 插件 → 技能转写：description + README → SKILL.md，且可被技能注册表加载。
    #[test]
    fn transcribe_plugin_to_skill_writes_skill_md() {
        let td = tempdir().unwrap();
        // 假插件：node_modules/@scope/demo-plugin
        let pkg_dir = td
            .path()
            .join("node_modules")
            .join("@scope")
            .join("demo-plugin");
        std::fs::create_dir_all(&pkg_dir).unwrap();
        std::fs::write(
            pkg_dir.join("package.json"),
            r#"{"name":"@scope/demo-plugin","description":"演示插件：提供文档处理能力"}"#,
        )
        .unwrap();
        std::fs::write(
            pkg_dir.join("README.md"),
            "# Demo Plugin\n\n处理文档的插件，使用方法：\n1. 步骤一\n2. 步骤二",
        )
        .unwrap();
        let skills_dir = td.path().join("skills");
        std::fs::create_dir_all(&skills_dir).unwrap();

        let name =
            transcribe_to_skill(td.path(), "@scope/demo-plugin", &skills_dir).expect("转写应成功");
        assert_eq!(name, "demo-plugin");
        let md = std::fs::read_to_string(skills_dir.join("demo-plugin").join("SKILL.md"))
            .expect("SKILL.md 应生成");
        assert!(md.contains("name: demo-plugin"));
        assert!(md.contains("演示插件：提供文档处理能力"));
        assert!(md.contains("步骤一"));
        assert!(md.contains("@scope/demo-plugin"), "正文应标注来源插件");

        // 可被技能注册表加载（agent 回合可用）
        let mut reg = crate::engine::skill::SkillRegistry::default();
        reg.load_from_dir(&skills_dir);
        let s = reg.get("demo-plugin").expect("转写技能应可加载");
        assert_eq!(s.description.as_deref(), Some("演示插件：提供文档处理能力"));
        assert!(s.invocation.model_invocable);

        // 未安装的插件拒绝
        assert!(transcribe_to_skill(td.path(), "@scope/nope", &skills_dir).is_err());
        // 重复转写拒绝（技能已存在）
        assert!(transcribe_to_skill(td.path(), "@scope/demo-plugin", &skills_dir).is_err());
    }
}
