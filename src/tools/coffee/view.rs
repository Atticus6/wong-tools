//! 咖啡工具视图：一杯 resvg 烤的咖啡（开盖蒸汽动画 / 合盖）+ 防休眠开关 + 时长。
//! 纯逻辑（预设、SVG、烤图、防休眠守卫）在 `super`；这里只做组装和状态。
//! 杯图走 RenderImage 直显（覆盖层冻结帧同款）：同步上传不闪，
//! 之前每帧换新 PNG 走异步解码，杯子一直在闪刷。

use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{ActiveTheme, Sizable as _};
use gpui_kit::*;
use image::{Frame, ImageBuffer};
use smallvec::SmallVec;

use super::DEFAULT_MINUTES;
use super::PRESETS;
use super::parse_custom_minutes;
use super::preset_label;
use super::remaining_text;
use super::render_cup_rgba;
use crate::i18n::t;
use crate::shell::{TEXT_MD, TEXT_SM, WsShell};

/// 杯子展示尺寸（逻辑像素）：backing 是 2 倍（440x488），摆这个尺寸看
/// 着正好（220x244 视图等比缩一点，页面不挤）。
const CUP_DISP_W: f32 = 200.0;
const CUP_DISP_H: f32 = 222.0;
/// 动画帧间隔：120ms 约 8fps，蒸汽连贯、烤图开销无感（小 SVG 一帧几毫秒）。
const ANIM_TICK_MS: u64 = 120;

/// 烤一帧杯子直显图：resvg 出 RGBA → 覆盖层同款换 BGRA → 新 RenderImage
/// （每次 id 都不同，GPUI 缓存不串台，上传同步不闪）。
fn bake_cup_frame(running: bool, frame: u64, dark: bool) -> Result<Arc<RenderImage>, String> {
    let rgba = render_cup_rgba(running, frame, dark)?;
    Ok(crate::tools::screenshot::overlay::rgba_to_render_image(&rgba))
}

/// 1x1 透明兜底图：首帧烤失败（SVG 是程序拼的，单测锁着，理论上走不到）
/// 界面也不崩，状态行会写原因。
fn blank_frame() -> Arc<RenderImage> {
    let buf = ImageBuffer::from_pixel(1, 1, image::Rgba([0, 0, 0, 0]));
    Arc::new(RenderImage::new(SmallVec::from_elem(Frame::new(buf), 1)))
}

/// 运行时：守卫活着就不休眠（`drop` 即放行）+ 到期点（`None` = 常开）。
struct CoffeeRunning {
    /// 持有即防休眠，`None`（stop）即放行。
    _guard: super::KeepAwake,
    /// 到期点（单调钟，系统时间改了也不跑偏）。
    ends_at: Option<Instant>,
}

/// 咖啡工具的状态：运行时（`None` 就是盖着）+ 时长选择（持久化）+
/// 屏幕开关（持久化）+ 状态行 + 杯图（开盖逐帧烤，合盖复用静态帧）。
pub(crate) struct CoffeeView {
    running: Option<CoffeeRunning>,
    minutes: Option<u64>,
    display: bool,
    status: Option<String>,
    /// 心跳代次：每次启动加一，老心跳下轮即退（stop 后不留空转）。
    tick_gen: u64,
    /// 自定义分钟输入框，常驻。
    custom: Entity<InputState>,
    /// 当前杯图（直显帧，开盖每帧换；新 Arc 即新 id，上传同步不闪）。
    cup_frame: Arc<RenderImage>,
    /// 当前杯图的主题：换主题后动画按新配色烤，合盖直接换静态帧。
    cup_dark: bool,
    /// 已播到的蒸汽帧号：下一帧按它 +1 烤，相位连续。
    frame_no: u64,
    /// 合盖静态帧（深/浅各一张，启动一次烤好，换主题直接换着显示，
    /// 不用等动画循环，更不用重烤）。
    cup_idle_dark: Arc<RenderImage>,
    cup_idle_light: Arc<RenderImage>,
}

