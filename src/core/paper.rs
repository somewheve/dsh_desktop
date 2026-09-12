//! 论文搜索：多学术源聚合（arXiv / Crossref / PubMed / OpenAlex /
//! Semantic Scholar），全部走免 key 公共 API。作为"内置扩展"提供：
//! 可在扩展页配置启停各源/条数/超时，可整体卸载。
//!
//! 并发扇出：std::thread::scope 并行请求各源（单源超时不拖累整体），
//! 逐源带回 Ok/Err —— 部分源不可达（无外联/被墙）不影响其余源。

use serde::{Deserialize, Serialize};
use std::time::Duration;

/// 单条论文结果（各源归一化）。
#[derive(Debug, Clone, Serialize)]
pub struct PaperHit {
    pub title: String,
    pub authors: Vec<String>,
    pub year: Option<u32>,
    pub venue: Option<String>,
    pub doi: Option<String>,
    pub url: Option<String>,
    pub citations: Option<u64>,
    /// 摘要片段（前 240 字符；arXiv/S2 有摘要，Crossref/PubMed 无）
    pub snippet: Option<String>,
    pub source: &'static str,
}

/// 源标识（稳定字符串，配置持久化用）。
pub const SOURCES: &[(&str, &str)] = &[
    ("arxiv", "arXiv 预印本"),
    ("crossref", "Crossref DOI 元数据"),
    ("pubmed", "PubMed 生物医学"),
    ("openalex", "OpenAlex 全学科"),
    ("semanticscholar", "Semantic Scholar 引用图"),
];

/// 内置扩展配置（EngineSettings 持久化）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PaperSearchConfig {
    /// 扩展总开关（卸载 = false 且 uninstalled = true）
    pub enabled: bool,
    /// 已卸载（工具从注册表移除；恢复按钮可装回）
    #[serde(default)]
    pub uninstalled: bool,
    /// 各源开关（源名 → 启用）
    pub sources: Vec<(String, bool)>,
    /// 每源最大条数
    pub max_per_source: usize,
    /// 单源 HTTP 超时（秒）
    pub timeout_secs: u64,
}

impl Default for PaperSearchConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            uninstalled: false,
            sources: SOURCES.iter().map(|(k, _)| (k.to_string(), true)).collect(),
            max_per_source: 5,
            timeout_secs: 12,
        }
    }
}

impl PaperSearchConfig {
    pub fn source_enabled(&self, name: &str) -> bool {
        self.enabled
            && !self.uninstalled
            && self
                .sources
                .iter()
                .any(|(k, on)| k == name && *on)
    }

    pub fn set_source(&mut self, name: &str, on: bool) {
        if let Some(e) = self.sources.iter_mut().find(|(k, _)| k == name) {
            e.1 = on;
        }
    }

    pub fn any_source_on(&self) -> bool {
        self.enabled && !self.uninstalled && self.sources.iter().any(|(_, on)| *on)
    }
}

