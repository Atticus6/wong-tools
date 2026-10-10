//! AI 转发工具视图：网关开关、端口、模型表、请求日志。
//! 出站协议和本地服务在 `super`（`zen.rs` / `server.rs`），这里只做组装和状态。

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::tab::{Tab, TabBar};
use gpui_kit::component::table::{Column, DataTable, TableDelegate, TableState};
use gpui_kit::component::{ActiveTheme, Sizable as _};
use gpui_kit::*;

use super::server::{LogSink, RelayHandle, DEFAULT_PORT};
use super::zen::{is_servable, wire_for, FALLBACK_MODELS};
use crate::i18n::t;
use crate::shell::{TEXT_MD, TEXT_SM, WsShell};

/// 模型标签和日志列表的最大高度：超了内部滚，不把整页撑爆。
const LIST_MAX_H: f32 = 220.0;

/// 请求表 delegate：列固定 6 列，行是展示字符串 + 置灰旗。
/// 数据由心跳重建（见 `refresh_req_table`），`render_td` 只读不算。
struct ReqTableRow {
    cells: Vec<String>,
    dimmed: bool,
}

struct ReqTableDelegate {
    columns: Vec<Column>,
    rows: Vec<ReqTableRow>,
}

/// 请求表列定义：key 稳定给取数用（排序这期不开，只当标识），
/// 名走文案，宽按内容给。
fn req_table_columns() -> Vec<Column> {
    [
        ("time", "app.airelay_col_time", 70.0),
        ("route", "app.airelay_col_route", 90.0),
        ("model", "app.airelay_col_model", 230.0),
        ("node", "app.airelay_col_node", 150.0),
        ("tokens", "app.airelay_col_tokens", 110.0),
        ("cache", "app.airelay_col_cache", 90.0),
        ("finish", "app.airelay_col_finish", 90.0),
        ("ms", "app.airelay_col_ms", 80.0),
        ("err", "app.airelay_col_err", 260.0),
    ]
    .into_iter()
    .map(|(key, name, width)| Column::new(key, t(name)).width(px(width)))
    .collect()
}

impl TableDelegate for ReqTableDelegate {
    fn columns_count(&self, _: &App) -> usize {
        self.columns.len()
    }

    fn rows_count(&self, _: &App) -> usize {
        self.rows.len()
    }

    fn column(&self, col_ix: usize, _: &App) -> Column {
        self.columns[col_ix].clone()
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let row = &self.rows[row_ix];
        // 失败行整行置灰，成功的用前景色。
        let color = if row.dimmed {
            cx.theme().muted_foreground
        } else {
            cx.theme().foreground
        };
        div()
            .text_size(px(TEXT_SM))
            .text_color(color)
            .overflow_hidden()
            .whitespace_nowrap()
            .text_ellipsis()
            .child(row.cells[col_ix].clone())
            .into_any_element()
    }
}

/// AI 转发工具的状态：网关句柄（`None` 就是没开）+ 日志槽常驻
/// （停了也留着看，下次启动继续往里记）。
pub(crate) struct AiRelayView {
    /// 运行中的网关（`None` 就是没开）。
    pub(crate) running: Option<RelayHandle>,
    /// 端口输入框 entity，常驻。
    port_input: Entity<InputState>,
    /// 请求日志槽，常驻（启停不丢）。
    log: LogSink,
    /// 请求 DataTable 状态：列固定 6 列，行每秒由心跳重建。
    req_table: Entity<TableState<ReqTableDelegate>>,
    /// 界面状态行（成功失败都写这里）。
    status: Option<String>,
    /// 目录刷新中：按钮防重入。
    refreshing: bool,
    /// 选中页：0 模型，1 请求（只记界面态，不持久化）。
    tab: usize,
    /// 出站展示（直连 / 代理 / 规则拦截）：心跳在后台探测刷新。
    egress: String,
}

impl AiRelayView {
    /// 端口框预填上次的值，日志槽常驻。
    pub(crate) fn new(window: &mut Window, cx: &mut Context<WsShell>) -> Self {
        let initial = crate::settings::airelay_port(DEFAULT_PORT).to_string();
        let port_input = cx.new(|cx| InputState::new(window, cx).default_value(initial));
        let req_table = cx.new(|cx| {
            TableState::new(
                ReqTableDelegate { columns: req_table_columns(), rows: Vec::new() },
                window,
                cx,
            )
            // 纯展示：选择、排序都关，点行没反应。
            .row_selectable(false)
            .col_selectable(false)
            .cell_selectable(false)
        });
        Self { running: None, port_input, log: LogSink::default(), req_table, status: None, refreshing: false, tab: 0, egress: String::new() }
    }

