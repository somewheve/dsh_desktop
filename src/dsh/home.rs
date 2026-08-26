//! DSH 集成：DSH_HOME 发现、profiles 枚举、web 端口探测。

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use log::{debug, info, warn};

use crate::config::{default_dsh_home, AppConfig};

/// 一个 DSH profile 的元信息。
#[derive(Debug, Clone)]
pub struct ProfileInfo {
    pub name: String,
    pub dir: PathBuf,
    /// package.json 里的 dsh.profile.bundles（plugin 层栈）
    pub bundles: Vec<String>,
}

/// 枚举 $DSH_HOME/profiles 下所有 profile。
pub fn list_profiles(home: &Path) -> Result<Vec<ProfileInfo>> {
    let profiles_dir = home.join("profiles");
    if !profiles_dir.is_dir() {
        debug!("no profiles dir at {}", profiles_dir.display());
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in std::fs::read_dir(&profiles_dir).context("read profiles dir")? {
        let entry = entry?;
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        let name = dir
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string();
        if name.is_empty() || name.starts_with('.') {
            continue;
        }
        let manifest_path = dir.join("package.json");
        if !manifest_path.is_file() {
            debug!("profile {name} has no package.json; skipping");
            continue;
        }
        let bundles = match read_bundles(&manifest_path) {
            Ok(b) => b,
            Err(e) => {
                warn!("profile {name}: cannot read bundles: {e:#}");
                Vec::new()
            }
        };
        out.push(ProfileInfo { name, dir, bundles });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    info!(
        "found {} profiles under {}",
        out.len(),
        profiles_dir.display()
    );
    Ok(out)
}

/// 读取 package.json 里的 dsh.profile.bundles。
fn read_bundles(manifest_path: &Path) -> Result<Vec<String>> {
    let text = std::fs::read_to_string(manifest_path)?;
    let v: serde_json::Value = serde_json::from_str(&text)?;
    Ok(v["dsh"]["profile"]["bundles"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default())
}

/// 探测 DSH web 是否在跑（TCP 连接测试）。
pub fn probe_web(port: u16) -> bool {
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::time::Duration;
    match TcpStream::connect_timeout(
        &format!("127.0.0.1:{port}").parse().unwrap(),
        Duration::from_millis(500),
    ) {
        Ok(mut s) => {
            let _ = s.set_read_timeout(Some(Duration::from_millis(300)));
            let _ = s.write_all(b"GET / HTTP/1.0\r\n\r\n");
            let mut buf = [0u8; 64];
            let _ = s.read(&mut buf);
            true
        }
        Err(_) => false,
    }
}

/// Web 探测缓存：后台线程每 3 秒探测一次，UI 只读原子值（避免每帧 TCP 连接）。
pub struct WebProbe {
    up: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl WebProbe {
    pub fn start(port: u16) -> Self {
        let up = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(probe_web(port)));
        let up2 = up.clone();
        std::thread::Builder::new()
            .name("web-probe".into())
            .spawn(move || loop {
                let v = probe_web(port);
                up2.store(v, std::sync::atomic::Ordering::Relaxed);
                std::thread::sleep(std::time::Duration::from_secs(3));
            })
            .expect("spawn web-probe thread");
        Self { up }
    }

    pub fn is_up(&self) -> bool {
        self.up.load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// 获取当前可用的 profile 列表（给 UI 用）。
pub fn available_profiles(cfg: &AppConfig) -> Vec<ProfileInfo> {
    list_profiles(&cfg.dsh_home).unwrap_or_default()
}

/// 默认 DSH home（与 config 一致，便于日志）。
pub fn dsh_home_path() -> PathBuf {
    default_dsh_home()
}
