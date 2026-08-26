//! 技能系统：provider 注册表 + kebab-case 名校验 + 用户目录磁盘加载（对齐 dsh-skill）。
//!
//! 技能来源（provider）合并目录，按 name 解析获胜技能；
//! 模型可调用性由 invocation.modelInvocable 控制。
//! 用户技能存放在 $DSH_HOME/skills/<skill-name>/ 下，支持两种文件：
//!   - skill.md（本应用模板）
//!   - SKILL.md（anthropics/skills 官方格式，front-matter YAML + 正文 instructions）
//! 支持从 GitHub 仓库（默认 anthropics/skills）远程安装技能。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// 默认技能源（Anthropic 官方技能仓库）。
pub const DEFAULT_GITHUB_SKILLS_REPO: (&str, &str, &str) = ("anthropics", "skills", "main");

/// 技能名文法（对齐 SKILL_NAME：kebab-case）。
pub fn is_skill_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !name.starts_with('-')
        && !name.ends_with('-')
        && !name.contains("--")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillInvocation {
    #[serde(default = "default_model_invocable")]
    pub model_invocable: bool,
}

fn default_model_invocable() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Skill {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    #[serde(default = "default_invocation")]
    pub invocation: SkillInvocation,
    /// provider 排名（对齐 RUNTIME_RANK=250 / BUNDLED_SKILL_RANK=600，低者优先）
    pub rank: u32,
}

fn default_invocation() -> SkillInvocation {
    SkillInvocation {
        model_invocable: true,
    }
}

/// 技能注册表。
#[derive(Debug, Default)]
pub struct SkillRegistry {
    skills: HashMap<String, Skill>,
}

impl SkillRegistry {
    /// 注册技能（同名低 rank 获胜，对齐 provider 合并语义）。
    pub fn register(&mut self, skill: Skill) {
        match self.skills.get(&skill.name) {
            Some(existing) if existing.rank <= skill.rank => {}
            _ => {
                self.skills.insert(skill.name.clone(), skill);
            }
        }
    }

    pub fn get(&self, name: &str) -> Option<&Skill> {
        self.skills.get(name)
    }

    pub fn model_invocable(&self, name: &str) -> bool {
        self.skills
            .get(name)
            .map(|s| s.invocation.model_invocable)
            .unwrap_or(false)
    }

    pub fn list(&self) -> Vec<&Skill> {
        let mut v: Vec<_> = self.skills.values().collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        v
    }

    pub fn len(&self) -> usize {
        self.skills.len()
    }

