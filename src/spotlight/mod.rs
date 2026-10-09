//! 聚光搜索：全局快捷键呼出的命令面板（工具跳转 + 命令 + 设置）。
//!
//! 快捷键原文统一存 `"Alt+Space"` 这种写法（`global-hotkey` 的 `FromStr`
//! 格式）：`+` 分隔、不区分大小写。应用内 keymap 要连字符写法，
//! `keymap_binding_string` 负责转换（见 gpui-kit keybinding 文档）。
//! 解析/转换/过滤都是纯函数，单测直接覆盖；系统注册才碰 OS。

use std::str::FromStr;

use global_hotkey::GlobalHotKeyManager;
use global_hotkey::hotkey::HotKey;

use crate::i18n::t;

mod view;
pub(crate) use view::SpotlightView;

// 面板动作：官方 `actions!` 注册，应用内 keymap 和面板共用同一套。
gpui_kit::actions!(
    spotlight,
    [
        ToggleSpotlight,
        PaletteUp,
        PaletteDown,
        PaletteConfirm,
        PaletteDismiss
    ]
);

/// 默认呼出键：Alt+Space（类 Spotlight/UTools 习惯，设置里可改）。
pub const DEFAULT_HOTKEY: &str = "Alt+Space";

/// 纯修饰键的按键名：录制时只按了它们不算一次有效组合，直接忽略。
const MODIFIER_KEYS: &[&str] = &[
    "shift", "control", "ctrl", "alt", "option", "cmd", "command", "super", "win", "meta",
    "function", "fn",
];

/// 解析快捷键原文：`global-hotkey` 能注册 + 必须带修饰键（裸字母会劫持
/// 打字，录制时直接拒掉）。失败返回展示给用户的中文原因。
pub fn parse_hotkey_spec(spec: &str) -> Result<HotKey, String> {
    let trimmed = spec.trim();
    if trimmed.is_empty() {
        // “快捷键不能为空”
        return Err(t("app.hotkey_invalid"));
    }
    let hotkey =
        HotKey::from_str(trimmed).map_err(|_| format!("{}：{trimmed}", t("app.hotkey_invalid")))?;
    if hotkey.mods.is_empty() {
        // “至少包含一个修饰键”
        return Err(t("app.hotkey_need_modifier"));
    }
    Ok(hotkey)
}

