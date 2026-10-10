//! Codex 一键配置：往 `~/.codex/config.toml` 写本地网关。
//!
//! Codex 的自定义 provider 写法（官方 `config.toml` 文档）：顶层
//! `model` / `model_provider` 指到 `[model_providers.<id>]` 的表，
//! `wire_api = "responses"`（`chat` 线 2026 年 2 月起已下掉，只能用它）。
//! 内置 id（`openai` / `ollama` / `lmstudio`）不让覆盖，所以单起
//! `wong-airelay`。网关不鉴权（回环即边界），`env_key` 不写。
//!
//! 只动自己这三处（顶层两键 + 自家 provider 表），别家 provider 和
//! 注释排版原样保留：`toml_edit` 按文档改写，不整文件重排。
//! 原文件不是合法 TOML 时直接报错，绝不覆盖用户手写的配置。

use std::path::{Path, PathBuf};

use toml_edit::{value, DocumentMut};

/// 自家 provider id：内置 id 不让覆盖，单起一个。
pub const PROVIDER_ID: &str = "wong-airelay";
/// provider 展示名。
pub const PROVIDER_NAME: &str = "Wong AI Relay";
/// Codex 现在只认这条线，网关侧 `/v1/responses` 本来就有。
pub const WIRE_API: &str = "responses";

/// Codex 用户级配置路径（`~/.codex/config.toml`）。
pub fn config_path() -> PathBuf {
    home_dir().join(".codex").join("config.toml")
}

/// 家目录：Windows 看 `USERPROFILE`，其他看 `HOME`，都没有回落当前目录。
fn home_dir() -> PathBuf {
    #[cfg(windows)]
    {
        std::env::var("USERPROFILE")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("."))
    }
    #[cfg(not(windows))]
    {
        std::env::var("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("."))
    }
}

/// 本地网关地址（Codex 的 `base_url`，带 `/v1` 后缀）。
pub fn base_url(port: u16) -> String {
    format!("http://127.0.0.1:{port}/v1")
}

/// 把三处配置合并进已有 TOML 文本：顶层 `model` / `model_provider` 覆盖，
/// 自家 provider 表建或更新，别的内容不动。空串当空配置。
/// 端口 0 / 空模型 / 非法 TOML / `model_providers` 不是表都报错。
pub fn apply_to_toml(existing: &str, port: u16, model: &str) -> Result<String, String> {
    if port == 0 {
        return Err("端口不能是 0".to_string());
    }
    let model = model.trim();
    if model.is_empty() {
        return Err("没有可用模型，先刷新目录".to_string());
    }
    let mut doc = existing
        .parse::<DocumentMut>()
        .map_err(|e| format!("Codex 配置解析失败，先手动修好它: {e}"))?;
    // 别家配坏了（比如手写 `model_providers = 1`）：直接报错，
    // 索引硬闯会 panic，不能拿用户配置冒险。
    if let Some(node) = doc.get("model_providers") {
        if !node.is_table() {
            return Err("Codex 配置里的 model_providers 不是表，先手动修好它".to_string());
        }
    }
    doc["model"] = value(model.to_string());
    doc["model_provider"] = value(PROVIDER_ID);
    doc["model_providers"][PROVIDER_ID]["name"] = value(PROVIDER_NAME);
    doc["model_providers"][PROVIDER_ID]["base_url"] = value(base_url(port));
    doc["model_providers"][PROVIDER_ID]["wire_api"] = value(WIRE_API);
    Ok(doc.to_string())
}

/// 一键写入：建目录、读旧文件（没有当空的）、合并、落盘，返回写的位置。
/// 文件在但读不出来（比如权限）按错报，不敢盲覆盖。
pub fn configure(port: u16, model: &str) -> Result<PathBuf, String> {
    configure_at(&config_path(), port, model)
}

/// `configure` 的路径可注入版（单测指临时文件，不碰真配置）。
fn configure_at(path: &Path, port: u16, model: &str) -> Result<PathBuf, String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("建 Codex 配置目录失败: {e}"))?;
    }
    let existing = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(format!("读 Codex 配置失败: {e}")),
    };
    let updated = apply_to_toml(&existing, port, model)?;
    std::fs::write(path, updated).map_err(|e| format!("写 Codex 配置失败: {e}"))?;
    Ok(path.to_path_buf())
}

