//! App shell: Spur-style custom title bar, sidebar tool navigation,
//! and a content area. Tools are placeholders; each gets its own view later.

use gpui_kit::assets::IconName;
use gpui_kit::component::button::Button;
use gpui_kit::component::{ActiveTheme, Icon, Sizable as _};
use gpui_kit::*;

/// Spur metric tokens.
const TITLEBAR_H: f32 = 38.0;
const HEADER_H: f32 = 44.0;
/// Native Windows caption button width (the system metric).
const CAPTION_W: f32 = 46.0;
const SIDEBAR_W: f32 = 216.0;

/// The one small type scale (logical px).
const TEXT_SM: f32 = 12.0;
const TEXT_MD: f32 = 13.0;
const TEXT_LG: f32 = 15.0;

/// One sidebar entry. Real tools replace the placeholder `hint` with
/// their own view; the entry itself stays a name + icon + hint triple.
struct ToolDef {
    name: &'static str,
    hint: &'static str,
    icon: IconName,
}

const TOOLS: &[ToolDef] = &[
    ToolDef {
        name: "欢迎",
        hint: "工具箱首页",
        icon: IconName::House,
    },
    ToolDef {
        name: "文本工具",
        hint: "敬请期待",
        icon: IconName::FileText,
    },
    ToolDef {
        name: "格式转换",
        hint: "敬请期待",
        icon: IconName::ArrowLeftRight,
    },
    ToolDef {
        name: "设置",
        hint: "主题与偏好",
        icon: IconName::Settings,
    },
];

/// Root view of the main window: owns the selected tool index.
pub struct WsShell {
    selected: usize,
}

impl WsShell {
    /// Create the shell. `window` is unused for now; later tools (dialogs,
    /// toasts) will need it at construction time.
    pub fn new(_window: &mut Window, _cx: &mut Context<Self>) -> Self {
        Self { selected: 0 }
    }

    fn render_titlebar(&self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
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
            // Brand monogram + wordmark; part of the drag surface.
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
                            .child("Wong Tools"),
                    ),
            )
            // Flexible drag area between brand and caption controls.
            .child(div().flex_1().h_full().window_control_area(WindowControlArea::Drag))
            .child(
                Button::new("new-tool")
                    .label("新建")
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
                    .child(div().text_size(px(TEXT_MD)).child(tool.name))
            }))
    }

    fn render_content(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let tool = &TOOLS[self.selected];
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
                    .child(div().text_size(px(TEXT_LG)).text_color(fg).child(tool.name))
                    .child(div().text_size(px(TEXT_SM)).text_color(muted).child(tool.hint)),
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
                    .child(div().text_size(px(TEXT_LG)).text_color(fg).child(tool.name))
                    .child(
                        div()
                            .text_size(px(TEXT_MD))
                            .text_color(muted)
                            .child("功能即将到来，敬请期待"),
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

/// Minimize / maximize-restore / close, flush right in the title bar.
///
/// Each button is a platform control area: the OS answers the hit test and
/// owns the behavior, including Snap Layouts and the close action.
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
        // Native hover behavior: a white wash for min/max, the system red
        // for close.
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
    // NOTE: do NOT `use super::*` here — it would pull gpui's own `#[test]`
    // macro into scope and shadow the built-in test attribute. Import only
    // what the tests need.
    use super::TOOLS;

    #[test]
    fn sidebar_tools_are_registered_without_duplicates() {
        // The sidebar renders one row per entry; an empty or duplicated
        // table would show a broken navigation.
        assert!(!TOOLS.is_empty(), "TOOLS needs at least one entry");
        let mut names = Vec::new();
        for tool in TOOLS {
            assert!(!tool.name.is_empty(), "tool name must not be empty");
            assert!(!tool.hint.is_empty(), "tool hint must not be empty");
            assert!(
                !names.contains(&tool.name),
                "duplicate sidebar entry {}",
                tool.name
            );
            names.push(tool.name);
        }
    }
}
