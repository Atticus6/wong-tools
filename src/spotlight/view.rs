//! 搜索面板窗口：独立 PopUp 小窗（类 Spotlight），不是主窗口里的悬浮层。
//!
//! 主窗口只留开关（`SpotlightView`：热键管理 + 面板句柄 + 录制态）；
//! 面板窗口是 `PaletteView`（本文件），查询/高亮随窗生死，命令执行回
//! 主窗口（`WeakEntity<WsShell>`），跑完自关并把主窗还原到前台
//! （平台级激活，最小化也拉回，再做应用级激活兜底）。
//! 多窗口做法见 https://gpui-kit.com/docs/multi-window/ ，键盘走
//! https://gpui-kit.com/docs/keybinding/ 。

use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::{ActiveTheme, Sizable as _};
use gpui_kit::Focusable as _;
use gpui_kit::*;

use super::PaletteConfirm;
use super::PaletteDismiss;
use super::PaletteDown;
use super::PaletteUp;
use super::Recorded;
use super::SpotlightAction;
use super::ToggleSpotlight;
use super::build_commands;
use super::filter_commands;
use super::format_recorded;
use crate::i18n::t;
use crate::shell::{TEXT_MD, TEXT_SM, WsShell};

/// 面板宽（逻辑像素）：类 Spotlight 的窄条。
const PANEL_W: f32 = 560.0;
/// 面板高：搜索框 + 9 行 + 底栏，写死才不用跟着内容跳。
const PANEL_H: f32 = 440.0;
/// 列表最多展示行数，和面板高配套。
const MAX_ROWS: usize = 9;
/// 命令行高：鼠标命中和键盘高亮同一套尺寸。
const ROW_H: f32 = 36.0;
/// 面板垂直位置：主屏可见区顶部往下约 18%（Spotlight 同款）。
const PANEL_TOP_FRACTION: f32 = 0.18;

/// 主窗口侧的搜索状态：热键管理 + 面板句柄 + 录制态。
/// 面板开着时查询和高亮活在面板窗口里，这里只记句柄做开关。
pub(crate) struct SpotlightView {
    /// 快捷键录制中（设置页）：按键走录制，不走面板导航。
    pub recording: bool,
    /// 当前快捷键原文（设置页展示）。
    pub spec: String,
    /// 注册/解析失败原因：设置页展示，`None` 就是正常。
    pub hotkey_error: Option<String>,
    /// 面板窗口句柄：开着才有，关窗（哪边关的都算）即清。
    pub palette: Option<AnyWindowHandle>,
    /// 代理是否在跑的缓存：启停方法是唯二改 `running` 的地方，顺手维护；
    /// 开面板时直接读它——开窗同步画首帧，那时借用都在，现读就嵌套崩。
    pub proxy_running: bool,
    /// 全局热键句柄：主线程持有，事件靠轮询读（见 `start_hotkey_poll`）。
    hotkey: super::HotkeyHandle,
    /// 上次开关面板时间：按住呼出键连发时防连跳（300ms 内只认一次）。
    last_toggle: std::time::Instant,
}

