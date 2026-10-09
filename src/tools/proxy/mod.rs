//! 代理工具：Clash 订阅解析、VLESS 直连、系统代理开关。
//!
//! 只支持订阅里的 VLESS + WS + TLS 节点（当前订阅全是这种）：本地在
//! `127.0.0.1:10809` 开混合入口（首字节嗅探：`0x05` 走 SOCKS5，否则按
//! HTTP 代理解析，CONNECT 与明文 GET 都接），出站走 TCP → TLS（SNI 用
//! 节点的 servername，照顾 CDN 前置）→ WebSocket → VLESS。
//! 不捆外部核心（mihomo 之类），纯 Rust 直连；阻塞式线程模型，不引入
//! 新的异步运行时。系统代理走注册表 + `wininet` 刷新，只实现 Windows。
//!
//! 视图在 `view`，逻辑和视图各管各的。

pub mod view;

// 外壳持有代理视图状态，走工具根路径拿，不直接钻 view。
pub(crate) use view::ProxyView;

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::thread::JoinHandle;
use std::time::Duration;

/// 本地混合入口端口：SOCKS5 与 HTTP 共用，选不常用的 10809，避开 10808。
pub const MIXED_PORT: u16 = 10809;

/// 出站建连超时：TCP + TLS + WS 握手共用，墙外 10 秒连不上就当节点死了。
const DIAL_TIMEOUT: Duration = Duration::from_secs(10);
/// 入口嗅探等待：浏览器连上后 5 秒还不发数据，多半是误触，直接关掉。
const PEEK_TIMEOUT: Duration = Duration::from_secs(5);
/// 订阅拉取超时：订阅正文 4MB 级别，20 秒是底线。
const FETCH_TIMEOUT: Duration = Duration::from_secs(20);
/// 中继轮询步长：两边都读超时才空转一轮，10ms 延迟可接受、CPU 可忽略。
const RELAY_POLL: Duration = Duration::from_millis(10);
/// 监听空转步长：非阻塞 accept 捡不到连接就睡一会儿，别空转烧 CPU。
const ACCEPT_IDLE: Duration = Duration::from_millis(50);
/// 单次转发的块大小：VLESS over WS 一帧也就这个量级，再大 WS 帧分片也没收益。
const CHUNK: usize = 16 * 1024;
/// HTTP 请求头上限：代理请求头很小，32KB 还没见到空行就是坏了。
const MAX_HTTP_HEAD: usize = 32 * 1024;

/// 本地混合入口地址（界面显示、系统代理共用，改端口只改这一处）。
pub fn mixed_addr() -> String {
    format!("127.0.0.1:{MIXED_PORT}")
}

/// 订阅里的一个 VLESS 节点：界面列表、持久化、出站拨号都用它。
/// 含 UUID（就是 Clash 订阅的凭证），存库等同 Clash 存本地订阅，
/// 只放本机 `%APPDATA%`，不往任何地方发。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProxyNode {
    /// 订阅里的名称（中文见订阅原文，如“新加坡Oracle5”）。
    pub name: String,
    /// TCP 连的目标（订阅里的 server，常被前置到 CDN）。
    pub server: String,
    pub port: u16,
    pub uuid: String,
    /// TLS SNI（订阅里的 servername，前置场景下和 server 不一样）。
    pub sni: String,
    /// WS 握手的 Host 头（订阅里 ws-opts/headers/Host，缺省跟 sni）。
    pub host: String,
    /// WS 路径（订阅里 ws-opts/path，缺省 "/"）。
    pub path: String,
}

/// 出站目标：域名或 IP + 端口（SOCKS5 / HTTP 解析出来都落到这里）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub host: String,
    pub port: u16,
}

/// 去掉首尾的单双引号：订阅里的 `name:` / `server:` 常带引号。
fn unquote(text: &str) -> String {
    let text = text.trim();
    if text.len() >= 2 {
        let bytes = text.as_bytes();
        let (first, last) = (bytes[0], bytes[bytes.len() - 1]);
        if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
            return text[1..text.len() - 1].to_string();
        }
    }
    text.to_string()
}

/// 订阅里的一个原始条目：先攒字段，最后只留下 `type: vless` 的。
#[derive(Debug, Default)]
struct RawNode {
    name: String,
    node_type: String,
    server: String,
    port: u16,
    uuid: String,
    servername: String,
    ws_host: String,
    ws_path: String,
}

impl RawNode {
    /// 凑成可用的节点：非 VLESS 返回 None（调用方直接丢掉）。
    fn into_proxy(self) -> Option<ProxyNode> {
        if self.node_type != "vless" || self.name.is_empty() || self.uuid.is_empty() {
            return None;
        }
        // 端口解析失败就当 443：订阅里 VLESS 全是 443，缺了也照这个连。
        let port = if self.port == 0 { 443 } else { self.port };
        let server = if self.server.is_empty() {
            return None;
        } else {
            self.server
        };
        // SNI 缺了就跟 server：直连场景下两者本来就是一个。
        let sni = if self.servername.is_empty() {
            server.clone()
        } else {
            self.servername
        };
        // Host 缺了就跟 SNI：订阅里两者一般一致。
        let host = if self.ws_host.is_empty() {
            sni.clone()
        } else {
            self.ws_host
        };
        let mut path = if self.ws_path.is_empty() {
            "/".to_string()
        } else {
            self.ws_path
        };
        if !path.starts_with('/') {
            path.insert(0, '/');
        }
        Some(ProxyNode {
            name: self.name,
            server,
            port,
            uuid: self.uuid,
            sni,
            host,
            path,
        })
    }
}

/// 订阅里的一个策略组：名称 + 类型 + 成员名。
/// 成员名三种：节点名、嵌套组名、`DIRECT` / `REJECT` 关键字，
/// 渲染时经 `resolve_members` 落位，对不上的一律丢掉。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProxyGroup {
    /// 组名（中文见订阅原文，如“节点选择”）。
    pub name: String,
    /// 组类型原文（`select` / `url-test` 之类，界面只做展示）。
    pub kind: String,
    pub members: Vec<String>,
}

/// 解析好的订阅：节点 + 策略组 + 规则。组里只存名字，渲染时再落到节点，
/// 这样节点改名只改一处，组不用跟着动。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Subscription {
    pub nodes: Vec<ProxyNode>,
    pub groups: Vec<ProxyGroup>,
    pub rules: Vec<Rule>,
}

/// 订阅里的一条分流规则：匹配器 + 命中后的去向（组名 / `DIRECT` / `REJECT`）。
/// 顺序就是订阅里的顺序，匹配永远从头找第一条命中的（Clash 同款）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rule {
    pub matcher: Matcher,
    pub target: String,
}

/// 规则匹配器。`Unsupported` 是跑在我们这里没意义的类型（`GEOIP` 没库、
/// `PROCESS-NAME` 拿不到进程名）：解析保留原样，匹配时直接跳过。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Matcher {
    Domain(String),
    DomainSuffix(String),
    DomainKeyword(String),
    IpCidr(CidrBlock),
    Match,
    Unsupported(String),
}

/// 一段 CIDR（v4 / v6 通吃）：地址按整数存，掩码按前缀长存，
/// 匹配就是按位与，标准做法。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CidrBlock {
    V4 { net: u32, prefix: u8 },
    V6 { net: u128, prefix: u8 },
}

impl CidrBlock {
    /// 解析 `1.2.3.4/24` / `::1/128`。前缀超界、地址非法都返回 None。
    pub fn parse(text: &str) -> Option<Self> {
        let (addr, prefix) = text.split_once('/')?;
        let prefix: u8 = prefix.trim().parse().ok()?;
        if let Ok(v4) = addr.trim().parse::<std::net::Ipv4Addr>() {
            if prefix > 32 {
                return None;
            }
            // 前缀 0 要特殊处理：u32 左移 32 位在 Rust 里会 panic。
            let mask = if prefix == 0 { 0 } else { u32::MAX << (32 - prefix) };
            return Some(CidrBlock::V4 {
                net: u32::from(v4) & mask,
                prefix,
            });
        }
        if let Ok(v6) = addr.trim().parse::<std::net::Ipv6Addr>() {
            if prefix > 128 {
                return None;
            }
            let mask = if prefix == 0 {
                0
            } else {
                u128::MAX << (128 - prefix)
            };
            return Some(CidrBlock::V6 {
                net: u128::from(v6) & mask,
                prefix,
            });
        }
        None
    }

    /// 目标 host 是字面 IP 才判，域名一律不中（订阅自带 `no-resolve`，
    /// 精神一致：不替用户做本地解析，既省一次 DNS 也不漏查询）。
    pub fn matches(&self, host: &str) -> bool {
        match host.parse::<std::net::IpAddr>() {
            Ok(std::net::IpAddr::V4(v4)) => match self {
                CidrBlock::V4 { net, prefix } => {
                    let mask = if *prefix == 0 {
                        0
                    } else {
                        u32::MAX << (32 - prefix)
                    };
                    u32::from(v4) & mask == *net
                }
                CidrBlock::V6 { .. } => false,
            },
            Ok(std::net::IpAddr::V6(v6)) => match self {
                CidrBlock::V6 { net, prefix } => {
                    let mask = if *prefix == 0 {
                        0
                    } else {
                        u128::MAX << (128 - prefix)
                    };
                    u128::from(v6) & mask == *net
                }
                CidrBlock::V4 { .. } => false,
            },
            Err(_) => false,
        }
    }
}

impl Matcher {
    /// 目标 host 是否命中。大小写不敏感（DNS 本来就不分大小写）。
    pub fn matches(&self, host: &str) -> bool {
        match self {
            Matcher::Domain(domain) => host.eq_ignore_ascii_case(domain),
            Matcher::DomainSuffix(suffix) => {
                let suffix = suffix.trim_start_matches('.');
                host.eq_ignore_ascii_case(suffix)
                    || host.len() > suffix.len()
                        && host[..host.len() - suffix.len()].ends_with('.')
                        && host[host.len() - suffix.len()..].eq_ignore_ascii_case(suffix)
            }
            Matcher::DomainKeyword(keyword) => {
                host.to_ascii_lowercase().contains(&keyword.to_ascii_lowercase())
            }
            Matcher::IpCidr(cidr) => cidr.matches(host),
            Matcher::Match => true,
            // 没法判的直接放行给下一条，不静默吃流量。
            Matcher::Unsupported(_) => false,
        }
    }
}

/// 一条连接的去向：直连、拦截、走指定节点。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    Direct,
    Reject,
    Node(ProxyNode),
}

/// 分流索引：87k 条 `DOMAIN-SUFFIX` 全塞 Vec 线扫太慢，
/// 后缀反转排好序，二分找，单次匹配压到微秒级。
/// 精确域名另建表，剩下的（关键字、网段、MATCH）本来就没几条，直接线扫。
/// 所有索引都记全局规则序号，跨类型拼出来的也是“第一条命中的赢”。
#[derive(Debug, Clone, Default)]
pub struct RuleSet {
    /// （反转后缀，规则序号）：反转如 `com.example`，同串留最小序号。
    suffixes: Vec<(String, usize)>,
    /// 精确域名（小写）→ 规则序号，同名留最小序号。
    exact: HashMap<String, usize>,
}

impl RuleSet {
    /// 从规则表建索引。调用方在拉取 / 启动时调一次，别每连接建一次。
    pub fn build(rules: &[Rule]) -> Self {
        let mut suffixes: HashMap<String, usize> = HashMap::new();
        let mut exact: HashMap<String, usize> = HashMap::new();
        for (index, rule) in rules.iter().enumerate() {
            match &rule.matcher {
                Matcher::DomainSuffix(suffix) => {
                    let key = reverse_labels(suffix);
                    suffixes
                        .entry(key)
                        .and_modify(|kept| *kept = (*kept).min(index))
                        .or_insert(index);
                }
                Matcher::Domain(domain) => {
                    exact
                        .entry(domain.to_ascii_lowercase())
                        .and_modify(|kept| *kept = (*kept).min(index))
                        .or_insert(index);
                }
                _ => {}
            }
        }
        let mut suffixes: Vec<(String, usize)> = suffixes.into_iter().collect();
        suffixes.sort();
        Self { suffixes, exact }
    }

    /// 目标 host 命中哪条规则（返回规则序号）：后缀走二分，精确走表，
    /// 剩下线扫，最后拼起来取最小序号。都没中返回 None。
    pub fn match_index(&self, rules: &[Rule], host: &str) -> Option<usize> {
        // 索引里全是小写，这里统一小写一次（DNS 本来不分大小写）。
        let host = host.to_ascii_lowercase();
        let mut best: Option<usize> = None;
        let mut consider = |index: usize| {
            if best.is_none_or(|kept| index < kept) {
                best = Some(index);
            }
        };
        // 后缀：host 的每个上级域都可能是规则（`a.b.c` 查 `c`、`b.c`、`a.b.c`）。
        let labels: Vec<&str> = host.split('.').collect();
        for start in 0..labels.len() {
            let key = labels[start..].iter().rev().copied().collect::<Vec<_>>().join(".");
            if let Ok(pos) = self
                .suffixes
                .binary_search_by(|probe| probe.0.as_str().cmp(&key))
            {
                consider(self.suffixes[pos].1);
            }
        }
        // 精确：整串一查就有。
        if let Some(index) = self.exact.get(&host) {
            consider(*index);
        }
        // 剩下：关键字、网段、MATCH，条数少直接扫；后缀和精确已经排过，
        // 这里跳过它们，只为序号兜底（序号比较保证第一条赢）。
        for (index, rule) in rules.iter().enumerate() {
            match &rule.matcher {
                Matcher::DomainSuffix(_) | Matcher::Domain(_) | Matcher::Unsupported(_) => {}
                matcher => {
                    if matcher.matches(&host) {
                        consider(index);
                    }
                }
            }
        }
        best
    }
}

/// `example.com` → `com.example`：后缀规则反转后排个序就能二分。
fn reverse_labels(domain: &str) -> String {
    domain
        .trim_start_matches('.')
        .split('.')
        .rev()
        .collect::<Vec<_>>()
        .join(".")
        .to_ascii_lowercase()
}

/// 一条连接的路由详情：给连接表看的，不只去向，还有命中的规则
/// 和走过的分组链（面板站“链路”列就是它）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteDetail {
    /// 命中的有效规则序号（手动在前），一条没中是 None（兜底拦截）。
    pub rule_index: Option<usize>,
    /// 命中规则的展示文本，兜底时为空。
    pub rule_text: String,
    /// 规则去向原文（组名 / `DIRECT` / `REJECT`），兜底时为空。
    pub target: String,
    /// 走过的分组 + 终点（节点名 / `DIRECT` / `REJECT`）。
    pub chain: Vec<String>,
    pub route: Route,
}