    /// 从用户技能目录加载全部技能（每技能一个子目录，含 skill.md 或 SKILL.md）。
    /// 用户目录 rank = 250（对齐 RUNTIME_RANK，优先于 bundle 技能）。
    pub fn load_from_dir(&mut self, dir: &Path) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            // anthropics/skills 用 SKILL.md；本应用模板用 skill.md
            let file = if path.join("SKILL.md").is_file() {
                path.join("SKILL.md")
            } else {
                path.join("skill.md")
            };
            if !file.is_file() {
                continue;
            }
            match parse_skill_md(&file) {
                Ok(skill) => {
                    log::info!("skill loaded: {} ({})", skill.name, file.display());
                    self.register(skill);
                }
                Err(e) => log::warn!("skill parse failed {}: {e}", file.display()),
            }
        }
    }

    /// 创建技能：目录 + skill.md 模板（name 需符合 kebab-case）。
    pub fn create_skill(
        &self,
        dir: &Path,
        name: &str,
        description: &str,
        instructions: &str,
    ) -> Result<PathBuf, String> {
        if !is_skill_name(name) {
            return Err(format!(
                "技能名必须为 kebab-case（小写字母/数字/连字符）: {name}"
            ));
        }
        let skill_dir = dir.join(name);
        if skill_dir.is_dir() {
            return Err(format!("技能已存在: {name}"));
        }
        std::fs::create_dir_all(&skill_dir).map_err(|e| format!("{e:#}"))?;
        let content = format!(
            "---\nname: {name}\ndescription: {}\nmodelInvocable: true\n---\n\n{}",
            description.trim(),
            instructions.trim()
        );
        std::fs::write(skill_dir.join("skill.md"), content).map_err(|e| format!("{e:#}"))?;
        Ok(skill_dir)
    }

    /// 从 GitHub 仓库安装技能：下载 <owner>/<repo>@<branch> 的
    /// `skills/<name>/SKILL.md`（anthropics/skills 官方布局）到本地技能目录。
    /// 已存在同名技能时拒绝覆盖（避免误删本地修改）。
    pub fn install_from_github(
        &self,
        dir: &Path,
        owner: &str,
        repo: &str,
        branch: &str,
        name: &str,
    ) -> Result<PathBuf, String> {
        if !is_skill_name(name) {
            return Err(format!("技能名必须为 kebab-case: {name}"));
        }
        let skill_dir = dir.join(name);
        if skill_dir.is_dir() {
            return Err(format!("技能已存在: {name}（如需更新请先移除）"));
        }
        let url = github_skill_raw_url(owner, repo, branch, name);
        let client = reqwest::blocking::Client::builder()
            .user_agent("dsh-desktop-skill-installer")
            .build()
            .map_err(|e| format!("HTTP 客户端初始化失败: {e}"))?;
        let resp = client
            .get(&url)
            .send()
            .map_err(|e| format!("下载失败（{url}）: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!(
                "下载失败：HTTP {}（{url}）——技能名可能不存在，或仓库路径不符",
                resp.status().as_u16()
            ));
        }
        let text = resp.text().map_err(|e| format!("读取响应失败: {e}"))?;
        if text.trim().is_empty() {
            return Err(format!("下载内容为空（{url}）"));
        }
        std::fs::create_dir_all(&skill_dir).map_err(|e| format!("{e:#}"))?;
        std::fs::write(skill_dir.join("SKILL.md"), text).map_err(|e| format!("{e:#}"))?;
        Ok(skill_dir)
    }

    /// 列出 GitHub 仓库 `skills/` 目录下的技能名（GitHub Contents API）。
    /// 仅返回目录型条目（技能 = 目录）。网络失败返回错误。
    pub fn list_remote_skills(
        owner: &str,
        repo: &str,
        branch: &str,
    ) -> Result<Vec<String>, String> {
        let url =
            format!("https://api.github.com/repos/{owner}/{repo}/contents/skills?ref={branch}");
        let client = reqwest::blocking::Client::builder()
            .user_agent("dsh-desktop-skill-installer")
            .build()
            .map_err(|e| format!("HTTP 客户端初始化失败: {e}"))?;
        let resp = client
            .get(&url)
            .send()
            .map_err(|e| format!("请求 GitHub API 失败: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("GitHub API HTTP {}", resp.status().as_u16()));
        }
        let items: serde_json::Value = resp
            .json()
            .map_err(|e| format!("解析 GitHub API 响应失败: {e}"))?;
        let mut names = Vec::new();
        if let Some(arr) = items.as_array() {
            for item in arr {
                if item.get("type").and_then(|t| t.as_str()) == Some("dir") {
                    if let Some(n) = item.get("name").and_then(|n| n.as_str()) {
                        if is_skill_name(n) {
                            names.push(n.to_string());
                        }
                    }
                }
            }
        }
        names.sort();
        Ok(names)
    }

    /// 从本地文件导入技能（skill.md / SKILL.md）。
    /// 技能名取 front-matter name；缺失时用父目录名。返回技能名。
    pub fn import_skill_file(&self, dir: &Path, src: &Path) -> Result<String, String> {
        let file_name = src
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_string();
        if file_name != "skill.md" && file_name != "SKILL.md" {
            return Err(format!(
                "仅支持 skill.md / SKILL.md 文件（实际: {file_name}）"
            ));
        }
        let parsed = parse_skill_md(src).map_err(|e| format!("解析失败: {e}"))?;
        let name = parsed.name.clone();
        if !is_skill_name(&name) {
            return Err(format!("技能名非法: {name}"));
        }
        let dest_dir = dir.join(&name);
        if dest_dir.exists() {
            return Err(format!("技能已存在: {name}"));
        }
        std::fs::create_dir_all(&dest_dir).map_err(|e| format!("{e:#}"))?;
        std::fs::copy(src, dest_dir.join(&file_name)).map_err(|e| format!("{e:#}"))?;
        Ok(name)
    }

    /// 从本地目录导入技能（目录内须含 skill.md 或 SKILL.md）。
    /// 目录名即技能名；整个目录递归复制。返回技能名。
    pub fn import_skill_dir(&self, dir: &Path, src: &Path) -> Result<String, String> {
        let name = src
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_string();
        if !is_skill_name(&name) {
            return Err(format!("目录名非法（需 kebab-case）: {name}"));
        }
        if !src.join("skill.md").is_file() && !src.join("SKILL.md").is_file() {
            return Err(format!(
                "目录内未找到 skill.md 或 SKILL.md: {}",
                src.display()
            ));
        }
        let dest_dir = dir.join(&name);
        if dest_dir.exists() {
            return Err(format!("技能已存在: {name}"));
        }
        copy_dir_all(src, &dest_dir).map_err(|e| format!("复制失败: {e}"))?;
        Ok(name)
    }
}

