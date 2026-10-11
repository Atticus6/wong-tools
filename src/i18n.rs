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

/// 启动时调用一次：持久化的选择优先，其次看 `WONG_LANG`，都没有用中文。
/// 选出来的值会再存一遍（比如这次是环境变量定的，下次启动就不用再传了）。
pub fn init() {
    set_language(persisted_language().unwrap_or_else(language_from_env));
}

/// 持久化的语言：存的代码必须还在支持列表里，否则当没存过
/// （比如将来 locale 改名了，旧值不能把界面卡死）。
fn persisted_language() -> Option<Language> {
    let saved = crate::settings::language_code()?;
    LANGUAGES.iter().find(|l| code(**l) == saved).copied()
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

/// 切换语言并持久化，已打开的视图要自己 `cx.notify()` 才重绘；
/// 标题栏等窗口级文字下次启动更新。
pub fn set_language(lang: Language) {
    gpui_kit::component::set_locale(code(lang));
    // 语言已经切了，持久化失败只记一笔日志。
    if let Err(e) = crate::settings::set_language_code(code(lang)) {
        eprintln!("存语言失败: {e}");
    }
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
    use crate::settings::TempSettings;

    /// 全部应用文案 key，加 key 时同步加这里，全量校验靠它覆盖。
    const KEYS: &[&str] = &[
        "app.app_name",
        "app.window_title",
        "app.sidebar_collapse",
        "app.sidebar_expand",
        "app.update_title",
        "app.update_hint",
        "app.update_check",
        "app.update_checking",
        "app.update_idle",
        "app.update_latest",
        "app.update_available",
        "app.update_open_page",
        "app.update_download",
        "app.update_failed",
        "app.tool_welcome",
        "app.tool_welcome_hint",
        "app.tool_screenshot",
        "app.tool_screenshot_hint",
        "app.tool_proxy",
        "app.tool_proxy_hint",
        "app.proxy_sub",
        "app.proxy_refresh",
        "app.proxy_fetching",
        "app.proxy_fetched",
        "app.proxy_empty",
        "app.proxy_nodes",
        "app.proxy_start",
        "app.proxy_stop",
        "app.proxy_running",
        "app.proxy_stopped",
        "app.proxy_local",
        "app.proxy_need_nodes",
        "app.proxy_started",
        "app.proxy_tab_sub",
        "app.proxy_speedtest",
        "app.proxy_testing",
        "app.proxy_tested",
        "app.proxy_timeout",
        "app.proxy_selfcheck",
        "app.proxy_selfchecking",
        "app.proxy_selfcheck_ok",
        "app.proxy_tab_rules",
        "app.proxy_lookup_hint",
        "app.proxy_lookup_go",
        "app.proxy_tag_manual",
        "app.proxy_tag_auto",
        "app.proxy_auto_badge",
        "app.proxy_auto_idle",
        "app.proxy_auto_locked",
        "app.proxy_manual_title",
        "app.proxy_effective_title",
        "app.proxy_effective_note",
        "app.proxy_auto_empty",
        "app.proxy_rule_add",
        "app.proxy_add_title",
        "app.proxy_cancel",
        "app.proxy_confirm",
        "app.proxy_kind_suffix",
        "app.proxy_kind_exact",
        "app.proxy_kind_keyword",
        "app.proxy_kind_cidr",
        "app.proxy_pattern_hint",
        "app.proxy_target_hint",
        "app.proxy_bad_target",
        "app.proxy_rule_empty",
        "app.proxy_sort",
        "app.proxy_move_up",
        "app.proxy_move_down",
        "app.proxy_delete",
        "app.proxy_need_rule",
        "app.proxy_bad_cidr",
        "app.proxy_route_direct",
        "app.proxy_route_reject",
        "app.proxy_tab_conns",
        "app.proxy_filter_hint",
        "app.proxy_col_host",
        "app.proxy_col_down",
        "app.proxy_col_up",
        "app.proxy_col_down_speed",
        "app.proxy_col_up_speed",
        "app.proxy_col_chain",
        "app.proxy_col_rule",
        "app.proxy_col_process",
        "app.proxy_col_age",
        "app.proxy_col_source",
        "app.proxy_col_dest",
        "app.proxy_col_kind",
        "app.proxy_active",
        "app.proxy_closed",
        "app.proxy_conns_empty",
        "app.proxy_kill_all",
        "app.proxy_clear_closed",
        "app.tool_settings",
        "app.tool_settings_hint",
        "app.coming_soon",
        "app.settings_language",
        "app.settings_language_hint",
        "app.settings_lang_zh",
        "app.settings_lang_en",
        "app.settings_theme",
        "app.settings_theme_hint",
        "app.shot_capture",
        "app.shot_capture_hint",
        "app.shot_copy",
        "app.shot_save",
        "app.shot_retake",
        "app.shot_copied",
        "app.shot_saved",
        "app.shot_region",
        "app.shot_overlay_title",
        "app.shot_hint",
        "app.shot_cancel",
        "app.shot_pin",
        "app.shot_pin_title",
        "app.shot_pin_reset",
        "app.shot_pin_close",
        "app.shot_tool_rect",
        "app.shot_tool_ellipse",
        "app.shot_tool_line",
        "app.shot_tool_arrow",
        "app.shot_tool_pencil",
        "app.shot_tool_highlighter",
        "app.shot_tool_mosaic",
        "app.shot_tool_text",
        "app.shot_tool_eraser",
        "app.shot_tool_undo",
        "app.shot_tool_redo",
        "app.shot_clear",
        "app.shot_size_dec",
        "app.shot_size_inc",
        "app.shot_c_red",
        "app.shot_c_orange",
        "app.shot_c_yellow",
        "app.shot_c_green",
        "app.shot_c_blue",
        "app.shot_c_white",
        "app.shot_c_gray",
        "app.shot_c_black",
        "app.shot_text_hint",
        "app.shot_color",
        "app.shot_draw_hint",
        "app.tray_show",
        "app.tray_quit",
        "app.spotlight_hint",
        "app.spotlight_empty",
        "app.spotlight_footer",
        "app.spotlight_group_tool",
        "app.spotlight_group_theme",
        "app.spotlight_group_language",
        "app.spotlight_group_proxy",
        "app.spotlight_group_system",
        "app.spotlight_hotkey_cmd",
        "app.settings_hotkey",
        "app.settings_hotkey_hint",
        "app.hotkey_change",
        "app.hotkey_reset",
        "app.hotkey_recording",
        "app.hotkey_need_modifier",
        "app.hotkey_invalid",
        "app.hotkey_conflict",
        "app.hotkey_unavailable",
        "app.tool_airelay",
        "app.tool_airelay_hint",
        "app.airelay_start",
        "app.airelay_stop",
        "app.airelay_running",
        "app.airelay_stopped",
        "app.airelay_started",
        "app.airelay_test",
        "app.airelay_testing",
        "app.airelay_test_ok",
        "app.airelay_port",
        "app.airelay_port_hint",
        "app.airelay_models",
        "app.airelay_live",
        "app.airelay_fallback",
        "app.airelay_refresh",
        "app.airelay_refreshing",
        "app.airelay_refreshed",
        "app.airelay_log",
        "app.airelay_col_time",
        "app.airelay_col_route",
        "app.airelay_col_model",
        "app.airelay_col_node",
        "app.airelay_col_tokens",
        "app.airelay_col_cache",
        "app.airelay_col_finish",
        "app.airelay_col_ms",
        "app.airelay_col_err",
        "app.airelay_egress_direct",
        "app.airelay_egress_proxy",
        "app.airelay_egress_blocked",
        "app.airelay_tab_models",
        "app.airelay_tab_reqs",
        "app.airelay_codex",
        "app.airelay_codex_hint",
        "app.airelay_codex_done",
        "app.tool_coffee",
        "app.tool_coffee_hint",
        "app.coffee_start",
        "app.coffee_stop",
        "app.coffee_running",
        "app.coffee_stopped",
        "app.coffee_no_limit",
        "app.coffee_remaining",
        "app.coffee_duration",
        "app.coffee_min_fmt",
        "app.coffee_hour_fmt",
        "app.coffee_display",
        "app.coffee_display_hint",
        "app.coffee_custom",
        "app.coffee_custom_hint",
        "app.coffee_apply",
        "app.coffee_bad_custom",
        "app.coffee_expired",
        "app.coffee_failed",
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
        // 切语言会写库，指向临时文件，不碰真实数据目录。
        let _settings = TempSettings::install("i18n-keys");
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
        // 切语言会写库，指向临时文件，不碰真实数据目录。
        let _settings = TempSettings::install("i18n-switch");
        let prev = language();
        set_language(Language::English);
        assert_eq!(t("app.tool_screenshot"), "Screenshot");
        set_language(Language::Chinese);
        // “截图”
        assert_eq!(t("app.tool_screenshot"), "截图");
        set_language(prev);
    }
}
