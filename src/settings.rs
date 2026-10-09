//! 设置持久化：libsql 本地库。
//!
//! 位置：Windows `%APPDATA%\WongTools\settings.db`，macOS
//! `~/Library/Application Support/WongTools/settings.db`，其他系统
//! `$XDG_CONFIG_HOME/wong-tools/settings.db`（没有则 `~/.config/...`）。
//! 就一张键值表（`settings`），key 目前只有主题名和语言代码。
//!
//! 读写失败一律吞掉（读返回 `None`、写记一笔日志）：设置丢了可以重选，
//! 绝不能把程序崩掉。libsql 全是 async 接口，设置读写又都是低频同步
//! 调用，所以每次现建一个 current-thread runtime 跑完就扔，比常驻便宜。
//!
//! 测试钩子：`WONG_SETTINGS_DB` 环境变量能把库指向别处，测试用它隔离，
//! 绝不碰真实数据目录（见 `TempSettings`）。

use std::path::{Path, PathBuf};

const DB_FILE: &str = "settings.db";
const KEY_THEME: &str = "theme";
const KEY_LANGUAGE: &str = "language";

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
    use super::set;
    use super::KEY_LANGUAGE;
    use super::KEY_THEME;
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
}
