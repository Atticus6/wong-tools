//! 区域选择覆盖层：冻结帧打底 + 鼠标拖选 + 确定/取消。
//!
//! shotori 同款做法：`canvas` 在 paint 里挂 `window.on_mouse_event`
//! （窗口级监听，拖出界不丢事件）；工具条按钮走元素级 handler。
//! 覆盖层只管框选：确定后裁剪写回主窗口的截图状态，预览/复制/保存不动。

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use gpui_kit::assets::IconName;
use gpui_kit::component::button::Button;
use gpui_kit::component::{ActiveTheme, Icon};
use gpui_kit::*;
use image::{Frame, ImageBuffer};
use smallvec::SmallVec;

use crate::i18n::t;
use super::select::LogicRect;
use crate::shell::WsShell;

const TEXT_SM: f32 = 12.0;
/// 小于这个的拖拽当误触，不成选区。
const MIN_SELECT: f32 = 3.0;
/// 变暗层透明度。shotori 默认 0.55；深色桌面上太糊看不清，用 0.4，
/// 选框内外不断层就行，不靠它藏东西。
const DIM_ALPHA: f32 = 0.4;

/// 写回主窗口的预览图 ID 计数器：和主窗口自截的计数器错开命名空间，
// GPUI 按 id 缓存解码图，每次必须不同。
static NEXT_CROPPED_ID: AtomicU64 = AtomicU64::new(1_000_000);

/// RGBA 内存像素 → 可直接 `img()` 的 RenderImage。
/// GPUI 要 BGRA（它自己的 PNG 解码路会换 R/B，我们跳过解码就得自己换），
/// 集中在这一个函数里，别处不要再换一次。
fn rgba_to_render_image(img: &image::RgbaImage) -> Arc<RenderImage> {
    let mut bgra = img.clone().into_raw();
    // 长度永远是 4 的倍数，余数分支走不到，纯粹让编译器闭嘴。
    for px in bgra.chunks_exact_mut(4) {
        px.swap(0, 2);
    }
    let buf =
        ImageBuffer::from_raw(img.width(), img.height(), bgra).expect("像素长度和尺寸对不上");
    Arc::new(RenderImage::new(SmallVec::from_elem(Frame::new(buf), 1)))
}

pub struct SelectOverlay {
    /// 全分辨率原图（裁剪用）。
    image: image::RgbaImage,
    /// 冻结帧：解码好的显存图，渲染零拷贝。
    frozen: Arc<RenderImage>,
    /// 打开时的窗口缩放：逻辑→物理就靠它（ freeze 时的 xcap 缩放只管窗口定位）。
    scale: f32,
    /// 窗口逻辑尺寸：选区钳制和工具条判定用（开窗时定死，全屏不动）。
    size: Size<Pixels>,
    anchor: Option<Point<Pixels>>,
    current: Point<Pixels>,
    dragging: bool,
    shell: WeakEntity<WsShell>,
}

