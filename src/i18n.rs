//! 多语言：官方 rust-i18n 流程，文案在 `locales/ui.yml`（`app.*`）。
//!
//! - 语言切换走 gpui-component 重导出的 `set_locale`，以后用上带内部文案
//!   的组件（DatePicker 之类）时它们会一起切；切完要 `cx.notify()` 或
//!   `refresh_windows()` 才会重绘（locale 是 GPUI 追踪之外的全局状态）。
//! - 应用文案用 `t("app.xxx")` 取；`extend!` 只影响组件查自己的 key，
//!   应用侧查不到组件内置文案，缺 key 时返回 key 本身。
//! - 加文案：yml 中英两列同时加，并在测试的 `KEYS` 里登记；加语言：
//!   加 locale 列 + `Language` 分支 + `LANGUAGES` 登记。

/// 支持的语言。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Language {
    Chinese,
    English,
}

/// 全部支持的语言。加新语言时在这里登记，设置页和测试会自动覆盖。
pub const LANGUAGES: &[Language] = &[Language::Chinese, Language::English];

/// 语言对应的 locale 代码（和 yml 列名一致）。
pub fn code(lang: Language) -> &'static str {
    match lang {
        Language::Chinese => "zh-CN",
        Language::English => "en",
    }
}

/// 启动时调用一次，从 `WONG_LANG` 装入语言。
pub fn init() {
    set_language(language_from_env());
}

/// 环境变量选中的语言：只有显式要求英文才切过去，其他一律中文。
fn language_from_env() -> Language {
    match std::env::var("WONG_LANG").map(|v| v.to_ascii_lowercase()) {
        Ok(v) if v == "en" || v == "english" => Language::English,
        _ => Language::Chinese,
    }
}

/// 当前语言（读组件的全局 locale）。
pub fn language() -> Language {
    if &*gpui_kit::component::locale() == "en" {
        Language::English
    } else {
        Language::Chinese
    }
}

/// 切换语言，已打开的视图要自己 `cx.notify()` 才重绘；
/// 标题栏等窗口级文字下次启动更新。
pub fn set_language(lang: Language) {
    gpui_kit::component::set_locale(code(lang));
}

/// 取应用文案。key 缺失时返回 key 本身，不要拿它当断言用，见测试。
pub fn t(key: &str) -> String {
    // 变量 key 查出来是 Cow，直接转 owned，调用方拿 String 省心。
    rust_i18n::t!(key).into_owned()
}

/// 测试串行锁：locale 是进程全局状态，改它的测试必须互斥，
// 否则并行跑起来互相覆盖。
#[cfg(test)]
pub(crate) static LOCALE_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// 拿测试串行锁（顺带处理前一个测试 panic 留下的毒化）。
#[cfg(test)]
pub(crate) fn lock_locale() -> std::sync::MutexGuard<'static, ()> {
    LOCALE_GUARD
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    // 注意：这里不要 `use super::*`——它会把 gpui 自带的 `#[test]` 宏引进
    // 来，盖住 Rust 内置的 test 属性导致递归展开失败。只显式引入用到的项。
    use super::code;
    use super::language;
    use super::language_from_env;
    use super::lock_locale;
    use super::set_language;
    use super::t;
    use super::LANGUAGES;
    use super::Language;

    /// 全部应用文案 key，加 key 时同步加这里，全量校验靠它覆盖。
    const KEYS: &[&str] = &[
        "app.app_name",
        "app.window_title",
        "app.new_tool",
        "app.tool_welcome",
        "app.tool_welcome_hint",
        "app.tool_text",
        "app.tool_text_hint",
        "app.tool_convert",
        "app.tool_convert_hint",
        "app.tool_settings",
        "app.tool_settings_hint",
        "app.coming_soon",
        "app.settings_language",
        "app.settings_language_hint",
        "app.settings_lang_zh",
        "app.settings_lang_en",
        "app.settings_theme",
        "app.settings_theme_hint",
    ];

    #[test]
    fn locale_codes_match_yml_columns() {
        assert_eq!(code(Language::Chinese), "zh-CN");
        assert_eq!(code(Language::English), "en");
    }

    #[test]
    fn english_needs_explicit_request() {
        // 改环境变量前备份，测完还原，不污染其他测试。
        let prev = std::env::var("WONG_LANG").ok();
        std::env::remove_var("WONG_LANG");
        assert_eq!(language_from_env(), Language::Chinese);
        std::env::set_var("WONG_LANG", "en");
        assert_eq!(language_from_env(), Language::English);
        std::env::set_var("WONG_LANG", "English");
        assert_eq!(language_from_env(), Language::English);
        std::env::set_var("WONG_LANG", "zh");
        assert_eq!(language_from_env(), Language::Chinese);
        match prev {
            Some(v) => std::env::set_var("WONG_LANG", v),
            None => std::env::remove_var("WONG_LANG"),
        }
    }

    #[test]
    fn every_locale_resolves_every_key() {
        // 每种语言下每个 key 都必须有东西；缺 key 时 t() 会原样返回 key，
        // 界面上会直接露出 key，所以这里把“返回 key”也算失败。
        let _guard = lock_locale();
        let prev = language();
        for lang in LANGUAGES {
            set_language(*lang);
            for key in KEYS {
                let text = t(key);
                assert!(
                    !text.is_empty() && text != *key,
                    "{lang:?} 的 {key} 缺失"
                );
            }
        }
        set_language(prev);
    }

    #[test]
    fn switching_language_switches_strings() {
        // 切换前后读到的必须是对应语言的表；测完还原，不影响其他测试。
        let _guard = lock_locale();
        let prev = language();
        set_language(Language::English);
        assert_eq!(t("app.new_tool"), "New");
        set_language(Language::Chinese);
        // “新建”
        assert_eq!(t("app.new_tool"), "新建");
        set_language(prev);
    }
}
