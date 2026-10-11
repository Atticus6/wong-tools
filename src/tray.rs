//! 系统托盘：常驻图标 + 菜单（显示主窗口/截图/识别文字/退出），关窗不退的底气。
//!
//! 事件走通道 + 前台定时轮询：`App::spawn` 的 future 跑在主线程，
//! 托盘库的回调可不是——直接在里面碰 `App` 会炸 RefCell。
//! 托盘建不出来（比如 Linux 缺 indicator）只记日志，应用照常用。

use std::time::Duration;

use gpui_kit::*;

use crate::i18n::t;

/// 主窗口状态：句柄（唤出/存档用）+ 内容 entity（调 WsShell 方法用）。
/// 注意存 entity 不能存句柄 downcast：`open_window` 把内容包进了一层
/// `base::Root`，根视图类型是 Root 不是 WsShell。
/// 关窗钩子按 `WindowId` 两边一起清，托盘下次重开不复活僵尸。
#[derive(Default)]
pub(crate) struct MainWindow {
    pub handle: Option<AnyWindowHandle>,
    pub shell: Option<Entity<crate::shell::WsShell>>,
}

impl Global for MainWindow {}

/// 托盘图标句柄：只负责持有，drop 就从托盘消失，所以必须存着。
/// `shot_item`/`ocr_item` 留着改键后刷新菜单后缀（muda 文本静态）。
/// 从不读 `tray` 字段，`dead_code` 误报直接允许。
struct TrayIconHandle {
    #[allow(dead_code)]
    tray: Option<tray_icon::TrayIcon>,
    shot_item: Option<muda::MenuItem>,
    ocr_item: Option<muda::MenuItem>,
}

impl Global for TrayIconHandle {}

const SHOW_ID: &str = "tray-show";
const SHOT_ID: &str = "tray-shot";
const OCR_ID: &str = "tray-ocr";
const QUIT_ID: &str = "tray-quit";
/// 截图全局键：微信/QQ 同款，mac/Win/Linux 都无系统占用。
/// 截图页设置 tab 可改（存档 + 热重绑 + 菜单后缀刷新）。
pub const SHOT_HOTKEY: &str = "Ctrl+Alt+A";
/// 识别全局键：直达 OCR 框选，松手即识别。截图页设置 tab 可改
/// （和截图键同一套：存档 + 热重绑 + 菜单后缀刷新）。
pub const OCR_HOTKEY: &str = "Ctrl+Alt+O";
/// 全局键连按去抖：覆盖层开着再按会叠两层（和连点区域按钮一样），先拦 1s。
/// 截图/识别各记各的上次触发，共用这个间隔。
const HOTKEY_DEBOUNCE_MS: u64 = 1000;

/// 截图热键状态：句柄 + 上次触发。托盘有没有都照按（无托盘环境热键是唯一的远端入口）。
pub(crate) struct ShotHotkey {
    pub(crate) handle: crate::spotlight::HotkeyHandle,
    last_fire: Option<std::time::Instant>,
}

impl Global for ShotHotkey {}
/// 识别热键状态：和截图键同一套（独立句柄 + 独立去抖，互不影响）。
pub(crate) struct OcrHotkey {
    pub(crate) handle: crate::spotlight::HotkeyHandle,
    last_fire: Option<std::time::Instant>,
}

impl Global for OcrHotkey {}
/// 轮询间隔：托盘事件低频，150ms 不烧电也不拖沓。
const POLL_MS: u64 = 150;
/// 托盘图标 macOS 走单色模板（菜单栏惯例，随深浅自动反色），
/// Win/Linux 走彩色 32px（浅色任务栏上白色看不见）。
#[cfg(target_os = "macos")]
const TRAY_ICON: &[u8] = include_bytes!("../assets/icons/tray-template.png");
#[cfg(not(target_os = "macos"))]
const TRAY_ICON: &[u8] = include_bytes!("../assets/icons/hicolor/32x32/apps/wong-tools.png");

