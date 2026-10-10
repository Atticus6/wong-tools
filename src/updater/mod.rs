//! 自动更新检查：启动查一次 GitHub 最新 Release，有新版标题栏右上亮图标。
//!
//! 按官方 auto-update 文档的归属表：我们发的是 Inno 安装包 / DMG / deb，
//! 安装归安装程序和包管理器管，应用只做“检查 + 提醒 + 跳下载页”，
//! 绝不自替换二进制（会破坏 mac 签名和各平台包数据库）。
//! 纯逻辑可单测；网络活调用方扔后台线程，别卡界面。

/// 发布仓库：Release 页跳转和 API 共用，改名/转库时一起改。
const REPO_OWNER: &str = "Atticus6";
/// 发布仓库名。
const REPO_NAME: &str = "wong-tools";
/// 检查超时（秒）：弱网下别 hang 住后台线程太久，失败下次启动再查。
const CHECK_TIMEOUT_SECS: u64 = 15;

/// 当前版本：只认 `Cargo.toml`，tag 只是扳机（见打包 workflow）。
pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// 检查结果：新版本号（去 `v` 前缀）+ 去下载的 Release 页面地址。
pub struct LatestRelease {
    /// 纯数字版本，如 `"0.1.1"`。
    pub version: String,
    /// Release 页面地址，点更新图标跳这里。
    pub url: String,
}

/// 更新状态机（官方文档建议的子集）：只查不装，没有下载/校验态，
/// 安装那半归各平台安装程序管。状态放 `WsShell`，检查跑后台，
/// 回来写状态 + `notify` 重绘。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum UpdateState {
    /// 还没查过（启动检查回来之前）。
    #[default]
    Idle,
    /// 正在请求 GitHub（防重入，查完才接受下一次）。
    Checking,
    /// 已经是最新。
    UpToDate,
    /// 有新版：版本号 + 下载页，标题栏图标和设置页入口共用。
    Available {
        /// 纯数字版本，如 `"0.1.2"`。
        version: String,
        /// Release 页面地址。
        url: String,
    },
    /// 查失败（断网/限流/格式变了）：留着原因给设置页展示，
    /// 不打扰标题栏，下次启动或手动再查。
    Failed(String),
}

/// `"v0.1.1"` / `"0.1.1"` 解析成数字三元组：缺段补 0，多段截断，
/// 前后空格容忍；非数字返回 `None`，调用方按“没新版”处理。
/// 版本比较只认数字元组，不直接比字符串（`"0.1.10"` 按字符串会输给 `"0.1.9"`）。
pub fn parse_version(tag: &str) -> Option<(u64, u64, u64)> {
    let tag = tag.trim().trim_start_matches(['v', 'V']);
    if tag.is_empty() {
        return None;
    }
    let mut parts = [0u64; 3];
    for (ix, piece) in tag.split('.').take(3).enumerate() {
        parts[ix] = piece.parse().ok()?;
    }
    // 第三段之后还带东西（如 `1.2.3.4`）不认：那不是我们的发版格式。
    if tag.split('.').nth(3).is_some() {
        return None;
    }
    Some((parts[0], parts[1], parts[2]))
}

/// 线上版本比当前新才算有更新；任何一边解析失败宁可不提醒，
/// 也别拿坏数据去跳下载页。
pub fn is_newer(latest_tag: &str, current: &str) -> bool {
    match (parse_version(latest_tag), parse_version(current)) {
        (Some(latest), Some(cur)) => latest > cur,
        _ => false,
    }
}

/// Release JSON 提炼出版本号和下载页（纯函数，好单测）：
/// 要 `tag_name` + `html_url`，缺任一直接报错。
pub fn parse_release(release: &serde_json::Value) -> Result<LatestRelease, String> {
    let tag = release["tag_name"].as_str().unwrap_or("").to_string();
    let url = release["html_url"].as_str().unwrap_or("").to_string();
    if tag.is_empty() || url.is_empty() {
        return Err("更新信息缺版本号或下载页".to_string());
    }
    // 版本号存纯数字：展示和比较都省一遍剥 `v`。
    let version = match parse_version(&tag) {
        Some((major, minor, patch)) => format!("{major}.{minor}.{patch}"),
        None => return Err(format!("版本号看不懂: {tag}")),
    };
    Ok(LatestRelease { version, url })
}

