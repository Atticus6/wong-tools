//! 代理工具视图：订阅拉取、节点分组卡片、延迟测速、规则管理。
//! 出站协议和分流在 `super`（`mod.rs`），覆盖层没有；这里只做组装和状态。

use std::collections::HashMap;
use std::rc::Rc;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::dialog::DialogFooter;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::scroll::{Scrollbar, ScrollbarAxis};

use gpui_kit::component::select::{Select, SelectState};
use gpui_kit::component::tab::{Tab, TabBar};
use gpui_kit::component::table::{Column, DataTable, TableDelegate, TableState};
use gpui_kit::component::WindowExt;
use gpui_kit::component::{ActiveTheme, Sizable as _, VirtualListScrollHandle, v_virtual_list};
use gpui_kit::*;

use crate::i18n::t;
use crate::shell::{TEXT_MD, TEXT_SM, WsShell};

/// 自动规则虚拟列表的行高：行元素必须 exactly 这么高（见行里的 `h`），
/// 否则滚动定位会对不齐。条数变化时重建 `auto_sizes`，和规则表同长。
const AUTO_ROW_H: f32 = 22.0;

/// 分流查询结果：输的 host、命中的规则（按优先级）、最终去向。
struct LookupOutcome {
    host: String,
    hits: Vec<crate::tools::proxy::LookupHit>,
    route: crate::tools::proxy::Route,
}

/// 代理工具的状态：订阅输入框常驻（entity 活在主窗口里），节点列表
/// 拉取后才有，`running` 拿着监听线程句柄（`None` 就是没开）。
/// `latencies` 是节点名到测速结果的映射（`None` 表示超时），测速中
/// 还没跑完的节点直接不在表里。
/// `manual_rules` 是用户手填的规则（独立库 key，更新订阅不碰它）。
pub(crate) struct ProxyView {
    sub: Entity<InputState>,
    lookup_input: Entity<InputState>,
    lookup_result: Option<LookupOutcome>,
    manual_pattern: Entity<InputState>,
    /// 目标下拉框状态：每次打开弹框按当前订阅重建（分组在前），
    /// 确定时读它的选中值；平时是 None，不占东西。
    target_select: Option<Entity<SelectState<Vec<String>>>>,
    manual_kind: crate::tools::proxy::ManualKind,
    manual_rules: Vec<crate::tools::proxy::ManualRule>,
    /// 自动规则虚拟列表的行尺寸缓存：和规则表同长同序，规则变就重建；
    /// 滚动句柄常驻，换页回来位置不丢。
    auto_sizes: Rc<Vec<Size<Pixels>>>,
    auto_scroll: VirtualListScrollHandle,
    /// 连接表：启动时建，停了也留着看（下次启动换新表）。
    /// 引擎往里记，界面读快照，`Mutex` 里面换手。
    conns: Option<crate::tools::proxy::Registry>,
    /// 连接表过滤框 entity，常驻。
    /// 表头两个开关：只看活跃 / 只看已关闭，默认都开。
    show_active: bool,
    show_closed: bool,
    /// 连接 DataTable 状态：列固定 12 列，行每秒由心跳重建。
    /// 过滤（开关 + 文本）进 delegate 行，不在渲染里算。
    conn_table: Entity<TableState<ConnTableDelegate>>,
    conn_filter: Entity<InputState>,
    /// 上次心跳时的计数：连接 id →（上行累计，下行累计），算速率用。
    conn_last: HashMap<u64, (u64, u64)>,
    tab: usize,
    subscription: crate::tools::proxy::Subscription,
    /// 各组独立的选中（组名 → 成员名，面板站同款）：节点、`DIRECT`、
    /// 嵌套组都能选，点哪组记哪组，持久化。分流时按组找去向，
    /// 不再有全局唯一的隧道节点。
    selections: HashMap<String, String>,
    /// 外壳的 Drop 靠它兜底清系统代理，字段必须可见。
    pub(crate) running: Option<crate::tools::proxy::ProxyHandle>,
    /// 活分流表：启动时建，停了就扔。界面改选择即换新表，
    /// 下一个连接直接用上，已建连的不受影响。
    live: Option<crate::tools::proxy::LiveRouting>,
    latencies: HashMap<String, Option<u64>>,
    testing: bool,
    status: Option<String>,
    busy: bool,
}

/// 连接表 delegate：列固定 12 列，行是展示字符串 + 置灰旗。
/// 数据每秒由心跳重建，`render_td` 只读不算，保持轻。
struct ConnTableRow {
    cells: Vec<String>,
    dimmed: bool,
}

struct ConnTableDelegate {
    columns: Vec<Column>,
    rows: Vec<ConnTableRow>,
}

/// 连接表列定义：key 稳定给排序/取数用（排序这期不开，只当标识），
/// 名走文案，宽和原来手写表格一致。
fn conn_table_columns() -> Vec<Column> {
    [
        ("host", "app.proxy_col_host", 210.0),
        ("down", "app.proxy_col_down", 80.0),
        ("up", "app.proxy_col_up", 80.0),
        ("down_speed", "app.proxy_col_down_speed", 85.0),
        ("up_speed", "app.proxy_col_up_speed", 85.0),
        ("chain", "app.proxy_col_chain", 230.0),
        ("rule", "app.proxy_col_rule", 190.0),
        ("process", "app.proxy_col_process", 100.0),
        ("age", "app.proxy_col_age", 70.0),
        ("source", "app.proxy_col_source", 140.0),
        ("dest", "app.proxy_col_dest", 140.0),
        ("kind", "app.proxy_col_kind", 90.0),
    ]
    .into_iter()
    .map(|(key, name, width)| Column::new(key, t(name)).width(px(width)))
    .collect()
}

impl TableDelegate for ConnTableDelegate {
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
        // 关掉的行整行置灰，活的用前景色。
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

/// 手动规则 id：毫秒时间戳 + 进程内序号，删除和排序认它。
fn next_manual_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!("m{ms}-{}", SEQ.fetch_add(1, Ordering::Relaxed))
}

