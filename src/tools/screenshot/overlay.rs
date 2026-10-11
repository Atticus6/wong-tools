//! 区域选择覆盖层：冻结帧打底 + 鼠标拖选 + 确定/取消。
//!
//! shotori 同款做法：`canvas` 在 paint 里挂 `window.on_mouse_event`
//! （窗口级监听，拖出界不丢事件）；工具条按钮走元素级 handler。
//! 覆盖层只管框选：确定后裁剪写回主窗口的截图状态，预览/复制/保存不动。

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use gpui_kit::assets::IconName;
use gpui_kit::base::Selectable as _;
use gpui_kit::component::button::Button;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{ActiveTheme, Disableable as _, Sizable as _};
use gpui_kit::*;
use image::{Frame, ImageBuffer};
use smallvec::SmallVec;

use crate::i18n::t;
use super::select::LogicRect;
use crate::shell::WsShell;

// 覆盖层动作：键盘和工具条按钮共用同一套（shotori 的 actions 三角：
// 按键绑定、工具条、覆盖层 handler 都认它，它谁也不认）。
// 注意 `main.rs` 里用 `Some("ShotOverlay")` 上下文绑定，别用 `None`——
// `enter`/`escape` 在聚光面板已经占了 `None` 上下文，后来居上会把
// 面板确认键顶掉（见 main 的注释）。
gpui_kit::actions!(
    shot,
    [ShotCopy, ShotSave, ShotPin, ShotCancel, ShotUndo, ShotRedo]
);

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
/// 集中在这一个函数里，别处不要再换一次（咖啡杯逐帧直显也走这里）。
pub(crate) fn rgba_to_render_image(img: &image::RgbaImage) -> Arc<RenderImage> {
    let mut bgra = img.clone().into_raw();
    // 长度永远是 4 的倍数，余数分支走不到，纯粹让编译器闭嘴。
    for px in bgra.chunks_exact_mut(4) {
        px.swap(0, 2);
    }
    let buf =
        ImageBuffer::from_raw(img.width(), img.height(), bgra).expect("像素长度和尺寸对不上");
    Arc::new(RenderImage::new(SmallVec::from_elem(Frame::new(buf), 1)))
}

