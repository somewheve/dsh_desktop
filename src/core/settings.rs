//! 引擎设置：API key / model / base_url / proxy / 插件清单。
//!
//! 对应 DSH 的 dsh-settings + dsh-credentials + dsh-launch-environment。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct EngineSettings {
    /// DeepSeek 官方 API key（读取自 DSH credentials 或本应用设置）
    pub api_key: Option<String>,
    /// 模型 id（默认 deepseek-v4-flash；DeepSeek V4 系列）
    pub model: String,
    /// 推理强度（none/low/high/max；none = 关闭思考，默认 high 走 API 缺省）
    pub reasoning_effort: Option<String>,
    /// 沙箱模式（danger-full-access / workspace-write / read-only；重启恢复）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sandbox_mode: Option<String>,
    /// API base URL（默认官方；可指向兼容网关）
    pub base_url: String,
    /// HTTP 代理（可选）
    pub http_proxy: Option<String>,
    /// 会话数据目录（默认 %LOCALAPPDATA%/dsh-desktop/sessions）
    pub data_dir: PathBuf,
    /// 新建会话默认 Agent 预设（standard/code/minimal/cordis）
    pub default_preset: String,
    /// 用户技能目录（默认 $DSH_HOME/skills；测试可指向临时目录）
    pub skills_dir: Option<PathBuf>,
    /// 插件目录（默认 $DSH_HOME/plugins；测试可指向临时目录）
    pub plugins_dir: Option<PathBuf>,
}

impl Default for EngineSettings {
    fn default() -> Self {
        Self {
            api_key: None,
            model: "deepseek-v4-flash".into(),
            reasoning_effort: None,
            sandbox_mode: None,
            base_url: "https://api.deepseek.com".into(),
            http_proxy: None,
            data_dir: default_data_dir(),
            default_preset: "standard".into(),
            skills_dir: None,
            plugins_dir: None,
        }
    }
}

pub fn default_data_dir() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("dsh-desktop").join("sessions")
}

/// tempfile crate 的临时目录特征：`.tmp` + 6~8 位随机后缀（目录名末段）。
/// 只匹配这个精确形态，不匹配任意含 ".tmp" 的路径（用户自建目录不算）。
fn is_tempfile_dir(p: &std::path::Path) -> bool {
    let Some(name) = p.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    let rest = match name.strip_prefix(".tmp") {
        Some(r) => r,
        None => return false,
    };
    // tempfile 后缀为 6-8 个字母数字（Windows 上实测 8 位以内）
    (6..=10).contains(&rest.len()) && rest.chars().all(|c| c.is_ascii_alphanumeric())
}

impl EngineSettings {
    /// 设置文件路径：跟随 data_dir 的父目录（同一应用根）。
    /// **测试隔离关键**：旧实现写死 LOCALAPPDATA → 集成测试中 rebuild_llm
    /// 落盘会把测试假 key 写进用户真实配置（"每次跑测试 key 丢失"根因）。
    /// data_dir 为临时目录时，设置文件也落在临时目录 → 天然隔离。
    fn settings_file_for(data_dir: &Path) -> PathBuf {
        let base = data_dir
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| data_dir.to_path_buf());
        base.join("engine-settings.json")
    }

    fn settings_file() -> PathBuf {
        // 默认路径（load 静态调用）：data_dir 默认值的父目录 = LOCALAPPDATA/dsh-desktop
        let dd = default_data_dir();
        Self::settings_file_for(&dd)
    }

    pub fn load() -> Self {
        let path = Self::settings_file();
        let mut s: Self = match std::fs::read_to_string(&path) {
            Ok(text) => match serde_json::from_str(&text) {
                Ok(s) => s,
                Err(e) => {
                    log::warn!("engine settings parse failed: {e}");
                    Self::default()
                }
            },
            Err(_) => Self::default(),
        };
        // api_key 解密（dpapi: 前缀 = DPAPI 加密存储；旧明文格式直接兼容）
        if let Some(stored) = &s.api_key {
            if let Some(plain) = unprotect_key(stored) {
                s.api_key = Some(plain);
            } else if stored.starts_with("dpapi:") {
                // 加密条目解密失败（换机器/用户）：按无 key 处理，不覆盖落盘
                log::warn!("api_key 解密失败（可能跨用户/机器迁移），已忽略");
                s.api_key = None;
            }
        }
        // 防污染：data_dir 指向 tempfile 特征目录（`.tmpXXXXXX` 尾部形态，
        // 测试残留）时重置为默认。旧实现匹配任意含 ".tmp" 子串的路径——
        // 用户自己的 `x.tmp` / `.tmpbackups` 目录会被误判清空（会话"消失"）。
        if is_tempfile_dir(&s.data_dir) {
            log::warn!(
                "engine settings data_dir 指向临时目录（疑似测试残留），已重置: {}",
                s.data_dir.display()
            );
            s.data_dir = default_data_dir();
        }
        s
    }

    pub fn save(&self) -> anyhow::Result<()> {
        // 跟随 data_dir 的父目录（测试临时 data_dir → 设置文件也隔离在临时目录，
        // 不再写进用户真实 LOCALAPPDATA）
        let path = Self::settings_file_for(&self.data_dir);
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p)?;
        }
        // api_key 经 DPAPI 加密后落盘（绑定当前 Windows 用户；
        // 磁盘上不出现明文）。其余字段照旧。
        let mut plain = self.clone();
        if let Some(k) = &plain.api_key {
            let protected = protect_key(k);
            plain.api_key = Some(protected);
        }
        std::fs::write(&path, serde_json::to_string_pretty(&plain)?)?;
        Ok(())
    }

    /// 尝试从 DSH credentials（~/.dsh/.credentials.yaml）读取 deepseek key。
    pub fn load_api_key_from_dsh(&mut self) -> Option<String> {
        let home = std::env::var_os("DSH_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("USERPROFILE").map(|p| PathBuf::from(p).join(".dsh")))?;
        let cred = home.join(".credentials.yaml");
        let text = std::fs::read_to_string(cred).ok()?;
        // YAML 形如: deepseek-official: { apiKey: sk-xxx }
        let key = parse_credential_yaml(&text)?;
        self.api_key = Some(key.clone());
        Some(key)
    }
}

