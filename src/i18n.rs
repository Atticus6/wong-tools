//! 多语言文案：默认中文，`WONG_LANG=en` 切英文。
//!
//! 文案住在 `i18n/*.json`，编译时打进二进制（单 exe 分发不丢文件）。
//! 加新语言：加一份 JSON + 在 `language()` 里接上选择条件；加文案：
//! JSON 和 `Strings` 两边同时加字段。JSON 侧多写或写错 key 会直接报错，
//! 防止文件越加越多之后出现僵尸 key。
//!
//! 需要组装的句子（数量、路径）写成方法，让每种语言自己决定语序和复数
//! （目前还没有，预留）。
//!
//! 不做本地化的：数据本身（工具输出、路径名）和键盘按键名。

use std::sync::OnceLock;

use serde::{Deserialize, Serialize};

/// 支持的语言。加新语言时同步加 JSON 文件和这里的分支。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Language {
    Chinese,
    English,
}

/// 全部支持的语言。加新语言时在这里登记，文案全量校验会自动覆盖它。
pub const LANGUAGES: &[Language] = &[Language::Chinese, Language::English];

/// 界面文案表：字段必须和各语言 JSON 的 key 一一对应。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Strings {
    pub app_name: String,
    pub window_title: String,
    pub new_tool: String,
    pub tool_welcome: String,
    pub tool_welcome_hint: String,
    pub tool_text: String,
    pub tool_text_hint: String,
    pub tool_convert: String,
    pub tool_convert_hint: String,
    pub tool_settings: String,
    pub tool_settings_hint: String,
    pub coming_soon: String,
}

const ZH_JSON: &str = include_str!("../i18n/zh.json");
const EN_JSON: &str = include_str!("../i18n/en.json");

/// 当前语言选中的文案表，首次调用 `t()` 时解析并缓存。
static ACTIVE: OnceLock<Strings> = OnceLock::new();

/// 当前语言：只有显式要求英文才切过去，其他一律中文。
pub fn language() -> Language {
    match std::env::var("WONG_LANG").map(|v| v.to_ascii_lowercase()) {
        Ok(v) if v == "en" || v == "english" => Language::English,
        _ => Language::Chinese,
    }
}

/// 解析一份语言 JSON。纯函数，专供测试和 `t()` 的首次加载。
pub fn parse(lang: Language) -> Strings {
    let (json, file) = match lang {
        Language::Chinese => (ZH_JSON, "i18n/zh.json"),
        Language::English => (EN_JSON, "i18n/en.json"),
    };
    serde_json::from_str(json).unwrap_or_else(|e| panic!("{file} 不是合法文案 JSON: {e}"))
}

/// 当前文案表。
pub fn t() -> &'static Strings {
    ACTIVE.get_or_init(|| parse(language()))
}

#[cfg(test)]
mod tests {
    // 注意：这里不要 `use super::*`——它会把 gpui 自带的 `#[test]` 宏引进
    // 来，盖住 Rust 内置的 test 属性导致递归展开失败。只显式引入用到的项。
    use super::language;
    use super::parse;
    use super::LANGUAGES;
    use super::Language;

    #[test]
    fn english_needs_explicit_request() {
        // 改环境变量前备份，测完还原，不污染其他测试。
        let prev = std::env::var("WONG_LANG").ok();
        std::env::remove_var("WONG_LANG");
        assert_eq!(language(), Language::Chinese);
        std::env::set_var("WONG_LANG", "en");
        assert_eq!(language(), Language::English);
        std::env::set_var("WONG_LANG", "English");
        assert_eq!(language(), Language::English);
        std::env::set_var("WONG_LANG", "zh");
        assert_eq!(language(), Language::Chinese);
        match prev {
            Some(v) => std::env::set_var("WONG_LANG", v),
            None => std::env::remove_var("WONG_LANG"),
        }
    }

    #[test]
    fn every_language_parses_with_no_empty_string() {
        // 每种语言都必须解析成功、且没有空文案；否则界面上会出现空白。
        for lang in LANGUAGES {
            let strings = parse(*lang);
            let value = serde_json::to_value(&strings).expect("文案表可序列化");
            for (key, text) in value.as_object().expect("文案表是扁平对象") {
                assert!(
                    text.as_str().is_some_and(|s| !s.is_empty()),
                    "{lang:?} 的 {key} 为空或不是字符串"
                );
            }
        }
    }
}
