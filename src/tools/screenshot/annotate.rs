//! 截图标注：形状模型 + 手势状态 + CPU 光栅化。
//!
//! shotori `annotation` 的简化移植：只收 wong-tools 工具条上有的 9 种工具
//! （矩形/椭圆/直线/箭头/画笔/荧光笔/马赛克/文字/橡皮擦）+ 撤销/重做。
//! 不要的（序号徽章/高斯模糊/折线/选择编辑）一律不搬，保持纯逻辑、
//! 不碰 GPUI，坐标全是窗口逻辑像素 `f32`，方便单测。

use super::select::LogicRect;
use std::cell::RefCell;

// 颜色是 0xRRGGBBAA（shotori 同款，`to_be_bytes` 即 [r,g,b,a]）。
// 配色对齐参考截图第二排：红橙黄绿蓝 + 白灰黑。
/// 调色盘：颜色 + 文案 key 后缀（`locales/ui.yml` 的 `shot_c_*`）。
pub const PALETTE: [(u32, &str); 8] = [
    (0xFF3B30FF, "red"),
    (0xFF9500FF, "orange"),
    (0xFFCC00FF, "yellow"),
    (0x34C759FF, "green"),
    (0x0A84FFFF, "blue"),
    (0xFFFFFFFF, "white"),
    (0x8E8E93FF, "gray"),
    (0x000000FF, "black"),
];

/// 荧光笔笔芯透明度：颜色只取 RGB，alpha 固定这一档（shotori 同款 0x60）。
pub const HIGHLIGHT_ALPHA: u8 = 0x60;

/// 工具类型。`Eraser` 是纯模式：按下即整形删除，从不落盘为形状。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShapeKind {
    Rectangle,
    Ellipse,
    Line,
    Arrow,
    Pencil,
    Highlighter,
    Mosaic,
    Text,
    Eraser,
}

/// 工具条按钮顺序（渲染和这里一致，别各写一份）。
pub const TOOL_ORDER: [ShapeKind; 9] = [
    ShapeKind::Rectangle,
    ShapeKind::Ellipse,
    ShapeKind::Line,
    ShapeKind::Arrow,
    ShapeKind::Pencil,
    ShapeKind::Highlighter,
    ShapeKind::Mosaic,
    ShapeKind::Text,
    ShapeKind::Eraser,
];

/// 每种工具的粗细范围（shotori `size_spec` 同款数值）。
pub fn size_spec(kind: ShapeKind) -> (f32, f32) {
    match kind {
        ShapeKind::Highlighter => (8., 60.),
        ShapeKind::Mosaic => (4., 48.),
        ShapeKind::Eraser => (8., 96.),
        ShapeKind::Text => (12., 72.),
        _ => (1., 20.),
    }
}

/// 一个标注形状。`bounds` 给区域类（矩形/椭圆/马赛克/文字），
/// `points` 给线类（直线/箭头 2 点，画笔/荧光笔点云）。
#[derive(Clone, Debug, PartialEq)]
pub struct Shape {
    pub kind: ShapeKind,
    pub text: Option<String>,
    pub bounds: LogicRect,
    pub color: u32,
    pub width: f32,
    pub points: Vec<(f32, f32)>,
}

/// 可逆操作。选中编辑没有，只有增删，索引全程稳定
/// （删除只在尾部 undo 恢复，Erase 按手势合并见 `erase_open`）。
#[derive(Clone)]
enum Op {
    Add(Shape),
    RemoveMany(Vec<(usize, Shape)>),
    RemoveAll(Vec<Shape>),
}

/// 每种工具记住上次粗细，互不串味（shotori `ToolSizes` 同款）。
struct Sizes {
    rectangle: f32,
    ellipse: f32,
    line: f32,
    arrow: f32,
    pencil: f32,
    highlighter: f32,
    mosaic: f32,
    text: f32,
    eraser: f32,
}

pub struct Annotations {
    tool: Option<ShapeKind>,
    color_ix: usize,
    sizes: Sizes,
    shapes: Vec<Shape>,
    history: Vec<Op>,
    redo: Vec<Op>,
    draft_start: (f32, f32),
    draft: Option<Shape>,
    pressed: bool,
    erasing: bool,
    erase_open: bool,
}

impl Default for Annotations {
    fn default() -> Self {
        Self {
            tool: None,
            color_ix: 0,
            sizes: Sizes {
                rectangle: 3.,
                ellipse: 3.,
                line: 3.,
                arrow: 3.,
                pencil: 3.,
                highlighter: 20.,
                mosaic: 16.,
                text: 24.,
                eraser: 32.,
            },
            shapes: Vec::new(),
            history: Vec::new(),
            redo: Vec::new(),
            draft_start: (0., 0.),
            draft: None,
            pressed: false,
            erasing: false,
            erase_open: false,
        }
    }
}

impl Annotations {
    pub fn tool(&self) -> Option<ShapeKind> {
        self.tool
    }

    /// 切换工具：点已激活的即收起。切换总是丢弃未提交的草稿。
    pub fn toggle(&mut self, kind: ShapeKind) {
        self.draft = None;
        self.pressed = false;
        self.erasing = false;
        self.erase_open = false;
        self.tool = if self.tool == Some(kind) {
            None
        } else {
            Some(kind)
        };
    }

    /// 有 paint 手势的工具才算启用（文字走编辑器，不算）。
    pub fn paint_enabled(&self) -> bool {
        matches!(
            self.tool,
            Some(
                ShapeKind::Rectangle
                    | ShapeKind::Ellipse
                    | ShapeKind::Line
                    | ShapeKind::Arrow
                    | ShapeKind::Pencil
                    | ShapeKind::Highlighter
                    | ShapeKind::Mosaic
                    | ShapeKind::Eraser
            )
        )
    }

    pub fn color_ix(&self) -> usize {
        self.color_ix
    }

    pub fn set_color(&mut self, ix: usize) {
        if ix < PALETTE.len() {
            self.color_ix = ix;
            // 荧光笔草稿颜色跟手换（已提交的不改，没有选中编辑）。
            let color = self.fresh_color();
            if let Some(d) = self.draft.as_mut() {
                if d.kind == ShapeKind::Highlighter {
                    d.color = color;
                }
            }
        }
    }

    /// 当前颜色（含荧光笔的固定透明度）。
    fn fresh_color(&self) -> u32 {
        let rgb = PALETTE[self.color_ix].0 & 0xFFFFFF00;
        if self.tool == Some(ShapeKind::Highlighter) {
            rgb | HIGHLIGHT_ALPHA as u32
        } else {
            rgb | 0xFF
        }
    }