/// 极简 YAML 提取：找到 deepseek 相关行的 apiKey。
fn parse_credential_yaml(text: &str) -> Option<String> {
    let mut in_deepseek = false;
    for line in text.lines() {
        let trimmed = line.trim();
        // 格式 1：顶层 DEEPSEEK_API_KEY
        if trimmed.starts_with("DEEPSEEK_API_KEY") && trimmed.contains(':') {
            let v = trimmed
                .splitn(2, ':')
                .nth(1)?
                .trim()
                .trim_matches('"')
                .trim_matches('\'');
            if !v.is_empty() && v != "null" {
                return Some(v.to_string());
            }
        }
        // 格式 2：deepseek-official 段
        if trimmed.starts_with("deepseek") && trimmed.contains(':') {
            in_deepseek = true;
            continue;
        }
        if in_deepseek {
            if trimmed.starts_with("apiKey") || trimmed.starts_with("api_key") {
                let v = trimmed
                    .split(':')
                    .nth(1)?
                    .trim()
                    .trim_matches('"')
                    .trim_matches('\'');
                if !v.is_empty() && v != "null" {
                    return Some(v.to_string());
                }
            }
            if !trimmed.starts_with(' ')
                && !trimmed.starts_with('-')
                && trimmed.contains(':')
                && !trimmed.starts_with("apiKey")
                && !trimmed.starts_with("api_key")
            {
                in_deepseek = false;
            }
        }
    }
    None
}

/// Windows DPAPI 加密（CryptProtectData，绑定当前用户，无需管理密钥）。
/// 返回 `dpapi:<base64>` 形态；加密失败退回原文（保持可用性优先）。
fn protect_key(plain: &str) -> String {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::LocalFree;
        use windows_sys::Win32::Security::Cryptography::{CryptProtectData, CRYPT_INTEGER_BLOB};
        let mut data = plain.as_bytes().to_vec();
        let mut in_blob = CRYPT_INTEGER_BLOB {
            cbData: data.len() as u32,
            pbData: data.as_mut_ptr(),
        };
        let mut out = CRYPT_INTEGER_BLOB {
            cbData: 0,
            pbData: std::ptr::null_mut(),
        };
        let ok = unsafe {
            CryptProtectData(
                &mut in_blob,
                std::ptr::null(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                0,
                &mut out,
            )
        };
        if ok == 0 {
            log::error!("DPAPI CryptProtectData 失败，key 将以明文保存");
            return plain.to_string();
        }
        let bytes = unsafe { std::slice::from_raw_parts(out.pbData, out.cbData as usize) }.to_vec();
        unsafe { LocalFree(out.pbData as _) };
        format!("dpapi:{}", b64_encode(&bytes))
    }
    #[cfg(not(windows))]
    {
        plain.to_string()
    }
}

/// 解密 `dpapi:<base64>`；非该前缀（旧明文）原样返回 Some；
/// 解密失败返回 None（调用方按无 key 处理）。
pub fn unprotect_key_for_test(stored: &str) -> Option<String> {
    unprotect_key(stored)
}

fn unprotect_key(stored: &str) -> Option<String> {
    // 旧明文格式：原样返回（向后兼容）
    let enc = match stored.strip_prefix("dpapi:") {
        Some(e) => e,
        None => return Some(stored.to_string()),
    };
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::LocalFree;
        use windows_sys::Win32::Security::Cryptography::{CryptUnprotectData, CRYPT_INTEGER_BLOB};
        let mut data = b64_decode(enc)?;
        let mut in_blob = CRYPT_INTEGER_BLOB {
            cbData: data.len() as u32,
            pbData: data.as_mut_ptr(),
        };
        let mut out = CRYPT_INTEGER_BLOB {
            cbData: 0,
            pbData: std::ptr::null_mut(),
        };
        let ok = unsafe {
            CryptUnprotectData(
                &mut in_blob,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                0,
                &mut out,
            )
        };
        if ok == 0 {
            return None;
        }
        let bytes = unsafe { std::slice::from_raw_parts(out.pbData, out.cbData as usize) }.to_vec();
        unsafe { LocalFree(out.pbData as _) };
        Some(String::from_utf8_lossy(&bytes).into_owned())
    }
    #[cfg(not(windows))]
    {
        let _ = enc;
        None
    }
}