/// 规则展示文本：面板站风格（`DomainSuffix(xxx)`），连接表“规则”列用它。
/// 和 `rule_text`（`TYPE,arg` 原样）分开，一个给人看，一个保真。
pub fn rule_display(rule: &Rule) -> String {
    match &rule.matcher {
        Matcher::Domain(arg) => format!("Domain({arg})"),
        Matcher::DomainSuffix(arg) => format!("DomainSuffix({arg})"),
        Matcher::DomainKeyword(arg) => format!("DomainKeyword({arg})"),
        Matcher::IpCidr(cidr) => format!("IPCIDR({})", cidr_text(cidr)),
        Matcher::Match => "Match".to_string(),
        Matcher::Unsupported(kind) => kind.clone(),
    }
}

/// 路由总入口：按规则找去向，多带规则文本和分组链给连接表。
/// `table` 是启动时建好的快照，连接线程只读不写，UI 线程随便改自己的那份。
pub fn select_route_detail(
    subscription: &Subscription,
    rules: &[Rule],
    ruleset: &RuleSet,
    selections: &HashMap<String, String>,
    latencies: &HashMap<String, Option<u64>>,
    host: &str,
) -> RouteDetail {
    let Some(index) = ruleset.match_index(rules, host) else {
        // 一条没中（比如订阅没写 MATCH 兜底）：宁可断不断网漏，
        // 直接拒掉，日志里能看到。
        eprintln!("分流无规则命中，已拦截: {host}");
        return RouteDetail {
            rule_index: None,
            rule_text: String::new(),
            target: String::new(),
            chain: vec!["REJECT".to_string()],
            route: Route::Reject,
        };
    };
    let mut chain = Vec::new();
    let route = resolve_target(
        subscription,
        selections,
        latencies,
        &rules[index].target,
        &mut Vec::new(),
        &mut chain,
    );
    RouteDetail {
        rule_index: Some(index),
        rule_text: rule_display(&rules[index]),
        target: rules[index].target.clone(),
        chain,
        route,
    }
}

/// 把规则目标落到实处：关键字直出，组名进组里找。
/// `visited` 防分组互指绕圈，8 层还出不来就拒掉；
/// `chain` 记走过的分组和终点，给连接表看。
fn resolve_target(
    subscription: &Subscription,
    selections: &HashMap<String, String>,
    latencies: &HashMap<String, Option<u64>>,
    target: &str,
    visited: &mut Vec<String>,
    chain: &mut Vec<String>,
) -> Route {
    if visited.len() >= 8 {
        eprintln!("分流解析太深，已拦截: {target}");
        return Route::Reject;
    }
    // 关键字精确匹配：组名恰好叫 `direct` 这种极端情况也不能误吞，
    // Clash 里关键字本来就是大写字面量。
    if target == "DIRECT" {
        chain.push("DIRECT".to_string());
        return Route::Direct;
    }
    if target == "REJECT" {
        chain.push("REJECT".to_string());
        return Route::Reject;
    }
    // 目标直接写节点名也认（订阅手写规则常这么干）。
    if let Some(node) = subscription.nodes.iter().find(|n| n.name == target) {
        chain.push(node.name.clone());
        return Route::Node(node.clone());
    }
    let Some(group) = subscription.groups.iter().find(|g| g.name == target) else {
        eprintln!("分流目标对不上，已拦截: {target}");
        chain.push("REJECT".to_string());
        return Route::Reject;
    };
    if visited.contains(&group.name) {
        eprintln!("分组成环，已拦截: {target}");
        chain.push("REJECT".to_string());
        return Route::Reject;
    }
    visited.push(group.name.clone());
    chain.push(group.name.clone());
    // `url-test` 组不看手动选择，永远拿延迟最低的（这就是它存在的意义）。
    if group.kind == "url-test" {
        return match fastest_node(subscription, group, latencies) {
            Some(node) => {
                chain.push(node.name.clone());
                Route::Node(node)
            }
            None => {
                chain.push("REJECT".to_string());
                Route::Reject
            }
        };
    }
    // 选择存的必须是本组成员，改名删节点导致对不上就回第一顺位，自愈。
    let choice = selections
        .get(&group.name)
        .filter(|name| group.members.iter().any(|m| m == *name))
        .cloned()
        .or_else(|| group.members.first().cloned())
        .unwrap_or_default();
    if choice == "DIRECT" {
        chain.push("DIRECT".to_string());
        return Route::Direct;
    }
    if choice == "REJECT" {
        chain.push("REJECT".to_string());
        return Route::Reject;
    }
    if let Some(node) = subscription.nodes.iter().find(|n| n.name == choice) {
        chain.push(node.name.clone());
        return Route::Node(node.clone());
    }
    // 嵌套组：进下一层继续找。
    if subscription.groups.iter().any(|g| g.name == choice) {
        return resolve_target(subscription, selections, latencies, &choice, visited, chain);
    }
    eprintln!("分组选择对不上，已拦截: {} → {choice}", group.name);
    chain.push("REJECT".to_string());
    Route::Reject
}

/// 手动规则的类型：界面四个 chip 对应这四个，`MATCH` 不开放手填
/// （订阅自带兜底，手填 MATCH 一脚踩住后面所有规则，太容易误伤）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ManualKind {
    Suffix,
    Exact,
    Keyword,
    Cidr,
}

impl ManualKind {
    /// 界面 chip 顺序固定，增减类型记得两边一起改。
    pub const ALL: [ManualKind; 4] = [
        ManualKind::Suffix,
        ManualKind::Exact,
        ManualKind::Keyword,
        ManualKind::Cidr,
    ];
}

/// 一条手动规则：类型 + 内容 + 去向 + 排序。
/// `sort` 越小越靠前（手动整体排在自动之前）；`id` 是毫秒时间戳 +
/// 进程内序号，删除和排序认它，不认下标（下标一动就乱）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManualRule {
    pub id: String,
    pub kind: ManualKind,
    pub pattern: String,
    pub target: String,
    pub sort: i64,
}

impl ManualRule {
    /// 转成引擎规则：网段写坏了返回 None（界面添加时已拦一道，
    /// 这里是兜底，有坏的整条跳过，别让半截规则乱匹配）。
    pub fn to_rule(&self) -> Option<Rule> {
        let matcher = match self.kind {
            ManualKind::Suffix => Matcher::DomainSuffix(self.pattern.clone()),
            ManualKind::Exact => Matcher::Domain(self.pattern.clone()),
            ManualKind::Keyword => Matcher::DomainKeyword(self.pattern.clone()),
            ManualKind::Cidr => Matcher::IpCidr(CidrBlock::parse(self.pattern.trim())?),
        };
        if self.pattern.trim().is_empty() || self.target.trim().is_empty() {
            return None;
        }
        Some(Rule {
            matcher,
            target: self.target.trim().to_string(),
        })
    }
}

/// 有效规则表：手动按 `sort` 排前面，自动原样跟后面；顺带返回
/// 实际转出来的手动条数（调用方拿它判命中归属）。
/// 手动优先是 Clash 系惯例（用户写的就是要覆盖订阅的），
/// 更新订阅只换自动那段，手动躺在另一个库 key 里，碰不着。
pub fn effective_rules(manual: &[ManualRule], auto: &[Rule]) -> (Vec<Rule>, usize) {
    let mut ordered: Vec<&ManualRule> = manual.iter().collect();
    ordered.sort_by_key(|rule| rule.sort);
    let mut rules: Vec<Rule> = ordered.into_iter().filter_map(ManualRule::to_rule).collect();
    let manual_len = rules.len();
    rules.extend(auto.iter().cloned());
    (rules, manual_len)
}

/// 网段标准文本：`1.2.3.0/24` / `::1/128`，展示和入库共用一套，
/// 两边写的不一样会导致入库读出来对不上。
pub fn cidr_text(cidr: &CidrBlock) -> String {
    match cidr {
        CidrBlock::V4 { net, prefix } => {
            format!(
                "{}.{}.{}.{}/{prefix}",
                net >> 24,
                (net >> 16) & 255,
                (net >> 8) & 255,
                net & 255
            )
        }
        CidrBlock::V6 { net, prefix } => {
            let segments = (0..8)
                .map(|i| ((net >> (112 - i * 16)) & 0xffff) as u16)
                .collect::<Vec<_>>();
            format!(
                "{:x}:{:x}:{:x}:{:x}:{:x}:{:x}:{:x}:{:x}/{prefix}",
                segments[0],
                segments[1],
                segments[2],
                segments[3],
                segments[4],
                segments[5],
                segments[6],
                segments[7]
            )
        }
    }
}

/// 规则展示文本：`TYPE,arg`，查询结果和自动列表都用它拼。
pub fn rule_text(rule: &Rule) -> String {
    match &rule.matcher {
        Matcher::Domain(arg)
        | Matcher::DomainSuffix(arg)
        | Matcher::DomainKeyword(arg) => {
            let kind = match &rule.matcher {
                Matcher::Domain(_) => "DOMAIN",
                Matcher::DomainSuffix(_) => "DOMAIN-SUFFIX",
                _ => "DOMAIN-KEYWORD",
            };
            format!("{kind},{arg}")
        }
        Matcher::IpCidr(cidr) => {
            let version = match cidr {
                CidrBlock::V4 { .. } => "",
                CidrBlock::V6 { .. } => "6",
            };
            format!("IP-CIDR{version},{}", cidr_text(cidr))
        }
        Matcher::Match => "MATCH".to_string(),
        Matcher::Unsupported(kind) => format!("{kind},…"),
    }
}

/// 查询命中：序号是有效规则表里的全局位置（手动在前），`manual` 标来源。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LookupHit {
    pub index: usize,
    pub manual: bool,
    pub text: String,
    pub target: String,
}

/// 分流查询：输入域名或 IP，返回所有命中的规则（按优先级）+ 最终去向。
/// 手动规则同样参与（标 `manual`），赢的永远是第一条。
/// 纯函数，8 万条全扫也就几毫秒，调用方直接在 UI 线程调都行。
pub fn lookup_route(
    subscription: &Subscription,
    manual: &[ManualRule],
    selections: &HashMap<String, String>,
    latencies: &HashMap<String, Option<u64>>,
    host: &str,
) -> (Vec<LookupHit>, Route) {
    let host = host.trim().trim_matches('"').to_string();
    if host.is_empty() {
        return (Vec::new(), Route::Reject);
    }
    let (rules, manual_len) = effective_rules(manual, &subscription.rules);
    let mut hits = Vec::new();
    for (index, rule) in rules.iter().enumerate() {
        if rule.matcher.matches(&host) {
            hits.push(LookupHit {
                index,
                manual: index < manual_len,
                text: rule_text(rule),
                target: rule.target.clone(),
            });
        }
    }
    let route = match hits.first() {
        Some(hit) => resolve_target(
            subscription,
            selections,
            latencies,
            &rules[hit.index].target,
            &mut Vec::new(),
            &mut Vec::new(),
        ),
        None => Route::Reject,
    };
    (hits, route)
}

/// 组内延迟最低的节点：没测过的跳过，都没测过回第一个节点，
/// 组里一个节点没有返回 None（调用方拒掉）。
fn fastest_node(
    subscription: &Subscription,
    group: &ProxyGroup,
    latencies: &HashMap<String, Option<u64>>,
) -> Option<ProxyNode> {
    let mut nodes = group.members.iter().filter_map(|name| {
        subscription.nodes.iter().find(|n| &n.name == name)
    });
    let first = nodes.next()?.clone();
    let mut best = first.clone();
    let mut best_ms = latencies.get(&first.name).copied().flatten();
    for node in nodes {
        let Some(ms) = latencies.get(&node.name).copied().flatten() else {
            continue;
        };
        if best_ms.is_none_or(|kept| ms < kept) {
            best_ms = Some(ms);
            best = node.clone();
        }
    }
    Some(best)
}

/// 解析 Clash 订阅正文：节点只收 `type: vless`（其他类型丢掉），
/// 策略组照单收（类型不限，展示用）。
/// 手写行解析，不引入 YAML 依赖：订阅结构固定（`proxies:` / `proxy-groups:`
/// 下的缩进行），行解析够用且无头测试不联网。
pub fn parse_subscription(text: &str) -> Subscription {
    // 按顶格 key 切三段：节点 / 分组 / 规则。`rules:` 之后全是规则行，
    // 再遇到别的顶格 key（理论上没有）就停。
    let mut node_lines: Vec<&str> = Vec::new();
    let mut group_lines: Vec<&str> = Vec::new();
    let mut rule_lines: Vec<&str> = Vec::new();
    let mut section = 0;
    for line in text.lines() {
        let trimmed = line.trim_start();
        let top_level = !line.starts_with(' ') && !line.starts_with('\t');
        if top_level && !trimmed.is_empty() {
            if trimmed.starts_with("proxy-groups:") {
                section = 1;
                continue;
            }
            if trimmed.starts_with("rules:") {
                section = 2;
                continue;
            }
            // `proxies:` 这类段头跳过；规则段里再冒顶格 key 就停。
            // （组段里的杂散顶格行不用管，下游解析按缩进过滤。）
            if section == 2 {
                break;
            }
        }
        match section {
            1 => group_lines.push(line),
            2 => rule_lines.push(line),
            _ => node_lines.push(line),
        }
    }
    Subscription {
        nodes: parse_nodes(&node_lines),
        groups: parse_groups(&group_lines),
        rules: parse_rules(&rule_lines),
    }
}

