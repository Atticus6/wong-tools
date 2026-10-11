//! 截图工具视图：历史 / 设置双 tab。
//! 历史：每次落盘一张（50 封顶）+ 瀑布流缩略图，点开展示；
//! 设置：截图全局键录制（存档 + 热重绑 + 托盘菜单后缀刷新）。
//! 纯逻辑在 `super`（`mod.rs`）和 `history`，覆盖层在 `overlay`；这里只做组装和状态。

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use gpui_kit::assets::IconName;
use gpui_kit::base::Selectable as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::{ActiveTheme, Icon, Sizable as _};
use gpui_kit::*;

use crate::i18n::t;
use crate::shell::{TEXT_MD, TEXT_SM, WsShell, grid_layout};

use super::history::{ShotRecord, distribute};

/// 截图页 tab：历史优先，设置改键。
#[derive(Clone, Copy, PartialEq, Eq)]
enum ShotTab {
    History,
    Settings,
}

/// 截图工具的状态：原图留内存（复制/保存用），预览是缩略 PNG（进界面用），
/// status 是操作反馈（一行小字，成功失败都写）。历史缩略图常驻内存
/// （50 张 480px，几 MB），点开展示才读原图。
pub(crate) struct ShotState {
    image: Option<image::RgbaImage>,
    preview: Option<Vec<u8>>,
    preview_id: u64,
    status: Option<String>,
    tab: ShotTab,
    history: Vec<ShotRecord>,
    thumbs: HashMap<u64, Vec<u8>>,
    hotkey_spec: String,
    pub(crate) recording: bool,
    hotkey_error: Option<String>,
}

/// 预览图 ID 计数器：GPUI 按 id 缓存解码后的图，每次截取必须换新 id，
// 不然缓存里还是上一张。
static NEXT_PREVIEW_ID: AtomicU64 = AtomicU64::new(1);

impl ShotState {
    pub(crate) fn empty() -> Self {
        // 历史和键读库：坏了回空表/默认键，不崩（启动不能倒在这里）。
        let history = crate::settings::shot_history();
        let mut thumbs = HashMap::new();
        for rec in &history {
            if let Ok(bytes) = super::history::read_thumb(rec) {
                thumbs.insert(rec.id, bytes);
            }
        }
        Self {
            image: None,
            preview: None,
            preview_id: 0,
            status: None,
            tab: ShotTab::History,
            history,
            thumbs,
            hotkey_spec: crate::settings::shot_hotkey()
                .unwrap_or_else(|| crate::tray::SHOT_HOTKEY.to_string()),
            recording: false,
            hotkey_error: None,
        }
    }

    /// 全屏截取：原图 + 预览一起换；失败只写 status，不动旧图。成功进历史。
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
                self.push_history(&image);
                self.preview_id = NEXT_PREVIEW_ID.fetch_add(1, Ordering::Relaxed);
                self.image = Some(image);
                self.preview = Some(png);
                self.status = None;
            }
            Err(e) => self.status = Some(e),
        }
    }

    /// 落一条历史：落盘失败记日志，不污染用户状态行。
    fn push_history(&mut self, image: &image::RgbaImage) {
        match super::history::save_capture(image) {
            Ok(rec) => {
                if let Ok(bytes) = super::history::read_thumb(&rec) {
                    self.thumbs.insert(rec.id, bytes);
                }
                super::history::push_record(rec);
                self.history = crate::settings::shot_history();
            }
            Err(e) => eprintln!("截图历史落盘失败: {e}"),
        }
    }
}

impl WsShell {
    /// 覆盖层确定后写回裁剪结果：原图 + 预览 + 新 id，状态清空，进历史。
    pub(crate) fn set_shot(&mut self, image: image::RgbaImage, preview: Vec<u8>, id: u64) {
        self.push_shot_history(&image);
        self.shot.image = Some(image);
        self.shot.preview = Some(preview);
        self.shot.preview_id = id;
        self.shot.status = None;
    }

    /// 经 WsShell 落历史（`set_shot` 是它的方法，存取走它）。
    fn push_shot_history(&mut self, image: &image::RgbaImage) {
        self.shot.push_history(image);
    }

