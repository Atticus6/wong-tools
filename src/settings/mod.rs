//! 设置持久化：libsql 本地库。
//!
//! 位置：Windows `%APPDATA%\WongTools\settings.db`，macOS
//! `~/Library/Application Support/WongTools/settings.db`，其他系统
//! `$XDG_CONFIG_HOME/wong-tools/settings.db`（没有则 `~/.config/...`）。
//! 键值表（`settings`）放小设置，自动规则另有独立表（`proxy_rules_auto`）。
//!
//! 读写失败一律吞掉（读返回 `None`、写记一笔日志）：设置丢了可以重选，
//! 绝不能把程序崩掉。libsql 全是 async 接口，设置读写又都是低频同步
//! 调用，所以每次现建一个 current-thread runtime 跑完就扔，比常驻便宜。
//!
//! 测试钩子：`WONG_SETTINGS_DB` 环境变量能把库指向别处，测试用它隔离，
//! 绝不碰真实数据目录（见 `TempSettings`）。
//!
//! 设置页视图在 `view`，持久化只管读写，不管界面。

pub mod view;

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

const DB_FILE: &str = "settings.db";
const KEY_THEME: &str = "theme";
const KEY_LANGUAGE: &str = "language";
/// 代理订阅地址（用户在代理页可改）。
const KEY_PROXY_SUB: &str = "proxy_sub";
/// 代理各组选中（组名 → 节点名，JSON 存一整张表）。
const KEY_PROXY_SELECTIONS: &str = "proxy_selections";
/// 代理节点（JSON 数组，重启免拉取）。
const KEY_PROXY_NODES: &str = "proxy_nodes";
/// 代理分组（JSON 数组，重启免拉取）。
const KEY_PROXY_GROUPS: &str = "proxy_groups";
/// 旧整份订阅大 JSON（节点 + 分组 + 规则全塞一个值，已废弃）：
/// 只在一次性迁移里读，迁完即删，新代码不再写它。
const KEY_PROXY_SUBSCRIPTION: &str = "proxy_subscription";
/// 代理手动规则（JSON 数组，独立 key：更新订阅只换自动段，手动不动）。
const KEY_PROXY_MANUAL: &str = "proxy_manual_rules";
/// 隧道开着的旗（"1" = 我们开了系统代理）：正常停就清掉，
/// 启动时还看到它，说明上次没收尾，先清系统代理再开窗。
const KEY_PROXY_RUNNING: &str = "proxy_running";
/// 主窗口上次的位置大小（JSON，逻辑像素 + 是否最大化）。
const KEY_WINDOW_BOUNDS: &str = "window_bounds";
/// 侧边栏上次选中的工具序号。
const KEY_SELECTED_TOOL: &str = "selected_tool";

/// 应用数据目录（平台相关，见模块文档）。
pub fn data_dir() -> PathBuf {
    #[cfg(windows)]
    {
        std::env::var("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("."))
            .join("WongTools")
    }
    #[cfg(not(windows))]
    {
        home_dir().join(data_subdir()).join(data_app_dir())
    }
}

/// 主目录。Windows 走 `APPDATA` 用不到它，编译期 gate 掉免得警告。
#[cfg(not(windows))]
fn home_dir() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
}

/// macOS 用 `~/Library/Application Support`，其他用 XDG（没有则 `~/.config`）。
#[cfg(not(windows))]
fn data_subdir() -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        PathBuf::from("Library/Application Support")
    }
    #[cfg(not(target_os = "macos"))]
    {
        std::env::var("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| home_dir().join(".config"))
    }
}

/// macOS/Windows 用 `WongTools`，其他系统用小写的 `wong-tools`（XDG 惯例）。
#[cfg(not(windows))]
fn data_app_dir() -> &'static str {
    #[cfg(target_os = "macos")]
    {
        "WongTools"
    }
    #[cfg(not(target_os = "macos"))]
    {
        "wong-tools"
    }
}

/// 库文件位置。测试时 `WONG_SETTINGS_DB` 可以覆盖它，见 `TempSettings`。
fn db_path() -> PathBuf {
    std::env::var("WONG_SETTINGS_DB")
        .map(PathBuf::from)
        .unwrap_or_else(|_| data_dir().join(DB_FILE))
}

