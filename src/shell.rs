//! 应用外壳：Spur 风格自绘标题栏、侧边工具导航、内容区。
//! 只管框架：各工具的视图在各自模块里（`tools/*`、`settings/view`），
//! 这里按 `TOOLS` 分发。跨模块调的都走 `pub(crate)`，别放出去。

use gpui_kit::assets::IconName;
use gpui_kit::base::spring;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::combobox::{ComboboxEvent, ComboboxState};
use gpui_kit::component::searchable_list::SearchableVec;
use gpui_kit::component::{ActiveTheme, Icon, IndexPath, Sizable as _};
use gpui_kit::*;

use crate::i18n::{LANGUAGES, Language, t};

/// Spur 度量 token。
const TITLEBAR_H: f32 = 38.0;
const HEADER_H: f32 = 44.0;
/// 原生 Windows 标题栏按钮宽度（系统度量）。
const CAPTION_W: f32 = 46.0;
const SIDEBAR_W: f32 = 216.0;
/// 收起态侧边栏宽（逻辑像素）：只剩图标列，内容区多出 164px。
const SIDEBAR_COLLAPSED_W: f32 = 52.0;

/// 唯一的小字号阶梯（逻辑像素）。各工具视图共用，见 `tools/*`。
pub(crate) const TEXT_SM: f32 = 12.0;
pub(crate) const TEXT_MD: f32 = 13.0;
pub(crate) const TEXT_LG: f32 = 15.0;

/// 网格卡片基准宽 + 间隙（逻辑像素）：列数按内容区实时宽度算，
/// 各工具的卡片网格共用，保证每行撑满且不断行错位。
const GRID_CARD_W: f32 = 232.0;
const GRID_GAP: f32 = 8.0;
/// 内容区左右内边距（工具页根 `px(16.)` 两边，列数公式里扣掉）。
const PAGE_PAD_X: f32 = 32.0;

/// 卡片网格布局：列数 + 网格定宽，一起算好往下传。
/// 定宽必须显式给像素：flex 链默认不拉伸，`w_full` 的百分比较到不定宽
/// 父级会退化成 auto，容器被内容挤小，轨道再怎么 1fr 也撑不满。
/// 调用方把定宽直接设到网格上（`min_w_0` 照带，防超宽内容顶出去）。
#[derive(Clone, Copy)]
pub(crate) struct GridLayout {
    /// 列数（2..=6）。
    pub cols: u16,
    /// 网格定宽（逻辑像素，等于内容区宽）。
    pub width: f32,
}

/// 网格布局：按实时窗宽算（resize 即换），列数夹在 2..=6 之间，
/// 定宽恒等于内容区宽（最小窗口两列不挤，超宽屏六列不空）。
/// 侧边栏宽传动画采样值（`render` 里一帧只采一次）：收起/展开时卡片
/// 跟着侧边栏一起走，不用等落点再跳。
pub(crate) fn grid_layout(window: &Window, sidebar_w: f32) -> GridLayout {
    let content = (window.bounds().size.width.as_f32() - sidebar_w - PAGE_PAD_X).max(0.);
    let cols = (((content + GRID_GAP) / (GRID_CARD_W + GRID_GAP)) as u16).clamp(2, 6);
    GridLayout { cols, width: content }
}

/// 侧边栏宽动画采样：目标由收起态定，连点反转靠 spring 保位置和速度
/// （目标驱动，不是 one-shot，见官方 animation 文档）；减弱动态下直接
/// 落目标，不请求多余帧。首帧直接采用目标，启动不播入场。
/// 通道 `"sidebar-width"` 一帧只在这里采一次，sidebar 和网格共用返回值——
/// 同一通道一帧多采会互相覆盖 retained 的位置/速度。
pub(crate) fn sidebar_width(window: &mut Window, cx: &mut App, collapsed: bool) -> f32 {
    let target = if collapsed {
        SIDEBAR_COLLAPSED_W
    } else {
        SIDEBAR_W
    };
    spring(
        "sidebar-width",
        target,
        cx.theme().motion_tokens().spring_move,
        window,
        cx,
    )
}

