//! Wong Tools 启动：字体、主题、窗口。
//!
//! release 构建是纯 GUI 程序：从资源管理器启动时不带黑窗口
//!（debug 构建保留控制台，方便看 `cargo run` 的诊断输出）。

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

// 应用文案：`locales/ui.yml` 的 `app.*`，默认中文。组件内置文案走
// 组件自己的 locale，`extend!` 负责把两边接起来（见 run 里的注册）。
rust_i18n::i18n!("locales", fallback = "zh-CN");

mod i18n;
mod settings;
mod shell;
mod spotlight;
mod telemetry;
mod theme;
mod tools;
mod updater;

use std::borrow::Cow;

use gpui_kit::*;

/// 内置 Geist + Geist Mono（SIL OFL 协议，见 assets/fonts/）；首帧前注册，
/// 主题里的字体才能解析出来。
fn load_fonts(cx: &mut App) {
    cx.text_system()
        .add_fonts(vec![
            Cow::Borrowed(include_bytes!("../assets/fonts/Geist.ttf").as_slice()),
            Cow::Borrowed(include_bytes!("../assets/fonts/Geist-Medium.ttf").as_slice()),
            Cow::Borrowed(include_bytes!("../assets/fonts/Geist-SemiBold.ttf").as_slice()),
            Cow::Borrowed(include_bytes!("../assets/fonts/GeistMono.ttf").as_slice()),
            Cow::Borrowed(include_bytes!("../assets/fonts/GeistMono-Medium.ttf").as_slice()),
        ])
        .expect("failed to load bundled Geist fonts");
}

/// 默认窗口大小（逻辑像素）：第一版写死的 1280x800，没存档时用它。
const DEFAULT_WIN_W: f32 = 1280.0;
const DEFAULT_WIN_H: f32 = 800.0;
/// 最小窗口（逻辑像素）：和 `window_min_size` 同一对值，恢复时小于它按它夹。
const MIN_WIN_W: f32 = 880.0;
const MIN_WIN_H: f32 = 600.0;
/// 显示器一块都拿不到时的回落原点（原来写死的 80,60，无头测试才走这里）。
const FALLBACK_ORIGIN_X: f32 = 80.0;
const FALLBACK_ORIGIN_Y: f32 = 60.0;

/// 启动落位结果：矩形 + 是否最大化，纯数据不碰 IO。
struct RestoredPlacement {
    bounds: Bounds<Pixels>,
    maximized: bool,
}

/// 启动落位纯逻辑：存档中心还在某块屏可见区内就用存档（尺寸夹到最小值
/// 和主屏之间），显示器拔掉/分辨率变了导致完全跑出去就按默认大小在主屏
/// 居中。不碰 IO，单测直接喂矩形。
fn place_window(
    saved: Option<crate::settings::SavedWindowBounds>,
    displays: &[Bounds<Pixels>],
) -> RestoredPlacement {
    // 主屏：`cx.displays()` 顺序主屏在前，拿第一块。
    let primary = displays.first();
    // 按给定大小在主屏可见区居中：屏比窗小先缩窗，保证整个窗看得见。
    let center_on_primary = |w: f32, h: f32| -> Bounds<Pixels> {
        match primary {
            Some(screen) => {
                let sw = screen.size.width.as_f32();
                let sh = screen.size.height.as_f32();
                let (w, h) = (w.min(sw), h.min(sh));
                Bounds {
                    origin: point(
                        px(screen.origin.x.as_f32() + (sw - w) / 2.),
                        px(screen.origin.y.as_f32() + (sh - h) / 2.),
                    ),
                    size: size(px(w), px(h)),
                }
            }
            None => Bounds {
                origin: point(px(FALLBACK_ORIGIN_X), px(FALLBACK_ORIGIN_Y)),
                size: size(px(w), px(h)),
            },
        }
    };
    let Some(saved) = saved else {
        return RestoredPlacement {
            bounds: center_on_primary(DEFAULT_WIN_W, DEFAULT_WIN_H),
            maximized: false,
        };
    };
    // 存档中心落在某块屏可见区内才算还在，不在就居中（拔显示器场景）。
    let center_x = saved.x + saved.w / 2.;
    let center_y = saved.y + saved.h / 2.;
    let on_screen = displays.iter().any(|d| {
        let (ox, oy, dw, dh) = (
            d.origin.x.as_f32(),
            d.origin.y.as_f32(),
            d.size.width.as_f32(),
            d.size.height.as_f32(),
        );
        center_x >= ox && center_x < ox + dw && center_y >= oy && center_y < oy + dh
    });
    if !on_screen {
        return RestoredPlacement {
            bounds: center_on_primary(DEFAULT_WIN_W, DEFAULT_WIN_H),
            maximized: false,
        };
    }
    // 还在：原点不动（允许跨屏 straddle，只要标题栏可抓），尺寸夹到
    // [最小值, 主屏]，4K 屏搬到小屏也不会撑出去。
    let (cap_w, cap_h) = primary
        .map(|s| (s.size.width.as_f32(), s.size.height.as_f32()))
        .unwrap_or((DEFAULT_WIN_W, DEFAULT_WIN_H));
    RestoredPlacement {
        bounds: Bounds {
            origin: point(px(saved.x), px(saved.y)),
            size: size(
                px(saved.w.max(MIN_WIN_W).min(cap_w)),
                px(saved.h.max(MIN_WIN_H).min(cap_h)),
            ),
        },
        maximized: saved.maximized,
    }
}