    /// 打开区域选择覆盖层。覆盖层自己截屏冻结，失败把错误写进状态行。
    pub(crate) fn open_region(&mut self, cx: &mut Context<Self>) {
        let shell = cx.entity().downgrade();
        if let Err(e) = crate::tools::screenshot::overlay::SelectOverlay::open(shell, cx) {
            self.shot.status = Some(e);
            cx.notify();
        }
    }

    /// 点开一条历史：原图读回展示（复制/保存行照常可用）。
    fn open_history_item(&mut self, id: u64) {
        let Some(rec) = self.shot.history.iter().find(|r| r.id == id).cloned() else {
            return;
        };
        match super::history::read_full(&rec) {
            Ok(image) => match crate::tools::screenshot::thumbnail_png(&image) {
                Ok(png) => {
                    self.shot.preview_id = NEXT_PREVIEW_ID.fetch_add(1, Ordering::Relaxed);
                    self.shot.image = Some(image);
                    self.shot.preview = Some(png);
                    self.shot.status = None;
                }
                Err(e) => self.shot.status = Some(e),
            },
            Err(e) => self.shot.status = Some(e),
        }
    }

    /// 删一条历史：出库 + 删文件 + 内存缩略图一起扔。
    fn delete_history_item(&mut self, id: u64) {
        super::history::delete_record(id);
        self.shot.thumbs.remove(&id);
        self.shot.history = crate::settings::shot_history();
    }

    /// 截图设置 tab 录制按键：和设置页聚光键同一套（`format_recorded`
    /// 拼串 + 全局句柄重绑），只是入库和菜单刷新走截图自己的。
    /// 按键由 `shell` 的 `intercept_keystrokes` 拦截器送来（不受焦点影响，
    /// 见 `WsShell::new` 的说明），本函数只管判组合 + 落定。
    pub(crate) fn record_shot_hotkey(&mut self, stroke: &Keystroke, cx: &mut Context<Self>) {
        use crate::spotlight::{Recorded, format_recorded};
        match format_recorded(stroke) {
            // Esc：退出录制，保持原快捷键。
            Recorded::Cancel => {
                self.shot.recording = false;
                cx.notify();
            }
            // 纯修饰键/裸键：继续等。
            Recorded::Ignore => {}
            // 有效组合：落定走恢复默认同一条路（重绑 + 入库 + 菜单后缀刷新）。
            Recorded::Spec(spec) => self.apply_shot_hotkey(spec, cx),
        }
    }

    /// 截图工具页：历史 / 设置双 tab。
    pub(crate) fn render_shot(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
        sidebar_w: f32,
    ) -> impl IntoElement {
        div()
            .flex()
            .flex_1()
            .flex_col()
            .min_h_0()
            .child(self.render_shot_tabs(cx))
            .children(match self.shot.tab {
                ShotTab::History => Some(self.render_shot_history(window, cx, sidebar_w)),
                ShotTab::Settings => None,
            })
            .children(match self.shot.tab {
                ShotTab::Settings => Some(self.render_shot_settings(cx)),
                ShotTab::History => None,
            })
    }

