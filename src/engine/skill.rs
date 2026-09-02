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

/// 内置领域增强技能（提示词级增强：模型按需 load_skill 加载领域规范；
/// 用户可在技能目录创建同名技能覆盖，目录 rank 250 < 内置 600）。
/// 领域插件也可以经 cordis 子进程插件提供工具级增强（tools 字段），
/// 两者互补：技能管"怎么思考"，插件管"额外能力"。
pub fn builtin_skills() -> Vec<Skill> {
    vec![
        Skill {
            name: "chemistry-research".into(),
            description: Some(
                "Domain enhancement for chemistry research: nomenclature \
                 and structure-representation standards, calculation and \
                 dimensional-analysis checks, experiment design, caution in \
                 spectral assignment, and strict data-integrity requirements"
                    .into(),
            ),
            instructions: Some(CHEMISTRY_INSTRUCTIONS.into()),
            invocation: SkillInvocation {
                model_invocable: true,
            },
            rank: 600,
        },
        Skill {
            name: "academic-writing".into(),
            description: Some(
                "Domain enhancement for academic paper writing: IMRaD \
                 structure, academic language standards, citation styles \
                 (APA / GB/T 7714), self-explanatory figures and tables, \
                 abstract and reviewer-response writing"
                    .into(),
            ),
            instructions: Some(WRITING_INSTRUCTIONS.into()),
            invocation: SkillInvocation {
                model_invocable: true,
            },
            rank: 600,
        },
        Skill {
            name: "paper-authenticity-review".into(),
            description: Some(
                "Authenticity review and scoring of an academic paper on a \
                 0-100 scale (0 = demonstrably fabricated): internal \
                 plausibility analysis of the paper's data and content, \
                 bounded one-level verification of its own reference list \
                 (no recursive citation chasing), and a weighted final \
                 verdict with itemized evidence. Use when the user asks to \
                 evaluate, fact-check, or score a paper / preprint / \
                 manuscript for credibility or fabrication risk"
                    .into(),
            ),
            instructions: Some(PAPER_REVIEW_INSTRUCTIONS.into()),
            invocation: SkillInvocation {
                model_invocable: true,
            },
            rank: 600,
        },
    ]
}

const CHEMISTRY_INSTRUCTIONS: &str = r#"# Chemistry Research Domain Standards

When handling chemistry-related tasks (research, calculations, experiment design, spectral interpretation, literature review), strictly follow:

## 1 Nomenclature and Structural Representation
- Prefer IUPAC nomenclature; common names may be given alongside (e.g., acetic acid (ethanoic acid)).
- Provide at least one canonical machine-readable structure representation: SMILES (or InChI). Whenever stereochemistry is involved it MUST be specified (R/S, E/Z, wedge bonds / @@).
- Report the molecular formula and molar mass with sensible significant figures; when computing molar mass, state the atomic-weight source (IUPAC 2021 standard).

## 2 Calculations
- Show every calculation step by step: given quantities -> formula -> substitution (with units) -> result (unit + significant figures).
- Carry dimensional analysis through every step. The result's significant figures must match the least precise input (pH/log quantities follow the decimal-place rule).
- Chemical equations MUST be balanced (atom conservation AND charge conservation); for redox reactions, show the half-reactions and the number of electrons transferred.
- State all assumptions in equilibrium/thermodynamics/kinetics calculations (ideal gas, constant temperature, side reactions neglected, etc.).

## 3 Data Integrity (MOST IMPORTANT)
- NEVER fabricate data: melting points, boiling points, pKa, solubility, bond lengths, spectral data, yields, etc. Well-established values may be given with the source category noted (e.g., "common handbook value"); when uncertain or potentially inconsistent, say explicitly that verification is required and suggest web_search or an authoritative database (CRC, NIST, PubChem, Reaxys, SciFinder).
- Spectral assignment (NMR/IR/MS/UV-Vis): give the assignment logic and alternative interpretations; avoid overclaiming — say "consistent with / supports", never "proves".
- Distinguish experimental values, literature values, and theoretical calculations (DFT/semi-empirical), and label the method and basis set where applicable.