/// Spur 风格窗口：藏掉系统标题栏，自己画。Windows 上平台保留缩放边框，
/// 拖拽区命中测试（拖拽、Snap Layouts、双击缩放）仍由系统原生接管。
/// 位置大小走上次存档（越界回正中），见 `place_window`。
fn window_options(cx: &App) -> WindowOptions {
    let displays: Vec<Bounds<Pixels>> = cx
        .displays()
        .iter()
        .map(|d| d.visible_bounds())
        .collect();
    let placed = place_window(crate::settings::window_bounds(), &displays);
    window_options_with_bounds(if placed.maximized {
        WindowBounds::Maximized(placed.bounds)
    } else {
        WindowBounds::Windowed(placed.bounds)
    })
}

/// 窗口参数组装（纯构造，好单测）：标题栏、最小尺寸都在这，
/// 落位矩形调用方算好传进来。
fn window_options_with_bounds(window_bounds: WindowBounds) -> WindowOptions {
    WindowOptions {
        titlebar: Some(TitlebarOptions {
            // “Wong Tools”
            title: Some(crate::i18n::t("app.window_title").into()),
            appears_transparent: true,
            traffic_light_position: None,
        }),
        window_bounds: Some(window_bounds),
        window_min_size: Some(size(px(MIN_WIN_W), px(MIN_WIN_H))),
        ..WindowOptions::default()
    }
}

/// `WindowBounds` 转存档：只记还原矩形 + 最大化旗，不记全屏。
/// Windows 上 `window.bounds()` 在最大化时给的是全屏尺寸，直接存会把
/// 还原尺寸撑大；`window.window_bounds()` 走 `GetWindowPlacement` 的
/// `rcNormalPosition`，拿的是还原矩形，最大化退出也不存错。
fn saved_from_window_bounds(window_bounds: WindowBounds) -> crate::settings::SavedWindowBounds {
    let (bounds, maximized) = match window_bounds {
        WindowBounds::Windowed(bounds) => (bounds, false),
        WindowBounds::Maximized(bounds) => (bounds, true),
        // 全屏没有对应的存档形态：留还原矩形，下次按普通窗口打开。
        WindowBounds::Fullscreen(bounds) => (bounds, false),
    };
    crate::settings::SavedWindowBounds {
        x: bounds.origin.x.as_f32(),
        y: bounds.origin.y.as_f32(),
        w: bounds.size.width.as_f32(),
        h: bounds.size.height.as_f32(),
        maximized,
    }
}

/// 读窗写库一条龙：退出两条路径（关闭按钮 / 应用退出）共用。
/// 写库失败只记一笔日志，绝不崩。
fn persist_window_bounds(window: &Window) {
    let saved = saved_from_window_bounds(window.window_bounds());
    if let Err(e) = crate::settings::set_window_bounds(&saved) {
        eprintln!("存窗口位置失败: {e}");
    }
}

/// 关掉我们开的系统代理并落旗：启动残留清理和退出钩子共用。
/// 只动自己开过的（旗举着才动），绝不碰用户自己的代理设置。
/// 失败吞掉：关机时注册表可能已经不可写，尽力而为。
fn cleanup_owned_proxy() {
    if crate::settings::proxy_running() {
        let _ = crate::tools::proxy::disable_system_proxy();
        let _ = crate::settings::set_proxy_running(false);
    }
}