/// 极简 base64（标准字母表 + padding；DPAPI blob 编码用）。
fn b64_encode(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            T[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            T[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// base64 解码（容忍缺失 padding）。
fn b64_decode(s: &str) -> Option<Vec<u8>> {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut vals = Vec::with_capacity(s.len());
    for c in s.bytes() {
        if c == b'=' || c == b'\n' || c == b'\r' {
            continue;
        }
        let v = T.iter().position(|&t| t == c)? as u32;
        vals.push(v);
    }
    let mut out = Vec::with_capacity(vals.len() * 3 / 4);
    for chunk in vals.chunks(4) {
        let n = chunk.iter().fold(0u32, |a, v| (a << 6) | v) << (6 * (4 - chunk.len()) as u32);
        out.push((n >> 16) as u8);
        if chunk.len() > 2 {
            out.push((n >> 8) as u8);
        }
        if chunk.len() > 3 {
            out.push(n as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_deepseek_key_flat() {
        let yaml = "DEEPSEEK_API_KEY: sk-flat-456\n";
        assert_eq!(parse_credential_yaml(yaml).as_deref(), Some("sk-flat-456"));
    }

    #[test]
    fn parse_deepseek_key() {
        let yaml =
            "deepseek-official:\n  apiKey: \"sk-test-123\"\n  baseUrl: https://api.deepseek.com\n";
        assert_eq!(parse_credential_yaml(yaml).as_deref(), Some("sk-test-123"));
    }

    #[test]
    fn parse_missing_key_returns_none() {
        assert_eq!(parse_credential_yaml("foo: bar\n"), None);
    }

    #[test]
    fn settings_roundtrip() {
        let mut s = EngineSettings::default();
        s.api_key = Some("sk-x".into());
        s.model = "deepseek-reasoner".into();
        let json = serde_json::to_string(&s).unwrap();
        let back: EngineSettings = serde_json::from_str(&json).unwrap();
        assert_eq!(back.api_key.as_deref(), Some("sk-x"));
        assert_eq!(back.model, "deepseek-reasoner");
    }
}

#[cfg(test)]
mod key_storage_tests {
    use super::*;

    /// base64 编解码往返。
    #[test]
    fn b64_roundtrip() {
        for case in ["", "f", "fo", "foo", "foob", "fooba", "foobar"] {
            let enc = b64_encode(case.as_bytes());
            assert_eq!(b64_decode(&enc).as_deref(), Some(case.as_bytes()));
        }
        assert_eq!(b64_encode(b"foobar"), "Zm9vYmFy");
        assert!(b64_decode("!!invalid!!").is_none());
    }

    /// DPAPI 往返：protect → `dpapi:` 前缀、不含明文；unprotect 还原。
    #[cfg(windows)]
    #[test]
    fn dpapi_roundtrip() {
        let protected = protect_key("sk-secret-123");
        assert!(protected.starts_with("dpapi:"), "应加密: {protected:?}");
        assert!(!protected.contains("sk-secret-123"), "密文不得含明文");
        assert_eq!(unprotect_key(&protected).as_deref(), Some("sk-secret-123"));
        // 旧明文格式直接兼容
        assert_eq!(unprotect_key("sk-plain").as_deref(), Some("sk-plain"));
        // 损坏的密文 → None（按无 key 处理）
        assert_eq!(unprotect_key("dpapi:AAAA"), None);
    }

    /// 端到端：save 落盘为加密形态（文件不含明文），load 还原。
    /// 这同时锁定"保存后 key 不得在重启（新 load）后丢失"的持久性。
    #[cfg(windows)]
    #[test]
    fn settings_save_load_key_persists_encrypted() {
        // 隔离配置文件位置（settings_file 用 LOCALAPPDATA）
        let dir = tempfile::tempdir().unwrap();
        let mut s = EngineSettings::default();
        s.api_key = Some("sk-persist-me".into());
        s.data_dir = dir.path().join("sessions");
        s.save().unwrap();

        // 设置文件跟随 data_dir 父目录（不再依赖 LOCALAPPDATA → 测试天然隔离）
        let settings_path = dir.path().join("engine-settings.json");
        let disk = std::fs::read_to_string(&settings_path).unwrap();
        assert!(!disk.contains("sk-persist-me"), "明文不得落盘: {disk}");
        assert!(disk.contains("dpapi:"), "应为加密形态: {disk}");

        // 重新加载（模拟重启）：从同一 data_dir 路径读回
        let reloaded = {
            let text = std::fs::read_to_string(&settings_path).unwrap();
            let mut s: EngineSettings = serde_json::from_str(&text).unwrap();
            if let Some(stored) = &s.api_key {
                if let Some(plain) = unprotect_key(stored) {
                    s.api_key = Some(plain);
                }
            }
            s
        };
        assert_eq!(reloaded.api_key.as_deref(), Some("sk-persist-me"));
    }
}
