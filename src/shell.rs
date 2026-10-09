//! 应用外壳：Spur 风格自绘标题栏、侧边工具导航、内容区。
//! 工具目前是占位项，以后每个真工具配独立视图。

use gpui_kit::assets::IconName;
use gpui_kit::component::button::Button;
use gpui_kit::component::{ActiveTheme, Icon, Sizable as _};
use gpui_kit::*;

use crate::i18n::{LANGUAGES, Language, t};

/// Spur 度量 token。
const TITLEBAR_H: f32 = 38.0;
const HEADER_H: f32 = 44.0;
/// 原生 Windows 标题栏按钮宽度（系统度量）。
const CAPTION_W: f32 = 46.0;
const SIDEBAR_W: f32 = 216.0;

/// 唯一的小字号阶梯（逻辑像素）。
const TEXT_SM: f32 = 12.0;
const TEXT_MD: f32 = 13.0;
const TEXT_LG: f32 = 15.0;

/// 工具标识：导航、标题、图标都从它派生，保证三处不脱节。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolId {
    Welcome,
    Text,
    Convert,
    Settings,
}

impl ToolId {
    /// 侧边栏和内容区显示的名称（中文见 locales/ui.yml）。
    fn name(self) -> String {
        match self {
            // “欢迎”
            ToolId::Welcome => t("app.tool_welcome"),
            // “文本工具”
            ToolId::Text => t("app.tool_text"),
            // “格式转换”
            ToolId::Convert => t("app.tool_convert"),
            // “设置”
            ToolId::Settings => t("app.tool_settings"),
        }
    }

    /// 名称旁边的副提示（中文见 locales/ui.yml）。
    fn hint(self) -> String {
        match self {
            // “工具箱首页”
            ToolId::Welcome => t("app.tool_welcome_hint"),
            // “敬请期待”
            ToolId::Text => t("app.tool_text_hint"),
            // “敬请期待”
            ToolId::Convert => t("app.tool_convert_hint"),
            // “主题与偏好”
            ToolId::Settings => t("app.tool_settings_hint"),
        }
    }
}

/// 一条侧边栏入口。真工具会配自己的视图；入口本身永远是标识 + 图标。
struct ToolDef {
    id: ToolId,
    icon: IconName,
}

const TOOLS: &[ToolDef] = &[
    ToolDef {
        id: ToolId::Welcome,
        icon: IconName::House,
    },
    ToolDef {
        id: ToolId::Text,
        icon: IconName::FileText,
    },
    ToolDef {
        id: ToolId::Convert,
        icon: IconName::ArrowLeftRight,
    },
    ToolDef {
        id: ToolId::Settings,
        icon: IconName::Settings,
    },
];

/// 主窗口根视图：持有当前选中的工具序号，以及设置页选中的主题名
/// （主题本体的应用走 `crate::theme`，这里只记选中态用来高亮）。
pub struct WsShell {
    selected: usize,
    settings_theme: String,
}

impl WsShell {
    /// 创建外壳。`window` 暂时用不到；以后的工具（对话框、
    /// 通知）会在构造时需要它，先把参数占上。
    pub fn new(_window: &mut Window, _cx: &mut Context<Self>) -> Self {
        Self {
            selected: 0,
            // 高亮跟随上次的选择：`theme::init` 启动时已经应用过了。
            settings_theme: crate::theme::selected(),
        }
    }