/// 聚合搜索：并发扇出全部启用源；返回 (hits, per-source errors)。
pub fn search(
    query: &str,
    cfg: &PaperSearchConfig,
    proxy: Option<&str>,
) -> (Vec<PaperHit>, Vec<(String, String)>) {
    let mut hits = Vec::new();
    let mut errors = Vec::new();
    let n = cfg.max_per_source.clamp(1, 20);
    let timeout = Duration::from_secs(cfg.timeout_secs.clamp(3, 60));

    let jobs: Vec<&str> = SOURCES
        .iter()
        .map(|(k, _)| *k)
        .filter(|k| cfg.source_enabled(k))
        .collect();
    if jobs.is_empty() {
        return (hits, vec![("config".into(), "无启用的论文源".into())]);
    }

    // 共享一个 HTTP 客户端（连接池复用；此前每源每查各建一个）
    let client = match http_client(timeout, proxy) {
        Ok(c) => c,
        Err(e) => return (hits, vec![("client".into(), e)]),
    };
    std::thread::scope(|scope| {
        let mut handles = Vec::new();
        for src in jobs {
            let q = query.to_string();
            let c = &client;
            handles.push((src, scope.spawn(move || run_source(c, src, &q, n))));
        }
        for (src, h) in handles {
            match h.join() {
                Ok(Ok(mut v)) => hits.append(&mut v),
                Ok(Err(e)) => errors.push((src.to_string(), e)),
                Err(_) => errors.push((src.to_string(), "provider thread panicked".into())),
            }
        }
    });

    // 去重：同 DOI/URL 只留一条，且**保留信息最丰富的记录**（有摘要、
    // 有引用数者优先）——此前只按 key 排序，线程完成顺序不定导致
    // "同论文哪个源存活"随机（注释承诺的优先级从未生效）
    let richness = |h: &PaperHit| {
        (h.snippet.is_some() as u8) * 2 + (h.citations.is_some() as u8)
    };
    hits.sort_by(|a, b| {
        let ka = a.doi.clone().or_else(|| a.url.clone()).unwrap_or_default();
        let kb = b.doi.clone().or_else(|| b.url.clone()).unwrap_or_default();
        // 同 key 时丰富度降序 → dedup 保留的首条即最丰富者
        ka.cmp(&kb).then(richness(b).cmp(&richness(a)))
    });
    hits.dedup_by(|a, b| {
        let ka = a.doi.clone().or_else(|| a.url.clone()).unwrap_or_default();
        let kb = b.doi.clone().or_else(|| b.url.clone()).unwrap_or_default();
        !ka.is_empty() && ka == kb
    });
    (hits, errors)
}

fn http_client(
    timeout: Duration,
    proxy: Option<&str>,
) -> Result<reqwest::blocking::Client, String> {
    let mut b = reqwest::blocking::Client::builder()
        .user_agent("dsh-desktop-paper-search/0.1 (mailto:dsh-desktop.local)")
        .timeout(timeout)
        .connect_timeout(Duration::from_secs(6));
    if let Some(p) = proxy {
        b = b.proxy(reqwest::Proxy::all(p).map_err(|e| format!("proxy: {e}"))?);
    }
    b.build().map_err(|e| e.to_string())
}

fn urlencode(s: &str) -> String {
    let mut out = String::new();
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn run_source(
    c: &reqwest::blocking::Client,
    src: &str,
    q: &str,
    n: usize,
) -> Result<Vec<PaperHit>, String> {
    match src {
        "arxiv" => arxiv(c, q, n),
        "crossref" => crossref(c, q, n),
        "pubmed" => pubmed(c, q, n),
        "openalex" => openalex(c, q, n),
        "semanticscholar" => semanticscholar(c, q, n),
        other => Err(format!("unknown source {other}")),
    }
}

// ===== arXiv（Atom XML，免 key）=====
fn arxiv(c: &reqwest::blocking::Client, q: &str, n: usize) -> Result<Vec<PaperHit>, String> {
    let url = format!(
        "https://export.arxiv.org/api/query?search_query=all:{}&max_results={n}",
        urlencode(q)
    );
    let xml = c.get(&url).send().map_err(|e| e.to_string())?.text().map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    for entry in xml.split("<entry>").skip(1) {
        let title = xml_tag(entry, "title").unwrap_or_default();
        if title.is_empty() {
            continue;
        }
        let authors: Vec<String> = entry
            .split("<author>")
            .skip(1)
            .filter_map(|a| xml_tag(a, "name"))
            .collect();
        let year = xml_tag(entry, "published")
            .and_then(|p| p.get(0..4).and_then(|y| y.parse().ok()));
        let url = xml_tag(entry, "id");
        let snippet = xml_tag(entry, "summary").map(|s| {
            let t: String = s.chars().take(240).collect();
            t
        });
        out.push(PaperHit {
            title,
            authors,
            year,
            venue: Some("arXiv".into()),
            doi: None,
            url,
            citations: None,
            snippet,
            source: "arxiv",
        });
        if out.len() >= n {
            break;
        }
    }
    if out.is_empty() {
        return Err("arXiv 无结果/解析失败".into());
    }
    Ok(out)
}

/// Atom 简易标签提取：`<tag>text</tag>`（首个，去掉 CDATA 与空白）。
fn xml_tag(s: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}");
    let start = s.find(&open)?;
    let gt = s[start..].find('>')? + start + 1;
    let close = format!("</{tag}>");
    let end = gt + s[gt..].find(&close)?;
    let mut inner = s[gt..end].to_string();
    if let Some(stripped) = inner
        .strip_prefix("<![CDATA[")
        .and_then(|x| x.strip_suffix("]]>"))
    {
        inner = stripped.to_string();
    }
    let t = inner.split_whitespace().collect::<Vec<_>>().join(" ");
    if t.is_empty() {
        None
    } else {
        Some(t)
    }
}