    /// 当前端口（输入框值，非法回落默认）。
    fn port(&self, cx: &App) -> u16 {
        match self.port_input.read(cx).value().to_string().trim().parse::<u16>() {
            Ok(p) if p > 0 => p,
            _ => DEFAULT_PORT,
        }
    }

    /// 可选模型表：开着读网关目录，没开用内置表；接不了的线过滤掉。
    /// 返回 `(models, 是否实时)`。
    fn model_list(&self) -> (Vec<String>, bool) {
        let (models, live) = match &self.running {
            Some(handle) => handle.catalog_snapshot(),
            None => (FALLBACK_MODELS.iter().map(|s| s.to_string()).collect(), false),
        };
        let models = models.into_iter().filter(|name| is_servable(name)).collect();
        (models, live)
    }

    /// 当前生效模型：存的还在目录里就用存的，否则默认第一张——
    /// 永远强制改写，没有跟随模式。
    fn effective_model(&self, models: &[String]) -> String {
        let saved = crate::settings::airelay_model().unwrap_or_default();
        if !saved.is_empty() && models.contains(&saved) {
            return saved;
        }
        models.first().cloned().unwrap_or_default()
    }
}

impl WsShell {
    /// AI 转发工具页：开关行 + 端口行 +（模型 / 请求）双页。
    /// 网关启停走后台线程（端口占用立刻报错），日志靠 1 秒心跳刷，
    /// 切到请求页也即刷一次（没开网关时心跳没有，表会是空的）。
    pub(crate) fn render_airelay(&self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        // 卡片网格布局按实时窗宽算，保证每行撑满。
        let grid = crate::shell::grid_layout(window);
        div()
            .flex()
            .flex_1()
            .min_h_0()
            // 同外壳：横向能收，宽表才有确定宽度出横滚。
            .min_w_0()
            .flex_col()
            .px(px(16.))
            .py(px(12.))
            .gap(px(12.))
            .child(self.render_airelay_controls(cx))
            .child(self.render_airelay_port(cx))
            .child(self.render_airelay_codex(cx))
            .child(
                TabBar::new("airelay-tabs")
                    .underline()
                    .selected_index(self.airelay.tab)
                    .on_click(cx.listener(|this, index, _, cx| {
                        this.airelay.tab = *index;
                        // 切到请求页即刷一次：行数据平时只靠心跳（网关跑着才有），
                        // 没开过网关切进来就是空表，标题却有数，对不上。
                        if *index == 1 {
                            this.refresh_req_table(cx);
                        }
                        cx.notify();
                    }))
                    // “模型”
                    .child(Tab::new().label(t("app.airelay_tab_models")))
                    // “请求”
                    .child(Tab::new().label(t("app.airelay_tab_reqs"))),
            )
            .child(match self.airelay.tab {
                1 => self.render_airelay_log(cx).into_any_element(),
                _ => self.render_airelay_models(grid, cx).into_any_element(),
            })
            .children(self.airelay.status.clone().map(|s| {
                div()
                    .text_size(px(TEXT_SM))
                    .text_color(cx.theme().muted_foreground)
                    .child(s)
            }))
    }