// 调用方全是同步上下文（启动、点击、测试），永远不会嵌进别的 runtime，
// 所以现建现扔，不存在嵌套 panic。
fn block_on<F: std::future::Future>(task: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("settings runtime builds")
        .block_on(task)
}

async fn open(path: &Path) -> Result<libsql::Connection, String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("建数据目录失败: {e}"))?;
    }
    let db = libsql::Builder::new_local(path)
        .build()
        .await
        .map_err(|e| format!("开库失败: {e}"))?;
    let conn = db.connect().map_err(|e| format!("连库失败: {e}"))?;
    conn.execute(
        "CREATE TABLE IF NOT EXISTS settings (key TEXT PRIMARY KEY, value TEXT NOT NULL)",
        (),
    )
    .await
    .map_err(|e| format!("建表失败: {e}"))?;
    // 自动规则独立表：`pos` 保序（分流第一条赢全靠它），一次拉取 8 万行，
    // 大 JSON 单值读写抖得厉害，拆表后只动这张表。
    conn.execute(
        "CREATE TABLE IF NOT EXISTS proxy_rules_auto (pos INTEGER PRIMARY KEY, kind TEXT NOT NULL, arg TEXT NOT NULL, target TEXT NOT NULL)",
        (),
    )
    .await
    .map_err(|e| format!("建规则表失败: {e}"))?;
    Ok(conn)
}

async fn get(conn: &libsql::Connection, key: &str) -> Result<Option<String>, String> {
    let mut rows = conn
        .query("SELECT value FROM settings WHERE key = ? LIMIT 1", [key])
        .await
        .map_err(|e| format!("读设置失败: {e}"))?;
    match rows
        .next()
        .await
        .map_err(|e| format!("读设置失败: {e}"))?
    {
        Some(row) => Ok(Some(
            row.get::<String>(0)
                .map_err(|e| format!("读设置失败: {e}"))?,
        )),
        None => Ok(None),
    }
}

async fn set(conn: &libsql::Connection, key: &str, value: &str) -> Result<(), String> {
    conn.execute(
        "INSERT INTO settings (key, value) VALUES (?, ?) \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        [key, value],
    )
    .await
    .map_err(|e| format!("写设置失败: {e}"))?;
    Ok(())
}

/// 读持久化的主题名；没存过或库坏了都返回 `None`，调用方回落默认主题。
pub fn theme_name() -> Option<String> {
    block_on(async {
        let conn = open(&db_path()).await.ok()?;
        get(&conn, KEY_THEME).await.ok()?
    })
}

/// 存主题名。失败返回错误信息，调用方记一笔日志即可（主题已经换了）。
pub fn set_theme_name(name: &str) -> Result<(), String> {
    block_on(async {
        let conn = open(&db_path()).await?;
        set(&conn, KEY_THEME, name).await
    })
}

/// 读持久化的语言代码（`zh-CN` / `en`）；没有返回 `None`，调用方看环境变量。
pub fn language_code() -> Option<String> {
    block_on(async {
        let conn = open(&db_path()).await.ok()?;
        get(&conn, KEY_LANGUAGE).await.ok()?
    })
}

/// 存语言代码。失败返回错误信息，调用方记一笔日志即可（语言已经切了）。
pub fn set_language_code(code: &str) -> Result<(), String> {
    block_on(async {
        let conn = open(&db_path()).await?;
        set(&conn, KEY_LANGUAGE, code).await
    })
}

/// 读持久化的代理订阅地址；没存过或库坏了返回 `None`，调用方用默认地址。
pub fn proxy_sub_url() -> Option<String> {
    block_on(async {
        let conn = open(&db_path()).await.ok()?;
        get(&conn, KEY_PROXY_SUB).await.ok()?
    })
}

/// 存代理订阅地址。失败返回错误信息，调用方记一笔日志即可。
pub fn set_proxy_sub_url(url: &str) -> Result<(), String> {
    block_on(async {
        let conn = open(&db_path()).await?;
        set(&conn, KEY_PROXY_SUB, url).await
    })
}

/// 读各组选中（组名 → 节点名）。库坏了、没存过、JSON 烂了都返回 `None`，
/// 调用方回落默认选中（烂数据不炸界面，下次点卡自然覆盖）。
pub fn proxy_selections() -> Option<HashMap<String, String>> {
    let raw = block_on(async {
        let conn = open(&db_path()).await.ok()?;
        get(&conn, KEY_PROXY_SELECTIONS).await.ok()?
    })?;
    serde_json::from_str(&raw).ok()
}