impl SpotlightView {
    /// 建热键管理 + 读存档绑全局热键 + 起事件轮询。
    pub fn new(cx: &mut Context<WsShell>) -> Self {
        let spec = crate::settings::hotkey()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| super::DEFAULT_HOTKEY.to_string());
        let mut hotkey = super::HotkeyHandle::new();
        let hotkey_error = hotkey.rebind(&spec).err();
        let view = Self {
            recording: false,
            spec,
            hotkey_error,
            palette: None,
            // 新进程启动隧道句柄一定是空的（不断线重连），从 false 开始。
            proxy_running: false,
            hotkey,
            // 启动即现在：首帧 300ms 内的杂事件不认。
            last_toggle: std::time::Instant::now(),
        };
        view.start_hotkey_poll(cx);
        view
    }

    /// 全局热键事件轮询：`global-hotkey` 自己起线程往全局 channel 里扔，
    /// 这里借连接表心跳同款 `cx.spawn` 定时读（120ms 无感）。
    /// 对上当前注册 id 才算数，野事件不理；窗口没了轮询退出。
    fn start_hotkey_poll(&self, cx: &mut Context<WsShell>) {
        cx.spawn(async move |this, cx| {
            let receiver = global_hotkey::GlobalHotKeyEvent::receiver();
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(120))
                    .await;
                let mut pressed = Vec::new();
                while let Ok(event) = receiver.try_recv() {
                    if event.state == global_hotkey::HotKeyState::Pressed {
                        pressed.push(event.id);
                    }
                }
                if pressed.is_empty() {
                    continue;
                }
                let alive = cx.update(|cx| {
                    this.update(cx, |view, cx| {
                        let current = view.spotlight.hotkey.current_id();
                        if current.is_some_and(|id| pressed.contains(&id)) {
                            view.spotlight.toggle_palette_window(cx);
                        }
                    })
                    .is_ok()
                });
                if !alive {
                    break;
                }
            }
        })
        .detach();
    }

    /// 开关面板窗口：开着就关，关着就开（句柄失效当没开过）。
    /// 全局热键、应用内快捷键、面板里的开关走同一入口，不会各干各的。
    pub(crate) fn toggle_palette_window(&mut self, cx: &mut Context<WsShell>) {
        const DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(300);
        if self.last_toggle.elapsed() < DEBOUNCE {
            return;
        }
        self.last_toggle = std::time::Instant::now();
        if let Some(handle) = self.palette.take() {
            // 关掉了就算这次是关闭；句柄失效说明外面已经没了，继续开新的。
            if handle
                .update(cx, |_, window, _| window.remove_window())
                .is_ok()
            {
                return;
            }
        }
        let main = cx.entity().downgrade();
        let running = self.proxy_running;
        match PaletteView::open(main, running, cx) {
            Ok(handle) => self.palette = Some(handle),
            Err(e) => {
                self.hotkey_error = Some(e);
                cx.notify();
            }
        }
    }

    /// 换绑全局热键：解析 + 注册 + 入库一条龙，失败不入库、旧绑定不断。
    /// 成功返回 keymap 写法，调用方拿它追加应用内兜底绑定；
    /// 失败返回中文原因，调用方展示。
    pub fn rebind_hotkey(&mut self, spec: &str) -> Result<String, String> {
        // keymap 那边也要能建（`KeyBinding::new` 会 panic，先验后建）。
        let keymap = super::keymap_binding_string(spec)?;
        self.hotkey.rebind(spec)?;
        if let Err(e) = crate::settings::set_hotkey(spec) {
            eprintln!("存快捷键失败: {e}");
        }
        self.spec = spec.to_string();
        self.hotkey_error = None;
        Ok(keymap)
    }
}

/// 面板在屏幕上的落点：水平居中、垂直约 18% 处，纯逻辑好单测。
/// 屏再小也不缩面板（PopUp 写死尺寸），居中可能出负原点，平台会夹。
fn palette_origin(screen: Bounds<Pixels>) -> Point<Pixels> {
    point(
        px(screen.origin.x.as_f32() + (screen.size.width.as_f32() - PANEL_W) / 2.),
        px(screen.origin.y.as_f32() + screen.size.height.as_f32() * PANEL_TOP_FRACTION),
    )
}

/// 搜索面板窗口：查询 + 高亮随窗生死；执行回主窗口，跑完自关。
pub(crate) struct PaletteView {
    /// 搜索框 entity，随窗生死（和主窗口常驻输入框不一样）。
    query: Entity<InputState>,
    /// 上次渲染时的查询串：变了高亮回到第一行。
    last_query: String,
    /// 高亮行下标（当前过滤结果内）。
    selected: usize,
    /// 首帧聚焦做过：render 里只抢一次焦点。
    focused_once: bool,
    /// 输入框失焦关窗订阅：随窗生死，关窗自动注销。
    /// 窗内点行会先丢一次焦点（行不可聚焦），靠鼠标位置放行，
    /// 等行自己的 on_click 跑命令；鼠标已出窗才是点窗外，直接关。
    /// 只为持有（drop 即退订），从不读。
    #[allow(dead_code)]
    blur_dismiss: Subscription,
    /// 打开时代理是否在跑：首帧是同步画出来的（`open_window` 里直接
    /// `draw`），那时主窗口正借着，渲染里现读会嵌套更新直接崩；
    /// 所以打开前快照一次。反正跑完命令就关窗，不存在过期。
    running: bool,
    /// 主窗口弱引用：命令执行和句柄清理走它，窗口关了就地忽略。
    main: WeakEntity<WsShell>,
}

