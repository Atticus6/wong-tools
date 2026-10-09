//! 主题：内置 Wong 主题，编译进二进制，启动时应用。
//!
//! 配色放在 `themes/wong.json`（Spur 风格语义 token：
//! background / sidebar / popover 表面、accent 家族、状态色）。
//! 等以后有了设置存储，再在这里加主题选择的持久化。

use std::rc::Rc;

use gpui_kit::component::{Theme, ThemeConfig, ThemeSet};
use gpui_kit::App;

/// 首次启动的默认主题。
pub const DEFAULT_NAME: &str = "Wong Dark";

const BUILTIN_JSON: &str = include_str!("../themes/wong.json");

/// 应用默认内置主题。在 `gpui_kit::init` 之后、开窗之前调用一次，
/// 让所有窗口从首帧就用它绘制。
pub fn init(cx: &mut App) {
    let set: ThemeSet =
        serde_json::from_str(BUILTIN_JSON).expect("built-in themes are valid JSON");
    let config: ThemeConfig = set
        .themes
        .iter()
        .find(|t| t.name == DEFAULT_NAME)
        .or_else(|| set.themes.first())
        .expect("at least one built-in theme")
        .clone();
    Theme::global_mut(cx).apply_config(&Rc::new(config));
}

#[cfg(test)]
mod tests {
    // 注意：这里不要 `use super::*`——它会把 gpui 自带的 `#[test]` 宏引进
    // 来，盖住 Rust 内置的 test 属性导致递归展开失败。只显式引入用到的项。
    use super::{BUILTIN_JSON, DEFAULT_NAME};
    use gpui_kit::component::{ThemeConfig, ThemeSet};

    fn builtins() -> Vec<ThemeConfig> {
        serde_json::from_str::<ThemeSet>(BUILTIN_JSON)
            .expect("built-in themes are valid JSON")
            .themes
    }

    #[test]
    fn builtin_themes_parse_and_contain_the_default() {
        let themes = builtins();
        assert!(!themes.is_empty(), "themes/wong.json 必须自带主题");
        assert!(
            themes.iter().any(|t| t.name == DEFAULT_NAME),
            "{DEFAULT_NAME} 在 themes/wong.json 里缺失"
        );
    }

    #[test]
    fn builtin_theme_names_are_unique() {
        let mut names = Vec::new();
        for theme in builtins() {
            let name = theme.name.to_string();
            assert!(!names.contains(&name), "主题重名 {name}");
            names.push(name);
        }
    }

    #[test]
    fn default_theme_is_dark_for_the_shell_washes() {
        // 外壳的 hover wash 和 hairline 都是白色半透明叠加，
        // 只在深色表面上才看得清。
        let default = builtins()
            .into_iter()
            .find(|t| t.name == DEFAULT_NAME)
            .expect("default theme exists");
        assert!(default.mode.is_dark(), "Wong Dark 必须保持深色");
    }
}
