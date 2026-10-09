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