// ===== Crossref（JSON，免 key；polite pool 走 UA mailto）=====
fn crossref(c: &reqwest::blocking::Client, q: &str, n: usize) -> Result<Vec<PaperHit>, String> {
    let url = format!(
        "https://api.crossref.org/works?query={}&rows={n}&select=DOI,title,author,issued,container-title,is-referenced-by-count",
        urlencode(q)
    );
    let v: serde_json::Value = c
        .get(&url)
        .send()
        .map_err(|e| e.to_string())?
        .json()
        .map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    if let Some(items) = v.pointer("/message/items").and_then(|x| x.as_array()) {
        for it in items {
            let title = it
                .get("title")
                .and_then(|t| t.as_array())
                .and_then(|a| a.first())
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_string();
            if title.is_empty() {
                continue;
            }
            let authors = it
                .get("author")
                .and_then(|a| a.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|p| {
                            let f = p.get("given").and_then(|x| x.as_str()).unwrap_or("");
                            let l = p.get("family").and_then(|x| x.as_str()).unwrap_or("");
                            let n = format!("{f} {l}").trim().to_string();
                            (!n.is_empty()).then_some(n)
                        })
                        .collect()
                })
                .unwrap_or_default();
            let year = it
                .pointer("/issued/date-parts/0/0")
                .and_then(|x| x.as_u64())
                .and_then(|y| u32::try_from(y).ok());
            let venue = it
                .get("container-title")
                .and_then(|t| t.as_array())
                .and_then(|a| a.first())
                .and_then(|x| x.as_str())
                .map(String::from);
            let doi = it.get("DOI").and_then(|x| x.as_str()).map(String::from);
            let citations = it.get("is-referenced-by-count").and_then(|x| x.as_u64());
            let url = doi
                .as_ref()
                .map(|d| format!("https://doi.org/{d}"));
            out.push(PaperHit {
                title,
                authors,
                year,
                venue,
                doi,
                url,
                citations,
                snippet: None,
                source: "crossref",
            });
            if out.len() >= n {
                break;
            }
        }
    }
    if out.is_empty() {
        return Err("Crossref 无结果".into());
    }
    Ok(out)
}