impl CoffeeView {
    /// 时长和屏幕开关读上次的（坏了回落默认），运行态永远从合盖开始
    /// （重启就默认放行，绝不悄悄举着断言）。合盖静态帧深浅各烤一张，
    /// 首帧同步就有图（纯 CPU 几毫秒，不进后台）。
    pub(crate) fn new(window: &mut Window, cx: &mut Context<WsShell>) -> Self {
        // “输入分钟数，如 90”
        let hint = t("app.coffee_custom_hint");
        let custom = cx.new(|cx| InputState::new(window, cx).placeholder(hint));
        let bake_idle = |dark: bool| {
            bake_cup_frame(false, 0, dark).unwrap_or_else(|e| {
                eprintln!("咖啡首帧烤失败: {e}");
                blank_frame()
            })
        };
        let cup_idle_dark = bake_idle(true);
        let cup_idle_light = bake_idle(false);
        Self {
            running: None,
            minutes: crate::settings::coffee_minutes().unwrap_or(DEFAULT_MINUTES),
            display: crate::settings::coffee_display().unwrap_or(false),
            status: None,
            tick_gen: 0,
            custom,
            cup_frame: cup_idle_light.clone(),
            cup_dark: false,
            frame_no: 0,
            cup_idle_dark,
            cup_idle_light,
        }
    }

    /// 退出兜底：守卫 drop 即放行，代次加一让心跳自己退，PID 顺手清掉
    /// （不清下次启动会把这次当没收尾，白跑一次认领）。
    pub(crate) fn shutdown(&mut self) {
        self.tick_gen += 1;
        self.running = None;
        // Windows 没记过 PID，清了也是空写，cfg 掉。
        #[cfg(not(windows))]
        let _ = crate::settings::set_coffee_pid(None);
    }

    /// 剩余秒（常开 / 没开都是 `None`，调用方按这个切文案）。
    fn remaining_secs(&self) -> Option<u64> {
        self.running.as_ref()?.ends_at.map(|end| {
            end.saturating_duration_since(Instant::now()).as_secs()
        })
    }

    /// 当前主题是不是深色：杯子配色跟它走（`super::palette` 两套）。
    fn is_dark(cx: &Context<WsShell>) -> bool {
        cx.theme().mode.is_dark()
    }

    /// 端起咖啡：举断言 + 按当前时长定到期点 + 起动画心跳。
    /// 第一帧先摆当前主题的静态盖（动画第一帧 120ms 后到，不断片）。
    /// 断言失败只写状态行，不动旧状态（旧的本来就是合盖）。
    fn start(&mut self, cx: &mut Context<WsShell>) {
        if self.running.is_some() {
            return;
        }
        match super::KeepAwake::acquire(self.display) {
            Ok(guard) => {
                let minutes = self.minutes;
                self.running = Some(CoffeeRunning {
                    _guard: guard,
                    ends_at: minutes.map(|m| Instant::now() + Duration::from_secs(m * 60)),
                });
                self.status = None;
                // PID 入库：崩了/被杀没清掉，下次启动按它认领孤儿。
                // Windows 没子进程，不记（`child_pid` 那边 cfg 掉了）。
                #[cfg(not(windows))]
                if let Some(running) = self.running.as_ref() {
                    let pid = running._guard.child_pid();
                    if let Err(e) = crate::settings::set_coffee_pid(Some(pid)) {
                        eprintln!("存咖啡 PID 失败: {e}");
                    }
                }
                let dark = Self::is_dark(cx);
                self.cup_dark = dark;
                self.cup_frame = self.pick_idle(dark);
                self.spawn_anim(cx);
            }
            Err(e) => self.status = Some(e),
        }
        cx.notify();
    }

    /// 当前主题的合盖静态帧（render 和 stop 共用，行为一致）。
    fn pick_idle(&self, dark: bool) -> Arc<RenderImage> {
        if dark {
            self.cup_idle_dark.clone()
        } else {
            self.cup_idle_light.clone()
        }
    }