/// 拉 GitHub 最新 Release（阻塞，调用方扔后台线程）。
/// `/releases/latest` 自动排除草稿和预发布，正好只要稳定版。
/// 失败只给原因字符串，调用方展示或记日志，不崩。
pub fn check_latest() -> Result<LatestRelease, String> {
    let api = format!("https://api.github.com/repos/{REPO_OWNER}/{REPO_NAME}/releases/latest");
    let agent = ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(CHECK_TIMEOUT_SECS))
        .build();
    let release: serde_json::Value = agent
        .get(&api)
        // GitHub API 强制要 User-Agent，没有直接 403。
        .set("User-Agent", "wong-tools")
        .set("Accept", "application/vnd.github+json")
        .call()
        .map_err(|e| format!("更新检查请求失败: {e}"))?
        .into_json()
        .map_err(|e| format!("更新信息解析失败: {e}"))?;
    parse_release(&release)
}

/// 浏览器打开下载页（调用方扔后台，装不装用户在那边定）。
/// 平台三选一：mac `open`、Windows `start`、Linux `xdg-open`。
pub fn open_release_page(url: &str) -> Result<(), String> {
    let status = {
        #[cfg(target_os = "windows")]
        {
            std::process::Command::new("cmd")
                .args(["/C", "start", "", url])
                .status()
        }
        #[cfg(target_os = "macos")]
        {
            std::process::Command::new("open").arg(url).status()
        }
        #[cfg(not(any(target_os = "windows", target_os = "macos")))]
        {
            std::process::Command::new("xdg-open").arg(url).status()
        }
    }
    .map_err(|e| format!("打不开浏览器: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("浏览器退出码异常: {status}"))
    }
}

#[cfg(test)]
mod tests {
    // 注意：这里不要 `use super::*`——它会把 gpui 自带的 `#[test]` 宏引进
    // 来，盖住 Rust 内置的 test 属性导致递归展开失败。只显式引入用到的项。
    use super::is_newer;
    use super::parse_release;
    use super::parse_version;

    #[test]
    fn version_parses_with_or_without_v_prefix() {
        // 带 v / 不带 v / 缺段补 0，都是合法发版格式。
        assert_eq!(parse_version("v0.1.1"), Some((0, 1, 1)));
        assert_eq!(parse_version("0.1.10"), Some((0, 1, 10)));
        assert_eq!(parse_version("v1"), Some((1, 0, 0)));
        assert_eq!(parse_version("  v2.0  "), Some((2, 0, 0)));
    }

    #[test]
    fn version_rejects_garbage() {
        // 空串、非数字、四段以上都不认，调用方按“没新版”处理。
        assert_eq!(parse_version(""), None);
        assert_eq!(parse_version("abc"), None);
        assert_eq!(parse_version("v1.x.2"), None);
        assert_eq!(parse_version("1.2.3.4"), None);
    }

    #[test]
    fn newer_compares_numerically_not_lexically() {
        // 字符串比较 "0.1.10" < "0.1.9"，数字比较才是对的。
        assert!(is_newer("v0.1.2", "0.1.1"));
        assert!(is_newer("v0.1.10", "0.1.9"));
        assert!(!is_newer("v0.1.1", "0.1.1"), "同版不算更新");
        assert!(!is_newer("v0.1.0", "0.1.1"), "旧版不算更新");
        assert!(!is_newer("next", "0.1.1"), "坏 tag 宁可不提醒");
    }

    #[test]
    fn release_json_extracts_version_and_url() {
        // 用 GitHub 真实回包形状喂：tag 剥 v，地址原样拿。
        let body: serde_json::Value = serde_json::json!({
            "tag_name": "v0.1.2",
            "html_url": "https://github.com/Atticus6/wong-tools/releases/tag/v0.1.2",
            "prerelease": false,
        });
        let latest = parse_release(&body).expect("合法回包能提炼");
        assert_eq!(latest.version, "0.1.2");
        assert_eq!(
            latest.url,
            "https://github.com/Atticus6/wong-tools/releases/tag/v0.1.2"
        );
        // 缺任一直接报错，不拿半截数据去跳页。
        assert!(parse_release(&serde_json::json!({"tag_name": "v0.1.2"})).is_err());
        assert!(parse_release(&serde_json::json!({"html_url": "https://x"})).is_err());
    }
}