/// 规则段解析：`  - TYPE,ARG,TARGET[,EXTRA...]` 一行一条，顺序保留。
/// `EXTRA`（如 `no-resolve`）直接忽略——我们本来就不做本地解析，
/// 语义天然一致。类型认不出的照单收下记 `Unsupported`，匹配时跳过。
fn parse_rules(lines: &[&str]) -> Vec<Rule> {
    let mut rules = Vec::new();
    for line in lines {
        let trimmed = line.trim_start();
        let Some(body) = trimmed.strip_prefix("- ") else {
            continue;
        };
        let mut parts = body.splitn(4, ',');
        let (Some(kind), Some(arg)) = (parts.next(), parts.next()) else {
            continue;
        };
        // `MATCH` 只有两段（`MATCH,target`），剩下都是三段起。
        let (matcher, target) = if kind.trim() == "MATCH" {
            (Matcher::Match, arg.trim().to_string())
        } else {
            let Some(target) = parts.next() else {
                continue;
            };
            let matcher = match kind.trim() {
                "DOMAIN" => Matcher::Domain(unquote(arg.trim())),
                "DOMAIN-SUFFIX" => Matcher::DomainSuffix(unquote(arg.trim())),
                "DOMAIN-KEYWORD" => Matcher::DomainKeyword(unquote(arg.trim())),
                "IP-CIDR" | "IP-CIDR6" => match CidrBlock::parse(arg.trim()) {
                    Some(cidr) => Matcher::IpCidr(cidr),
                    // 网段写坏了整条丢掉，别让半截规则乱匹配。
                    None => continue,
                },
                other => Matcher::Unsupported(other.to_string()),
            };
            (matcher, unquote(target.trim()))
        };
        if target.is_empty() {
            continue;
        }
        rules.push(Rule { matcher, target });
    }
    rules
}

/// 节点段解析：`  - name:` 开新条目，后续缩进键值对填字段。
fn parse_nodes(lines: &[&str]) -> Vec<ProxyNode> {
    let mut raws: Vec<RawNode> = Vec::new();
    for line in lines {
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix("- name:") {
            let mut raw = RawNode::default();
            raw.name = unquote(rest.trim());
            // VLESS 默认 443：条目里没写 port 时不至于连到 0 端口去。
            raw.port = 443;
            raws.push(raw);
            continue;
        }
        // 顶格的行是新段落，不属于任何节点。
        let Some(current) = raws.last_mut() else {
            continue;
        };
        if !line.starts_with(' ') && !line.starts_with('\t') {
            continue;
        }
        let Some(colon) = trimmed.find(':') else {
            continue;
        };
        let (key, value) = (trimmed[..colon].trim(), unquote(trimmed[colon + 1..].trim()));
        match key {
            "type" => current.node_type = value,
            "server" => current.server = value,
            // `servername:` 必须全字匹配：前缀匹配会把 `server:` 误吃掉。
            "servername" => current.servername = value,
            "port" => current.port = value.parse().unwrap_or(443),
            "uuid" => current.uuid = value,
            "path" => current.ws_path = value,
            // `Host:` 首字母大写：订阅里 headers 下就是这么写的。
            "Host" => current.ws_host = value,
            _ => {}
        }
    }
    raws.into_iter().filter_map(RawNode::into_proxy).collect()
}

/// 分组段解析：`  - name:` 开新组，`type:` 记类型，`      - xxx` 收成员。
/// 成员行绝不含冒号（含冒号的是 `- name:`，上面已经收了），所以名字里
/// 带冒号的成员会被跳过——订阅是机器生成的，暂不处理这种 corner。
fn parse_groups(lines: &[&str]) -> Vec<ProxyGroup> {
    let mut groups: Vec<ProxyGroup> = Vec::new();
    for line in lines {
        if line.trim().is_empty() {
            continue;
        }
        let trimmed = line.trim_start();
        // 组条目和成员都是 `- ` 开头：先看横线后是不是 `name:`。
        if let Some(rest) = trimmed.strip_prefix('-') {
            let rest = rest.trim_start();
            if let Some(name) = rest.strip_prefix("name:") {
                groups.push(ProxyGroup {
                    name: unquote(name.trim()),
                    kind: String::new(),
                    members: Vec::new(),
                });
                continue;
            }
            // 成员行：去掉 `- ` 前缀，去引号；含冒号的不是成员，跳过。
            if !rest.contains(':') {
                if let Some(group) = groups.last_mut() {
                    group.members.push(unquote(rest.trim()));
                }
            }
            continue;
        }
        // 组字段只认 `type:`，`url:` / `interval:` 之类直接忽略。
        let Some(current) = groups.last_mut() else {
            continue;
        };
        if !line.starts_with(' ') && !line.starts_with('\t') {
            continue;
        }
        if let Some(colon) = trimmed.find(':') {
            let (key, value) = (
                trimmed[..colon].trim(),
                unquote(trimmed[colon + 1..].trim()),
            );
            if key == "type" {
                current.kind = value;
            }
        }
    }
    // 空组（一个成员都没有）直接丢掉，界面没东西可渲染。
    groups
        .into_iter()
        .filter(|group| !group.members.is_empty())
        .collect()
}

/// 组成员的落位结果：节点卡（可点可测）、关键字卡（展示）、嵌套组卡（展示）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GroupMember<'a> {
    Node(&'a ProxyNode),
    Keyword(&'a str),
    Group(&'a str),
}

/// 各组的初始选中：订阅里的第一顺位（Clash 面板同款，管它是节点、
/// `DIRECT` 还是嵌套组）。空组（parse 层已丢）到不了这里。
/// 返回组名到成员名的映射，调用方存着当各组的独立选中态。
pub fn default_selections(subscription: &Subscription) -> HashMap<String, String> {
    subscription
        .groups
        .iter()
        .filter_map(|group| {
            group
                .members
                .first()
                .map(|first| (group.name.clone(), first.clone()))
        })
        .collect()
}

/// 用存过的选中覆盖默认：存的值必须是本组成员（改名、删节点、
/// 手改库导致对不上就沿用默认，绝不把坏名字写进界面）。
/// `saved` 是组名到成员名的映射（调用方从库里读，坏了就当空表）。
pub fn restore_selections(
    subscription: &Subscription,
    saved: &HashMap<String, String>,
) -> HashMap<String, String> {
    let mut selections = default_selections(subscription);
    for group in &subscription.groups {
        let Some(want) = saved.get(&group.name) else {
            continue;
        };
        if group.members.iter().any(|member| member == want) {
            selections.insert(group.name.clone(), want.clone());
        }
    }
    selections
}

/// 把组的成员名落位：先找节点，再认 `DIRECT` / `REJECT` 关键字，
/// 再找嵌套组名，都对不上就丢掉（比如订阅写错了名字）。
pub fn resolve_members<'a>(
    subscription: &'a Subscription,
    group: &'a ProxyGroup,
) -> Vec<GroupMember<'a>> {
    group
        .members
        .iter()
        .filter_map(|name| {
            if let Some(node) = subscription.nodes.iter().find(|n| &n.name == name) {
                return Some(GroupMember::Node(node));
            }
            if name == "DIRECT" || name == "REJECT" {
                return Some(GroupMember::Keyword(name));
            }
            if subscription.groups.iter().any(|g| &g.name == name) {
                return Some(GroupMember::Group(name));
            }
            None
        })
        .collect()
}

/// UUID 文本转 16 字节：去横线后必须是 32 个十六进制字符。
pub fn parse_uuid_bytes(text: &str) -> Result<[u8; 16], String> {
    let hex: String = text.chars().filter(|c| *c != '-').collect();
    if hex.len() != 32 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!("UUID 格式不对: {text}"));
    }
    let mut out = [0u8; 16];
    for (i, slot) in out.iter_mut().enumerate() {
        let pair = &hex[i * 2..i * 2 + 2];
        *slot = u8::from_str_radix(pair, 16).map_err(|_| format!("UUID 格式不对: {text}"))?;
    }
    Ok(out)
}

/// VLESS 请求头：版本(0) + UUID + 附加长度(0) + 命令(1=TCP) + 端口 + 地址。
/// 地址类型沿用 VMess 那套：1=IPv4，2=域名，3=IPv6（注意不是 SOCKS5 的 1/3/4）。
pub fn build_vless_handshake(uuid: &[u8; 16], host: &str, port: u16) -> Result<Vec<u8>, String> {
    let mut buf = Vec::with_capacity(26 + host.len());
    buf.push(0x00);
    buf.extend_from_slice(uuid);
    buf.push(0x00);
    buf.push(0x01);
    buf.extend_from_slice(&port.to_be_bytes());
    if let Ok(v4) = host.parse::<std::net::Ipv4Addr>() {
        buf.push(0x01);
        buf.extend_from_slice(&v4.octets());
    } else if let Ok(v6) = host.parse::<std::net::Ipv6Addr>() {
        buf.push(0x03);
        buf.extend_from_slice(&v6.octets());
    } else {
        let bytes = host.as_bytes();
        // 域名长度占一个字节，空域名和超长域名都发不出去，早拦早报错。
        if bytes.is_empty() || bytes.len() > 255 {
            return Err(format!("目标地址不合法: {host}"));
        }
        buf.push(0x02);
        buf.push(bytes.len() as u8);
        buf.extend_from_slice(bytes);
    }
    Ok(buf)
}

/// 在已读字节里找 HTTP 请求头结束位置（返回 `\r\n\r\n` 之后的长度）。
pub fn http_head_len(buf: &[u8]) -> Option<usize> {
    buf.windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|pos| pos + 4)
}

/// 解析 `host:port`：端口缺省用给定的默认值（CONNECT 缺省 443，明文缺省 80）。
/// 方括号 IPv6（`[::1]:443`）也处理，浏览器偶尔会这么发。
pub fn parse_authority(authority: &str, default_port: u16) -> Result<Target, String> {
    let authority = authority.trim().trim_matches('"');
    if authority.is_empty() {
        return Err("目标地址是空的".to_string());
    }
    if let Some(stripped) = authority
        .strip_prefix('[')
        .and_then(|rest| rest.find(']').map(|end| (&rest[..end], &rest[end + 1..])))
    {
        let (inside, after) = stripped;
        if inside.is_empty() {
            return Err(format!("目标地址不合法: {authority}"));
        }
        let port = after
            .strip_prefix(':')
            .filter(|port| !port.is_empty())
            .map(str::parse)
            .transpose()
            .map_err(|_| format!("端口不合法: {authority}"))?
            .unwrap_or(default_port);
        return Ok(Target {
            host: inside.to_string(),
            port,
        });
    }
    match authority.rfind(':') {
        Some(idx) if !authority[idx + 1..].is_empty() => {
            let port: u16 = authority[idx + 1..]
                .parse()
                .map_err(|_| format!("端口不合法: {authority}"))?;
            Ok(Target {
                host: authority[..idx].to_string(),
                port,
            })
        }
        _ => Ok(Target {
            host: authority.trim_end_matches(':').to_string(),
            port: default_port,
        }),
    }
}

/// 解析代理请求的目标：CONNECT 取 authority；明文 absolute-URI 取其中的
/// host；origin-form 则看 Host 头。方法大小写不敏感。
pub fn parse_http_target(
    method: &str,
    target: &str,
    host_header: Option<&str>,
) -> Result<Target, String> {
    if method.eq_ignore_ascii_case("CONNECT") {
        return parse_authority(target, 443);
    }
    let lower = target.to_ascii_lowercase();
    for prefix in ["http://", "https://"] {
        if lower.starts_with(prefix) {
            let rest = &target[prefix.len()..];
            let authority = rest.split('/').next().unwrap_or("");
            let default_port = if prefix == "https://" { 443 } else { 80 };
            return parse_authority(authority, default_port);
        }
    }
    // origin-form（如 `GET /path`）：目标只能看 Host 头。
    match host_header {
        Some(host) => parse_authority(host, 80),
        None => Err("HTTP 请求既不是 CONNECT，也没有 Host 头".to_string()),
    }
}

/// 解析 SOCKS5 连接请求（问候已由调用方吃掉，`req` 从 VER 开始）。
/// 只接受 CMD=1（CONNECT）；BIND / UDP ASSOCIATE 直接拒掉并报错。
pub fn parse_socks5_request(req: &[u8]) -> Result<Target, String> {
    if req.len() < 4 {
        return Err("SOCKS5 请求太短".to_string());
    }
    if req[0] != 0x05 {
        return Err("不是 SOCKS5 请求".to_string());
    }
    if req[1] != 0x01 {
        return Err("只支持 CONNECT，UDP 请走 TUN（暂不支持）".to_string());
    }
    match req[3] {
        // ATYP=1 IPv4：4 字节地址 + 2 字节端口。
        0x01 => {
            if req.len() < 10 {
                return Err("SOCKS5 IPv4 请求不完整".to_string());
            }
            Ok(Target {
                host: std::net::Ipv4Addr::new(req[4], req[5], req[6], req[7]).to_string(),
                port: u16::from_be_bytes([req[8], req[9]]),
            })
        }
        // ATYP=3 域名：1 字节长度 + 域名 + 2 字节端口。
        0x03 => {
            if req.len() < 5 {
                return Err("SOCKS5 域名请求不完整".to_string());
            }
            let len = req[4] as usize;
            if req.len() < 5 + len + 2 {
                return Err("SOCKS5 域名请求不完整".to_string());
            }
            let host = std::str::from_utf8(&req[5..5 + len])
                .map_err(|_| "SOCKS5 域名不是 UTF-8".to_string())?;
            Ok(Target {
                host: host.to_string(),
                port: u16::from_be_bytes([req[5 + len], req[6 + len]]),
            })
        }
        // ATYP=4 IPv6：16 字节地址 + 2 字节端口。
        0x04 => {
            if req.len() < 22 {
                return Err("SOCKS5 IPv6 请求不完整".to_string());
            }
            let mut octets = [0u8; 16];
            octets.copy_from_slice(&req[4..20]);
            Ok(Target {
                host: std::net::Ipv6Addr::from(octets).to_string(),
                port: u16::from_be_bytes([req[20], req[21]]),
            })
        }
        other => Err(format!("不支持的地址类型: {other}")),
    }
}

/// 注册表位置：HKCU 互联网设置，改它就是改系统代理，不用管理员权限。
const INET_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Internet Settings";

/// `reg add` 改 DWORD 的参数：调用方直接喂给 `reg` 命令。
pub fn reg_set_dword_args(name: &str, value: u32) -> Vec<String> {
    vec![
        "add".to_string(),
        INET_KEY.to_string(),
        "/v".to_string(),
        name.to_string(),
        "/t".to_string(),
        "REG_DWORD".to_string(),
        "/d".to_string(),
        value.to_string(),
        "/f".to_string(),
    ]
}

/// `reg add` 改字符串的参数：调用方直接喂给 `reg` 命令。
pub fn reg_set_string_args(name: &str, value: &str) -> Vec<String> {
    vec![
        "add".to_string(),
        INET_KEY.to_string(),
        "/v".to_string(),
        name.to_string(),
        "/t".to_string(),
        "REG_SZ".to_string(),
        "/d".to_string(),
        value.to_string(),
        "/f".to_string(),
    ]
}

