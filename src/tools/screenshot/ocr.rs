//! 截图 OCR：把选区里的文字认出来，可复制。
//!
//! 引擎走系统原生（`native-ocr`：macOS Vision，Windows WinRT OCR），中英文
//! 跟系统走，不打包模型、不调云。Linux 暂无引擎，`is_available` 为 false，
//! 调用方把入口藏掉即可。识别是阻塞调用（Vision 单张约 1 秒），调用方必须
//! 扔后台线程（`background_executor().spawn`），别卡界面。
//!
//! 文件分两截：上半是纯逻辑（限幅/规整/识别/复制文本，可单测），下半是
//! `OcrWindow` 结果浮窗（和钉屏同款 PopUp，读文 + 再复制）。

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::{ActiveTheme, Sizable as _};
use gpui_kit::*;

use crate::i18n::t;

/// 空图直接拒掉，不进引擎（各平台行为一致，单测不依赖系统 OCR）。
const MIN_SIDE: u32 = 1;

/// 本机有没有 OCR 引擎（Windows/macOS 有，Linux 没有）。
pub fn is_available() -> bool {
    native_ocr::is_available()
}

/// 引擎接受的最大边（0 表示没引擎，调用方先看 `is_available`）。
pub fn max_side() -> u32 {
    native_ocr::max_image_dimension()
}

/// 只缩不放：最长边压进 `limit` 内等比换算，小图原样返回。
/// `limit` 为 0（没引擎）时原样返回，免得除零。
pub fn fit_down(w: u32, h: u32, limit: u32) -> (u32, u32) {
    if w == 0 || h == 0 || limit == 0 {
        return (w.max(1), h.max(1));
    }
    let longest = w.max(h);
    if longest <= limit {
        return (w, h);
    }
    let scale = limit as f64 / longest as f64;
    (
        ((w as f64 * scale).round() as u32).max(1),
        ((h as f64 * scale).round() as u32).max(1),
    )
}

/// 超限就等比缩小（大屏整屏识别才走得到，选区一般用不上）。
/// 小图不动：放大只会糊，Vision 不需要。
pub fn prepare(img: &image::RgbaImage) -> image::RgbaImage {
    let (w, h) = (img.width(), img.height());
    let (tw, th) = fit_down(w, h, max_side());
    if (tw, th) == (w, h) {
        return img.clone();
    }
    image::imageops::resize(img, tw, th, image::imageops::FilterType::Triangle)
}

/// 引擎吐的行拼起来：去首尾空、扔空行，剩不下东西算没认出来。
/// 纯函数，可测（Vision 行尾常带空格，不清展示时尾巴参差不齐）。
pub fn normalize(raw: &str) -> String {
    raw.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// 认一张 RGBA 图，返回规整后的文本。空图和白板都算失败（`Err` 里是中文，
/// 调用方直接展示）。阻塞调用，必须放后台线程。
pub fn recognize(img: &image::RgbaImage) -> Result<String, String> {
    if img.width() < MIN_SIDE || img.height() < MIN_SIDE {
        return Err(t("app.shot_ocr_empty"));
    }
    if !is_available() {
        return Err(t("app.shot_ocr_unavailable"));
    }
    let img = prepare(img);
    let raw = native_ocr::recognize_rgba(img.as_raw(), img.width(), img.height()).map_err(|e| {
        use native_ocr::Error::*;
        match e {
            // Windows 用户语言没有 OCR 包最常见，单独给一句能照着修的。
            NoLanguage => t("app.shot_ocr_nolanguage"),
            Unavailable => t("app.shot_ocr_unavailable"),
            Failed(detail) => detail,
            InvalidImage(detail) => detail,
        }
    })?;
    let text = normalize(&raw);
    if text.is_empty() {
        // “没识别到文字”
        return Err(t("app.shot_ocr_empty"));
    }
    Ok(text)
}

/// 文本进系统剪贴板（识别成功顺手调一次，结果窗里的复制按钮也走它）。
pub fn copy_text(text: &str) -> Result<(), String> {
    let mut clipboard =
        arboard::Clipboard::new().map_err(|e| format!("打开剪贴板失败: {e}"))?;
    clipboard
        .set_text(text.to_string())
        .map_err(|e| format!("写入剪贴板失败: {e}"))?;
    Ok(())
}

/// 识别结果浮窗：只读文本 + 复制/关闭。
///
/// 钉屏同款 PopUp 置顶浮窗（对照着原文看，复制完再关）。文本不可选中是
/// GPUI 的限制（`div` 文本没有选区能力），v1 靠复制按钮拿走全文。
pub(crate) struct OcrWindow {
    /// 识别文本（构造时定死，不可编辑）。
    text: String,
    /// 刚复制成功：按钮旁给一行反馈，和钉屏的 `flash` 同款思路。
    copied: bool,
    /// 新窗标准配置：以后加 Esc 关窗直接能用。
    focus: FocusHandle,
}

impl OcrWindow {
    /// 打开结果窗。`text` 是规整过的非空文本，调用方保证。
    pub(crate) fn open(text: String, cx: &mut App) -> Result<(), String> {
        // 480x360：窄栏文章够读，又不至于盖住选区原文。
        const WIN_W: f32 = 480.0;
        const WIN_H: f32 = 360.0;
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                point(px(120.), px(120.)),
                size(px(WIN_W), px(WIN_H)),
            ))),
            titlebar: Some(TitlebarOptions {
                // “识别结果”
                title: Some(t("app.shot_ocr_title").into()),
                appears_transparent: true,
                // macOS 红绿灯挪出窗外：要的是无边框文本浮窗，框架没给
                // 隐藏开关，位置法是目前唯一的办法（同覆盖层/钉屏）。
                traffic_light_position: if cfg!(target_os = "macos") {
                    Some(point(px(-100.), px(-100.)))
                } else {
                    None
                },
            }),
            kind: WindowKind::PopUp,
            focus: true,
            show: true,
            is_movable: true,
            is_resizable: false,
            ..WindowOptions::default()
        };
        open_window(options, cx, |window, cx| {
            cx.new(|cx| Self::new(text, window, cx))
        })
        .map(|_| ())
        .map_err(|e| format!("打不开识别结果窗口: {e}"))
    }

    fn new(text: String, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        Self {
            text,
            copied: false,
            focus,
        }
    }

    /// 再复制一次全文（打开时已经自动复制过一次，这里是兜底）。
    fn copy(&mut self, cx: &mut Context<Self>) {
        // 复制失败不关窗：文本还在，用户能重来。
        if copy_text(&self.text).is_ok() {
            self.copied = true;
            cx.notify();
        }
    }

    fn close(&mut self, window: &mut Window) {
        window.remove_window();
    }
}

