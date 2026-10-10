//! 应用外壳：Spur 风格自绘标题栏、侧边工具导航、内容区。
//! 只管框架：各工具的视图在各自模块里（`tools/*`、`settings/view`），
//! 这里按 `TOOLS` 分发。跨模块调的都走 `pub(crate)`，别放出去。

use gpui_kit::assets::IconName;
use gpui_kit::component::button::Button;
use gpui_kit::component::{ActiveTheme, Icon, Sizable as _};
use gpui_kit::*;

use crate::i18n::t;

/// Spur 度量 token。
const TITLEBAR_H: f32 = 38.0;
const HEADER_H: f32 = 44.0;
/// 原生 Windows 标题栏按钮宽度（系统度量）。
const CAPTION_W: f32 = 46.0;
const SIDEBAR_W: f32 = 216.0;

/// 唯一的小字号阶梯（逻辑像素）。各工具视图共用，见 `tools/*`。
pub(crate) const TEXT_SM: f32 = 12.0;
pub(crate) const TEXT_MD: f32 = 13.0;
pub(crate) const TEXT_LG: f32 = 15.0;

/// 工具标识：导航、标题、图标都从它派生，保证三处不脱节。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolId {
    Welcome,
    Screenshot,
    Proxy,
    AiRelay,
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
            // “截图”
            ToolId::Screenshot => t("app.tool_screenshot"),
            // “代理”
            ToolId::Proxy => t("app.tool_proxy"),
            // “AI 转发”
            ToolId::AiRelay => t("app.tool_airelay"),
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
            // “截取主显示器全屏”
            ToolId::Screenshot => t("app.tool_screenshot_hint"),
            // “VLESS 订阅直连”
            ToolId::Proxy => t("app.tool_proxy_hint"),
            // “Zen 免费模型网关”
            ToolId::AiRelay => t("app.tool_airelay_hint"),
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
        id: ToolId::Screenshot,
        icon: IconName::Camera,
    },
    ToolDef {
        id: ToolId::Proxy,
        icon: IconName::Globe,
    },
    ToolDef {
        id: ToolId::AiRelay,
        icon: IconName::Bot,
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

/// 聚光搜索用的工具入口：序号 + 名称 + 英文检索词。
/// 和 `TOOLS` 同源，顺序一致，加新工具时关键字一起补。
pub(crate) struct ToolEntry {
    /// 在 `TOOLS` 里的序号，也是选中态的值。
    pub index: usize,
    /// 工具名（走文案）。
    pub name: String,
    /// 英文检索词（搜 `proxy` 直达代理）。
    pub keywords: &'static str,
}

/// 侧边栏顺序的工具入口（聚光搜索跳转命令用）。
pub(crate) fn tool_entries() -> Vec<ToolEntry> {
    const KEYWORDS: &[&str] = &[
        // 欢迎 / 截图 / 代理 / AI 转发 / 文本工具 / 格式转换 / 设置
        "welcome home start",
        "screenshot capture printscreen",
        "proxy vless vpn tunnel",
        "ai relay zen models gateway",
        "text tools",
        "convert format transform",
        "settings preferences hotkey",
    ];
    TOOLS
        .iter()
        .enumerate()
        .map(|(index, tool)| ToolEntry {
            index,
            name: tool.id.name(),
            keywords: KEYWORDS.get(index).copied().unwrap_or(""),
        })
        .collect()
}

/// 设置工具在 `TOOLS` 里的序号（聚光搜索“改快捷键”跳转用）。
pub(crate) fn settings_tool_index() -> usize {
    TOOLS
        .iter()
        .position(|tool| tool.id == ToolId::Settings)
        .unwrap_or(0)
}

/// 主窗口根视图：持有当前选中的工具序号，以及设置页选中的主题名
/// （主题本体的应用走 `crate::theme`，这里只记选中态用来高亮）。
pub struct WsShell {
    pub(crate) selected: usize,
    pub(crate) settings_theme: String,
    pub(crate) shot: crate::tools::screenshot::ShotState,
    pub(crate) proxy: crate::tools::proxy::ProxyView,
    pub(crate) spotlight: crate::spotlight::SpotlightView,
    pub(crate) airelay: crate::tools::airelay::view::AiRelayView,
}

impl WsShell {
    /// 创建外壳。各工具的状态在自己模块里建（要 `window` 和 `cx`）。
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        Self {
            // 上次选中的工具：序号越界（工具增删）回落欢迎页。
            selected: crate::settings::selected_tool()
                .filter(|ix| *ix < TOOLS.len())
                .unwrap_or(0),
            // 高亮跟随上次的选择：`theme::init` 启动时已经应用过了。
            settings_theme: crate::theme::selected(),
            shot: crate::tools::screenshot::ShotState::empty(),
            proxy: crate::tools::proxy::ProxyView::new(window, cx),
            spotlight: crate::spotlight::SpotlightView::new(cx),
            airelay: crate::tools::airelay::view::AiRelayView::new(window, cx),
        }
    }

    /// 侧边栏选中并持久化：导航点击和搜索面板跳转共用，行为一致。
    /// 切工具顺手退出快捷键录制（录制行不在了还记着会误事）。
    pub(crate) fn select_tool(&mut self, ix: usize) {
        self.selected = ix;
        self.spotlight.recording = false;
        if let Err(e) = crate::settings::set_selected_tool(ix) {
            eprintln!("存选中工具失败: {e}");
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
        // macOS 红绿灯占左上约 60px：标题栏左边多让位，品牌往右挪不被盖住；
        // Windows 右侧是自绘最小化/最大化/关闭，本来就贴边，不留白。
        // `cfg!` 运行时分支：函数引用还在，跨平台编译不报 dead_code。
        let is_mac = cfg!(target_os = "macos");
        let left_pad = if is_mac { 72. } else { 12. };
        let right_pad = if is_mac { 12. } else { 0. };

        div()
            .flex()
            .flex_none()
            .items_center()
            .h(px(TITLEBAR_H))
            .pl(px(left_pad))
            .pr(px(right_pad))
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
            // macOS 用系统红绿灯（左上），右侧不画 Windows 按钮；
            // Windows 才要 8px 间隔 + 三个贴边按钮。
            .children(if is_mac {
                Vec::new()
            } else {
                vec![div().w(px(8.)).into_any_element()]
            })
            .children(if is_mac {
                Vec::new()
            } else {
                caption_controls(window, muted, fg)
            })
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
                        this.select_tool(ix);
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
            // 横向也要能收：宽表（请求/连接）才会被夹出确定宽度，表内横滚出得来，
            // 不然整页被撑出窗口，右边的列直接看不见。
            .min_w_0()
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
            ToolId::Screenshot => self.render_shot(cx).into_any_element(),
            ToolId::Proxy => self.render_proxy(cx).into_any_element(),
            ToolId::AiRelay => self.render_airelay(cx).into_any_element(),
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
            // 全局呼出开关：`None` 上下文，焦点在哪都能触发；
            // 和系统级热键走同一个 `toggle_palette_window`，不双跳。
            .on_action(cx.listener(
                |this, _: &crate::spotlight::ToggleSpotlight, _, cx| {
                    this.spotlight.toggle_palette_window(cx);
                },
            ))
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

impl Drop for WsShell {
    /// 退出兜底：隧道还开着就掐掉、系统代理复原、旗落下来，别把用户断网留在那。
    /// 只动自己开过的代理（`running` 为空说明没开过，不碰用户自己的设置）。
    /// 注意：进程被杀时 Drop 不跑，靠启动时的残留清理（见 `main.rs`）。
    fn drop(&mut self) {
        if let Some(handle) = self.proxy.running.take() {
            handle.stop();
            let _ = crate::tools::proxy::disable_system_proxy();
            let _ = crate::settings::set_proxy_running(false);
        }
        // 网关只听回环，退出掐掉即可（端口不占着，下次起得来）。
        if let Some(relay) = self.airelay.running.take() {
            relay.stop();
        }
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