impl ProxyView {
    /// 订阅框预填上次的地址，节点/分组/选中/手动规则从库里恢复
    /// （上次拉取过就不用再拉，手动规则更新订阅也不动）。
    pub(crate) fn new(window: &mut Window, cx: &mut Context<WsShell>) -> Self {
        // 没存过就空着：订阅地址由用户填写，填完点更新即存库。
        let initial = crate::settings::proxy_sub_url().unwrap_or_default();
        let sub = cx.new(|cx| InputState::new(window, cx).default_value(initial));
        // “输入域名或 IP 查分流”
        let lookup_hint = t("app.proxy_lookup_hint");
        let lookup_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(lookup_hint)
        });
        // “内容，如 example.com”
        let pattern_hint = t("app.proxy_pattern_hint");
        let manual_pattern = cx.new(|cx| {
            InputState::new(window, cx).placeholder(pattern_hint)
        });
        // “过滤条件”
        let filter_hint = t("app.proxy_filter_hint");
        let conn_filter = cx.new(|cx| {
            InputState::new(window, cx).placeholder(filter_hint)
        });

        let subscription: crate::tools::proxy::Subscription =
            crate::settings::proxy_subscription().unwrap_or_default();
        let saved = crate::settings::proxy_selections().unwrap_or_default();
        let selections =
            crate::tools::proxy::restore_selections(&subscription, &saved);
        let mut manual_rules = crate::settings::proxy_manual_rules();
        manual_rules.sort_by_key(|rule| rule.sort);
        let conn_table = cx.new(|cx| {
            TableState::new(
                ConnTableDelegate {
                    columns: conn_table_columns(),
                    rows: Vec::new(),
                },
                window,
                cx,
            )
            // 纯展示：选择、排序都关，点行没反应。
            .row_selectable(false)
            .col_selectable(false)
            .cell_selectable(false)
        });
        let mut view = Self {
            sub,
            lookup_input,
            lookup_result: None,
            manual_pattern,
            target_select: None,
            manual_kind: crate::tools::proxy::ManualKind::Suffix,
            manual_rules,
            auto_sizes: Rc::new(Vec::new()),
            auto_scroll: VirtualListScrollHandle::new(),
            conns: None,
            conn_filter,
            show_active: true,
            show_closed: true,
            conn_table,
            conn_last: HashMap::new(),
            tab: 0,
            subscription,
            selections,
            running: None,
            live: None,
            latencies: HashMap::new(),
            testing: false,
            status: None,
            busy: false,
        };
        view.refresh_auto_sizes();
        view
    }

    /// 按有效规则表重建虚拟列表行尺寸：等高行，一行一个。
    /// 手动条数也算进去（总表 = 手动 + 自动），拉取回来、
    /// 手动增删改后都重建，条数对不上滚动会越界。
    /// 手动转规则失败的行（入库时已校验，理论上没有）两边一起跳过，
    /// 行号才对得齐。
    fn refresh_auto_sizes(&mut self) {
        // 只数转得出来的手动条数，和 `effective_row` 的跳过逻辑一致。
        let manual_len = self
            .manual_rules
            .iter()
            .filter(|rule| rule.to_rule().is_some())
            .count();
        let total = manual_len + self.subscription.rules.len();
        self.auto_sizes = Rc::new(
            (0..total)
                .map(|_| size(px(800.), px(AUTO_ROW_H)))
                .collect(),
        );
    }

    /// 有效规则第 `ix` 行：手动在前（带来源标记），自动跟后。
    /// 和 `refresh_auto_sizes` 同一套算法，行号才对得齐。
    fn effective_row(&self, ix: usize) -> Option<(bool, String, String)> {
        let mut ordered: Vec<&crate::tools::proxy::ManualRule> =
            self.manual_rules.iter().collect();
        ordered.sort_by_key(|rule| rule.sort);
        let mut manual_len = 0;
        for rule in &ordered {
            if ix == manual_len {
                let converted = rule.to_rule()?;
                return Some((
                    true,
                    crate::tools::proxy::rule_text(&converted),
                    converted.target,
                ));
            }
            if rule.to_rule().is_some() {
                manual_len += 1;
            }
        }
        let auto = self.subscription.rules.get(ix - manual_len)?;
        Some((
            false,
            crate::tools::proxy::rule_text(auto),
            auto.target.clone(),
        ))
    }

    /// 自检兜底用的主节点：按组顺序找第一个能落到节点的组选择，
    /// 组要是选了 `DIRECT` 之流就往下找，都没有返回 None（调用方提示拉订阅）。
    fn primary_node(&self) -> Option<crate::tools::proxy::ProxyNode> {
        for group in &self.subscription.groups {
            let choice = self
                .selections
                .get(&group.name)
                .cloned()
                .or_else(|| group.members.first().cloned())?;
            if let Some(node) = self.subscription.nodes.iter().find(|n| n.name == choice) {
                return Some(node.clone());
            }
        }
        self.subscription.nodes.first().cloned()
    }
}

impl WsShell {
    /// 代理工具页：三个标签（代理 / 订阅 / 规则）+ 共用的状态行。
    /// 拉取和测速都走后台线程（订阅 4MB 级别，主线程跑会卡住界面）。
    /// 分流查询 8 万条全扫几毫秒，直接 UI 线程算。
    pub(crate) fn render_proxy(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        div()
            .flex()
            .flex_1()
            .min_h_0()
            .flex_col()
            .px(px(16.))
            .py(px(12.))
            .gap(px(12.))
            .child(
                TabBar::new("proxy-tabs")
                    .underline()
                    .selected_index(self.proxy.tab)
                    .on_click(cx.listener(|this, index, _, cx| {
                        this.proxy.tab = *index;
                        cx.notify();
                    }))
                    // “代理”
                    .child(Tab::new().label(t("app.tool_proxy")))
                    // “订阅”
                    .child(Tab::new().label(t("app.proxy_tab_sub")))
                    // “规则”
                    .child(Tab::new().label(t("app.proxy_tab_rules")))
                    // “连接”
                    .child(Tab::new().label(t("app.proxy_tab_conns"))),
            )
            .child(match self.proxy.tab {
                1 => self.render_proxy_sub(cx).into_any_element(),
                2 => self.render_proxy_rules(cx).into_any_element(),
                3 => self.render_proxy_conns(cx).into_any_element(),
                _ => self.render_proxy_main(cx).into_any_element(),
            })
            .children(self.proxy.status.clone().map(|s| {
                div()
                    .text_size(px(TEXT_SM))
                    .text_color(muted)
                    // 状态行：成功失败都写这里，中文见 locales/ui.yml。
                    .child(s)
            }))
    }

    /// 慢操作互斥：拉订阅和测速不同时跑，界面按钮靠它防重入。
    fn proxy_occupied(&self) -> bool {
        self.proxy.busy || self.proxy.testing
    }

    /// 一键测速：全部节点并行建连，结果回写后重绘。正在测或没节点直接返回。
    fn start_speedtest(&mut self, cx: &mut Context<Self>) {
        if self.proxy.testing || self.proxy.busy || self.proxy.subscription.nodes.is_empty() {
            return;
        }
        let nodes = self.proxy.subscription.nodes.clone();
        // 组名空着就是整表测，状态行不带组名。
        self.run_speedtest(nodes, String::new(), cx);
    }

    /// 隧道自检：直连主节点穿隧道取测试页，报通过 + 耗时或失败原因。
    /// 和测速共用互斥旗（一次只干一件慢事）；不碰系统代理，开没开都能点。
    fn start_selfcheck(&mut self, cx: &mut Context<Self>) {
        if self.proxy_occupied() {
            return;
        }
        let Some(node) = self.proxy.primary_node() else {
            // “先更新订阅并选择节点”
            self.proxy.status = Some(t("app.proxy_need_nodes"));
            cx.notify();
            return;
        };
        self.proxy.testing = true;
        // “正在自检…”
        self.proxy.status = Some(t("app.proxy_selfchecking"));
        cx.notify();
        cx.spawn(async move |this, cx| {
            let checked = cx
                .background_executor()
                .spawn(async move { crate::tools::proxy::self_check(&node) })
                .await;
            cx.update(|cx| {
                let _ = this.update(cx, |view, cx| {
                    view.proxy.testing = false;
                    // “自检通过：123ms”，失败直接展示原因。
                    view.proxy.status = Some(match checked {
                        Ok(ms) => format!("{}：{ms}ms", t("app.proxy_selfcheck_ok")),
                        Err(e) => e,
                    });
                    cx.notify();
                });
            });
        })
        .detach();
    }

    /// 分组测速：只测该组内的节点，结果并入延迟表（别组的旧结果留着）。
    /// 组里一个节点都没有直接返回（这种组不画测速按钮，防呆）。
    fn start_group_speedtest(&mut self, group: String, cx: &mut Context<Self>) {
        if self.proxy.testing || self.proxy.busy {
            return;
        }
        let Some(target) = self
            .proxy
            .subscription
            .groups
            .iter()
            .find(|g| g.name == group)
        else {
            return;
        };
        let nodes: Vec<crate::tools::proxy::ProxyNode> =
            crate::tools::proxy::resolve_members(&self.proxy.subscription, target)
                .into_iter()
                .filter_map(|member| match member {
                    crate::tools::proxy::GroupMember::Node(node) => Some(node.clone()),
                    _ => None,
                })
                .collect();
        if nodes.is_empty() {
            return;
        }
        self.run_speedtest(nodes, group, cx);
    }