    /// 盖上盖子：守卫 drop 即放行，杯图换回当前主题的静态帧。
    /// `expired` 为 true 是到点自动盖的，状态行报一声；
    /// 手动盖的报休息中。
    fn stop(&mut self, expired: bool, cx: &mut Context<WsShell>) {
        self.tick_gen += 1;
        self.running = None;
        // 正常放行清 PID：不清下次启动会白跑一次认领。Windows 没记过不写。
        #[cfg(not(windows))]
        let _ = crate::settings::set_coffee_pid(None);
        let dark = Self::is_dark(cx);
        self.cup_dark = dark;
        self.cup_frame = self.pick_idle(dark);
        self.status = Some(if expired {
            // “时间到，已盖上盖子”
            t("app.coffee_expired")
        } else {
            // “盖着盖子 · 休息中”
            t("app.coffee_stopped")
        });
        cx.notify();
    }

    /// 动画心跳：120ms 一帧，干两件事——到期自动盖、心跳烤下一帧蒸汽。
    /// 烤图扔后台（UI 线程不卡），回写时再验代次（stop 后回来的晚帧丢掉）。
    /// 主题每轮现读：换主题后下一帧自动按新配色烤，不用重进页面。
    /// 停了 / 新一代起了自己退（代理连接表同款）。
    fn spawn_anim(&mut self, cx: &mut Context<WsShell>) {
        self.tick_gen += 1;
        let gen = self.tick_gen;
        cx.spawn(async move |this, cx| {
            // 每轮的三态：退出 / 到期盖盖 / 烤下一帧。读状态只用一次 update，
            // 帧号和主题在读时就定好（渲染稍慢半拍也无妨，相位是连续函数）。
            enum Step {
                Exit,
                Expired,
                Frame { frame: u64, dark: bool },
            }
            loop {
                cx.background_executor().timer(Duration::from_millis(ANIM_TICK_MS)).await;
                let step = cx
                    .update(|cx| {
                        this.update(cx, |view, cx| {
                            if view.coffee.tick_gen != gen || view.coffee.running.is_none() {
                                return Step::Exit;
                            }
                            let expired = view.coffee.running.as_ref().is_some_and(|r| {
                                r.ends_at.is_some_and(|end| Instant::now() >= end)
                            });
                            if expired {
                                return Step::Expired;
                            }
                            Step::Frame {
                                frame: view.coffee.frame_no + 1,
                                dark: Self::is_dark(cx),
                            }
                        })
                        .unwrap_or(Step::Exit)
                    });
                match step {
                    Step::Exit => break,
                    Step::Expired => {
                        cx.update(|cx| {
                            let _ = this.update(cx, |view, cx| view.coffee.stop(true, cx));
                        });
                        break;
                    }
                    Step::Frame { frame, dark } => {
                        let baked = cx
                            .background_executor()
                            .spawn(async move { bake_cup_frame(true, frame, dark) })
                            .await;
                        cx.update(|cx| {
                            let _ = this.update(cx, |view, cx| {
                                if view.coffee.tick_gen != gen || view.coffee.running.is_none() {
                                    return;
                                }
                                match baked {
                                    Ok(image) => {
                                        view.coffee.frame_no = frame;
                                        view.coffee.cup_dark = dark;
                                        view.coffee.cup_frame = image;
                                    }
                                    Err(e) => view.coffee.status = Some(e),
                                }
                                cx.notify();
                            });
                        });
                    }
                }
            }
        })
        .detach();
    }

    /// 选时长：持久化；开着就按新时长重定到期点（不用先盖再端）。
    fn select_minutes(&mut self, minutes: Option<u64>, cx: &mut Context<WsShell>) {
        self.minutes = minutes;
        if let Err(e) = crate::settings::set_coffee_minutes(minutes) {
            eprintln!("存咖啡时长失败: {e}");
        }
        if let Some(running) = self.running.as_mut() {
            running.ends_at = minutes.map(|m| Instant::now() + Duration::from_secs(m * 60));
        }
        self.status = None;
        cx.notify();
    }

