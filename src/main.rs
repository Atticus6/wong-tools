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
mod theme;

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

/// Spur 风格窗口：藏掉系统标题栏，自己画。Windows 上平台保留缩放边框，
/// 拖拽区命中测试（拖拽、Snap Layouts、双击缩放）仍由系统原生接管。
fn window_options() -> WindowOptions {
    WindowOptions {
        titlebar: Some(TitlebarOptions {
            // “Wong Tools”
            title: Some(crate::i18n::t("app.window_title").into()),
            appears_transparent: true,
            traffic_light_position: None,
        }),
        window_bounds: Some(WindowBounds::Windowed(Bounds {
            origin: point(px(80.), px(60.)),
            size: size(px(1280.), px(800.)),
        })),
        window_min_size: Some(size(px(880.), px(600.))),
        ..WindowOptions::default()
    }
}

fn main() {
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

        open_window(window_options(), cx, |window, cx| {
            cx.new(|cx| shell::WsShell::new(window, cx))
        })
        .expect("Failed to open window");
    });
}

#[cfg(test)]
mod tests {
    // 注意：这里不要 `use super::*`——它会把 gpui 自带的 `#[test]` 宏引进
    // 来，盖住 Rust 内置的 test 属性导致递归展开失败。只显式引入用到的项。
    use super::window_options;
    use gpui_kit::px;

    #[test]
    fn window_uses_transparent_titlebar_and_sane_bounds() {
        // 外壳自己画了 38px 标题栏，系统标题栏必须藏掉；
        // 这里回退了就会看到两个标题栏。
        let opts = window_options();
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
}