    /// 测速干活：并行建连扔后台，回来把结果并入延迟表后重绘。
    /// `group` 为空是整表测，状态行不带组名；否则状态行带组名。
    fn run_speedtest(
        &mut self,
        nodes: Vec<crate::tools::proxy::ProxyNode>,
        group: String,
        cx: &mut Context<Self>,
    ) {
        self.proxy.testing = true;
        // “测速中…”（分组测带组名）。
        self.proxy.status = Some(if group.is_empty() {
            t("app.proxy_testing")
        } else {
            format!("{}：{group}", t("app.proxy_testing"))
        });
        cx.notify();
        // 建连阻塞，并行扔后台，回来再写 entity（窗口关了就静默丢掉）。
        cx.spawn(async move |this, cx| {
            let results = cx
                .background_executor()
                .spawn(async move { crate::tools::proxy::measure_all(&nodes) })
                .await;
            let group = group.clone();
            cx.update(|cx| {
                let _ = this.update(cx, |view, cx| {
                    view.proxy.testing = false;
                    view.proxy.latencies.extend(results);
                    // 延迟变了，url-test 组下个连接就要新结果，顺手刷新活表。
                    view.refresh_live_routing();
                    // “测速完成”（分组测带组名）。
                    view.proxy.status = Some(if group.is_empty() {
                        t("app.proxy_tested")
                    } else {
                        format!("{}：{group}", t("app.proxy_tested"))
                    });
                    cx.notify();
                });
            });
        })
        .detach();
    }

    /// 刷新活分流表：按界面当前状态重建，换进去下个连接就用新的。
    /// 没在跑（`live` 是 None）直接返回；重建要排 8 万条后缀，
    /// 几十毫秒，点的那一下无感。调用方顺手 `notify`。
    fn refresh_live_routing(&mut self) {
        let Some(live) = self.proxy.live.clone() else {
            return;
        };
        let table = crate::tools::proxy::RoutingTable::build(
            self.proxy.subscription.clone(),
            &self.proxy.manual_rules,
            self.proxy.selections.clone(),
            self.proxy.latencies.clone(),
        );
        // 毒化就拿毒里的继续写（几乎不可能）：下次点还会再刷，不丢数据。
        let mut guard = live.write().unwrap_or_else(|poisoned| poisoned.into_inner());
        *guard = table;
    }

    /// 连接表刷新：快照读出来，速率和展示行一起算好塞进 delegate。
    /// 注册表没建（没启动过）直接返回；死掉又被裁掉的连接，
    /// 基线一起清，表不漏水。过滤（开关 + 文本）在这里落，不在渲染里算。
    fn refresh_conn_table(&mut self, cx: &mut Context<Self>) {
        use crate::tools::proxy::{format_age, format_bytes, format_speed, unix_millis_now};
        let Some(registry) = self.proxy.conns.clone() else {
            return;
        };
        let snapshots = registry
            .lock()
            .map(|registry| registry.snapshot())
            .unwrap_or_default();
        let needle = self.proxy.conn_filter.read(cx).value().to_string();
        let needle = needle.trim().to_ascii_lowercase();
        let now = unix_millis_now();
        let mut rates = HashMap::new();
        let mut rows = Vec::new();
        for snapshot in snapshots.iter().filter(|snapshot| {
            (snapshot.active && self.proxy.show_active
                || !snapshot.active && self.proxy.show_closed)
                && (needle.is_empty()
                    || snapshot.host.to_ascii_lowercase().contains(&needle)
                    || snapshot.dest.to_ascii_lowercase().contains(&needle))
        }) {
            let (last_up, last_down) = self
                .proxy
                .conn_last
                .get(&snapshot.id)
                .copied()
                // 第一轮没基线，先按当前记，速率记 0，下轮自然有数。
                .unwrap_or((snapshot.up, snapshot.down));
            let (up_rate, down_rate) = (
                snapshot.up.saturating_sub(last_up),
                snapshot.down.saturating_sub(last_down),
            );
            rates.insert(snapshot.id, (up_rate, down_rate));
            self.proxy.conn_last.insert(snapshot.id, (snapshot.up, snapshot.down));
            rows.push(ConnTableRow {
                cells: vec![
                    snapshot.host.clone(),
                    format_bytes(snapshot.down),
                    format_bytes(snapshot.up),
                    format_speed(down_rate),
                    format_speed(up_rate),
                    snapshot.chain.clone(),
                    snapshot.rule.clone(),
                    snapshot.process.clone(),
                    format_age(now.saturating_sub(snapshot.started_ms)),
                    snapshot.source.clone(),
                    snapshot.dest.clone(),
                    snapshot.kind.clone(),
                ],
                dimmed: !snapshot.active,
            });
        }
        self.proxy
            .conn_last
            .retain(|id, _| rates.contains_key(id));
        self.proxy.conn_table.update(cx, |state, _| {
            state.delegate_mut().rows = rows;
        });
    }