/// 通知运行中的程序重读代理设置：先发“变了”(39)再发“刷新”(37)，
/// 少一个浏览器就不跟，这是 Windows 改系统代理后的标准收尾。
pub fn internet_refresh_script() -> String {
    "Add-Type -Namespace WinInetProxy -Name Native -MemberDefinition '[DllImport(\"wininet.dll\")] public static extern bool InternetSetOption(IntPtr hInternet, int dwOption, IntPtr lpBuffer, int dwBufferLength);'; [WinInetProxy.Native]::InternetSetOption([IntPtr]::Zero, 39, [IntPtr]::Zero, 0) | Out-Null; [WinInetProxy.Native]::InternetSetOption([IntPtr]::Zero, 37, [IntPtr]::Zero, 0) | Out-Null".to_string()
}

#[cfg(windows)]
fn run_command(program: &str, args: &[String]) -> Result<(), String> {
    let output = std::process::Command::new(program)
        .args(args)
        .output()
        .map_err(|e| format!("执行 {program} 失败: {e}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "{program} 返回非零退出码: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

/// 打开系统代理：ProxyEnable=1 + ProxyServer 指向本地入口，再刷新生效。
/// 只支持 Windows（本应用只发 Windows，其他平台直接报错）。
pub fn enable_system_proxy(port: u16) -> Result<(), String> {
    #[cfg(not(windows))]
    {
        let _ = port;
        return Err("系统代理开关只支持 Windows".to_string());
    }
    #[cfg(windows)]
    {
        run_command("reg", &reg_set_dword_args("ProxyEnable", 1))?;
        run_command(
            "reg",
            &reg_set_string_args("ProxyServer", &format!("127.0.0.1:{port}")),
        )?;
        // 本地直连不走代理（Clash 同款）：免得局域网、路由器管理页、
        // 本地开发服务被吸进隧道。取舍：会覆盖用户原来的例外名单，
        // v1 先保证开箱可用，名单合并以后再做。
        run_command("reg", &reg_set_string_args("ProxyOverride", "<local>"))?;
        run_command(
            "powershell",
            &[
                "-NoProfile".to_string(),
                "-Command".to_string(),
                internet_refresh_script(),
            ],
        )?;
        Ok(())
    }
}

/// 关闭系统代理：只把 ProxyEnable 置 0（服务器地址留着，下次开不用重填），再刷新。
pub fn disable_system_proxy() -> Result<(), String> {
    #[cfg(not(windows))]
    {
        return Err("系统代理开关只支持 Windows".to_string());
    }
    #[cfg(windows)]
    {
        run_command("reg", &reg_set_dword_args("ProxyEnable", 0))?;
        run_command(
            "powershell",
            &[
                "-NoProfile".to_string(),
                "-Command".to_string(),
                internet_refresh_script(),
            ],
        )?;
        Ok(())
    }
}

/// 一条连接的遥测快照：界面连接表直接拿去渲染，不碰锁。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnSnapshot {
    pub id: u64,
    pub host: String,
    pub source: String,
    pub dest: String,
    pub kind: String,
    pub rule: String,
    pub chain: String,
    pub route: String,
    pub process: String,
    /// 建连时的 Unix 毫秒：界面换算“几秒前”，快照里不带 Instant
    /// （Instant 不能跨线程算差值还保持直观，毫秒数最省事）。
    pub started_ms: u64,
    pub up: u64,
    pub down: u64,
    pub active: bool,
}

/// 连接表里的单条记录：快照再加 active 旗和 kill 旗。
/// kill 是界面“断开全部”用的：中继循环每轮看一眼，举旗就收尾关连接，
/// 比跨线程掐 socket 安全（谁建谁关，不抢句柄）。
#[derive(Debug, Clone)]
struct ConnRecord {
    snapshot: ConnSnapshot,
    active: bool,
    killed: bool,
}

/// 活连接上限：浏览器开几百个是常事，表只留最近的，已关闭的只留尾巴。
const MAX_CONNS: usize = 300;
/// 已关闭保留条数：表头“已关闭 N”要数，太多占内存，留个尾巴够看。
const MAX_CLOSED_KEPT: usize = 50;

/// 连接注册表：监听线程写，界面线程读，`Mutex` 包起来换手。
/// 临界区都极短（计数自增、首尾插删）， contention 可忽略。
#[derive(Debug, Default)]
pub struct ConnRegistry {
    next_id: u64,
    order: VecDeque<u64>,
    conns: HashMap<u64, ConnRecord>,
}

impl ConnRegistry {
    /// 登记一条新连接，返回 id（调用方拿着往里记字节）。
    pub fn register(&mut self, mut snapshot: ConnSnapshot) -> u64 {
        self.next_id += 1;
        let id = self.next_id;
        snapshot.id = id;
        snapshot.active = true;
        self.order.push_back(id);
        self.conns.insert(
            id,
            ConnRecord {
                snapshot,
                active: true,
                killed: false,
            },
        );
        // 活连接太多（比如下载器狂开），掐掉最老已关闭的，活的再多也不动。
        while self.order.len() > MAX_CONNS {
            if let Some(oldest) = self.order.pop_front() {
                let remove = self
                    .conns
                    .get(&oldest)
                    .is_none_or(|record| !record.active);
                if remove {
                    self.conns.remove(&oldest);
                } else {
                    self.order.push_front(oldest);
                    break;
                }
            } else {
                break;
            }
        }
        id
    }

    /// 上行加数（浏览器 → 远端）。
    pub fn add_up(&mut self, id: u64, n: u64) {
        if let Some(record) = self.conns.get_mut(&id) {
            record.snapshot.up += n;
        }
    }

    /// 下行加数（远端 → 浏览器）。
    pub fn add_down(&mut self, id: u64, n: u64) {
        if let Some(record) = self.conns.get_mut(&id) {
            record.snapshot.down += n;
        }
    }

    /// 连接结束：标关闭，已关闭的只留尾巴，老的删掉。
    pub fn finish(&mut self, id: u64) {
        if let Some(record) = self.conns.get_mut(&id) {
            record.active = false;
            record.snapshot.active = false;
        }
        let closed: Vec<u64> = self
            .order
            .iter()
            .copied()
            .filter(|id| self.conns.get(id).is_none_or(|record| !record.active))
            .collect();
        if closed.len() > MAX_CLOSED_KEPT {
            for id in closed.iter().take(closed.len() - MAX_CLOSED_KEPT) {
                self.conns.remove(id);
                self.order.retain(|kept| kept != id);
            }
        }
    }

    /// 断开全部活连接：只举旗不动 socket，中继循环下轮看到就收尾。
    /// 关连接本身还是各线程自己关，不抢句柄。
    pub fn kill_all(&mut self) {
        for record in self.conns.values_mut() {
            if record.active {
                record.killed = true;
            }
        }
    }

    /// 某条连接是否被断开过：中继循环每轮查一次。
    pub fn is_killed(&self, id: u64) -> bool {
        self.conns.get(&id).is_some_and(|record| record.killed)
    }

    /// 清除已关闭的记录（“清除已关闭”按钮用它，活的不动）。
    pub fn clear_closed(&mut self) {
        let dead: Vec<u64> = self
            .order
            .iter()
            .copied()
            .filter(|id| self.conns.get(id).is_none_or(|record| !record.active))
            .collect();
        for id in dead {
            self.conns.remove(&id);
            self.order.retain(|kept| kept != &id);
        }
    }

    /// 当前快照：插入顺序，活的在前、关的在后，界面直接渲染。
    /// 表头计数调用方数快照就行，不再单开接口。
    pub fn snapshot(&self) -> Vec<ConnSnapshot> {
        let (mut live, mut dead): (Vec<_>, Vec<_>) = self
            .order
            .iter()
            .filter_map(|id| self.conns.get(id))
            .partition(|record| record.active);
        live.extend(dead.drain(..));
        live.into_iter().map(|record| record.snapshot.clone()).collect()
    }
}

/// 连接上的流量表：`relay` 两边每转一块调一次，注册表在 `Mutex` 后面，
/// 调用方（连接线程）拿着随便调，界面线程读快照。
/// 注册表句柄：监听线程和界面线程各拿一个 `Arc`，`Mutex` 里面换手。
/// 临界区都极短， contention 可忽略。
pub type Registry = Arc<Mutex<ConnRegistry>>;

/// 连接上的流量表：`relay` 两边每转一块调一次，注册表在 `Mutex` 后面，
/// 调用方（连接线程）拿着随便调，界面线程读快照。
#[derive(Debug, Clone)]
pub struct ConnMeter {
    registry: Registry,
    id: u64,
}

impl ConnMeter {
    pub fn add_up(&self, n: u64) {
        if let Ok(mut registry) = self.registry.lock() {
            registry.add_up(self.id, n);
        }
    }

    pub fn add_down(&self, n: u64) {
        if let Ok(mut registry) = self.registry.lock() {
            registry.add_down(self.id, n);
        }
    }

    pub fn finish(&self) {
        if let Ok(mut registry) = self.registry.lock() {
            registry.finish(self.id);
        }
    }

    pub fn is_killed(&self) -> bool {
        self.registry
            .lock()
            .map(|registry| registry.is_killed(self.id))
            .unwrap_or(false)
    }
}

/// 当前 Unix 毫秒：连接建连时间戳用它，界面换算相对时间。
pub fn unix_millis_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 字节量：B / KB / MB，连接表下载上传列用它。
pub fn format_bytes(n: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * 1024;
    if n < KB {
        format!("{n} B")
    } else if n < MB {
        format!("{:.2} KB", n as f64 / KB as f64)
    } else {
        format!("{:.2} MB", n as f64 / MB as f64)
    }
}

/// 速率：字节量 + `/s`，连接表速度列用它。
pub fn format_speed(bytes_per_sec: u64) -> String {
    format!("{}/s", format_bytes(bytes_per_sec))
}

/// 相对时间：`X秒前 / X分钟前 / X小时前`，连接表时间列用它。
/// 入参是毫秒差（调用方 `now - started`），纯函数好测试。
pub fn format_age(ms: u64) -> String {
    let secs = ms / 1000;
    if secs < 60 {
        format!("{secs}秒前")
    } else if secs < 3600 {
        format!("{}分钟前", secs / 60)
    } else {
        format!("{}小时前", secs / 3600)
    }
}

/// 拉取订阅正文：地址必须 http(s) 开头，状态码非 200 直接报错。
/// 阻塞调用，调用方负责放后台线程（界面线程调它会卡住）。
pub fn fetch_subscription(url: &str) -> Result<String, String> {
    let url = url.trim();
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err("订阅地址必须以 http:// 或 https:// 开头".to_string());
    }
    let response = ureq::get(url)
        .timeout(FETCH_TIMEOUT)
        .call()
        .map_err(|e| format!("拉取订阅失败: {e}"))?;
    if response.status() != 200 {
        return Err(format!("订阅服务返回 {}", response.status()));
    }
    response
        .into_string()
        .map_err(|e| format!("读订阅正文失败: {e}"))
}

/// 运行中的代理：拿着停机旗和监听线程，drop 不会自动停，必须调 `stop`。
/// 监听线程是后台线程，连接线程随连接结束自生自灭。
pub struct ProxyHandle {
    port: u16,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl ProxyHandle {
    /// 停机：先举旗，再连一下自己把 accept 叫醒，最后等监听线程收尾。
    /// 已建立的连接不断，让它自己流完（强制掐会断下载）。
    pub fn stop(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let addr = format!("127.0.0.1:{}", self.port);
        if let Ok(sock) = addr.parse::<std::net::SocketAddr>() {
            let _ = TcpStream::connect_timeout(&sock, Duration::from_millis(500));
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// 分流快照：订阅 + 各组选择 + 延迟，索引一次建好。
/// 界面每次改选择就换一份新的（`LiveRouting` 里整个换掉）；
/// 每个连接拿自己那份，已建连的不受影响。`Clone` 只是里面
/// `Arc` 引用计数加一，随便拷。
#[derive(Clone)]
pub struct RoutingTable {
    table: Arc<RoutingTableInner>,
}

struct RoutingTableInner {
    subscription: Subscription,
    selections: HashMap<String, String>,
    latencies: HashMap<String, Option<u64>>,
    /// 有效规则（手动在前）：匹配和取目标都认它，不认订阅原表。
    rules: Vec<Rule>,
    ruleset: RuleSet,
}

impl RoutingTable {
    /// 从界面状态建快照：有效规则拼好，分流索引（8 万条后缀排序）一次建好，
    /// 别每连接建一次。调用在点启动的瞬间，几十毫秒可接受。
    pub fn build(
        subscription: Subscription,
        manual: &[ManualRule],
        selections: HashMap<String, String>,
        latencies: HashMap<String, Option<u64>>,
    ) -> Self {
        let (rules, _) = effective_rules(manual, &subscription.rules);
        let ruleset = RuleSet::build(&rules);
        Self {
            table: Arc::new(RoutingTableInner {
                subscription,
                selections,
                latencies,
                rules,
                ruleset,
            }),
        }
    }

    /// 带详情的路由：规则从头匹配，第一条命中的赢，多给规则文本和分组链。
    pub fn route_detail(&self, host: &str) -> RouteDetail {
        select_route_detail(
            &self.table.subscription,
            &self.table.rules,
            &self.table.ruleset,
            &self.table.selections,
            &self.table.latencies,
            host,
        )
    }
}

/// 入站协议：连接表“类型”列就展示它，括号里永远是 tcp
/// （UDP 还没接，谎报类型不如不写）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Inbound {
    /// SOCKS5 CONNECT。
    Socks,
    /// HTTP CONNECT（浏览器的 HTTPS 就是它）。
    Https,
    /// 明文 HTTP 代理请求。
    Http,
}

impl Inbound {
    fn label(self) -> &'static str {
        match self {
            Inbound::Socks => "SOCKS(tcp)",
            Inbound::Https => "HTTPS(tcp)",
            Inbound::Http => "HTTP(tcp)",
        }
    }
}

/// 活分流表：界面和监听线程各拿一个 `Arc`，`RwLock` 里面换手。
/// 界面改选择就整表替换（`RoutingTable` 里是 `Arc`，换手便宜）；
/// 每个连接进来读一次、用自己的那份，已建连的不受影响。
pub type LiveRouting = Arc<RwLock<RoutingTable>>;

/// 读活分流表的当前快照：锁毒化（基本不可能）就拿毒里的继续跑，
/// 绝不因为一把锁把连接掐了。
fn live_snapshot(live: &LiveRouting) -> RoutingTable {
    live.read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

/// 启动本地混合入口：端口被占直接报错，调用方提示用户换端口或关冲突软件。
/// `live` 是活分流表（界面改选择即换，新连接即用）；
/// `registry` 是连接表，监听线程往里记，界面线程读快照。
pub fn start_proxy(
    live: LiveRouting,
    registry: Registry,
    port: u16,
) -> Result<ProxyHandle, String> {
    let listener = TcpListener::bind(format!("127.0.0.1:{port}"))
        .map_err(|e| format!("本地端口 {port} 占不下来: {e}"))?;
    listener
        .set_nonblocking(true)
        .map_err(|e| format!("监听器切非阻塞失败: {e}"))?;
    let stop = Arc::new(AtomicBool::new(false));
    let flag = stop.clone();
    let thread = std::thread::Builder::new()
        .name("wong-proxy".to_string())
        .spawn(move || accept_loop(listener, live, registry, flag))
        .map_err(|e| format!("起监听线程失败: {e}"))?;
    Ok(ProxyHandle {
        port,
        stop,
        thread: Some(thread),
    })
}

/// 监听循环：非阻塞 accept，捡不到就睡一会儿；每个连接起线程处理。
/// 对端地址（源地址）这里就记下来，后面 `peer_addr` 可能拿不到。
fn accept_loop(
    listener: TcpListener,
    live: LiveRouting,
    registry: Registry,
    stop: Arc<AtomicBool>,
) {
    while !stop.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, peer)) => {
                // 每个连接读一次活表：界面刚换的选择，下一个连接就用上；
                // 已建连的手里是旧快照，不受影响。
                let table = live_snapshot(&live);
                let registry = registry.clone();
                let source = peer.to_string();
                let _ = std::thread::Builder::new()
                    .name("wong-proxy-conn".to_string())
                    .spawn(move || {
                        // 连接继承了监听器的非阻塞，拨回阻塞，
                        // 否则后面的 read_exact 直接吃 WouldBlock。
                        let _ = stream.set_nonblocking(false);
                        if let Err(e) = handle_client(stream, &table, &registry, source) {
                            eprintln!("代理连接失败: {e}");
                        }
                    });
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(ACCEPT_IDLE);
            }
            Err(e) => {
                eprintln!("监听出错: {e}");
                std::thread::sleep(Duration::from_millis(200));
            }
        }
    }
}