impl SelectOverlay {
    /// 截屏并打开覆盖层。失败返回错误，主窗口直接显示，不开窗。
    pub fn open(shell: WeakEntity<WsShell>, cx: &mut App) -> Result<(), String> {
        let t0 = std::time::Instant::now();
        // 先冻结再开窗：用户看到的第一帧就是截到的那张，不闪。
        // 冻结帧走 RenderImage 直显，不经过 PNG 编解码（debug 下全分辨率
        // PNG 编码要几秒，这就是之前覆盖层“过很久才出现”的原因）。
        let geo = super::primary_geometry()?;
        let image = super::capture_primary()?;
        let frozen = rgba_to_render_image(&image);
        eprintln!(
            "overlay: 冻结 {}ms（{}x{}）",
            t0.elapsed().as_millis(),
            image.width(),
            image.height()
        );
        let (lx, ly, lw, lh) = geo.logical();
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds {
                origin: point(px(lx), px(ly)),
                size: size(px(lw), px(lh)),
            })),
            titlebar: Some(TitlebarOptions {
                // “框选截图”
                title: Some(t("app.shot_overlay_title").into()),
                appears_transparent: true,
                traffic_light_position: None,
            }),
            // 置顶弹出窗：盖住任务栏和主窗口，做全屏框选。
            kind: WindowKind::PopUp,
            focus: true,
            show: true,
            is_movable: false,
            is_resizable: false,
            ..WindowOptions::default()
        };
        open_window(options, cx, |window, cx| {
            cx.new(|cx| Self::new(shell, image, frozen, lw, lh, window, cx))
        })
        .map(|_| ())
        .map_err(|e| format!("打不开覆盖层: {e}"))?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn new(
        shell: WeakEntity<WsShell>,
        image: image::RgbaImage,
        frozen: Arc<RenderImage>,
        lw: f32,
        lh: f32,
        window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Self {
        Self {
            image,
            frozen,
            scale: window.scale_factor(),
            size: size(px(lw), px(lh)),
            anchor: None,
            current: point(px(0.), px(0.)),
            dragging: false,
            shell,
        }
    }

    /// 结束拖拽（松手）：定稿当前选区，太小的当误触清掉。
    fn end_drag(&mut self) {
        self.dragging = false;
        if self.selection().is_none() {
            self.anchor = None;
        }
    }

    /// 当前选区（逻辑像素，已钳制到窗口内）；没按下过或太小返回 None。
    fn selection(&self) -> Option<LogicRect> {
        let a = self.anchor?;
        let r = super::select::normalize(
            a.x.as_f32(),
            a.y.as_f32(),
            self.current.x.as_f32(),
            self.current.y.as_f32(),
        );
        let r = super::select::clamp(r, self.size.width.as_f32(), self.size.height.as_f32());
        if r.w < MIN_SELECT || r.h < MIN_SELECT {
            None
        } else {
            Some(r)
        }
    }

    /// 工具条矩形：跟选区走（下面→上面→底部居中），见 select。
    fn toolbar_rect(&self) -> (f32, f32, f32, f32) {
        super::select::toolbar_rect(
            self.selection(),
            self.size.width.as_f32(),
            self.size.height.as_f32(),
        )
    }

    /// 按下是否落在工具条上：是的话归按钮，不管框选。
    /// 注意拖拽中选区是活的，这里的判定和渲染用同一个矩形，不会打架。
    /// 没选区时工具条根本不显示，直接返回 false，别吞了起拖的按下。
    fn in_toolbar(&self, p: Point<Pixels>) -> bool {
        if self.selection().is_none() {
            return false;
        }
        let (x, y, w, h) = self.toolbar_rect();
        let (px, py) = (p.x.as_f32(), p.y.as_f32());
        px >= x && px <= x + w && py >= y && py <= y + h
    }

    /// 当前选区的裁剪图（逻辑→物理换算 + 界内钳制，见 select）。
    fn cropped(&self) -> Option<image::RgbaImage> {
        let rect = self.selection()?;
        let (x, y, w, h) =
            super::select::to_pixels(rect, self.scale, self.image.width(), self.image.height())?;
        Some(super::select::crop(&self.image, x, y, w, h))
    }

    /// 复制选区：裁剪→剪贴板→写回主窗口→关窗。
    /// 剪贴板失败就留在覆盖层（选区还在，可重试或取消）。
    fn finish_copy(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(cropped) = self.cropped() else {
            return;
        };
        if super::copy_image(&cropped).is_err() {
            return;
        }
        self.finish_with(cropped, window, cx);
    }

    /// 保存选区：裁剪→图片库→写回主窗口→关窗，失败同上。
    fn finish_save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(cropped) = self.cropped() else {
            return;
        };
        let path =
            super::pictures_dir().join(super::timestamped_name());
        if super::save_png(&cropped, &path).is_err() {
            return;
        }
        self.finish_with(cropped, window, cx);
    }

    /// 收尾：缩略图写回主窗口，关覆盖层。主窗口没了就只关窗。
    fn finish_with(
        &mut self,
        cropped: image::RgbaImage,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Ok(preview) = super::thumbnail_png(&cropped) {
            let id = NEXT_CROPPED_ID.fetch_add(1, Ordering::Relaxed);
            let _ = self.shell.update(cx, |shell, cx| {
                shell.set_shot(cropped, preview, id);
                cx.notify();
            });
        }
        window.remove_window();
    }

    /// 取消：不写回，直接关窗。
    fn cancel(&mut self, window: &mut Window) {
        window.remove_window();
    }
}