fn main() {
    // 崩溃上报先行：panic hook 是进程级的，放第一行，后面哪崩都抓得到。
    // 初始化失败只记日志，应用照常启动。
    crate::telemetry::init();
    // 完整图标集：默认 `Assets` 只内嵌了一小部分，侧边栏和标题栏的图标
    // 需要 AllAssets 才找得到。
    let app = application().with_assets(assets::AllAssets);
    app.run(|cx| {
        // 把应用文案并入组件：只调一次，必须在 `init` 之前。
        use gpui_kit::component as gpui_component;
        rust_i18n::extend!(gpui_component);
        init(cx);
        load_fonts(cx);
        theme::init(cx);
        crate::i18n::init();
        // 上次非正常退出可能留下系统代理指向已死的隧道：先清掉再开窗，
        // 不然用户一启动就断网，还以为是新 bug。
        cleanup_owned_proxy();

        // 快捷键：组件默认在 `init` 里注册，应用的后加。
        // 官方文档做法（https://gpui-kit.com/docs/keybinding/ ）：
        // `None` 上下文 + 后加 = 同深度后来居上，面板没打开时没有
        // handler，不消费其他输入，原样落到组件默认行为。
        {
            use crate::spotlight::{
                PaletteConfirm, PaletteDismiss, PaletteDown, PaletteUp, ToggleSpotlight,
            };
            let mut bindings = vec![
                KeyBinding::new("up", PaletteUp, None),
                KeyBinding::new("down", PaletteDown, None),
                KeyBinding::new("enter", PaletteConfirm, None),
                KeyBinding::new("escape", PaletteDismiss, None),
            ];
            // 呼出键走用户存档：没存过回落默认，非法就跳过绑定
            // （设置页会展示原因）。`KeyBinding::new` 非法会 panic，
            // 先验（`keymap_binding_string`）后建。
            let spec = crate::settings::hotkey()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| crate::spotlight::DEFAULT_HOTKEY.to_string());
            match crate::spotlight::keymap_binding_string(&spec) {
                Ok(keymap) => {
                    bindings.push(KeyBinding::new(&keymap, ToggleSpotlight, None));
                }
                Err(e) => eprintln!("呼出快捷键无效，跳过应用内绑定: {e}"),
            }
            cx.bind_keys(bindings);
        }

        // 关窗策略：所有窗口都没了才退出（官方多窗口做法，
        // https://gpui-kit.com/docs/multi-window/ ）。
        // 面板窗和主窗谁先关都不影响另一个，全关了才退。
        cx.on_window_closed(|cx, _| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();

        // 正常退出也要清系统代理 + 存窗口位置：Drop 不一定跑（被杀更不会），
        // 钩子兜底。detach 让钩子活到进程结束，不然闭包结束就注销了。
        // 存两处：`on_window_should_close` 管右上角关闭（窗还在，能读到矩形；
        // `on_window_closed` 回调时窗已经没了，读不到）；`on_app_quit` 管
        // 窗还活着的退出（菜单/快捷键/`quit()`）。两处都写，关几次存几次，
        // 幂等不怕重复。
        open_window(window_options(cx), cx, |window, cx| {
            cx.new(|cx| shell::WsShell::new(window, cx))
        })
        .map(|(main_window, _)| {
            // 关闭按钮路径：窗还没销毁，先把位置落盘，放行关闭。
            let _ = main_window.update(cx, |_, window, cx| {
                window.on_window_should_close(&*cx, |window, _| {
                    persist_window_bounds(window);
                    true
                });
            });
            // future 要求 'static 不能借 `cx`：读窗和写库都在闭包同步段做完，
            // 只回一个空 future。
            cx.on_app_quit(move |cx: &mut App| {
                cleanup_owned_proxy();
                // 窗口已经没了就跳过（`update` 返回 Err）：关闭按钮路径
                // 已经存过，不写脏数据。
                let _ = main_window.update(cx, |_, window, _| {
                    persist_window_bounds(window);
                });
                async {}
            })
            .detach();
        })
        .expect("Failed to open window");
    });
}

#[cfg(test)]
mod tests {
    // 注意：这里不要 `use super::*`——它会把 gpui 自带的 `#[test]` 宏引进
    // 来，盖住 Rust 内置的 test 属性导致递归展开失败。只显式引入用到的项。
    use super::place_window;
    use super::saved_from_window_bounds;
    use super::window_options_with_bounds;
    use super::DEFAULT_WIN_H;
    use super::DEFAULT_WIN_W;
    use super::MIN_WIN_H;
    use super::MIN_WIN_W;
    use gpui_kit::px;
    use gpui_kit::size;
    use gpui_kit::point;
    use gpui_kit::Bounds;
    use gpui_kit::WindowBounds;

    /// 单屏（主屏 2560x1440，原点 0,0），各单测共用。
    fn one_screen() -> Vec<Bounds<gpui_kit::Pixels>> {
        vec![Bounds {
            origin: point(px(0.), px(0.)),
            size: size(px(2560.), px(1440.)),
        }]
    }

    /// 存档矩形，调用方按需改字段。
    fn saved_at(x: f32, y: f32) -> crate::settings::SavedWindowBounds {
        crate::settings::SavedWindowBounds {
            x,
            y,
            w: DEFAULT_WIN_W,
            h: DEFAULT_WIN_H,
            maximized: false,
        }
    }

