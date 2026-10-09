//! 应用外壳：Spur 风格自绘标题栏、侧边工具导航、内容区。
//! 工具目前是占位项，以后每个真工具配独立视图。

use gpui_kit::assets::IconName;
use gpui_kit::component::button::Button;
use gpui_kit::component::{ActiveTheme, Icon, Sizable as _};
use gpui_kit::*;

use crate::i18n::{Strings, t};

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
    /// 侧边栏和内容区显示的名称，随当前语言走。
    fn name(self, s: &Strings) -> &str {
        match self {
            ToolId::Welcome => &s.tool_welcome,
            ToolId::Text => &s.tool_text,
            ToolId::Convert => &s.tool_convert,
            ToolId::Settings => &s.tool_settings,
        }
    }

    /// 名称旁边的副提示，随当前语言走。
    fn hint(self, s: &Strings) -> &str {
        match self {
            ToolId::Welcome => &s.tool_welcome_hint,
            ToolId::Text => &s.tool_text_hint,
            ToolId::Convert => &s.tool_convert_hint,
            ToolId::Settings => &s.tool_settings_hint,
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

/// 主窗口根视图：持有当前选中的工具序号。
pub struct WsShell {
    selected: usize,
}

impl WsShell {
    /// 创建外壳。`window` 暂时用不到；以后的工具（对话框、
    /// 通知）会在构造时需要它，先把参数占上。
    pub fn new(_window: &mut Window, _cx: &mut Context<Self>) -> Self {
        Self { selected: 0 }
    }

    fn render_titlebar(&self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        let tx = t();
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
                            .child(tx.app_name.as_str()),
                    ),
            )
            // 品牌和标题栏按钮之间的弹性拖拽区。
            .child(div().flex_1().h_full().window_control_area(WindowControlArea::Drag))
            .child(
                Button::new("new-tool")
                    // “新建”
                    .label(tx.new_tool.as_str())
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
                    // 行标签：工具名，中文见 i18n/zh.json（如“欢迎”）。
                    .child(div().text_size(px(TEXT_MD)).child(tool.id.name(t())))
            }))
    }

    fn render_content(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let tool = &TOOLS[self.selected];
        let tx = t();
        let name = tool.id.name(tx);
        let hint = tool.id.hint(tx);
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
            .child(
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
                            .child(tx.coming_soon.as_str()),
                    ),
            )
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
    use crate::i18n::{LANGUAGES, parse};

    #[test]
    fn sidebar_tools_are_registered_without_duplicates() {
        // 侧边栏按表渲染，一行一项；表空了或重名导航就坏了。
        assert!(!TOOLS.is_empty(), "TOOLS 至少要有一项");
        // 所有语言的文案表都要能解析，每种语言下逐项检查。
        let langs: Vec<_> = LANGUAGES.iter().map(|l| parse(*l)).collect();
        let mut names = Vec::new();
        for tool in TOOLS {
            // 每种语言都要有东西，不能有一边是空的。
            for lang in &langs {
                assert!(!tool.id.name(lang).is_empty(), "工具名不能为空");
                assert!(!tool.id.hint(lang).is_empty(), "工具提示不能为空");
            }
            let name = tool.id.name(&langs[0]).to_string();
            assert!(!names.contains(&name), "侧边栏入口重名 {name}");
            names.push(name);
        }
    }
}