/// 单个入站连接：首字节嗅探，`0x05` 走 SOCKS5，否则按 HTTP 代理解析。
fn handle_client(
    stream: TcpStream,
    table: &RoutingTable,
    registry: &Registry,
    source: String,
) -> Result<(), String> {
    stream
        .set_read_timeout(Some(PEEK_TIMEOUT))
        .map_err(|e| format!("设超时失败: {e}"))?;
    let mut peek = [0u8; 1];
    stream
        .peek(&mut peek)
        .map_err(|e| format!("读首字节失败: {e}"))?;
    if peek[0] == 0x05 {
        handle_socks5(stream, table, registry, source)
    } else {
        handle_http(stream, table, registry, source)
    }
}

/// 建连接表条目：目标解析完、分流定下来就登记，`finish` 在连接结束调。
/// 进程归因先填横线：Windows 按连接查进程要 IP Helper 轮询，
/// 代价和复杂度都超出一期，先留空不断功能。
fn register_conn(
    registry: &Registry,
    source: String,
    inbound: Inbound,
    target: &Target,
    dest: String,
    detail: &RouteDetail,
) -> ConnMeter {
    let route = match &detail.route {
        Route::Direct => "DIRECT".to_string(),
        Route::Reject => "REJECT".to_string(),
        Route::Node(node) => node.name.clone(),
    };
    let snapshot = ConnSnapshot {
        id: 0,
        host: format!("{}:{}", target.host, target.port),
        source,
        dest,
        kind: inbound.label().to_string(),
        rule: if detail.rule_text.is_empty() {
            "–".to_string()
        } else {
            detail.rule_text.clone()
        },
        chain: detail.chain.join(" / "),
        route,
        process: "–".to_string(),
        started_ms: unix_millis_now(),
        up: 0,
        down: 0,
        active: true,
    };
    // 快照小，克隆一份走正常路径，毒化才用原件（几乎走不到）。
    let id = registry
        .lock()
        .map(|mut registry| registry.register(snapshot.clone()))
        .unwrap_or_else(|poisoned| poisoned.into_inner().register(snapshot));
    ConnMeter {
        registry: registry.clone(),
        id,
    }
}

