//! Theme: built-in Wong themes, compiled in and applied at startup.
//!
//! The palettes live in `themes/wong.json` (Spur-style semantic tokens:
//! background / sidebar / popover surfaces, accent family, status colors).
//! The first matching theme below is the default; add selection persistence
//! later when a settings store exists.

use std::rc::Rc;

use gpui_kit::component::{Theme, ThemeConfig, ThemeSet};
use gpui_kit::App;

/// Default theme on first launch.
pub const DEFAULT_NAME: &str = "Wong Dark";

const BUILTIN_JSON: &str = include_str!("../themes/wong.json");

/// Apply the default built-in theme. Call once after `gpui_kit::init`
/// and before opening windows, so every window paints with it from the
/// first frame.
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
    // NOTE: do NOT `use super::*` here — it would pull gpui's own `#[test]`
    // macro into scope and shadow the built-in test attribute. Import only
    // what the tests need.
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
        assert!(!themes.is_empty(), "themes/wong.json must ship themes");
        assert!(
            themes.iter().any(|t| t.name == DEFAULT_NAME),
            "{DEFAULT_NAME} is missing from themes/wong.json"
        );
    }

    #[test]
    fn builtin_theme_names_are_unique() {
        let mut names = Vec::new();
        for theme in builtins() {
            let name = theme.name.to_string();
            assert!(!names.contains(&name), "duplicate theme {name}");
            names.push(name);
        }
    }

    #[test]
    fn default_theme_is_dark_for_the_shell_washes() {
        // The shell's hover washes and hairlines are white-alpha overlays,
        // which only read correctly on dark surfaces.
        let default = builtins()
            .into_iter()
            .find(|t| t.name == DEFAULT_NAME)
            .expect("default theme exists");
        assert!(default.mode.is_dark(), "Wong Dark must stay dark");
    }
}