#[cfg(test)]
mod tests {
    // 注意：这里不要 `use super::*`——它会把 gpui 自带的 `#[test]` 宏引进
    // 来，盖住 Rust 内置的 test 属性导致递归展开失败。只显式引入用到的项。
    use super::PROVIDER_ID;
    use super::apply_to_toml;
    use super::base_url;
    use super::config_path;
    use super::configure_at;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// 临时文件序号：并行跑的测试各写各的，不碰真配置。
    static SEQ: AtomicU64 = AtomicU64::new(0);

    fn temp_config() -> std::path::PathBuf {
        let id = SEQ.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("wong-codex-{}-{id}.toml", std::process::id()))
    }

    #[test]
    fn fresh_doc_sets_model_provider_and_table() {
        // 空配置：一键后顶层两键 + 自家表齐了，地址带当前端口。
        let out = apply_to_toml("", 11459, "mimo-v2.6-flash-free").expect("能写");
        let doc = out.parse::<toml_edit::DocumentMut>().expect("产出合法 TOML");
        assert_eq!(doc["model"].as_str(), Some("mimo-v2.6-flash-free"));
        assert_eq!(doc["model_provider"].as_str(), Some(PROVIDER_ID));
        assert_eq!(doc["model_providers"][PROVIDER_ID]["base_url"].as_str(), Some("http://127.0.0.1:11459/v1"));
        assert_eq!(doc["model_providers"][PROVIDER_ID]["wire_api"].as_str(), Some("responses"));
        assert_eq!(base_url(11459), "http://127.0.0.1:11459/v1");
    }

    #[test]
    fn existing_config_is_preserved() {
        // 别人家的键和 provider 都留着，只改顶层两键 + 自家表。
        let existing = "approval_policy = \"on-request\"\nmodel = \"gpt-5\"\nmodel_provider = \"other\"\n[model_providers.other]\nbase_url = \"https://example.com/v1\"\n";
        let out = apply_to_toml(existing, 18080, "union-alpha").expect("能合并");
        let doc = out.parse::<toml_edit::DocumentMut>().expect("产出合法 TOML");
        assert_eq!(
            doc["approval_policy"].as_str(),
            Some("on-request"),
            "无关键不能动"
        );
        assert_eq!(doc["model"].as_str(), Some("union-alpha"));
        assert_eq!(doc["model_provider"].as_str(), Some(PROVIDER_ID));
        assert_eq!(
            doc["model_providers"]["other"]["base_url"].as_str(),
            Some("https://example.com/v1"),
            "别家 provider 不能动"
        );
        assert_eq!(
            doc["model_providers"][PROVIDER_ID]["base_url"].as_str(),
            Some("http://127.0.0.1:18080/v1"),
            "端口跟着当前输入走"
        );
    }

    #[test]
    fn bad_input_errors_without_touching_disk() {
        // 非法 TOML、空模型、0 端口、`model_providers` 不是表：都报错。
        assert!(apply_to_toml("[[[", 11459, "m").is_err(), "烂 TOML 不能硬写");
        assert!(apply_to_toml("", 11459, "  ").is_err(), "空模型不进配置");
        assert!(apply_to_toml("", 0, "m").is_err(), "0 端口不进配置");
        assert!(
            apply_to_toml("model_providers = 1\n", 11459, "m").is_err(),
            "类型错了先让用户手动修"
        );
    }

    #[test]
    fn configure_at_writes_merged_file() {
        // 落盘走临时文件：第一次建出来，第二次合并（旧内容还在）。
        let path = temp_config();
        configure_at(&path, 11459, "mimo-v2.6-flash-free").expect("能写");
        std::fs::write(&path, "sandbox_mode = \"workspace-write\"\n").expect("能种旧内容");
        let back = configure_at(&path, 11459, "mimo-v2.6-flash-free").expect("能合并");
        assert_eq!(back, path);
        let text = std::fs::read_to_string(&path).expect("能读回");
        let doc = text.parse::<toml_edit::DocumentMut>().expect("合法 TOML");
        assert_eq!(doc["sandbox_mode"].as_str(), Some("workspace-write"));
        assert_eq!(doc["model_provider"].as_str(), Some(PROVIDER_ID));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn config_path_points_at_codex_config() {
        // 路径形状：`~/.codex/config.toml`，文件名不能错。
        let path = config_path();
        assert_eq!(path.file_name().and_then(|s| s.to_str()), Some("config.toml"));
        assert_eq!(
            path.parent().and_then(|p| p.file_name()).and_then(|s| s.to_str()),
            Some(".codex")
        );
    }
}