/// 原文转应用内 keymap 写法（`"Alt+Space"` → `"alt-space"`），并用
/// `Keystroke::parse` 验一遍：过了 `KeyBinding::new` 才不会 panic。
/// 录制时三道校验（全局解析、修饰键、keymap 解析）全过才入库。
pub fn keymap_binding_string(spec: &str) -> Result<String, String> {
    let converted = spec
        .split('+')
        .map(|token| token.trim().to_lowercase())
        .filter(|token| !token.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    gpui_kit::Keystroke::parse(&converted)
        .map_err(|_| format!("{}：{spec}", t("app.hotkey_invalid")))?;
    Ok(converted)
}

/// 设置页展示用：`"Super+Space"` 在 mac 显示 Cmd、其他平台显示 Win，
/// 修饰键之间加空格好读。只做展示，存还是原文。
pub fn display_hotkey(spec: &str) -> String {
    spec.split('+')
        .map(|token| {
            let trimmed = token.trim();
            if trimmed.eq_ignore_ascii_case("super")
                || trimmed.eq_ignore_ascii_case("cmd")
                || trimmed.eq_ignore_ascii_case("command")
            {
                if cfg!(target_os = "macos") {
                    "Cmd".to_string()
                } else {
                    "Win".to_string()
                }
            } else if trimmed.eq_ignore_ascii_case("ctrl")
                || trimmed.eq_ignore_ascii_case("control")
            {
                "Ctrl".to_string()
            } else {
                let mut chars = trimmed.chars();
                match chars.next() {
                    Some(first) => {
                        first.to_uppercase().collect::<String>() + chars.as_str()
                    }
                    None => String::new(),
                }
            }
        })
        .filter(|token| !token.is_empty())
        .collect::<Vec<_>>()
        .join(" + ")
}

/// 录制结果：取消（Esc）、忽略（纯修饰键/无修饰键）、有效组合。
pub enum Recorded {
    /// Esc：退出录制，保持原快捷键。
    Cancel,
    /// 还不算组合：继续等下一次按键。
    Ignore,
    /// 有效组合原文，可直接走校验入库。
    Spec(String),
}

/// 录制用：GPUI 按键拼成原文。`platform` 是 Cmd/Win 键，统一记 `Super`
///（两边解析器都认，展示时再按平台翻译）。
pub fn format_recorded(stroke: &gpui_kit::Keystroke) -> Recorded {
    let key = stroke.key.trim();
    if key.eq_ignore_ascii_case("escape") {
        return Recorded::Cancel;
    }
    let mods = &stroke.modifiers;
    let has_modifier = mods.control || mods.alt || mods.shift || mods.platform;
    if !has_modifier || key.is_empty() || MODIFIER_KEYS.contains(&key.to_lowercase().as_str()) {
        return Recorded::Ignore;
    }
    // 修饰键固定顺序，同一组合每次录出来同一个字符串；
    // 键名首字母大写（`space` → `Space`），展示和存档同一份。
    let mut tokens: Vec<String> = Vec::new();
    if mods.control {
        tokens.push("Ctrl".to_string());
    }
    if mods.alt {
        tokens.push("Alt".to_string());
    }
    if mods.shift {
        tokens.push("Shift".to_string());
    }
    if mods.platform {
        tokens.push("Super".to_string());
    }
    let mut chars = key.chars();
    match chars.next() {
        Some(first) => {
            tokens.push(first.to_uppercase().collect::<String>() + chars.as_str());
        }
        None => return Recorded::Ignore,
    }
    Recorded::Spec(tokens.join("+"))
}

/// 全局热键句柄：`GlobalHotKeyManager` 要求主线程创建，
/// 跟着 `SpotlightView` 活（视图以外只读 `current_id` 配事件）。
pub struct HotkeyHandle {
    manager: Option<GlobalHotKeyManager>,
    current: Option<HotKey>,
}

impl HotkeyHandle {
    /// 建管理器：OS 层失败（极少见）记成 `None`，面板靠应用内快捷键兜底，
    /// 设置页展示原因。
    pub fn new() -> Self {
        Self {
            manager: GlobalHotKeyManager::new().ok(),
            current: None,
        }
    }

    /// 换绑：先解旧的再注册新的，失败回滚旧绑定（不断连）。
    /// 成功无事，失败返回中文原因，调用方展示 + 不入库。
    pub fn rebind(&mut self, spec: &str) -> Result<(), String> {
        let hotkey = parse_hotkey_spec(spec)?;
        let Some(manager) = &self.manager else {
            // “系统热键不可用”
            return Err(t("app.hotkey_unavailable"));
        };
        if self.current == Some(hotkey) {
            return Ok(());
        }
        let old = self.current.take();
        match manager.register(hotkey) {
            Ok(()) => {
                self.current = Some(hotkey);
                Ok(())
            }
            Err(e) => {
                // 新的没抢下来（被占用）：旧的穿回去，别把呼出搞没了。
                if let Some(old) = old {
                    let _ = manager.register(old);
                    self.current = Some(old);
                }
                // “注册失败，可能被其他应用占用”
                Err(format!("{}：{e}", t("app.hotkey_conflict")))
            }
        }
    }

    /// 当前注册的热键 id：轮询事件时对暗号，`None` 说明没注册成功。
    pub fn current_id(&self) -> Option<u32> {
        self.current.map(|hotkey| hotkey.id())
    }
}

/// 面板命令执行目标：视图层按它分发，无窗口依赖好单测过滤。
#[derive(Debug, Clone)]
pub enum SpotlightAction {
    /// 跳到侧边栏第几个工具。
    GotoTool(usize),
    /// 换主题（内置名）。
    SetTheme(String),
    /// 换语言。
    SetLanguage(crate::i18n::Language),
    /// 代理启停（按当前状态二选一展示）。
    ProxyStart,
    /// 代理启停（按当前状态二选一展示）。
    ProxyStop,
    /// 跳设置页并进入快捷键录制。
    EditHotkey,
}

/// 面板里的一行：标题 + 分组 + 英文检索词 + 执行目标。
/// 标题走 `t()` 实时翻译，检索词是静态英文（搜拼音首字母不在 v1）。
pub struct CommandDef {
    /// 行主标题（中文见 locales/ui.yml）。
    pub title: String,
    /// 分组副标题（工具 / 主题 / 语言 / 代理 / 系统）。
    pub group: String,
    /// 英文检索词（`proxy theme` 之类，大小写不敏感）。
    pub keywords: &'static str,
    /// 执行目标。
    pub action: SpotlightAction,
}

/// 命令全表：工具跳转 + 主题 + 语言 + 代理启停（二选一）+ 改快捷键。
/// `proxy_running` 决定展示启动还是停止；标题每次现翻，语言切换即时生效。
pub fn build_commands(proxy_running: bool) -> Vec<CommandDef> {
    // “工具”
    let tool_group = t("app.spotlight_group_tool");
    // “主题”
    let theme_group = t("app.spotlight_group_theme");
    // “语言”
    let lang_group = t("app.spotlight_group_language");
    // “代理”
    let proxy_group = t("app.spotlight_group_proxy");
    // “系统”
    let system_group = t("app.spotlight_group_system");
    let mut commands: Vec<CommandDef> = crate::shell::tool_entries()
        .into_iter()
        .map(|entry| CommandDef {
            title: entry.name,
            group: tool_group.clone(),
            keywords: entry.keywords,
            action: SpotlightAction::GotoTool(entry.index),
        })
        .collect();
    commands.extend(crate::theme::names().into_iter().map(|name| CommandDef {
        title: name.clone(),
        group: theme_group.clone(),
        keywords: "theme color skin",
        action: SpotlightAction::SetTheme(name),
    }));
    commands.extend(
        crate::i18n::LANGUAGES
            .iter()
            .map(|lang| CommandDef {
                // “中文” / “English”，自称展示。
                title: match lang {
                    crate::i18n::Language::Chinese => t("app.settings_lang_zh"),
                    crate::i18n::Language::English => t("app.settings_lang_en"),
                },
                group: lang_group.clone(),
                keywords: "language locale chinese english",
                action: SpotlightAction::SetLanguage(*lang),
            }),
    );
    commands.push(if proxy_running {
        CommandDef {
            // “断开”
            title: t("app.proxy_stop"),
            group: proxy_group,
            keywords: "proxy stop disconnect vless tunnel",
            action: SpotlightAction::ProxyStop,
        }
    } else {
        CommandDef {
            // “启动代理”
            title: t("app.proxy_start"),
            group: proxy_group,
            keywords: "proxy start connect vless tunnel",
            action: SpotlightAction::ProxyStart,
        }
    });
    commands.push(CommandDef {
        // “修改呼出快捷键”
        title: t("app.spotlight_hotkey_cmd"),
        group: system_group,
        keywords: "hotkey shortcut keybinding kuaijiejian",
        action: SpotlightAction::EditHotkey,
    });
    commands
}

/// 命令过滤：分词后每词都必须出现在（标题 + 分组 + 检索词）里，
/// 大小写不敏感。空查询返回全表，原序（工具在前）。
pub fn filter_commands<'a>(query: &str, commands: &'a [CommandDef]) -> Vec<&'a CommandDef> {
    let tokens: Vec<String> = query
        .split_whitespace()
        .map(|token| token.to_lowercase())
        .filter(|token| !token.is_empty())
        .collect();
    if tokens.is_empty() {
        return commands.iter().collect();
    }
    commands
        .iter()
        .filter(|command| {
            let haystack = format!(
                "{} {} {}",
                command.title, command.group, command.keywords
            )
            .to_lowercase();
            tokens.iter().all(|token| haystack.contains(token))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    // 注意：这里不要 `use super::*`——它会把 gpui 自带的 `#[test]` 宏引进
    // 来，盖住 Rust 内置的 test 属性导致递归展开失败。只显式引入用到的项。
    use super::DEFAULT_HOTKEY;
    use super::CommandDef;
    use super::Recorded;
    use super::SpotlightAction;
    use super::build_commands;
    use super::display_hotkey;
    use super::filter_commands;
    use super::format_recorded;
    use super::keymap_binding_string;
    use super::parse_hotkey_spec;
    use crate::i18n::{lock_locale, set_language};
    use crate::settings::TempSettings;

    fn test_commands() -> Vec<CommandDef> {
        vec![
            CommandDef {
                title: "代理".to_string(),
                group: "工具".to_string(),
                keywords: "proxy vless",
                action: SpotlightAction::ProxyStart,
            },
            CommandDef {
                title: "Wong Dark".to_string(),
                group: "主题".to_string(),
                keywords: "theme dark",
                action: SpotlightAction::SetTheme("Wong Dark".to_string()),
            },
        ]
    }

    #[test]
    fn hotkey_spec_parses_and_requires_modifier() {
        // 默认键能过，三种写法都要认。
        assert!(parse_hotkey_spec(DEFAULT_HOTKEY).is_ok());
        assert!(parse_hotkey_spec("ctrl+shift+p").is_ok());
        assert!(parse_hotkey_spec("Cmd+K").is_ok());
        // 空的、裸键、瞎写的都拒掉。
        assert!(parse_hotkey_spec("").is_err());
        assert!(parse_hotkey_spec("Space").is_err());
        assert!(parse_hotkey_spec("Alt+Nope").is_err());
        // keymap 转换同步验证：过了全局解析但 keymap 不认也拒。
        assert_eq!(
            keymap_binding_string("Alt+Space").as_deref(),
            Ok("alt-space")
        );
        assert_eq!(
            keymap_binding_string("Ctrl+Shift+P").as_deref(),
            Ok("ctrl-shift-p")
        );
    }

    #[test]
    fn recorded_keystroke_builds_canonical_spec() {
        use gpui_kit::Keystroke;
        use gpui_kit::Modifiers;
        // Alt+Space：标准录入。
        let stroke = Keystroke {
            modifiers: Modifiers {
                alt: true,
                ..Default::default()
            },
            key: "space".to_string(),
            key_char: None,
        };
        match format_recorded(&stroke) {
            Recorded::Spec(spec) => {
                assert_eq!(spec, "Alt+Space");
                assert!(parse_hotkey_spec(&spec).is_ok());
            }
            _ => panic!("Alt+Space 必须录成有效组合"),
        }
        // Esc：取消。纯 Alt：忽略。裸字母：忽略（会劫持打字）。
        let esc = Keystroke {
            modifiers: Modifiers::default(),
            key: "escape".to_string(),
            key_char: None,
        };
        assert!(matches!(format_recorded(&esc), Recorded::Cancel));
        let lone_alt = Keystroke {
            modifiers: Modifiers {
                alt: true,
                ..Default::default()
            },
            key: "alt".to_string(),
            key_char: None,
        };
        assert!(matches!(format_recorded(&lone_alt), Recorded::Ignore));
        let bare = Keystroke {
            modifiers: Modifiers::default(),
            key: "a".to_string(),
            key_char: Some("a".to_string()),
        };
        assert!(matches!(format_recorded(&bare), Recorded::Ignore));
    }

    #[test]
    fn display_hotkey_translates_super_per_platform() {
        // 修饰键首字母大写、键原样，Super 按平台翻译。
        assert_eq!(display_hotkey("Alt+Space"), "Alt + Space");
        let super_display = display_hotkey("Super+K");
        if cfg!(target_os = "macos") {
            assert_eq!(super_display, "Cmd + K");
        } else {
            assert_eq!(super_display, "Win + K");
        }
    }

    #[test]
    fn command_filter_matches_tokens_case_insensitively() {
        let commands = test_commands();
        // 空查询给全表。
        assert_eq!(filter_commands("", &commands).len(), 2);
        // 中文标题、英文检索词、大小写不敏感都能命中。
        assert_eq!(filter_commands("代理", &commands).len(), 1);
        assert_eq!(filter_commands("PROXY", &commands).len(), 1);
        assert_eq!(filter_commands("theme dark", &commands).len(), 1);
        // 多词必须全中，命中不了返回空。
        assert!(filter_commands("theme light", &commands).is_empty());
        assert!(filter_commands("不存在", &commands).is_empty());
    }

    #[test]
    fn command_table_covers_tools_themes_and_proxy_toggle() {
        // 全表需要文案和主题表：隔离库 + 串行 locale。
        let _guard = lock_locale();
        let _settings = TempSettings::install("spotlight-commands");
        let prev = crate::i18n::language();
        set_language(crate::i18n::Language::Chinese);
        let stopped = build_commands(false);
        let started = build_commands(true);
        // 工具 6 个 + 主题 + 语言 2 个 + 代理 1 个 + 改快捷键 1 个。
        let tool_count = crate::shell::tool_entries().len();
        let theme_count = crate::theme::names().len();
        assert_eq!(stopped.len(), tool_count + theme_count + 2 + 1 + 1);
        assert_eq!(stopped.len(), started.len());
        // 代理启停按状态二选一。
        assert!(stopped.iter().any(|c| matches!(
            c.action,
            SpotlightAction::ProxyStart
        )));
        assert!(started.iter().any(|c| matches!(
            c.action,
            SpotlightAction::ProxyStop
        )));
        set_language(prev);
    }
}