impl PaletteView {
    /// 开搜索窗：PopUp 置顶小窗，主屏可见区顶部居中（overlay 同款做法）。
    /// 返回窗口句柄，调用方（主窗口）留着做开关。
    /// 注意：`open_window` 同步画第一帧，`new` 和首帧渲染里绝不能碰主窗口
    /// （调用方借着它调进来的，碰就是嵌套更新直接崩）；代理状态调用方
    /// 传快照进来。
    pub fn open(
        main: WeakEntity<WsShell>,
        proxy_running: bool,
        cx: &mut App,
    ) -> Result<AnyWindowHandle, String> {
        let screen = cx
            .primary_display()
            .map(|display| display.visible_bounds());
        let origin = screen.map(palette_origin).unwrap_or(point(px(80.), px(60.)));
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds {
                origin,
                size: size(px(PANEL_W), px(PANEL_H)),
            })),
            titlebar: Some(TitlebarOptions {
                // “Wong Tools”
                title: Some(t("app.window_title").into()),
                appears_transparent: true,
                // macOS 红绿灯：PopUp 小窗不需要它们，挪到窗外眼不见为净。
                // 框架没给隐藏开关，位置法是目前唯一的办法（窗口会裁掉
                // 越界内容），升级 gpui-kit 时复查。Windows 无边框自绘
                // 本来就没有原生按钮，传 None。
                traffic_light_position: if cfg!(target_os = "macos") {
                    Some(point(px(-100.), px(-100.)))
                } else {
                    None
                },
            }),
            // 置顶弹出窗：盖在主窗口和其他应用上面。
            kind: WindowKind::PopUp,
            focus: true,
            show: true,
            is_resizable: false,
            ..WindowOptions::default()
        };
        open_window(options, cx, |window, cx| {
            cx.new(|cx| Self::new(main, proxy_running, window, cx))
        })
        .map(|(handle, _)| handle)
        .map_err(|e| format!("打不开搜索窗口: {e}"))
    }

    /// 建常驻本窗口的搜索框（要 `window` 和 `cx`）。
    fn new(
        main: WeakEntity<WsShell>,
        proxy_running: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        // “搜索工具、命令、设置…”
        let hint = t("app.spotlight_hint");
        let query = cx.new(|cx| InputState::new(window, cx).placeholder(hint));
        // 输入框失焦关窗：点窗外（鼠标已出本窗）直接关，窗内点行放行。
        // render 首帧才抢焦点，之前不可能 blur，不会误关。
        let focus = query.read(cx).focus_handle(cx).clone();
        let dismiss_main = main.clone();
        let opened_at = std::time::Instant::now();
        let blur_dismiss = window.on_focus_out(&focus, &mut *cx, move |_event, window, cx| {
            // 鼠标坐标是窗内相对值：还在窗内就是点行之类的窗内交互，
            // 放行等 on_click；出了窗才是真的离开。
            let local = Bounds {
                origin: point(px(0.), px(0.)),
                size: window.bounds().size,
            };
            if local.contains(&window.mouse_position()) {
                return;
            }
            // 开窗初期的抖动 blur 不认：后台呼出时焦点同步会先丢一次，
            // 用户还没碰过就关了等于闪退。1.5 秒足够同步落定。
            const GRACE: std::time::Duration = std::time::Duration::from_millis(1500);
            if opened_at.elapsed() < GRACE {
                return;
            }
            window.remove_window();
            let _ = dismiss_main.update(cx, |view, _| view.spotlight.palette = None);
        });
        Self {
            query,
            last_query: String::new(),
            selected: 0,
            focused_once: false,
            running: proxy_running,
            main,
            blur_dismiss,
        }
    }

    /// 当前过滤结果：标题每次现翻，语言切换即时生效。
    /// 行号即下标，键盘确认和鼠标点击用同一份。
    /// 注意：渲染里只读快照，绝不碰主窗口（首帧在借用内，碰就崩）。
    fn matches(&self, cx: &mut Context<Self>) -> Vec<super::CommandDef> {
        let query = self.query.read(cx).value().to_string();
        let commands = build_commands(self.running);
        filter_commands(&query, &commands)
            .into_iter()
            .map(|command| super::CommandDef {
                title: command.title.clone(),
                group: command.group.clone(),
                keywords: command.keywords,
                action: command.action.clone(),
            })
            .collect()
    }

    /// 执行高亮行：主窗口分发 + 收尾（关窗、前台，见 `run_and_close`）。
    /// 主窗口已经没了就只关窗，不报错。
    fn confirm_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let action = self
            .matches(cx)
            .get(self.selected)
            .map(|command| command.action.clone());
        let Some(action) = action else {
            return;
        };
        self.run_and_close(&action, window, cx);
    }

    /// 执行收尾：主窗分发 → 关本窗 → 清句柄 → 主窗拉回前台 → 应用级激活。
    /// 主窗前台必须走平台级 `activate_window`：它会先还原最小化的窗口；
    /// 光调应用级 `activate` 不够，Windows 下最小化的主窗只会在任务栏闪，
    /// 用户看到的就是“点了没反应”。主窗口已经没了就只关窗，不报错。
    fn run_and_close(&self, action: &SpotlightAction, window: &mut Window, cx: &mut Context<Self>) {
        let main = self.main.clone();
        let _ = main.update(cx, |view, cx| view.run_spotlight_action(action, cx));
        window.remove_window();
        let _ = main.update(cx, |view, _| view.spotlight.palette = None);
        let _ = main.update(cx, |view, cx| {
            let _ = view
                .main_window
                .update(cx, |_, window, _| window.activate_window());
        });
        cx.activate(true);
    }

    /// 关本窗：清掉主窗口记的句柄（下次呼出重开），再把自己摘掉。
    fn close_self(&self, window: &mut Window, cx: &mut Context<Self>) {
        let main = self.main.clone();
        window.remove_window();
        let _ = main.update(cx, |view, _| view.spotlight.palette = None);
    }
}