    /// 开关行：启动/停止 + 连通自检；开着显示入口地址。
    fn render_airelay_controls(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let fg = cx.theme().foreground;
        let row = div().flex().flex_none().items_center().gap(px(8.));
        match &self.airelay.running {
            Some(handle) => {
                let addr = format!("http://127.0.0.1:{}/v1", handle.port());
                row.child(
                    Button::new("airelay-stop")
                        // “停止”
                        .label(t("app.airelay_stop"))
                        .small()
                        .outline()
                        .cursor_pointer()
                        .on_click(cx.listener(|this, _, _, cx| {
                            if let Some(relay) = this.airelay.running.take() {
                                relay.stop();
                            }
                            this.airelay.egress.clear();
                            // “已停止”
                            this.airelay.status = Some(t("app.airelay_stopped"));
                            cx.notify();
                        })),
                )
                .child(
                    Button::new("airelay-test")
                        // “连通自检”
                        .label(t("app.airelay_test"))
                        .small()
                        .outline()
                        .cursor_pointer()
                        .on_click(cx.listener(|this, _, _, cx| {
                            let port = this.airelay.port(cx);
                            // “正在自检…”
                            this.airelay.status = Some(t("app.airelay_testing"));
                            cx.notify();
                            // 建连阻塞扔后台，回来写状态行（窗口关了静默丢掉）。
                            cx.spawn(async move |this, cx| {
                                let checked = cx
                                    .background_executor()
                                    .spawn(async move {
                                        let agent = ureq::AgentBuilder::new()
                                            .timeout_connect(std::time::Duration::from_secs(5))
                                            .timeout(std::time::Duration::from_secs(5))
                                            .build();
                                        agent
                                            .get(&format!("http://127.0.0.1:{port}/v1/models"))
                                            .call()
                                            .map(|_| ())
                                            .map_err(|e| e.to_string())
                                    })
                                    .await;
                                cx.update(|cx| {
                                    let _ = this.update(cx, |view, cx| {
                                        // “网关正常”，失败直接展示原因。
                                        view.airelay.status = Some(match checked {
                                            Ok(()) => t("app.airelay_test_ok"),
                                            Err(e) => e,
                                        });
                                        cx.notify();
                                    });
                                });
                            })
                            .detach();
                        })),
                )
                .child(
                    div()
                        .text_size(px(TEXT_MD))
                        .text_color(fg)
                        // “运行中 · 地址 · 出站”
                        .child(if self.airelay.egress.is_empty() {
                            format!("{} · {addr}", t("app.airelay_running"))
                        } else {
                            format!(
                                "{} · {addr} · {}",
                                t("app.airelay_running"),
                                self.airelay.egress
                            )
                        }),
                )
            }
            None => row
                .child(
                    Button::new("airelay-start")
                        // “启动网关”
                        .label(t("app.airelay_start"))
                        .small()
                        .primary()
                        .cursor_pointer()
                        .on_click(cx.listener(|this, _, _, cx| {
                            let port = this.airelay.port(cx);
                            // 端口先入库（非法进不了库，直接报错）。
                            if let Err(e) = crate::settings::set_airelay_port(port) {
                                this.airelay.status = Some(e);
                                cx.notify();
                                return;
                            }
                            // 先占端口再说：被占直接报错，不动系统设置。
                            match RelayHandle::start(port, this.airelay.log.clone()) {
                                Ok(handle) => {
                                    // 启动即套生效模型（没选过默认第一张，永远强制）。
                                    let (models, _) = this.airelay.model_list();
                                    handle.set_forced_model(&this.airelay.effective_model(&models));
                                    this.airelay.running = Some(handle);
                                    // “网关已启动”
                                    this.airelay.status = Some(format!(
                                        "{}：http://127.0.0.1:{port}/v1",
                                        t("app.airelay_started")
                                    ));
                                    // 表格 + 出站心跳：1 秒刷一次，开着才跳，
                                    // 停了自己退（running 没了就 break）。
                                    // 出站探测（连入口 + 读规则）放后台，
                                    // 200ms 探测不能卡 UI 线程：隧道克隆在
                                    // UI 线程拿（轻量），探测扔后台。
                                    cx.spawn(async move |this, cx| {
                                        loop {
                                            cx.background_executor()
                                                .timer(std::time::Duration::from_secs(1))
                                                .await;
                                            let egress = cx
                                                .background_executor()
                                                .spawn(async move {
                                                    super::server::egress_label()
                                                })
                                                .await;
                                            let on = cx
                                                .update(|cx| {
                                                    this.update(cx, |view, cx| {
                                                        let on = view.airelay.running.is_some();
                                                        if on {
                                                            view.airelay.egress = egress;
                                                            view.refresh_req_table(cx);
                                                            cx.notify();
                                                        }
                                                        on
                                                    })
                                                    .unwrap_or(false)
                                                });
                                            if !on {
                                                break;
                                            }
                                        }
                                    })
                                    .detach();
                                }
                                Err(e) => this.airelay.status = Some(e),
                            }
                            cx.notify();
                        })),
                )
                .child(
                    div()
                        .text_size(px(TEXT_SM))
                        .text_color(muted)
                        // “已停止”
                        .child(t("app.airelay_stopped")),
                ),
        }
    }

