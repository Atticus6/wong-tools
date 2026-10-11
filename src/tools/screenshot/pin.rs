//! 钉屏：把选区开成置顶浮窗，对着看、能拖、能缩放、能复制。
//!
//! shotori 同款交互，但只搬能跨平台的那部分：它的 layer-shell（Wayland
//! 专有）在 macOS/Windows 上换成普通置顶窗，几何和手势逻辑照旧——
//! 滚轮以光标为锚点缩放、拖动后至少留一截可抓、缩到原始尺寸为复位。
//!
//! 本文件只放**纯几何**（钳制/缩放/复位），能单测；窗口组装在同目录，
//! 见 `PinWindow`。`clamp_to` 特意不用 `f32::clamp`：lo > hi（退化屏幕
//! 尺寸）时 `clamp` 会 panic，钉屏宁可钳到 lo 也不能崩。

use std::sync::Arc;

use gpui_kit::assets::IconName;
use gpui_kit::component::button::Button;
use gpui_kit::component::{ActiveTheme, Icon};
use gpui_kit::*;
use image::{Frame, ImageBuffer};
use smallvec::SmallVec;

use crate::i18n::t;

/// 钉屏图：RGBA 内存像素 → 可直接 `img()` 的 RenderImage。
///
/// GPUI 要 BGRA（它自己的 PNG 解码路会换 R/B，我们跳过解码就得自己换），
/// 和覆盖层冻结帧同一份约定，集中在这一个函数里。
fn rgba_to_render_image(img: &image::RgbaImage) -> Arc<RenderImage> {
    let mut bgra = img.clone().into_raw();
    for px in bgra.chunks_exact_mut(4) {
        px.swap(0, 2);
    }
    let buf =
        ImageBuffer::from_raw(img.width(), img.height(), bgra).expect("像素长度和尺寸对不上");
    Arc::new(RenderImage::new(SmallVec::from_elem(Frame::new(buf), 1)))
}

/// 一格滚轮的缩放倍率。
pub const ZOOM_STEP: f32 = 1.12;
/// 缩放下限：再小就是一张邮票，看不清等于没有。
pub const ZOOM_MIN: f32 = 0.05;
/// 缩放上限：够深挖像素，又不至于把一个像素放大到铺满全屏。
pub const ZOOM_MAX: f32 = 24.0;
/// 拖动后至少留在屏幕内的边长（逻辑像素），否则钉屏可能被拖到完全
/// 屏幕外再也抓不回来。
pub const MIN_VISIBLE: f32 = 24.0;

/// 滚轮行数（像素差已归一）→ 下一档缩放，钳在区间内。
pub fn next_zoom(zoom: f32, lines: f32) -> f32 {
    (zoom * ZOOM_STEP.powf(lines)).clamp(ZOOM_MIN, ZOOM_MAX)
}

/// 某档缩放下的显示尺寸。
pub fn zoomed_size(base: Size<Pixels>, zoom: f32) -> Size<Pixels> {
    size(
        px((f32::from(base.width) * zoom).max(1.0)),
        px((f32::from(base.height) * zoom).max(1.0)),
    )
}

/// 永不 panic 的钳制：lo > hi（退化尺寸）时返回 lo。
/// 姿态和 `select::clamp_to` 一致——`f32::clamp` 遇反向边界直接 panic。
pub fn clamp_to(v: f32, lo: f32, hi: f32) -> f32 {
    if lo > hi { lo } else { v.clamp(lo, hi) }
}

/// 把原点钳进 `desk`，至少留 `MIN_VISIBLE` 可见。
/// 允许大部分移出屏幕（贴边停靠是正常用法），但不能整个拖没。
pub fn clamp_to_desk(
    origin: Point<Pixels>,
    pin: Size<Pixels>,
    desk: Bounds<Pixels>,
) -> Point<Pixels> {
    let (w, h) = (f32::from(pin.width), f32::from(pin.height));
    let (vw, vh) = (
        w.min(MIN_VISIBLE).min(f32::from(desk.size.width)),
        h.min(MIN_VISIBLE).min(f32::from(desk.size.height)),
    );
    let (dl, dt) = (f32::from(desk.left()), f32::from(desk.top()));
    let (dr, db) = (f32::from(desk.right()), f32::from(desk.bottom()));
    point(
        px(clamp_to(f32::from(origin.x), dl - (w - vw), dr - vw)),
        px(clamp_to(f32::from(origin.y), dt - (h - vh), db - vh)),
    )
}

/// 钉屏开窗的初始位置：和选区原位重叠（所见即所钉），只做越界钳制。
pub fn initial_rect(selection: Bounds<Pixels>, desk: Bounds<Pixels>) -> Bounds<Pixels> {
    let base = selection.size;
    Bounds::new(
        clamp_to_desk(selection.origin, base, desk),
        base,
    )
}