impl Render for PaletteView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // 首帧聚焦搜索框：打开即打字，之后不再抢。
        if !self.focused_once {
            self.focused_once = true;
            let handle = self.query.read(cx).focus_handle(cx).clone();
            window.focus(&handle, cx);
        }
        // 查询变了高亮回第一行（订阅式更新的轻量替代，见 `last_query`）。
        let query_text = self.query.read(cx).value().to_string();
        if query_text != self.last_query {
            self.last_query = query_text;
            self.selected = 0;
        }
        let muted = cx.theme().muted_foreground;
        let bg = cx.theme().background;
        let border = cx.theme().border;
        let row_bg = cx.theme().sidebar_accent;
        let row_fg = cx.theme().sidebar_accent_foreground;
        let idle_fg = cx.theme().sidebar_foreground;

        let matches = self.matches(cx);
        let selected = self.selected.min(matches.len().saturating_sub(1));
        self.selected = selected;
        let rows: Vec<AnyElement> = matches
            .into_iter()
            .take(MAX_ROWS)
            .enumerate()
            .map(|(ix, command)| {
                let active = ix == selected;
                let (bg, fg) = if active {
                    (row_bg, row_fg)
                } else {
                    (hsla(0., 0., 0., 0.), idle_fg)
                };
                let action = command.action.clone();
                div()
                    .id(ElementId::named_usize("spot-row", ix))
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap(px(8.))
                    .px(px(10.))
                    .h(px(ROW_H))
                    .rounded(px(6.))
                    .bg(bg)
                    .text_color(fg)
                    .cursor_pointer()
                    .hover(move |s| if active { s } else { s.bg(row_bg) })
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.run_and_close(&action, window, cx);
                    }))
                    .child(
                        div()
                            .flex_1()
                            .text_size(px(TEXT_MD))
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .child(command.title),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_size(px(TEXT_SM))
                            .text_color(muted)
                            .child(command.group),
                    )
                    .into_any_element()
            })
            .collect();
        let body: AnyElement = if rows.is_empty() {
            div()
                .flex()
                .items_center()
                .justify_center()
                .h(px(ROW_H * 2.))
                .text_size(px(TEXT_MD))
                .text_color(muted)
                // “没有匹配结果”
                .child(t("app.spotlight_empty"))
                .into_any_element()
        } else {
            div().flex().flex_col().gap(px(2.)).children(rows).into_any_element()
        };

        div()
            .size_full()
            .bg(bg)
            .border_1()
            .border_color(border)
            .rounded(px(10.))
            .p(px(8.))
            .flex()
            .flex_col()
            .gap(px(6.))
            // 面板导航键：`None` 上下文后来居上（见 main 的 `bind_keys`），
            // 输入框的光标键会被这里先接住；本窗关了 handler 随窗走，
            // 不影响主窗口输入。
            .on_action(cx.listener(|this, _: &PaletteUp, _, cx| {
                if !this.matches(cx).is_empty() {
                    this.selected = this.selected.saturating_sub(1);
                    cx.notify();
                }
            }))
            .on_action(cx.listener(|this, _: &PaletteDown, _, cx| {
                let max = this.matches(cx).len().saturating_sub(1).min(MAX_ROWS - 1);
                if !this.matches(cx).is_empty() {
                    this.selected = (this.selected + 1).min(max);
                    cx.notify();
                }
            }))
            .on_action(cx.listener(|this, _: &PaletteConfirm, window, cx| {
                this.confirm_selected(window, cx);
            }))
            .on_action(cx.listener(|this, _: &PaletteDismiss, window, cx| {
                this.close_self(window, cx);
            }))
            .on_action(cx.listener(|this, _: &ToggleSpotlight, window, cx| {
                this.close_self(window, cx);
            }))
            .child(
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap(px(8.))
                    .px(px(6.))
                    .child(Input::new(&self.query).small().w_full()),
            )
            .child(body)
            .child(
                div()
                    .flex_none()
                    .text_size(px(TEXT_SM))
                    .text_color(muted)
                    // “↑↓ 选择 · Enter 确认 · Esc 关闭”
                    .child(t("app.spotlight_footer")),
            )
    }
}

