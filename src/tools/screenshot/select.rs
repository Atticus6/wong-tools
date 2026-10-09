//! 选区矩形数学：归一化、钳制、逻辑→物理换算、裁剪、变暗矩形。
//! 全是纯函数，不碰显示器不碰窗口，可测。

/// 逻辑像素矩形：左上原点 + 非负宽高。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LogicRect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

/// 两点归一化：任意拖拽方向都收成左上 + 宽高。
pub fn normalize(ax: f32, ay: f32, bx: f32, by: f32) -> LogicRect {
    LogicRect {
        x: ax.min(bx),
        y: ay.min(by),
        w: (ax - bx).abs(),
        h: (ay - by).abs(),
    }
}

/// 钳制到窗口界内（`max_w`/`max_h` 是窗口逻辑像素）。
pub fn clamp(rect: LogicRect, max_w: f32, max_h: f32) -> LogicRect {
    let x = rect.x.clamp(0., max_w);
    let y = rect.y.clamp(0., max_h);
    LogicRect {
        x,
        y,
        w: rect.w.min(max_w - x).max(0.),
        h: rect.h.min(max_h - y).max(0.),
    }
}

/// 逻辑像素 → 物理像素（× 缩放、四舍五入），并钳到图像界内。
/// 宽或高不足 1 物理像素时返回 `None`（误触，不成选区）。
pub fn to_pixels(
    rect: LogicRect,
    scale: f32,
    img_w: u32,
    img_h: u32,
) -> Option<(u32, u32, u32, u32)> {
    let x = (rect.x * scale).round() as u32;
    let y = (rect.y * scale).round() as u32;
    // f32→u32 是饱和转换，负数变 0、超大变 u32::MAX，不会 wrap。
    let w = (rect.w * scale).round() as u32;
    let h = (rect.h * scale).round() as u32;
    let w = w.min(img_w.saturating_sub(x));
    let h = h.min(img_h.saturating_sub(y));
    if w == 0 || h == 0 {
        None
    } else {
        Some((x, y, w, h))
    }
}

/// 按物理矩形裁剪出新图。调用方必须传 `to_pixels` 的输出（界内保证）；
/// 这里再钳一次当保险，绝不 panic。
pub fn crop(img: &image::RgbaImage, x: u32, y: u32, w: u32, h: u32) -> image::RgbaImage {
    let w = w.min(img.width().saturating_sub(x));
    let h = h.min(img.height().saturating_sub(y));
    image::imageops::crop_imm(img, x, y, w, h).to_image()
}

/// 选区之外的变暗矩形（上/下/左/右，x,y,w,h 逻辑像素）。
/// 没选区时返回整窗一块，调用方直接全暗。
pub fn dim_rects(sel: Option<LogicRect>, w: f32, h: f32) -> Vec<(f32, f32, f32, f32)> {
    match sel {
        None => vec![(0., 0., w, h)],
        Some(r) => vec![
            (0., 0., w, r.y),
            (0., r.y + r.h, w, h - r.y - r.h),
            (0., r.y, r.x, r.h),
            (r.x + r.w, r.y, w - r.x - r.w, r.h),
        ],
    }
}

/// 工具条尺寸（逻辑像素）：3 个 30px 图标按钮 + 分隔符 + 间距 + 内边距。
/// 渲染（overlay 工具条）和命中判定用同一组数，对不上就穿帮。
pub const TOOLBAR_W: f32 = 128.0;
pub const TOOLBAR_H: f32 = 36.0;

/// 工具条矩形 (x, y, w, h)：跟在选区下方，空间不够放上方，左右钳住；
/// 没选区时回落到底部居中。shotori 同款：工具条只在松手定稿后出现，
/// 长得离屏幕边缘越远越不会被任务栏吃掉。
pub fn toolbar_rect(sel: Option<LogicRect>, w: f32, h: f32) -> (f32, f32, f32, f32) {
    match sel {
        Some(r) => {
            let x = if w > TOOLBAR_W + 16. {
                (r.x + r.w / 2. - TOOLBAR_W / 2.).clamp(8., w - TOOLBAR_W - 8.)
            } else {
                ((w - TOOLBAR_W) / 2.).max(0.)
            };
            // 下方能放下就放下面，否则放上面；上下都放不下就贴顶。
            let y = if r.y + r.h + 8. + TOOLBAR_H <= h - 8. {
                r.y + r.h + 8.
            } else {
                (r.y - 8. - TOOLBAR_H).max(8.)
            };
            (x, y, TOOLBAR_W, TOOLBAR_H)
        }
        None => (
            ((w - TOOLBAR_W) / 2.).max(0.),
            (h - TOOLBAR_H - 16.).max(0.),
            TOOLBAR_W,
            TOOLBAR_H,
        ),
    }
}