impl Render for SelectOverlay {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let input = cx.entity().downgrade();
        let accent = cx.theme().link;
        let sel = self.selection();
        // 工具条跟选区走；没选区不用摆出来，拖拽中也不闪，
        // 只在松手定稿后显示。
        let (bx, by, bw, bh) = self.toolbar_rect();
        let show_toolbar = !self.dragging && sel.is_some();

        div()
            .id("select-overlay")
            .size_full()
            .relative()
            .bg(black())
            .cursor_crosshair()
            // 冻结帧：不透明铺满（shotori 原样：flow + size_full）。
            .child(img(self.frozen.clone()).size_full())
            // 变暗 + 描边：canvas 直接画，不经过布局（shotori 原样）。
            .child(dim_canvas(sel, accent))
            // 事件网：窗口级鼠标监听（shotori 原样）。
            .child(pointer_sink(input))
            // 选区描边由画布画了，这里只剩尺寸标签。
            .children(sel.map(|r| {
                let label = format!("{} × {}", r.w as u32, r.h as u32);
                // 标签放选区上方，顶边不够地方就放下面。
                let (lx, ly) = if r.y >= 24. {
                    (r.x, r.y - 22.)
                } else {
                    (r.x, r.y + r.h + 4.)
                };
                div()
                    .absolute()
                    .left(px(lx))
                    .top(px(ly))
                    .bg(black().opacity(0.7))
                    .rounded(px(4.))
                    .px(px(6.))
                    .text_size(px(TEXT_SM))
                    .text_color(white())
                    .child(label)
                    .into_any_element()
            }))
            // 没选区时的居中提示。
            .children(sel.is_none().then(|| {
                div()
                    .flex_1()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(px(15.))
                    .text_color(white().opacity(0.8))
                    // “拖拽框选区域”
                    .child(t("app.shot_hint"))
                    .into_any_element()
            }))
            // 工具条：跟选区走，定稿后才显示。shotori 原样：图标按钮 +
            // tooltip + 分隔符（复制/保存收尾并关窗，取消只关窗）。
            .children(show_toolbar.then(|| {
                let bar_bg = cx.theme().popover;
                let bar_border = cx.theme().border;
                div()
                    .absolute()
                    .left(px(bx))
                    .top(px(by))
                    .w(px(bw))
                    .h(px(bh))
                    .flex()
                    .items_center()
                    .justify_center()
                    .gap(px(2.))
                    .px(px(8.))
                    .rounded(px(8.))
                    .bg(bar_bg)
                    .border_1()
                    .border_color(bar_border)
                    // 工具条吃掉左键按下，不让它漏到框选去（shotori 原样；
                    // 几何判定还在，真漏了也开不出选区）。
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(toolbar_icon(
                        "overlay-copy",
                        IconName::Copy,
                        // “复制”
                        t("app.shot_copy"),
                        cx.listener(|this, _, window, cx| this.finish_copy(window, cx)),
                    ))
                    .child(toolbar_icon(
                        "overlay-save",
                        IconName::Save,
                        // “保存”
                        t("app.shot_save"),
                        cx.listener(|this, _, window, cx| this.finish_save(window, cx)),
                    ))
                    .child(div().w(px(1.)).h(px(18.)).mx(px(4.)).bg(bar_border))
                    .child(toolbar_icon(
                        "overlay-cancel",
                        IconName::X,
                        // “取消”
                        t("app.shot_cancel"),
                        cx.listener(|this, _, window, _| this.cancel(window)),
                    ))
            }))
    }
}

/// 变暗 + 描边画布：直接 paint_quad，不经过布局（shotori 原样）。
/// 矩形用 select::dim_rects 算，画布只管画。
fn dim_canvas(sel: Option<LogicRect>, accent: Hsla) -> impl IntoElement {
    canvas(
        |_, _, _| (),
        move |viewport, (), window, _| {
            let ws = viewport.size;
            for (x, y, rw, rh) in
                super::select::dim_rects(sel, ws.width.as_f32(), ws.height.as_f32())
            {
                if rw > 0. && rh > 0. {
                    window.paint_quad(fill(
                        Bounds::from_corners(
                            point(px(x), px(y)),
                            point(px(x + rw), px(y + rh)),
                        ),
                        black().opacity(DIM_ALPHA),
                    ));
                }
            }
            if let Some(r) = sel {
                window.paint_quad(outline(
                    Bounds::from_corners(
                        point(px(r.x), px(r.y)),
                        point(px(r.x + r.w), px(r.y + r.h)),
                    ),
                    accent,
                    BorderStyle::default(),
                ));
            }
        },
    )
    .absolute()
    .top_0()
    .left_0()
    .size_full()
}