/// 工具标识：导航、标题、图标都从它派生，保证三处不脱节。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolId {
    Welcome,
    Screenshot,
    Proxy,
    AiRelay,
    Coffee,
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
            // “咖啡”
            ToolId::Coffee => t("app.tool_coffee"),
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
            // “端起咖啡，电脑不睡”
            ToolId::Coffee => t("app.tool_coffee_hint"),
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
        id: ToolId::Coffee,
        icon: IconName::Coffee,
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
        // 欢迎 / 截图 / 代理 / AI 转发 / 咖啡 / 设置
        "welcome home start",
        "screenshot capture printscreen",
        "proxy vless vpn tunnel",
        "ai relay zen models gateway",
        "coffee caffeine keepawake awake fangxiumian",
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
    /// 侧边栏收起态：随手持久化（`sidebar_collapsed`），启动恢复；
    /// 收起时只剩图标列，网格布局按窄栏重算。
    pub(crate) sidebar_collapsed: bool,
    /// 更新检查状态：启动查一次 + 设置页可手查；有新版标题栏右上亮图标。
    pub(crate) update: crate::updater::UpdateState,
    /// 语言下拉状态：选中即切语言；聚光面板也能改语言，render 里对账防过期。
    pub(crate) lang_combo: Entity<ComboboxState<SearchableVec<String>>>,
    /// 主题下拉状态：选中即换主题；同理对账。
    pub(crate) theme_combo: Entity<ComboboxState<SearchableVec<String>>>,
    /// 下拉事件订阅：Subscription drop 即退订，必须活着。
    combo_subs: Vec<Subscription>,
    /// 快捷键录制（呼出键/截图键）的按键拦截：见 `new` 里的说明，drop 即退订。
    hotkey_capture: Subscription,
    pub(crate) settings_theme: String,
    /// 主窗口句柄：搜索面板执行完把主窗拉回前台用（最小化也要还原）。
    /// 构造期只记 id，不读窗口几何（那时 `bounds()` 不可靠，句柄不受影响）。
    pub(crate) main_window: AnyWindowHandle,
    pub(crate) shot: crate::tools::screenshot::ShotState,
    pub(crate) proxy: crate::tools::proxy::ProxyView,
    pub(crate) spotlight: crate::spotlight::SpotlightView,
    pub(crate) airelay: crate::tools::airelay::view::AiRelayView,
    pub(crate) coffee: crate::tools::coffee::CoffeeView,
}

