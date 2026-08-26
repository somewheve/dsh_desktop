//! 界面中英双语：Lang 枚举 + tr 辅助。
//!
//! 用法：`ui.label(tr(self.lang, "中文", "English"))`。
//! 语言存储于 AppConfig.lang（"zh" / "en"），UI 组件每帧从 app 同步。

/// 界面语言。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    Zh,
    En,
}

impl Lang {
    pub fn parse(s: &str) -> Lang {
        if s.eq_ignore_ascii_case("en") {
            Lang::En
        } else {
            Lang::Zh
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Lang::Zh => "zh",
            Lang::En => "en",
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Lang::Zh => "简体中文",
            Lang::En => "English",
        }
    }
}

/// 双语取词：zh 为中文文案，en 为英文文案。
pub fn tr(lang: Lang, zh: &str, en: &str) -> String {
    match lang {
        Lang::Zh => zh.to_string(),
        Lang::En => en.to_string(),
    }
}