    pub fn size_of(&self, kind: ShapeKind) -> f32 {
        match kind {
            ShapeKind::Rectangle => self.sizes.rectangle,
            ShapeKind::Ellipse => self.sizes.ellipse,
            ShapeKind::Line => self.sizes.line,
            ShapeKind::Arrow => self.sizes.arrow,
            ShapeKind::Pencil => self.sizes.pencil,
            ShapeKind::Highlighter => self.sizes.highlighter,
            ShapeKind::Mosaic => self.sizes.mosaic,
            ShapeKind::Text => self.sizes.text,
            ShapeKind::Eraser => self.sizes.eraser,
        }
    }

    fn set_size_of(&mut self, kind: ShapeKind, v: f32) {
        let (min, max) = size_spec(kind);
        let v = v.clamp(min, max);
        match kind {
            ShapeKind::Rectangle => self.sizes.rectangle = v,
            ShapeKind::Ellipse => self.sizes.ellipse = v,
            ShapeKind::Line => self.sizes.line = v,
            ShapeKind::Arrow => self.sizes.arrow = v,
            ShapeKind::Pencil => self.sizes.pencil = v,
            ShapeKind::Highlighter => self.sizes.highlighter = v,
            ShapeKind::Mosaic => self.sizes.mosaic = v,
            ShapeKind::Text => self.sizes.text = v,
            ShapeKind::Eraser => self.sizes.eraser = v,
        }
    }

    /// 当前工具的粗细（无工具回落 3）。
    pub fn tool_size(&self) -> f32 {
        self.tool.map_or(3., |t| self.size_of(t))
    }

    /// 改当前工具粗细，顺手同步未提交草稿的线宽。
    pub fn set_tool_size(&mut self, v: f32) {
        let Some(tool) = self.tool else { return };
        self.set_size_of(tool, v);
        let w = self.size_of(tool);
        if let Some(d) = self.draft.as_mut() {
            if d.kind == tool {
                d.width = w;
            }
        }
    }

    pub fn step_tool_size(&mut self, up: bool) {
        let Some(tool) = self.tool else { return };
        let (min, max) = size_spec(tool);
        let next = self.size_of(tool) + if up { 1. } else { -1. };
        if next >= min && next <= max {
            self.set_tool_size(next);
        }
    }

    /// 落笔：选区外的点直接无视。橡皮擦没有草稿，按下即删。
    pub fn begin(&mut self, p: (f32, f32), sel: LogicRect) {
        if !self.paint_enabled() || !contains(sel, p) {
            return;
        }
        self.pressed = true;
        self.erase_open = false;
        if self.tool == Some(ShapeKind::Eraser) {
            self.erasing = true;
            let r = self.size_of(ShapeKind::Eraser) / 2.;
            self.erase_at(p, r);
            return;
        }
        let kind = self.tool.expect("paint_enabled 已守卫");
        self.draft_start = p;
        let shape = Shape {
            kind,
            text: None,
            bounds: LogicRect { x: p.0, y: p.1, w: 0., h: 0. },
            color: self.fresh_color(),
            width: self.size_of(kind),
            points: match kind {
                ShapeKind::Line | ShapeKind::Arrow => vec![p, p],
                ShapeKind::Pencil | ShapeKind::Highlighter => vec![p],
                _ => Vec::new(),
            },
        };
        self.draft = Some(shape);
    }

    /// 拖到：返回画面是否变了（没变就别重烤预览层）。
    pub fn drag_to(&mut self, p: (f32, f32), sel: LogicRect) -> bool {
        if self.erasing {
            let r = self.size_of(ShapeKind::Eraser) / 2.;
            return self.erase_at(clamp_point(p, sel), r);
        }
        let Some(d) = self.draft.as_mut() else {
            return false;
        };
        match d.kind {
            ShapeKind::Pencil | ShapeKind::Highlighter => {
                let end = clamp_point(p, sel);
                if d.points.last() == Some(&end) {
                    false
                } else {
                    d.points.push(end);
                    true
                }
            }
            ShapeKind::Line | ShapeKind::Arrow => {
                let end = clamp_point(p, sel);
                let last = d.points.len() - 1;
                if d.points[last] == end {
                    false
                } else {
                    d.points[last] = end;
                    true
                }
            }
            _ => {
                // 起点落笔时已在选区内（begin 守卫），终点钳住，归一化即在界内。
                let end = clamp_point(p, sel);
                let bounds = super::select::normalize(
                    self.draft_start.0,
                    self.draft_start.1,
                    end.0,
                    end.1,
                );
                if d.bounds == bounds {
                    false
                } else {
                    d.bounds = bounds;
                    true
                }
            }
        }
    }

    /// 松手：有效草稿落盘，无效（误触小点）丢弃。
    pub fn end(&mut self) {
        let was_erasing = self.erasing;
        self.pressed = false;
        self.erasing = false;
        self.erase_open = false;
        if was_erasing {
            return;
        }
        if let Some(d) = self.draft.take() {
            let valid = match d.kind {
                ShapeKind::Line | ShapeKind::Arrow => dist(d.points[0], d.points[1]) >= 2.,
                ShapeKind::Pencil | ShapeKind::Highlighter => true,
                _ => d.bounds.w >= 2. && d.bounds.h >= 2.,
            };
            if valid {
                self.record_add(d);
            }
        }
    }

    /// 丢弃草稿（Esc 第一级）。返回是否真丢了东西。
    pub fn cancel_draft(&mut self) -> bool {
        self.pressed = false;
        self.erasing = false;
        self.erase_open = false;
        self.draft.take().is_some()
    }

    /// 收起工具（Esc 第二级）。返回是否真收了东西。
    pub fn deactivate(&mut self) -> bool {
        self.cancel_draft();
        if self.tool.take().is_some() {
            return true;
        }
        self.cancel_text()
    }

    /// 整形删除：笔尖圆碰到墨即删整形，同一手势合并成一条历史。
    fn erase_at(&mut self, p: (f32, f32), radius: f32) -> bool {
        let mut removed = Vec::new();
        let mut ix = self.shapes.len();
        while ix > 0 {
            ix -= 1;
            if shape_hit(&self.shapes[ix], p, radius) {
                removed.push((ix, self.shapes.remove(ix)));
            }
        }
        if removed.is_empty() {
            return false;
        }
        match self.history.last_mut() {
            Some(Op::RemoveMany(prev)) if self.erase_open => prev.extend(removed),
            _ => {
                self.history.push(Op::RemoveMany(removed));
                self.redo.clear();
                self.erase_open = true;
            }
        }
        true
    }

    fn record_add(&mut self, shape: Shape) {
        self.shapes.push(shape.clone());
        self.history.push(Op::Add(shape));
        self.redo.clear();
    }