/// 钉屏窗口：一块置顶、可拖、可缩放的截图浮窗。
///
/// 手势参考 shotori，但**只用 gpui 真正暴露的能力**（和 shotori 的
/// Wayland 版本有本质差别，值得记一笔）：
///
/// - shotori 的钉屏是 layer-shell 面上自绘的一堆 pin，位置/缩放全是自己算的
///   矩形。gpui 在 macOS/Windows 上没有 layer-shell，也没有窗口**设位置**
///   的 API（只有 `resize` 和原生的 `start_window_move`），自绘矩形挪不动
///   窗口。所以这里老老实实开一个真浮窗：拖动交给系统（`start_window_move`
///   原生拖窗，带正确的阴影/边缘行为），缩放用 `resize` 改窗口尺寸。
/// - 代价：缩放锚点是窗口中心附近而不是光标（`resize` 不改位置），
///   也只能约束在一块屏内。胜在三个平台行为一致，不用自己维护坐标。
pub struct PinWindow {
    /// 钉住的像素（复制用）。
    image: image::RgbaImage,
    /// 渲染用的显存图：`new` 里建好，`render` 只复用（全分辨率 RGBA
    /// 每帧重拷一次又贵又没必要）。
    render: Arc<RenderImage>,
    /// 100% 时的尺寸（逻辑像素）。
    base: Size<Pixels>,
    /// 当前缩放倍率（滚轮累乘，1.0 = 原始尺寸）。
    zoom: f32,
    /// 刚复制成功的时间：拿它显示一瞬间的反馈，钉屏本体不动。
    flash: Option<std::time::Instant>,
    focus: FocusHandle,
}

impl PinWindow {
    /// 打开一个钉屏窗。`selection` 是选区（全局逻辑坐标），`cropped` 是
    /// 对应的裁剪像素——两者必须同源，否则钉出来的和框选的对不上。
    pub fn open(
        cropped: image::RgbaImage,
        selection: Bounds<Pixels>,
        desk: Bounds<Pixels>,
        cx: &mut App,
    ) -> Result<(), String> {
        // 大图铺满浮窗会盖住半个屏幕，先按屏幕上限缩到能看全。
        let base = fit_to_desk(selection.size, desk.size);
        let rect = initial_rect(Bounds::new(selection.origin, base), desk);
        let image = rgba_to_render_image(&cropped);
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(rect)),
            titlebar: Some(TitlebarOptions {
                // “钉屏”
                title: Some(t("app.shot_pin_title").into()),
                // 自绘工具条，原生标题栏只会碍事。
                appears_transparent: true,
                // macOS 红绿灯挪出窗外：钉屏要的是一块无边框的图，框架没给
                // 隐藏开关，位置法是目前唯一的办法（同 spotlight 面板）。
                traffic_light_position: if cfg!(target_os = "macos") {
                    Some(point(px(-100.), px(-100.)))
                } else {
                    None
                },
            }),
            // 置顶浮窗：`PopUp` 级别（盖在所有窗口上；`Floating` 只保盖父窗）。
            // 盖在主窗口和其他应用上面。
            kind: WindowKind::PopUp,
            focus: true,
            show: true,
            // 拖动走原生 `start_window_move`，系统自己挪窗口。
            is_movable: true,
            is_resizable: false,
            ..WindowOptions::default()
        };
        open_window(options, cx, |window, cx| {
            cx.new(|cx| Self::new(cropped, image, base, window, cx))
        })
        .map(|_| ())
        .map_err(|e| format!("打不开钉屏窗口: {e}"))
    }

    fn new(
        image_pixels: image::RgbaImage,
        image: Arc<RenderImage>,
        base: Size<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        Self {
            image: image_pixels,
            render: image,
            base,
            zoom: 1.0,
            flash: None,
            focus,
        }
    }

    /// 复制钉住的图到剪贴板。
    fn copy(&mut self) {
        // 复制失败不关窗、不弹错：钉屏还在，用户还能重来。
        if super::copy_image(&self.image).is_ok() {
            self.flash = Some(std::time::Instant::now());
        }
    }

    /// 当前显示尺寸 = 原始尺寸 × 缩放。
    fn shown_size(&self) -> Size<Pixels> {
        zoomed_size(self.base, self.zoom)
    }

    /// 滚轮缩放：改窗口尺寸即可（`resize` 不动位置，锚点自然在窗口角）。
    fn zoom_by(&mut self, lines: f32, window: &mut Window) {
        let next = next_zoom(self.zoom, lines);
        if (next - self.zoom).abs() < f32::EPSILON {
            return; // 已到上下限，别白 resize 一次
        }
        self.zoom = next;
        window.resize(self.shown_size());
    }

    /// 缩放回 100%：恢复原始窗口尺寸。
    fn reset(&mut self, window: &mut Window) {
        self.zoom = 1.0;
        window.resize(self.base);
    }

    /// 关窗。
    fn close(&mut self, window: &mut Window) {
        window.remove_window();
    }
}

