//! 设置页视图：“语言”和“主题”两组，一行一点即切。
//! 持久化在 `super`（`mod.rs`）；这里只做组装。

use gpui_kit::assets::IconName;
use gpui_kit::component::{ActiveTheme, Icon};
use gpui_kit::*;

use crate::i18n::{LANGUAGES, Language, t};
use crate::shell::{TEXT_MD, TEXT_SM, WsShell};

impl WsShell {
    /// 设置页：“语言”和“主题”两组，一行一点即切。
    pub(crate) fn render_settings(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_1()
            .flex_col()
            .px(px(16.))
            .py(px(12.))
            .gap(px(16.))
            // “语言”分组
            .child(self.settings_section(
                // “语言”
                t("app.settings_language"),
                // “立即生效，标题栏下次启动更新”
                t("app.settings_language_hint"),
                self.language_rows(cx),
                cx,
            ))
            // “主题”分组
            .child(self.settings_section(
                // “主题”
                t("app.settings_theme"),
                // “点击切换，立即生效”
                t("app.settings_theme_hint"),
                self.theme_rows(cx),
                cx,
            ))
            // “快捷键”分组
            .child(self.settings_section(
                // “呼出快捷键”
                t("app.settings_hotkey"),
                // “全局生效，点修改后直接按下组合键”
                t("app.settings_hotkey_hint"),
                self.hotkey_rows(cx),
                cx,
            ))
    }