/// 递归复制目录。
fn copy_dir_all(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_dir_all(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

/// 构造 GitHub raw 下载 URL：skills/<name>/SKILL.md。
pub fn github_skill_raw_url(owner: &str, repo: &str, branch: &str, name: &str) -> String {
    format!("https://raw.githubusercontent.com/{owner}/{repo}/{branch}/skills/{name}/SKILL.md")
}

/// 解析 skill.md / SKILL.md：front-matter（YAML）+ 正文 instructions。
pub(crate) fn parse_skill_md(file: &Path) -> Result<Skill, String> {
    let text = std::fs::read_to_string(file).map_err(|e| format!("{e:#}"))?;
    let (front, body) = match split_front_matter(&text) {
        Some((f, b)) => (Some(f), b),
        None => (None, None),
    };
    let mut skill = Skill {
        name: file
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .unwrap_or("skill")
            .to_string(),
        description: None,
        instructions: None,
        invocation: default_invocation(),
        rank: 250, // 用户技能：低 rank 优先
    };
    if let Some(front) = front {
        if let Ok(v) = serde_yaml::from_str::<serde_yaml::Value>(front) {
            if let Some(n) = v.get("name").and_then(|x| x.as_str()) {
                if !n.trim().is_empty() {
                    skill.name = n.trim().to_string();
                }
            }
            skill.description = v
                .get("description")
                .and_then(|x| x.as_str())
                .map(|s| s.to_string());
            if let Some(mi) = v.get("modelInvocable").and_then(|x| x.as_bool()) {
                skill.invocation.model_invocable = mi;
            }
        }
    }
    let body = body.unwrap_or("");
    if !body.trim().is_empty() {
        skill.instructions = Some(body.trim().to_string());
    }
    Ok(skill)
}

/// 拆 front-matter：开头 `---\n ... \n---\n` 为 front，其余为 body。
fn split_front_matter(text: &str) -> Option<(&str, Option<&str>)> {
    let t = text.trim_start_matches('\u{feff}');
    let rest = t
        .strip_prefix("---")
        .or_else(|| t.strip_prefix("---\r\n"))?;
    let rest = rest
        .strip_prefix('\n')
        .or_else(|| rest.strip_prefix("\r\n"))?;
    let end = rest.find("\n---")?;
    let front = &rest[..end];
    let after = &rest[end + 4..];
    let after = after.strip_prefix('\n').unwrap_or(after);
    let after = after.strip_prefix("\r\n").unwrap_or(after);
    Some((front, Some(after)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn skill(name: &str, rank: u32) -> Skill {
        Skill {
            name: name.into(),
            description: Some("desc".into()),
            instructions: None,
            invocation: SkillInvocation {
                model_invocable: true,
            },
            rank,
        }
    }

    #[test]
    fn skill_name_grammar() {
        assert!(is_skill_name("hfqr"));
        assert!(is_skill_name("my-skill-2"));
        assert!(!is_skill_name("MySkill"));
        assert!(!is_skill_name("-bad"));
        assert!(!is_skill_name("bad--name"));
        assert!(!is_skill_name(""));
    }

    #[test]
    fn lower_rank_wins() {
        let mut reg = SkillRegistry::default();
        reg.register(skill("demo", 600));
        reg.register(skill("demo", 250));
        assert_eq!(reg.get("demo").unwrap().rank, 250);
        assert!(reg.model_invocable("demo"));
    }

    #[test]
    fn split_front_matter_basic() {
        let md = "---\nname: demo\ndescription: 测试\n---\n\n这是正文";
        let (front, body) = split_front_matter(md).expect("front matter");
        assert!(front.contains("name: demo"));
        assert_eq!(body.unwrap_or("").trim(), "这是正文");
        // 无 front matter
        assert!(split_front_matter("plain text").is_none());
    }

    #[test]
    fn load_and_create_skills_from_dir() {
        use tempfile::tempdir;
        let td = tempdir().unwrap();
        let reg = SkillRegistry::default();
        // 创建技能（用户目录）
        let dir = reg
            .create_skill(td.path(), "my-skill", "我的技能", "按以下步骤执行…")
            .unwrap();
        assert!(dir.join("skill.md").is_file());
        // 非法名拒绝
        assert!(reg.create_skill(td.path(), "Bad Skill", "x", "y").is_err());
        // 从目录加载
        let mut reg2 = SkillRegistry::default();
        reg2.load_from_dir(td.path());
        assert_eq!(reg2.len(), 1);
        let s = reg2.get("my-skill").unwrap();
        assert_eq!(s.description.as_deref(), Some("我的技能"));
        assert_eq!(s.instructions.as_deref(), Some("按以下步骤执行…"));
        assert!(s.invocation.model_invocable);
        assert_eq!(s.rank, 250);
    }

    /// anthropics/skills 格式：SKILL.md（大写）同样被识别加载。
    #[test]
    fn loads_anthropic_skill_md() {
        use tempfile::tempdir;
        let td = tempdir().unwrap();
        let skill_dir = td.path().join("webapp-testing");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: webapp-testing\ndescription: Test web apps with Playwright\n---\n\n# Test Steps\nRun playwright...",
        )
        .unwrap();
        let mut reg = SkillRegistry::default();
        reg.load_from_dir(td.path());
        let s = reg.get("webapp-testing").expect("SKILL.md 技能应被加载");
        assert_eq!(
            s.description.as_deref(),
            Some("Test web apps with Playwright")
        );
        assert!(s
            .instructions
            .as_deref()
            .unwrap()
            .contains("Run playwright"));
    }

    /// GitHub 安装：URL 构造 + 同名拒绝（不实际网络下载）。
    #[test]
    fn github_install_url_and_guard() {
        assert_eq!(
            github_skill_raw_url("anthropics", "skills", "main", "webapp-testing"),
            "https://raw.githubusercontent.com/anthropics/skills/main/skills/webapp-testing/SKILL.md"
        );
        use tempfile::tempdir;
        let td = tempdir().unwrap();
        let reg = SkillRegistry::default();
        // 非法名拒绝（不触发网络）
        assert!(reg
            .install_from_github(td.path(), "anthropics", "skills", "main", "Bad Name")
            .is_err());
        // 同名已存在拒绝（不触发网络）
        std::fs::create_dir_all(td.path().join("existing")).unwrap();
        assert!(reg
            .install_from_github(td.path(), "anthropics", "skills", "main", "existing")
            .is_err());
    }

    /// 本地导入：文件 + 目录。
    #[test]
    fn import_local_file_and_dir() {
        use tempfile::tempdir;
        let td = tempdir().unwrap();
        let skills_dir = td.path().join("skills");
        std::fs::create_dir_all(&skills_dir).unwrap();
        let reg = SkillRegistry::default();

        // 导入文件（front-matter name 决定技能名）
        let src_file = td.path().join("SKILL.md");
        std::fs::write(
            &src_file,
            "---\nname: from-file\ndescription: 文件导入\n---\n\n正文",
        )
        .unwrap();
        let name = reg
            .import_skill_file(&skills_dir, &src_file)
            .expect("导入文件");
        assert_eq!(name, "from-file");
        assert!(skills_dir.join("from-file").join("SKILL.md").is_file());

        // 非法文件类型拒绝
        let bad = td.path().join("notes.txt");
        std::fs::write(&bad, "x").unwrap();
        assert!(reg.import_skill_file(&skills_dir, &bad).is_err());

        // 导入目录（目录名即技能名）
        let src_dir = td.path().join("my-local-skill");
        std::fs::create_dir_all(&src_dir).unwrap();
        std::fs::write(
            src_dir.join("skill.md"),
            "---\nname: my-local-skill\ndescription: 目录导入\n---\n\n正文",
        )
        .unwrap();
        std::fs::write(src_dir.join("extra.txt"), "附带文件").unwrap();
        let name = reg
            .import_skill_dir(&skills_dir, &src_dir)
            .expect("导入目录");
        assert_eq!(name, "my-local-skill");
        assert!(skills_dir.join("my-local-skill").join("skill.md").is_file());
        assert!(
            skills_dir
                .join("my-local-skill")
                .join("extra.txt")
                .is_file(),
            "目录应完整复制"
        );

        // 无 skill 文件的目录拒绝
        let empty = td.path().join("empty-dir");
        std::fs::create_dir_all(&empty).unwrap();
        assert!(reg.import_skill_dir(&skills_dir, &empty).is_err());

        // 导入后可加载
        let mut reg2 = SkillRegistry::default();
        reg2.load_from_dir(&skills_dir);
        assert_eq!(reg2.len(), 2);
        assert!(reg2.get("from-file").is_some());
        assert!(reg2.get("my-local-skill").is_some());
    }
}