// ===== PubMed（esearch → esummary，免 key）=====
fn pubmed(c: &reqwest::blocking::Client, q: &str, n: usize) -> Result<Vec<PaperHit>, String> {
    let search_url = format!(
        "https://eutils.ncbi.nlm.nih.gov/entrez/eutils/esearch.fcgi?db=pubmed&retmode=json&retmax={n}&term={}",
        urlencode(q)
    );
    let sv: serde_json::Value = c
        .get(&search_url)
        .send()
        .map_err(|e| e.to_string())?
        .json()
        .map_err(|e| e.to_string())?;
    let ids: Vec<String> = sv
        .pointer("/esearchresult/idlist")
        .and_then(|x| x.as_array())
        .map(|a| a.iter().filter_map(|i| i.as_str().map(String::from)).collect())
        .unwrap_or_default();
    if ids.is_empty() {
        return Err("PubMed 无结果".into());
    }
    let sum_url = format!(
        "https://eutils.ncbi.nlm.nih.gov/entrez/eutils/esummary.fcgi?db=pubmed&retmode=json&id={}",
        ids.join(",")
    );
    let uv: serde_json::Value = c
        .get(&sum_url)
        .send()
        .map_err(|e| e.to_string())?
        .json()
        .map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    for id in &ids {
        let Some(it) = uv.pointer(&format!("/result/{id}")) else {
            continue;
        };
        let title = it
            .get("title")
            .and_then(|x| x.as_str())
            .unwrap_or_default()
            .to_string();
        if title.is_empty() {
            continue;
        }
        let authors = it
            .get("authors")
            .and_then(|a| a.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|p| p.get("name").and_then(|x| x.as_str()).map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        let year = it
            .get("pubdate")
            .and_then(|x| x.as_str())
            .and_then(|s| s.get(0..4))
            .and_then(|y| y.trim().parse().ok());
        let venue = it
            .get("fulljournalname")
            .and_then(|x| x.as_str())
            .map(String::from);
        let doi = it
            .get("elocationid")
            .and_then(|x| x.as_str())
            .and_then(|s| s.strip_prefix("doi: "))
            .map(String::from);
        let url = Some(format!("https://pubmed.ncbi.nlm.nih.gov/{id}/"));
        out.push(PaperHit {
            title,
            authors,
            year,
            venue,
            doi,
            url,
            citations: None,
            snippet: None,
            source: "pubmed",
        });
        if out.len() >= n {
            break;
        }
    }
    if out.is_empty() {
        return Err("PubMed 摘要解析失败".into());
    }
    Ok(out)
}

// ===== OpenAlex（JSON，免 key）=====
fn openalex(c: &reqwest::blocking::Client, q: &str, n: usize) -> Result<Vec<PaperHit>, String> {
    let url = format!(
        "https://api.openalex.org/works?search={}&per_page={n}",
        urlencode(q)
    );
    let v: serde_json::Value = c
        .get(&url)
        .send()
        .map_err(|e| e.to_string())?
        .json()
        .map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    if let Some(items) = v.get("results").and_then(|x| x.as_array()) {
        for it in items {
            let title = it
                .get("display_name")
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_string();
            if title.is_empty() {
                continue;
            }
            let authors = it
                .get("authorships")
                .and_then(|a| a.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|x| {
                            x.pointer("/author/display_name")
                                .and_then(|n| n.as_str())
                                .map(String::from)
                        })
                        .collect()
                })
                .unwrap_or_default();
            let year = it.get("publication_year").and_then(|x| x.as_u64()).and_then(|y| u32::try_from(y).ok());
            let venue = it
                .pointer("/primary_location/source/display_name")
                .and_then(|x| x.as_str())
                .map(String::from);
            let doi = it
                .get("doi")
                .and_then(|x| x.as_str())
                .and_then(|d| d.trim_start_matches("https://doi.org/").to_string().into());
            let doi = if doi.as_deref().unwrap_or("").is_empty() {
                None
            } else {
                doi
            };
            let url2 = it
                .pointer("/primary_location/landing_page_url")
                .and_then(|x| x.as_str())
                .map(String::from)
                .or_else(|| doi.clone());
            let citations = it.get("cited_by_count").and_then(|x| x.as_u64());
            out.push(PaperHit {
                title,
                authors,
                year,
                venue,
                doi,
                url: url2,
                citations,
                snippet: None,
                source: "openalex",
            });
            if out.len() >= n {
                break;
            }
        }
    }
    if out.is_empty() {
        return Err("OpenAlex 无结果".into());
    }
    Ok(out)
}