    /// 屏幕开关：持久化；开着就重举断言（新旗即生效），重举失败保持旧状
    /// 态（不断不断言，开关也回滚到旧值，界面不说谎）。
    fn set_display(&mut self, display: bool, cx: &mut Context<WsShell>) {
        if display == self.display {
            return;
        }
        if let Some(running) = self.running.as_mut() {
            match super::KeepAwake::acquire(display) {
                Ok(guard) => {
                    running._guard = guard;
                    self.display = display;
                    if let Err(e) = crate::settings::set_coffee_display(display) {
                        eprintln!("存屏幕常亮失败: {e}");
                    }
                    self.status = None;
                }
                Err(e) => self.status = Some(e),
            }
        } else {
            self.display = display;
            if let Err(e) = crate::settings::set_coffee_display(display) {
                eprintln!("存屏幕常亮失败: {e}");
            }
        }
        cx.notify();
    }

    /// 自定义时长：输入框值验过（1–1440）才走选中流程，不合法只写状态行。
    fn apply_custom(&mut self, cx: &mut Context<WsShell>) {
        let raw = self.custom.read(cx).value().to_string();
        match parse_custom_minutes(&raw) {
            Ok(m) => self.select_minutes(Some(m), cx),
            // “分钟数要在 1–1440 之间”
            Err(()) => {
                self.status = Some(t("app.coffee_bad_custom"));
                cx.notify();
            }
        }
    }
}

impl WsShell {
    /// 咖啡工具页：杯子 + 状态行 + 开关 + 时长 + 屏幕 + 自定义。
    /// 内容居中，一屏能装下（短窗出横滚，不裁杯子）。
    pub(crate) fn render_coffee(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let running = self.coffee.running.is_some();
        div()
            .flex()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .flex_col()
            .items_center()
            .justify_center()
            .overflow_y_scrollbar()
            .px(px(16.))
            .py(px(12.))
            .gap(px(10.))
            .child(self.render_cup(cx))
            .child(self.render_coffee_status(cx))
            .child(self.render_coffee_switch(running, cx))
            .child(self.render_coffee_durations(cx))
            .child(self.render_coffee_display(cx))
            .child(self.render_coffee_custom(cx))
    }