/// 存各组选中（整表 JSON 覆盖写）。失败返回错误信息，调用方记一笔日志即可。
pub fn set_proxy_selections(selections: &HashMap<String, String>) -> Result<(), String> {
    let raw =
        serde_json::to_string(selections).map_err(|e| format!("序列化分组选中失败: {e}"))?;
    block_on(async {
        let conn = open(&db_path()).await?;
        set(&conn, KEY_PROXY_SELECTIONS, &raw).await
    })
}

/// 匹配器拆成行字段：网段还原成标准文本，`Match` 的 arg 置空，
/// `Unsupported` 的原类型塞进 arg（读回时原样恢复）。
fn split_rule_matcher(
    matcher: &crate::tools::proxy::Matcher,
) -> (String, String) {
    use crate::tools::proxy::Matcher;
    match matcher {
        Matcher::Domain(arg) => ("domain".to_string(), arg.clone()),
        Matcher::DomainSuffix(arg) => ("suffix".to_string(), arg.clone()),
        Matcher::DomainKeyword(arg) => ("keyword".to_string(), arg.clone()),
        Matcher::IpCidr(cidr) => (
            "cidr".to_string(),
            crate::tools::proxy::cidr_text(cidr),
        ),
        Matcher::Match => ("match".to_string(), String::new()),
        Matcher::Unsupported(kind) => ("unsupported".to_string(), kind.clone()),
    }
}

/// 行字段拼回匹配器：对不上就 None（调用方跳过该行，坏一行不炸整表）。
fn join_rule_matcher(kind: &str, arg: &str) -> Option<crate::tools::proxy::Matcher> {
    use crate::tools::proxy::Matcher;
    match kind {
        "domain" => Some(Matcher::Domain(arg.to_string())),
        "suffix" => Some(Matcher::DomainSuffix(arg.to_string())),
        "keyword" => Some(Matcher::DomainKeyword(arg.to_string())),
        "cidr" => crate::tools::proxy::CidrBlock::parse(arg).map(Matcher::IpCidr),
        "match" => Some(Matcher::Match),
        "unsupported" => Some(Matcher::Unsupported(arg.to_string())),
        _ => None,
    }
}

/// 自动规则整表重写：先清后插，一个事务一次提交。
/// 8 万行逐条插也是一秒级，调用方（拉取）本来就在后台。
async fn write_auto_rules(
    conn: &libsql::Connection,
    rules: &[crate::tools::proxy::Rule],
) -> Result<(), String> {
    let tx = conn
        .transaction()
        .await
        .map_err(|e| format!("规则事务开启失败: {e}"))?;
    let result = async {
        tx.execute("DELETE FROM proxy_rules_auto", ())
            .await
            .map_err(|e| format!("清规则表失败: {e}"))?;
        for (pos, rule) in rules.iter().enumerate() {
            let (kind, arg) = split_rule_matcher(&rule.matcher);
            tx.execute(
                "INSERT INTO proxy_rules_auto (pos, kind, arg, target) VALUES (?, ?, ?, ?)",
                vec![
                    libsql::Value::Integer(pos as i64),
                    libsql::Value::Text(kind),
                    libsql::Value::Text(arg),
                    libsql::Value::Text(rule.target.clone()),
                ],
            )
            .await
            .map_err(|e| format!("写规则行失败: {e}"))?;
        }
        Ok::<(), String>(())
    }
    .await;
    match result {
        Ok(()) => tx.commit().await.map_err(|e| format!("规则事务提交失败: {e}")),
        Err(e) => {
            let _ = tx.rollback().await;
            Err(e)
        }
    }
}