    pub fn can_undo(&self) -> bool {
        !self.history.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    pub fn undo(&mut self) -> bool {
        self.draft = None;
        self.pressed = false;
        self.erasing = false;
        self.erase_open = false;
        let Some(op) = self.history.pop() else {
            return false;
        };
        match &op {
            Op::Add(_) => {
                self.shapes.pop();
            }
            Op::RemoveMany(removals) => {
                for (ix, shape) in removals.iter().rev() {
                    self.shapes.insert(*ix, shape.clone());
                }
            }
            Op::RemoveAll(shapes) => {
                self.shapes = shapes.clone();
            }
        }
        self.redo.push(op);
        true
    }

    pub fn redo(&mut self) -> bool {
        if self.draft.is_some() {
            return false;
        }
        let Some(op) = self.redo.pop() else {
            return false;
        };
        match &op {
            Op::Add(shape) => self.shapes.push(shape.clone()),
            Op::RemoveMany(removals) => {
                for (ix, _) in removals {
                    self.shapes.remove(*ix);
                }
            }
            Op::RemoveAll(_) => self.shapes.clear(),
        }
        self.history.push(op);
        true
    }

    /// 清空全部（垃圾桶）：一条历史，一次撤销全回来。
    pub fn clear_all(&mut self) -> bool {
        if self.shapes.is_empty() {
            return false;
        }
        self.draft = None;
        self.pressed = false;
        self.erasing = false;
        self.erase_open = false;
        self.history.push(Op::RemoveAll(std::mem::take(
            &mut self.shapes,
        )));
        self.redo.clear();
        true
    }

    pub fn committed(&self) -> &[Shape] {
        &self.shapes
    }

    pub fn draft(&self) -> Option<&Shape> {
        self.draft.as_ref()
    }
}

impl Annotations {
    /// 草稿在画布上的包围盒（含线宽余量），预览层按它烤小图。
    /// `canvas` 是窗口逻辑尺寸，出来钳在窗内。
    pub fn draft_render_bounds(&self, canvas_w: f32, canvas_h: f32) -> Option<LogicRect> {
        let d = self.draft.as_ref()?;
        let b = shape_render_bounds(d);
        let x0 = b.x.max(0.);
        let y0 = b.y.max(0.);
        let x1 = (b.x + b.w).min(canvas_w);
        let y1 = (b.y + b.h).min(canvas_h);
        if x1 <= x0 || y1 <= y0 {
            return None;
        }
        Some(LogicRect { x: x0, y: y0, w: x1 - x0, h: y1 - y0 })
    }

    // ---- 文字（编辑器在 overlay 侧，状态机在这一侧）----

    /// 输入中：每敲一键调一次，草稿实时跟手。
    /// `wrap_w` 是换行宽（逻辑像素），`scale` 是窗口缩放。
    pub fn preview_text(&mut self, anchor: (f32, f32), wrap_w: f32, text: String, scale: f32) {
        let font_size = self.size_of(ShapeKind::Text);
        let (w, h) = measure_text(&text, font_size * scale, wrap_w * scale);
        // 空串也留草稿（占位盒），提交时再拦。
        let w = (w / scale).max(4.);
        let h = (h / scale).max(font_size * 1.35);
        self.draft = Some(Shape {
            kind: ShapeKind::Text,
            text: Some(text),
            bounds: LogicRect { x: anchor.0, y: anchor.1, w, h },
            color: PALETTE[self.color_ix].0,
            width: font_size,
            points: Vec::new(),
        });
    }

    pub fn editing_text(&self) -> bool {
        self.draft.as_ref().is_some_and(|d| d.kind == ShapeKind::Text)
    }

    /// 提交：空串算取消。返回是否落了盘。
    pub fn commit_text(&mut self) -> bool {
        let take = self.draft.take().filter(|d| {
            d.kind == ShapeKind::Text
                && d.text.as_deref().is_some_and(|s| !s.trim().is_empty())
        });
        self.pressed = false;
        if let Some(shape) = take {
            self.record_add(shape);
            true
        } else {
            false
        }
    }

