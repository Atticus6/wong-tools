//! 截图工具视图：全屏捕获页（预览 + 复制 / 保存 / 重截）。
//! 纯逻辑在 `super`（`mod.rs`），覆盖层在 `overlay`；这里只做组装和状态。

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{ActiveTheme, Icon, Sizable as _};
use gpui_kit::*;

use crate::i18n::t;
use crate::shell::{TEXT_SM, WsShell};

/// 截图工具的状态：原图留内存（复制/保存用），预览是缩略 PNG（进界面用），
/// status 是操作反馈（一行小字，成功失败都写）。
pub(crate) struct ShotState {
    image: Option<image::RgbaImage>,
    preview: Option<Vec<u8>>,
    preview_id: u64,
    status: Option<String>,
}

/// 预览图 ID 计数器：GPUI 按 id 缓存解码后的图，每次截取必须换新 id，
// 不然缓存里还是上一张。
static NEXT_PREVIEW_ID: AtomicU64 = AtomicU64::new(1);

impl ShotState {
    pub(crate) fn empty() -> Self {
        Self {
            image: None,
            preview: None,
            preview_id: 0,
            status: None,
        }
    }

    /// 全屏截取：原图 + 预览一起换；失败只写 status，不动旧图。
    fn capture(&mut self) {
        let image = match crate::tools::screenshot::capture_primary() {
            Ok(image) => image,
            Err(e) => {
                self.status = Some(e);
                return;
            }
        };
        match crate::tools::screenshot::thumbnail_png(&image) {
            Ok(png) => {
                self.preview_id = NEXT_PREVIEW_ID.fetch_add(1, Ordering::Relaxed);
                self.image = Some(image);
                self.preview = Some(png);
                self.status = None;
            }
            Err(e) => self.status = Some(e),
        }
    }
}

impl WsShell {
    /// 覆盖层确定后写回裁剪结果：原图 + 预览 + 新 id，状态清空。
    pub(crate) fn set_shot(&mut self, image: image::RgbaImage, preview: Vec<u8>, id: u64) {
        self.shot.image = Some(image);
        self.shot.preview = Some(preview);
        self.shot.preview_id = id;
        self.shot.status = None;
    }

    /// 打开区域选择覆盖层。覆盖层自己截屏冻结，失败把错误写进状态行。
    fn open_region(&mut self, cx: &mut Context<Self>) {
        let shell = cx.entity().downgrade();
        if let Err(e) = crate::tools::screenshot::overlay::SelectOverlay::open(shell, cx) {
            self.shot.status = Some(e);
            cx.notify();
        }
    }

    /// 截图工具页：没截过显示大按钮，截过显示预览 + 操作行 + 状态。
    pub(crate) fn render_shot(&self, cx: &mut Context<Self>) -> impl IntoElement {
        match self.shot.preview.clone() {
            Some(png) => self.render_shot_result(png, cx).into_any_element(),
            None => self.render_shot_empty(cx).into_any_element(),
        }
    }

    /// 还没截过：大按钮 + 说明。
    fn render_shot_empty(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        div()
            .flex()
            .flex_1()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(px(10.))
            .child(Icon::new(IconName::Camera).size(px(28.)).text_color(muted))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(
                        Button::new("shot-capture")
                            // “截取全屏”
                            .label(t("app.shot_capture"))
                            .primary()
                            .cursor_pointer()
                            .on_click(cx.listener(|this, _, _, cx| {
                                // GDI 全屏捕获几百毫秒内完成，直接跑主线程；
                                // 以后做区域选择再挪后台。
                                this.shot.capture();
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("shot-region")
                            // “区域选择”
                            .label(t("app.shot_region"))
                            .outline()
                            .cursor_pointer()
                            .on_click(cx.listener(|this, _, _, cx| this.open_region(cx))),
                    ),
            )
            .child(
                div()
                    .text_size(px(TEXT_SM))
                    .text_color(muted)
                    // “截取主显示器全屏，区域选择下一步再做”
                    .child(t("app.shot_capture_hint")),
            )
            .children(self.shot.status.clone().map(|s| {
                div().text_size(px(TEXT_SM)).text_color(muted).child(s)
            }))
    }

    /// 截过了：预览图 + 复制 / 保存 / 重截 + 状态行。
    fn render_shot_result(&self, png: Vec<u8>, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let border = cx.theme().border;
        div()
            .flex()
            .flex_1()
            .flex_col()
            .items_center()
            .justify_center()
            .px(px(16.))
            .py(px(12.))
            .gap(px(8.))
            .child(
                div()
                    .border_1()
                    .border_color(border)
                    .rounded(px(8.))
                    .child(
                        img(ImageSource::Image(Arc::new(Image {
                            format: ImageFormat::Png,
                            bytes: png,
                            id: self.shot.preview_id,
                        })))
                        .max_w(px(640.))
                        .max_h(px(400.)),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(
                        Button::new("shot-copy")
                            // “复制”
                            .label(t("app.shot_copy"))
                            .small()
                            .outline()
                            .cursor_pointer()
                            .on_click(cx.listener(|this, _, _, cx| {
                                if let Some(img) = this.shot.image.clone() {
                                    // “已复制到剪贴板”
                                    let done = t("app.shot_copied");
                                    this.shot.status = Some(
                                        crate::tools::screenshot::copy_image(&img)
                                            .map(|_| done)
                                            .unwrap_or_else(|e| e),
                                    );
                                    cx.notify();
                                }
                            })),
                    )
                    .child(
                        Button::new("shot-save")
                            // “保存”
                            .label(t("app.shot_save"))
                            .small()
                            .outline()
                            .cursor_pointer()
                            .on_click(cx.listener(|this, _, _, cx| {
                                if let Some(img) = this.shot.image.clone() {
                                    let path = crate::tools::screenshot::pictures_dir()
                                        .join(crate::tools::screenshot::timestamped_name());
                                    this.shot.status = Some(
                                        match crate::tools::screenshot::save_png(&img, &path) {
                                            // “已保存到”
                                            Ok(()) => format!(
                                                "{}：{}",
                                                t("app.shot_saved"),
                                                path.display()
                                            ),
                                            Err(e) => e,
                                        },
                                    );
                                    cx.notify();
                                }
                            })),
                    )
                    .child(
                        Button::new("shot-retake")
                            // “重新截取”
                            .label(t("app.shot_retake"))
                            .small()
                            .outline()
                            .cursor_pointer()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.shot.capture();
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("shot-reregion")
                            // “区域选择”
                            .label(t("app.shot_region"))
                            .small()
                            .outline()
                            .cursor_pointer()
                            .on_click(cx.listener(|this, _, _, cx| this.open_region(cx))),
                    ),
            )
            .children(self.shot.status.clone().map(|s| {
                div().text_size(px(TEXT_SM)).text_color(muted).child(s)
            }))
    }
}