/// 自动规则按序读回：坏行跳过并记一笔（写坏的概率极低，读出来对不上
/// 大概率是手改了库，跳过比整个分流罢工强）。
async fn read_auto_rules(
    conn: &libsql::Connection,
) -> Result<Vec<crate::tools::proxy::Rule>, String> {
    let mut rows = conn
        .query(
            "SELECT kind, arg, target FROM proxy_rules_auto ORDER BY pos",
            (),
        )
        .await
        .map_err(|e| format!("读规则表失败: {e}"))?;
    let mut rules = Vec::new();
    loop {
        let next = rows
            .next()
            .await
            .map_err(|e| format!("读规则表失败: {e}"))?;
        let Some(row) = next else {
            break;
        };
        let kind: String = row
            .get(0)
            .map_err(|e| format!("读规则表失败: {e}"))?;
        let arg: String = row
            .get(1)
            .map_err(|e| format!("读规则表失败: {e}"))?;
        let target: String = row
            .get(2)
            .map_err(|e| format!("读规则表失败: {e}"))?;
        match join_rule_matcher(&kind, &arg) {
            Some(matcher) => rules.push(crate::tools::proxy::Rule { matcher, target }),
            None => eprintln!("规则表有坏行，已跳过: {kind},{arg}"),
        }
    }
    Ok(rules)
}

/// 读整份订阅：节点 + 分组走小 JSON，规则走独立表，三件拼起来。
/// 任一件坏了都返回 `None`（用户点更新订阅即可）；旧大 JSON 键还在
/// 就走一次性迁移（读出来拆开放，再删旧键）。
pub fn proxy_subscription() -> Option<crate::tools::proxy::Subscription> {
    block_on(async {
        let conn = open(&db_path()).await.ok()?;
        // 新格式三件套齐了直接拼。
        if let Some(nodes_raw) = get(&conn, KEY_PROXY_NODES).await.ok()? {
            let nodes = serde_json::from_str(&nodes_raw).ok()?;
            let groups = get(&conn, KEY_PROXY_GROUPS)
                .await
                .ok()?
                .and_then(|text| serde_json::from_str(&text).ok())
                .unwrap_or_default();
            let rules = read_auto_rules(&conn).await.unwrap_or_default();
            return Some(crate::tools::proxy::Subscription {
                nodes,
                groups,
                rules,
            });
        }
        // 旧大 JSON 还在：拆开放，删旧键，下次直接走新格式。
        migrate_legacy_subscription(&conn).await.ok()?
    })
}

/// 存整份订阅：节点 + 分组小 JSON，规则整表重写，最后删旧大 JSON 键
/// （6MB 死重别留在库里）。失败返回错误信息，调用方记一笔日志即可
/// （内存里那份照样用）。
pub fn set_proxy_subscription(
    subscription: &crate::tools::proxy::Subscription,
) -> Result<(), String> {
    let nodes = serde_json::to_string(&subscription.nodes)
        .map_err(|e| format!("序列化节点失败: {e}"))?;
    let groups = serde_json::to_string(&subscription.groups)
        .map_err(|e| format!("序列化分组失败: {e}"))?;
    block_on(async {
        let conn = open(&db_path()).await?;
        set(&conn, KEY_PROXY_NODES, &nodes).await?;
        set(&conn, KEY_PROXY_GROUPS, &groups).await?;
        write_auto_rules(&conn, &subscription.rules).await?;
        conn.execute("DELETE FROM settings WHERE key = ?", [KEY_PROXY_SUBSCRIPTION])
            .await
            .map_err(|e| format!("清旧订阅键失败: {e}"))?;
        Ok(())
    })
}

/// 一次性迁移：旧大 JSON（整个 Subscription）拆成三件套，再删旧键。
/// 旧键没有返回 `Ok(None)`；转坏了返回 Err（调用方当没存过，用户重拉）。
async fn migrate_legacy_subscription(
    conn: &libsql::Connection,
) -> Result<Option<crate::tools::proxy::Subscription>, String> {
    let Some(raw) = get(conn, KEY_PROXY_SUBSCRIPTION)
        .await
        .map_err(|e| format!("读旧订阅失败: {e}"))?
    else {
        return Ok(None);
    };
    let sub: crate::tools::proxy::Subscription =
        serde_json::from_str(&raw).map_err(|e| format!("旧订阅解析失败: {e}"))?;
    set(
        conn,
        KEY_PROXY_NODES,
        &serde_json::to_string(&sub.nodes).map_err(|e| format!("序列化节点失败: {e}"))?,
    )
    .await?;
    set(
        conn,
        KEY_PROXY_GROUPS,
        &serde_json::to_string(&sub.groups).map_err(|e| format!("序列化分组失败: {e}"))?,
    )
    .await?;
    write_auto_rules(conn, &sub.rules).await?;
    conn.execute("DELETE FROM settings WHERE key = ?", [KEY_PROXY_SUBSCRIPTION])
        .await
        .map_err(|e| format!("清旧订阅键失败: {e}"))?;
    Ok(Some(sub))
}