/// 建托盘 + 起轮询。失败只记日志不崩（无托盘环境照常用）。
/// 热键与托盘无关，单独初始化：没托盘也要能按。
/// 截图/识别键都读存档（设置 tab 可改），没存过回落默认。
pub fn init(cx: &mut App) {
    cx.set_global(TrayIconHandle {
        tray: None,
        shot_item: None,
        ocr_item: None,
    });
    match build_tray(&shot_hotkey_spec(), &ocr_hotkey_spec()) {
        Ok((tray, shot, ocr)) => {
            cx.set_global(TrayIconHandle {
                tray: Some(tray),
                shot_item: Some(shot),
                ocr_item: Some(ocr),
            });
        }
        Err(e) => eprintln!("托盘不可用，常驻失效（关窗后需重起进程）: {e}"),
    }
    let mut shot_handle = crate::spotlight::HotkeyHandle::new();
    if let Err(e) = shot_handle.rebind(&shot_hotkey_spec()) {
        eprintln!("截图热键注册失败（可能被占用），托盘菜单照常可用: {e}");
    }
    cx.set_global(ShotHotkey {
        handle: shot_handle,
        last_fire: None,
    });
    // 识别键只在有引擎的平台注册：Linux 没 Vision/WinRT，注册了也进不来。
    let mut ocr_handle = crate::spotlight::HotkeyHandle::new();
    if crate::tools::screenshot::ocr::is_available() {
        if let Err(e) = ocr_handle.rebind(&ocr_hotkey_spec()) {
            eprintln!("识别热键注册失败（可能被占用），托盘菜单照常可用: {e}");
        }
    }
    cx.set_global(OcrHotkey {
        handle: ocr_handle,
        last_fire: None,
    });
    cx.spawn(pump).detach();
}

/// 当前截图键原文：存档优先，没存过回落默认（纯函数，好单测）。
pub(crate) fn shot_hotkey_spec() -> String {
    crate::settings::shot_hotkey().unwrap_or_else(|| SHOT_HOTKEY.to_string())
}

/// 当前识别键原文：存档优先，没存过回落默认（纯函数，好单测）。
pub(crate) fn ocr_hotkey_spec() -> String {
    crate::settings::ocr_hotkey().unwrap_or_else(|| OCR_HOTKEY.to_string())
}

fn tray_image() -> Result<tray_icon::Icon, String> {
    let img = image::load_from_memory(TRAY_ICON)
        .map_err(|e| format!("托盘图标解码失败: {e}"))?
        .to_rgba8();
    let (w, h) = (img.width(), img.height());
    tray_icon::Icon::from_rgba(img.into_raw(), w, h).map_err(|e| format!("托盘图标非法: {e:?}"))
}

fn build_tray(
    shot_spec: &str,
    ocr_spec: &str,
) -> Result<(tray_icon::TrayIcon, muda::MenuItem, muda::MenuItem), String> {
    let menu = muda::Menu::new();
    // “显示主窗口” / “截图  Ctrl + Alt + A” / “识别文字  Ctrl + Alt + O” / “退出”
    // （两键都带热键后缀，改键后调下面的刷新）。
    // Linux 没 OCR 引擎，识别项置灰（菜单构建时定死，见 AGENTS）。
    let show = muda::MenuItem::with_id(SHOW_ID, t("app.tray_show"), true, None);
    let shot = muda::MenuItem::with_id(SHOT_ID, shot_menu_label(shot_spec), true, None);
    let ocr = muda::MenuItem::with_id(
        OCR_ID,
        ocr_menu_label(ocr_spec),
        crate::tools::screenshot::ocr::is_available(),
        None,
    );
    let quit = muda::MenuItem::with_id(QUIT_ID, t("app.tray_quit"), true, None);
    menu.append(&show)
        .and_then(|_| menu.append(&shot))
        .and_then(|_| menu.append(&ocr))
        .and_then(|_| menu.append(&muda::PredefinedMenuItem::separator()))
        .and_then(|_| menu.append(&quit))
        .map_err(|e| format!("托盘菜单组装失败: {e}"))?;
    let icon = tray_image()?;
    let builder = tray_icon::TrayIcon::builder()
        .with_menu(Box::new(menu))
        // “Wong Tools”
        .with_tooltip(t("app.window_title"))
        // 右键出菜单；左键处理器留给能收到左键事件的平台（Windows）。
        // 注意：macOS 上图标挂了菜单后左键必然弹菜单（系统行为，实测左键
        // 原生事件根本不分发），左键直唤无解，保持现状别折腾。
        .with_menu_on_left_click(false);
    // macOS 菜单栏只认模板图（单色随深浅反色）；其他平台用彩色。
    #[cfg(target_os = "macos")]
    let builder = builder.with_icon_templated(icon);
    #[cfg(not(target_os = "macos"))]
    let builder = builder.with_icon(icon);
    let tray = builder.build().map_err(|e| format!("托盘创建失败: {e}"))?;
    Ok((tray, shot, ocr))
}

/// 截图菜单项文本：`截图  Ctrl + Alt + A` 这种，构建和刷新共用。
fn shot_menu_label(spec: &str) -> String {
    format!("{}  {}", t("app.tray_shot"), crate::spotlight::display_hotkey(spec))
}

