//! Wong Tools bootstrap: fonts, theme, window.
//!
//! Release builds are GUI apps: no console window when launched from Explorer
//! (debug builds keep the console for `cargo run` diagnostics).

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod shell;
mod theme;

use std::borrow::Cow;

use gpui_kit::*;

/// Bundled Geist + Geist Mono (SIL OFL, see assets/fonts/); registered
/// before the first frame so the theme's font faces resolve.
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

/// Spur-style window: hidden OS caption with a custom-drawn title bar. On
/// Windows the platform keeps the resize frame and answers the control-area
/// hit tests (drag, snap layouts, double-click zoom) natively.
fn window_options() -> WindowOptions {
    WindowOptions {
        titlebar: Some(TitlebarOptions {
            title: Some("Wong Tools".into()),
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
    // The full icon catalog: the default `Assets` bundle only embeds a small
    // subset, so use AllAssets for the sidebar and title bar icons.
    let app = application().with_assets(assets::AllAssets);
    app.run(|cx| {
        init(cx);
        load_fonts(cx);
        theme::init(cx);

        open_window(window_options(), cx, |window, cx| {
            cx.new(|cx| shell::WsShell::new(window, cx))
        })
        .expect("Failed to open window");
    });
}