/// 分流 dispatch：规则定去向，直连裸连、拦截按协议回包、节点进隧道。
/// 拦截不报错（广告天天被拦，刷屏没意义），回完包直接关就行。
fn dispatch(
    mut stream: TcpStream,
    table: &RoutingTable,
    registry: &Registry,
    source: String,
    inbound: Inbound,
    target: &Target,
    first: Option<Vec<u8>>,
) -> Result<(), String> {
    let detail = table.route_detail(&target.host);
    match &detail.route {
        Route::Reject => {
            // 拦截回包：SOCKS 回失败码，CONNECT 回 403，明文回 403，
            // 直接关连接客户端会一直重试转圈。
            match inbound {
                Inbound::Socks => {
                    let _ = stream.write_all(&[0x05, 0x02, 0x00, 0x01, 0, 0, 0, 0, 0, 0]);
                }
                _ => {
                    let _ =
                        stream.write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n");
                }
            }
            let meter = register_conn(
                registry,
                source,
                inbound,
                target,
                format!("{}:{}", target.host, target.port),
                &detail,
            );
            meter.finish();
            Ok(())
        }
        Route::Direct => {
            // 直连先回成功包再干活，和隧道一个顺序（客户端先看到成功）。
            match inbound {
                Inbound::Socks => {
                    stream
                        .write_all(&[0x05, 0x00, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
                        .map_err(|e| format!("写 SOCKS5 成功回包失败: {e}"))?;
                }
                Inbound::Https => {
                    stream
                        .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                        .map_err(|e| format!("写 CONNECT 回包失败: {e}"))?;
                }
                Inbound::Http => {}
            }
            // 目标地址能解析就记解析后的，不能（域名）就记原文。
            let dest = direct_addr(target).unwrap_or_else(|| {
                format!("{}:{}", target.host, target.port)
            });
            let meter = register_conn(registry, source, inbound, target, dest, &detail);
            let result = relay_direct(stream, target, &meter);
            meter.finish();
            result
        }
        Route::Node(node) => {
            let node = node.clone();
            match inbound {
                Inbound::Socks => {
                    stream
                        .write_all(&[0x05, 0x00, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
                        .map_err(|e| format!("写 SOCKS5 成功回包失败: {e}"))?;
                }
                Inbound::Https => {
                    stream
                        .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                        .map_err(|e| format!("写 CONNECT 回包失败: {e}"))?;
                }
                Inbound::Http => {}
            }
            let meter = register_conn(
                registry,
                source,
                inbound,
                target,
                format!("{}:{}", target.host, target.port),
                &detail,
            );
            let result = tunnel(stream, &node, target, first, &meter);
            meter.finish();
            result
        }
    }
}

/// 直连目标解析：能解出 IP 就记 IP（连接表目标地址列好看），
/// 解不出（域名）返回 None，调用方记原文。
fn direct_addr(target: &Target) -> Option<String> {
    (target.host.as_str(), target.port)
        .to_socket_addrs()
        .ok()?
        .next()
        .map(|addr| addr.to_string())
}

/// SOCKS5 入站：问候（无认证）→ CONNECT 请求 → 按规则分流。
/// 回包（成功/拦截）统一挪到 `dispatch` 里，成功包和隧道一个顺序。
fn handle_socks5(
    mut stream: TcpStream,
    table: &RoutingTable,
    registry: &Registry,
    source: String,
) -> Result<(), String> {
    let mut head = [0u8; 2];
    stream
        .read_exact(&mut head)
        .map_err(|e| format!("读 SOCKS5 问候失败: {e}"))?;
    if head[0] != 0x05 {
        return Err("不是 SOCKS5 请求".to_string());
    }
    let mut methods = vec![0u8; head[1] as usize];
    stream
        .read_exact(&mut methods)
        .map_err(|e| format!("读 SOCKS5 方法失败: {e}"))?;
    stream
        .write_all(&[0x05, 0x00])
        .map_err(|e| format!("写 SOCKS5 问候回包失败: {e}"))?;
    // 请求头先读 4 字节定长部分，再按地址类型补读，拼起来统一解析。
    let mut fixed = [0u8; 4];
    stream
        .read_exact(&mut fixed)
        .map_err(|e| format!("读 SOCKS5 请求失败: {e}"))?;
    let mut raw = fixed.to_vec();
    let rest_len = match fixed[3] {
        0x01 => 4 + 2,
        0x04 => 16 + 2,
        0x03 => {
            let mut len = [0u8; 1];
            stream
                .read_exact(&mut len)
                .map_err(|e| format!("读 SOCKS5 域名长度失败: {e}"))?;
            raw.push(len[0]);
            len[0] as usize + 2
        }
        other => {
            let _ = stream.write_all(&[0x05, 0x08, 0x00, 0x01, 0, 0, 0, 0, 0, 0]);
            return Err(format!("不支持的地址类型: {other}"));
        }
    };
    let mut rest = vec![0u8; rest_len];
    stream
        .read_exact(&mut rest)
        .map_err(|e| format!("读 SOCKS5 地址失败: {e}"))?;
    raw.extend_from_slice(&rest);
    let target = match parse_socks5_request(&raw) {
        Ok(target) => target,
        Err(e) => {
            let _ = stream.write_all(&[0x05, 0x07, 0x00, 0x01, 0, 0, 0, 0, 0, 0]);
            return Err(e);
        }
    };
    dispatch(stream, table, registry, source, Inbound::Socks, &target, None)
}

/// HTTP 入站：读到请求头结束，CONNECT 和明文都按去向走 `dispatch`。
/// 回包（200/403）统一在 `dispatch` 里，成功包和隧道一个顺序。
fn handle_http(
    mut stream: TcpStream,
    table: &RoutingTable,
    registry: &Registry,
    source: String,
) -> Result<(), String> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    let head_len = loop {
        let n = stream
            .read(&mut tmp)
            .map_err(|e| format!("读 HTTP 请求失败: {e}"))?;
        if n == 0 {
            return Err("HTTP 请求没发完就断了".to_string());
        }
        buf.extend_from_slice(&tmp[..n]);
        if buf.len() > MAX_HTTP_HEAD {
            return Err("HTTP 请求头太大".to_string());
        }
        if let Some(len) = http_head_len(&buf) {
            break len;
        }
    };
    let head_text = String::from_utf8_lossy(&buf[..head_len]).into_owned();
    let mut lines = head_text.split("\r\n");
    let request_line = lines.next().unwrap_or("");
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let target_raw = parts.next().unwrap_or("").to_string();
    // Host 头大小写不敏感，逐行找 `xxx:` 前缀。
    let mut host_header: Option<String> = None;
    for line in lines {
        if line.len() > 5 && line[..5].eq_ignore_ascii_case("host:") {
            host_header = Some(line[5..].trim().to_string());
            break;
        }
    }
    let target = parse_http_target(&method, &target_raw, host_header.as_deref())?;
    if method.eq_ignore_ascii_case("CONNECT") {
        dispatch(
            stream,
            table,
            registry,
            source,
            Inbound::Https,
            &target,
            None,
        )
    } else {
        // 明文 HTTP：已读字节整体带过去（absolute-URI 源站一般照收）。
        // 注意不能只截到头结束：POST 的 body 可能已经读进来了一部分，
        // 截掉就丢数据，剩下的一半中继循环会接着从流里读。
        dispatch(
            stream,
            table,
            registry,
            source,
            Inbound::Http,
            &target,
            Some(buf),
        )
    }
}

/// 出站 WS 流的别名：到处都要写，省点墨。
type WsStream = tungstenite::WebSocket<native_tls::TlsStream<TcpStream>>;

/// VLESS 响应头切分：版本(1) + 附加长度(1) + 附加(N)，返回（头总长，头后数据）。
/// 长度不够返回 `Ok(None)` 让调用方继续攒；版本不对直接报错（对端不是 VLESS）。
fn split_vless_response(buf: &[u8]) -> Result<Option<(usize, &[u8])>, String> {
    if buf.len() < 2 {
        return Ok(None);
    }
    if buf[0] != 0x00 {
        return Err(format!("VLESS 响应版本不对: {:#04x}", buf[0]));
    }
    let total = 2 + buf[1] as usize;
    if buf.len() < total {
        return Ok(None);
    }
    Ok(Some((total, &buf[total..])))
}

/// 吃掉 VLESS 响应头，返回响应头之后多带的数据（一般没有）。
/// 关键：响应头是 VLESS 协议自己的，必须客户端消费，绝不能转发给浏览器——
/// 转了 HTTPS 第一字节就不是 0x16，浏览器报 ERR_SSL_PROTOCOL_ERROR。
/// 响应头可能和第一包数据拼在一个 WS 消息里，所以按字节攒够了再切。
fn read_vless_response(ws: &mut WsStream) -> Result<Vec<u8>, String> {
    use tungstenite::{Error, Message};
    // 响应头 10 秒内不到，这条隧道就别要了。
    let deadline = std::time::Instant::now() + DIAL_TIMEOUT;
    let mut head: Vec<u8> = Vec::new();
    loop {
        if std::time::Instant::now() > deadline {
            return Err("等 VLESS 响应头超时".to_string());
        }
        match ws.read() {
            Ok(Message::Binary(data)) => head.extend_from_slice(&data),
            // Ping 等控制帧忽略。
            Ok(_) => {}
            Err(Error::Io(e)) if is_timeout(&e) => {}
            Err(e) => return Err(format!("读 VLESS 响应头失败: {e}")),
        }
        match split_vless_response(&head)? {
            Some((_, rest)) => return Ok(rest.to_vec()),
            None => {}
        }
    }
}

/// 进隧道：VLESS 握手 + 首包一次发掉，吃掉响应头，然后双向中继直到一边挂掉。
fn tunnel(
    client: TcpStream,
    node: &ProxyNode,
    target: &Target,
    first: Option<Vec<u8>>,
    meter: &ConnMeter,
) -> Result<(), String> {
    let uuid = parse_uuid_bytes(&node.uuid)?;
    let mut hello = build_vless_handshake(&uuid, &target.host, target.port)?;
    if let Some(data) = first {
        hello.extend_from_slice(&data);
    }
    let ws = dial_ws(node)?;
    relay(client, ws, hello, meter)
}

/// 出站拨号：TCP 连 server → TLS（SNI 用节点的 servername）→ WS 握手。
/// WS 的 URI host 用节点的 Host 头（一般和 SNI 一致，前置场景靠它路由）。
fn dial_ws(
    node: &ProxyNode,
) -> Result<tungstenite::WebSocket<native_tls::TlsStream<TcpStream>>, String> {
    dial_ws_stream(node, DIAL_TIMEOUT)
}

/// 可定超时的出站拨号：隧道走默认超时，测速走短超时（慢节点不等它）。
fn dial_ws_stream(
    node: &ProxyNode,
    timeout: Duration,
) -> Result<tungstenite::WebSocket<native_tls::TlsStream<TcpStream>>, String> {
    let mut last_err = format!("{} 解析不到地址", node.server);
    let mut tcp_ok: Option<TcpStream> = None;
    let addrs = (node.server.as_str(), node.port)
        .to_socket_addrs()
        .map_err(|e| format!("解析 {} 失败: {e}", node.server))?;
    // 解析到多个地址挨个试：第一个不通就试下一个，别一个挂掉全挂。
    for addr in addrs {
        match TcpStream::connect_timeout(&addr, timeout) {
            Ok(tcp) => {
                tcp_ok = Some(tcp);
                break;
            }
            Err(e) => last_err = format!("连 {} 失败: {e}", node.server),
        }
    }
    let tcp = tcp_ok.ok_or(last_err)?;
    tcp.set_read_timeout(Some(timeout))
        .map_err(|e| format!("设读超时失败: {e}"))?;
    tcp.set_write_timeout(Some(timeout))
        .map_err(|e| format!("设写超时失败: {e}"))?;
    let tls = native_tls::TlsConnector::new()
        .map_err(|e| format!("TLS 初始化失败: {e}"))?
        .connect(&node.sni, tcp)
        .map_err(|e| format!("TLS 握手失败（SNI={}）: {e}", node.sni))?;
    // URI 里的 host 只决定 HTTP Host 头，真正的 SNI 在上面 TLS 那一步。
    let uri: tungstenite::http::Uri = format!("wss://{}{}", node.host, node.path)
        .parse()
        .map_err(|e| format!("WS 地址拼错了: {e}"))?;
    let request = tungstenite::ClientRequestBuilder::new(uri);
    let (ws, _) = tungstenite::client::client(request, tls)
        .map_err(|e| format!("WS 握手失败: {e}"))?;
    Ok(ws)
}

/// 测速超时：单个节点建连（TCP + TLS + WS 握手）超过它就记超时，不等了。
const MEASURE_TIMEOUT: Duration = Duration::from_secs(5);

/// 单节点测速：出站全链路建连耗时（毫秒），连不上返回 None（记超时）。
/// 只建连不发数据，对端挂掉前我们先关，不产生实质流量。
/// 阻塞调用，调用方负责放后台（多节点用 `measure_all` 并行）。
pub fn measure_node(node: &ProxyNode) -> Option<u64> {
    let start = std::time::Instant::now();
    // 建连成功就够了：ws 拿到了立刻关掉，不发 VLESS 握手。
    let mut ws = dial_ws_stream(node, MEASURE_TIMEOUT).ok()?;
    let _ = ws.close(None);
    Some(start.elapsed().as_millis() as u64)
}

/// 并行测一整组节点：一个节点一个线程，结果按输入顺序回来。
/// 调用方自己决定扔哪个后台（界面侧走 background_executor，别卡住）。
pub fn measure_all(nodes: &[ProxyNode]) -> Vec<(String, Option<u64>)> {
    std::thread::scope(|scope| {
        let handles: Vec<_> = nodes
            .iter()
            .map(|node| scope.spawn(|| (node.name.clone(), measure_node(node))))
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().unwrap_or_else(|_| (String::new(), None)))
            .collect()
    })
}

/// 延迟档位：界面按档上色（面板站风格：绿快、蓝中、红慢、灰超时）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LatencyTone {
    Fast,
    Normal,
    Slow,
    Dead,
}

/// 隧道自检：直连选定节点，穿隧道取 example.com 首页，返回首字节耗时（毫秒）。
/// 只建连不断系统设置，调用方放后台；失败文本直接展示给用户看，方便定位。
/// 回包跨帧就拼起来验，累积到 64KB 还验不过判失败。
pub fn self_check(node: &ProxyNode) -> Result<u64, String> {
    use tungstenite::Message;
    let uuid = parse_uuid_bytes(&node.uuid)?;
    let hello = build_vless_handshake(&uuid, "example.com", 80)?;
    let mut ws = dial_ws(node)?;
    ws.send(Message::Binary(hello.into()))
        .map_err(|e| format!("自检发握手失败: {e}"))?;
    // 响应头同样要吃掉，不然回包前面多个脏字节；
    // 多带的数据是回包开头，留着一起验。
    let pending = read_vless_response(&mut ws)?;
    ws.send(Message::Binary(
        b"GET / HTTP/1.0\r\nHost: example.com\r\n\r\n".to_vec().into(),
    ))
    .map_err(|e| format!("自检发请求失败: {e}"))?;
    let start = std::time::Instant::now();
    // 首包计时从请求发完算：握手耗时测速里已经量过，这里只量取数。
    let mut body = pending;
    loop {
        match ws.read() {
            Ok(Message::Binary(data)) => {
                body.extend_from_slice(&data);
                // 状态行在第一帧，标志串一般也在前几 KB，64KB 还没见就判失败。
                if body.len() > 65536 {
                    let _ = ws.close(None);
                    return Err("自检回包太大却验不过".to_string());
                }
                let text = String::from_utf8_lossy(&body);
                if text.contains("200") && text.contains("Example Domain") {
                    let _ = ws.close(None);
                    return Ok(start.elapsed().as_millis() as u64);
                }
            }
            // Ping 等控制帧忽略；底层 10 秒读超时会结束等待，不会卡死。
            Ok(_) => {}
            Err(e) => return Err(format!("自检读回包失败: {e}")),
        }
    }
}

/// 延迟分档：250ms 内绿，500ms 内蓝，能通但更慢红，不通灰。
/// 阈值对着延迟面板的观感调的（绿 ~150-210，蓝 ~250-330）。
pub fn latency_tone(ms: Option<u64>) -> LatencyTone {
    match ms {
        None => LatencyTone::Dead,
        Some(ms) if ms <= 250 => LatencyTone::Fast,
        Some(ms) if ms <= 500 => LatencyTone::Normal,
        Some(_) => LatencyTone::Slow,
    }
}

/// 双向中继：单线程轮询两边，短超时保证一边有数据另一边不饿死。
/// 浏览器先挂就收尾关 WS；远端先挂就直接返回，调用方关连接。
fn relay(
    mut client: TcpStream,
    mut ws: tungstenite::WebSocket<native_tls::TlsStream<TcpStream>>,
    hello: Vec<u8>,
    meter: &ConnMeter,
) -> Result<(), String> {
    use tungstenite::{Error, Message};
    client
        .set_read_timeout(Some(RELAY_POLL))
        .map_err(|e| format!("设读超时失败: {e}"))?;
    client
        .set_write_timeout(Some(DIAL_TIMEOUT))
        .map_err(|e| format!("设写超时失败: {e}"))?;
    // WS 底层的 TCP 读超时也要短，否则 ws.read() 一卡就是 10 秒，
    // 浏览器方向的数据就发不出去了。
    ws.get_ref()
        .get_ref()
        .set_read_timeout(Some(RELAY_POLL))
        .map_err(|e| format!("设远端读超时失败: {e}"))?;
    ws.get_ref()
        .get_ref()
        .set_write_timeout(Some(DIAL_TIMEOUT))
        .map_err(|e| format!("设远端写超时失败: {e}"))?;
    ws.send(Message::Binary(hello.into()))
        .map_err(|e| format!("发 VLESS 握手失败: {e}"))?;
    // 响应头吃掉，多带的数据先发给浏览器，再进循环。
    let pending = read_vless_response(&mut ws)?;
    if !pending.is_empty() {
        client
            .write_all(&pending)
            .map_err(|e| format!("写本地失败: {e}"))?;
    }
    let mut buf = [0u8; CHUNK];
    loop {
        // 界面点了断开全部就收尾：流不用掐完，关连接就行。
        if meter.is_killed() {
            return Ok(());
        }
        // 浏览器 → 远端。
        match client.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                meter.add_up(n as u64);
                ws.send(Message::Binary(buf[..n].to_vec().into()))
                    .map_err(|e| format!("发远端失败: {e}"))?;
            }
            Err(e) if is_timeout(&e) => {}
            // 浏览器 RST/Abort（预连接、取消导航、池化回收）是日常，
            // 不算错误，直接正常收尾。
            Err(e) if is_client_gone(&e) => return Ok(()),
            Err(e) => return Err(format!("读本地失败: {e}")),
        }
        // 远端 → 浏览器。
        match ws.read() {
            Ok(Message::Binary(data)) => {
                meter.add_down(data.len() as u64);
                if let Err(e) = client.write_all(&data) {
                    // 写不进去说明浏览器已经走了，同样正常收尾。
                    if is_client_gone(&e) {
                        return Ok(());
                    }
                    return Err(format!("写本地失败: {e}"));
                }
            }
            Ok(Message::Ping(data)) => {
                let _ = ws.send(Message::Pong(data));
            }
            Ok(Message::Close(_)) => break,
            // 文本/Pong 帧：VLESS 只走二进制，其余忽略。
            Ok(_) => {}
            Err(Error::Io(e)) if is_timeout(&e) => {}
            Err(Error::ConnectionClosed) | Err(Error::AlreadyClosed) => break,
            // 远端 RST/Abort（空闲超时掐线最常见）：这条连接已经没了，
            // 正常收尾，不当错误。
            Err(Error::Io(e)) if is_client_gone(&e) => break,
            // 远端直接断 TCP 不发 WS 关闭帧：VLESS 服务端常见收尾，
            // 数据已经收完，算正常结束，不当错误。
            Err(Error::Protocol(
                tungstenite::error::ProtocolError::ResetWithoutClosingHandshake,
            )) => break,
            Err(e) => return Err(format!("读远端失败: {e}")),
        }
    }
    let _ = ws.close(None);
    Ok(())
}

/// 直连中继：裸 TCP 对裸 TCP，和 `relay` 同一个单线程轮询形状，
/// 只是两边都不包 WS。读写任意一边没了都算正常收尾（对端先挂常见）。
fn relay_direct(
    mut client: TcpStream,
    target: &Target,
    meter: &ConnMeter,
) -> Result<(), String> {
    let mut last_err = format!("{} 解析不到地址", target.host);
    let mut remote_ok: Option<TcpStream> = None;
    let addrs = (target.host.as_str(), target.port)
        .to_socket_addrs()
        .map_err(|e| format!("解析 {} 失败: {e}", target.host))?;
    // 多个地址挨个试，和出站拨号一个策略。
    for addr in addrs {
        match TcpStream::connect_timeout(&addr, DIAL_TIMEOUT) {
            Ok(remote) => {
                remote_ok = Some(remote);
                break;
            }
            Err(e) => last_err = format!("直连 {} 失败: {e}", target.host),
        }
    }
    let mut remote = remote_ok.ok_or(last_err)?;
    client
        .set_read_timeout(Some(RELAY_POLL))
        .map_err(|e| format!("设读超时失败: {e}"))?;
    client
        .set_write_timeout(Some(DIAL_TIMEOUT))
        .map_err(|e| format!("设写超时失败: {e}"))?;
    remote
        .set_read_timeout(Some(RELAY_POLL))
        .map_err(|e| format!("设远端读超时失败: {e}"))?;
    remote
        .set_write_timeout(Some(DIAL_TIMEOUT))
        .map_err(|e| format!("设远端写超时失败: {e}"))?;
    let mut buf = [0u8; CHUNK];
    loop {
        // 界面点了断开全部就收尾：流不用掐完，关连接就行。
        if meter.is_killed() {
            return Ok(());
        }
        // 浏览器 → 远端。
        match client.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                meter.add_up(n as u64);
                if let Err(e) = remote.write_all(&buf[..n]) {
                    if is_client_gone(&e) {
                        return Ok(());
                    }
                    return Err(format!("直连发远端失败: {e}"));
                }
            }
            Err(e) if is_timeout(&e) => {}
            Err(e) if is_client_gone(&e) => return Ok(()),
            Err(e) => return Err(format!("直连读本地失败: {e}")),
        }
        // 远端 → 浏览器。
        match remote.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                meter.add_down(n as u64);
                if let Err(e) = client.write_all(&buf[..n]) {
                    if is_client_gone(&e) {
                        return Ok(());
                    }
                    return Err(format!("直连写本地失败: {e}"));
                }
            }
            Err(e) if is_timeout(&e) => {}
            Err(e) if is_client_gone(&e) => break,
            Err(e) => return Err(format!("直连读远端失败: {e}")),
        }
    }
    Ok(())
}

/// 读超时（含 WouldBlock）：轮询空转的正常情况，不是错误。
fn is_timeout(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
    )
}

/// 对端没了：Abort（Windows 10053）、Reset（10054）、BrokenPipe。
/// 浏览器天天这么断连接（预连接、取消、池化回收），远端空闲掐线也常见，
/// 都算正常收尾，不刷错误日志。
fn is_client_gone(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::ConnectionAborted
            | std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::BrokenPipe
    )
}

#[cfg(test)]
mod tests {
    // 注意：这里不要 `use super::*`——它会把 gpui 自带的 `#[test]` 宏引进
    // 来，盖住 Rust 内置的 test 属性导致递归展开失败。只显式引入用到的项。
    use super::LatencyTone;
    use super::build_vless_handshake;
    use super::http_head_len;
    use super::is_client_gone;
    use super::latency_tone;
    use super::internet_refresh_script;
    use super::mixed_addr;
    use super::parse_authority;
    use super::parse_http_target;
    use super::GroupMember;
    use super::default_selections;
    use super::parse_socks5_request;
    use super::restore_selections;
    use super::split_vless_response;
    use std::collections::HashMap;
    use super::parse_subscription;
    use super::resolve_members;
    use super::parse_uuid_bytes;
    use super::reg_set_dword_args;
    use super::reg_set_string_args;
    use super::MIXED_PORT;