impl Render for OcrWindow {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let fg = cx.theme().foreground;
        let muted = cx.theme().muted_foreground;
        let border = cx.theme().border;
        // 浮窗底走 popover（和覆盖层工具条同一块色，边框外再描一圈）。
        let card = cx.theme().popover;
        // “共 {} 字”
        let count = t("app.shot_ocr_count")
            .replace("{}", &self.text.chars().count().to_string());
        let body: SharedString = self.text.clone().into();
        div()
            .id("ocr-window")
            .size_full()
            .flex()
            .flex_col()
            .bg(card)
            .rounded(px(10.))
            .border_1()
            .border_color(border)
            .track_focus(&self.focus)
            // 头：标题 + 字数。
            .child(
                div()
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_between()
                    .px(px(12.))
                    .pt(px(10.))
                    .pb(px(6.))
                    .child(
                        div()
                            .text_size(px(13.))
                            .text_color(fg)
                            // “识别结果”
                            .child(t("app.shot_ocr_title")),
                    )
                    .child(div().text_size(px(12.)).text_color(muted).child(count)),
            )
            // 正文：纵滚只读，复制走底下按钮。
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .mx(px(12.))
                    .p(px(10.))
                    .rounded(px(8.))
                    .border_1()
                    .border_color(border)
                    .overflow_y_scrollbar()
                    .text_size(px(13.))
                    .text_color(fg)
                    .child(body),
            )
            // 底：复制状态 + 复制/关闭。
            .child(
                div()
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_between()
                    .px(px(12.))
                    .py(px(10.))
                    .child(
                        div().text_size(px(12.)).text_color(muted).child(
                            if self.copied {
                                // “已识别并复制”
                                t("app.shot_ocr_copied")
                            } else {
                                String::new()
                            },
                        ),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .child(
                                Button::new("ocr-copy")
                                    // “复制”
                                    .label(t("app.shot_copy"))
                                    .small()
                                    .primary()
                                    .cursor_pointer()
                                    .icon(IconName::Copy)
                                    .on_click(cx.listener(|this, _, _, cx| this.copy(cx))),
                            )
                            .child(
                                Button::new("ocr-close")
                                    // “关闭”
                                    .label(t("app.shot_pin_close"))
                                    .small()
                                    .outline()
                                    .cursor_pointer()
                                    .icon(IconName::X)
                                    .on_click(cx.listener(|this, _, window, _| this.close(window))),
                            ),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    // 注意：这里不要 `use super::*`——它会把 gpui 自带的 `#[test]` 宏引进
    // 来，盖住 Rust 内置的 test 属性导致递归展开失败。只显式引入用到的项。
    use super::fit_down;
    use super::is_available;
    use super::normalize;
    use super::recognize;

    #[test]
    fn fit_down_only_shrinks_oversized_images() {
        // 小图原样：放大只会糊，引擎不需要。
        assert_eq!(fit_down(800, 600, 4000), (800, 600));
        // 压线的不动。
        assert_eq!(fit_down(4000, 3000, 4000), (4000, 3000));
        // 超的按最长边等比压。
        assert_eq!(fit_down(8000, 6000, 4000), (4000, 3000));
        assert_eq!(fit_down(6000, 8000, 4000), (3000, 4000));
        // 没引擎（limit 0）原样返回，不除零。
        assert_eq!(fit_down(100, 100, 0), (100, 100));
        // 零尺寸给 1 占位，调用方不会拿零尺寸进来。
        assert_eq!(fit_down(0, 100, 4000), (1, 100));
    }

    #[test]
    fn normalize_trims_and_drops_blank_lines() {
        // 行尾空格和空行是 Vision 的常见输出，展示前清掉。
        assert_eq!(normalize("  hello  \n\n  world \n   \n"), "hello\nworld");
        assert_eq!(normalize("   \n  "), "");
        assert_eq!(normalize("a\nb"), "a\nb");
    }

    #[test]
    fn is_available_matches_platform_engine() {
        // 原生引擎只在 Windows/macOS 有，Linux 恒为 false。
        assert_eq!(
            is_available(),
            cfg!(any(windows, target_os = "macos")),
            "引擎可用性必须跟平台走"
        );
    }

    #[test]
    fn recognize_rejects_empty_image_without_engine() {
        // 零尺寸在进引擎前就拒了：Linux 没引擎也走同一条路，可测。
        let img = image::RgbaImage::new(0, 0);
        assert!(
            recognize(&img).is_err(),
            "空图必须直接失败，不能进引擎"
        );
    }
}