impl WsShell {
    /// 创建外壳。各工具的状态在自己模块里建（要 `window` 和 `cx`）。
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        // 语言下拉：选项是自称，选中当前语言；事件里切语言
        // （聚光面板改了语言不经过这里，render 对账补回来）。
        let lang_labels = Self::language_labels();
        let lang_ix = Self::language_index();
        let lang_selected = if lang_ix < lang_labels.len() {
            vec![IndexPath::new(lang_ix)]
        } else {
            Vec::new()
        };
        let lang_combo = cx.new(|cx| {
            ComboboxState::new(SearchableVec::new(lang_labels), lang_selected, window, cx)
        });
        // 主题下拉：选项来自 themes/wong.json，选中当前主题；事件里换主题。
        // 主题表坏了（空表）就不预选，下拉点开是空的，不崩。
        let theme_names = crate::theme::names();
        let theme_ix = Self::theme_index(&crate::theme::selected());
        let theme_selected = if theme_ix < theme_names.len() {
            vec![IndexPath::new(theme_ix)]
        } else {
            Vec::new()
        };
        let theme_combo = cx.new(|cx| {
            ComboboxState::new(SearchableVec::new(theme_names), theme_selected, window, cx)
        });
        // 录制拦截器要按窗口 id 过滤（面板窗的按键不算），构造期取一次就够。
        let main_window_id = window.window_handle().window_id();
        let mut this = Self {
            // 上次选中的工具：序号越界（工具增删）回落欢迎页。
            selected: crate::settings::selected_tool()
                .filter(|ix| *ix < TOOLS.len())
                .unwrap_or(0),
            // 侧边栏收起态：没存过回落展开。
            sidebar_collapsed: crate::settings::sidebar_collapsed().unwrap_or(false),
            // 更新检查从 Idle 起，扔后台，回来写状态。
            update: crate::updater::UpdateState::Idle,
            // 高亮跟随上次的选择：`theme::init` 启动时已经应用过了。
            settings_theme: crate::theme::selected(),
            main_window: window.window_handle(),
            shot: crate::tools::screenshot::ShotState::empty(),
            proxy: crate::tools::proxy::ProxyView::new(window, cx),
            spotlight: crate::spotlight::SpotlightView::new(cx),
            airelay: crate::tools::airelay::view::AiRelayView::new(window, cx),
            coffee: crate::tools::coffee::CoffeeView::new(window, cx),
            lang_combo,
            theme_combo,
            // 订阅先空着，下面建完再填。
            combo_subs: Vec::new(),
            // 同上，录制拦截的订阅在下面建。
            hotkey_capture: Subscription::new(|| {}),
        };
        // 快捷键录制（呼出键 + 截图键共用一个拦截器）：走
        // `App::intercept_keystrokes`（窗口级、在动作解析之前）。
        //
        // **不能**用元素上的 `capture_key_down`：那个只跑在聚焦路径上，
        // 而录制一开，承载按钮的那一行就被“按下快捷键…”的提示行顶掉了
        // （截图键那边还额外 `window.blur()` 扔焦点）。按钮卸载 → 焦点丢失
        // → 聚焦路径塌回根节点 → 行级/根级监听都收不到按键，录制成了死键。
        // 拦截器不看焦点，两个录制都稳；录制期间 `stop_propagation` 顺带
        // 挡住回车/空格把刚点的按钮再点一遍。
        let weak = cx.entity().downgrade();
        this.hotkey_capture = cx.intercept_keystrokes(move |event, window, cx| {
            // 面板窗/覆盖层窗的按键不认：录制只在主窗发起。
            if window.window_handle().window_id() != main_window_id {
                return;
            }
            let (shot, palette) = weak
                .read_with(cx, |this, _| (this.shot.recording, this.spotlight.recording))
                .unwrap_or((false, false));
            if !shot && !palette {
                return;
            }
            cx.stop_propagation();
            let stroke = event.keystroke.clone();
            let _ = weak.update(cx, |this, cx| {
                if shot {
                    this.record_shot_hotkey(&stroke, cx);
                } else {
                    this.record_hotkey(&stroke, cx);
                }
            });
        });
        // 下拉事件：Change 在每次点选后触发（单选随后还有 Confirm，值一样，
        // 只认 Change）。选中即应用 + 重绘，实体自己的选中态不用再动。
        let lang_sub = cx.subscribe(
            &this.lang_combo,
            |_this, _, event: &ComboboxEvent<SearchableVec<String>>, cx| {
                if let ComboboxEvent::Change(values) = event {
                    if let Some(value) = values.first() {
                        crate::i18n::set_language(Self::language_from_label(value));
                        cx.notify();
                    }
                }
            },
        );
        let theme_sub = cx.subscribe(
            &this.theme_combo,
            |this, _, event: &ComboboxEvent<SearchableVec<String>>, cx| {
                if let ComboboxEvent::Change(values) = event {
                    if let Some(name) = values.first() {
                        // 换主题并重绘所有窗口；名字对不上什么都不做。
                        if crate::theme::apply(cx, name) {
                            this.settings_theme = name.clone();
                        }
                        cx.notify();
                    }
                }
            },
        );
        this.combo_subs = vec![lang_sub, theme_sub];
        // 启动查一次新版：后台跑，失败静默（标题栏不亮灯，设置页可手查）。
        this.check_update(cx);
        this
    }

    /// 语言下拉选项：各语言的自称，中英两列写的一样，locale 怎么切都不用重建。
    fn language_labels() -> Vec<String> {
        LANGUAGES
            .iter()
            .map(|lang| match lang {
                // “中文”
                Language::Chinese => t("app.settings_lang_zh"),
                // “English”
                Language::English => t("app.settings_lang_en"),
            })
            .collect()
    }

    /// 当前语言在下拉选项里的序号：对不上回落 0（中文）。
    fn language_index() -> usize {
        LANGUAGES
            .iter()
            .position(|lang| *lang == crate::i18n::language())
            .unwrap_or(0)
    }

    /// 下拉值反查语言：命中英文自称即英文，否则中文（下拉里只有这两项）。
    /// 纯函数，好单测；locale 相关的查表留在调用方。
    fn language_from_label(value: &str) -> Language {
        if value == t("app.settings_lang_en") {
            Language::English
        } else {
            Language::Chinese
        }
    }

    /// 当前主题在下拉选项里的序号：主题文件被改得对不上就回落 0。
    fn theme_index(theme: &str) -> usize {
        crate::theme::names()
            .iter()
            .position(|name| name == theme)
            .unwrap_or(0)
    }

    /// 下拉选中对账：聚光面板改语言/主题不经过下拉实体，过期就在渲染前补上。
    /// `set_selected_indices` 只写状态不发事件，严格条件（不一致才写），
    /// 写完即收敛，不会逐帧打扰。跨实体 `update` 不和渲染借冲突，
    /// 同实体才 panic（gpui entity_map 的 double lease 干的）。
    fn sync_combos(&self, window: &mut Window, cx: &mut Context<Self>) {
        let labels = Self::language_labels();
        let lang_ix = Self::language_index();
        if labels.get(lang_ix).is_some() {
            let _ = self.lang_combo.update(cx, |state, cx| {
                if state.selected_value() != labels.get(lang_ix).cloned() {
                    state.set_selected_indices([IndexPath::new(lang_ix)], window, cx);
                }
            });
        }
        let names = crate::theme::names();
        let theme_ix = Self::theme_index(&self.settings_theme);
        if names.get(theme_ix).is_some() {
            let _ = self.theme_combo.update(cx, |state, cx| {
                if state.selected_value() != names.get(theme_ix).cloned() {
                    state.set_selected_indices([IndexPath::new(theme_ix)], window, cx);
                }
            });
        }
    }

    /// 侧边栏选中并持久化：导航点击和搜索面板跳转共用，行为一致。
    /// 切工具顺手退出快捷键录制（录制行不在了还记着会误事）。
    pub(crate) fn select_tool(&mut self, ix: usize) {
        self.selected = ix;
        self.spotlight.recording = false;
        self.shot.recording = false;
        if let Err(e) = crate::settings::set_selected_tool(ix) {
            eprintln!("存选中工具失败: {e}");
        }
    }

    /// 侧边栏收起/展开：翻转即持久化，失败只记日志，通知重绘让网格重算。
    fn toggle_sidebar(&mut self, cx: &mut Context<Self>) {
        self.sidebar_collapsed = !self.sidebar_collapsed;
        if let Err(e) = crate::settings::set_sidebar_collapsed(self.sidebar_collapsed) {
            eprintln!("存侧边栏收起态失败: {e}");
        }
        cx.notify();
    }

    /// 查新版（启动调一次 + 设置页手动）：防重入，网络活扔后台，
    /// 回来写状态 + 重绘。失败只记状态，界面照常能用。
    pub(crate) fn check_update(&mut self, cx: &mut Context<Self>) {
        // 正在查就别叠：GitHub 未登录限流 60 次/小时，省着点。
        if matches!(self.update, crate::updater::UpdateState::Checking) {
            return;
        }
        self.update = crate::updater::UpdateState::Checking;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let outcome = cx
                .background_executor()
                .spawn(async move {
                    match crate::updater::check_latest() {
                        Err(e) => crate::updater::UpdateState::Failed(e),
                        Ok(latest) => {
                            if crate::updater::is_newer(
                                &latest.version,
                                crate::updater::CURRENT_VERSION,
                            ) {
                                crate::updater::UpdateState::Available {
                                    version: latest.version,
                                    url: latest.url,
                                }
                            } else {
                                crate::updater::UpdateState::UpToDate
                            }
                        }
                    }
                })
                .await;
            cx.update(|cx| {
                let _ = this.update(cx, |view, cx| {
                    view.update = outcome;
                    cx.notify();
                });
            });
        })
        .detach();
    }

    /// 打开下载页（标题栏图标 / 设置页入口）：浏览器拉起 Release 页，
    /// 装不装用户在那边定。拉起失败只记日志，状态不动（下载页还在）。
    pub(crate) fn open_update_page(&self, cx: &mut Context<Self>) {
        let url = match &self.update {
            crate::updater::UpdateState::Available { url, .. } => url.clone(),
            _ => return,
        };
        cx.spawn(async move |_this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { crate::updater::open_release_page(&url) })
                .await;
            if let Err(e) = result {
                eprintln!("打开下载页失败: {e}");
            }
        })
        .detach();
    }

    fn render_titlebar(&self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        // “Wong Tools”
        let app_name = t("app.app_name");
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
        // 侧边栏收起按钮紧靠标题右侧：官方 SidebarToggleButton，
        // 图标随状态在 PanelLeftClose/Open 间切；无障碍标签按状态给动作名。
        let collapsed = self.sidebar_collapsed;
        let toggle_label = if collapsed {
            // “展开侧边栏”
            t("app.sidebar_expand")
        } else {
            // “收起侧边栏”
            t("app.sidebar_collapse")
        };

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
            // 侧边栏收起按钮紧靠标题右边：官方 SidebarToggleButton 没透出
            // tooltip，自建同款（ghost 小图标 + 开合图标 + hover 提示）。
            // 独立孩子，不进拖拽区，不然点不到。
            .child({
                let icon = if collapsed {
                    IconName::PanelLeftOpen
                } else {
                    IconName::PanelLeftClose
                };
                Button::new("sidebar-toggle")
                    .ghost()
                    .small()
                    .icon(Icon::new(icon).size(px(14.)))
                    // hover 提示 + 无障碍标签：按状态给动作名，走文案。
                    .tooltip(toggle_label.clone())
                    .accessibility_label(toggle_label)
                    .cursor_pointer()
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.toggle_sidebar(cx);
                    }))
            })
            // 品牌和标题栏按钮之间的弹性拖拽区。
            .child(div().flex_1().h_full().window_control_area(WindowControlArea::Drag))
            // 有新版才亮更新图标：ghost 下载箭头，hover 报版本号，点即跳下载页。
            .children(match &self.update {
                crate::updater::UpdateState::Available { version, .. } => {
                    // “发现新版本 v0.1.2，点击前往下载”
                    let tip = format!(
                        "{} v{version}，{}",
                        t("app.update_available"),
                        t("app.update_open_page")
                    );
                    vec![
                        Button::new("app-update")
                            .ghost()
                            .small()
                            .icon(Icon::new(IconName::Download).size(px(14.)))
                            .tooltip(tip.clone())
                            .accessibility_label(tip)
                            .cursor_pointer()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.open_update_page(cx);
                            }))
                            .into_any_element(),
                    ]
                }
                _ => Vec::new(),
            })
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

    fn render_sidebar(&self, sidebar_w: f32, cx: &mut Context<Self>) -> impl IntoElement {
        let collapsed = self.sidebar_collapsed;
        let sidebar_bg = cx.theme().sidebar;
        let sidebar_border = cx.theme().sidebar_border;
        let row_bg = cx.theme().sidebar_accent;
        let row_fg = cx.theme().sidebar_accent_foreground;
        let idle_fg = cx.theme().sidebar_foreground;

        div()
            .flex_none()
            .flex()
            .flex_col()
            .w(px(sidebar_w))
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
                let row = div()
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
                    // 行标签：工具名，中文见 locales/ui.yml（如“欢迎”）；
                    // 收起态藏掉，只剩图标列。
                    .children(if collapsed {
                        Vec::new()
                    } else {
                        vec![
                            div()
                                .text_size(px(TEXT_MD))
                                .child(tool.id.name())
                                .into_any_element(),
                        ]
                    });
                // 收起态只剩一个图标，居中；展开态图标 + 标签左排。
                if collapsed { row.justify_center() } else { row }
            }))
    }

    fn render_content(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
        sidebar_w: f32,
    ) -> impl IntoElement {
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
            .child(self.render_tool_body(tool, window, cx, sidebar_w))
    }

    /// 各工具的主体：每个分支调自己视图的渲染（欢迎是占位，其他都是真工具）。
    fn render_tool_body(
        &self,
        tool: &ToolDef,
        window: &Window,
        cx: &mut Context<Self>,
        sidebar_w: f32,
    ) -> impl IntoElement {
        match tool.id {
            ToolId::Screenshot => self
                .render_shot(window, cx, sidebar_w)
                .into_any_element(),
            ToolId::Proxy => self.render_proxy(window, cx, sidebar_w).into_any_element(),
            ToolId::AiRelay => self.render_airelay(window, cx, sidebar_w).into_any_element(),
            ToolId::Coffee => self.render_coffee(cx).into_any_element(),
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
        // 侧边栏宽一帧采一次：sidebar 和内容区网格共用，动画全程对齐。
        let sidebar_w = sidebar_width(window, cx, self.sidebar_collapsed);
        // 下拉选中对账：聚光面板在外改了语言/主题就补上，只多一帧。
        self.sync_combos(window, cx);

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
                    .child(self.render_sidebar(sidebar_w, cx))
                    .child(self.render_content(window, cx, sidebar_w)),
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
        // 咖啡断言跟进程走：守卫 drop 即放行（被杀则系统自动回收）。
        self.coffee.shutdown();
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
    use super::WsShell;
    use crate::i18n::{LANGUAGES, Language, language, lock_locale, set_language};
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

    #[test]
    fn combo_label_maps_back_to_language() {
        // 下拉值反查：命中英文自称即英文，其他一律中文（下拉里只有这两项）。
        // 自称两列写的一样，locale 怎么切都不影响。
        let _guard = lock_locale();
        let _settings = TempSettings::install("shell-lang-label");
        let prev = language();
        for lang in LANGUAGES {
            set_language(*lang);
            assert_eq!(WsShell::language_from_label("English"), Language::English);
            assert_eq!(WsShell::language_from_label("中文"), Language::Chinese);
            assert_eq!(WsShell::language_from_label("???"), Language::Chinese);
        }
        set_language(prev);
    }
}