    /// 取消输入（Esc）。返回是否真取消了东西。
    pub fn cancel_text(&mut self) -> bool {
        if self.editing_text() {
            self.draft = None;
            self.pressed = false;
            true
        } else {
            false
        }
    }
}

/// 形状在画布上的包围盒（含线宽/箭头头余量）。
/// 预览裁剪用：只重烤和草稿相交的已提交形状，拖拽帧成本和形状总数脱钩。
pub fn shape_render_bounds(shape: &Shape) -> LogicRect {
    let pad = match shape.kind {
        // 箭头头最大伸出：head=(4w).max(10)，再留 AA 余量。
        ShapeKind::Arrow => (shape.width * 4.).max(10.) + shape.width + 2.,
        ShapeKind::Line | ShapeKind::Pencil | ShapeKind::Highlighter => shape.width + 2.,
        // 矩形/椭圆描边内缩，马赛克/文字无外溢，2px 给抗锯齿。
        _ => 2.,
    };
    let (x0, y0, x1, y1) = match shape.kind {
        ShapeKind::Line | ShapeKind::Arrow | ShapeKind::Pencil | ShapeKind::Highlighter => {
            let mut r = (f32::INFINITY, f32::INFINITY, f32::NEG_INFINITY, f32::NEG_INFINITY);
            for &(x, y) in &shape.points {
                r.0 = r.0.min(x);
                r.1 = r.1.min(y);
                r.2 = r.2.max(x);
                r.3 = r.3.max(y);
            }
            r
        }
        _ => (
            shape.bounds.x,
            shape.bounds.y,
            shape.bounds.x + shape.bounds.w,
            shape.bounds.y + shape.bounds.h,
        ),
    };
    LogicRect { x: x0 - pad, y: y0 - pad, w: x1 - x0 + pad * 2., h: y1 - y0 + pad * 2. }
}

/// 两矩形是否相交。贴边接触不算：谁也 paint 不到对方的像素列。
pub fn bounds_overlap(a: LogicRect, b: LogicRect) -> bool {
    a.x < b.x + b.w && b.x < a.x + a.w && a.y < b.y + b.h && b.y < a.y + a.h
}

fn contains(r: LogicRect, p: (f32, f32)) -> bool {
    p.0 >= r.x && p.0 <= r.x + r.w && p.1 >= r.y && p.1 <= r.y + r.h
}

fn clamp_point(p: (f32, f32), sel: LogicRect) -> (f32, f32) {
    (
        p.0.clamp(sel.x, sel.x + sel.w),
        p.1.clamp(sel.y, sel.y + sel.h),
    )
}

fn dist(a: (f32, f32), b: (f32, f32)) -> f32 {
    (b.0 - a.0).hypot(b.1 - a.1)
}

/// 橡皮擦命中：区域类看框（外扩半径），线类看与折线的距离。
fn shape_hit(shape: &Shape, p: (f32, f32), radius: f32) -> bool {
    match shape.kind {
        ShapeKind::Rectangle | ShapeKind::Ellipse | ShapeKind::Mosaic | ShapeKind::Text => {
            let b = shape.bounds;
            p.0 >= b.x - radius
                && p.0 <= b.x + b.w + radius
                && p.1 >= b.y - radius
                && p.1 <= b.y + b.h + radius
        }
        _ => {
            let slack = radius + shape.width / 2.;
            if shape.points.len() < 2 {
                return shape.points.first().is_some_and(|&q| dist(p, q) <= slack);
            }
            shape
                .points
                .windows(2)
                .any(|w| seg_dist(p, w[0], w[1]) <= slack)
        }
    }
}

fn seg_dist(p: (f32, f32), a: (f32, f32), b: (f32, f32)) -> f32 {
    let dx = b.0 - a.0;
    let dy = b.1 - a.1;
    let len2 = dx * dx + dy * dy;
    if len2 < 1e-6 {
        return dist(p, a);
    }
    let t = (((p.0 - a.0) * dx + (p.1 - a.1) * dy) / len2).clamp(0., 1.);
    dist(p, (a.0 + dx * t, a.1 + dy * t))
}

// ---- 光栅化（shotori line/filter/text 的简化移植） ----

/// 把若干形状画进 RGBA（行主序、非预乘）。
/// `origin` 是这块像素左上在形状坐标系里的位置（逻辑像素），
/// `scale` 是物理/逻辑比。`rgba[o+3]==0` 的屏间缝隙一律跳过。
pub fn rasterize(
    shapes: impl Iterator<Item = Shape>,
    rgba: &mut [u8],
    w: u32,
    h: u32,
    origin: (f32, f32),
    scale: f32,
) {
    for shape in shapes {
        match shape.kind {
            ShapeKind::Text => rasterize_text(&shape, rgba, w, h, origin, scale),
            ShapeKind::Mosaic => rasterize_mosaic(&shape, rgba, w, h, origin, scale),
            ShapeKind::Rectangle => rasterize_rect(&shape, rgba, w, h, origin, scale),
            ShapeKind::Ellipse => rasterize_ellipse(&shape, rgba, w, h, origin, scale),
            _ => rasterize_line(&shape, rgba, w, h, origin, scale),
        }
    }
}

fn blend(rgba: &mut [u8], w: u32, x: i32, y: i32, color: [u8; 4], coverage: f32) {
    if coverage <= 0. || x < 0 || y < 0 {
        return;
    }
    let (x, y) = (x as u32, y as u32);
    if x >= w {
        return;
    }
    let h = (rgba.len() / 4 / w as usize) as u32;
    if y >= h {
        return;
    }
    let o = ((y * w + x) * 4) as usize;
    if rgba[o + 3] == 0 {
        return;
    }
    let alpha = (coverage * color[3] as f32 / 255.).min(1.);
    for c in 0..3 {
        rgba[o + c] = (rgba[o + c] as f32 * (1. - alpha) + color[c] as f32 * alpha).round() as u8;
    }
}

/// 矩形：四条内缩描边（预览和导出画同一份，所见即所得）。
fn rasterize_rect(
    shape: &Shape,
    rgba: &mut [u8],
    w: u32,
    h: u32,
    origin: (f32, f32),
    scale: f32,
) {
    let color = shape.color.to_be_bytes();
    let b = shape.bounds;
    let lw = shape.width.min(b.w / 2.).min(b.h / 2.);
    let strokes = [
        (b.x, b.y, b.w, lw),
        (b.x, b.y + b.h - lw, b.w, lw),
        (b.x, b.y + lw, lw, b.h - lw * 2.),
        (b.x + b.w - lw, b.y + lw, lw, b.h - lw * 2.),
    ];
    for (x, y, sw, sh) in strokes {
        let x0 = ((x - origin.0) * scale).round().clamp(0., w as f32) as u32;
        let y0 = ((y - origin.1) * scale).round().clamp(0., h as f32) as u32;
        let x1 = ((x + sw - origin.0) * scale).round().clamp(0., w as f32) as u32;
        let y1 = ((y + sh - origin.1) * scale).round().clamp(0., h as f32) as u32;
        for row in y0..y1 {
            for col in x0..x1 {
                blend(rgba, w, col as i32, row as i32, color, 1.);
            }
        }
    }
}

/// 椭圆环：2x2 超采样测环（外内反绕打孔思路的像素版）。
/// 小到 `rx<=线宽` 直接退化成实心椭圆。
fn rasterize_ellipse(
    shape: &Shape,
    rgba: &mut [u8],
    w: u32,
    h: u32,
    origin: (f32, f32),
    scale: f32,
) {
    let color = shape.color.to_be_bytes();
    let b = shape.bounds;
    let rx = b.w * scale / 2.;
    let ry = b.h * scale / 2.;
    if rx <= 0. || ry <= 0. {
        return;
    }
    let cx = (b.x - origin.0) * scale + rx;
    let cy = (b.y - origin.1) * scale + ry;
    let solid = rx <= shape.width * scale || ry <= shape.width * scale;
    let irx = rx - shape.width * scale;
    let iry = ry - shape.width * scale;
    let left = (cx - rx).floor().clamp(0., w as f32) as i32;
    let right = (cx + rx).ceil().clamp(0., w as f32) as i32;
    let top = (cy - ry).floor().clamp(0., h as f32) as i32;
    let bottom = (cy + ry).ceil().clamp(0., h as f32) as i32;
    let inside = |x: f32, y: f32| {
        let dx = (x - cx) / rx;
        let dy = (y - cy) / ry;
        dx * dx + dy * dy <= 1.
    };
    let hole = |x: f32, y: f32| {
        if solid {
            return false;
        }
        let dx = (x - cx) / irx;
        let dy = (y - cy) / iry;
        dx * dx + dy * dy <= 1.
    };
    for row in top..bottom {
        for col in left..right {
            let mut hit = 0;
            for sy in 0..2 {
                for sx in 0..2 {
                    let x = col as f32 + (sx as f32 + 0.5) / 2.;
                    let y = row as f32 + (sy as f32 + 0.5) / 2.;
                    if inside(x, y) && !hole(x, y) {
                        hit += 1;
                    }
                }
            }
            blend(rgba, w, col, row, color, hit as f32 / 4.);
        }
    }
}

// 线类：胶囊并集（shotori `polygons` 同款：每段 34 边形胶囊，单点 32 边形圆）。
type Poly = Vec<(f32, f32)>;

fn capsule(a: (f32, f32), b: (f32, f32), width: f32) -> Option<Poly> {
    let dx = b.0 - a.0;
    let dy = b.1 - a.1;
    if dx.hypot(dy) < 0.001 {
        return None;
    }
    let angle = dy.atan2(dx);
    let mut poly = Vec::with_capacity(34);
    for (c, start) in [(b, angle - std::f32::consts::FRAC_PI_2), (a, angle + std::f32::consts::FRAC_PI_2)] {
        for i in 0..=16 {
            let (s, co) = (start + i as f32 * std::f32::consts::PI / 16.).sin_cos();
            poly.push((c.0 + co * width / 2., c.1 + s * width / 2.));
        }
    }
    Some(poly)
}

fn dot(c: (f32, f32), width: f32) -> Poly {
    (0..32)
        .map(|i| {
            let (s, co) = (i as f32 * std::f32::consts::TAU / 32.).sin_cos();
            (c.0 + co * width / 2., c.1 + s * width / 2.)
        })
        .collect()
}

/// 箭头几何：杆胶囊 + 三角头，tip 钉死在释放点（shotori `geometry` 同款比例）。
fn arrow_geometry(points: &[(f32, f32)], width: f32) -> Vec<Poly> {
    let Some([tip, w1, w2]) = arrow_head(points, width) else {
        return Vec::new();
    };
    let start = match points {
        [s, _] => *s,
        _ => return Vec::new(),
    };
    // 杆长只到翼中点（= tip - unit*head，和下面同源）。
    let base = ((w1.0 + w2.0) / 2., (w1.1 + w2.1) / 2.);
    let len = dist(start, tip);
    let shaft = width.min(len * 0.25);
    let mut out = capsule(start, base, shaft).map(|p| vec![p]).unwrap_or_default();
    out.push(vec![tip, w1, w2]);
    out
}

/// 箭头三角头顶点 [tip, 左翼, 右翼]，overlay 矢量预览也吃这一份。
/// 比例和光栅化同源：head=(4w).max(10).min(len*0.6)，半宽 0.45head，
/// tip 钉死在释放点。
pub fn arrow_head(points: &[(f32, f32)], width: f32) -> Option<[(f32, f32); 3]> {
    let (start, tip) = match points {
        [s, t] => (*s, *t),
        _ => return None,
    };
    let dx = tip.0 - start.0;
    let dy = tip.1 - start.1;
    let len = dx.hypot(dy);
    if len < 0.001 {
        return None;
    }
    let (ux, uy) = (dx / len, dy / len);
    let (nx, ny) = (-uy, ux);
    let head = (width * 4.).max(10.).min(len * 0.6);
    let base = (tip.0 - ux * head, tip.1 - uy * head);
    Some([
        tip,
        (base.0 + nx * head * 0.45, base.1 + ny * head * 0.45),
        (base.0 - nx * head * 0.45, base.1 - ny * head * 0.45),
    ])
}

fn line_polygons(shape: &Shape) -> Vec<Poly> {
    if shape.kind == ShapeKind::Arrow {
        return arrow_geometry(&shape.points, shape.width);
    }
    if shape.points.len() == 1 {
        return vec![dot(shape.points[0], shape.width)];
    }
    shape
        .points
        .windows(2)
        .filter_map(|w| capsule(w[0], w[1], shape.width))
        .collect()
}

/// 线类光栅化：扫描线 + 8 垂直子采样 + 先求并再混合
/// （shotori `coverage` 同款：单笔内回描交叉永远只算一层，荧光笔不叠色）。
fn rasterize_line(
    shape: &Shape,
    rgba: &mut [u8],
    w: u32,
    h: u32,
    origin: (f32, f32),
    scale: f32,
) {
    let polys: Vec<Poly> = line_polygons(shape)
        .into_iter()
        .map(|p| {
            p.into_iter()
                .map(|(x, y)| ((x - origin.0) * scale, (y - origin.1) * scale))
                .collect()
        })
        .collect();
    if polys.is_empty() {
        return;
    }
    let all_x = polys.iter().flatten().map(|p| p.0);
    let all_y = polys.iter().flatten().map(|p| p.1);
    let left = all_x.clone().fold(f32::INFINITY, f32::min).floor().clamp(0., w as f32) as usize;
    let right = all_x.fold(f32::NEG_INFINITY, f32::max).ceil().clamp(0., w as f32) as usize;
    let top = all_y.clone().fold(f32::INFINITY, f32::min).floor().clamp(0., h as f32) as usize;
    let bottom = all_y.fold(f32::NEG_INFINITY, f32::max).ceil().clamp(0., h as f32) as usize;
    if left >= right || top >= bottom {
        return;
    }
    // 边表（shotori 同款半开区间，水平边自然退化）。
    let edges: Vec<Vec<(f32, f32, f32, f32, f32, f32)>> = polys
        .iter()
        .map(|poly| {
            poly.iter()
                .zip(poly.iter().cycle().skip(1))
                .map(|(a, b)| (a.1.min(b.1), a.1.max(b.1), a.0, a.1, b.0, b.1))
                .collect()
        })
        .collect();
    let color = shape.color.to_be_bytes();
    let mut cover = vec![0f32; right - left];
    for row in top..bottom {
        cover.fill(0.);
        for sample in 0..8 {
            let y = row as f32 + (sample as f32 + 0.5) / 8.;
            let mut intervals = Vec::new();
            for poly_edges in &edges {
                let mut lo = f32::INFINITY;
                let mut hi = f32::NEG_INFINITY;
                for &(ymin, ymax, xa, ya, xb, yb) in poly_edges {
                    if y >= ymin && y < ymax {
                        let x = xa + (y - ya) / (yb - ya) * (xb - xa);
                        lo = lo.min(x);
                        hi = hi.max(x);
                    }
                }
                if lo < hi {
                    intervals.push((lo, hi));
                }
            }
            intervals.sort_by(|a, b| a.0.total_cmp(&b.0));
            let mut union: Option<(f32, f32)> = None;
            let mut paint = |s: f32, e: f32| {
                let first = s.floor().clamp(left as f32, right as f32) as usize;
                let last = e.ceil().clamp(left as f32, right as f32) as usize;
                for x in first..last {
                    cover[x - left] += (e.min(x as f32 + 1.) - s.max(x as f32)).max(0.) / 8.;
                }
            };
            for (s, e) in intervals {
                match union {
                    Some((lo, hi)) if s <= hi => union = Some((lo, hi.max(e))),
                    Some((lo, hi)) => {
                        paint(lo, hi);
                        union = Some((s, e));
                    }
                    None => union = Some((s, e)),
                }
            }
            if let Some((s, e)) = union {
                paint(s, e);
            }
        }
        for (i, c) in cover.iter().enumerate() {
            if *c > 0. {
                blend(rgba, w, (left + i) as i32, row as i32, color, c.min(1.));
            }
        }
    }
}

/// 马赛克：块内 alpha 加权均值回填（shotori `filter` 同款，透明缝隙不晕黑）。
fn rasterize_mosaic(
    shape: &Shape,
    rgba: &mut [u8],
    w: u32,
    h: u32,
    origin: (f32, f32),
    scale: f32,
) {
    let b = shape.bounds;
    let left = ((b.x - origin.0) * scale).round().clamp(0., w as f32) as u32;
    let top = ((b.y - origin.1) * scale).round().clamp(0., h as f32) as u32;
    let right = ((b.x + b.w - origin.0) * scale).round().clamp(0., w as f32) as u32;
    let bottom = ((b.y + b.h - origin.1) * scale).round().clamp(0., h as f32) as u32;
    let step = (shape.width * scale).round().max(1.) as u32;
    for by in (top..bottom).step_by(step as usize) {
        for bx in (left..right).step_by(step as usize) {
            let ex = (bx + step).min(right);
            let ey = (by + step).min(bottom);
            let mut sum = [0u64; 4];
            for row in by..ey {
                for col in bx..ex {
                    let o = ((row * w + col) * 4) as usize;
                    let a = rgba[o + 3] as u64;
                    sum[3] += a;
                    for c in 0..3 {
                        sum[c] += rgba[o + c] as u64 * a;
                    }
                }
            }
            if sum[3] == 0 {
                continue;
            }
            let avg = [
                (sum[0] + sum[3] / 2) / sum[3],
                (sum[1] + sum[3] / 2) / sum[3],
                (sum[2] + sum[3] / 2) / sum[3],
            ];
            for row in by..ey {
                for col in bx..ex {
                    let o = ((row * w + col) * 4) as usize;
                    if rgba[o + 3] == 0 {
                        continue;
                    }
                    for c in 0..3 {
                        rgba[o + c] = avg[c] as u8;
                    }
                }
            }
        }
    }
}

// ---- 文字（cosmic-text 系统字体栈 + 自动回退，shotori text.rs 同款） ----

thread_local! {
    static FONTS: RefCell<(cosmic_text::FontSystem, cosmic_text::SwashCache)> =
        RefCell::new((cosmic_text::FontSystem::new(), cosmic_text::SwashCache::new()));
}

/// 量文字：物理像素下的 (宽, 高)，调用方除回 scale。
pub fn measure_text(text: &str, font_px: f32, wrap_px: f32) -> (f32, f32) {
    use cosmic_text::{Attrs, Buffer, Family, Metrics, Shaping};
    FONTS.with_borrow_mut(|(fonts, _)| {
        let mut buffer = Buffer::new(fonts, Metrics::new(font_px, font_px * 1.35));
        buffer.set_size(Some(wrap_px.max(1.)), Some(10000.));
        buffer.set_text(text, &Attrs::new().family(Family::SansSerif), Shaping::Advanced, None);
        buffer.shape_until_scroll(fonts, false);
        let mut w = 0f32;
        let mut bottom = 0f32;
        for run in buffer.layout_runs() {
            w = w.max(run.line_w);
            bottom = run.line_y + run.line_height;
        }
        (w, bottom)
    })
}

fn rasterize_text(
    shape: &Shape,
    rgba: &mut [u8],
    w: u32,
    h: u32,
    origin: (f32, f32),
    scale: f32,
) {
    use cosmic_text::{Attrs, Buffer, Color, Family, Metrics, Shaping};
    let Some(text) = &shape.text else { return };
    let b = shape.bounds;
    let left = ((b.x - origin.0) * scale).round() as i32;
    let top = ((b.y - origin.1) * scale).round() as i32;
    let width = (b.w * scale).round() as i32;
    let height = (b.h * scale).round() as i32;
    if width <= 0 || height <= 0 {
        return;
    }
    FONTS.with_borrow_mut(|(fonts, cache)| {
        let mut buffer = Buffer::new(
            fonts,
            Metrics::new(shape.width * scale, shape.width * scale * 1.35),
        );
        buffer.set_size(Some(width as f32), Some(height as f32));
        buffer.set_text(text, &Attrs::new().family(Family::SansSerif), Shaping::Advanced, None);
        let [r, g, bl, a] = shape.color.to_be_bytes();
        buffer.draw(fonts, cache, Color::rgba(r, g, bl, a), |x, y, pw, ph, color| {
            for row in y.max(0)..(y + ph as i32).min(height) {
                for col in x.max(0)..(x + pw as i32).min(width) {
                    let (dx, dy) = (left + col, top + row);
                    if dx < 0 || dy < 0 || dx >= w as i32 || dy >= h as i32 {
                        continue;
                    }
                    let p = &mut rgba[((dy as u32 * w + dx as u32) * 4) as usize..][..4];
                    if p[3] == 0 {
                        continue;
                    }
                    let alpha = color.a() as f32 / 255.;
                    for (c, ink) in [color.r(), color.g(), color.b()].into_iter().enumerate() {
                        p[c] = (p[c] as f32 * (1. - alpha) + ink as f32 * alpha).round() as u8;
                    }
                }
            }
        });
    });
}

#[cfg(test)]
mod tests {
    // 注意：这里不要 `use super::*`——它会把 gpui 自带的 `#[test]` 宏引进
    // 来，盖住 Rust 内置的 test 属性导致递归展开失败。只显式引入用到的项。
    use super::Annotations;
    use super::PALETTE;
    use super::ShapeKind;
    use super::bounds_overlap;
    use super::rasterize;
    use super::seg_dist;
    use super::shape_render_bounds;
    use super::size_spec;