/// 把 `base` 等比缩到不超过 `desk`，超出的才缩；小图保持原尺寸
/// （截图本来就小，放大钉屏没意义）。
pub fn fit_to_desk(base: Size<Pixels>, desk: Size<Pixels>) -> Size<Pixels> {
    let (w, h) = (f32::from(base.width), f32::from(base.height));
    let (dw, dh) = (f32::from(desk.width), f32::from(desk.height));
    if w <= dw && h <= dh {
        return base;
    }
    let scale = (dw / w).min(dh / h);
    size(px((w * scale).max(1.0)), px((h * scale).max(1.0)))
}

impl Render for PinWindow {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let bar_bg = cx.theme().popover;
        let bar_border = cx.theme().border;
        // 显存图只建一次（new 里），render 里复用：全分辨率 RGBA 每帧拷一次
        // 又贵又没必要，缩放走 GPU。
        let image = self.render.clone();

        div()
            .id("pin-window")
            .size_full()
            .relative()
            // 2px 主题边框当画框：置顶浮窗本身无边框，不框住跟底下内容糊一片。
            // 边框走内缩 padding 实现，不会被窗口 edge 裁掉一半；
            // 图片圆角小一圈 + 裁角，四角不顶出来。
            .bg(bar_border)
            .p(px(2.))
            .rounded(px(10.))
            // 焦点跟踪：新窗的标准配置，以后加键盘操作（Esc 关钉屏）直接能用。
            .track_focus(&self.focus)
            // 图像铺满窗：缩放 = 窗口尺寸本身，GPU 重采样。
            .child(
                div()
                    .size_full()
                    .rounded(px(8.))
                    .overflow_hidden()
                    .child(img(image).size_full().object_fit(ObjectFit::Fill)),
            )
            // 左键拖窗：原生 `start_window_move`，系统接管整个手势
            // （阴影/贴边/多屏行为都是对的），比手算原点省事且不会丢事件。
            // 工具条自己吃掉按下（见下），点按钮不会顺手拖窗。
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|_, _, window, _| window.start_window_move()),
            )
            // 滚轮缩放：只改窗口尺寸，`resize` 不动位置。
            .on_scroll_wheel(cx.listener(
                |this, event: &ScrollWheelEvent, window, cx| {
                    let lines = match event.delta {
                        // shotori 同款换算：触控板像素差按 40px 一档折行数。
                        ScrollDelta::Lines(lines) => lines.y,
                        ScrollDelta::Pixels(pixels) => f32::from(pixels.y) / 40.0,
                    };
                    this.zoom_by(lines, window);
                    cx.notify();
                },
            ))
            // 底部工具条：复制 / 复位 / 关闭，常驻。
            .child(
                div()
                    .absolute()
                    .bottom_0()
                    .left_0()
                    .right_0()
                    .flex()
                    .justify_center()
                    .py(px(6.))
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(2.))
                            .px(px(8.))
                            .py(px(4.))
                            .rounded(px(8.))
                            .bg(bar_bg)
                            .border_1()
                            .border_color(bar_border)
                            .child(pin_button(
                                "pin-copy",
                                IconName::Copy,
                                // “复制”
                                t("app.shot_copy"),
                                cx.listener(|this, _, _, cx| {
                                    this.copy();
                                    cx.notify();
                                }),
                            ))
                            .child(pin_button(
                                "pin-reset",
                                IconName::Focus,
                                // “回到 100%”
                                t("app.shot_pin_reset"),
                                cx.listener(|this, _, window, cx| {
                                    this.reset(window);
                                    cx.notify();
                                }),
                            ))
                            .child(div().w(px(1.)).h(px(18.)).bg(bar_border))
                            .child(pin_button(
                                "pin-close",
                                IconName::X,
                                // “关闭”
                                t("app.shot_pin_close"),
                                cx.listener(|this, _, window, _| this.close(window)),
                            )),
                    ),
            )
    }
}

/// 钉屏工具条按钮（覆盖层同款：30px 方块 + 18px 图标 + tooltip）。
fn pin_button(
    id: &'static str,
    icon: IconName,
    tooltip: String,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    Button::new(id)
        .tooltip(tooltip)
        .cursor_pointer()
        .size(px(30.))
        .rounded(px(5.))
        .child(Icon::new(icon).size(px(18.)))
        .on_click(on_click)
}