    fn render_titlebar(&self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        // “Wong Tools”
        let app_name = t("app.app_name");
        // “新建”
        let new_tool = t("app.new_tool");
        let accent = cx.theme().link;
        let fg = cx.theme().foreground;
        let title_bg = cx.theme().title_bar;
        let title_border = cx.theme().title_bar_border;
        let muted = cx.theme().muted_foreground;

        div()
            .flex()
            .flex_none()
            .items_center()
            .h(px(TITLEBAR_H))
            .pl(px(12.))
            .gap(px(4.))
            .bg(title_bg)
            .border_b_1()
            .border_color(title_border)
            // 品牌 monogram + 字标，属于拖拽区的一部分。
            .child(
                div()
                    .flex_none()
                    .flex()
                    .items_center()
                    .h_full()
                    .gap(px(8.))
                    .pr(px(8.))
                    .window_control_area(WindowControlArea::Drag)
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_center()
                            .size(px(18.))
                            .rounded(px(6.))
                            .bg(accent)
                            .text_size(px(11.))
                            .text_color(title_bg)
                            .child("W"),
                    )
                    .child(
                        div()
                            .text_size(px(TEXT_MD))
                            .text_color(fg)
                            // “Wong Tools”
                            .child(app_name),
                    ),
            )
            // 品牌和标题栏按钮之间的弹性拖拽区。
            .child(div().flex_1().h_full().window_control_area(WindowControlArea::Drag))
            .child(
                Button::new("new-tool")
                    // “新建”
                    .label(new_tool)
                    .small()
                    .outline()
                    .cursor_pointer()
                    .on_click(|_, _, _| println!("New tool (not implemented yet)")),
            )
            .child(div().w(px(8.)))
            .children(caption_controls(window, muted, fg))
    }

    fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let sidebar_bg = cx.theme().sidebar;
        let sidebar_border = cx.theme().sidebar_border;
        let row_bg = cx.theme().sidebar_accent;
        let row_fg = cx.theme().sidebar_accent_foreground;
        let idle_fg = cx.theme().sidebar_foreground;

        div()
            .flex_none()
            .flex()
            .flex_col()
            .w(px(SIDEBAR_W))
            .h_full()
            .bg(sidebar_bg)
            .border_r_1()
            .border_color(sidebar_border)
            .py(px(8.))
            .px(px(8.))
            .gap(px(2.))
            .children(TOOLS.iter().enumerate().map(|(ix, tool)| {
                let active = ix == self.selected;
                let (bg, fg) = if active {
                    (row_bg, row_fg)
                } else {
                    (hsla(0., 0., 0., 0.), idle_fg)
                };
                div()
                    .id(ElementId::named_usize("nav", ix))
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap(px(8.))
                    .px(px(10.))
                    .h(px(32.))
                    .rounded(px(6.))
                    .bg(bg)
                    .text_color(fg)
                    .cursor_pointer()
                    .hover(move |s| {
                        if active {
                            s
                        } else {
                            s.bg(row_bg)
                        }
                    })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.selected = ix;
                        cx.notify();
                    }))
                    .child(Icon::new(tool.icon.clone()).size(px(14.)))
                    // 行标签：工具名，中文见 locales/ui.yml（如“欢迎”）。
                    .child(div().text_size(px(TEXT_MD)).child(tool.id.name()))
            }))
    }

    fn render_content(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let tool = &TOOLS[self.selected];
        let name = tool.id.name();
        let hint = tool.id.hint();
        let bg = cx.theme().background;
        let fg = cx.theme().foreground;
        let border = cx.theme().border;
        let muted = cx.theme().muted_foreground;

        div()
            .flex()
            .flex_1()
            .flex_col()
            .h_full()
            .bg(bg)
            .child(
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .h(px(HEADER_H))
                    .px(px(16.))
                    .gap(px(8.))
                    .border_b_1()
                    .border_color(border)
                    .child(div().text_size(px(TEXT_LG)).text_color(fg).child(name))
                    .child(div().text_size(px(TEXT_SM)).text_color(muted).child(hint)),
            )
            .child(self.render_tool_body(tool, cx))
    }

    /// 各工具的主体：设置页是真交互，其他工具暂时是空状态占位。
    /// 真工具落地时在这里加分支，每个分支调自己视图的渲染。
    fn render_tool_body(&self, tool: &ToolDef, cx: &mut Context<Self>) -> impl IntoElement {
        match tool.id {
            ToolId::Settings => self.render_settings(cx).into_any_element(),
            _ => self.render_placeholder(tool, cx).into_any_element(),
        }
    }

    /// 占位工具的空状态：大图标 + 名称 + “敬请期待”。
    fn render_placeholder(&self, tool: &ToolDef, cx: &mut Context<Self>) -> impl IntoElement {
        let name = tool.id.name();
        let fg = cx.theme().foreground;
        let muted = cx.theme().muted_foreground;

        div()
            .flex()
            .flex_1()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(px(8.))
            .child(Icon::new(tool.icon.clone()).size(px(28.)).text_color(muted))
            .child(div().text_size(px(TEXT_LG)).text_color(fg).child(name))
            .child(
                div()
                    .text_size(px(TEXT_MD))
                            .text_color(muted)
                            // “功能即将到来，敬请期待”
                            .child(t("app.coming_soon")),
            )
    }

    /// 设置页：“语言”和“主题”两组，一行一点即切。
    fn render_settings(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_1()
            .flex_col()
            .px(px(16.))
            .py(px(12.))
            .gap(px(16.))
            // “语言”分组
            .child(self.settings_section(
                // “语言”
                t("app.settings_language"),
                // “立即生效，标题栏下次启动更新”
                t("app.settings_language_hint"),
                self.language_rows(cx),
                cx,
            ))
            // “主题”分组
            .child(self.settings_section(
                // “主题”
                t("app.settings_theme"),
                // “点击切换，立即生效”
                t("app.settings_theme_hint"),
                self.theme_rows(cx),
                cx,
            ))
    }

    /// 设置页的一组：标题 + 副提示 + 若干选项行。
    /// 文案走 `SharedString`：借来的 `&str` 在元素树里活不够长。
    fn settings_section(
        &self,
        title: impl Into<SharedString>,
        hint: impl Into<SharedString>,
        rows: Vec<AnyElement>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let fg = cx.theme().foreground;
        let muted = cx.theme().muted_foreground;
        let title: SharedString = title.into();
        let hint: SharedString = hint.into();
        div()
            .flex()
            .flex_none()
            .flex_col()
            .gap(px(6.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(div().text_size(px(TEXT_MD)).text_color(fg).child(title))
                    .child(div().text_size(px(TEXT_SM)).text_color(muted).child(hint)),
            )
            .child(div().flex().flex_col().gap(px(2.)).children(rows))
    }

    /// 设置页通用选项行：选中高亮，悬停 wash，右侧对勾标出当前项。
    /// 标签走 `SharedString`：借来的 `&str` 在元素树里活不够长。
    fn option_row(
        label: impl Into<SharedString>,
        selected: bool,
        row_id: ElementId,
        on_click: impl Fn(&mut WsShell, &ClickEvent, &mut Window, &mut Context<WsShell>) + 'static,
        cx: &mut Context<WsShell>,
    ) -> AnyElement {
        let row_bg = cx.theme().sidebar_accent;
        let row_fg = cx.theme().sidebar_accent_foreground;
        let idle_fg = cx.theme().sidebar_foreground;
        let accent = cx.theme().link;
        let label: SharedString = label.into();
        let (bg, fg) = if selected {
            (row_bg, row_fg)
        } else {
            (hsla(0., 0., 0., 0.), idle_fg)
        };
        div()
            .id(row_id)
            .flex()
            .flex_none()
            .items_center()
            .gap(px(8.))
            .px(px(10.))
            .h(px(32.))
            .rounded(px(6.))
            .bg(bg)
            .text_color(fg)
            .cursor_pointer()
            .hover(move |s| {
                if selected {
                    s
                } else {
                    s.bg(row_bg)
                }
            })
            .on_click(cx.listener(on_click))
            .child(div().flex_1().text_size(px(TEXT_MD)).child(label))
            .children(selected.then(|| Icon::new(IconName::Check).size(px(13.)).text_color(accent)))
            .into_any_element()
    }

    /// “语言”分组：每种语言一行（中文 / English），点即切换、即时重绘。
    /// 行标签用各语言的自称；加新语言时 match 穷举会逼着补标签。
    fn language_rows(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        LANGUAGES
            .iter()
            .map(|lang| {
                let lang = *lang;
                let label = match lang {
                    // “中文”
                    Language::Chinese => t("app.settings_lang_zh"),
                    // “English”
                    Language::English => t("app.settings_lang_en"),
                };
                let selected = crate::i18n::language() == lang;
                Self::option_row(
                    label,
                    selected,
                    ElementId::named_usize("lang", lang as usize),
                    move |_, _, _, cx| {
                        crate::i18n::set_language(lang);
                        cx.notify();
                    },
                    cx,
                )
            })
            .collect()
    }

    /// “主题”分组：全部内置主题，一行一个，中文名见 themes/wong.json。
    fn theme_rows(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        crate::theme::names()
            .into_iter()
            .enumerate()
            .map(|(ix, name)| {
                let selected = self.settings_theme == name;
                let label = name.clone();
                Self::option_row(
                    label,
                    selected,
                    ElementId::named_usize("theme", ix),
                    move |this, _, _, cx| {
                        // 换主题并重绘所有窗口；名字对不上时什么都不做。
                        if crate::theme::apply(cx, &name) {
                            this.settings_theme = name.clone();
                        }
                        cx.notify();
                    },
                    cx,
                )
            })
            .collect()
    }
}