    /// 顶部 tab 行：历史 / 设置，选中态走 selected 样式。
    fn render_shot_tabs(&self, cx: &mut Context<Self>) -> impl IntoElement {
        // “历史”
        let history_label = t("app.shot_tab_history");
        // “设置”
        let settings_label = t("app.shot_tab_settings");
        let history_active = self.shot.tab == ShotTab::History;
        let settings_active = self.shot.tab == ShotTab::Settings;
        div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(4.))
            .px(px(16.))
            .pt(px(12.))
            .child(
                Button::new("shot-tab-history")
                    .ghost()
                    .label(history_label)
                    .small()
                    .cursor_pointer()
                    .selected(history_active)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.shot.tab = ShotTab::History;
                        this.shot.recording = false;
                        cx.notify();
                    })),
            )
            .child(
                Button::new("shot-tab-settings")
                    .ghost()
                    .label(settings_label)
                    .small()
                    .cursor_pointer()
                    .selected(settings_active)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.shot.tab = ShotTab::Settings;
                        this.shot.recording = false;
                        cx.notify();
                    })),
            )
            .into_any_element()
    }

    /// 历史 tab：当前结果（或截取入口）+ 瀑布流。
    fn render_shot_history(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
        sidebar_w: f32,
    ) -> AnyElement {
        let current = match self.shot.preview.clone() {
            Some(png) => self.render_shot_result(png, cx).into_any_element(),
            None => self.render_shot_empty(cx).into_any_element(),
        };
        div()
            .flex()
            .flex_1()
            .flex_col()
            .min_h_0()
            .child(current)
            .child(self.render_history_grid(window, cx, sidebar_w))
            .into_any_element()
    }

    /// 历史瀑布流：按列宽估高贪心分列，纵滚。缩略图读不到（外部删了）
    /// 整格跳过，不崩；点开展示，进复制保存行。
    fn render_history_grid(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
        sidebar_w: f32,
    ) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let border = cx.theme().border;
        if self.shot.history.is_empty() {
            return div()
                .flex_none()
                .px(px(16.))
                .py(px(8.))
                .text_size(px(TEXT_SM))
                .text_color(muted)
                // “还没有截图，截一张自动记在这里”
                .child(t("app.shot_history_empty"))
                .into_any_element();
        }
        let grid = grid_layout(window, sidebar_w);
        let cols = grid.cols.max(2) as usize;
        let col_w = grid.width / cols as f32;
        let heights: Vec<f32> = self
            .shot
            .history
            .iter()
            .map(|r| {
                if r.w > 0 {
                    col_w * r.h as f32 / r.w as f32
                } else {
                    col_w
                }
            })
            .collect();
        let columns = distribute(cols, &heights);
        div()
            .flex_1()
            .min_h_0()
            .overflow_y_scrollbar()
            .px(px(16.))
            .pb(px(12.))
            // “历史记录”
            .child(
                div()
                    .flex_none()
                    .pb(px(8.))
                    .text_size(px(TEXT_SM))
                    .text_color(muted)
                    .child(t("app.shot_history_title")),
            )
            .child(
                div()
                    .flex()
                    .w_full()
                    .gap(px(8.))
                    .children(columns.into_iter().map(|items| {
                        div()
                            .flex_1()
                            .flex_col()
                            .min_w_0()
                            .gap(px(8.))
                            .children(items.into_iter().filter_map(|ix| {
                                let rec = self.shot.history.get(ix)?.clone();
                                let bytes = self.shot.thumbs.get(&rec.id)?.clone();
                                let id = rec.id;
                                Some(
                                    div()
                                        .relative()
                                        .border_1()
                                        .border_color(border)
                                        .rounded(px(8.))
                                        .overflow_hidden()
                                        .child(
                                            Button::new(ElementId::named_usize("shot-hist", ix))
                                                .ghost()
                                                .p(px(0.))
                                                .cursor_pointer()
                                                .child(
                                                    img(ImageSource::Image(Arc::new(Image {
                                                        format: ImageFormat::Png,
                                                        bytes,
                                                        id,
                                                    })))
                                                    .w_full(),
                                                )
                                                .on_click(cx.listener(move |this, _, _, cx| {
                                                    this.open_history_item(id);
                                                    cx.notify();
                                                })),
                                        )
                                        .child(
                                            Button::new(ElementId::named_usize(
                                                "shot-hist-del",
                                                ix,
                                            ))
                                            .icon(IconName::X)
                                            .small()
                                            .cursor_pointer()
                                            // “删除”
                                            .tooltip(t("app.shot_delete"))
                                            .absolute()
                                            .top(px(4.))
                                            .right(px(4.))
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                this.delete_history_item(id);
                                                cx.notify();
                                            })),
                                        )
                                        .into_any_element(),
                                )
                            }))
                            .into_any_element()
                    }))
                    .into_any_element(),
            )
            .into_any_element()
    }

    /// 设置 tab：截图全局键卡片（版式和应用设置页同一套）。
    fn render_shot_settings(&self, cx: &mut Context<Self>) -> AnyElement {
        div()
            .flex()
            .flex_1()
            .flex_col()
            .min_h_0()
            .px(px(16.))
            .py(px(12.))
            .gap(px(16.))
            .child(self.settings_section(
                // “截图快捷键”
                t("app.shot_hotkey_title"),
                self.shot_hotkey_rows(cx),
                cx,
            ))
            .into_any_element()
    }

    /// 截图键行：当前组合 + 修改/恢复默认，失败原因另起一行。
    /// 整行常驻（录制态只换标题，不卸载按钮）。
    /// 按键不在这里挂：行级 `capture_key_down` 只在聚焦路径上跑，而“修改”
    /// 点了要 `blur()` 扔焦点，焦点一没行级监听就哑了（实测：怎么按都不录）。
    /// 真正的接键在 `shell` 的 `intercept_keystrokes`，和焦点无关。
    /// 切 tab/切工具顺手关录制（行没了就别再录了）。
    fn shot_hotkey_rows(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        use gpui_kit::component::button::Button;

        let fg = cx.theme().foreground;
        let muted = cx.theme().muted_foreground;
        let red = cx.theme().red;
        // 录制中标题显示提示，否则显示当前组合。
        let title = if self.shot.recording {
            // “按下快捷键…，Esc 取消”
            t("app.hotkey_recording")
        } else {
            crate::spotlight::display_hotkey(&self.shot.hotkey_spec)
        };
        let mut rows = vec![
            div()
                .id("shot-hotkey-row")
                .flex()
                .flex_none()
                .items_center()
                .justify_between()
                .gap(px(12.))
                .child(
                    div()
                        .flex_1()
                        .flex_col()
                        .gap(px(2.))
                        .child(div().text_size(px(TEXT_MD)).text_color(fg).child(title))
                        .child(
                            div()
                                .text_size(px(TEXT_SM))
                                .text_color(muted)
                                // “全局生效，点修改后直接按下组合键”
                                .child(t("app.settings_hotkey_hint")),
                        ),
                )
                .child(
                    div()
                        .flex_none()
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .child(
                            Button::new("shot-hotkey-change")
                                // “修改”（录制中再点 = 取消）
                                .label(t("app.hotkey_change"))
                                .small()
                                .outline()
                                .cursor_pointer()
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.shot.recording = !this.shot.recording;
                                    // 焦点扔掉：留在按钮上，回车/空格会误触重录。
                                    window.blur(cx);
                                    cx.notify();
                                })),
                        )
                        .child(
                            Button::new("shot-hotkey-reset")
                                // “恢复默认”
                                .label(t("app.hotkey_reset"))
                                .small()
                                .outline()
                                .cursor_pointer()
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.apply_shot_hotkey(crate::tray::SHOT_HOTKEY.to_string(), cx);
                                })),
                        ),
                )
                .into_any_element(),
        ];
        if let Some(error) = &self.shot.hotkey_error {
            rows.push(
                div()
                    .text_size(px(TEXT_SM))
                    .text_color(red)
                    .child(error.clone())
                    .into_any_element(),
            );
        }
        rows
    }

    /// 截图键落定：重绑 + 入库 + 菜单后缀刷新（三件必须一起成）。
    /// 修改/恢复默认共用；失败只写行内错误，保持录制态可重试。
    fn apply_shot_hotkey(&mut self, spec: String, cx: &mut Context<Self>) {
        match (|| {
            cx.global_mut::<crate::tray::ShotHotkey>().handle.rebind(&spec)?;
            crate::settings::set_shot_hotkey(&spec)?;
            Ok::<(), String>(())
        })() {
            Ok(()) => {
                crate::tray::update_shot_menu_label(&mut *cx, &spec);
                self.shot.hotkey_spec = spec;
                self.shot.hotkey_error = None;
                self.shot.recording = false;
                cx.notify();
            }
            Err(e) => {
                self.shot.hotkey_error = Some(e);
                cx.notify();
            }
        }
    }

    /// 还没截过：大按钮 + 说明。
    fn render_shot_empty(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        div()
            .flex()
            .flex_none()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(px(10.))
            .py(px(16.))
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
                                // GDI 全屏捕获几百毫秒内完成，直接跑主线程。
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
                    // “截取主显示器全屏”
                    .child(t("app.shot_capture_hint")),
            )
            .children(self.shot.status.clone().map(|s| {
                div().text_size(px(TEXT_SM)).text_color(muted).child(s)
            }))
    }

    /// 截过了：预览图 + 复制 / 保存 / 重截 + 区域 + 状态行。
    fn render_shot_result(&self, png: Vec<u8>, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let border = cx.theme().border;
        div()
            .flex()
            .flex_none()
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
