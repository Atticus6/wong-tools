//! 系统托盘：常驻图标 + 菜单（显示主窗口/退出），关窗不退的底气。
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
/// `shot_item` 留着改键后刷新菜单后缀（muda 文本静态）。
/// 从不读 `tray` 字段，`dead_code` 误报直接允许。
struct TrayIconHandle {
    #[allow(dead_code)]
    tray: Option<tray_icon::TrayIcon>,
    shot_item: Option<muda::MenuItem>,
}

impl Global for TrayIconHandle {}

const SHOW_ID: &str = "tray-show";
const SHOT_ID: &str = "tray-shot";
const QUIT_ID: &str = "tray-quit";
/// 截图全局键：微信/QQ 同款，mac/Win/Linux 都无系统占用。
/// 截图页设置 tab 可改（存档 + 热重绑 + 菜单后缀刷新）。
pub const SHOT_HOTKEY: &str = "Ctrl+Alt+A";
/// 热键连按去抖：覆盖层开着再按会叠两层（和连点区域按钮一样），先拦 1s。
const SHOT_DEBOUNCE_MS: u64 = 1000;

/// 截图热键状态：句柄 + 上次触发。托盘有没有都照按（无托盘环境热键是唯一的远端入口）。
pub(crate) struct ShotHotkey {
    pub(crate) handle: crate::spotlight::HotkeyHandle,
    last_fire: Option<std::time::Instant>,
}

impl Global for ShotHotkey {}
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
/// 截图键读存档（设置 tab 可改），没存过回落默认。
pub fn init(cx: &mut App) {
    cx.set_global(TrayIconHandle {
        tray: None,
        shot_item: None,
    });
    match build_tray(&shot_hotkey_spec()) {
        Ok((tray, shot)) => {
            cx.set_global(TrayIconHandle {
                tray: Some(tray),
                shot_item: Some(shot),
            });
        }
        Err(e) => eprintln!("托盘不可用，常驻失效（关窗后需重起进程）: {e}"),
    }
    let mut handle = crate::spotlight::HotkeyHandle::new();
    if let Err(e) = handle.rebind(&shot_hotkey_spec()) {
        eprintln!("截图热键注册失败（可能被占用），托盘菜单照常可用: {e}");
    }
    cx.set_global(ShotHotkey {
        handle,
        last_fire: None,
    });
    cx.spawn(pump).detach();
}

/// 当前截图键原文：存档优先，没存过回落默认（纯函数，好单测）。
pub(crate) fn shot_hotkey_spec() -> String {
    crate::settings::shot_hotkey().unwrap_or_else(|| SHOT_HOTKEY.to_string())
}

fn tray_image() -> Result<tray_icon::Icon, String> {
    let img = image::load_from_memory(TRAY_ICON)
        .map_err(|e| format!("托盘图标解码失败: {e}"))?
        .to_rgba8();
    let (w, h) = (img.width(), img.height());
    tray_icon::Icon::from_rgba(img.into_raw(), w, h).map_err(|e| format!("托盘图标非法: {e:?}"))
}

fn build_tray(spec: &str) -> Result<(tray_icon::TrayIcon, muda::MenuItem), String> {
    let menu = muda::Menu::new();
    // “显示主窗口” / “截图  Ctrl + Alt + A” / “退出”（热键后缀展示，
    // 和 spotlight 的 display_hotkey 同一份写法；改键后调下面的刷新）。
    let show = muda::MenuItem::with_id(SHOW_ID, t("app.tray_show"), true, None);
    let shot = muda::MenuItem::with_id(SHOT_ID, shot_menu_label(spec), true, None);
    let quit = muda::MenuItem::with_id(QUIT_ID, t("app.tray_quit"), true, None);
    menu.append(&show)
        .and_then(|_| menu.append(&shot))
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
    Ok((tray, shot))
}

/// 截图菜单项文本：`截图  Ctrl + Alt + A` 这种，构建和刷新共用。
fn shot_menu_label(spec: &str) -> String {
    format!("{}  {}", t("app.tray_shot"), crate::spotlight::display_hotkey(spec))
}

/// 改键后刷新截图菜单项后缀（muda 文本静态，不调一直是旧键）。
/// 托盘没建成（句柄 None）就跳过，下次启动读存档自然对上。
pub(crate) fn update_shot_menu_label(cx: &mut App, spec: &str) {
    if let Some(item) = &cx.global::<TrayIconHandle>().shot_item {
        item.set_text(shot_menu_label(spec));
    }
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
        // （静默，成了就 stick；成过的不再碰）。
        if ticks % 40 == 0 {
            cx.update(|cx| {
                let slot = cx.global_mut::<ShotHotkey>();
                if slot.handle.current_id().is_none() {
                    let _ = slot.handle.rebind(&shot_hotkey_spec());
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
                TrayAction::Quit => quit = true,
                TrayAction::Ignore => {}
            }
        }
        // 截图全局键：对上注册 id 才算数（和聚光面板同款只认 Pressed）。
        while let Ok(ev) = global_hotkey::GlobalHotKeyEvent::receiver().try_recv() {
            if ev.state != global_hotkey::HotKeyState::Pressed {
                continue;
            }
            let mine = cx.update(|cx| {
                let current = cx.global::<ShotHotkey>().handle.current_id();
                current.is_some_and(|id| id == ev.id)
            });
            if mine {
                let now = std::time::Instant::now();
                let fire = cx.update(|cx| {
                    let slot = cx.global_mut::<ShotHotkey>();
                    let ok = slot.last_fire.is_none_or(|t| {
                        now.duration_since(t).as_millis() >= SHOT_DEBOUNCE_MS as u128
                    });
                    if ok {
                        slot.last_fire = Some(now);
                    }
                    ok
                });
                if fire {
                    cx.update(take_shot);
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
    Quit,
    Ignore,
}

fn menu_action(id: &str) -> TrayAction {
    if id == SHOW_ID {
        TrayAction::Show
    } else if id == SHOT_ID {
        TrayAction::Shot
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

#[cfg(test)]
mod tests {
    // 注意：这里不要 `use super::*`——它会把 gpui 自带的 `#[test]` 宏引进
    // 来，盖住 Rust 内置的 test 属性导致递归展开失败。只显式引入用到的项。
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
        assert_eq!(menu_action(QUIT_ID), TrayAction::Quit);
        assert_eq!(menu_action("tray-nope"), TrayAction::Ignore);
        assert_ne!(SHOW_ID, QUIT_ID, "菜单 id 重了就全乱套");
    }

    #[test]
    fn shot_hotkey_spec_registers() {
        // 默认截图键必须过三道校验（全局解析 + 修饰键），不然静默无键。
        assert!(crate::spotlight::parse_hotkey_spec(SHOT_HOTKEY).is_ok());
    }
}
