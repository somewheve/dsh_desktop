//! 公共工具：字体加载、日志文件、路径辅助。

use std::path::PathBuf;

/// 应用数据目录（日志等）。
pub fn app_data_dir() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("dsh-desktop")
}

/// 日志文件路径。
pub fn log_file_path() -> PathBuf {
    app_data_dir().join("logs").join("dsh-desktop.log")
}

/// 初始化日志：stderr + 文件（追加）。
pub fn init_logging() {
    let file = log_file_path();
    if let Some(parent) = file.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let file_writer = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&file);
    let mut builder =
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"));
    builder.format_timestamp_millis();
    if let Ok(w) = file_writer {
        let file_log: Box<dyn std::io::Write + Send> = Box::new(LogFile(std::sync::Mutex::new(w)));
        builder.target(env_logger::Target::Pipe(file_log));
    }
    let _ = builder.try_init();
    log::info!("dsh-desktop logging initialized (file: {})", file.display());
}

/// 找到系统里可用的等宽字体字节（用于终端渲染）。返回 (字体名, 字节)。
pub fn load_mono_font() -> Option<(String, Vec<u8>)> {
    let candidates = [
        ("CascadiaMono", "C:\\Windows\\Fonts\\CascadiaMono.ttf"),
        ("Consolas", "C:\\Windows\\Fonts\\consola.ttf"),
        ("CourierNew", "C:\\Windows\\Fonts\\cour.ttf"),
    ];
    for (name, path) in candidates {
        if let Ok(bytes) = std::fs::read(path) {
            log::info!("mono font loaded: {name} ({path})");
            return Some((name.to_string(), bytes));
        }
    }
    None
}

/// 找到系统里可用的 CJK 字体字节（中文字符回退）。
pub fn load_cjk_font() -> Option<(String, Vec<u8>)> {
    let candidates = [
        ("MicrosoftYaHei", "C:\\Windows\\Fonts\\msyh.ttc"),
        ("SimHei", "C:\\Windows\\Fonts\\simhei.ttf"),
        ("SimSun", "C:\\Windows\\Fonts\\simsun.ttc"),
    ];
    for (name, path) in candidates {
        if let Ok(bytes) = std::fs::read(path) {
            log::info!("cjk font loaded: {name} ({path})");
            return Some((name.to_string(), bytes));
        }
    }
    None
}

/// 找到系统里可用的**粗体**字体字节（Markdown 加粗渲染）。
/// egui 默认字体没有粗体变体（strong 只是颜色增强），所以加载一个
/// 真正的粗体字形族（微软雅黑 Bold / 黑体），注册为独立字体族。
pub fn load_bold_font() -> Option<(String, Vec<u8>)> {
    let candidates = [
        ("MicrosoftYaHeiBold", "C:\\Windows\\Fonts\\msyhbd.ttc"),
        ("SimHei", "C:\\Windows\\Fonts\\simhei.ttf"),
    ];
    for (name, path) in candidates {
        if let Ok(bytes) = std::fs::read(path) {
            log::info!("bold font loaded: {name} ({path})");
            return Some((name.to_string(), bytes));
        }
    }
    None
}

/// 找到系统里的 emoji 字体字节（Segoe UI Emoji）。
/// 必须作为字体家族的最后兜底：egui 默认 emoji 子集缺失的字形
/// （如 📋 U+1F4CB 渲染成"口"字 tofu）由它补齐。
pub fn load_emoji_font() -> Option<(String, Vec<u8>)> {
    let candidates = [
        ("SegoeUIEmoji", "C:\\Windows\\Fonts\\seguiemj.ttf"),
        ("SegoeUIEmoji", "C:\\Windows\\Fonts\\seguiemj.ttc"),
    ];
    for (name, path) in candidates {
        if let Ok(bytes) = std::fs::read(path) {
            log::info!("emoji font loaded: {name} ({path})");
            return Some((name.to_string(), bytes));
        }
    }
    None
}

