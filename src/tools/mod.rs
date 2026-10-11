//! 工具集：一个工具一个文件夹，逻辑 + 视图都关在里面。
//! 跨工具的叫法统一走这里，别处只认 `tools::xxx` 路径。

/// 截图工具：全屏捕获、区域框选、复制保存。
pub mod screenshot;

/// 代理工具：VLESS 订阅解析、直连隧道、系统代理开关。
pub mod proxy;

/// AI 转发：本地 OpenAI / Anthropic 兼容网关，转到 Zen 免费车道。
pub mod airelay;

/// 咖啡：端起防休眠，盖上放行（时长 + 屏幕开关持久化，运行态不恢复）。
pub mod coffee;