    fn sel() -> super::LogicRect {
        super::LogicRect { x: 0., y: 0., w: 200., h: 200. }
    }

    #[test]
    fn palette_is_opaque_and_keys_unique() {
        let mut keys = std::collections::HashSet::new();
        for (color, key) in PALETTE {
            assert_eq!(color & 0xFF, 0xFF, "调色盘必须不透明");
            assert!(keys.insert(key), "颜色 key 重名: {key}");
        }
        assert_eq!(PALETTE.len(), 8);
    }

    #[test]
    fn rect_draft_clamps_and_commits() {
        let mut a = Annotations::default();
        a.toggle(ShapeKind::Rectangle);
        a.begin((10., 10.), sel());
        assert!(a.drag_to((50., 40.), sel()));
        // 同一点再拖不变。
        assert!(!a.drag_to((50., 40.), sel()));
        a.end();
        assert_eq!(a.committed().len(), 1);
        let s = &a.committed()[0];
        assert_eq!((s.bounds.x, s.bounds.y, s.bounds.w, s.bounds.h), (10., 10., 40., 30.));
        // 超出选区的钳住。
        a.begin((190., 190.), sel());
        a.drag_to((500., 500.), sel());
        a.end();
        let s = &a.committed()[1];
        assert!(s.bounds.x + s.bounds.w <= 200.);
        assert!(s.bounds.y + s.bounds.h <= 200.);
    }

