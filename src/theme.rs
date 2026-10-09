//! 主题：内置 Wong 主题，编译进二进制，启动时应用。
//!
//! 配色放在 `themes/wong.json`（Spur 风格语义 token：
//! background / sidebar / popover 表面、accent 家族、状态色）。
//! 等以后有了设置存储，再在这里加主题选择的持久化。

use std::rc::Rc;
use std::sync::Mutex;
use std::sync::OnceLock;

use gpui_kit::component::{Theme, ThemeConfig, ThemeSet};
use gpui_kit::App;

/// 默认主题，首次启动（或存的名字对不上）时用它。
pub const DEFAULT_NAME: &str = "Wong Dark";

const BUILTIN_JSON: &str = include_str!("../themes/wong.json");

/// 全部内置主题，解析一次后缓存。
static BUILTINS: OnceLock<Vec<ThemeConfig>> = OnceLock::new();

/// 当前主题名：`init` / `apply` 写入，设置页读它画高亮。
static SELECTED: Mutex<String> = Mutex::new(String::new());

fn remember(name: &str) {
    *SELECTED
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = name.to_string();
}

/// 全部内置主题。纯函数，设置页列主题名不需要 App。
pub fn all() -> &'static [ThemeConfig] {
    BUILTINS.get_or_init(|| {
        serde_json::from_str::<ThemeSet>(BUILTIN_JSON)
            .expect("built-in themes are valid JSON")
            .themes
    })
}

/// 全部内置主题名，文件顺序（默认主题在前）。
pub fn names() -> Vec<String> {
    all().iter().map(|t| t.name.to_string()).collect()
}

/// 当前主题名（还没 `init` 过时是默认主题）。
pub fn selected() -> String {
    let name = SELECTED
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    if name.is_empty() {
        DEFAULT_NAME.to_string()
    } else {
        name
    }
}

fn config_for(name: &str) -> Option<ThemeConfig> {
    all().iter().find(|t| t.name == name).cloned()
}

/// 应用启动时的主题：先看持久化的选择，对不上（比如主题改名了）再回落
/// 默认。在 `gpui_kit::init` 之后、开窗之前调用一次。
pub fn init(cx: &mut App) {
    // 存的名字必须还在内置表里，否则可能是旧版本的手写主题，当没存过。
    let name = crate::settings::theme_name()
        .filter(|n| config_for(n).is_some())
        .unwrap_or_else(|| DEFAULT_NAME.to_string());
    if let Some(config) = config_for(&name) {
        remember(&name);
        Theme::global_mut(cx).apply_config(&Rc::new(config));
    }
}

/// 按名应用主题、持久化并重绘所有窗口。不认识的名字返回 false，什么都不做。
pub fn apply(cx: &mut App, name: &str) -> bool {
    let Some(config) = config_for(name) else {
        return false;
    };
    Theme::global_mut(cx).apply_config(&Rc::new(config));
    remember(name);
    // 主题已经换了，持久化失败只记一笔日志。
    if let Err(e) = crate::settings::set_theme_name(name) {
        eprintln!("存主题失败: {e}");
    }
    cx.refresh_windows();
    true
}

#[cfg(test)]
mod tests {
    // 注意：这里不要 `use super::*`——它会把 gpui 自带的 `#[test]` 宏引进
    // 来，盖住 Rust 内置的 test 属性导致递归展开失败。只显式引入用到的项。
    use super::DEFAULT_NAME;
    use super::all;
    use super::names;
    use super::selected;

    #[test]
    fn builtin_themes_parse_and_contain_the_default() {
        assert!(!all().is_empty(), "themes/wong.json 必须自带主题");
        assert!(
            names().iter().any(|n| n == DEFAULT_NAME),
            "{DEFAULT_NAME} 在 themes/wong.json 里缺失"
        );
    }

    #[test]
    fn builtin_theme_names_are_unique() {
        let mut seen = Vec::new();
        for name in names() {
            assert!(!seen.contains(&name), "主题重名 {name}");
            seen.push(name);
        }
    }

    #[test]
    fn selected_falls_back_to_default() {
        // selected() 只读内存标记不碰磁盘；apply/init 需要 App 测不到，
        // 所以这里永远是默认主题，稳定。
        assert_eq!(selected(), DEFAULT_NAME);
    }

    #[test]
    fn default_theme_is_dark_for_the_shell_washes() {
        // 外壳的 hover wash 和 hairline 都是白色半透明叠加，
        // 只在深色表面上才看得清。
        let default = all()
            .iter()
            .find(|t| t.name == DEFAULT_NAME)
            .expect("default theme exists");
        assert!(default.mode.is_dark(), "Wong Dark 必须保持深色");
    }
}