/// GPUI 的 `Bounds<Pixels>` 摊成 `(x, y, w, h)`，`overlay_placement` 好接。
fn rect4(b: Bounds<Pixels>) -> (f32, f32, f32, f32) {
    (
        b.origin.x.as_f32(),
        b.origin.y.as_f32(),
        b.size.width.as_f32(),
        b.size.height.as_f32(),
    )
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
    /// 顶边被系统保留区（macOS 菜单栏）吃掉的高度，逻辑像素。冻结帧要按
    /// 它往上挪，选区换算回原图时也要补回来，否则差一个菜单栏的高度。
    top_inset: f32,
    /// 冻结帧整幅的逻辑高度（含被菜单栏盖住那几行），渲染时按它铺。
    frozen_h: f32,
    anchor: Option<Point<Pixels>>,
    current: Point<Pixels>,
    dragging: bool,
    shell: WeakEntity<WsShell>,
    /// 键盘焦点：动作走 `ShotOverlay` 上下文分发，根元素必须在焦点路径里。
    focus: FocusHandle,
    /// 标注状态（shotori `Annotations` 的简化版：9 工具 + 撤销/重做）。
    ann: super::annotate::Annotations,
    /// 标注笔画在途（选区拖拽走 `dragging`，两边互斥）。
    ann_drag: bool,
    /// 已提交标注的预览层（窗口尺寸 × scale，透明底），落笔/撤销时重烤。
    committed_img: Option<Arc<RenderImage>>,
    /// 未提交草稿的预览层（包围盒小图），拖拽/输入时重烤。
    draft_img: Option<(Arc<RenderImage>, (f32, f32), (f32, f32))>,
    /// 文字输入框（实体 + 锚点窗口坐标），`None` 即不在输入。
    text_editor: Option<(Entity<InputState>, (f32, f32))>,
    /// 输入框事件订阅：必须存着，drop 即退订，输入中就收不到改字事件了。
    /// 和 `text_editor` 同生共死。
    text_sub: Option<Subscription>,
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

        // 落位要按 GPUI 的显示器信息算，不能只信 xcap：macOS 会把盖菜单栏的
        // 窗口往下挪，我们得自己把顶部让出来，不然屏幕上两个菜单栏
        // （详见 overlay_placement 的注释）。读不到显示器就退回全屏无内缩，
        // 行为跟修复前一致，不会更糟。
        let (_, _, lw, lh) = geo.logical();
        let placement = cx
            .primary_display()
            .map(|d| {
                let f = d.bounds();
                let v = d.visible_bounds();
                super::overlay_placement(rect4(f), rect4(v))
            })
            .unwrap_or(super::overlay_placement((0., 0., lw, lh), (0., 0., lw, lh)));
        // 冻结帧整幅高：拿捕获图的物理像素反推逻辑高，别信窗口尺寸——
        // 窗口被系统挪过/裁过，尺寸不等于捕获图原高。
        let frozen_h = image.height() as f32 / geo.scale;

        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds {
                origin: point(px(placement.origin.0), px(placement.origin.1)),
                size: size(px(placement.size.0), px(placement.size.1)),
            })),
            titlebar: Some(TitlebarOptions {
                // “框选截图”
                title: Some(t("app.shot_overlay_title").into()),
                appears_transparent: true,
                // macOS 红绿灯挪出窗外：框选要的是全屏无边框冻结帧，框架没给
                // 隐藏开关，位置法是目前唯一的办法（同 spotlight 面板/钉屏窗）。
                traffic_light_position: if cfg!(target_os = "macos") {
                    Some(point(px(-100.), px(-100.)))
                } else {
                    None
                },
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
            cx.new(|cx| {
                Self::new(
                    shell,
                    image,
                    frozen,
                    placement.size.0,
                    placement.size.1,
                    placement.top_inset,
                    frozen_h,
                    window,
                    cx,
                )
            })
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
        top_inset: f32,
        frozen_h: f32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        // 键盘动作要进焦点路径：开窗即聚焦根元素。
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        Self {
            image,
            frozen,
            scale: window.scale_factor(),
            size: size(px(lw), px(lh)),
            top_inset,
            frozen_h,
            anchor: None,
            current: point(px(0.), px(0.)),
            dragging: false,
            shell,
            focus,
            ann: super::annotate::Annotations::default(),
            ann_drag: false,
            committed_img: None,
            draft_img: None,
            text_editor: None,
            text_sub: None,
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

    /// 按下是否落在工具条上：是的话归按钮，不管框选/标注。
    /// 注意拖拽中选区是活的，这里的判定和渲染用同一个矩形，不会打架。
    /// 没选区时工具条根本不显示，直接返回 false，别吞了起拖的按下。
    /// 第二排只在显示时算（工具激活或输入中），藏着时不挡画布。
    fn in_toolbar(&self, p: Point<Pixels>) -> bool {
        if self.selection().is_none() {
            return false;
        }
        let row = self.toolbar_rect();
        if hit_rect(row, p) {
            return true;
        }
        if self.ann.tool().is_some() || self.text_editor.is_some() {
            let opt = super::select::options_rect(
                row,
                self.size.width.as_f32(),
                self.size.height.as_f32(),
            );
            if hit_rect(opt, p) {
                return true;
            }
        }
        false
    }

    /// 当前选区的裁剪图（逻辑→物理换算 + 界内钳制，见 select）。
    ///
    /// 窗内 y=0 是覆盖层窗口顶边，它在屏幕上比捕获图顶端低 `top_inset`
    /// （macOS 菜单栏）。所以选区往回收之前先把这段补回去，否则裁出来的
    /// 图整体上移一个菜单栏的高度。
    ///
    /// 标注一起合成：形状是窗内坐标，裁剪区左上在捕获图里是
    /// `(sel.x, sel.y + top_inset)`，光栅化原点传它。
    fn cropped(&self) -> Option<image::RgbaImage> {
        let rect = self.selection()?;
        let rect = super::select::translate(rect, 0., self.top_inset);
        let (x, y, w, h) =
            super::select::to_pixels(rect, self.scale, self.image.width(), self.image.height())?;
        let mut img = super::select::crop(&self.image, x, y, w, h);
        super::annotate::rasterize(
            self.ann.committed().iter().cloned(),
            &mut img,
            w,
            h,
            (rect.x, rect.y),
            self.scale,
        );
        Some(img)
    }

    /// 窗口区底图：从捕获图里按窗内坐标裁（预览层做底，标注直接盖上去）。
    /// 预览层不能是透明空画布：`blend` 碰到 alpha=0 的像素直接跳过
    /// （多显示器缝隙保护），空画布上画什么都留不下。盖在冻结帧拷贝上，
    /// 像素行为和导出完全一致；马赛克也必须有源像素才能看。
    /// 窗内 (x,y) 在捕获图里是 (x, y+top_inset)，再 ×scale 到物理像素。
    fn window_base(&self, x: f32, y: f32, w: f32, h: f32) -> Option<image::RgbaImage> {
        let s = self.scale;
        let w = (w * s).round().max(1.) as u32;
        let h = (h * s).round().max(1.) as u32;
        let ox = ((x * s).round().max(0.) as u32).min(self.image.width().saturating_sub(1));
        let oy = (((y + self.top_inset) * s).round().max(0.) as u32)
            .min(self.image.height().saturating_sub(1));
        Some(super::select::crop(&self.image, ox, oy, w, h))
    }

    /// 重烤已提交层：冻结帧拷贝 + 标注盖上去，落笔/撤销/清空后调。
    fn refresh_committed(&mut self) {
        if self.ann.committed().is_empty() {
            self.committed_img = None;
            return;
        }
        let (cw, ch) = (self.size.width.as_f32(), self.size.height.as_f32());
        let Some(mut base) = self.window_base(0., 0., cw, ch) else {
            self.committed_img = None;
            return;
        };
        let (w, h) = (base.width(), base.height());
        super::annotate::rasterize(
            self.ann.committed().iter().cloned(),
            &mut base,
            w,
            h,
            (0., 0.),
            self.scale,
        );
        self.committed_img = Some(rgba_to_render_image(&base));
    }

    /// 重烤草稿层：只剩文字草稿走 CPU 光栅（敲键时烤，不在鼠标移动路径上）。
    /// 其他草稿由下面的矢量画布 GPU 直画，跟手不烤像素——之前整笔重烤
    /// 是划线卡顿的主因（debug 下更明显）。
    fn refresh_draft(&mut self) {
        use super::annotate::ShapeKind;
        let is_text = self
            .ann
            .draft()
            .is_some_and(|d| d.kind == ShapeKind::Text);
        if !is_text {
            self.draft_img = None;
            return;
        }
        let cw = self.size.width.as_f32();
        let ch = self.size.height.as_f32();
        let (Some(d), Some(b)) = (self.ann.draft(), self.ann.draft_render_bounds(cw, ch)) else {
            self.draft_img = None;
            return;
        };
        // 空文字没墨：别盖一张同样的底图上去浪费。
        if d.kind == super::annotate::ShapeKind::Text
            && d.text.as_deref().is_none_or(|s| s.is_empty())
        {
            self.draft_img = None;
            return;
        }
        let Some(mut base) = self.window_base(b.x, b.y, b.w, b.h) else {
            self.draft_img = None;
            return;
        };
        let (w, h) = (base.width(), base.height());
        // 已提交的一起烤进去：草稿小图盖在全窗层上面，框住那块旧线
        // 不能丢，否则划线时框内旧线闪没；只烤相交的，帧成本和形状
        // 总数脱钩（各光栅化自己按包围盒裁剪，过滤不改变画面）。
        let committed = self.ann.committed().iter().filter(|s| {
            super::annotate::bounds_overlap(super::annotate::shape_render_bounds(s), b)
        });
        super::annotate::rasterize(
            committed.cloned().chain([d.clone()]),
            &mut base,
            w,
            h,
            (b.x, b.y),
            self.scale,
        );
        self.draft_img = Some((rgba_to_render_image(&base), (b.x, b.y), (b.w, b.h)));
    }

    /// 在选区内点下文字工具：锚点钳在选区里，输入框随后在 render 里摆出来。
    fn open_text_editor(
        &mut self,
        at: Point<Pixels>,
        sel: super::select::LogicRect,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let anchor = (
            at.x.as_f32().clamp(sel.x, sel.x + sel.w),
            at.y.as_f32().clamp(sel.y, sel.y + sel.h),
        );
        // 换行宽：锚点到选区右 edge，太窄也给 80 保底。
        let wrap = ((sel.x + sel.w - anchor.0).max(80.)).min(480.);
        self.ann.preview_text(anchor, wrap, String::new(), self.scale);
        // “输入文字，回车确定”
        let hint = t("app.shot_text_hint");
        let editor = cx.new(|cx| InputState::new(window, cx).placeholder(hint));
        let sub = cx.subscribe(
            &editor,
            |this, editor, event: &InputEvent, cx| match event {
                InputEvent::Change => {
                    let value = editor.read(cx).value().to_string();
                    if let Some((_, (ax, ay))) = this.text_editor_anchor() {
                        if let Some(sel) = this.selection() {
                            let wrap = ((sel.x + sel.w - ax).max(80.)).min(480.);
                            this.ann.preview_text((ax, ay), wrap, value, this.scale);
                            this.refresh_draft();
                            cx.notify();
                        }
                    }
                }
                InputEvent::PressEnter { .. } => {
                    this.commit_text_editor();
                    this.refresh_draft();
                    cx.notify();
                }
                InputEvent::Blur => {
                    // 点窗外即确定，别丢字（Esc 取消走 ShotCancel，见 cancel）。
                    if this.text_editor.is_some() {
                        this.commit_text_editor();
                        this.refresh_committed();
                        this.refresh_draft();
                        cx.notify();
                    }
                }
                InputEvent::Focus => {}
            },
        );
        window.focus(&editor.read(cx).focus_handle(cx), cx);
        self.text_editor = Some((editor, anchor));
        self.text_sub = Some(sub);
        self.refresh_draft();
    }

    fn text_editor_anchor(&self) -> Option<(Entity<InputState>, (f32, f32))> {
        self.text_editor.clone()
    }

    /// 提交输入：空串算取消。返回是否落了盘（调用方重烤已提交层）。
    fn commit_text_editor(&mut self) -> bool {
        self.text_editor = None;
        self.text_sub = None;
        self.ann.commit_text()
    }

    /// 取消输入。返回是否真取消了东西。
    fn cancel_text_editor(&mut self) -> bool {
        self.text_editor = None;
        self.text_sub = None;
        self.ann.cancel_text()
    }

    /// 收尾前先把手里的活结掉：在途笔画提交，输入框按提交算（别丢字）。
    fn settle_annotations(&mut self) {
        if self.text_editor.is_some() {
            if self.commit_text_editor() {
                self.refresh_committed();
            }
        } else if self.ann.draft().is_some() {
            self.ann.end();
            self.refresh_committed();
        }
        self.draft_img = None;
        self.ann_drag = false;
    }

    /// 复制选区：裁剪→剪贴板→写回主窗口→关窗。
    /// 剪贴板失败就留在覆盖层（选区还在，可重试或取消）。
    fn finish_copy(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // 输入框开着时回车是“提交文字”不是复制（见订阅里的 PressEnter 注释）。
        if self.text_editor.is_some() {
            if self.commit_text_editor() {
                self.refresh_committed();
            }
            self.refresh_draft();
            cx.notify();
            return;
        }
        self.settle_annotations();
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
        self.settle_annotations();
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

    /// 钉屏：裁剪→开置顶浮窗→关覆盖层。
    /// 和复制/保存不同，这里不写回主窗口：钉屏是独立的一扇窗，用户对照着看，
    /// 主窗口的截图状态原样保留。钉屏开窗失败就留在覆盖层（选区还在）。
    fn finish_pin(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.settle_annotations();
        let Some(rect) = self.selection() else {
            return;
        };
        let Some(cropped) = self.cropped() else {
            return;
        };
        // 选区是窗内坐标，转全局：钉屏开在屏幕上，不是覆盖层里。
        let origin = window.bounds().origin;
        let global = Bounds::new(
            point(
                px(origin.x.as_f32() + rect.x),
                px(origin.y.as_f32() + rect.y),
            ),
            size(px(rect.w), px(rect.h)),
        );
        let desk = cx
            .primary_display()
            .map(|d| d.bounds())
            .unwrap_or_else(|| window.bounds());
        if super::pin::PinWindow::open(cropped, global, desk, cx).is_err() {
            return;
        }
        window.remove_window();
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

    /// 取消：Esc 分级——输入中先退输入框，草稿先丢草稿，工具先收工具，
    /// 都没有才关窗（shotori 两段式同款）。
    fn cancel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.cancel_text_editor() {
            self.refresh_draft();
            cx.notify();
            return;
        }
        if self.ann.cancel_draft() {
            self.refresh_draft();
            cx.notify();
            return;
        }
        if self.ann.deactivate() {
            cx.notify();
            return;
        }
        window.remove_window();
    }

    /// 输入中换颜色/粗细：按新字号颜色重排草稿（字是编辑器里的，锚点不变）。
    fn repreview_text(&mut self, cx: &mut App) {
        if let Some((editor, (ax, ay))) = self.text_editor.clone() {
            if let Some(sel) = self.selection() {
                let value = editor.read(cx).value().to_string();
                let wrap = ((sel.x + sel.w - ax).max(80.)).min(480.);
                self.ann.preview_text((ax, ay), wrap, value, self.scale);
            }
        }
    }

    /// 工具按钮：先把输入框按提交结算（别丢字），再切换。
    fn select_tool(&mut self, kind: super::annotate::ShapeKind) {
        if self.text_editor.is_some() && self.commit_text_editor() {
            self.refresh_committed();
        }
        self.ann.toggle(kind);
        self.refresh_draft();
    }

    /// 撤销/重做：刷新已提交层。
    fn undo_ann(&mut self, cx: &mut Context<Self>) {
        if self.ann.undo() {
            self.refresh_committed();
            cx.notify();
        }
    }

    fn redo_ann(&mut self, cx: &mut Context<Self>) {
        if self.ann.redo() {
            self.refresh_committed();
            cx.notify();
        }
    }

    /// 清空全部标注：一条历史，一次撤销全回来。
    fn clear_ann(&mut self, cx: &mut Context<Self>) {
        if self.ann.clear_all() {
            self.refresh_committed();
            cx.notify();
        }
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
        // 第二排跟第一排走；工具激活或输入中才摆出来。
        let (ox, oy, ow, oh) = super::select::options_rect(
            (bx, by, bw, bh),
            self.size.width.as_f32(),
            self.size.height.as_f32(),
        );
        let show_options =
            show_toolbar && (self.ann.tool().is_some() || self.text_editor.is_some());

        div()
            .id("select-overlay")
            .size_full()
            .relative()
            .bg(black())
            .cursor_crosshair()
            // 键盘动作：`ShotOverlay` 上下文（见 main 的绑定），根元素进焦点路径
            // 才能收到。没选区时动作自己 guard（`cropped()` 返回 None 直接回），
            // 不会误触。
            .track_focus(&self.focus)
            .key_context("ShotOverlay")
            .on_action(cx.listener(|this, _: &ShotCopy, window, cx| {
                this.finish_copy(window, cx);
            }))
            .on_action(cx.listener(|this, _: &ShotSave, window, cx| {
                this.finish_save(window, cx);
            }))
            .on_action(cx.listener(|this, _: &ShotPin, window, cx| {
                this.finish_pin(window, cx);
            }))
            .on_action(cx.listener(|this, _: &ShotCancel, window, cx| {
                this.cancel(window, cx);
            }))
            .on_action(cx.listener(|this, _: &ShotUndo, _, cx| {
                this.undo_ann(cx);
            }))
            .on_action(cx.listener(|this, _: &ShotRedo, _, cx| {
                this.redo_ann(cx);
            }))
            // 冻结帧：不透明铺满（shotori 原样：flow + size_full）。
            // 上移 top_inset：窗顶在菜单栏之下，冻结帧第 0 行是捕获时的菜单栏，
            // 不上移就会跟真菜单栏叠成两个（见 open() 的注释）。
            .child(
                img(self.frozen.clone())
                    .absolute()
                    .left_0()
                    .top(px(-self.top_inset))
                    .w(self.size.width)
                    .h(px(self.frozen_h)),
            )
            // 事件网：窗口级鼠标监听（shotori 原样，看不见摆哪都行）。
            .child(pointer_sink(input))
            // 标注预览层：已提交全窗层 + 草稿包围盒小图。冻结帧拷贝打底、
            // CPU 现烤的 RenderImage，和导出同一套光栅化，所见即所得
            // （马赛克/荧光笔/文字都不走矢量近似）。
            .children(self.committed_img.clone().map(|image| {
                img(image)
                    .absolute()
                    .left_0()
                    .top_0()
                    .w(self.size.width)
                    .h(self.size.height)
                    .into_any_element()
            }))
            .children(self.draft_img.clone().map(|(image, (dx, dy), (dw, dh))| {
                img(image)
                    .absolute()
                    .left(px(dx))
                    .top(px(dy))
                    .w(px(dw))
                    .h(px(dh))
                    .into_any_element()
            }))
            // 变暗 + 描边必须在预览层上面：已提交层是不透明实拍拷贝，
            // 放它后面会把变暗和选区框全盖掉（直接画，不经过布局）。
            .child(dim_canvas(sel, accent))
            // 草稿矢量预览（GPU 直画，跟手不卡；文字草稿走光栅小图）。
            .child(draft_canvas(self.ann.draft().cloned(), accent))
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
            // 没选区时的居中提示。工具激活后换成绘制提示。
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
            .children(
                (sel.is_some()
                    && self.ann.tool().is_some()
                    && self.ann.committed().is_empty()
                    && self.ann.draft().is_none()
                    && self.text_editor.is_none())
                .then(|| {
                    div()
                        .flex_1()
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_size(px(15.))
                        .text_color(white().opacity(0.8))
                        // “在选区内拖拽绘制”
                        .child(t("app.shot_draw_hint"))
                        .into_any_element()
                }),
            )
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
                    // Snipaste 节奏：钮间 4px，比之前的 2px 透气，图标不挤。
                    .gap(px(4.))
                    .px(px(8.))
                    .rounded(px(8.))
                    .bg(bar_bg)
                    .border_1()
                    .border_color(bar_border)
                    // 工具条吃掉左键按下，不让它漏到框选去（shotori 原样；
                    // 几何判定还在，真漏了也开不出选区）。
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    // 标注组（shotori 同款顺序 + 末尾清空）：点即切换工具，
                    // 再点收起；选中态走 selected 样式。
                    .children(super::annotate::TOOL_ORDER.iter().map(|kind| {
                        let (id, icon, tip) = tool_meta(*kind);
                        let active = self.ann.tool() == Some(*kind);
                        let k = *kind;
                        tool_toggle(
                            id,
                            icon,
                            tip,
                            active,
                            cx.listener(move |this, _, _, cx| {
                                this.select_tool(k);
                                cx.notify();
                            }),
                        )
                        .into_any_element()
                    }))
                    .child(toolbar_icon(
                        "overlay-clear",
                        IconName::Trash,
                        // “清空全部标注”
                        t("app.shot_clear"),
                        cx.listener(|this, _, _, cx| this.clear_ann(cx)),
                    ))
                    .child(div().w(px(1.)).h(px(18.)).mx(px(4.)).bg(bar_border))
                    // 历史组：没东西时置灰，点了也没事（handler 里再守一次，
                    // `disabled` 只换样式不拦点击）。
                    .child(
                        Button::new("overlay-undo")
                            .tooltip(
                                // “撤销”
                                t("app.shot_tool_undo"),
                            )
                            .cursor_pointer()
                            .size(px(30.))
                            .rounded(px(5.))
                            .disabled(!self.ann.can_undo())
                            .icon(IconName::Undo2)
                            .on_click(cx.listener(|this, _, _, cx| this.undo_ann(cx))),
                    )
                    .child(
                        Button::new("overlay-redo")
                            .tooltip(
                                // “重做”
                                t("app.shot_tool_redo"),
                            )
                            .cursor_pointer()
                            .size(px(30.))
                            .rounded(px(5.))
                            .disabled(!self.ann.can_redo())
                            .icon(IconName::Redo2)
                            .on_click(cx.listener(|this, _, _, cx| this.redo_ann(cx))),
                    )
                    .child(div().w(px(1.)).h(px(18.)).mx(px(4.)).bg(bar_border))
                    .child(toolbar_icon(
                        "overlay-copy",
                        IconName::Copy,
                        // “复制”
                        t("app.shot_copy"),
                        cx.listener(|this, _, window, cx| this.finish_copy(window, cx)),
                    ))
                    .child(toolbar_icon(
                        "overlay-save",
                        // 软盘样式过时，下载箭头更直观（对齐截图工具栏）。
                        IconName::Download,
                        // “保存”
                        t("app.shot_save"),
                        cx.listener(|this, _, window, cx| this.finish_save(window, cx)),
                    ))
                    .child(toolbar_icon(
                        "overlay-pin",
                        IconName::Pin,
                        // “钉屏”
                        t("app.shot_pin"),
                        cx.listener(|this, _, window, cx| this.finish_pin(window, cx)),
                    ))
                    .child(div().w(px(1.)).h(px(18.)).mx(px(4.)).bg(bar_border))
                    .child(toolbar_icon(
                        "overlay-cancel",
                        IconName::X,
                        // “取消”
                        t("app.shot_cancel"),
                        cx.listener(|this, _, window, cx| this.cancel(window, cx)),
                    ))
            }))
            // 第二排：粗细 + 颜色（参考截图第二排）。工具激活或输入中才摆出来。
            .children(show_options.then(|| {
                let bar_bg = cx.theme().popover;
                let bar_border = cx.theme().border;
                let size = self.ann.tool_size();
                let color_ix = self.ann.color_ix();
                div()
                    .absolute()
                    .left(px(ox))
                    .top(px(oy))
                    .w(px(ow))
                    .h(px(oh))
                    .flex()
                    .items_center()
                    .justify_center()
                    .gap(px(4.))
                    .px(px(8.))
                    .rounded(px(8.))
                    .bg(bar_bg)
                    .border_1()
                    .border_color(bar_border)
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(toolbar_icon(
                        "overlay-size-dec",
                        IconName::Minus,
                        // “变细”
                        t("app.shot_size_dec"),
                        cx.listener(|this, _, _, cx| {
                            this.ann.step_tool_size(false);
                            this.repreview_text(cx);
                            this.refresh_draft();
                            cx.notify();
                        }),
                    ))
                    .child(
                        div()
                            .w(px(36.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_size(px(TEXT_SM))
                            .child(format!("{:.0}", size)),
                    )
                    .child(toolbar_icon(
                        "overlay-size-inc",
                        IconName::Plus,
                        // “变粗”
                        t("app.shot_size_inc"),
                        cx.listener(|this, _, _, cx| {
                            this.ann.step_tool_size(true);
                            this.repreview_text(cx);
                            this.refresh_draft();
                            cx.notify();
                        }),
                    ))
                    .child(div().w(px(1.)).h(px(18.)).mx(px(4.)).bg(bar_border))
                    .children(super::annotate::PALETTE.iter().enumerate().map(|(ix, (color, key))| {
                        let name = t(&format!("app.shot_c_{key}"));
                        // “颜色：红”…
                        let tip = format!("{}：{name}", t("app.shot_color"));
                        let bytes = color.to_be_bytes();
                        let hsla: Hsla = Rgba {
                            r: bytes[0] as f32 / 255.,
                            g: bytes[1] as f32 / 255.,
                            b: bytes[2] as f32 / 255.,
                            a: 1.,
                        }
                        .into();
                        Button::new(ElementId::named_usize("overlay-color", ix))
                            .tooltip(tip)
                            .cursor_pointer()
                            .size(px(28.))
                            .rounded(px(5.))
                            .toggled(color_ix == ix)
                            .selected(color_ix == ix)
                            .child(
                                div()
                                    .size(px(20.))
                                    .rounded(px(4.))
                                    .bg(hsla)
                                    .border_1()
                                    .border_color(bar_border),
                            )
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.ann.set_color(ix);
                                this.repreview_text(cx);
                                this.refresh_draft();
                                cx.notify();
                            }))
                            .into_any_element()
                    }))
            }))
            // 文字输入框：锚点摆出来，宽 300（近右 edge 收一收）。
            .children(self.text_editor.clone().map(|(editor, (ax, ay))| {
                let ex = ax.min(self.size.width.as_f32() - 310.).max(8.);
                div()
                    .absolute()
                    .left(px(ex))
                    .top(px(ay))
                    .w(px(300.))
                    .rounded(px(8.))
                    .bg(cx.theme().popover)
                    .border_1()
                    .border_color(cx.theme().border)
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(Input::new(&editor).small().w_full())
                    .into_any_element()
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

/// 工具条图标按钮（官方范式：`Button::icon()` 内置图标，
/// 尺寸/各状态配色按钮自己管，不再手塞 `svg()`/`Icon` 子元素）。
/// 30px 方块；图标大小跟按钮走（见组件文档 icon-only 用法）。
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
        .icon(icon)
        .on_click(on_click)
}

/// 工具的 id/图标/tooltip（AGENTS 约定：文案旁注中文原文）。
fn tool_meta(kind: super::annotate::ShapeKind) -> (&'static str, IconName, String) {
    use super::annotate::ShapeKind::*;
    match kind {
        // “矩形”
        // 实线方框小尺寸糊成一横，虚线框+光标=拖拽画框的手势，一眼可辨。
        Rectangle => (
            "overlay-tool-rect",
            IconName::SquareDashedMousePointer,
            t("app.shot_tool_rect"),
        ),
        // “椭圆”
        // Lucide 的 ellipse 又扁又宽（rx10 ry6），小尺寸像“二”；
        // 正圆环一眼可辨，工具照样画椭圆。
        Ellipse => ("overlay-tool-ellipse", IconName::Circle, t("app.shot_tool_ellipse")),
        // “直线”
        Line => ("overlay-tool-line", IconName::Slash, t("app.shot_tool_line")),
        // “箭头”
        Arrow => (
            "overlay-tool-arrow",
            IconName::ArrowUpRight,
            t("app.shot_tool_arrow"),
        ),
        // “画笔”
        Pencil => ("overlay-tool-pencil", IconName::Pencil, t("app.shot_tool_pencil")),
        // “荧光笔”
        Highlighter => (
            "overlay-tool-highlighter",
            IconName::Highlighter,
            t("app.shot_tool_highlighter"),
        ),
        // “马赛克”
        // 2x2 看着像表格，用 3x3 像素点才是马赛克。
        Mosaic => ("overlay-tool-mosaic", IconName::Grid3x3, t("app.shot_tool_mosaic")),
        // “文字”
        Text => ("overlay-tool-text", IconName::Type, t("app.shot_tool_text")),
        // “橡皮擦”
        Eraser => ("overlay-tool-eraser", IconName::Eraser, t("app.shot_tool_eraser")),
    }
}

/// 工具开关：同尺寸图标按钮 + 激活态样式（`selected` 管样式，
/// `toggled` 补无障碍元数据，两边都置）。
/// 注意别调 `.hover()`：Button 内部已经设了 hover，再设会 panic。
fn tool_toggle(
    id: &'static str,
    icon: IconName,
    tooltip: String,
    active: bool,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    Button::new(id)
        .tooltip(tooltip)
        .cursor_pointer()
        .size(px(30.))
        .rounded(px(5.))
        .toggled(active)
        .selected(active)
        .icon(icon)
        .on_click(on_click)
}

/// 矩形命中（逻辑像素，含 edge）：工具条两排的按下判定共用。
fn hit_rect(r: (f32, f32, f32, f32), p: Point<Pixels>) -> bool {
    let (x, y) = (p.x.as_f32(), p.y.as_f32());
    x >= r.0 && x <= r.0 + r.2 && y >= r.1 && y <= r.1 + r.3
}

/// u32 颜色（0xRRGGBBAA）转 Hsla。
fn hsla_of(color: u32) -> Hsla {
    let b = color.to_be_bytes();
    Rgba {
        r: b[0] as f32 / 255.,
        g: b[1] as f32 / 255.,
        b: b[2] as f32 / 255.,
        a: b[3] as f32 / 255.,
    }
    .into()
}

/// 折线描边路径（画笔/直线/箭头杆/椭圆近似共用）。
/// 单点或零长画圆点：描边路径画不出东西。
fn stroke_path(pts: &[(f32, f32)], width: f32) -> Option<Path<Pixels>> {
    let mut it = pts.iter();
    let &(x0, y0) = it.next()?;
    let mut b = PathBuilder::stroke(px(width));
    b.move_to(point(px(x0), px(y0)));
    for &(x, y) in it {
        b.line_to(point(px(x), px(y)));
    }
    b.build().ok()
}

/// 实心多边形（箭头头/圆点）。
fn fill_path(pts: &[(f32, f32)]) -> Option<Path<Pixels>> {
    let mut it = pts.iter();
    let &(x0, y0) = it.next()?;
    let mut b = PathBuilder::fill();
    b.move_to(point(px(x0), px(y0)));
    for &(x, y) in it {
        b.line_to(point(px(x), px(y)));
    }
    b.close();
    b.build().ok()
}

/// 实心圆点（单击不成线时）：16 边形近似。
fn dot_path(c: (f32, f32), width: f32) -> Option<Path<Pixels>> {
    let pts: Vec<(f32, f32)> = (0..16)
        .map(|i| {
            let (s, co) = (i as f32 * std::f32::consts::TAU / 16.).sin_cos();
            (c.0 + co * width / 2., c.1 + s * width / 2.)
        })
        .collect();
    fill_path(&pts)
}

/// 草稿矢量预览：GPU 路画当前笔，落盘才走 CPU 光栅化。
/// 每帧只有 O(点数) 的路径拼装 + 一次 notify，不裁底图不烤像素，
/// 跟手只花零头。比例和光栅化同源（矩形内缩描边/箭头头见 annotate），
/// 预览和落盘基本一致；荧光笔自交处预览会轻微叠色（单遍混合），
/// 落盘是单层——草稿是过渡态，可接受。
/// 文字草稿不走这里（仍走光栅小图，只在敲键时烤）。
fn draft_canvas(draft: Option<super::annotate::Shape>, accent: Hsla) -> impl IntoElement {
    use super::annotate::ShapeKind;
    canvas(
        |_, _, _| (),
        move |_, (), window, _| {
            let Some(d) = &draft else { return };
            if d.kind == ShapeKind::Text || d.kind == ShapeKind::Eraser {
                return;
            }
            let color = hsla_of(d.color);
            match d.kind {
                ShapeKind::Rectangle => {
                    // 和 rasterize_rect 同一份内缩描边。
                    let b = super::select::LogicRect {
                        x: d.bounds.x,
                        y: d.bounds.y,
                        w: d.bounds.w,
                        h: d.bounds.h,
                    };
                    let lw = d.width.min(b.w / 2.).min(b.h / 2.);
                    for (x, y, w, h) in [
                        (b.x, b.y, b.w, lw),
                        (b.x, b.y + b.h - lw, b.w, lw),
                        (b.x, b.y + lw, lw, (b.h - lw * 2.).max(0.)),
                        (b.x + b.w - lw, b.y + lw, lw, (b.h - lw * 2.).max(0.)),
                    ] {
                        if w > 0. && h > 0. {
                            window.paint_quad(fill(
                                Bounds::from_corners(point(px(x), px(y)), point(px(x + w), px(y + h))),
                                color,
                            ));
                        }
                    }
                }
                ShapeKind::Ellipse => {
                    // 64 边形近似环。
                    let cx = d.bounds.x + d.bounds.w / 2.;
                    let cy = d.bounds.y + d.bounds.h / 2.;
                    let (rx, ry) = (d.bounds.w / 2., d.bounds.h / 2.);
                    if rx <= 0. || ry <= 0. {
                        return;
                    }
                    let pts: Vec<(f32, f32)> = (0..64)
                        .map(|i| {
                            let a = i as f32 * std::f32::consts::TAU / 64.;
                            (cx + a.cos() * rx, cy + a.sin() * ry)
                        })
                        .collect();
                    let mut it = pts.iter();
                    let &(x0, y0) = it.next().expect("64 个点，非空");
                    let mut b = PathBuilder::stroke(px(d.width));
                    b.move_to(point(px(x0), px(y0)));
                    for &(x, y) in it {
                        b.line_to(point(px(x), px(y)));
                    }
                    b.close();
                    if let Ok(path) = b.build() {
                        window.paint_path(path, color);
                    }
                }
                ShapeKind::Line => {
                    // 按下未拖时两点重合，描边画不出东西，落个圆点。
                    if d.points[0] == d.points[1] {
                        if let Some(path) = dot_path(d.points[0], d.width) {
                            window.paint_path(path, color);
                        }
                    } else if let Some(path) = stroke_path(&d.points, d.width) {
                        window.paint_path(path, color);
                    }
                }
                ShapeKind::Arrow => {
                    if let Some([tip, w1, w2]) = super::annotate::arrow_head(&d.points, d.width) {
                        let base = ((w1.0 + w2.0) / 2., (w1.1 + w2.1) / 2.);
                        if let Some(path) = stroke_path(&[d.points[0], base], d.width) {
                            window.paint_path(path, color);
                        }
                        if let Some(path) = fill_path(&[tip, w1, w2]) {
                            window.paint_path(path, color);
                        }
                    } else if let Some(path) = dot_path(d.points[0], d.width) {
                        window.paint_path(path, color);
                    }
                }
                ShapeKind::Pencil | ShapeKind::Highlighter => {
                    if d.points.len() < 2 {
                        if let Some(p) = d.points.first() {
                            if let Some(path) = dot_path(*p, d.width) {
                                window.paint_path(path, color);
                            }
                        }
                    } else if let Some(path) = stroke_path(&d.points, d.width) {
                        window.paint_path(path, color);
                    }
                }
                ShapeKind::Mosaic => {
                    // 拖拽中只画范围框 + 半透明底（真马赛克落盘才烤，要源像素）。
                    let b = d.bounds;
                    if b.w <= 0. || b.h <= 0. {
                        return;
                    }
                    let corners = [
                        (b.x, b.y),
                        (b.x + b.w, b.y),
                        (b.x + b.w, b.y + b.h),
                        (b.x, b.y + b.h),
                    ];
                    if let Some(path) = fill_path(&corners) {
                        window.paint_path(path, white().opacity(0.15));
                    }
                    let mut it = corners.iter();
                    let &(x0, y0) = it.next().expect("4 个角，非空");
                    let mut pb = PathBuilder::stroke(px(2.));
                    pb.move_to(point(px(x0), px(y0)));
                    for &(x, y) in it {
                        pb.line_to(point(px(x), px(y)));
                    }
                    pb.close();
                    if let Ok(path) = pb.build() {
                        window.paint_path(path, accent);
                    }
                }
                ShapeKind::Text | ShapeKind::Eraser => {}
            }
        },
    )
    .absolute()
    .size_full()
}

/// 看不见的事件网：paint 里挂窗口级鼠标监听，一帧挂一次。
/// shotori 同款做法——元素级 handler 在拖出界时会丢事件，窗口级不会。
/// 路由：输入中点别处先提交文字；工具条归按钮；标注工具点选区内归标注；
/// 文字工具点选区内弹输入框；否则归框选。
fn pointer_sink(input: WeakEntity<SelectOverlay>) -> impl IntoElement {
    canvas(
        |_, _, _| (),
        move |_, (), window, _| {
            let view = input.clone();
            window.on_mouse_event(move |event: &MouseDownEvent, phase, window, cx| {
                if phase != DispatchPhase::Bubble || event.button != MouseButton::Left {
                    return;
                }
                let _ = view.update(cx, |this, cx| {
                    // 输入中点别处：按提交算（别丢字），这次点击不再干别的。
                    if this.text_editor.is_some() {
                        if this.commit_text_editor() {
                            this.refresh_committed();
                        }
                        this.refresh_draft();
                        cx.notify();
                        return;
                    }
                    // 工具条的按下归按钮，不管框选/标注。
                    if this.in_toolbar(event.position) {
                        return;
                    }
                    let pos = (event.position.x.as_f32(), event.position.y.as_f32());
                    if let Some(sel) = this.selection() {
                        if this.ann.paint_enabled() {
                            this.ann.begin(pos, sel);
                            this.ann_drag = true;
                            this.refresh_draft();
                            cx.notify();
                            return;
                        }
                        if this.ann.tool() == Some(super::annotate::ShapeKind::Text) {
                            this.open_text_editor(event.position, sel, window, cx);
                            cx.notify();
                            return;
                        }
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
                    if this.ann_drag {
                        // 松手事件丢了（比如松在窗口外面）：在途笔画提交，
                        // 不然一直卡在绘制态，第二排也不出来。
                        if !event.dragging() {
                            this.ann.end();
                            this.ann_drag = false;
                            this.refresh_committed();
                            this.refresh_draft();
                        } else if let Some(sel) = this.selection() {
                            let pos = (event.position.x.as_f32(), event.position.y.as_f32());
                            if this.ann.drag_to(pos, sel) {
                                this.refresh_draft();
                            }
                        }
                        cx.notify();
                        return;
                    }
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
                    if this.ann_drag {
                        this.ann.end();
                        this.ann_drag = false;
                        this.refresh_committed();
                        this.refresh_draft();
                        cx.notify();
                    } else if this.dragging {
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