    /// 状态胶囊：圆点（开着绿 / 合着灰）+ 文案，描边胶囊钉在杯卡下面。
    /// 开着报剩余（常开另报），合着报休息中，操作反馈优先显示。
    fn render_coffee_status(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let fg = cx.theme().foreground;
        let border = cx.theme().border;
        let wash = cx.theme().accent;
        let running = self.coffee.running.is_some();
        let text = if let Some(status) = &self.coffee.status {
            status.clone()
        } else if running {
            match self.coffee.remaining_secs() {
                // “冒着热气 · 保持唤醒” + 剩余
                Some(secs) => format!("{} · {}", t("app.coffee_running"), remaining_text(secs)),
                // “冒着热气 · 保持唤醒” + 常开
                None => format!("{} · {}", t("app.coffee_running"), t("app.coffee_no_limit")),
            }
        } else {
            // “盖着盖子 · 休息中”
            t("app.coffee_stopped")
        };
        // 状态点：开着用主题绿，合着用弱色，一眼看出在不在工作。
        let dot = if running { cx.theme().green } else { muted };
        div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(8.))
            .px(px(12.))
            .py(px(5.))
            .rounded_full()
            .bg(wash)
            .border_1()
            .border_color(border)
            .child(div().flex_none().size(px(8.)).rounded_full().bg(dot))
            .child(div().text_size(px(TEXT_MD)).text_color(fg).child(text))
    }

    /// 开关行：合着是主按钮端起，开着是描边按钮盖上。
    fn render_coffee_switch(&self, running: bool, cx: &mut Context<Self>) -> impl IntoElement {
        div().flex().flex_none().items_center().gap(px(8.)).child(if running {
            Button::new("coffee-stop")
                // “盖上盖子”
                .label(t("app.coffee_stop"))
                .outline()
                .cursor_pointer()
                .on_click(cx.listener(|this, _, _, cx| {
                    this.coffee.stop(false, cx);
                }))
                .into_any_element()
        } else {
            Button::new("coffee-start")
                // “端起咖啡”
                .label(t("app.coffee_start"))
                .primary()
                .cursor_pointer()
                .on_click(cx.listener(|this, _, _, cx| {
                    this.coffee.start(cx);
                }))
                .into_any_element()
        })
    }

    /// 时长行：标签 + 预设一排，选中铺主色（模型卡单选同款语义）。
    /// 自定义值不在预设里时谁都不高亮（输入框里留着原值）。
    fn render_coffee_durations(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let mut row = div().flex().flex_none().items_center().gap(px(8.)).child(
            div()
                .flex_none()
                .text_size(px(TEXT_MD))
                .text_color(muted)
                // “时长”
                .child(t("app.coffee_duration")),
        );
        for (ix, minutes) in PRESETS.iter().enumerate() {
            let selected = self.coffee.minutes == *minutes;
            let label = preset_label(*minutes);
            let mut button = Button::new(ElementId::named_usize("coffee-preset", ix))
                .label(label)
                .small()
                .cursor_pointer();
            button = if selected { button.primary() } else { button.outline() };
            let minutes = *minutes;
            row = row.child(button.on_click(cx.listener(move |this, _, _, cx| {
                this.coffee.select_minutes(minutes, cx);
            })));
        }
        row
    }

    /// 屏幕行：开关 + 说明（默认只保主机不睡，屏幕照常可熄）。
    fn render_coffee_display(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(8.))
            .child(
                Switch::new("coffee-display")
                    .small()
                    // “屏幕也常亮”
                    .label(t("app.coffee_display"))
                    // “默认只防主机休眠，屏幕照常可熄”
                    .tooltip(t("app.coffee_display_hint"))
                    .checked(self.coffee.display)
                    .on_click(cx.listener(|this, checked: &bool, _, cx| {
                        this.coffee.set_display(*checked, cx);
                    })),
            )
            .child(
                div()
                    .text_size(px(TEXT_SM))
                    .text_color(muted)
                    // “默认只防主机休眠，屏幕照常可熄”
                    .child(t("app.coffee_display_hint")),
            )
    }

    /// 自定义行：分钟输入框 + 使用按钮（1–1440，非法只写状态行）。
    fn render_coffee_custom(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(8.))
            .child(
                div()
                    .flex_none()
                    .text_size(px(TEXT_MD))
                    .text_color(muted)
                    // “自定义”
                    .child(t("app.coffee_custom")),
            )
            .child(div().w(px(110.)).child(Input::new(&self.coffee.custom).small().w_full()))
            .child(
                Button::new("coffee-apply")
                    // “使用”
                    .label(t("app.coffee_apply"))
                    .small()
                    .outline()
                    .cursor_pointer()
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.coffee.apply_custom(cx);
                    })),
            )
    }

    /// 杯子：RenderImage 直显帧（开盖是逐帧烤的蒸汽动画，合盖是静态帧）。
    /// 合盖按当前主题选静态帧（深浅各一张，启动一次烤好）；开着显示动画帧
    /// （主题每 120ms 跟烤，换主题半帧就对上）。杯子直接摆，不套卡片
    /// （套了像多出来的框，杯子自带阴影和杯托，足够落地）。
    fn render_cup(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let frame = if self.coffee.running.is_some() {
            self.coffee.cup_frame.clone()
        } else {
            self.coffee.pick_idle(CoffeeView::is_dark(cx))
        };
        div()
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            .child(img(frame).w(px(CUP_DISP_W)).h(px(CUP_DISP_H)))
            .into_any_element()
    }
}