/// 识别菜单项文本：`识别文字  Ctrl + Alt + O` 这种，构建和刷新共用。
fn ocr_menu_label(spec: &str) -> String {
    format!("{}  {}", t("app.tray_ocr"), crate::spotlight::display_hotkey(spec))
}

/// 改键后刷新截图菜单项后缀（muda 文本静态，不调一直是旧键）。
/// 托盘没建成（句柄 None）就跳过，下次启动读存档自然对上。
pub(crate) fn update_shot_menu_label(cx: &mut App, spec: &str) {
    if let Some(item) = &cx.global::<TrayIconHandle>().shot_item {
        item.set_text(shot_menu_label(spec));
    }
}

/// 改键后刷新识别菜单项后缀（同截图，muda 文本静态）。
pub(crate) fn update_ocr_menu_label(cx: &mut App, spec: &str) {
    if let Some(item) = &cx.global::<TrayIconHandle>().ocr_item {
        item.set_text(ocr_menu_label(spec));
    }
}

/// 截图键去抖：1s 内连按只认第一次（覆盖层叠窗防护）。
/// 泵里调，`AsyncApp::update` 把读写收进主线程。
fn debounce_shot(cx: &mut AsyncApp) -> bool {
    let now = std::time::Instant::now();
    cx.update(|cx| {
        let slot = cx.global_mut::<ShotHotkey>();
        let ok = slot.last_fire.is_none_or(|t| {
            now.duration_since(t).as_millis() >= HOTKEY_DEBOUNCE_MS as u128
        });
        if ok {
            slot.last_fire = Some(now);
        }
        ok
    })
}

/// 识别键去抖：和截图键同一间隔，各记各的上次触发。
fn debounce_ocr(cx: &mut AsyncApp) -> bool {
    let now = std::time::Instant::now();
    cx.update(|cx| {
        let slot = cx.global_mut::<OcrHotkey>();
        let ok = slot.last_fire.is_none_or(|t| {
            now.duration_since(t).as_millis() >= HOTKEY_DEBOUNCE_MS as u128
        });
        if ok {
            slot.last_fire = Some(now);
        }
        ok
    })
}

/// 事件泵：150ms 扫一次两个通道，全在主线程里分发。
async fn pump(cx: &mut AsyncApp) {
    use tray_icon::{MouseButton, TrayIconEvent};
    let mut ticks = 0u32;
    loop {
        cx.background_executor()
            .timer(Duration::from_millis(POLL_MS))
            .await;
        ticks += 1;
        // 补注册：启动太早管理器建不出来时 id 一直是 None，隔 ~6s 重试一次
        // （静默，成了就 stick；成过的不再碰）。截图/识别各管各的句柄。
        if ticks % 40 == 0 {
            cx.update(|cx| {
                let slot = cx.global_mut::<ShotHotkey>();
                if slot.handle.current_id().is_none() {
                    let _ = slot.handle.rebind(&shot_hotkey_spec());
                }
                if crate::tools::screenshot::ocr::is_available() {
                    let slot = cx.global_mut::<OcrHotkey>();
                    if slot.handle.current_id().is_none() {
                        let _ = slot.handle.rebind(&ocr_hotkey_spec());
                    }
                }
            });
        }
        while let Ok(ev) = TrayIconEvent::receiver().try_recv() {
            match ev {
                TrayIconEvent::Click {
                    button: MouseButton::Left,
                    ..
                }
                | TrayIconEvent::DoubleClick { .. } => {
                    // 按下抬起都认：有的平台只给 Down；唤出幂等，多认无害。
                    cx.update(show_main);
                }
                _ => {}
            }
        }
        let mut quit = false;
        while let Ok(ev) = muda::MenuEvent::receiver().try_recv() {
            match menu_action(&ev.id.0) {
                TrayAction::Show => cx.update(show_main),
                TrayAction::Shot => cx.update(take_shot),
                TrayAction::Ocr => cx.update(take_ocr),
                TrayAction::Quit => quit = true,
                TrayAction::Ignore => {}
            }
        }
        // 全局键：对上注册 id 才算数（和聚光面板同款只认 Pressed）。
        // 截图/识别各对各的 id，各走各的去抖，分发到不同的覆盖层入口。
        while let Ok(ev) = global_hotkey::GlobalHotKeyEvent::receiver().try_recv() {
            if ev.state != global_hotkey::HotKeyState::Pressed {
                continue;
            }
            let shot_mine = cx.update(|cx| {
                let current = cx.global::<ShotHotkey>().handle.current_id();
                current.is_some_and(|id| id == ev.id)
            });
            if shot_mine {
                if debounce_shot(cx) {
                    cx.update(take_shot);
                }
                continue;
            }
            let ocr_mine = cx.update(|cx| {
                let current = cx.global::<OcrHotkey>().handle.current_id();
                current.is_some_and(|id| id == ev.id)
            });
            if ocr_mine {
                if debounce_ocr(cx) {
                    cx.update(take_ocr);
                }
            }
        }
        if quit {
            // 点托盘的退出才是真退出：走正常退出钩子（存档/清代理都跑）。
            cx.update(|cx| cx.quit());
            return;
        }
    }
}

