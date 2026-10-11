//! 系统托盘：常驻图标 + 菜单（显示主窗口/退出），关窗不退的底气。
//!
//! 事件走通道 + 前台定时轮询：`App::spawn` 的 future 跑在主线程，
//! 托盘库的回调可不是——直接在里面碰 `App` 会炸 RefCell。
//! 托盘建不出来（比如 Linux 缺 indicator）只记日志，应用照常用。

use std::time::Duration;

use gpui_kit::*;

use crate::i18n::t;

/// 主窗口句柄：关了就清掉（`on_window_closed` 里认 `WindowId`），
/// 托盘下次重开而不是复活僵尸。
#[derive(Default)]
pub(crate) struct MainWindow(pub Option<AnyWindowHandle>);

impl Global for MainWindow {}

/// 托盘图标句柄：只负责持有，drop 就从托盘消失，所以必须存着。
/// 从不读字段，`dead_code` 误报直接允许。
#[allow(dead_code)]
struct TrayIconHandle(Option<tray_icon::TrayIcon>);

impl Global for TrayIconHandle {}

const SHOW_ID: &str = "tray-show";
const QUIT_ID: &str = "tray-quit";
/// 轮询间隔：托盘事件低频，150ms 不烧电也不拖沓。
const POLL_MS: u64 = 150;
/// 托盘图标 macOS 走单色模板（菜单栏惯例，随深浅自动反色），
/// Win/Linux 走彩色 32px（浅色任务栏上白色看不见）。
#[cfg(target_os = "macos")]
const TRAY_ICON: &[u8] = include_bytes!("../assets/icons/tray-template.png");
#[cfg(not(target_os = "macos"))]
const TRAY_ICON: &[u8] = include_bytes!("../assets/icons/hicolor/32x32/apps/wong-tools.png");

/// 建托盘 + 起轮询。失败只记日志不崩（无托盘环境照常用）。
pub fn init(cx: &mut App) {
    cx.set_global(TrayIconHandle(None));
    match build_tray() {
        Ok(tray) => {
            cx.set_global(TrayIconHandle(Some(tray)));
            cx.spawn(pump).detach();
        }
        Err(e) => eprintln!("托盘不可用，常驻失效（关窗后需重起进程）: {e}"),
    }
}

fn tray_image() -> Result<tray_icon::Icon, String> {
    let img = image::load_from_memory(TRAY_ICON)
        .map_err(|e| format!("托盘图标解码失败: {e}"))?
        .to_rgba8();
    let (w, h) = (img.width(), img.height());
    tray_icon::Icon::from_rgba(img.into_raw(), w, h).map_err(|e| format!("托盘图标非法: {e:?}"))
}

fn build_tray() -> Result<tray_icon::TrayIcon, String> {
    let menu = muda::Menu::new();
    // “显示主窗口” / “退出”
    let show = muda::MenuItem::with_id(SHOW_ID, t("app.tray_show"), true, None);
    let quit = muda::MenuItem::with_id(QUIT_ID, t("app.tray_quit"), true, None);
    menu.append(&show)
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
    builder.build().map_err(|e| format!("托盘创建失败: {e}"))
}

/// 事件泵：150ms 扫一次两个通道，全在主线程里分发。
async fn pump(cx: &mut AsyncApp) {
    use tray_icon::{MouseButton, TrayIconEvent};
    loop {
        cx.background_executor()
            .timer(Duration::from_millis(POLL_MS))
            .await;
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
                TrayAction::Quit => quit = true,
                TrayAction::Ignore => {}
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
    Quit,
    Ignore,
}

fn menu_action(id: &str) -> TrayAction {
    if id == SHOW_ID {
        TrayAction::Show
    } else if id == QUIT_ID {
        TrayAction::Quit
    } else {
        TrayAction::Ignore
    }
}

/// 唤出主窗口：还活着就拉到前台，关了就重开。
fn show_main(cx: &mut App) {
    let alive = cx
        .global::<MainWindow>()
        .0
        .and_then(|h| {
            h.update(cx, |_, window, _| {
                window.activate_window();
            })
            .ok()
        })
        .is_some();
    if !alive {
        match crate::open_main_window(cx) {
            Ok(handle) => {
                cx.global_mut::<MainWindow>().0 = Some(handle);
            }
            Err(e) => eprintln!("托盘唤起主窗口失败: {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    // 注意：这里不要 `use super::*`——它会把 gpui 自带的 `#[test]` 宏引进
    // 来，盖住 Rust 内置的 test 属性导致递归展开失败。只显式引入用到的项。
    use super::QUIT_ID;
    use super::SHOW_ID;
    use super::TrayAction;
    use super::menu_action;

    #[test]
    fn menu_dispatch_routes_show_quit_and_unknown() {
        assert_eq!(menu_action(SHOW_ID), TrayAction::Show);
        assert_eq!(menu_action(QUIT_ID), TrayAction::Quit);
        assert_eq!(menu_action("tray-nope"), TrayAction::Ignore);
        assert_ne!(SHOW_ID, QUIT_ID, "菜单 id 重了就全乱套");
    }
}