    #[test]
    fn tiny_drag_is_ignored() {
        // 误触小点不成形。
        let mut a = Annotations::default();
        a.toggle(ShapeKind::Rectangle);
        a.begin((10., 10.), sel());
        a.drag_to((11., 10.5), sel());
        a.end();
        assert!(a.committed().is_empty());
        // 直线太短也不成形。
        a.toggle(ShapeKind::Line);
        a.begin((10., 10.), sel());
        a.drag_to((10.5, 10.), sel());
        a.end();
        assert!(a.committed().is_empty());
    }

    #[test]
    fn pencil_dot_commits_and_paints() {
        // 单点落成圆点（shotori polygons 单点分支同款）。
        let mut a = Annotations::default();
        a.toggle(ShapeKind::Pencil);
        a.begin((50., 50.), sel());
        a.end();
        assert_eq!(a.committed().len(), 1);
        let mut px = vec![255u8; 100 * 100 * 4];
        rasterize(a.committed().iter().cloned(), &mut px, 100, 100, (0., 0.), 1.);
        assert!(px.chunks_exact(4).any(|p| p[0] != 255 || p[1] != 255 || p[2] != 255));
    }

    #[test]
    fn undo_redo_and_clear() {
        let mut a = Annotations::default();
        assert!(!a.can_undo() && !a.can_redo());
        a.toggle(ShapeKind::Rectangle);
        a.begin((10., 10.), sel());
        a.drag_to((30., 30.), sel());
        a.end();
        a.begin((50., 50.), sel());
        a.drag_to((70., 70.), sel());
        a.end();
        assert_eq!(a.committed().len(), 2);
        assert!(a.undo());
        assert_eq!(a.committed().len(), 1);
        assert!(a.redo());
        assert_eq!(a.committed().len(), 2);
        // 新落笔清掉 redo。
        assert!(a.undo());
        a.begin((80., 80.), sel());
        a.drag_to((90., 90.), sel());
        a.end();
        assert!(!a.can_redo());
        // 清空一条历史，一次撤销全回来。
        assert!(a.clear_all());
        assert!(a.committed().is_empty());
        assert!(a.undo());
        assert_eq!(a.committed().len(), 2);
        assert!(a.redo());
        assert!(a.committed().is_empty());
    }