#[cfg(test)]
mod tests {
    // 注意：这里不要 `use super::*`——它会把 gpui 自带的 `#[test]` 宏引进
    // 来，盖住 Rust 内置的 test 属性导致递归展开失败。只显式引入用到的项。
    use super::clamp;
    use super::crop;
    use super::dim_rects;
    use super::normalize;
    use super::to_pixels;
    use super::toolbar_rect;
    use super::TOOLBAR_H;
    use super::TOOLBAR_W;

    #[test]
    fn normalize_folds_any_drag_direction() {
        // 四个方向拖出来是同一个矩形。
        let expected = super::LogicRect {
            x: 10.,
            y: 20.,
            w: 100.,
            h: 50.,
        };
        assert_eq!(normalize(10., 20., 110., 70.), expected);
        assert_eq!(normalize(110., 70., 10., 20.), expected);
        assert_eq!(normalize(110., 20., 10., 70.), expected);
        assert_eq!(normalize(10., 70., 110., 20.), expected);
    }

    #[test]
    fn clamp_keeps_rect_inside_window() {
        // 负起点收进界，超出的宽高砍掉。
        let r = clamp(normalize(-10., -5., 200., 100.), 150., 80.);
        assert_eq!((r.x, r.y), (0., 0.));
        assert_eq!((r.w, r.h), (150., 80.));
        // 框完全在外面时宽高归零（不成选区）。
        let r = clamp(normalize(200., 200., 300., 300.), 150., 80.);
        assert_eq!((r.w, r.h), (0., 0.));
    }

    #[test]
    fn to_pixels_scales_and_rejects_dots() {
        // 2 倍缩放：逻辑 10,20,100,50 → 物理 20,40,200,100。
        assert_eq!(
            to_pixels(super::LogicRect { x: 10., y: 20., w: 100., h: 50. }, 2., 1000, 1000),
            Some((20, 40, 200, 100))
        );
        // 不足 1 物理像素的误触不成选区。
        assert_eq!(
            to_pixels(super::LogicRect { x: 0., y: 0., w: 0.4, h: 0.4 }, 1., 1000, 1000),
            None
        );
        // 超出图像界的钳住不断言。
        assert_eq!(
            to_pixels(super::LogicRect { x: 990., y: 0., w: 100., h: 10. }, 1., 1000, 1000),
            Some((990, 0, 10, 10))
        );
    }

    #[test]
    fn crop_returns_sized_copy() {
        let img = image::RgbaImage::from_pixel(100, 100, image::Rgba([1, 2, 3, 255]));
        let cut = crop(&img, 10, 10, 20, 30);
        assert_eq!((cut.width(), cut.height()), (20, 30));
        assert_eq!(cut.get_pixel(0, 0), img.get_pixel(10, 10));
    }

    #[test]
    fn dim_rects_cover_everything_but_selection() {
        // 没选区：整窗一块。
        assert_eq!(dim_rects(None, 1000., 800.), vec![(0., 0., 1000., 800.)]);
        // 有选区：四块拼起来正好是全窗减选区（面积守恒）。
        let sel = super::LogicRect { x: 10., y: 20., w: 100., h: 50. };
        let rects = dim_rects(Some(sel), 1000., 800.);
        assert_eq!(rects.len(), 4);
        let area: f32 = rects.iter().map(|(_, _, w, h)| w * h).sum();
        assert!(
            (area - (1000. * 800. - 100. * 50.)).abs() < 0.01,
            "变暗面积不对: {area}"
        );
    }

    #[test]
    fn toolbar_follows_selection_or_parks_at_bottom() {
        // 没选区：底部居中。
        assert_eq!(
            toolbar_rect(None, 1000., 800.),
            ((1000. - TOOLBAR_W) / 2., 800. - TOOLBAR_H - 16., TOOLBAR_W, TOOLBAR_H)
        );
        // 选区在中间：跟下方，水平居中。
        assert_eq!(
            toolbar_rect(
                Some(super::LogicRect { x: 100., y: 100., w: 200., h: 100. }),
                1000.,
                800.
            ),
            (136., 208., TOOLBAR_W, TOOLBAR_H)
        );
        // 选区贴底：放不下就放上方。
        assert_eq!(
            toolbar_rect(
                Some(super::LogicRect { x: 100., y: 700., w: 200., h: 60. }),
                1000.,
                800.
            ),
            (136., 656., TOOLBAR_W, TOOLBAR_H)
        );
        // 选区贴右：水平钳住不飞出屏。
        assert_eq!(
            toolbar_rect(
                Some(super::LogicRect { x: 900., y: 100., w: 80., h: 60. }),
                1000.,
                800.
            ),
            (864., 168., TOOLBAR_W, TOOLBAR_H)
        );
    }
}
