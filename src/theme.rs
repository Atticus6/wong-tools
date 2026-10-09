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