    #[test]
    fn eraser_removes_whole_shape_and_merges_gesture() {
        let mut a = Annotations::default();
        a.toggle(ShapeKind::Rectangle);
        for (x, y) in [(10., 10.), (60., 60.)] {
            a.begin((x, y), sel());
            a.drag_to((x + 20., y + 20.), sel());
            a.end();
        }
        assert_eq!(a.committed().len(), 2);
        a.toggle(ShapeKind::Eraser);
        a.begin((15., 15.), sel());
        // 同一手势内两次删除合并成一条历史（没有这条合并就是两笔账）。
        let n0 = a.history.len();
        a.drag_to((65., 65.), sel());
        a.end();
        assert!(a.committed().is_empty());
        assert_eq!(a.history.len(), n0, "同一手势该合并成一条历史");
        assert!(a.undo());
        assert_eq!(a.committed().len(), 2);
    }

    #[test]
    fn rect_raster_stays_inside_and_paints_border() {
        // 背景不透明：光栅化只写 RGB 不碰 alpha（透明缝隙跳过），断言才有意义。
        let mut px = vec![0u8; 100 * 100 * 4];
        for p in px.chunks_exact_mut(4) {
            p.copy_from_slice(&[200, 200, 200, 255]);
        }
        let shape = super::Shape {
            kind: ShapeKind::Rectangle,
            text: None,
            bounds: super::LogicRect { x: 10., y: 10., w: 20., h: 20. },
            color: 0xFF0000FF,
            width: 3.,
            points: Vec::new(),
        };
        rasterize([shape].into_iter(), &mut px, 100, 100, (0., 0.), 1.);
        let at = |x: usize, y: usize| &px[(y * 100 + x) * 4..][..4];
        // 上边框红了，框内没动。
        assert_eq!(at(15, 11), &[255, 0, 0, 255]);
        assert_eq!(at(20, 20), &[200, 200, 200, 255]);
        // 框外没动。
        assert_eq!(at(5, 5), &[200, 200, 200, 255]);
    }

    #[test]
    fn ellipse_ring_has_hole_and_aa_edge() {
        let mut px = vec![0u8; 60 * 60 * 4];
        for p in px.chunks_exact_mut(4) {
            p.copy_from_slice(&[10, 10, 10, 255]);
        }
        let shape = super::Shape {
            kind: ShapeKind::Ellipse,
            text: None,
            bounds: super::LogicRect { x: 10., y: 10., w: 40., h: 30. },
            color: 0x00FF00FF,
            width: 4.,
            points: Vec::new(),
        };
        rasterize([shape].into_iter(), &mut px, 60, 60, (0., 0.), 1.);
        let at = |x: usize, y: usize| &px[(y * 60 + x) * 4..][..4];
        // 圆心是洞。
        assert_eq!(at(30, 25), &[10, 10, 10, 255]);
        // 环上有绿（含抗锯齿过渡，不全 255 也可以）。
        assert!(px.chunks_exact(4).any(|p| p[1] > 100 && p[0] < 100));
    }

    #[test]
    fn arrow_tip_lands_on_release_point() {
        let shape = super::Shape {
            kind: ShapeKind::Arrow,
            text: None,
            bounds: super::LogicRect { x: 0., y: 0., w: 0., h: 0. },
            color: 0xFF0000FF,
            width: 3.,
            points: vec![(20., 30.), (70., 30.)],
        };
        let mut px = vec![0u8; 100 * 100 * 4];
        for p in px.chunks_exact_mut(4) {
            p.copy_from_slice(&[0, 0, 0, 255]);
        }
        rasterize([shape].into_iter(), &mut px, 100, 100, (0., 0.), 1.);
        let at = |x: usize, y: usize| &px[(y * 100 + x) * 4..][..4];
        // 杆上有墨，头内有墨但不超射太远。注意 tip 本体是子像素尖：
        // 1x 下 (70,30) 恰好擦过采样行，验头内像素不验尖端像素。
        assert_eq!(at(40, 30)[0], 255);
        assert_eq!(at(60, 30)[0], 255);
        assert_eq!(at(60, 33)[0], 255);
        assert!(at(69, 30)[0] > 0);
        assert_eq!(at(80, 30), &[0, 0, 0, 255]);
    }