    /// 设置页的一组：标题 + 副提示 + 若干选项行。
    /// 文案走 `SharedString`：借来的 `&str` 在元素树里活不够长。
    fn settings_section(
        &self,
        title: impl Into<SharedString>,
        hint: impl Into<SharedString>,
        rows: Vec<AnyElement>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let fg = cx.theme().foreground;
        let muted = cx.theme().muted_foreground;
        let title: SharedString = title.into();
        let hint: SharedString = hint.into();
        div()
            .flex()
            .flex_none()
            .flex_col()
            .gap(px(6.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(div().text_size(px(TEXT_MD)).text_color(fg).child(title))
                    .child(div().text_size(px(TEXT_SM)).text_color(muted).child(hint)),
            )
            .child(div().flex().flex_col().gap(px(2.)).children(rows))
    }

    /// 设置页通用选项行：选中高亮，悬停 wash，右侧对勾标出当前项。
    /// 标签走 `SharedString`：借来的 `&str` 在元素树里活不够长。
    fn option_row(
        label: impl Into<SharedString>,
        selected: bool,
        row_id: ElementId,
        on_click: impl Fn(&mut WsShell, &ClickEvent, &mut Window, &mut Context<WsShell>) + 'static,
        cx: &mut Context<WsShell>,
    ) -> AnyElement {
        let row_bg = cx.theme().sidebar_accent;
        let row_fg = cx.theme().sidebar_accent_foreground;
        let idle_fg = cx.theme().sidebar_foreground;
        let accent = cx.theme().link;
        let label: SharedString = label.into();
        let (bg, fg) = if selected {
            (row_bg, row_fg)
        } else {
            (hsla(0., 0., 0., 0.), idle_fg)
        };
        div()
            .id(row_id)
            .flex()
            .flex_none()
            .items_center()
            .gap(px(8.))
            .px(px(10.))
            .h(px(32.))
            .rounded(px(6.))
            .bg(bg)
            .text_color(fg)
            .cursor_pointer()
            .hover(move |s| {
                if selected {
                    s
                } else {
                    s.bg(row_bg)
                }
            })
            .on_click(cx.listener(on_click))
            .child(div().flex_1().text_size(px(TEXT_MD)).child(label))
            .children(selected.then(|| Icon::new(IconName::Check).size(px(13.)).text_color(accent)))
            .into_any_element()
    }

    /// “语言”分组：每种语言一行（中文 / English），点即切换、即时重绘。
    /// 行标签用各语言的自称；加新语言时 match 穷举会逼着补标签。
    fn language_rows(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        LANGUAGES
            .iter()
            .map(|lang| {
                let lang = *lang;
                let label = match lang {
                    // “中文”
                    Language::Chinese => t("app.settings_lang_zh"),
                    // “English”
                    Language::English => t("app.settings_lang_en"),
                };
                let selected = crate::i18n::language() == lang;
                Self::option_row(
                    label,
                    selected,
                    ElementId::named_usize("lang", lang as usize),
                    move |_, _, _, cx| {
                        crate::i18n::set_language(lang);
                        cx.notify();
                    },
                    cx,
                )
            })
            .collect()
    }

    /// “快捷键”分组：当前组合 + 修改/恢复默认，失败原因另起一行。
    /// 录制时整行挂 `capture_key_down`（按钮点完焦点还在行内，按键冒泡
    /// 前先到）；切走页面录制态由 `select_tool` 顺手清掉。
    fn hotkey_rows(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        use gpui_kit::component::button::Button;
        use gpui_kit::component::Sizable as _;

        let fg = cx.theme().foreground;
        let red = cx.theme().red;
        let mut row = div()
            .id("hotkey-row")
            .flex()
            .flex_none()
            .items_center()
            .gap(px(8.))
            .px(px(10.))
            .h(px(32.))
            .rounded(px(6.));
        if self.spotlight.recording {
            // 录制态：提示语 + 按键捕获（`WeakEntity::update` 写回主窗口，
            // overlay 同款）。
            let weak = cx.entity().downgrade();
            row = row
                .capture_key_down(move |event: &KeyDownEvent, _: &mut Window, cx: &mut App| {
                    let stroke = event.keystroke.clone();
                    let _ = weak.update(cx, |this, cx| {
                        this.record_hotkey(&stroke, cx);
                    });
                })
                .child(
                    div()
                        .flex_1()
                        .text_size(px(TEXT_MD))
                        .text_color(fg)
                        // “按下快捷键…，Esc 取消”
                        .child(t("app.hotkey_recording")),
                );
        } else {
            row = row
                .child(
                    div()
                        .flex_1()
                        .text_size(px(TEXT_MD))
                        .text_color(fg)
                        .child(crate::spotlight::display_hotkey(&self.spotlight.spec)),
                )
                .child(
                    Button::new("hotkey-change")
                        // “修改”
                        .label(t("app.hotkey_change"))
                        .small()
                        .outline()
                        .cursor_pointer()
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.spotlight.recording = true;
                            cx.notify();
                        })),
                )
                .child(
                    Button::new("hotkey-reset")
                        // “恢复默认”
                        .label(t("app.hotkey_reset"))
                        .small()
                        .outline()
                        .cursor_pointer()
                        .on_click(cx.listener(|this, _, _, cx| {
                            match this
                                .spotlight
                                .rebind_hotkey(crate::spotlight::DEFAULT_HOTKEY)
                            {
                                Ok(keymap) => {
                                    cx.bind_keys([KeyBinding::new(
                                        &keymap,
                                        crate::spotlight::ToggleSpotlight,
                                        None,
                                    )]);
                                }
                                Err(e) => this.spotlight.hotkey_error = Some(e),
                            }
                            cx.notify();
                        })),
                );
        }
        let mut rows = vec![row.into_any_element()];
        if let Some(error) = &self.spotlight.hotkey_error {
            rows.push(
                div()
                    .text_size(px(TEXT_SM))
                    .text_color(red)
                    .child(error.clone())
                    .into_any_element(),
            );
        }
        // 注册失败也不藏：用户切走再回来还能看到原因。
        rows
    }

    /// “主题”分组：全部内置主题，一行一个，中文名见 themes/wong.json。
    fn theme_rows(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        crate::theme::names()
            .into_iter()
            .enumerate()
            .map(|(ix, name)| {
                let selected = self.settings_theme == name;
                let label = name.clone();
                Self::option_row(
                    label,
                    selected,
                    ElementId::named_usize("theme", ix),
                    move |this, _, _, cx| {
                        // 换主题并重绘所有窗口；名字对不上时什么都不做。
                        if crate::theme::apply(cx, &name) {
                            this.settings_theme = name.clone();
                        }
                        cx.notify();
                    },
                    cx,
                )
            })
            .collect()
    }
}