## 4 Experiment Design
- Provide: objective -> reaction/scheme rationale -> reagents and quantities (molar ratios) -> conditions (temperature/time/atmosphere) -> workup -> characterization methods -> expected results and acceptance criteria.
- Safety: any hazardous reagent or operation (strong acids/bases, cyanides, azides, nitration, high pressure, runaway-exotherm risk) MUST carry a safety warning with the GHS category. Do NOT provide synthesis routes for controlled substances, explosives, or illicit drugs.
- Recommend controlled variables, replication (n >= 3 for statistical robustness), and blanks/controls.

## 5 Presentation
- Use standard unit symbols (mol/L or M consistently; note kPa/bar/atm where relevant); state temperature unambiguously (degrees C vs K).
- Organize review-type answers by branch (inorganic/organic/physical/analytical/polymer) and note the limitations of the knowledge cutoff.
"#;

const WRITING_INSTRUCTIONS: &str = r#"# Academic Paper Writing Standards

When handling paper writing (any manuscript section, abstract, cover letter, reviewer response, polishing), strictly follow:

## 1 Structure (IMRaD)
- Introduction: background funnel (field -> gap -> this paper's contribution); list contributions explicitly as 1-3 items.
- Methods: reproducibility standard — a third party must be able to repeat the work from this section alone; reagents/instruments/parameters/statistical methods complete.
- Results: report facts only, kept separate from Discussion; every figure/table is cited in the text by number (Fig. 1, Table 2).
- Discussion/Conclusion: interpret the results, compare with the literature, state limitations and outlook; do not re-list the results.

## 2 Academic Language
- Objective, precise, restrained: avoid absolute claims such as "very / extremely / the first (unless substantiated)".
- Voice follows the target journal's convention (active "we / this paper" voice is acceptable by default).
- Define every abbreviation at first use (full term + abbreviation, e.g., response surface methodology (RSM)); use abbreviations consistently throughout.
- Keep sentence length controlled in both English and Chinese (one idea per sentence); use logical connectives precisely (however / therefore / furthermore).

## 3 Citation and Link Authenticity (HARD RED LINE)
- NEVER fabricate references, DOIs, or URLs. Do not write any link/DOI/page number unless you are certain it genuinely exists.
- Mandatory literature verification workflow (whenever the user asks for a literature search, reference recommendations, or a literature review):
  1. Verify each item individually with web_search (query pattern: `"<paper title>" <first-author surname> <year>`);
  2. Verification passes ONLY if the title is actually seen in the search results (journal page / PubMed / Google Scholar / DOI record);
  3. Output in two clearly separated groups, never mixed —
     - VERIFIED: may include links (use ONLY URLs actually returned in the search results; never assemble doi.org or journal-page URLs yourself);
     - NOT VERIFIED ONLINE: give the bibliographic record only (author + title + year + journal), clearly marked "not verified; confirm before citing", with NO link;
  4. NEVER generate a "reference list" from memory — any list that has not passed steps 1-2 belongs to the NOT-VERIFIED group.
- Citations used within writing follow the same rule: search and verify before citing; if verification fails, give the bibliographic record with a note and no link.
- Write a DOI only if it was actually seen in the verification results (uniform form https://doi.org/10.xxxx/xxxx).
- High-risk hallucination fields — volume/issue/page numbers, year, author order, journal-name abbreviations — must each be cross-checked against the source.
- If a user-supplied citation looks wrong (misspelled author / nonexistent journal), point it out and suggest verification.
- Switch citation style on demand: GB/T 7714 (Chinese journals), APA 7, Vancouver/AMA, Elsevier numbered; in-text and end-of-paper styles must agree.
- Paraphrase rather than copy; mark third-party figures as "reproduced from / adapted from".
- Language and originality polishing preserves the author's meaning; do not ghostwrite content the user declares as originality-sensitive.

## 4 Figures and Tables
- Self-explanatory captions: state what + under which condition + which result; axes carry units; abbreviations consistent with the text.
- Tables use the three-line (booktabs) style per academic convention; annotate statistics (n, p, meaning of error bars).
- When suggesting improvements, point to concrete problems (readability / units / color accessibility).

## 5 Abstract and Submission Documents
- Structured abstract: background (1 sentence) -> methods (1-2 sentences) -> key results (2-3 sentences, quantified) -> significance (1 sentence).
- Cover letter: novelty + why it fits this journal + no simultaneous-submission statement; concise (< 350 words).
- Reviewer response: respond point by point with numbering; for accepted points state what changed and where; for rejected points give a polite, evidence-based rationale.

## 6 Delivery Habits
- Follow the journal/institution template supplied by the user; when absent, ask for the format requirements first (citation style, word limit, language).
- For long documents, deliver an outline for confirmation before expanding; for revised drafts, include a change note (what changed and why).
"#;

const PAPER_REVIEW_INSTRUCTIONS: &str = r#"# Paper Authenticity Review and Scoring Protocol

When the user asks to evaluate, fact-check, verify, or score a paper / preprint / manuscript for authenticity, credibility, or fabrication risk, execute this protocol strictly. Write the report in the user's language.

## 0 Scope and Hard Bounds (MUST obey)
- Input: a paper supplied as a file path, pasted text, or URL. Read the full text first (read_file / read_url), including the complete reference list. If only an abstract or fragment is available, say so explicitly and mark every score as reduced-confidence.
- DEPTH LIMIT (hard): verify ONLY the references in the paper under review. NEVER open, search, or enumerate the reference lists of the cited works (no second-level citations), and NEVER iterate over bibliographies found in search results. Citation analysis depth is exactly one level.
- BUDGET LIMIT (hard): at most 25 web_search calls for the entire review, and at most 20 references verified individually. If the reference list is longer than 20, sample by the priority rules in Stage B and state the sampling (checked N of R).
- If the user asks to exceed these bounds (e.g., "check the references of the references"), refuse and restate the depth limit; proceed only on explicit confirmation, and still never recurse.
- Do not modify the paper. This is a read-only review.

## 1 Stage A - Content and Data Plausibility (0-40 points)
Examine and report item by item, each with its location (section / table / figure):
- Numeric cross-consistency: compare every number that appears in more than one place (abstract vs body vs tables vs figure captions): sample sizes, percentages, means, standard deviations, p-values, effect sizes, accuracy metrics. Any unreconciled contradiction is a major finding.
- Dimensional and magnitude sanity: units, scales, and orders of magnitude against domain norms (e.g., yields above 100%, physically impossible concentrations or temperatures, questionnaire categories not summing to 100%, error bars smaller than the reported measurement resolution).
- Statistical validity: p-values consistent with the reported test statistics and degrees of freedom, confidence intervals consistent with p-values, baseline-group sizes, multiple-comparison handling, and overfitting signals (number of variables comparable to or larger than n).
- Methodological red flags: procedures that cannot work as described, missing parameters that make reproduction impossible, claimed datasets or instruments that do not plausibly exist, ethics-approval or trial-registration mismatches, impossible experimental timelines.
- Narrative consistency: whether the evidence presented actually supports each conclusion drawn; overclaiming; author-affiliation-venue mismatches; template or AI-generation artifacts.
- If nothing anomalous is found, state precisely: "no internal contradictions found within the checked items" - never phrase absence of findings as proof of authenticity.

## 2 Stage B - Reference List Analysis (0-40 points)
- Extract the full reference list. Record the total count R.
- Verification priority (when R > 20): (1) references that support the paper's central claims (most frequently cited in the text), (2) self-citations, (3) foundational or methodological references, (4) the head of the remaining list.
- For each selected reference, run web_search with the query `"<exact reference title>" <first-author surname> <year>`. Use one refined retry (title words only) before concluding NOT FOUND. Classify each as:
  - VERIFIED - the title is actually seen in the results (journal page, PubMed, Google Scholar, publisher record, or DOI record) and the authors / venue / year are consistent;
  - PARTIAL - a highly similar work exists but the metadata disagrees (different authors, venue, or year);
  - NOT FOUND - no matching record after the retry;
  - UNVERIFIABLE - books, pre-internet literature, theses, or items outside index coverage; these are NEVER counted as fabricated.
- Citation-claim fidelity spot check (at most 5 references, chosen where the paper's key claims depend on them): from the abstract or summary of the cited work, does it actually support the sentence it is attached to? For paywalled full texts, check at abstract level and say so.
- Flag any DOI or URL that does not resolve to the claimed work, and any reference entry lacking title, venue, and year altogether.
- Report the counts (verified / partial / not-found / unverifiable) and a per-item evidence table.

## 3 Stage C - Composite Score (0-100; 0 = fabricated)
Weights: Stage A 40 + Stage B 40 + transparency 20. The final score must be reproducible from the itemized deductions; always show the arithmetic.
- Stage A (start at 40): each demonstrated internal numeric contradiction -8 (floor 0); each implausible methodological or data element -5; each instance of overclaiming -3.
- Stage B: base = 40 x (verified + 0.5 x unverifiable) / checked; then each PARTIAL -2, each NOT FOUND -6, each misattributed key citation -8, each fabricated-looking DOI/URL -10, omitting the fidelity spot-checks without stated reason -5 (floor 0).
- Transparency (0-20): availability of data and code, methodological detail sufficient for reproduction, ethics / registration information, coherent author-affiliation-venue record.
Score discipline:
- 0 is reserved for demonstrable fabrication: internal evidence that the data were invented, or the majority of the checked references provably do not exist. NEVER assign 0 merely because the paper is offline, unindexed, non-English, or behind a paywall.
- Unverifiable does not mean nonexistent. Never assert "this paper is fraudulent" as a certainty; present the evidence and use calibrated language (e.g., "consistent with fabricated references: 7 of 10 checked items not found").
- Do not reward volume: a long reference list adds nothing unless its entries verify.

## 4 Confidentiality and Data-Use Restriction (hard)
- Treat the paper's core information (unpublished data, methods, results, figures, and any supplementary material) as confidential review material. It MUST NOT be used for AI retraining or for any purpose other than this review, and it MUST NOT be reproduced or redistributed beyond the report delivered to the user.
- Outbound minimization: external verification queries may carry only the minimum identifying metadata (reference titles, author names, venue, year). NEVER transmit the paper's core content (datasets, novel methods, unpublished results) to any external service.
- The report MUST end with this notice verbatim: "Confidentiality: the reviewed paper's core information was used solely for this assessment, only the verifying metadata above was sent externally, and it may not be used for AI retraining or redistribution."

## 5 Output Template
1. Verdict band (one line): 0-15 fabricated | 16-39 severe concerns | 40-59 mixed reliability | 60-79 sound with caveats | 80-100 credible.
2. Final score X/100, with the three subscores and the deduction arithmetic.
3. Stage A findings table: finding, location, severity (major / moderate / minor).
4. Stage B reference table: ref number, short title, classification, evidence note; plus the sampling statement (checked N of R) and total search count used.
5. Limitations (full-text access, language, index coverage, PDF-only checks) and recommended human follow-ups (contact the journal, COPE guidelines, publisher inquiry).
6. The confidentiality notice from section 4, verbatim.
"#;

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
        // 先注册内置领域技能（rank 600）；目录技能（rank 250）后到覆盖
        for sk in builtin_skills() {
            self.register(sk);
        }
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
        // 超时保护：挂起的请求不能让面板永久卡"操作中"（busy 不复位）
        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .user_agent("dsh-desktop-skill-installer")
            .connect_timeout(std::time::Duration::from_secs(10))
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
            .timeout(std::time::Duration::from_secs(30))
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
        // len = 目录技能 + 3 个内置领域技能；断言目标技能内容正确
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
        // len = 导入的 2 个 + 3 个内置领域技能
        assert!(reg2.get("from-file").is_some());
        assert!(reg2.get("my-local-skill").is_some());
    }
}

#[cfg(test)]
mod builtin_skill_tests {
    use super::*;

    /// 内置领域技能：注册后可列出、可读取指令、模型可调用。
    #[test]
    fn builtin_domain_skills_available() {
        let mut reg = SkillRegistry::default();
        reg.load_from_dir(std::path::Path::new("Z:/不存在的目录")); // 目录为空 → 仅内置
        assert!(reg.len() >= 2, "至少两个内置领域技能: {}", reg.len());
        let chem = reg.get("chemistry-research").expect("chemistry skill");
        assert!(chem.invocation.model_invocable);
        assert!(chem.instructions.as_deref().unwrap().contains("IUPAC"));
        assert!(chem
            .instructions
            .as_deref()
            .unwrap()
            .contains("NEVER fabricate data"));
        let writ = reg.get("academic-writing").expect("writing skill");
        assert!(writ.instructions.as_deref().unwrap().contains("IMRaD"));
        assert!(writ.instructions.as_deref().unwrap().contains("GB/T 7714"));
        // 引用真实性红线（用户明确要求：链接必须真实有效，不得编造）
        assert!(writ
            .instructions
            .as_deref()
            .unwrap()
            .contains("NEVER fabricate references, DOIs, or URLs"));
        assert!(writ
            .instructions
            .as_deref()
            .unwrap()
            .contains("search and verify before citing"));
        // 文献查找强制核验流程（逐条 web_search 确认存在；禁止凭记忆生成文献列表）
        assert!(writ
            .instructions
            .as_deref()
            .unwrap()
            .contains("Mandatory literature verification workflow"));
        assert!(writ
            .instructions
            .as_deref()
            .unwrap()
            .contains("reference list\" from memory"));
        assert!(writ
            .instructions
            .as_deref()
            .unwrap()
            .contains("URLs actually returned"));
        // 论文真实性评分技能（用户要求：数据/内容合理性 + 仅一层引用核验 +
        // 0-100 综合分，严禁递归查引用）
        let paper = reg
            .get("paper-authenticity-review")
            .expect("paper authenticity skill");
        assert!(paper.invocation.model_invocable);
        let ins = paper.instructions.as_deref().unwrap();
        assert!(ins.contains("Composite Score (0-100; 0 = fabricated)"));
        assert!(paper
            .description
            .as_deref()
            .unwrap()
            .contains("0-100 scale"));
        assert!(ins.contains("DEPTH LIMIT"));
        assert!(ins.contains("exactly one level"));
        assert!(ins.contains("BUDGET LIMIT"));
        assert!(ins.contains("at most 25 web_search"));
        assert!(ins.contains("0 = fabricated"));
        assert!(ins.contains("Unverifiable does not mean nonexistent"));
        // 保密与再训练禁令（用户要求：论文核心信息不得用于 AI 再训练）
        assert!(ins.contains("MUST NOT be used for AI retraining"));
        assert!(ins.contains("Outbound minimization"));
    }

    /// 用户目录同名技能覆盖内置（rank 250 < 600）。
    #[test]
    fn user_dir_overrides_builtin() {
        let dir = tempfile::tempdir().unwrap();
        let sk = dir.path().join("chemistry-research");
        std::fs::create_dir_all(&sk).unwrap();
        std::fs::write(
            sk.join("skill.md"),
            "---\nname: chemistry-research\ndescription: 自定义版\n---\n我的自定义指令",
        )
        .unwrap();
        let mut reg = SkillRegistry::default();
        reg.load_from_dir(dir.path());
        let got = reg.get("chemistry-research").unwrap();
        assert_eq!(got.description.as_deref(), Some("自定义版"));
        assert!(got
            .instructions
            .as_deref()
            .unwrap()
            .contains("我的自定义指令"));
    }
}
