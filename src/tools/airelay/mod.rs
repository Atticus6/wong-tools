//! AI 转发：本地 OpenAI / Anthropic 兼容网关，把请求转到 Zen 免费车道。
//!
//! 协议纯逻辑在 `zen`（会话铸造、指纹、三种线协议转换、SSE 解码，单测全覆盖）；
//! `server` 是回环 HTTP 服务（阻塞线程模型，和代理工具同款）；
//! `view` 是工具页（`impl WsShell`，状态走 `pub(crate)`）。

pub mod server;
pub mod view;
pub mod zen;