#[cfg(test)]
mod tests {
    // 注意：这里不要 `use super::*`——它会把 gpui 自带的 `#[test]` 宏引进
    // 来，盖住 Rust 内置的 test 属性导致递归展开失败。只显式引入用到的项。
    use super::clamp_to;
    use super::clamp_to_desk;
    use super::fit_to_desk;
    use super::initial_rect;
    use super::next_zoom;
    use super::zoomed_size;
    use super::MIN_VISIBLE;
    use super::ZOOM_MAX;
    use super::ZOOM_MIN;
    // 几何函数的签名要 `Bounds`/`Pixels`，测试里得自己把 gpui 的构造器
    // 显式引进来（`use super::*` 会把 gpui 的 `#[test]` 宏引进来的问题，
    // 见 AGENTS）。
    use gpui_kit::{Bounds, Pixels, point, px, size};

    /// 1920x1080 桌面。
    fn desk() -> Bounds<Pixels> {
        Bounds::new(point(px(0.), px(0.)), size(px(1920.), px(1080.)))
    }

    #[test]
    fn next_zoom_clamps_both_ends() {
        assert!((next_zoom(1.0, 1.0) - 1.12).abs() < 1e-5);
        assert!((next_zoom(1.0, -1.0) - 1.0 / 1.12).abs() < 1e-5);
        // 连续放大撞上限，缩小撞下限，都不越界。
        assert_eq!(next_zoom(1000.0, 50.0), ZOOM_MAX);
        assert_eq!(next_zoom(0.0001, -50.0), ZOOM_MIN);
    }

    #[test]
    fn zoomed_size_never_collapses_to_zero() {
        let s = zoomed_size(size(px(100.), px(50.)), ZOOM_MIN);
        assert!(f32::from(s.width) >= 1.0 && f32::from(s.height) >= 1.0);
    }

    #[test]
    fn clamp_to_survives_inverted_bounds() {
        // f32::clamp 在 lo > hi 时 panic；钉屏必须钳到 lo 而不是崩。
        assert_eq!(clamp_to(50.0, 100.0, 10.0), 100.0);
        assert_eq!(clamp_to(5.0, 0.0, 10.0), 5.0);
    }

    #[test]
    fn desk_clamp_keeps_a_grabbable_slice_on_screen() {
        // 把钉屏往右下角外拖：至少留 MIN_VISIBLE 在屏内。
        let pin = size(px(400.), px(300.));
        let o = clamp_to_desk(point(px(5000.), px(5000.)), pin, desk());
        assert!(o.x <= px(1920. - MIN_VISIBLE));
        assert!(o.y <= px(1080. - MIN_VISIBLE));
        // 往左上外拖同理。
        let o = clamp_to_desk(point(px(-5000.), px(-5000.)), pin, desk());
        assert!(o.x + pin.width >= px(MIN_VISIBLE));
        assert!(o.y + pin.height >= px(MIN_VISIBLE));
        // 屏内原样返回。
        let inside = point(px(300.), px(200.));
        assert_eq!(clamp_to_desk(inside, pin, desk()), inside);
    }

    #[test]
    fn fit_to_desk_only_shrinks_oversized_pins() {
        let desk = size(px(1920.), px(1080.));
        // 小图原样钉，不放大。
        let small = size(px(400.), px(300.));
        assert_eq!(fit_to_desk(small, desk), small);
        // 超宽的等比压进屏幕，保持长宽比。
        let wide = size(px(3840.), px(1080.));
        let fitted = fit_to_desk(wide, desk);
        assert!(fitted.width <= px(1920.) && fitted.height <= px(1080.));
        let ratio = f32::from(fitted.width) / f32::from(fitted.height);
        assert!((ratio - 3840.0 / 1080.0).abs() < 0.01, "长宽比坏了: {ratio}");
    }

    #[test]
    fn initial_rect_overlaps_selection() {
        // 默认和选区原位重叠（所见即所钉）。
        let sel = Bounds::new(point(px(400.), px(700.)), size(px(600.), px(300.)));
        let r = initial_rect(sel, desk());
        assert_eq!(r.size, sel.size);
        assert_eq!(r.origin, sel.origin);
    }

    #[test]
    fn initial_rect_clamps_instead_of_flipping() {
        // 比屏幕还宽的钉屏：原位出界，钳到可抓范围内，尺寸不动。
        let sel = Bounds::new(point(px(1900.), px(1070.)), size(px(600.), px(260.)));
        let r = initial_rect(sel, desk());
        assert_eq!(r.size, sel.size);
        // 右 edge：1920-24=1896；下 edge：1080-24=1056。
        assert_eq!(r.origin, point(px(1896.), px(1056.)));
    }
}