    /// 订阅样例：2 个 VLESS（一个字段齐全、一个缺 Host/path）+ 1 个 VMess（要丢掉）。
    const SAMPLE: &str = r#"
proxies:
  - name: "sg-full"
    type: vless
    server: "www.shopify.com"
    port: 443
    uuid: 4dfae26b-42b2-4c31-bed4-7836307a3986
    network: ws
    tls: true
    servername: "demo.trycloudflare.com"
    ws-opts:
      path: "/"
      headers:
        Host: "demo.trycloudflare.com"
  - name: plain-min
    type: vless
    server: "example.com"
    port: 443
    uuid: 4dfae26b-42b2-4c31-bed4-7836307a3986
  - name: drop-me
    type: vmess
    server: "1.2.3.4"
    port: 80
    uuid: 4dfae26b-42b2-4c31-bed4-7836307a3986
proxy-groups:
  - name: "select"
    type: select
    proxies:
      - "sg-full"
      - DIRECT
      - "auto"
  - name: "auto"
    type: url-test
    proxies:
      - "plain-min"
      - "ghost"
"#;

    #[test]
    fn subscription_keeps_only_vless_with_defaults() {
        let nodes = parse_subscription(SAMPLE).nodes;
        assert_eq!(nodes.len(), 2, "VMess 条目必须丢掉，只留 2 个 VLESS");
        let full = &nodes[0];
        assert_eq!(full.name, "sg-full");
        assert_eq!(full.server, "www.shopify.com");
        assert_eq!(full.port, 443);
        assert_eq!(full.sni, "demo.trycloudflare.com");
        assert_eq!(full.host, "demo.trycloudflare.com");
        assert_eq!(full.path, "/");
        // 缺字段的走缺省：SNI/Host 跟 server，路径补 "/"。
        let min = &nodes[1];
        assert_eq!(min.sni, "example.com");
        assert_eq!(min.host, "example.com");
        assert_eq!(min.path, "/");
    }

    #[test]
    fn subscription_ignores_group_section() {
        // `proxy-groups:` 下的 `- name:` 绝不能被当成节点。
        let sub = parse_subscription("proxies:\n  - name: n1\n    type: vless\n    server: a.com\n    uuid: 4dfae26b-42b2-4c31-bed4-7836307a3986\nproxy-groups:\n  - name: select\n");
        assert_eq!(sub.nodes.len(), 1);
        assert_eq!(sub.nodes[0].name, "n1");
        // 没有成员的组直接丢掉，不进界面。
        assert!(sub.groups.is_empty(), "空组不能留下来");
    }

    #[test]
    fn groups_parse_kind_and_members() {
        let sub = parse_subscription(SAMPLE);
        assert_eq!(sub.groups.len(), 2, "两个组都要收下来");
        let select = &sub.groups[0];
        assert_eq!(select.name, "select");
        assert_eq!(select.kind, "select");
        assert_eq!(select.members, vec!["sg-full", "DIRECT", "auto"]);
        let auto = &sub.groups[1];
        assert_eq!(auto.kind, "url-test");
    }

    #[test]
    fn group_members_resolve_to_cards() {
        let sub = parse_subscription(SAMPLE);
        let cards = resolve_members(&sub, &sub.groups[0]);
        assert_eq!(cards.len(), 3, "节点、关键字、嵌套组各一张卡");
        assert!(matches!(cards[0], GroupMember::Node(n) if n.name == "sg-full"));
        assert!(matches!(cards[1], GroupMember::Keyword("DIRECT")));
        assert!(matches!(cards[2], GroupMember::Group("auto")));
        // 对不上的名字（ghost）直接丢掉，不占卡片。
        let auto_cards = resolve_members(&sub, &sub.groups[1]);
        assert_eq!(auto_cards.len(), 1, "ghost 必须丢掉，只留 plain-min");
    }

    #[test]
    fn default_selections_take_first_member() {
        let sub = parse_subscription(SAMPLE);
        let picks = default_selections(&sub);
        assert_eq!(picks.get("select").map(String::as_str), Some("sg-full"));
        assert_eq!(picks.get("auto").map(String::as_str), Some("plain-min"));
        // 第一顺位是关键字也照收（Clash 里 DIRECT 开头的组很多）。
        let sub = parse_subscription(
            "proxies:\n  - name: n1\n    type: vless\n    server: a.com\n    uuid: 4dfae26b-42b2-4c31-bed4-7836307a3986\nproxy-groups:\n  - name: g\n    type: select\n    proxies:\n      - DIRECT\n      - n1\n",
        );
        let picks = default_selections(&sub);
        assert_eq!(picks.get("g").map(String::as_str), Some("DIRECT"));
    }

    #[test]
    fn restore_selections_trusts_saved_only_when_valid() {
        let sub = parse_subscription(SAMPLE);
        let mut saved = HashMap::new();
        // select 组存的值不在本组（plain-min 是别组的），沿用默认；
        // auto 组存的值有效就用存的；不存在的组直接忽略。
        saved.insert("select".to_string(), "plain-min".to_string());
        saved.insert("auto".to_string(), "plain-min".to_string());
        saved.insert("gone".to_string(), "sg-full".to_string());
        let picks = restore_selections(&sub, &saved);
        assert_eq!(
            picks.get("select").map(String::as_str),
            Some("sg-full"),
            "存的值不在本组，沿用默认"
        );
        assert_eq!(
            picks.get("auto").map(String::as_str),
            Some("plain-min"),
            "存的值有效就用存的"
        );
        assert!(!picks.contains_key("gone"), "订阅里没这个组就丢掉");
    }

    #[test]
    fn uuid_parses_with_and_without_dashes() {
        let dashed = parse_uuid_bytes("4dfae26b-42b2-4c31-bed4-7836307a3986").expect("带横线能解析");
        let plain = parse_uuid_bytes("4dfae26b42b24c31bed47836307a3986").expect("不带横线能解析");
        assert_eq!(dashed, plain);
        assert_eq!(dashed[0], 0x4d);
        assert!(parse_uuid_bytes("short").is_err(), "太短必须报错");
        assert!(
            parse_uuid_bytes("zzzzzzzz-42b2-4c31-bed4-7836307a3986").is_err(),
            "非十六进制必须报错"
        );
    }

    #[test]
    fn vless_handshake_layout_matches_spec() {
        let uuid = parse_uuid_bytes("4dfae26b-42b2-4c31-bed4-7836307a3986").expect("能解析");
        let hello = build_vless_handshake(&uuid, "example.com", 443).expect("能拼");
        // 版本 + UUID + 附加长度 + 命令 + 端口 + 地址类型 + 域名长度。
        assert_eq!(hello[0], 0x00, "版本必须是 0");
        assert_eq!(&hello[1..17], &uuid, "UUID 紧跟版本");
        assert_eq!(hello[17], 0x00, "附加长度必须是 0");
        assert_eq!(hello[18], 0x01, "命令必须是 TCP(1)");
        assert_eq!(&hello[19..21], &[0x01, 0xbb], "443 的大端");
        assert_eq!(hello[21], 0x02, "域名类型是 2（不是 SOCKS5 的 3）");
        assert_eq!(hello[22] as usize, "example.com".len());
        assert_eq!(&hello[23..], b"example.com");
    }

    #[test]
    fn vless_handshake_ip_uses_numeric_atyp() {
        let uuid = [0u8; 16];
        let v4 = build_vless_handshake(&uuid, "1.2.3.4", 80).expect("IPv4 能拼");
        assert_eq!(v4[21], 0x01, "IPv4 类型是 1");
        assert_eq!(&v4[22..26], &[1, 2, 3, 4]);
        let v6 = build_vless_handshake(&uuid, "::1", 80).expect("IPv6 能拼");
        assert_eq!(v6[21], 0x03, "IPv6 类型是 3");
        // 版本 1 + UUID 16 + 附加 1 + 命令 1 + 端口 2 + 类型 1 + 地址 16。
        assert_eq!(v6.len(), 1 + 16 + 1 + 1 + 2 + 1 + 16);
        assert!(build_vless_handshake(&uuid, "", 80).is_err(), "空域名必须报错");
    }

    #[test]
    fn socks5_request_parses_all_atyps() {
        // IPv4：VER CMD RSV ATYP + 4 字节 + 端口。
        let v4 = parse_socks5_request(&[5, 1, 0, 1, 1, 2, 3, 4, 0, 80]).expect("IPv4 能解析");
        assert_eq!(v4.host, "1.2.3.4");
        assert_eq!(v4.port, 80);
        // 域名：长度 + 内容 + 端口。
        let mut domain = vec![5, 1, 0, 3, 11];
        domain.extend_from_slice(b"example.com");
        domain.extend_from_slice(&[0x01, 0xbb]);
        let parsed = parse_socks5_request(&domain).expect("域名能解析");
        assert_eq!(parsed.host, "example.com");
        assert_eq!(parsed.port, 443);
        // IPv6：16 字节（::1）+ 端口 443。
        let mut v6 = vec![5, 1, 0, 4];
        v6.extend_from_slice(&[0u8; 15]);
        v6.push(1);
        v6.extend_from_slice(&[0x01, 0xbb]);
        let parsed = parse_socks5_request(&v6).expect("IPv6 能解析");
        assert_eq!(parsed.port, 443);
        // UDP ASSOCIATE 必须拒掉。
        assert!(
            parse_socks5_request(&[5, 3, 0, 1, 1, 2, 3, 4, 0, 80]).is_err(),
            "非 CONNECT 必须报错"
        );
    }

    #[test]
    fn http_head_boundary_is_found() {
        let buf = b"CONNECT a.com:443 HTTP/1.1\r\nHost: a.com\r\n\r\n";
        assert_eq!(http_head_len(buf), Some(buf.len()));
        assert_eq!(http_head_len(b"GET / HTTP/1.1\r\nHost: a"), None);
    }

    #[test]
    fn authority_parsing_covers_brackets_and_defaults() {
        let target = parse_authority("example.com:8443", 443).expect("带端口能解析");
        assert_eq!((target.host.as_str(), target.port), ("example.com", 8443));
        let target = parse_authority("example.com", 443).expect("缺端口走缺省");
        assert_eq!(target.port, 443);
        let target = parse_authority("[::1]:443", 80).expect("方括号 IPv6 能解析");
        assert_eq!((target.host.as_str(), target.port), ("::1", 443));
        assert!(parse_authority("", 443).is_err(), "空地址必须报错");
        assert!(parse_authority("a.com:notaport", 443).is_err(), "坏端口必须报错");
    }

    #[test]
    fn http_target_routing_matches_method() {
        let target = parse_http_target("CONNECT", "example.com:443", None).expect("CONNECT 能解析");
        assert_eq!((target.host.as_str(), target.port), ("example.com", 443));
        let target = parse_http_target("GET", "http://example.com/path", None).expect("绝对 URI 能解析");
        assert_eq!((target.host.as_str(), target.port), ("example.com", 80));
        let target = parse_http_target("GET", "/path", Some("example.com:8080")).expect("Host 头能解析");
        assert_eq!((target.host.as_str(), target.port), ("example.com", 8080));
        assert!(
            parse_http_target("GET", "/path", None).is_err(),
            "没 Host 头的 origin-form 必须报错"
        );
    }

    #[test]
    fn reg_commands_target_internet_settings() {
        let enable = reg_set_dword_args("ProxyEnable", 1);
        assert_eq!(&enable[0], "add");
        assert!(enable.contains(&"ProxyEnable".to_string()));
        assert!(enable.contains(&1.to_string()));
        let server = reg_set_string_args("ProxyServer", "127.0.0.1:10809");
        assert!(server.contains(&"ProxyServer".to_string()));
        assert!(server.contains(&"127.0.0.1:10809".to_string()));
        // 键路径必须是 HKCU 互联网设置（HKCU 不用管理员权限）。
        assert!(enable.iter().any(|a| a.contains("Internet Settings")));
    }

    #[test]
    fn refresh_script_pokes_wininet() {
        let script = internet_refresh_script();
        assert!(script.contains("wininet.dll"), "必须调 wininet");
        assert!(script.contains("InternetSetOption"), "必须调 InternetSetOption");
        assert!(script.contains("39"), "先发 SETTINGS_CHANGED(39)");
        assert!(script.contains("37"), "再发 REFRESH(37)");
    }

    #[test]
    fn client_abort_is_quiet_shutdown_not_error() {
        use std::io::ErrorKind;
        // Windows 10053/10054、BrokenPipe：浏览器和远端的日常断线。
        for kind in [
            ErrorKind::ConnectionAborted,
            ErrorKind::ConnectionReset,
            ErrorKind::BrokenPipe,
        ] {
            assert!(
                is_client_gone(&std::io::Error::new(kind, "gone")),
                "{kind:?} 应该算正常收尾"
            );
        }
        assert!(!is_client_gone(&std::io::Error::new(ErrorKind::TimedOut, "slow")));
        assert!(!is_client_gone(&std::io::Error::new(
            ErrorKind::Other,
            "boom"
        )));
    }

    #[test]
    fn vless_response_header_splits_cleanly() {
        // 长度不够继续攒，不报错。
        assert!(split_vless_response(&[]).expect("空不报错").is_none());
        assert!(split_vless_response(&[0x00]).expect("1 字节不报错").is_none());
        // 版本不对直接报错：对端不是 VLESS。
        assert!(split_vless_response(&[0x01, 0x00]).is_err());
        // 标准 2 字节头 + 数据：头吃掉，数据原样吐回。
        let (len, rest) = split_vless_response(&[0x00, 0x00, b'H', b'i'])
            .expect("能切")
            .expect("够长");
        assert_eq!((len, rest), (2, b"Hi".as_slice()));
        // 光头没数据：余部为空。
        let (len, rest) = split_vless_response(&[0x00, 0x00])
            .expect("能切")
            .expect("够长");
        assert_eq!((len, rest.len()), (2, 0));
        // 带附加的头：附加一起吃掉。
        let (len, rest) = split_vless_response(&[0x00, 0x02, 0xaa, 0xbb, b'X'])
            .expect("能切")
            .expect("够长");
        assert_eq!((len, rest), (4, b"X".as_slice()));
        // 附加没收齐继续攒。
        assert!(split_vless_response(&[0x00, 0x02, 0xaa])
            .expect("不报错")
            .is_none());
    }