// ===== Semantic Scholar（JSON，免 key 低频）=====
fn semanticscholar(
    c: &reqwest::blocking::Client,
    q: &str,
    n: usize,
) -> Result<Vec<PaperHit>, String> {
    let url = format!(
        "https://api.semanticscholar.org/graph/v1/paper/search?query={}&limit={n}&fields=title,authors,year,venue,externalIds,citationCount,abstract",
        urlencode(q)
    );
    let v: serde_json::Value = c
        .get(&url)
        .send()
        .map_err(|e| e.to_string())?
        .json()
        .map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    if let Some(items) = v.get("data").and_then(|x| x.as_array()) {
        for it in items {
            let title = it
                .get("title")
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_string();
            if title.is_empty() {
                continue;
            }
            let authors = it
                .get("authors")
                .and_then(|a| a.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|p| p.get("name").and_then(|x| x.as_str()).map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            let year = it.get("year").and_then(|x| x.as_u64()).and_then(|y| u32::try_from(y).ok());
            let venue = it.get("venue").and_then(|x| x.as_str()).map(String::from);
            let doi = it
                .pointer("/externalIds/DOI")
                .and_then(|x| x.as_str())
                .map(String::from);
            let url = doi
                .as_ref()
                .map(|d| format!("https://doi.org/{d}"))
                .or_else(|| {
                    it.get("paperId")
                        .and_then(|x| x.as_str())
                        .map(|p| format!("https://www.semanticscholar.org/paper/{p}"))
                });
            let citations = it.get("citationCount").and_then(|x| x.as_u64());
            let snippet = it.get("abstract").and_then(|x| x.as_str()).map(|s| {
                s.chars().take(240).collect()
            });
            out.push(PaperHit {
                title,
                authors,
                year,
                venue,
                doi,
                url,
                citations,
                snippet,
                source: "semanticscholar",
            });
            if out.len() >= n {
                break;
            }
        }
    }
    if out.is_empty() {
        return Err("Semantic Scholar 无结果（免费额度有限）".into());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_default_and_toggles() {
        let mut c = PaperSearchConfig::default();
        assert!(c.any_source_on());
        assert!(c.source_enabled("arxiv"));
        c.set_source("pubmed", false);
        assert!(!c.source_enabled("pubmed"));
        c.uninstalled = true;
        assert!(!c.any_source_on());
        // serde 往返（旧配置无 uninstalled 字段 → default）
        let json = r#"{"enabled":true,"sources":[["arxiv",true]],"max_per_source":3,"timeout_secs":8}"#;
        let c2: PaperSearchConfig = serde_json::from_str(json).unwrap();
        assert!(!c2.uninstalled && c2.max_per_source == 3);
    }

    #[test]
    fn arxiv_atom_parse() {
        let xml = r#"<?xml version="1.0"?><feed><entry><id>http://arxiv.org/abs/2401.00001</id><updated>2024-01-02T00:00:00Z</updated><published>2024-01-01T00:00:00Z</published><title>  A Study
  of Things </title><summary>Abstract text here</summary><author><name>Alice</name></author><author><name>Bob</name></author></entry></feed>"#;
        let hits = {
            // 直接测内部解析：借用 arxiv() 的解析逻辑抽出——用 xml_tag 手工跑
            let entry = xml.split("<entry>").nth(1).unwrap();
            vec![PaperHit {
                title: xml_tag(entry, "title").unwrap(),
                authors: entry
                    .split("<author>")
                    .skip(1)
                    .filter_map(|a| xml_tag(a, "name"))
                    .collect(),
                year: xml_tag(entry, "published")
                    .and_then(|p| p.get(0..4).and_then(|y| y.parse().ok())),
                venue: Some("arXiv".into()),
                doi: None,
                url: xml_tag(entry, "id"),
                citations: None,
                snippet: xml_tag(entry, "summary"),
                source: "arxiv",
            }]
        };
        assert_eq!(hits[0].title, "A Study of Things");
        assert_eq!(hits[0].authors, vec!["Alice", "Bob"]);
        assert_eq!(hits[0].year, Some(2024));
        assert_eq!(hits[0].url.as_deref(), Some("http://arxiv.org/abs/2401.00001"));
        assert!(hits[0].snippet.as_deref().unwrap().contains("Abstract"));
    }

    #[test]
    fn search_no_sources_returns_config_error() {
        let mut c = PaperSearchConfig::default();
        c.sources.clear();
        let (hits, errs) = search("test", &c, None);
        assert!(hits.is_empty());
        assert!(errs.iter().any(|(k, _)| k == "config"));
    }
}