/// 托盘菜单分发（纯逻辑，好单测）：泵里只管调它。
#[derive(Debug, PartialEq, Eq)]
enum TrayAction {
    Show,
    Shot,
    Ocr,
    Quit,
    Ignore,
}

fn menu_action(id: &str) -> TrayAction {
    if id == SHOW_ID {
        TrayAction::Show
    } else if id == SHOT_ID {
        TrayAction::Shot
    } else if id == OCR_ID {
        TrayAction::Ocr
    } else if id == QUIT_ID {
        TrayAction::Quit
    } else {
        TrayAction::Ignore
    }
}

/// 主窗可用状态：活着就拿来，关了就重开。唤出/截图两条路共用。
fn ensure_main(cx: &mut App) -> Option<(AnyWindowHandle, Entity<crate::shell::WsShell>)> {
    let main = cx.global::<MainWindow>();
    if let (Some(h), Some(s)) = (main.handle, main.shell.clone()) {
        if h.update(cx, |_, _, _| {}).is_ok() {
            return Some((h, s));
        }
    }
    match crate::open_main_window(cx) {
        Ok((h, s)) => {
            *cx.global_mut::<MainWindow>() = MainWindow {
                handle: Some(h),
                shell: Some(s.clone()),
            };
            Some((h, s))
        }
        Err(e) => {
            eprintln!("托盘唤起主窗口失败: {e}");
            None
        }
    }
}

/// 唤出主窗口：还活着就拉到前台，关了就重开。
fn show_main(cx: &mut App) {
    if let Some((h, _)) = ensure_main(cx) {
        let _ = h.update(cx, |_, window, _| {
            window.activate_window();
        });
    }
}

/// 托盘截图：借主窗的 WsShell 起区域覆盖层（写回/报错都落它状态行）。
/// 主窗关了就先开（落位按存档），再起覆盖层。
fn take_shot(cx: &mut App) {
    if let Some((_, shell)) = ensure_main(cx) {
        let _ = shell.update(cx, |shell, cx| shell.open_region(cx));
    }
}

/// 托盘识别：同截图的起法，进的是 OCR 直达覆盖层（松手即识别）。
/// 主窗关了同样先开；没引擎菜单项置灰，正常点不进来。
fn take_ocr(cx: &mut App) {
    if let Some((_, shell)) = ensure_main(cx) {
        let _ = shell.update(cx, |shell, cx| shell.open_region_for_ocr(cx));
    }
}

#[cfg(test)]
mod tests {
    // 注意：这里不要 `use super::*`——它会把 gpui 自带的 `#[test]` 宏引进
    // 来，盖住 Rust 内置的 test 属性导致递归展开失败。只显式引入用到的项。
    use super::OCR_HOTKEY;
    use super::OCR_ID;
    use super::QUIT_ID;
    use super::SHOT_HOTKEY;
    use super::SHOT_ID;
    use super::SHOW_ID;
    use super::TrayAction;
    use super::menu_action;

    #[test]
    fn menu_dispatch_routes_show_quit_and_unknown() {
        assert_eq!(menu_action(SHOW_ID), TrayAction::Show);
        assert_eq!(menu_action(SHOT_ID), TrayAction::Shot);
        assert_eq!(menu_action(OCR_ID), TrayAction::Ocr);
        assert_eq!(menu_action(QUIT_ID), TrayAction::Quit);
        assert_eq!(menu_action("tray-nope"), TrayAction::Ignore);
        assert_ne!(SHOW_ID, QUIT_ID, "菜单 id 重了就全乱套");
        assert_ne!(SHOT_ID, OCR_ID, "截图和识别 id 重了就全乱套");
    }

    #[test]
    fn shot_hotkey_spec_registers() {
        // 默认截图键必须过三道校验（全局解析 + 修饰键），不然静默无键。
        assert!(crate::spotlight::parse_hotkey_spec(SHOT_HOTKEY).is_ok());
    }

    #[test]
    fn ocr_hotkey_spec_registers() {
        // 默认识别键同截图键：非法默认值等于没键，启动即少一个入口。
        assert!(crate::spotlight::parse_hotkey_spec(OCR_HOTKEY).is_ok());
        assert_ne!(OCR_HOTKEY, SHOT_HOTKEY, "识别键和截图键撞了会抢事件");
    }
}