    /// “连接”标签：表头（活跃 / 已关闭 / 过滤）+ 官方 DataTable。
    /// 行数据在 delegate 里（心跳刷新），渲染只摆架子；
    /// DataTable 自己管虚拟滚动和双滚动条，外面包个定界 div 即可。
    fn render_proxy_conns(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let Some(registry) = self.proxy.conns.clone() else {
            // 没启动过，表都不存在：提示先启动，别摆空表。
            return div()
                .flex()
                .flex_1()
                .items_center()
                .justify_center()
                .text_size(px(TEXT_MD))
                .text_color(muted)
                // “暂无连接，先启动代理”
                .child(t("app.proxy_conns_empty"))
                .into_any_element();
        };
        let snapshots = registry
            .lock()
            .map(|registry| registry.snapshot())
            .unwrap_or_default();
        let live = snapshots.iter().filter(|s| s.active).count();
        let dead = snapshots.len() - live;
        div()
            .flex()
            .flex_1()
            .min_h_0()
            .flex_col()
            .gap(px(8.))
            .child(
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap(px(8.))
                    .child(Self::conn_filter_button(
                        "proxy-conns-show-active",
                        // “活跃 N”
                        format!("{} {live}", t("app.proxy_active")),
                        self.proxy.show_active,
                        cx,
                    ))
                    .child(Self::conn_filter_button(
                        "proxy-conns-show-closed",
                        // “已关闭 N”
                        format!("{} {dead}", t("app.proxy_closed")),
                        self.proxy.show_closed,
                        cx,
                    ))
                    .child(
                        Button::new("proxy-conns-kill")
                            // “断开全部”
                            .label(t("app.proxy_kill_all"))
                            .small()
                            .outline()
                            .cursor_pointer()
                            .on_click(cx.listener(|this, _, _, cx| {
                                if let Some(registry) = this.proxy.conns.clone() {
                                    if let Ok(mut registry) = registry.lock() {
                                        registry.kill_all();
                                    }
                                    // 举旗立刻读一轮，不用等 1 秒心跳。
                                    this.refresh_conn_table(cx);
                                    cx.notify();
                                }
                            })),
                    )
                    .child(
                        Button::new("proxy-conns-clear")
                            // “清除已关闭”
                            .label(t("app.proxy_clear_closed"))
                            .small()
                            .outline()
                            .cursor_pointer()
                            .on_click(cx.listener(|this, _, _, cx| {
                                if let Some(registry) = this.proxy.conns.clone() {
                                    if let Ok(mut registry) = registry.lock() {
                                        registry.clear_closed();
                                    }
                                    this.refresh_conn_table(cx);
                                    cx.notify();
                                }
                            })),
                    )
                    .child(div().flex_1())
                    .child(
                        div().w(px(220.)).child(
                            Input::new(&self.proxy.conn_filter)
                                .cleanable(true)
                                .small()
                                .w_full(),
                        ),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .child(
                        DataTable::new(&self.proxy.conn_table)
                            .scrollbar_visible(true, true)
                            .small(),
                    ),
            )
            .into_any_element()
    }

    /// 连接表过滤开关：开高亮 primary，关灰 outline，点即翻转重绘。
    fn conn_filter_button(
        id: &'static str,
        label: String,
        on: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let mut button = Button::new(id)
            .label(label)
            .small()
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, _, cx| {
                if id == "proxy-conns-show-active" {
                    this.proxy.show_active = !this.proxy.show_active;
                } else {
                    this.proxy.show_closed = !this.proxy.show_closed;
                }
                cx.notify();
            }));
        button = if on { button.primary() } else { button.outline() };
        button
    }

    /// “代理”标签：开关行 + 节点卡片网格（面板站风格：名 + 延迟）。
    /// 整列能压到 0 高，里面的滚动区才有机会滚而不是把页面撑爆。
    fn render_proxy_main(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_1()
            .min_h_0()
            .flex_col()
            .gap(px(12.))
            .child(self.render_proxy_controls(cx))
            .child(self.render_proxy_grid(cx))
    }

    /// 订阅行：“订阅地址”标签 + 地址框 + 更新按钮。
    fn render_proxy_sub(&self, cx: &mut Context<Self>) -> impl IntoElement {
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
                    // “订阅地址”
                    .child(t("app.proxy_sub")),
            )
            .child(
                div()
                    .flex_1()
                    .child(Input::new(&self.proxy.sub).cleanable(true).small().w_full()),
            )
            .child(
                Button::new("proxy-refresh")
                    // “更新订阅”
                    .label(t("app.proxy_refresh"))
                    .small()
                    .primary()
                    .cursor_pointer()
                    .on_click(cx.listener(|this, _, _, cx| {
                        if this.proxy_occupied() {
                            return;
                        }
                        let url = this.proxy.sub.read(cx).value().to_string();
                        this.proxy.busy = true;
                        // “正在拉取订阅…”
                        this.proxy.status = Some(t("app.proxy_fetching"));
                        cx.notify();
                        // 拉取 + 解析 + 入库全扔后台（8 万条规则解析入库要几秒），
                        // 回来再写 entity（窗口关了就静默丢掉）。
                        cx.spawn(async move |this, cx| {
                            let fetched = cx
                                .background_executor()
                                .spawn(async move {
                                    let text =
                                        crate::tools::proxy::fetch_subscription(&url)?;
                                    let sub =
                                        crate::tools::proxy::parse_subscription(&text);
                                    crate::settings::set_proxy_subscription(&sub)?;
                                    Ok::<_, String>(sub)
                                })
                                .await;
                            cx.update(|cx| {
                                let _ = this.update(cx, |view, cx| {
                                    view.proxy.busy = false;
                                    match fetched {
                                        Ok(sub) => {
                                            let count = sub.nodes.len();
                                            // 整份订阅已在后台存库：下次启动直接恢复，不用再拉。
                                            view.proxy.subscription = sub;
                                            view.proxy.refresh_auto_sizes();
                                            if count == 0 {
                                                // “订阅里没有 VLESS 节点”
                                                view.proxy.status =
                                                    Some(t("app.proxy_empty"));
                                            } else {
                                                let addr =
                                                    view.proxy.sub.read(cx).value().to_string();
                                                if let Err(e) =
                                                    crate::settings::set_proxy_sub_url(&addr)
                                                {
                                                    eprintln!("存订阅地址失败: {e}");
                                                }
                                                // “节点已加载：N”。延迟不自动测，
                                                // 等用户点各组头的测速按钮（订阅 4MB，自动测太吵）。
                                                view.proxy.status = Some(format!(
                                                    "{}：{}",
                                                    t("app.proxy_fetched"),
                                                    count
                                                ));
                                                view.proxy.latencies.clear();
                                                // 各组选中：库里有就恢复（非法值会被过滤），
                                                // 没有就用各组第一顺位。
                                                let saved = crate::settings::proxy_selections()
                                                    .unwrap_or_default();
                                                view.proxy.selections =
                                                    crate::tools::proxy::restore_selections(
                                                        &view.proxy.subscription,
                                                        &saved,
                                                    );
                                                // 订阅都换了，活表必须重建（开着才有活表）。
                                                view.refresh_live_routing();
                                            }
                                        }
                                        Err(e) => view.proxy.status = Some(e),
                                    }
                                    cx.notify();
                                });
                            });
                        })
                        .detach();
                    })),
            )
    }

    /// “规则”标签：分流查询 + 手动规则 + 自动规则，一列可滚到底。
    /// 规则页不包外层滚动：查询和手动区定高，自动列表 `flex_1` 自己滚；
    /// 外面再套一层滚，虚拟列表会被撑成 8 万行全高，白虚拟了。
    fn render_proxy_rules(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_1()
            .min_h_0()
            .flex_col()
            .gap(px(12.))
            .child(self.render_lookup(cx))
            .child(self.render_manual_rules(cx))
            .child(self.render_auto_rules(cx))
            .into_any_element()
    }

    /// 分流查询：输入域名或 IP，看它命中哪条规则、最终去哪。
    /// 8 万条全扫几毫秒，直接 UI 线程算，不用开后台。
    fn render_lookup(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let fg = cx.theme().foreground;
        let mut column = div().flex().flex_col().gap(px(6.));
        column = column.child(
            div()
                .flex()
                .flex_none()
                .items_center()
                .gap(px(8.))
                .child(
                    div()
                        .flex_1()
                        .child(Input::new(&self.proxy.lookup_input).cleanable(true).small().w_full()),
                )
                .child(
                    Button::new("proxy-lookup-go")
                        // “查询”
                        .label(t("app.proxy_lookup_go"))
                        .small()
                        .primary()
                        .cursor_pointer()
                        .on_click(cx.listener(|this, _, _, cx| {
                            let host = this.proxy.lookup_input.read(cx).value().to_string();
                            if host.trim().is_empty() {
                                return;
                            }
                            let (hits, route) = crate::tools::proxy::lookup_route(
                                &this.proxy.subscription,
                                &this.proxy.manual_rules,
                                &this.proxy.selections,
                                &this.proxy.latencies,
                                &host,
                            );
                            this.proxy.lookup_result = Some(LookupOutcome {
                                host: host.trim().to_string(),
                                hits,
                                route,
                            });
                            cx.notify();
                        })),
                ),
        );
        // 查询结果：首行是赢家去向，下面是命中的规则（手动标出来）。
        if let Some(outcome) = &self.proxy.lookup_result {
            let route_text = match &outcome.route {
                // “直连” / “拦截”
                crate::tools::proxy::Route::Direct => t("app.proxy_route_direct"),
                crate::tools::proxy::Route::Reject => t("app.proxy_route_reject"),
                crate::tools::proxy::Route::Node(node) => node.name.clone(),
            };
            column = column.child(
                div()
                    .text_size(px(TEXT_MD))
                    .text_color(fg)
                    .child(format!("{} → {route_text}", outcome.host)),
            );
            // 命中太多只看前 20 条，赢家本来就在第一条。
            column = column.child(div().flex().flex_col().gap(px(2.)).children(
                outcome.hits.iter().take(20).enumerate().map(|(ix, hit)| {
                    // “手动” / “自动”
                    let source = if hit.manual {
                        t("app.proxy_tag_manual")
                    } else {
                        t("app.proxy_tag_auto")
                    };
                    let color = if ix == 0 { fg } else { muted };
                    div()
                        .text_size(px(TEXT_SM))
                        .text_color(color)
                        .child(format!(
                            "[{}] {source} {} → {}",
                            hit.index, hit.text, hit.target
                        ))
                }),
            ));
        }
        column
    }

    /// 手动规则类型文案：chip、行里、弹框里共用，不各写一套。
    fn manual_kind_label(kind: crate::tools::proxy::ManualKind) -> String {
        match kind {
            // “后缀” / “精确” / “关键字” / “网段”
            crate::tools::proxy::ManualKind::Suffix => t("app.proxy_kind_suffix"),
            crate::tools::proxy::ManualKind::Exact => t("app.proxy_kind_exact"),
            crate::tools::proxy::ManualKind::Keyword => t("app.proxy_kind_keyword"),
            crate::tools::proxy::ManualKind::Cidr => t("app.proxy_kind_cidr"),
        }
    }

    /// 手动规则：标题 + 独立添加按钮，下面是行（排序 + 上移下移删除）。
    /// 添加表单搬进弹框（按钮在标题右边），主界面只留管理行。
    /// 手动整体排在自动前面，更新订阅只换自动段，手动躺独立库 key 里不动。
    fn render_manual_rules(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let count = self.proxy.manual_rules.len();
        let mut column = div().flex().flex_col().gap(px(6.));
        column = column.child(
            div()
                .flex()
                .flex_none()
                .items_center()
                .gap(px(8.))
                .child(
                    div()
                        .flex_1()
                        .text_size(px(TEXT_MD))
                        .text_color(muted)
                        // “手动规则”
                        .child(format!("{} ({count})", t("app.proxy_manual_title"))),
                )
                .child(
                    Button::new("proxy-rule-add")
                        // “添加”
                        .label(t("app.proxy_rule_add"))
                        .small()
                        .primary()
                        .cursor_pointer()
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.open_rule_dialog(window, cx);
                        })),
                ),
        );
        if self.proxy.manual_rules.is_empty() {
            // “暂无手动规则”
            column = column.child(
                div()
                    .text_size(px(TEXT_SM))
                    .text_color(muted)
                    .child(t("app.proxy_rule_empty")),
            );
        }
        // 手动行一般就几条，封顶 220px，超了内部滚，不把整页撑爆。
        column = column.child(
            div()
                .max_h(px(220.))
                .overflow_y_scrollbar()
                .child(div().flex().flex_col().gap(px(2.)).children(
                    self.proxy.manual_rules.iter().enumerate().map(|(ix, rule)| {
                let id = rule.id.clone();
                let up_id = rule.id.clone();
                let down_id = rule.id.clone();
                // “排序”
                let sort_text = format!("{} {}", t("app.proxy_sort"), rule.sort);
                let kind_text = Self::manual_kind_label(rule.kind);
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(
                        div()
                            .flex_none()
                            .w(px(72.))
                            .text_size(px(TEXT_SM))
                            .text_color(muted)
                            .child(sort_text),
                    )
                    .child(
                        div()
                            .flex_none()
                            .w(px(52.))
                            .text_size(px(TEXT_SM))
                            .text_color(muted)
                            .child(kind_text),
                    )
                    .child(
                        div()
                            .flex_1()
                            .text_size(px(TEXT_MD))
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .child(format!("{} → {}", rule.pattern, rule.target)),
                    )
                    .child(
                        Button::new(ElementId::named_usize("proxy-mup", ix))
                            // “上移”
                            .label(t("app.proxy_move_up"))
                            .small()
                            .outline()
                            .cursor_pointer()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.move_manual_rule(&up_id, -1);
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new(ElementId::named_usize("proxy-mdown", ix))
                            // “下移”
                            .label(t("app.proxy_move_down"))
                            .small()
                            .outline()
                            .cursor_pointer()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.move_manual_rule(&down_id, 1);
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new(ElementId::named_usize("proxy-mdel", ix))
                            // “删除”
                            .label(t("app.proxy_delete"))
                            .small()
                            .outline()
                            .cursor_pointer()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.proxy.manual_rules.retain(|r| r.id != id);
                                this.save_manual_rules();
                                this.proxy.refresh_auto_sizes();
                                cx.notify();
                            })),
                    )
            }),
                )));
        column
    }

    /// 有效规则：手动在前、自动跟后，每行打来源，一眼看出自动还是手动。
    /// 只读，跟着订阅和手动表走；全量走虚拟列表，只渲染看得见的行。
    fn render_auto_rules(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let count = self.proxy.auto_sizes.len();
        // “有效规则”
        let title = format!("{} ({count})", t("app.proxy_effective_title"));
        // “手动在前，按优先级排列”
        let note = t("app.proxy_effective_note");
        div()
            .flex()
            .flex_1()
            .min_h_0()
            .flex_col()
            .gap(px(6.))
            .child(
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap(px(8.))
                    .child(div().text_size(px(TEXT_MD)).text_color(muted).child(title))
                    .child(div().text_size(px(TEXT_SM)).text_color(muted).child(note)),
            )
            .child(self.render_auto_list(cx))
    }

    /// 自动规则虚拟列表：行高固定 `AUTO_ROW_H`，行数和规则表同长
    /// （`refresh_auto_sizes` 保证，拉取回来就重建）。
    /// 闭包是 `'static` 的，数据走 `view` 参数拿，不直接捕获。
    fn render_auto_list(&self, cx: &mut Context<Self>) -> impl IntoElement {
        if self.proxy.subscription.rules.is_empty() {
            return div()
                .text_size(px(TEXT_SM))
                .text_color(cx.theme().muted_foreground)
                // “暂无自动规则，先更新订阅”
                .child(t("app.proxy_auto_empty"))
                .into_any_element();
        }
        // 列表自己滚（`track_scroll`），滚动条显式叠一层：
        // VirtualList 不自带可见滚动条，不叠这层就只能盲滚。
        div()
            .flex()
            .flex_1()
            .min_h_0()
            .relative()
            .child(
                v_virtual_list(
                    cx.entity().clone(),
                    "proxy-auto-rules",
                    self.proxy.auto_sizes.clone(),
            |view, visible_range, _, cx| {
                let muted = cx.theme().muted_foreground;
                let link = cx.theme().link;
                visible_range
                    .filter_map(|ix| {
                        let (manual, text, target) = view.proxy.effective_row(ix)?;
                        // “手动”蓝字标出来，自动灰字，一眼区分来源。
                        let (source, color) = if manual {
                            (t("app.proxy_tag_manual"), link)
                        } else {
                            (t("app.proxy_tag_auto"), muted)
                        };
                        Some(
                            div()
                                .w_full()
                                .h(px(AUTO_ROW_H))
                                .flex()
                                .items_center()
                                .gap(px(6.))
                                .text_size(px(TEXT_SM))
                                .child(
                                    div()
                                        .flex_none()
                                        .text_color(muted)
                                        .child(format!("[{ix}]")),
                                )
                                .child(
                                    div().flex_none().text_color(color).child(source),
                                )
                                .child(
                                    div()
                                        .flex_1()
                                        .text_color(muted)
                                        .overflow_hidden()
                                        .whitespace_nowrap()
                                        .text_ellipsis()
                                        .child(format!("{text} → {target}")),
                                )
                                .into_any_element(),
                        )
                    })
                    .collect::<Vec<AnyElement>>()
            },
                )
                .track_scroll(&self.proxy.auto_scroll)
                .flex_1()
                .min_h_0(),
            )
            .child(
                div().absolute().top_0().left_0().right_0().bottom_0().child(
                    Scrollbar::vertical(&self.proxy.auto_scroll)
                        .axis(ScrollbarAxis::Vertical),
                ),
            )
            .into_any_element()
    }

    /// 打开添加规则弹框：类型 chip + 内容 + 目标下拉 + 确定/取消。
    /// 目标下拉是官方 Select：选项分组在前（订阅顺序），再跟 DIRECT / REJECT。
    /// 下拉状态每次打开按当前订阅重建，旧的不留（分组刚换过也不怕）。
    /// 内容框 entity 常驻 view，弹框只是换个地方渲染它；确定走正常添加流程
    /// （校验不过关弹框照关，错误写主界面状态行）。
    fn open_rule_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // 选项分组在前：先分组（订阅顺序），再跟 DIRECT / REJECT。
        let mut options: Vec<String> = self
            .proxy
            .subscription
            .groups
            .iter()
            .map(|group| group.name.clone())
            .collect();
        options.push("DIRECT".to_string());
        options.push("REJECT".to_string());
        let select = cx.new(|cx| SelectState::new(options, None, window, cx));
        self.proxy.target_select = Some(select);
        let view = cx.entity().downgrade();
        let pattern = self.proxy.manual_pattern.clone();
        window.open_dialog(cx, move |dialog, _, cx| {
            let active = view
                .update(cx, |this, _| this.proxy.manual_kind)
                .unwrap_or(crate::tools::proxy::ManualKind::Suffix);
            dialog
                // “添加手动规则”
                .title(t("app.proxy_add_title"))
                .w(px(520.))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(8.))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(6.))
                                .children(
                                    crate::tools::proxy::ManualKind::ALL
                                        .iter()
                                        .enumerate()
                                        .map(|(ix, kind)| {
                                            let kind = *kind;
                                            let current = active == kind;
                                            let pick = view.clone();
                                            let mut button = Button::new(
                                                ElementId::named_usize("proxy-dkind", ix),
                                            )
                                            .label(Self::manual_kind_label(kind))
                                            .small()
                                            .cursor_pointer();
                                            button = if current {
                                                button.primary()
                                            } else {
                                                button.outline()
                                            };
                                            button.on_click(move |_, _, cx| {
                                                let _ = pick.update(cx, |this, cx| {
                                                    this.proxy.manual_kind = kind;
                                                    cx.notify();
                                                });
                                            })
                                        }),
                                ),
                        )
                        .child(Input::new(&pattern).cleanable(true).small().w_full())
                        .child({
                            // 下拉状态存在 view 里，每次重绘都现取，保证选项最新。
                            let select = view
                                .update(cx, |this, _| this.proxy.target_select.clone())
                                .ok()
                                .flatten();
                            match select {
                                Some(state) => Select::new(&state)
                                    // “目标：组名 / DIRECT / REJECT”
                                    .placeholder(t("app.proxy_target_hint"))
                                    .small()
                                    .w_full()
                                    .into_any_element(),
                                None => div().into_any_element(),
                            }
                        }),
                )
                .footer({
                    let confirm = view.clone();
                    DialogFooter::new()
                        .child(
                            Button::new("proxy-add-cancel")
                                .outline()
                                // “取消”
                                .label(t("app.proxy_cancel"))
                                .on_click(|_, window, cx| {
                                    window.close_dialog(cx);
                                }),
                        )
                        .child(
                            Button::new("proxy-add-ok")
                                .primary()
                                // “确定”
                                .label(t("app.proxy_confirm"))
                            .on_click(move |_, window, cx| {
                                let _ = confirm.update(cx, |this, cx| {
                                    // 确定时读下拉框的选中值，不经过中间字段。
                                    let target = this
                                        .proxy
                                        .target_select
                                        .as_ref()
                                        .and_then(|state| {
                                            state.read(cx).selected_value().cloned()
                                        });
                                    this.add_manual_rule(target, cx);
                                });
                                window.close_dialog(cx);
                            }),
                        )
                })
        });
    }

    /// 手动规则持久化：整表覆盖写，失败记一笔日志（界面上的表照样用）。
    fn save_manual_rules(&mut self) {
        if let Err(e) = crate::settings::set_proxy_manual_rules(&self.proxy.manual_rules) {
            eprintln!("存手动规则失败: {e}");
        }
    }

    /// 添加手动规则：两空校验 + 目标归属校验 + 网段格式校验，
    /// 序号取最小减一（新规则置顶）。目标直接传下拉框的选中值。
    fn add_manual_rule(&mut self, target: Option<String>, cx: &mut Context<Self>) {
        let pattern = self.proxy.manual_pattern.read(cx).value().to_string();
        let target = target.unwrap_or_default();
        if pattern.trim().is_empty() || target.trim().is_empty() {
            // “内容和目标都要填”
            self.proxy.status = Some(t("app.proxy_need_rule"));
            cx.notify();
            return;
        }
        // 下拉只给合法选项，但订阅可能刚换过：目标不在分组里、
        // 又不是关键字就拒掉，免得存条永远命中不了的规则。
        let known = target.eq_ignore_ascii_case("DIRECT")
            || target.eq_ignore_ascii_case("REJECT")
            || self
                .proxy
                .subscription
                .groups
                .iter()
                .any(|group| group.name == target);
        if !known {
            // “目标不在分组里”
            self.proxy.status = Some(t("app.proxy_bad_target"));
            cx.notify();
            return;
        }
        if self.proxy.manual_kind == crate::tools::proxy::ManualKind::Cidr
            && crate::tools::proxy::CidrBlock::parse(pattern.trim()).is_none()
        {
            // “网段格式不对，如 10.0.0.0/8”
            self.proxy.status = Some(t("app.proxy_bad_cidr"));
            cx.notify();
            return;
        }
        let sort = self
            .proxy
            .manual_rules
            .iter()
            .map(|rule| rule.sort)
            .min()
            .unwrap_or(1)
            - 1;
        self.proxy.manual_rules.push(crate::tools::proxy::ManualRule {
            id: next_manual_id(),
            kind: self.proxy.manual_kind,
            pattern: pattern.trim().to_string(),
            target: target.trim().to_string(),
            sort,
        });
        self.proxy.manual_rules.sort_by_key(|rule| rule.sort);
        self.save_manual_rules();
        // 条数变了，有效列表重建行尺寸。
        self.proxy.refresh_auto_sizes();
        cx.notify();
    }

    /// 手动规则上移 / 下移：和邻居换序号，持久化。顶和底再点没反应。
    fn move_manual_rule(&mut self, id: &str, delta: isize) {
        let mut order: Vec<usize> = (0..self.proxy.manual_rules.len()).collect();
        order.sort_by_key(|&ix| self.proxy.manual_rules[ix].sort);
        let Some(pos) = order.iter().position(|&ix| self.proxy.manual_rules[ix].id == id) else {
            return;
        };
        let len = order.len() as isize;
        let target = (pos as isize + delta).clamp(0, len - 1) as usize;
        if target == pos {
            return;
        }
        let (a, b) = (order[pos], order[target]);
        let (sort_a, sort_b) = (
            self.proxy.manual_rules[a].sort,
            self.proxy.manual_rules[b].sort,
        );
        self.proxy.manual_rules[a].sort = sort_b;
        self.proxy.manual_rules[b].sort = sort_a;
        self.proxy.manual_rules.sort_by_key(|rule| rule.sort);
        self.save_manual_rules();
        // 顺序变了行号跟着变，有效列表重建（条数不变，重建只是图个齐）。
        self.proxy.refresh_auto_sizes();
    }

    /// 节点网格：有分组按订阅的组分节（组头 + 成员卡片，面板站同款），
    /// 没分组退化成扁平列表。成员三种卡：节点卡（可点可测）、关键字卡
    /// （`DIRECT` / `REJECT`，纯展示）、嵌套组卡（纯展示）。
    fn render_proxy_grid(&self, cx: &mut Context<Self>) -> impl IntoElement {
        if self.proxy.subscription.groups.is_empty() {
            return self.render_proxy_flat_grid(cx).into_any_element();
        }
        // 卡片 id 要全局唯一：组号和组内序号拼起来（节点和组都远少于一万）。
        const ID_SPAN: usize = 10_000;
        let mut sections = Vec::new();
        for (gi, group) in self.proxy.subscription.groups.iter().enumerate() {
            let members =
                crate::tools::proxy::resolve_members(&self.proxy.subscription, group);
            // 没节点成员的组不给测速按钮（关键字组点它没意义）。
            let has_nodes = members.iter().any(|member| {
                matches!(member, crate::tools::proxy::GroupMember::Node(_))
            });
            let cards: Vec<AnyElement> = members
            .iter()
            .enumerate()
            .map(|(mi, member)| match member {
                crate::tools::proxy::GroupMember::Node(node) => self
                    .render_node_card(
                        node,
                        ElementId::named_usize("proxy-node", gi * ID_SPAN + mi),
                        Some(group.name.clone()),
                        cx,
                    )
                    .into_any_element(),
                crate::tools::proxy::GroupMember::Keyword(word) => {
                    let picked = self
                        .proxy
                        .selections
                        .get(&group.name)
                        .is_some_and(|choice| choice == word);
                    Self::render_tag_card(
                        word.to_string(),
                        ElementId::named_usize("proxy-node", gi * ID_SPAN + mi),
                        group.name.clone(),
                        picked,
                        cx,
                    )
                    .into_any_element()
                }
                crate::tools::proxy::GroupMember::Group(name) => {
                    let picked = self
                        .proxy
                        .selections
                        .get(&group.name)
                        .is_some_and(|choice| choice == name);
                    Self::render_tag_card(
                        name.to_string(),
                        ElementId::named_usize("proxy-node", gi * ID_SPAN + mi),
                        group.name.clone(),
                        picked,
                        cx,
                    )
                    .into_any_element()
                }
            })
            .collect();
            if cards.is_empty() {
                continue;
            }
            // 组头：组名 + 类型 + 张数（类型为空就不占位置），
            // 右边是本组测速按钮（只测组内节点）。
            let mut header = format!("{} ({})", group.name, cards.len());
            if !group.kind.is_empty() {
                header = format!("{} · {} ({})", group.name, group.kind, cards.len());
            }
            let muted = cx.theme().muted_foreground;
            let group_name = group.name.clone();
            sections.push(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .child(
                        div()
                            .flex()
                            .flex_none()
                            .items_center()
                            .gap(px(8.))
                            .child(
                                div()
                                    .flex_1()
                                    .text_size(px(TEXT_MD))
                                    .text_color(muted)
                                    .child(header),
                            )
                            .children(has_nodes.then(|| {
                                Button::new(ElementId::named_usize("proxy-group-test", gi))
                                    // “测速”
                                    .label(t("app.proxy_speedtest"))
                                    .small()
                                    .outline()
                                    .cursor_pointer()
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.start_group_speedtest(group_name.clone(), cx);
                                    }))
                                    .into_any_element()
                            })),
                    )
                    .child(div().flex().flex_wrap().gap(px(8.)).children(cards))
                    .into_any_element(),
            );
        }
        div()
            .flex()
            .flex_1()
            .min_h_0()
            .overflow_y_scrollbar()
            .child(div().flex().flex_col().gap(px(12.)).children(sections))
            .into_any_element()
    }

    /// 没分组的订阅退化成扁平列表（老样子）。
    fn render_proxy_flat_grid(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let count = self.proxy.subscription.nodes.len();
        let cards: Vec<AnyElement> = self
            .proxy
            .subscription
            .nodes
            .iter()
            .enumerate()
            .map(|(ix, node)| {
                self.render_node_card(
                    node,
                    ElementId::named_usize("proxy-node", ix),
                    None,
                    cx,
                )
                .into_any_element()
            })
            .collect();
        div()
            .flex()
            .flex_1()
            .flex_col()
            .gap(px(6.))
            .child(
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .child(
                        div()
                            .text_size(px(TEXT_MD))
                            .text_color(muted)
                            // “节点”
                            .child(format!("{} ({count})", t("app.proxy_nodes"))),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scrollbar()
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .gap(px(8.))
                            .children(cards),
                    ),
            )
            .into_any_element()
    }

    /// 节点卡：左名（超长省略）右延迟，选中铺底色描边。
    /// `group` 有值时高亮走该组的独立选中；扁平页没有组，不高亮。
    fn render_node_card(
        &self,
        node: &crate::tools::proxy::ProxyNode,
        id: ElementId,
        group: Option<String>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let fg = cx.theme().foreground;
        let border = cx.theme().border;
        let link = cx.theme().link;
        let row_bg = cx.theme().sidebar_accent;
        let green = cx.theme().green;
        let blue = cx.theme().blue;
        let red = cx.theme().red;
        let selected = match &group {
            // 组内高亮只看本组的选择，别组的不掺和。
            Some(name) => self
                .proxy
                .selections
                .get(name)
                .is_some_and(|picked| *picked == node.name),
            // 扁平列表没有组，选中态退化成“隧道目标”？不——分流后没有
            // 全局目标，扁平页点卡只记持久化里的首选（给自检用），不高亮。
            None => false,
        };
        // 选中铺底色描边。运行中不单独标：分流后每条连接去向都可能不一样，
        // 单卡标“在用”是误导。
        let (bg, edge) = if selected {
            (row_bg, link)
        } else {
            (hsla(0., 0., 0., 0.), border)
        };
        let latency = self.proxy.latencies.get(&node.name).copied();
        let (tone_text, tone_color) = match latency {
            Some(Some(ms)) => {
                let color = match crate::tools::proxy::latency_tone(Some(ms)) {
                    crate::tools::proxy::LatencyTone::Fast => green,
                    crate::tools::proxy::LatencyTone::Normal => blue,
                    _ => red,
                };
                (ms.to_string(), color)
            }
            // 测出来是超时：灰字“超时”。
            Some(None) => (t("app.proxy_timeout"), muted),
            // 还没测：灰色横线占位。
            None => ("–".to_string(), muted),
        };
        let name = node.name.clone();
        let select_name = node.name.clone();
        div()
            .id(id)
            .flex_none()
            .w(px(232.))
            .px(px(10.))
            .py(px(8.))
            .rounded(px(6.))
            .border_1()
            .border_color(edge)
            .bg(bg)
            .cursor_pointer()
            .hover(move |s| {
                if selected {
                    s
                } else {
                    s.bg(row_bg)
                }
            })
                            .on_click(cx.listener(move |this, _, _, cx| {
                                // 组内点卡只记本组，不动别的组；扁平页没有组，不记
                                // （分流只认组选择）。整表持久化。
                                if let Some(name) = group.clone() {
                                    this.proxy.selections.insert(name, select_name.clone());
                                    if let Err(e) = crate::settings::set_proxy_selections(
                                        &this.proxy.selections,
                                    ) {
                                        eprintln!("存分组选中失败: {e}");
                                    }
                                }
                // 活表即时换新：下个连接直接用新的，已建连的不动，
                // 重连提示从此退休。
                this.refresh_live_routing();
                cx.notify();
            }))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .child(
                        div()
                            .flex_1()
                            .text_size(px(TEXT_MD))
                            .text_color(fg)
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .child(name),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_size(px(TEXT_SM))
                            .text_color(tone_color)
                            .child(tone_text),
                    ),
            )
    }

    /// 展示卡：关键字（`DIRECT` / `REJECT`）和嵌套组名用它，可点，
    /// 点即把该组切到它（分流认这个选择，照样持久化）。
    fn render_tag_card(
        label: String,
        id: ElementId,
        group: String,
        selected: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let link = cx.theme().link;
        let border = cx.theme().border;
        let row_bg = cx.theme().sidebar_accent;
        let (bg, edge) = if selected {
            (row_bg, link)
        } else {
            (hsla(0., 0., 0., 0.), border)
        };
        // 闭包是 `Fn`（可能点多次），点的目标提前克隆出来，
        // `label` 本体留给下面的文字。
        let pick_group = group.clone();
        let pick_label = label.clone();
        div()
            .id(id)
            .flex_none()
            .w(px(232.))
            .px(px(10.))
            .py(px(8.))
            .rounded(px(6.))
            .border_1()
            .border_color(edge)
            .bg(bg)
            .cursor_pointer()
            .hover(move |s| {
                if selected {
                    s
                } else {
                    s.bg(row_bg)
                }
            })
            .on_click(cx.listener(move |this, _, _, cx| {
                this.proxy
                    .selections
                    .insert(pick_group.clone(), pick_label.clone());
                if let Err(e) = crate::settings::set_proxy_selections(&this.proxy.selections) {
                    eprintln!("存分组选中失败: {e}");
                }
                // 活表即时换新：下个连接直接用新的，已建连的不动。
                this.refresh_live_routing();
                cx.notify();
            }))
            .child(
                div()
                    .text_size(px(TEXT_MD))
                    .text_color(muted)
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .child(label),
            )
    }

    /// 开关行：启动/断开 + 测速 + 本地入口；开着显示运行信息。
    fn render_proxy_controls(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let fg = cx.theme().foreground;
        let row = div().flex().flex_none().items_center().gap(px(8.));
        let row = row
            .child(
                Button::new("proxy-speedtest")
                    // “测速”
                    .label(t("app.proxy_speedtest"))
                    .small()
                    .outline()
                    .cursor_pointer()
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.start_speedtest(cx);
                    })),
            )
            .child(
                Button::new("proxy-selfcheck")
                    // “自检”
                    .label(t("app.proxy_selfcheck"))
                    .small()
                    .outline()
                    .cursor_pointer()
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.start_selfcheck(cx);
                    })),
            );
        match &self.proxy.running {
            Some(_) => {
                // 分流后没有唯一的“在用节点”，报组数 + 节点数 + 入口。
                let info = format!(
                    "{} · {}组 · {}节点 · {}",
                    t("app.proxy_running"),
                    self.proxy.subscription.groups.len(),
                    self.proxy.subscription.nodes.len(),
                    crate::tools::proxy::mixed_addr()
                );
                row.child(
                    Button::new("proxy-stop")
                        // “断开”
                        .label(t("app.proxy_stop"))
                        .small()
                        .outline()
                        .cursor_pointer()
                        .on_click(cx.listener(|this, _, _, cx| {
                            if let Some(handle) = this.proxy.running.take() {
                                handle.stop();
                            }
                            // 活表一起扔：下次启动建新的，旧快照不复活。
                            this.proxy.live = None;
                            match crate::tools::proxy::disable_system_proxy() {
                                // “已断开”
                                Ok(()) => {
                                    if let Err(e) = crate::settings::set_proxy_running(false) {
                                        eprintln!("落隧道旗失败: {e}");
                                    }
                                    this.proxy.status = Some(t("app.proxy_stopped"))
                                }
                                Err(e) => this.proxy.status = Some(e),
                            }
                            cx.notify();
                        })),
                )
                .child(div().text_size(px(TEXT_MD)).text_color(fg).child(info))
            }
            None => row
                .child(
                    Button::new("proxy-start")
                        // “启动代理”
                        .label(t("app.proxy_start"))
                        .small()
                        .primary()
                        .cursor_pointer()
                        .on_click(cx.listener(|this, _, _, cx| {
                            if this.proxy_occupied() {
                                return;
                            }
                            if this.proxy.subscription.nodes.is_empty() {
                                // “先更新订阅并选择节点”
                                this.proxy.status = Some(t("app.proxy_need_nodes"));
                                cx.notify();
                                return;
                            };
                            // 活分流表在点击瞬间建好：建索引（8 万条后缀排序）
                            // 加占端口，几十毫秒，按钮上点一下无感。
                            // 之后改选择直接换表里的内容，新连接即用，
                            // 已建连的手里是旧快照，不受影响。
                            let live = std::sync::Arc::new(std::sync::RwLock::new(
                                crate::tools::proxy::RoutingTable::build(
                                    this.proxy.subscription.clone(),
                                    &this.proxy.manual_rules,
                                    this.proxy.selections.clone(),
                                    this.proxy.latencies.clone(),
                                ),
                            ));
                            // 连接表和活分流表一起建：表是活的（一直记）。
                            let registry = std::sync::Arc::new(std::sync::Mutex::new(
                                crate::tools::proxy::ConnRegistry::default(),
                            ));
                            // 先占端口再开系统代理：端口被占直接报错，不动系统设置。
                            match crate::tools::proxy::start_proxy(
                                live.clone(),
                                registry.clone(),
                                crate::tools::proxy::MIXED_PORT,
                            ) {
                                Ok(handle) => {
                                    match crate::tools::proxy::enable_system_proxy(
                                        crate::tools::proxy::MIXED_PORT,
                                    ) {
                                        Ok(()) => {
                                            // 隧道旗举起来：崩了/被杀了下次启动能认出来收尾。
                                            if let Err(e) = crate::settings::set_proxy_running(true)
                                            {
                                                eprintln!("存隧道旗失败: {e}");
                                            }
                                            this.proxy.running = Some(handle);
                                            this.proxy.conns = Some(registry);
                                            this.proxy.live = Some(live);
                                            // 连接表心跳：1 秒刷一次速度和时间，开着才跳，
                                            // 停了自己退（running 没了就 break）。
                                            cx.spawn(async move |this, cx| {
                                                loop {
                                                    cx.background_executor()
                                                        .timer(std::time::Duration::from_secs(1))
                                                        .await;
                                                    let alive = cx
                                                        .update(|cx| {
                                                            this.update(cx, |view, cx| {
                                                                let on = view.proxy.running.is_some();
                                                                if on {
                                                                    view.refresh_conn_table(cx);
                                                                    cx.notify();
                                                                }
                                                                on
                                                            })
                                                            .unwrap_or(false)
                                                        });
                                                    if !alive {
                                                        break;
                                                    }
                                                }
                                            })
                                            .detach();
                                            // “隧道已建立，系统代理已打开：N组M节点（入口）”
                                            this.proxy.status = Some(format!(
                                                "{}：{}组{}节点（{}）",
                                                t("app.proxy_started"),
                                                this.proxy.subscription.groups.len(),
                                                this.proxy.subscription.nodes.len(),
                                                crate::tools::proxy::mixed_addr()
                                            ));
                                        }
                                        Err(e) => {
                                            handle.stop();
                                            this.proxy.status = Some(e);
                                        }
                                    }
                                }
                                Err(e) => this.proxy.status = Some(e),
                            }
                            cx.notify();
                        })),
                )
                .child(
                    div()
                        .text_size(px(TEXT_SM))
                        .text_color(muted)
                        // “本地入口”
                        .child(format!("{}：{}", t("app.proxy_local"), crate::tools::proxy::mixed_addr())),
                ),
        }
    }
}