/// 读手动规则：库坏了、没存过、JSON 烂了都给空表（用户重加即可），
/// 绝不崩。更新订阅不碰这个 key，手动规则跨更新一直在。
pub fn proxy_manual_rules() -> Vec<crate::tools::proxy::ManualRule> {
    let raw: Option<String> = block_on(async {
        let conn = open(&db_path()).await.ok()?;
        get(&conn, KEY_PROXY_MANUAL).await.ok()?
    });
    raw.and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// 存手动规则（整表 JSON 覆盖写）。失败返回错误信息，调用方记一笔日志即可。
pub fn set_proxy_manual_rules(rules: &[crate::tools::proxy::ManualRule]) -> Result<(), String> {
    let raw =
        serde_json::to_string(rules).map_err(|e| format!("序列化手动规则失败: {e}"))?;
    block_on(async {
        let conn = open(&db_path()).await?;
        set(&conn, KEY_PROXY_MANUAL, &raw).await
    })
}

/// 隧道是不是我们开着的：读旗为 "1" 才是。任何失败都当没开，
/// 顶多下次启动少一次残留清理，绝不崩。
pub fn proxy_running() -> bool {
    block_on(async {
        let conn = open(&db_path()).await.ok()?;
        let value = get(&conn, KEY_PROXY_RUNNING).await.ok()??;
        Some(value == "1")
    })
    .unwrap_or(false)
}

/// 写隧道旗。失败返回错误信息，调用方记一笔日志即可。
pub fn set_proxy_running(running: bool) -> Result<(), String> {
    block_on(async {
        let conn = open(&db_path()).await?;
        set(&conn, KEY_PROXY_RUNNING, if running { "1" } else { "0" }).await
    })
}

/// 主窗口位置快照：逻辑像素矩形 + 关闭时是否最大化，
/// 启动恢复用（`main.rs` 负责校验越界，存只管记）。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct SavedWindowBounds {
    /// 全局坐标系左上角（逻辑像素）。
    pub x: f32,
    /// 全局坐标系左上角（逻辑像素）。
    pub y: f32,
    /// 窗口宽（逻辑像素）。
    pub w: f32,
    /// 窗口高（逻辑像素）。
    pub h: f32,
    /// 关闭时是否最大化：是则下次照样最大化打开。
    pub maximized: bool,
}

/// 读上次的窗口位置；没存过、JSON 烂了、库坏了都返回 `None`，
/// 调用方回落默认位置居中。
pub fn window_bounds() -> Option<SavedWindowBounds> {
    let raw = block_on(async {
        let conn = open(&db_path()).await.ok()?;
        get(&conn, KEY_WINDOW_BOUNDS).await.ok()?
    })?;
    serde_json::from_str(&raw).ok()
}

/// 存窗口位置（移动/缩放节流调 + 退出时调，调用方节流）。
/// 失败返回错误信息，调用方记一笔日志即可。
pub fn set_window_bounds(bounds: &SavedWindowBounds) -> Result<(), String> {
    let raw =
        serde_json::to_string(bounds).map_err(|e| format!("序列化窗口位置失败: {e}"))?;
    block_on(async {
        let conn = open(&db_path()).await?;
        set(&conn, KEY_WINDOW_BOUNDS, &raw).await
    })
}

/// 读上次选中的工具序号；没有、非法、库坏了都返回 `None`，
/// 调用方回落 0（欢迎页），序号越界调用方自己夹。
pub fn selected_tool() -> Option<usize> {
    let raw = block_on(async {
        let conn = open(&db_path()).await.ok()?;
        get(&conn, KEY_SELECTED_TOOL).await.ok()?
    })?;
    raw.parse().ok()
}

/// 存选中的工具序号（点侧边栏即调，低频）。
/// 失败返回错误信息，调用方记一笔日志即可。
pub fn set_selected_tool(ix: usize) -> Result<(), String> {
    block_on(async {
        let conn = open(&db_path()).await?;
        set(&conn, KEY_SELECTED_TOOL, &ix.to_string()).await
    })
}