impl Render for WsShell {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let bg = cx.theme().background;
        let fg = cx.theme().foreground;

        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(bg)
            .text_color(fg)
            .child(self.render_titlebar(window, cx))
            .child(
                div()
                    .flex()
                    .flex_1()
                    .child(self.render_sidebar(cx))
                    .child(self.render_content(cx)),
            )
    }
}

/// 最小化 / 最大化-还原 / 关闭，贴在标题栏右侧。
///
/// 每个按钮都是平台控制区：由操作系统接管命中测试和行为，
/// 包括 Snap Layouts 和关闭动作。
fn caption_controls(window: &Window, muted: Hsla, fg: Hsla) -> Vec<AnyElement> {
    let max_icon = if window.is_maximized() {
        IconName::WindowRestore
    } else {
        IconName::WindowMaximize
    };
    [
        ("window-minimize", IconName::WindowMinimize, WindowControlArea::Min, false),
        ("window-maximize", max_icon, WindowControlArea::Max, false),
        ("window-close", IconName::WindowClose, WindowControlArea::Close, true),
    ]
    .into_iter()
    .map(|(id, icon, area, close)| {
        // 原生 hover 行为：最小化/最大化用白色 wash，关闭用系统红。
        let hover_bg: Hsla = if close {
            rgb(0xe81123).into()
        } else {
            hsla(0., 0., 1., 0.08)
        };
        let active_bg: Hsla = if close {
            rgb(0xc50f1f).into()
        } else {
            hsla(0., 0., 1., 0.12)
        };
        let hover_fg: Hsla = if close { white() } else { fg };

        div()
            .id(id)
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .w(px(CAPTION_W))
            .h_full()
            .text_color(muted)
            .hover(move |s| s.bg(hover_bg).text_color(hover_fg))
            .active(move |s| s.bg(active_bg).text_color(hover_fg))
            .window_control_area(area)
            .child(Icon::new(icon).size(px(13.)))
            .into_any_element()
    })
    .collect()
}

#[cfg(test)]
mod tests {
    // 注意：这里不要 `use super::*`——它会把 gpui 自带的 `#[test]` 宏引进
    // 来，盖住 Rust 内置的 test 属性导致递归展开失败。只显式引入用到的项。
    use super::TOOLS;
    use crate::i18n::{LANGUAGES, language, lock_locale, set_language};
    use crate::settings::TempSettings;

    #[test]
    fn sidebar_tools_are_registered_without_duplicates() {
        // 侧边栏按表渲染，一行一项；表空了或重名导航就坏了。
        assert!(!TOOLS.is_empty(), "TOOLS 至少要有一项");
        // 改全局 locale 的测试要串行；切语言会写库，指向临时文件。
        let _guard = lock_locale();
        let _settings = TempSettings::install("shell-tools");
        let prev = language();
        for lang in LANGUAGES {
            set_language(*lang);
            let mut names = Vec::new();
            for tool in TOOLS {
                let name = tool.id.name();
                let hint = tool.id.hint();
                assert!(!name.is_empty(), "工具名不能为空");
                assert!(!hint.is_empty(), "工具提示不能为空");
                assert!(!names.contains(&name), "侧边栏入口重名 {name}");
                names.push(name);
            }
        }
        set_language(prev);
    }
}
