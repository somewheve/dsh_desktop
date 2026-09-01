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

/// 界面主字体 Consolas（Windows 系统自带：Regular + Bold + Italic + Bold
/// Italic 四风格；从系统字体目录读取，不内嵌分发）。缺失时返回 None →
/// 沿用默认/等宽回退链。
pub fn load_ui_font_consolas() -> Option<(String, Vec<u8>)> {
    let path = r"C:\Windows\Fonts\consola.ttf";
    match std::fs::read(path) {
        Ok(bytes) => {
            log::info!("ui font loaded: Consolas ({path})");
            Some(("Consolas".to_string(), bytes))
        }
        Err(_) => None,
    }
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
        ("ConsolasBold", "C:\\Windows\\Fonts\\consolab.ttf"),
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

/// 找到系统里的符号字体字节（Segoe UI Symbol）。
/// ⧉（U+29C9，复制图标）等符号字形在 Consolas / 雅黑 / Segoe Emoji 中
/// 都不存在，只有符号字体有——缺它就渲染成 tofu"口"。注册为家族最后
/// 兜底：排在主字体、emoji、CJK 之后，已有字形仍由前面的字体提供。
pub fn load_symbol_font() -> Option<(String, Vec<u8>)> {
    let candidates = [
        ("SegoeUISymbol", "C:\\Windows\\Fonts\\seguisym.ttf"),
        ("SegoeUISymbol", "C:\\Windows\\Fonts\\seguisym.ttc"),
    ];
    for (name, path) in candidates {
        if let Ok(bytes) = std::fs::read(path) {
            log::info!("symbol font loaded: {name} ({path})");
            return Some((name.to_string(), bytes));
        }
    }
    None
}

/// 字体度量探测字号（px）。越大，计算出的 FontTweak 比例越精确
/// （round_ui 按 1px 取整，256px 下相对误差 < 0.4%）。
const FONT_PROBE_SIZE: f32 = 256.0;

/// 用 egui 文本管线实测两个字体家族的度量，计算把 `fallback` 归一到
/// `primary` 行高与基线所需的 [`egui::FontTweak`]。
///
/// 背景：egui 对一行内每个字形按**它所属字体自身**的 ascent/row_height
/// 摆放基线，并取该行所有字体 row_height 的最大值作为行高。主字体
/// Consolas 行高 1.17em、ascent 0.74em，而微软雅黑行高 1.32em、ascent
/// 1.06em —— 不归一则中英混排时：
/// 1. 含中文的行比纯英文行高 ~13%，行距跳变；
/// 2. 两种字体 ascent 不同，同一行内中英文字形基线错位 ~0.2em，
///    中文视觉上明显偏高（"英文和中文字体不等高"）。
///
/// 归一原理：
/// - `scale` 等比缩放回退字体的 ascent/descent/行高/字形，
///   取 `主字体行高 / 回退字体行高`，行高即一致；
/// - `y_offset_factor = (主字体ascent − 回退ascent×scale) / (字号×scale)`，
///   把回退字形平移到主字体基线。两个系数都与字号成线性比，任意字号成立。
///
/// `probe` 必须是 primary 不含、fallback 覆盖的字符（中文回退用 `'中'`，
/// emoji 回退用 `'📋'`）。探测失败或结果超出常见字体度量比时返回
/// `None`（放弃归一，维持原渲染）。
pub fn font_tweak_to_match_primary(
    primary: &(String, Vec<u8>),
    fallback: &(String, Vec<u8>),
    probe: char,
) -> Option<egui::FontTweak> {
    // 与正式注册相同的家族顺序：primary 首位、fallback 第二位，
    // 保证 probe 字符确实由 fallback 渲染（primary 无此字形）。
    let mut defs = egui::FontDefinitions::default();
    defs.font_data.insert(
        primary.0.clone(),
        egui::FontData::from_owned(primary.1.clone()).into(),
    );
    defs.font_data.insert(
        fallback.0.clone(),
        egui::FontData::from_owned(fallback.1.clone()).into(),
    );
    let family = defs.families.get_mut(&egui::FontFamily::Proportional)?;
    family.insert(0, primary.0.clone());
    family.insert(1, fallback.0.clone());

    let mut fonts = egui::epaint::text::Fonts::new(
        egui::epaint::text::TextOptions::default(),
        defs,
    );
    let mut fonts = fonts.with_pixels_per_point(1.0);
    let fid = egui::FontId::proportional(FONT_PROBE_SIZE);

    // (ascent, row_height) of the face actually rendering `text`.
    let face_metrics = |fonts: &mut egui::epaint::text::FontsView,
                        text: &str|
     -> Option<(f32, f32)> {
        let galley = fonts.layout_no_wrap(
            text.to_owned(),
            fid.clone(),
            egui::Color32::WHITE,
        );
        let row = galley.rows.first()?;
        let g = row.row.glyphs.first()?;
        Some((g.font_face_ascent, g.font_face_height))
    };
    let (p_asc, p_height) = face_metrics(&mut fonts, "A")?;
    let (f_asc, f_height) = face_metrics(&mut fonts, &probe.to_string())?;

    if !(p_asc.is_finite()
        && f_asc.is_finite()
        && p_height > 0.0
        && f_height > 0.0)
    {
        return None;
    }
    let scale = p_height / f_height;
    // 保护：只接受常见系统字体之间的度量比，越界说明探测出了异常
    // （如 probe 字形落到了 tofu），宁可不归一也不要错误缩放。
    if !(0.5..=1.5).contains(&scale) {
        log::warn!(
            "font tweak out of range: {} -> {} scale={scale:.3}, \
             skipping normalization",
            fallback.0,
            primary.0
        );
        return None;
    }
    let y_offset_factor =
        (p_asc - f_asc * scale) / (FONT_PROBE_SIZE * scale);
    log::info!(
        "font metrics normalized: {} -> {} (scale={scale:.4}, \
         y_offset_factor={y_offset_factor:+.4})",
        fallback.0,
        primary.0
    );
    Some(egui::FontTweak {
        scale,
        y_offset_factor,
        ..Default::default()
    })
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

    /// 回归：中英混排"等高"与基线对齐。
    ///
    /// 旧行为（不归一）：雅黑行高 1.32em > Consolas 1.17em，含中文的行
    /// 更高；两字体 ascent 不同 → 同一行基线错位。
    ///
    /// epaint 的摆放公式（text_layout.rs galley_from_rows）：
    /// `pos.y = font_face_ascent + valign·(max_row − line_height)
    ///          + 0.5·(font_height − font_face_height)`，
    /// 渲染位图再整体平移 `y_offset = size·scale·y_offset_factor`。
    /// 因此"等高 + 基线对齐"等价于对每个字形：
    /// 1. `font_face_height == font_height`（行高一致，后两项归零）；
    /// 2. `font_face_ascent + y_offset == font_ascent`（视觉基线一致）。
    /// 断言在多个字号下成立，容差 1px（round_ui 取整残差）。
    #[test]
    fn cjk_latin_metrics_normalized() {
        let (Some(primary), Some(cjk)) =
            (load_ui_font_consolas(), load_cjk_font())
        else {
            eprintln!("无 Consolas 或 CJK 字体（非 Windows）：跳过");
            return;
        };
        let tweak = font_tweak_to_match_primary(&primary, &cjk, '中')
            .expect("应能计算出归一 FontTweak");

        let build_defs = |tweak: Option<egui::FontTweak>| {
            let mut defs = egui::FontDefinitions::default();
            defs.font_data.insert(
                primary.0.clone(),
                egui::FontData::from_owned(primary.1.clone()).into(),
            );
            let data = egui::FontData::from_owned(cjk.1.clone())
                .tweak(tweak.unwrap_or_default());
            defs.font_data.insert(cjk.0.clone(), data.into());
            let family = defs
                .families
                .get_mut(&egui::FontFamily::Proportional)
                .unwrap();
            family.insert(0, primary.0.clone());
            family.push(cjk.0.clone());
            defs
        };

        // 每个字形 (font_height, font_ascent, font_face_height,
        // font_face_ascent)。
        let layout = |defs: &egui::FontDefinitions,
                      text: &str,
                      size: f32|
         -> Vec<(f32, f32, f32, f32)> {
            let mut fonts = egui::epaint::text::Fonts::new(
                egui::epaint::text::TextOptions::default(),
                defs.clone(),
            );
            let mut fonts = fonts.with_pixels_per_point(1.0);
            fonts
                .layout_no_wrap(
                    text.to_owned(),
                    egui::FontId::proportional(size),
                    egui::Color32::WHITE,
                )
                .rows[0]
                .row
                .glyphs
                .iter()
                .map(|g| {
                    (
                        g.font_height,
                        g.font_ascent,
                        g.font_face_height,
                        g.font_face_ascent,
                    )
                })
                .collect()
        };

        for size in [12.0_f32, 14.0, 16.0, 21.0] {
            // 未归一（旧行为）在所有测试字号下都必须暴露问题，防止空转。
            let old = build_defs(None);
            let old_en = layout(&old, "Apg", size);
            let old_mixed = layout(&old, "Ap中g字", size);
            let old_cjk_height = old_mixed[2].2; // '中' 的 face 行高
            assert!(
                (old_cjk_height - old_en[0].0).abs() > 1.0,
                "{size}px: 未归一时中文行高应明显不同 \
                 ({old_cjk_height} vs {})",
                old_en[0].0
            );

            // 归一（新行为）。
            let fixed = build_defs(Some(tweak.clone()));
            let mixed = layout(&fixed, "Ap中g字", size);
            let y_off = size * tweak.scale * tweak.y_offset_factor;
            for (i, &(fh, fa, ffh, ffa)) in mixed.iter().enumerate() {
                assert!(
                    (ffh - fh).abs() <= 1.0,
                    "{size}px 字形{i}: 行高未归一 (face={ffh} primary={fh})"
                );
                let is_fallback = i == 2 || i == 4; // '中' '字'
                let baseline = if is_fallback { ffa + y_off } else { ffa };
                assert!(
                    (baseline - fa).abs() <= 1.0,
                    "{size}px 字形{i}: 基线未对齐 ({baseline} vs {fa})"
                );
            }
        }
    }
}

#[cfg(test)]
mod consolas_tests {
    use super::*;

    /// Windows 本机：Consolas Regular 可加载且为合法 TrueType。
    #[test]
    fn ui_font_consolas_loads() {
        if let Some((name, bytes)) = load_ui_font_consolas() {
            assert_eq!(name, "Consolas");
            assert!(bytes.len() > 100_000, "字体应完整: {} bytes", bytes.len());
            assert_eq!(&bytes[..4], &[0x00, 0x01, 0x00, 0x00], "sfnt TrueType");
        } else {
            eprintln!("无 Consolas（非 Windows）：跳过（回退链兜底）");
        }
    }

    /// 粗体链首选 Consolas Bold（存在时），中文粗体回退雅黑。
    #[test]
    fn bold_font_prefers_consolas() {
        if let Some((name, _)) = load_bold_font() {
            assert_eq!(name, "ConsolasBold");
        }
    }
}