/// 测试隔离器：把设置库指向临时文件，drop 时还原指向并删除临时文件。
/// 必须在 `i18n::lock_locale()` 之后使用（和改 locale 的测试同串行）。
#[cfg(test)]
pub(crate) struct TempSettings {
    path: PathBuf,
    prev: Option<String>,
}

#[cfg(test)]
impl TempSettings {
    pub(crate) fn install(tag: &str) -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let id = SEQ.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "wong-tools-{}-{tag}-{id}.db",
            std::process::id()
        ));
        let prev = std::env::var("WONG_SETTINGS_DB").ok();
        std::env::set_var("WONG_SETTINGS_DB", &path);
        Self { path, prev }
    }
}

#[cfg(test)]
impl Drop for TempSettings {
    fn drop(&mut self) {
        match &self.prev {
            Some(v) => std::env::set_var("WONG_SETTINGS_DB", v),
            None => std::env::remove_var("WONG_SETTINGS_DB"),
        }
        std::fs::remove_file(&self.path).ok();
        std::fs::remove_file(self.path.with_extension("db-wal")).ok();
        std::fs::remove_file(self.path.with_extension("db-journal")).ok();
    }
}

#[cfg(test)]
mod tests {
    // 注意：这里不要 `use super::*`——它会把 gpui 自带的 `#[test]` 宏引进
    // 来，盖住 Rust 内置的 test 属性导致递归展开失败。只显式引入用到的项。
    use super::data_dir;
    use super::get;
    use super::open;
    use super::proxy_manual_rules;
    use super::proxy_running;
    use super::proxy_selections;
    use super::proxy_subscription;
    use super::set;
    use super::set_proxy_manual_rules;
    use super::set_proxy_running;
    use super::set_proxy_selections;
    use super::set_proxy_subscription;
    use super::TempSettings;
    use super::KEY_LANGUAGE;
    use super::KEY_THEME;
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    /// 测试库序号：并行跑的测试各用各的文件，互不干扰。
    static SEQ: AtomicU64 = AtomicU64::new(0);