    #[test]
    fn highlighter_does_not_double_darken() {
        // 同一笔回描交叉只算一层（shotori 先求并同款）。
        let shape = super::Shape {
            kind: ShapeKind::Highlighter,
            text: None,
            bounds: super::LogicRect { x: 0., y: 0., w: 0., h: 0. },
            color: 0xFFD43B60,
            width: 12.,
            points: vec![(10., 30.), (50., 30.), (10., 30.)],
        };
        let mut px = vec![0u8; 60 * 60 * 4];
        for p in px.chunks_exact_mut(4) {
            p.copy_from_slice(&[255, 255, 255, 255]);
        }
        rasterize([shape].into_iter(), &mut px, 60, 60, (0., 0.), 1.);
        // 单层混合值：白底 + (255,212,59,0x60) → (255,239,181)。
        // 抗锯齿 edge 是向白过渡（g 更大）；叠两层会更暗（g≈229）。
        // 所以：所有上过色的像素 g 都不该比单层更暗。
        let mut painted = 0;
        for p in px.chunks_exact(4) {
            if p[0] != 255 || p[1] != 255 || p[2] != 255 {
                painted += 1;
                assert!(
                    p[1] >= 238,
                    "回描叠色了（g 二次混合会掉到 ~229）：{p:?}"
                );
            }
        }
        assert!(painted > 0);
    }

    #[test]
    fn mosaic_averages_blocks() {
        let mut px = vec![0u8; 8 * 8 * 4];
        for (i, p) in px.chunks_exact_mut(4).enumerate() {
            let v = (i % 2 * 255) as u8;
            p.copy_from_slice(&[v, 0, 0, 255]);
        }
        let shape = super::Shape {
            kind: ShapeKind::Mosaic,
            text: None,
            bounds: super::LogicRect { x: 0., y: 0., w: 8., h: 8. },
            color: 0,
            width: 4.,
            points: Vec::new(),
        };
        rasterize([shape].into_iter(), &mut px, 8, 8, (0., 0.), 1.);
        // 4x4 块内黑白平均成 ~127/128。
        let at = |x: usize, y: usize| px[(y * 8 + x) * 4];
        assert!((120..=135).contains(&at(0, 0)), "块均值不对: {}", at(0, 0));
        assert_eq!(at(0, 0), at(3, 3));
    }

    #[test]
    fn text_raster_does_not_panic_and_paints() {
        // 系统字体差异大，只验不崩 + 有墨（ASCII 必有字形）。
        let shape = super::Shape {
            kind: ShapeKind::Text,
            text: Some("Hi".into()),
            bounds: super::LogicRect { x: 5., y: 5., w: 100., h: 40. },
            color: 0x000000FF,
            width: 20.,
            points: Vec::new(),
        };
        let mut px = vec![255u8; 120 * 60 * 4];
        rasterize([shape].into_iter(), &mut px, 120, 60, (0., 0.), 1.);
        assert!(px.chunks_exact(4).any(|p| p[0] < 200));
        let (w, h) = super::measure_text("Hi", 20., 100.);
        assert!(w > 0. && h > 0.);
    }

    #[test]
    fn seg_dist_hits_segment() {
        assert!(seg_dist((5., 5.), (0., 0.), (10., 0.)) - 5. < 0.01);
        assert!(seg_dist((15., 0.), (0., 0.), (10., 0.)) - 5. < 0.01);
    }

    #[test]
    fn arrow_head_tip_exact_and_wings_symmetric() {
        // tip 钉死在释放点，两翼对称，短拖拽等比缩。
        let head = super::arrow_head(&[(20., 30.), (70., 30.)], 3.).expect("成头");
        assert_eq!(head[0], (70., 30.));
        assert!((head[1].1 - 30. - (30. - head[2].1)).abs() < 0.01);
        assert!(head[1].0 < 70. && head[2].0 < 70.);
        // 零长不成头。
        assert!(super::arrow_head(&[(5., 5.), (5., 5.)], 3.).is_none());
        // 单点不成头。
        assert!(super::arrow_head(&[(5., 5.)], 3.).is_none());
    }

    #[test]
    fn render_bounds_cover_shape_and_overlap_filters() {
        // 区域类：框外扩余量。
        let rect = super::Shape {
            kind: ShapeKind::Rectangle,
            text: None,
            bounds: super::LogicRect { x: 10., y: 10., w: 20., h: 20. },
            color: 0xFF0000FF,
            width: 3.,
            points: Vec::new(),
        };
        let b = shape_render_bounds(&rect);
        assert!(b.x <= 10. && b.y <= 10.);
        assert!(b.x + b.w >= 30. && b.y + b.h >= 30.);
        // 线类：点云外扩线宽。
        let line = super::Shape {
            kind: ShapeKind::Pencil,
            text: None,
            bounds: super::LogicRect { x: 0., y: 0., w: 0., h: 0. },
            color: 0xFF0000FF,
            width: 4.,
            points: vec![(0., 0.), (100., 0.)],
        };
        let b = shape_render_bounds(&line);
        assert!(b.x <= 0. && b.y <= -2.);
        assert!(b.x + b.w >= 100.);
        // 相交判定：贴边不算（没有共享像素），差 1px 交上才算。
        let a = super::LogicRect { x: 0., y: 0., w: 10., h: 10. };
        assert!(bounds_overlap(a, a));
        assert!(bounds_overlap(
            a,
            super::LogicRect { x: 9., y: 9., w: 5., h: 5. }
        ));
        assert!(!bounds_overlap(
            a,
            super::LogicRect { x: 10., y: 10., w: 5., h: 5. }
        ));
        assert!(!bounds_overlap(
            a,
            super::LogicRect { x: 11., y: 0., w: 5., h: 5. }
        ));
    }

    #[test]
    fn sizes_are_per_tool_and_clamped() {
        let mut a = Annotations::default();
        a.toggle(ShapeKind::Pencil);
        a.set_tool_size(10.);
        a.toggle(ShapeKind::Rectangle);
        assert_eq!(a.tool_size(), 3.);
        a.toggle(ShapeKind::Pencil);
        assert_eq!(a.tool_size(), 10.);
        // 范围外钳住（铅笔 1..20）。
        a.set_tool_size(99.);
        assert_eq!(a.tool_size(), 20.);
        assert_eq!(size_spec(ShapeKind::Text), (12., 72.));
    }

    #[test]
    fn text_commit_rejects_blank() {
        let mut a = Annotations::default();
        a.toggle(ShapeKind::Text);
        a.preview_text((10., 10.), 200., "   ".into(), 1.);
        assert!(a.editing_text());
        assert!(!a.commit_text());
        assert!(a.committed().is_empty());
        a.preview_text((10., 10.), 200., "ok".into(), 1.);
        assert!(a.commit_text());
        assert_eq!(a.committed().len(), 1);
    }

    #[test]
    fn toggle_cancels_draft() {
        let mut a = Annotations::default();
        a.toggle(ShapeKind::Rectangle);
        a.begin((10., 10.), sel());
        a.drag_to((30., 30.), sel());
        a.toggle(ShapeKind::Line);
        assert!(a.draft().is_none());
        assert!(a.committed().is_empty());
    }
}