/// Mutex<File> 的 Write 包装（env_logger Target::Pipe 需要 Box<dyn Write + Send>）。
struct LogFile(std::sync::Mutex<std::fs::File>);

impl std::io::Write for LogFile {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.0.lock().unwrap().flush()
    }
}

/// 简易 id（时间 + 熵）。
pub fn simple_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let ptr = &simple_id as *const _ as usize as u64;
    format!("{nanos:x}{ptr:x}")
}

/// Windows 上静默启动子进程：设置 CREATE_NO_WINDOW，避免 GUI 应用
/// spawn 控制台程序（cmd/powershell/dsh 等）时弹出黑色终端窗口。
pub fn hide_console(cmd: &mut std::process::Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    #[cfg(not(windows))]
    {
        let _ = cmd;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 回归：📋 (U+1F4CB) 等 emoji 必须由 Segoe UI Emoji 渲染，
    /// 而不是 egui 内置 NotoEmoji（0.81 缩小的单色字形在 12px 下形似"口"字）。
    /// egui 内置 NotoEmoji 也含 📋，所以必须把 seguiemj 插到它**之前**；
    /// 验证方式：插到主字体后（index 1）布局 📋，度量必须与默认配置不同。
    #[test]
    fn emoji_glyph_resolves_via_fallback_font() {
        let _ = env_logger::builder()
            .filter_level(log::LevelFilter::Warn)
            .try_init();
        let fid = egui::FontId::proportional(14.0);
        let layout_clip = |defs: &egui::FontDefinitions| {
            let mut fonts = egui::epaint::text::Fonts::new(
                egui::epaint::text::TextOptions::default(),
                defs.clone(),
            );
            let mut v = fonts.with_pixels_per_point(1.0);
            let g = v.layout_no_wrap("📋".to_string(), fid.clone(), egui::Color32::WHITE);
            let gg = &g.rows[0].glyphs[0];
            (
                gg.advance_width,
                gg.font_ascent,
                gg.font_height,
                gg.uv_rect.min,
                gg.uv_rect.max,
            )
        };
        // 基线：默认字体（NotoEmoji 渲染 📋，scale 0.81）
        let base = layout_clip(&egui::FontDefinitions::default());
        eprintln!("base 📋 metrics: {base:?}");
        // 修复配置：seguiemj 插到主字体后（index 1，NotoEmoji 之前）
        let (name, bytes) = load_emoji_font().expect("本机应有 Segoe UI Emoji");
        let mut defs = egui::FontDefinitions::default();
        defs.font_data
            .insert(name.clone(), egui::FontData::from_owned(bytes).into());
        for family in defs.families.values_mut() {
            if family.is_empty() {
                family.push(name.clone());
            } else {
                family.insert(1, name.clone());
            }
        }
        let fixed = layout_clip(&defs);
        eprintln!("fixed 📋 metrics: {fixed:?}");
        // 家族顺序必须是主字体之后第二位（NotoEmoji 之前）
        let prop = defs
            .families
            .get(&egui::FontFamily::Proportional)
            .expect("Proportional family");
        let segoe_pos = prop
            .iter()
            .position(|n| n == &name)
            .expect("segoe 在家族中");
        assert_eq!(
            segoe_pos, 1,
            "Segoe UI Emoji 必须插到主字体之后（NotoEmoji 之前）"
        );
        // 度量必须变化：seguiemj 的 📋 更大更宽（NotoEmoji 0.81 缩小）
        assert_ne!(
            base, fixed,
            "插入 seguiemj 后 📋 的字形度量必须变化（否则仍由 NotoEmoji 渲染 → 口字）"
        );
        assert!(
            fixed.0 > base.0,
            "seguiemj 的 📋 应更宽（当前 base={:.2} fixed={:.2}）",
            base.0,
            fixed.0
        );
    }
}