impl WsShell {
    /// 命令分发：跳转 / 主题 / 语言 / 代理启停 / 改快捷键。
    /// 代理启停复用工具页按钮同款方法，行为一致。
    /// 调用方是面板窗口（执行完自己关窗），主窗口只管执行。
    pub(crate) fn run_spotlight_action(
        &mut self,
        action: &SpotlightAction,
        cx: &mut Context<Self>,
    ) {
        match action {
            SpotlightAction::GotoTool(ix) => {
                self.select_tool(*ix);
            }
            SpotlightAction::SetTheme(name) => {
                // 换主题并重绘所有窗口；名字对不上什么都不做。
                if crate::theme::apply(cx, name) {
                    self.settings_theme = name.clone();
                }
            }
            SpotlightAction::SetLanguage(lang) => {
                crate::i18n::set_language(*lang);
            }
            SpotlightAction::ProxyStart => {
                self.proxy_tunnel_start(cx);
            }
            SpotlightAction::ProxyStop => {
                self.proxy_tunnel_stop(cx);
            }
            SpotlightAction::EditHotkey => {
                self.select_tool(crate::shell::settings_tool_index());
                self.spotlight.recording = true;
            }
        }
        cx.notify();
    }

    /// 设置页录制按键：调用方只在录制态调（`recording` 为 true 时挂
    /// `capture_key_down`，平时不监听）。Esc 退出，其他组合走校验入库。
    pub(crate) fn record_hotkey(&mut self, stroke: &gpui_kit::Keystroke, cx: &mut Context<Self>) {
        match format_recorded(stroke) {
            // Esc：退出录制，保持原快捷键。
            Recorded::Cancel => {
                self.spotlight.recording = false;
                cx.notify();
            }
            // 纯修饰键/裸键：继续等。
            Recorded::Ignore => {}
            // 有效组合：校验 + 重绑 + 入库，失败展示原因并保持录制。
            Recorded::Spec(spec) => match self.spotlight.rebind_hotkey(&spec) {
                Ok(keymap) => {
                    // 应用内兜底绑定追加：同深度后来居上，老绑定自然降级。
                    cx.bind_keys([KeyBinding::new(&keymap, ToggleSpotlight, None)]);
                    self.spotlight.recording = false;
                    cx.notify();
                }
                Err(e) => {
                    self.spotlight.hotkey_error = Some(e);
                    cx.notify();
                }
            },
        }
    }
}

#[cfg(test)]
mod tests {
    // 注意：这里不要 `use super::*`——它会把 gpui 自带的 `#[test]` 宏引进
    // 来，盖住 Rust 内置的 test 属性导致递归展开失败。只显式引入用到的项。
    use super::PANEL_H;
    use super::PANEL_W;
    use super::palette_origin;
    use gpui_kit::Bounds;
    use gpui_kit::point;
    use gpui_kit::px;
    use gpui_kit::size;

    #[test]
    fn palette_lands_top_center_of_screen() {
        // 主屏 2560x1440：面板水平居中、垂直约 18% 处。
        let screen = Bounds {
            origin: point(px(0.), px(0.)),
            size: size(px(2560.), px(1440.)),
        };
        let origin = palette_origin(screen);
        assert_eq!(origin.x, px((2560. - PANEL_W) / 2.));
        assert_eq!(origin.y, px(1440. * 0.18));
        // 副屏原点不在零点：跟着屏走，不跑回主屏。
        let second = Bounds {
            origin: point(px(2560.), px(0.)),
            size: size(px(1920.), px(1080.)),
        };
        let origin = palette_origin(second);
        assert_eq!(origin.x, px(2560. + (1920. - PANEL_W) / 2.));
        // 屏比面板还窄：不缩面板（PopUp 写死尺寸），居中即可。
        let tiny = Bounds {
            origin: point(px(0.), px(0.)),
            size: size(px(400.), px(300.)),
        };
        let origin = palette_origin(tiny);
        assert_eq!(origin.x, px((400. - PANEL_W) / 2.));
    }

    #[test]
    fn palette_size_constants_match_row_math() {
        // 面板高必须装下搜索框 + 9 行 + 底栏，不然内容被裁。
        assert_eq!(PANEL_H, 440.0);
        assert!(PANEL_H >= 44.0 + 9.0 * 36.0 + 28.0 + 32.0);
    }
}