    /// 隔离的测试库：系统临时目录 + 唯一文件名，测完删除。
    fn temp_db() -> PathBuf {
        let id = SEQ.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("wong-tools-test-{}-{id}.db", std::process::id()))
    }

    /// 删测试库及 libsql 可能留下的 `-wal` / `-journal` 伴生文件。
    fn remove_db(path: &Path) {
        std::fs::remove_file(path).ok();
        std::fs::remove_file(path.with_extension("db-wal")).ok();
        std::fs::remove_file(path.with_extension("db-journal")).ok();
    }

    #[test]
    fn settings_roundtrip_in_isolated_db() {
        let path = temp_db();
        super::block_on(async {
            let conn = open(&path).await.expect("测试库能打开");
            // 空库读出来是 None，不是报错。
            assert_eq!(get(&conn, KEY_THEME).await.expect("能读"), None);
            set(&conn, KEY_THEME, "Wong Dark").await.expect("能写");
            set(&conn, KEY_LANGUAGE, "zh-CN").await.expect("能写");
            assert_eq!(
                get(&conn, KEY_THEME).await.expect("能读").as_deref(),
                Some("Wong Dark")
            );
            // 覆盖写只改对应 key。
            set(&conn, KEY_THEME, "Wong Light").await.expect("能写");
            assert_eq!(
                get(&conn, KEY_THEME).await.expect("能读").as_deref(),
                Some("Wong Light")
            );
            assert_eq!(
                get(&conn, KEY_LANGUAGE)
                    .await
                    .expect("能读")
                    .as_deref(),
                Some("zh-CN")
            );
        });
        remove_db(&path);
    }

    #[test]
    fn proxy_selections_roundtrip_through_public_api() {
        // 走公共 API 就要改 `WONG_SETTINGS_DB`，和改 locale 的测试同串行，
        // 库指向临时文件，不碰真实数据目录。
        let _guard = crate::i18n::lock_locale();
        let _settings = TempSettings::install("proxy-selections");
        assert_eq!(proxy_selections(), None, "空库读出来是 None");
        let mut first = HashMap::new();
        first.insert("🚀 节点选择".to_string(), "sg-1".to_string());
        set_proxy_selections(&first).expect("能写");
        assert_eq!(proxy_selections(), Some(first), "整表能读回来");
        // 覆盖写整表替换，不合并。
        let mut second = HashMap::new();
        second.insert("🤖 AI".to_string(), "us-1".to_string());
        set_proxy_selections(&second).expect("能写");
        assert_eq!(proxy_selections(), Some(second));
    }

    #[test]
    fn proxy_subscription_and_running_roundtrip() {
        // 同上：改库指向的测试串行，指向临时文件。
        let _guard = crate::i18n::lock_locale();
        let _settings = TempSettings::install("proxy-sub");
        assert_eq!(proxy_running(), false, "空库旗是落下的");
        assert!(proxy_subscription().is_none(), "空库没订阅");
        set_proxy_running(true).expect("能写旗");
        assert_eq!(proxy_running(), true);
        let mut sub = crate::tools::proxy::Subscription::default();
        sub.nodes.push(crate::tools::proxy::ProxyNode {
            name: "n1".to_string(),
            server: "a.com".to_string(),
            port: 443,
            uuid: "4dfae26b-42b2-4c31-bed4-7836307a3986".to_string(),
            sni: "a.com".to_string(),
            host: "a.com".to_string(),
            path: "/".to_string(),
        });
        sub.groups.push(crate::tools::proxy::ProxyGroup {
            name: "g1".to_string(),
            kind: "select".to_string(),
            members: vec!["n1".to_string()],
        });
        set_proxy_subscription(&sub).expect("能写订阅");
        assert_eq!(proxy_subscription(), Some(sub), "订阅整体能读回来");
        set_proxy_running(false).expect("能落旗");
        assert_eq!(proxy_running(), false);
    }

    #[test]
    fn proxy_manual_rules_roundtrip_and_survive() {
        // 手动规则独立 key：读写走公共 API，串行 + 临时库。
        let _guard = crate::i18n::lock_locale();
        let _settings = TempSettings::install("proxy-manual");
        assert!(proxy_manual_rules().is_empty(), "空库是空表");
        let rules = vec![
            crate::tools::proxy::ManualRule {
                id: "m1".to_string(),
                kind: crate::tools::proxy::ManualKind::Suffix,
                pattern: "ads.example.com".to_string(),
                target: "REJECT".to_string(),
                sort: 0,
            },
            crate::tools::proxy::ManualRule {
                id: "m2".to_string(),
                kind: crate::tools::proxy::ManualKind::Exact,
                pattern: "work.example.com".to_string(),
                target: "DIRECT".to_string(),
                sort: 10,
            },
        ];
        set_proxy_manual_rules(&rules).expect("能写");
        assert_eq!(proxy_manual_rules(), rules, "整表能读回来");
    }

    #[test]
    fn proxy_rules_table_roundtrip_all_kinds() {
        // 规则表往返走公共 API，串行 + 临时库。
        let _guard = crate::i18n::lock_locale();
        let _settings = TempSettings::install("proxy-rules");
        use crate::tools::proxy::CidrBlock;
        use crate::tools::proxy::Matcher;
        use crate::tools::proxy::Rule;
        use crate::tools::proxy::Subscription;
        let rules = vec![
            Rule {
                matcher: Matcher::Domain("a.com".to_string()),
                target: "g".to_string(),
            },
            Rule {
                matcher: Matcher::DomainSuffix("b.com".to_string()),
                target: "g".to_string(),
            },
            Rule {
                matcher: Matcher::DomainKeyword("key".to_string()),
                target: "g".to_string(),
            },
            Rule {
                matcher: Matcher::IpCidr(
                    CidrBlock::parse("10.0.0.0/8").expect("能解析"),
                ),
                target: "g".to_string(),
            },
            Rule {
                matcher: Matcher::IpCidr(
                    CidrBlock::parse("::1/128").expect("能解析"),
                ),
                target: "g".to_string(),
            },
            Rule {
                matcher: Matcher::Match,
                target: "g".to_string(),
            },
            Rule {
                matcher: Matcher::Unsupported("GEOIP".to_string()),
                target: "g".to_string(),
            },
        ];
        let sub = Subscription {
            nodes: Vec::new(),
            groups: Vec::new(),
            rules,
        };
        super::set_proxy_subscription(&sub).expect("能写");
        assert_eq!(
            super::proxy_subscription(),
            Some(sub),
            "规则表往返不能丢行、不能乱序"
        );
    }

    #[test]
    fn proxy_legacy_big_json_migrates_once() {
        // 旧大 JSON 键还在：读一次拆成三件套，旧键删掉，下次直接走新格式。
        let _guard = crate::i18n::lock_locale();
        let _settings = TempSettings::install("proxy-migrate");
        use crate::tools::proxy::Matcher;
        use crate::tools::proxy::ProxyGroup;
        use crate::tools::proxy::ProxyNode;
        use crate::tools::proxy::Rule;
        use crate::tools::proxy::Subscription;
        let sub = Subscription {
            nodes: vec![ProxyNode {
                name: "n1".to_string(),
                server: "a.com".to_string(),
                port: 443,
                uuid: "4dfae26b-42b2-4c31-bed4-7836307a3986".to_string(),
                sni: "a.com".to_string(),
                host: "a.com".to_string(),
                path: "/".to_string(),
            }],
            groups: vec![ProxyGroup {
                name: "g1".to_string(),
                kind: "select".to_string(),
                members: vec!["n1".to_string()],
            }],
            rules: vec![Rule {
                matcher: Matcher::Match,
                target: "g1".to_string(),
            }],
        };
        // 手工种旧键：模拟没升级前的库。
        super::block_on(async {
            let path = std::env::var("WONG_SETTINGS_DB").expect("临时库已指向");
            let conn = super::open(std::path::Path::new(&path))
                .await
                .expect("测试库能打开");
            super::set(
                &conn,
                "proxy_subscription",
                &serde_json::to_string(&sub).expect("能序列化"),
            )
            .await
            .expect("能种旧键");
        });
        assert_eq!(super::proxy_subscription(), Some(sub), "迁移后三件要齐");
        // 旧键已删：再读走新格式，不再碰迁移。
        super::block_on(async {
            let path = std::env::var("WONG_SETTINGS_DB").expect("临时库已指向");
            let conn = super::open(std::path::Path::new(&path))
                .await
                .expect("测试库能打开");
            assert_eq!(
                super::get(&conn, "proxy_subscription")
                    .await
                    .expect("能读"),
                None,
                "旧键必须删掉，6MB 死重别留着"
            );
        });
    }

    #[test]
    fn data_dir_is_wongtools_specific() {
        // 数据目录名必须带 WongTools，绝不能和其他应用撞车。
        let name = data_dir()
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();
        assert!(
            name.eq_ignore_ascii_case("wongtools") || name == "wong-tools",
            "数据目录名不对: {}",
            data_dir().display()
        );
    }

    #[test]
    fn window_state_roundtrip_through_public_api() {
        // 窗口位置 + 选中工具走公共 API 往返：空库读 None，写完读回一致。
        let _guard = crate::i18n::lock_locale();
        let _settings = TempSettings::install("window-state");
        use super::SavedWindowBounds;
        use super::selected_tool;
        use super::set_selected_tool;
        use super::set_window_bounds;
        use super::window_bounds;
        assert_eq!(window_bounds(), None, "没存过读 None");
        assert_eq!(selected_tool(), None, "没存过读 None");
        let saved = SavedWindowBounds {
            x: 80.0,
            y: 60.0,
            w: 1280.0,
            h: 800.0,
            maximized: false,
        };
        set_window_bounds(&saved).expect("能写窗口位置");
        set_selected_tool(2).expect("能写选中工具");
        assert_eq!(window_bounds(), Some(saved), "窗口位置往返一致");
        assert_eq!(selected_tool(), Some(2), "选中工具往返一致");
        // 最大化旗和非法序号：旗照存，非法序号读 None（调用方回落欢迎页）。
        let maxed = SavedWindowBounds { maximized: true, ..saved };
        set_window_bounds(&maxed).expect("能写最大化");
        assert_eq!(window_bounds(), Some(maxed), "最大化旗不能丢");
        super::block_on(async {
            let path = std::env::var("WONG_SETTINGS_DB").expect("临时库已指向");
            let conn = super::open(std::path::Path::new(&path))
                .await
                .expect("测试库能打开");
            super::set(&conn, super::KEY_SELECTED_TOOL, "abc")
                .await
                .expect("能种坏数据");
        });
        assert_eq!(selected_tool(), None, "非法序号读 None");
    }
}