/// 工具条图标按钮（shotori 原样）：30px 方块 + 18px 图标 + tooltip。
/// 注意别调 `.hover()`：Button 内部已经设了 hover，再设会 panic。
/// `on_click` 传 `cx.listener(...)` 的结果，它正好就是 Button 要的形状。
fn toolbar_icon(
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

/// 看不见的事件网：paint 里挂窗口级鼠标监听，一帧挂一次。
/// shotori 同款做法——元素级 handler 在拖出界时会丢事件，窗口级不会。
fn pointer_sink(input: WeakEntity<SelectOverlay>) -> impl IntoElement {
    canvas(
        |_, _, _| (),
        move |_, (), window, _| {
            let view = input.clone();
            window.on_mouse_event(move |event: &MouseDownEvent, phase, _, cx| {
                if phase != DispatchPhase::Bubble || event.button != MouseButton::Left {
                    return;
                }
                let _ = view.update(cx, |this, cx| {
                    // 工具条的按下归按钮，不管框选。
                    if this.in_toolbar(event.position) {
                        return;
                    }
                    this.anchor = Some(event.position);
                    this.current = event.position;
                    this.dragging = true;
                    cx.notify();
                });
            });
            let view = input.clone();
            window.on_mouse_event(move |event: &MouseMoveEvent, phase, _, cx| {
                if phase != DispatchPhase::Bubble {
                    return;
                }
                let _ = view.update(cx, |this, cx| {
                    // 定稿后的悬停不许碰 current：选区是 anchor + current
                    // 算出来的，碰了框就跟着鼠标跑。
                    if !this.dragging {
                        return;
                    }
                    // 松手事件丢了（比如松在窗口外面）：move 里左键已经
                    // 不在了，在这里收尾，不然一直卡在拖拽态，工具条也不出来。
                    if !event.dragging() {
                        this.end_drag();
                    } else {
                        this.current = event.position;
                    }
                    cx.notify();
                });
            });
            let view = input.clone();
            window.on_mouse_event(move |event: &MouseUpEvent, phase, _, cx| {
                if phase != DispatchPhase::Bubble || event.button != MouseButton::Left {
                    return;
                }
                let _ = view.update(cx, |this, cx| {
                    if this.dragging {
                        this.current = event.position;
                        this.end_drag();
                        cx.notify();
                    }
                });
            });
        },
    )
    .absolute()
    .size_full()
}

#[cfg(test)]
mod tests {
    // 注意：这里不要 `use super::*`——它会把 gpui 自带的 `#[test]` 宏引进
    // 来，盖住 Rust 内置的 test 属性导致递归展开失败。只显式引入用到的项。
    use super::rgba_to_render_image;

    #[test]
    fn frozen_frame_keeps_pixels_and_swaps_to_bgra() {
        // 2x1：红 + 蓝。GPUI 要 BGRA，R/B 必须互换，A 不动。
        let img = image::RgbaImage::from_fn(2, 1, |x, _| {
            if x == 0 {
                image::Rgba([255, 0, 0, 255])
            } else {
                image::Rgba([0, 0, 255, 255])
            }
        });
        let a = rgba_to_render_image(&img);
        let b = rgba_to_render_image(&img);
        // 同一张图每次 id 不同，GPUI 缓存不会串台。
        assert_ne!(a.id, b.id, "RenderImage id 必须唯一");
        let bytes = a.as_bytes(0).expect("有像素");
        assert_eq!(bytes.len(), 8, "像素字节数不对");
        assert_eq!(&bytes[0..4], &[0, 0, 255, 255], "红要换成蓝");
        assert_eq!(&bytes[4..8], &[255, 0, 0, 255], "蓝要换成红");
    }
}