    #[test]
    fn window_uses_transparent_titlebar_and_sane_bounds() {
        // 外壳自己画了 38px 标题栏，系统标题栏必须藏掉；
        // 这里回退了就会看到两个标题栏。
        let opts = window_options_with_bounds(WindowBounds::Windowed(Bounds {
            origin: point(px(80.), px(60.)),
            size: size(px(DEFAULT_WIN_W), px(DEFAULT_WIN_H)),
        }));
        let titlebar = opts.titlebar.expect("需要自绘标题栏参数");
        assert!(
            titlebar.appears_transparent,
            "系统标题栏必须隐藏，自绘标题栏才生效"
        );
        assert_eq!(
            titlebar.title.as_deref(),
            Some("Wong Tools"),
            "窗口标题被改动了"
        );
        let min = opts.window_min_size.expect("需要最小窗口尺寸");
        assert!(
            min.width >= px(800.) && min.height >= px(500.),
            "最小尺寸太小，装不下标题栏 + 侧边栏：{min:?}"
        );
    }

    #[test]
    fn saved_placement_is_kept_when_still_on_screen() {
        // 存档中心还在屏内：原点原样回来，最大化旗带着。
        let mut saved = saved_at(100., 100.);
        saved.maximized = true;
        let placed = place_window(Some(saved), &one_screen());
        assert!(placed.maximized, "最大化旗不能丢");
        assert_eq!(placed.bounds.origin, point(px(100.), px(100.)));
        assert_eq!(
            placed.bounds.size,
            size(px(DEFAULT_WIN_W), px(DEFAULT_WIN_H))
        );
    }

    #[test]
    fn missing_or_offscreen_placement_centers_default_on_primary() {
        // 没存档：默认大小在主屏居中。
        let placed = place_window(None, &one_screen());
        assert!(!placed.maximized);
        assert_eq!(
            placed.bounds.size,
            size(px(DEFAULT_WIN_W), px(DEFAULT_WIN_H))
        );
        assert_eq!(
            placed.bounds.origin,
            point(px((2560. - DEFAULT_WIN_W) / 2.), px((1440. - DEFAULT_WIN_H) / 2.))
        );
        // 存档飞出去了（显示器拔掉）：同样回正中，不记住最大化。
        let mut off = saved_at(5000., 5000.);
        off.maximized = true;
        let placed = place_window(Some(off), &one_screen());
        assert!(!placed.maximized, "越界了就别最大化");
        assert_eq!(
            placed.bounds.origin,
            point(px((2560. - DEFAULT_WIN_W) / 2.), px((1440. - DEFAULT_WIN_H) / 2.))
        );
        // 一块屏都没有（无头）：回落固定原点，别崩。
        let placed = place_window(None, &[]);
        assert_eq!(placed.bounds.origin, point(px(80.), px(60.)));
    }

    #[test]
    fn tiny_or_oversized_placement_is_clamped() {
        // 存得太小：夹到最小窗口。
        let tiny = crate::settings::SavedWindowBounds {
            x: 100.,
            y: 100.,
            w: 100.,
            h: 100.,
            maximized: false,
        };
        let placed = place_window(Some(tiny), &one_screen());
        assert_eq!(placed.bounds.size, size(px(MIN_WIN_W), px(MIN_WIN_H)));
        // 屏比默认窗还小：缩窗居中，保证整个窗看得见。
        let small = vec![Bounds {
            origin: point(px(0.), px(0.)),
            size: size(px(800.), px(500.)),
        }];
        let placed = place_window(None, &small);
        assert_eq!(placed.bounds.size, size(px(800.), px(500.)));
        assert_eq!(placed.bounds.origin, point(px(0.), px(0.)));
    }

    #[test]
    fn window_bounds_snapshot_keeps_restore_rect() {
        // 存档只记还原矩形：最大化退出也不能把全屏尺寸当还原尺寸存了，
        // 不然下次还原会撑满屏。`window.window_bounds()` 已经是还原矩形，
        // 这里只验形态转换不丢字段。
        let rect = Bounds {
            origin: point(px(100.), px(120.)),
            size: size(px(1280.), px(800.)),
        };
        let windowed = saved_from_window_bounds(WindowBounds::Windowed(rect));
        assert!(!windowed.maximized);
        assert_eq!((windowed.x, windowed.y, windowed.w, windowed.h), (100., 120., 1280., 800.));
        let maximized = saved_from_window_bounds(WindowBounds::Maximized(rect));
        assert!(maximized.maximized, "最大化旗不能丢");
        assert_eq!((maximized.x, maximized.y, maximized.w, maximized.h), (100., 120., 1280., 800.));
        let fullscreen = saved_from_window_bounds(WindowBounds::Fullscreen(rect));
        assert!(!fullscreen.maximized, "全屏按普通窗口存，还原矩形留着");
        assert_eq!((fullscreen.x, fullscreen.y, fullscreen.w, fullscreen.h), (100., 120., 1280., 800.));
    }
}