    /// 分流样例：2 节点 + 3 组 + 覆盖全支持类型的规则。
    const RULE_SAMPLE: &str = r#"
proxies:
  - name: "sg-1"
    type: vless
    server: "a.com"
    port: 443
    uuid: 4dfae26b-42b2-4c31-bed4-7836307a3986
  - name: "us-1"
    type: vless
    server: "b.com"
    port: 443
    uuid: 4dfae26b-42b2-4c31-bed4-7836307a3986
proxy-groups:
  - name: "proxy"
    type: select
    proxies:
      - "sg-1"
      - "us-1"
  - name: "direct"
    type: select
    proxies:
      - DIRECT
      - "proxy"
  - name: "auto"
    type: url-test
    proxies:
      - "sg-1"
      - "us-1"
rules:
  - DOMAIN,exact.com,proxy
  - DOMAIN-SUFFIX,example.com,direct
  - DOMAIN-SUFFIX,sub.example.com,proxy
  - DOMAIN-KEYWORD,adservice,proxy
  - IP-CIDR,10.0.0.0/8,direct
  - IP-CIDR6,::1/128,direct
  - GEOIP,CN,direct
  - IP-CIDR,999.1.1.1/24,proxy
  - MATCH,proxy
"#;

    #[test]
    fn rules_parse_kinds_and_skip_bad_cidr() {
        use super::Matcher;
        let sub = parse_subscription(RULE_SAMPLE);
        // 写坏的网段整条丢掉，剩下 8 条。
        assert_eq!(sub.rules.len(), 8, "坏 CIDR 那条必须丢掉");
        assert!(matches!(sub.rules[0].matcher, Matcher::Domain(_)));
        assert!(matches!(sub.rules[1].matcher, Matcher::DomainSuffix(_)));
        assert!(matches!(sub.rules[3].matcher, Matcher::DomainKeyword(_)));
        assert!(matches!(sub.rules[4].matcher, Matcher::IpCidr(_)));
        assert!(matches!(sub.rules[6].matcher, Matcher::Unsupported(_)));
        assert!(matches!(sub.rules[7].matcher, Matcher::Match));
        assert_eq!(sub.rules[7].target, "proxy");
    }

    #[test]
    fn suffix_match_first_rule_wins_not_longest() {
        use super::RuleSet;
        let sub = parse_subscription(RULE_SAMPLE);
        let ruleset = RuleSet::build(&sub.rules);
        // `sub.example.com` 同时命中第 2 条（example.com）和第 3 条，
        // 赢的是排前面的第 2 条，不是更长的第 3 条。
        assert_eq!(ruleset.match_index(&sub.rules, "sub.example.com"), Some(1));
        assert_eq!(ruleset.match_index(&sub.rules, "other.example.com"), Some(1));
        // 根域名本身也命中后缀规则（Clash 语义）。
        assert_eq!(ruleset.match_index(&sub.rules, "example.com"), Some(1));
        // 大小写不敏感。
        assert_eq!(ruleset.match_index(&sub.rules, "SUB.EXAMPLE.COM"), Some(1));
        // 精确和关键字正常工作。
        assert_eq!(ruleset.match_index(&sub.rules, "exact.com"), Some(0));
        assert_eq!(
            ruleset.match_index(&sub.rules, "x.adservice.y.com"),
            Some(3)
        );
        // GEOIP（序号 6）跳过，CN 相关域名一路掉到 MATCH（序号 7）。
        assert_eq!(ruleset.match_index(&sub.rules, "anything.cn"), Some(7));
    }

    #[test]
    fn cidr_only_matches_literal_ips() {
        use super::CidrBlock;
        let v4 = CidrBlock::parse("10.0.0.0/8").expect("能解析");
        assert!(v4.matches("10.1.2.3"));
        assert!(!v4.matches("11.0.0.1"));
        // v4 网段不认 v6 地址。
        assert!(!v4.matches("::1"));
        let v6 = CidrBlock::parse("::1/128").expect("能解析");
        assert!(v6.matches("::1"));
        assert!(!v6.matches("::2"));
        // 域名不解析，直接不中。
        assert!(!v4.matches("example.com"));
        // 非法输入返回 None，不 panic。
        assert!(CidrBlock::parse("10.0.0.0/33").is_none());
        assert!(CidrBlock::parse("nope").is_none());
        let ruleset = super::RuleSet::build(&parse_subscription(RULE_SAMPLE).rules);
        let sub = parse_subscription(RULE_SAMPLE);
        assert_eq!(ruleset.match_index(&sub.rules, "10.9.9.9"), Some(4));
    }

    #[test]
    fn select_route_covers_direct_node_reject() {
        use super::Route;
        use super::RuleSet;
        use super::select_route_detail;
        let sub = parse_subscription(RULE_SAMPLE);
        let ruleset = RuleSet::build(&sub.rules);
        let no_picks: HashMap<String, String> = HashMap::new();
        let no_lat: HashMap<String, Option<u64>> = HashMap::new();
        // 后缀命中 direct 组：组内第一顺位是 DIRECT，直接走直连。
        assert_eq!(
            select_route_detail(&sub, &sub.rules, &ruleset, &no_picks, &no_lat, "a.example.com")
                .route,
            Route::Direct
        );
        // 精确命中 proxy 组：组内第一顺位是 sg-1。
        assert_eq!(
            select_route_detail(&sub, &sub.rules, &ruleset, &no_picks, &no_lat, "exact.com")
                .route,
            Route::Node(sub.nodes[0].clone())
        );
        // 手动把 proxy 组切到 us-1，路由跟着走。
        let mut picks = HashMap::new();
        picks.insert("proxy".to_string(), "us-1".to_string());
        assert_eq!(
            select_route_detail(&sub, &sub.rules, &ruleset, &picks, &no_lat, "exact.com")
                .route,
            Route::Node(sub.nodes[1].clone())
        );
        // 存的值对不上（节点改名了），自愈回第一顺位，不断流。
        picks.insert("proxy".to_string(), "gone".to_string());
        assert_eq!(
            select_route_detail(&sub, &sub.rules, &ruleset, &picks, &no_lat, "exact.com")
                .route,
            Route::Node(sub.nodes[0].clone())
        );
    }

    #[test]
    fn url_test_group_picks_fastest_measured() {
        use super::Route;
        use super::RuleSet;
        use super::select_route_detail;
        let sub = parse_subscription(RULE_SAMPLE);
        // auto 组是 url-test：sg-1 更快就走 sg-1，无视手动选择。
        let mut latencies = HashMap::new();
        latencies.insert("sg-1".to_string(), Some(300));
        latencies.insert("us-1".to_string(), Some(100));
        let mut picks = HashMap::new();
        picks.insert("auto".to_string(), "sg-1".to_string());
        // 借一条排最前的规则把目标直接指到 auto 组，测嵌套解析。
        let mut sub2 = sub.clone();
        sub2.rules.insert(
            0,
            super::Rule {
                matcher: super::Matcher::Domain("auto.test".to_string()),
                target: "auto".to_string(),
            },
        );
        let ruleset2 = RuleSet::build(&sub2.rules);
        assert_eq!(
            select_route_detail(&sub2, &sub2.rules, &ruleset2, &picks, &latencies, "auto.test")
                .route,
            Route::Node(sub.nodes[1].clone()),
            "us-1 更快，url-test 必须选它"
        );
    }

    fn manual_rule(kind: super::ManualKind, pattern: &str, target: &str, sort: i64) -> super::ManualRule {
        use std::sync::atomic::{AtomicI64, Ordering};
        static SEQ: AtomicI64 = AtomicI64::new(0);
        super::ManualRule {
            id: format!("t{}", SEQ.fetch_add(1, Ordering::Relaxed)),
            kind,
            pattern: pattern.to_string(),
            target: target.to_string(),
            sort,
        }
    }

    #[test]
    fn effective_rules_manual_first_and_sorted() {
        use super::ManualKind;
        use super::effective_rules;
        let sub = parse_subscription(RULE_SAMPLE);
        let manual = vec![
            manual_rule(ManualKind::Suffix, "vip.example.com", "DIRECT", 20),
            manual_rule(ManualKind::Exact, "exact.com", "REJECT", 10),
            // 坏网段进不了有效表。
            manual_rule(ManualKind::Cidr, "999.1.1.1/24", "DIRECT", 5),
        ];
        let (rules, manual_len) = effective_rules(&manual, &sub.rules);
        assert_eq!(manual_len, 2, "坏网段那条必须丢掉");
        assert_eq!(rules.len(), sub.rules.len() + 2);
        // 手动按 sort 排前面：sort=10 的精确先于 sort=20 的后缀。
        assert_eq!(super::rule_text(&rules[0]), "DOMAIN,exact.com");
        assert_eq!(super::rule_text(&rules[1]), "DOMAIN-SUFFIX,vip.example.com");
        assert_eq!(super::rule_text(&rules[2]), "DOMAIN,exact.com");
    }

    #[test]
    fn lookup_returns_hits_and_winner() {
        use super::ManualKind;
        use super::Route;
        use super::lookup_route;
        let sub = parse_subscription(RULE_SAMPLE);
        let empty_map: HashMap<String, String> = HashMap::new();
        let empty_lat: HashMap<String, Option<u64>> = HashMap::new();
        let manual = vec![manual_rule(
            ManualKind::Suffix,
            "vip.example.com",
            "REJECT",
            0,
        )];
        // 手动后缀赢了自动后缀和 MATCH：命中三条，赢的是手动那条，路由拦截。
        let (hits, route) = lookup_route(&sub, &manual, &empty_map, &empty_lat, "vip.example.com");
        assert_eq!(hits.len(), 3, "手动、自动后缀、MATCH 各中一条");
        assert!(hits[0].manual, "赢的必须是手动");
        assert!(!hits[1].manual);
        assert!(!hits[2].manual);
        assert_eq!(route, Route::Reject);
        // 空输入直接拦截，不扫表。
        let (hits, route) = lookup_route(&sub, &manual, &empty_map, &empty_lat, "  ");
        assert!(hits.is_empty());
        assert_eq!(route, Route::Reject);
    }

    #[test]
    fn conn_registry_tracks_and_prunes() {
        use super::ConnRegistry;
        use super::ConnSnapshot;
        fn snap(host: &str) -> ConnSnapshot {
            ConnSnapshot {
                id: 0,
                host: host.to_string(),
                source: "127.0.0.1:1".to_string(),
                dest: host.to_string(),
                kind: "HTTPS(tcp)".to_string(),
                rule: "Match".to_string(),
                chain: "g".to_string(),
                route: "n".to_string(),
                process: "–".to_string(),
                started_ms: 0,
                up: 0,
                down: 0,
                active: true,
            }
        }
        let mut registry = ConnRegistry::default();
        let a = registry.register(snap("a.com:443"));
        let b = registry.register(snap("b.com:80"));
        assert_ne!(a, b, "id 必须唯一");
        registry.add_up(a, 100);
        registry.add_down(a, 200);
        let live = registry.snapshot().iter().filter(|r| r.active).count();
        assert_eq!((live, registry.snapshot().len() - live), (2, 0));
        registry.finish(a);
        let rows = registry.snapshot();
        assert_eq!(rows.len(), 2);
        // 活的排前面，关的沉底。
        assert!(rows[0].active);
        assert!(!rows[1].active);
        assert_eq!(rows[1].up, 100);
        assert_eq!(rows[1].down, 200);
        // 不存在的 id 加减不 panic 也不影响别人。
        registry.add_up(999999, 1);
        registry.finish(999999);
        assert_eq!(registry.snapshot().len(), 2);
        // 断开全部只给活的举旗，关了的不动。
        registry.kill_all();
        assert!(registry.is_killed(b), "活的要被举旗");
        assert!(!registry.is_killed(a), "关了的不用再举");
        assert!(!registry.is_killed(999999), "不存在的 id 当没被举");
        // 都结束了再清：只剩 0 条，活的一个不少（这俩都关了，全清）。
        registry.finish(b);
        registry.clear_closed();
        assert!(registry.snapshot().is_empty(), "关了的清完表就空了");
        let _ = b;
    }

    #[test]
    fn format_helpers_cover_units_and_ages() {
        use super::format_age;
        use super::format_bytes;
        use super::format_speed;
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(1023), "1023 B");
        assert_eq!(format_bytes(2048), "2.00 KB");
        assert_eq!(format_bytes(3 * 1024 * 1024), "3.00 MB");
        assert_eq!(format_speed(0), "0 B/s");
        assert_eq!(format_speed(2048), "2.00 KB/s");
        assert_eq!(format_age(5000), "5秒前");
        assert_eq!(format_age(125000), "2分钟前");
        assert_eq!(format_age(7300000), "2小时前");
    }

    #[test]
    fn route_detail_carries_rule_and_chain() {
        use super::select_route_detail;
        let sub = parse_subscription(RULE_SAMPLE);
        let ruleset = super::RuleSet::build(&sub.rules);
        let empty_map: HashMap<String, String> = HashMap::new();
        let empty_lat: HashMap<String, Option<u64>> = HashMap::new();
        // 后缀命中 direct 组：链上记组名，终点 DIRECT。
        let detail = select_route_detail(
            &sub,
            &sub.rules,
            &ruleset,
            &empty_map,
            &empty_lat,
            "a.example.com",
        );
        assert_eq!(detail.rule_index, Some(1));
        assert_eq!(detail.rule_text, "DomainSuffix(example.com)");
        assert_eq!(detail.target, "direct");
        // 链上记走过的组 + 终点：进了 direct 组，终点是 DIRECT。
        assert_eq!(detail.chain, vec!["direct".to_string(), "DIRECT".to_string()]);
        assert_eq!(detail.route, super::Route::Direct);
        // 一条没中：链上只剩 REJECT，去向拦截。
        let detail = select_route_detail(
            &sub,
            &[],
            &super::RuleSet::build(&[]),
            &empty_map,
            &empty_lat,
            "a.example.com",
        );
        assert_eq!(detail.rule_index, None);
        assert_eq!(detail.chain, vec!["REJECT".to_string()]);
        assert_eq!(detail.route, super::Route::Reject);
    }

    #[test]
    fn latency_tone_grades_boundaries() {
        assert_eq!(latency_tone(None), LatencyTone::Dead, "不通是灰");
        assert_eq!(latency_tone(Some(0)), LatencyTone::Fast);
        assert_eq!(latency_tone(Some(250)), LatencyTone::Fast, "250 是绿的上限");
        assert_eq!(latency_tone(Some(251)), LatencyTone::Normal);
        assert_eq!(latency_tone(Some(500)), LatencyTone::Normal, "500 是蓝的上限");
        assert_eq!(latency_tone(Some(501)), LatencyTone::Slow);
    }

    #[test]
    fn mixed_endpoint_is_sane() {
        assert_eq!(MIXED_PORT, 10809);
        assert_eq!(mixed_addr(), "127.0.0.1:10809");
    }
}
