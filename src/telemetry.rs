//! 错误上报：PostHog error-tracking，blocking 客户端。
//!
//! 阻塞式线程模型：`posthog-rs` 用 `default-features = false` 引进来，
//! 后台 worker 自己管线程，应用侧全是同步调用，不引入新的异步运行时。
//! 启动时调一次 [`init`]（装全局客户端 + panic 自动捕获），上报失败
//! 只记日志，绝不把崩溃上报本身变成新的崩溃源。

/// PostHog 项目公钥（`phc_` 开头是可发布键：照官方文档做法直接嵌在
/// 客户端里，和前端埋点键一个性质，不是个人 API Key）。
/// 要换项目/换区，key 和 [`POSTHOG_HOST`] 一起换。
const POSTHOG_KEY: &str = "phc_x5ViCkQUaR6bpKTFe6Z3CTb7VjS6Sr5Tm6j5LXuz97Qn";
/// 上报入口（美国区，上面 key 的归属区）。
const POSTHOG_HOST: &str = "https://us.i.posthog.com";

/// 组装客户端选项：纯构造不碰网络，好单测；[`init`] 负责让它生效。
fn build_options() -> Result<posthog_rs::ClientOptions, String> {
    use posthog_rs::ClientOptionsBuilder;
    use posthog_rs::ErrorTrackingOptionsBuilder;
    let tracking = ErrorTrackingOptionsBuilder::default()
        // panic 自动捕获：装的是进程级 hook，崩了也尽力把现场发出去。
        .capture_panics(true)
        // 本包帧标成应用内：堆栈在后台按这个分应用/依赖，崩在哪一眼可见。
        .in_app_include_paths(vec!["wong_tools::".to_string()])
        .build()
        .map_err(|e| format!("组装错误上报选项失败: {e}"))?;
    ClientOptionsBuilder::default()
        .api_key(POSTHOG_KEY.to_string())
        .host(POSTHOG_HOST.to_string())
        .error_tracking(tracking)
        .build()
        .map_err(|e| format!("组装上报客户端选项失败: {e}"))
}

/// 启动时调一次（越早越好，`main` 第一行）：装全局客户端并打开
/// panic 自动捕获。网络不通、key 写错都只记一笔日志，应用照常启动；
/// 同进程调两次会报已初始化，同样只记日志。
pub fn init() {
    match build_options() {
        Ok(options) => {
            if let Err(e) = posthog_rs::init_global(options) {
                eprintln!("错误上报初始化失败: {e}");
            }
        }
        Err(e) => eprintln!("{e}"),
    }
}

#[cfg(test)]
mod tests {
    // 注意：这里不要 `use super::*`——它会把 gpui 自带的 `#[test]` 宏引进
    // 来，盖住 Rust 内置的 test 属性导致递归展开失败。只显式引入用到的项。
    use super::build_options;

    #[test]
    fn error_tracking_options_build_without_network() {
        // 纯组装：不断网也不发包，key/host 写错格式在这里就能拦住。
        build_options().expect("上报选项必须能组装出来");
    }
}
