//! 设置页视图：描边卡片分组，一行左标题 + 说明、右控件（官方 Settings 版式）。
//! 持久化在 `super`（`mod.rs`）；这里只做组装。

use gpui_kit::component::ActiveTheme;
use gpui_kit::*;

use crate::i18n::t;
use crate::shell::{TEXT_MD, TEXT_SM, WsShell};

impl WsShell {
    /// 设置页：语言 / 主题 / 快捷键 / 更新四组卡片。
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
                self.language_rows(cx),
                cx,
            ))
            // “主题”分组
            .child(self.settings_section(
                // “主题”
                t("app.settings_theme"),
                self.theme_rows(cx),
                cx,
            ))
            // “快捷键”分组
            .child(self.settings_section(
                // “呼出快捷键”
                t("app.settings_hotkey"),
                self.hotkey_rows(cx),
                cx,
            ))
            // “更新”分组
            .child(self.settings_section(
                // “更新”
                t("app.update_title"),
                self.update_rows(cx),
                cx,
            ))
    }

    /// 设置页的一组：描边卡片 + 组名 + 若干行。
    /// 文案走 `SharedString`：借来的 `&str` 在元素树里活不够长。
    /// 截图工具的设置 tab 共用同一套版式（`pub(crate)` 特意开的口）。
    pub(crate) fn settings_section(
        &self,
        title: impl Into<SharedString>,
        rows: Vec<AnyElement>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let border = cx.theme().border;
        let muted = cx.theme().muted_foreground;
        let title: SharedString = title.into();
        div()
            .flex()
            .flex_none()
            .flex_col()
            .gap(px(10.))
            .border_1()
            .border_color(border)
            .rounded(px(8.))
            .p(px(12.))
            .child(div().text_size(px(TEXT_MD)).text_color(muted).child(title))
            .child(div().flex().flex_col().gap(px(10.)).children(rows))
    }

    /// 组里的一行：左标题 + 说明，右控件。
    /// 文案走 `SharedString`：借来的 `&str` 在元素树里活不够长。
    /// 同上，截图设置 tab 共用。
    pub(crate) fn setting_row(
        title: impl Into<SharedString>,
        desc: impl Into<SharedString>,
        control: AnyElement,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let fg = cx.theme().foreground;
        let muted = cx.theme().muted_foreground;
        let title: SharedString = title.into();
        let desc: SharedString = desc.into();
        div()
            .flex()
            .flex_none()
            .items_center()
            // 两端对齐：控件钉死右端。注意控件再包一层普通 div：
            // Combobox 做 flex 直接孩子时布局不正常（粘在文字后面），
            // 包一层后左侧 flex_1 才能把它顶到右端。
            .justify_between()
            .gap(px(12.))
            .child(
                div()
                    .flex_1()
                    .flex_col()
                    .gap(px(2.))
                    .child(div().text_size(px(TEXT_MD)).text_color(fg).child(title))
                    .child(div().text_size(px(TEXT_SM)).text_color(muted).child(desc)),
            )
            .child(div().flex_none().child(control))
            .into_any_element()
    }

    /// “语言”分组：一下拉框，选中即切；聚光面板改了语言，render 对账补回来。
    fn language_rows(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        use gpui_kit::component::Sizable as _;
        use gpui_kit::component::combobox::Combobox;

        vec![Self::setting_row(
            // “语言”
            t("app.settings_language"),
            // “立即生效，标题栏下次启动更新”
            t("app.settings_language_hint"),
            Combobox::new(&self.lang_combo)
                .small()
                .w(px(200.))
                // 菜单跟触发框等宽：默认菜单按内容撑满整行，太长。
                .menu_width(px(200.))
                .into_any_element(),
            cx,
        )]
    }

    /// “快捷键”分组：当前组合 + 修改/恢复默认，失败原因另起一行。
    /// 录制时整行换成提示语；按键不在这里挂 `capture_key_down`——那玩意
    /// 只跑在聚焦路径上，而点“修改”后按钮连同承载它的那一行都被换掉了，
    /// 焦点一丢路径就塌回根节点，怎么按都录不进。接键统一在 `shell` 的
    /// `intercept_keystrokes`（与截图键共用一个，见 `WsShell::new`）。
    /// 切走页面录制态由 `select_tool` 顺手清掉。
    fn hotkey_rows(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        use gpui_kit::component::button::Button;
        use gpui_kit::component::Sizable as _;

        let red = cx.theme().red;
        if self.spotlight.recording {
            // 录制态：提示语占整行，按键由全局拦截器接（见函数注释）。
            let row = div()
                .id("hotkey-row")
                .flex()
                .flex_none()
                .items_center()
                .h(px(32.))
                .child(
                    div()
                        .flex_1()
                        .text_size(px(TEXT_MD))
                        // “按下快捷键…，Esc 取消”
                        .child(t("app.hotkey_recording")),
                )
                .into_any_element();
            return vec![row];
        }
        let mut rows = vec![Self::setting_row(
            crate::spotlight::display_hotkey(&self.spotlight.spec),
            // “全局生效，点修改后直接按下组合键”
            t("app.settings_hotkey_hint"),
            div()
                .flex()
                .flex_none()
                .items_center()
                .gap(px(8.))
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
                            // 旧的应用内绑定写法，换键后要 Unbind 掉。
                            let old_keymap =
                                crate::spotlight::keymap_binding_string(&this.spotlight.spec)
                                    .unwrap_or_default();
                            match this
                                .spotlight
                                .rebind_hotkey(crate::spotlight::DEFAULT_HOTKEY)
                            {
                                Ok(_) => this.spotlight.rebind_app_keymap(&old_keymap, cx),
                                Err(e) => this.spotlight.hotkey_error = Some(e),
                            }
                            cx.notify();
                        })),
                )
                .into_any_element(),
            cx,
        )];
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

    /// “更新”分组：当前版本 + 状态 + 检查按钮；有新版再给一行下载入口，
    /// 失败原因另起一行（标题栏只在有新版时亮灯，失败不打扰）。
    fn update_rows(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        use gpui_kit::component::button::Button;
        use gpui_kit::component::Sizable as _;

        let red = cx.theme().red;
        let mut rows = vec![Self::setting_row(
            format!(
                "v{} · {}",
                crate::updater::CURRENT_VERSION,
                self.update_status_text()
            ),
            // “启动自动检查一次”
            t("app.update_hint"),
            Button::new("update-check")
                // “检查更新”
                .label(t("app.update_check"))
                .small()
                .outline()
                .cursor_pointer()
                .on_click(cx.listener(|this, _, _, cx| {
                    this.check_update(cx);
                }))
                .into_any_element(),
            cx,
        )];
        // 有新版：下载页入口行（标题栏图标是主入口，这里再给一行）。
        if let crate::updater::UpdateState::Available { version, .. } = &self.update {
            rows.push(Self::setting_row(
                // “发现新版本 v0.1.2”
                format!("{} v{version}", t("app.update_available")),
                // “点击前往下载”
                t("app.update_open_page"),
                Button::new("update-open")
                    // “前往下载”
                    .label(t("app.update_download"))
                    .small()
                    .outline()
                    .cursor_pointer()
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.open_update_page(cx);
                    }))
                    .into_any_element(),
                cx,
            ));
        }
        // 检查失败：原因红字另起一行，下次启动或手动重试。
        if let crate::updater::UpdateState::Failed(error) = &self.update {
            rows.push(
                div()
                    .text_size(px(TEXT_SM))
                    .text_color(red)
                    .child(format!("{}：{error}", t("app.update_failed")))
                    .into_any_element(),
            );
        }
        rows
    }

    /// 更新状态行文案：版本号在代码里拼，key 只管固定词。
    fn update_status_text(&self) -> String {
        match &self.update {
            crate::updater::UpdateState::Idle => t("app.update_idle"),
            crate::updater::UpdateState::Checking => t("app.update_checking"),
            crate::updater::UpdateState::UpToDate => t("app.update_latest"),
            crate::updater::UpdateState::Available { version, .. } => {
                format!("{} v{version}", t("app.update_available"))
            }
            crate::updater::UpdateState::Failed(error) => {
                format!("{}：{error}", t("app.update_failed"))
            }
        }
    }
    /// “主题”分组：一下拉框（中文名见 themes/wong.json），选中即换；
    /// 聚光面板改了主题，render 对账补回来。
    fn theme_rows(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        use gpui_kit::component::Sizable as _;
        use gpui_kit::component::combobox::Combobox;

        vec![Self::setting_row(
            // “主题”
            t("app.settings_theme"),
            // “点击切换，立即生效”
            t("app.settings_theme_hint"),
            Combobox::new(&self.theme_combo)
                .small()
                .w(px(200.))
                // 菜单跟触发框等宽：默认菜单按内容撑满整行，太长。
                .menu_width(px(200.))
                .into_any_element(),
            cx,
        )]
    }
}