    /// 端口行：输入框 + 说明（改完点启动生效，启动时入库）。
    fn render_airelay_port(&self, cx: &mut Context<Self>) -> impl IntoElement {
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
                    // “端口”
                    .child(t("app.airelay_port")),
            )
            .child(div().w(px(120.)).child(Input::new(&self.airelay.port_input).small().w_full()))
            .child(
                div()
                    .text_size(px(TEXT_SM))
                    .text_color(muted)
                    // “改完点启动生效”
                    .child(t("app.airelay_port_hint")),
            )
    }

    /// Codex 一键配置行：当前端口 + 生效模型写入 `~/.codex/config.toml`。
    /// 网关开不开都能点（读的是输入框和模型表）；只写本地小文件，
    /// 毫秒级，直接在点击里调，不扔后台。
    fn render_airelay_codex(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(8.))
            .child(
                Button::new("airelay-codex")
                    // “配置 Codex”
                    .label(t("app.airelay_codex"))
                    .small()
                    .outline()
                    .cursor_pointer()
                    .on_click(cx.listener(|this, _, _, cx| {
                        let port = this.airelay.port(cx);
                        let (models, _) = this.airelay.model_list();
                        let model = this.airelay.effective_model(&models);
                        // “Codex 配置已写入”，失败直接展示原因。
                        this.airelay.status = Some(match super::codex::configure(port, &model) {
                            Ok(path) => {
                                format!("{}：{}", t("app.airelay_codex_done"), path.display())
                            }
                            Err(e) => e,
                        });
                        cx.notify();
                    })),
            )
            .child(
                div()
                    .text_size(px(TEXT_SM))
                    .text_color(muted)
                    // “写入 ~/.codex/config.toml，指向本地网关”
                    .child(t("app.airelay_codex_hint")),
            )
    }

    /// 模型页：卡片单选，开着读实时目录（兜底标出来），没开用内置表。
    /// 永远强制改写，没有跟随模式。行一般十来个，封顶高度超了内部滚。
    /// 网格等列撑满整行（布局调用方按窗宽算好传进来）。
    fn render_airelay_models(&self, grid: crate::shell::GridLayout, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let (models, live) = self.airelay.model_list();
        // “实时” / “兜底”
        let source = if live { t("app.airelay_live") } else { t("app.airelay_fallback") };
        // “模型”
        let title = format!("{} ({}) · {source}", t("app.airelay_models"), models.len());
        let mut column = div().flex().flex_col().gap(px(6.));
        column = column.child(
            div()
                .flex()
                .flex_none()
                .items_center()
                .gap(px(8.))
                .child(div().flex_1().text_size(px(TEXT_MD)).text_color(muted).child(title))
                .child(
                    Button::new("airelay-refresh")
                        // “刷新目录”
                        .label(t("app.airelay_refresh"))
                        .small()
                        .outline()
                        .cursor_pointer()
                        .on_click(cx.listener(|this, _, _, cx| {
                            // handle 不能进 async（借用活不过 await）：目录和隧道
                            // 克隆出来再扔后台，回来写状态（窗口关了静默丢掉）。
                            let Some(catalog) = this
                                .airelay
                                .running
                                .as_ref()
                                .map(|h| h.refresh_parts().0) else {
                                // 没开时目录就是内置表，刷它没意义。
                                this.airelay.status = Some(t("app.airelay_stopped"));
                                cx.notify();
                                return;
                            };
                            if this.airelay.refreshing {
                                return;
                            }
                            this.airelay.refreshing = true;
                            // “刷新目录中…”
                            this.airelay.status = Some(t("app.airelay_refreshing"));
                            cx.notify();
                            cx.spawn(async move |this, cx| {
                                let result = cx
                                    .background_executor()
                                    .spawn(async move { catalog.refresh() })
                                    .await;
                                cx.update(|cx| {
                                    let _ = this.update(cx, |view, cx| {
                                        view.airelay.refreshing = false;
                                        view.airelay.status = Some(match result {
                                            // “目录已更新：N 个”
                                            Ok(ids) => format!(
                                                "{}：{}",
                                                t("app.airelay_refreshed"),
                                                ids.len()
                                            ),
                                            Err(e) => e,
                                        });
                                        cx.notify();
                                    });
                                });
                            })
                            .detach();
                        })),
                ),
        );
        // 模型卡按目录顺序排（接不了的线 model_list 里已经过滤）。
        let current = self.airelay.effective_model(&models);
        let cards: Vec<AnyElement> = models
            .iter()
            .enumerate()
            .map(|(ix, name)| self.model_card(ix, name.clone(), current == *name, cx))
            .collect();
        column = column.child(
            div().max_h(px(LIST_MAX_H)).overflow_y_scrollbar().child(
                // 网格定宽铺满：当 flex 孩子会被内容挤成 max-content，
                // 定死像素宽度（调用方按内容区算好）+ 最小 0，轨道才按 1fr 均分整行。
                div().grid().grid_cols(grid.cols).gap(px(8.)).w(px(grid.width)).min_w_0().children(cards),
            ),
        );
        column
    }

    /// 模型卡片：选中铺底色描边（代理节点卡同款），网格等列撑满。
    /// 点即换：持久化 + 开着直接热换到网关，不用重启。
    fn model_card(&self, ix: usize, model: String, selected: bool, cx: &mut Context<Self>) -> AnyElement {
        let fg = cx.theme().foreground;
        let border = cx.theme().border;
        let link = cx.theme().link;
        let row_bg = cx.theme().sidebar_accent;
        let (bg, edge) = if selected {
            (row_bg, link)
        } else {
            (hsla(0., 0., 0., 0.), border)
        };
        // 模型名 + 线标签（chat 默认不标）。
        let tag = match wire_for(&model) {
            crate::tools::airelay::zen::Wire::Responses => " · responses",
            crate::tools::airelay::zen::Wire::Messages => " · messages",
            _ => "",
        };
        let title = format!("{model}{tag}");
        div()
            .id(ElementId::named_usize("airelay-model", ix))
            // 网格孩子：轨道定宽，不设 flex 和定宽（设了会被轨道覆盖，还乱换行）。
            .px(px(10.))
            .py(px(8.))
            .rounded(px(6.))
            .border_1()
            .border_color(edge)
            .bg(bg)
            .cursor_pointer()
            .hover(move |s| if selected { s } else { s.bg(row_bg) })
            .on_click(cx.listener(move |this, _, _, cx| {
                if let Err(e) = crate::settings::set_airelay_model(&model) {
                    eprintln!("存强制模型失败: {e}");
                }
                // 开着直接热换：下个请求就用新的，已建连的不动。
                if let Some(handle) = this.airelay.running.as_ref() {
                    handle.set_forced_model(&model);
                }
                cx.notify();
            }))
            .child(
                div()
                    .text_size(px(TEXT_MD))
                    .text_color(fg)
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .child(title),
            )
            .into_any_element()
    }

    /// 请求表刷新：日志槽读出来，展示行塞进 delegate。
    /// 表自己管虚拟滚动和双滚动条，100 行全给也不卡。
    /// 行数据在 delegate 里（心跳刷新），渲染只摆架子。
    fn refresh_req_table(&mut self, cx: &mut Context<Self>) {
        let snapshots = self.airelay.log.snapshot();
        let rows = snapshots
            .iter()
            .map(|entry| ReqTableRow {
                cells: vec![
                    super::server::clock(entry.at_ms),
                    entry.route.clone(),
                    entry.model.clone(),
                    // 直连没有节点，显示 `-`。
                    if entry.node.is_empty() { "-".to_string() } else { entry.node.clone() },
                    // 输入/输出放一列，缓存单列（都是 0 就显示 `-`）。
                    if entry.input_tokens + entry.output_tokens > 0 {
                        format!("{}/{}", entry.input_tokens, entry.output_tokens)
                    } else {
                        "-".to_string()
                    },
                    if entry.cache_tokens > 0 {
                        entry.cache_tokens.to_string()
                    } else {
                        "-".to_string()
                    },
                    entry.finish.clone(),
                    if entry.err.is_empty() {
                        format!("{}ms", entry.ms)
                    } else {
                        "-".to_string()
                    },
                    if entry.err.is_empty() { "-".to_string() } else { entry.err.clone() },
                ],
                dimmed: !entry.err.is_empty(),
            })
            .collect();
        self.airelay.req_table.update(cx, |state, _| {
            state.delegate_mut().rows = rows;
        });
    }

    /// 请求日志：标题 + 官方 DataTable（虚拟滚动 + 双滚动条）。
    /// 没请求过摆空表（表头还在，比一句空提示有用）。
    fn render_airelay_log(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let count = self.airelay.log.snapshot().len();
        div()
            .flex()
            .flex_1()
            .min_h_0()
            // 横向能收：表格宽度被夹住，超宽列走表内横滚。
            .min_w_0()
            .flex_col()
            .gap(px(6.))
            .child(
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap(px(8.))
                    // “请求”
                    .child(
                        div()
                            .text_size(px(TEXT_MD))
                            .text_color(muted)
                            .child(format!("{} ({count})", t("app.airelay_log"))),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .min_w_0()
                    .child(
                        DataTable::new(&self.airelay.req_table)
                            .scrollbar_visible(true, true)
                            .small(),
                    ),
            )
    }
}
