//! Zen 转发协议层：纯逻辑，不碰网络和 GPUI。
//!
//! 把下游三种写法（OpenAI chat / OpenAI responses / Anthropic messages）
//! 先转成统一消息（`Message` + `ToolDef`），再按模型选线上协议（chat /
//! responses / messages）组装上游请求（`https://opencode.ai/zen/v1/*`），
//! 会话铸造 + 指纹头 + 工具四件套一个不少；上游 SSE 逐帧译成 `Chunk`，
//! 各路由再译成下游形状。移植自 zen-gate（Go），字段级对齐，
//! 行为差异只收敛在三处（见各函数注释）。
//!
//! 上游永远 `stream: true`，非流式下游由调用方攒完一次组装。

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicI64, Ordering};

use serde_json::{Map, Value};

// 上游根：测试可改（zen-gate 用变量，单测覆盖整个转译链）。
pub(crate) static UPSTREAM_BASE: std::sync::Mutex<String> =
    std::sync::Mutex::new(String::new());

/// 上游根地址（默认 `https://opencode.ai`）。
pub fn upstream_base() -> String {
    let guard = UPSTREAM_BASE.lock().unwrap_or_else(|p| p.into_inner());
    if guard.is_empty() {
        "https://opencode.ai".to_string()
    } else {
        guard.clone()
    }
}

/// 拉不到目录时的冷启动兜底（zen-gate `FallbackCatalogIDs` 原样）。
pub const FALLBACK_MODELS: &[&str] = &[
    "mimo-v2.6-flash-free",
    "mimo-v2.5-free",
    "ling-3.0-flash-fin-free",
    "nemotron-3-ultra-free",
    "nemotron-3.5-lightning-free",
    "space-bunny-free",
    "muse-spark-1.3-contributor-free",
    "muse-spark-1.2-contributor-free",
];

/// 免费车道判定：上游目录混着付费 id，只有这些不用用户密钥。
/// 常驻两席 + 名字里带独立 `free` 段（`(?:^|[-_])free(?:$|[-_.])`）。
pub fn is_free_lane(model_id: &str) -> bool {
    let base = base_model_id(model_id);
    if base == "union-alpha" || base == "space-bunny-free" {
        return true;
    }
    let lower = base.to_lowercase();
    let mut start = 0usize;
    while let Some(i) = lower[start..].find("free") {
        let abs = start + i;
        let before_ok = abs == 0
            || matches!(lower.as_bytes()[abs - 1], b'-' | b'_');
        let after = abs + 4;
        let after_ok = after >= lower.len()
            || matches!(lower.as_bytes()[after], b'-' | b'_' | b'.');
        if before_ok && after_ok {
            return true;
        }
        start = abs + 1;
    }
    false
}

/// 目录解析：`{"data":[{"id":…}]}` / `{"models":[…]}` / 裸数组；
/// 条目是字符串或 `{"id"}` 对象。过滤免费 + 去重（保序）。
pub fn parse_listing(payload: &Value) -> Vec<String> {
    let rows: Vec<&Value> = if let Some(data) = payload.get("data").and_then(Value::as_array) {
        data.iter().collect()
    } else if let Some(models) = payload.get("models").and_then(Value::as_array) {
        models.iter().collect()
    } else if let Some(arr) = payload.as_array() {
        arr.iter().collect()
    } else {
        Vec::new()
    };
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for row in rows {
        let id = match row {
            Value::String(s) => s.clone(),
            Value::Object(_) => row.get("id").and_then(Value::as_str).unwrap_or("").to_string(),
            _ => continue,
        };
        let id = id.trim().to_string();
        if id.is_empty() || !is_free_lane(&id) {
            continue;
        }
        let base = base_model_id(&id);
        if seen.contains(&base) {
            continue;
        }
        seen.insert(base);
        out.push(id);
    }
    out
}

/// 拉上游目录：`GET /zen/v1/models` + 指纹头（15 秒超时）。
/// `proxy` 有值则走 HTTP CONNECT（代理工具的混合入口）。
/// 失败调用方回退 `FALLBACK_MODELS`（冷启动不断粮）。
pub fn fetch_listing(proxy: Option<&str>) -> Result<Vec<String>, String> {
    let url = format!("{}/zen/v1/models", upstream_base());
    let session = session_for_conversation("catalog:zen-gate");
    let request_id = mint_request_id(0);
    let agent = {
        let mut builder = ureq::AgentBuilder::new()
            .timeout_connect(std::time::Duration::from_secs(15))
            .timeout(std::time::Duration::from_secs(15));
        if let Some(url) = proxy {
            let proxy = ureq::Proxy::new(url).map_err(|e| format!("代理地址非法: {e}"))?;
            builder = builder.proxy(proxy);
        }
        builder.build()
    };
    let mut req = agent.get(&url);
    for (k, v) in gateway_headers(&session, &request_id, false, "application/json") {
        req = req.set(&k, &v);
    }
    match req.call() {
        Ok(resp) => {
            let text = resp.into_string().map_err(|e| format!("读目录失败: {e}"))?;
            let payload: Value =
                serde_json::from_str(&text).map_err(|e| format!("目录不是 JSON: {e}"))?;
            Ok(parse_listing(&payload))
        }
        Err(ureq::Error::Status(status, resp)) => {
            let text = resp.into_string().unwrap_or_default();
            let (_, code, _) = classify_failure(status, &text);
            Err(format!("目录接口 {code}"))
        }
        Err(ureq::Error::Transport(e)) => Err(format!("目录连不上: {e}")),
    }
}

/// 桌面客户端 UA（上游有版本下限，照抄 zen-gate）。
pub const CLIENT_UA: &str = "opencode/1.18.31";

/// 免费车道点名的工具四件套（小写）：请求体里必须齐。
pub const FINGERPRINT_TOOLS: &[&str] = &["bash", "glob", "grep", "read"];

/// 能替某个槽位顶班的真工具（`pwsh` 能答 `bash` 的活）。
pub const QUARTET_DONORS: &[(&str, &str)] = &[("bash", "pwsh")];

/// 线上协议：按模型分流（`muse-spark` 走 responses，`union-alpha` 走
/// messages，`jev*` 是只答题不聊天的决策模型，v1 直接拒掉）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wire {
    /// 默认：`/zen/v1/chat/completions`。
    Chat,
    /// `muse-spark` 系：`/zen/v1/responses`。
    Responses,
    /// `union-alpha`：`/zen/v1/messages`。
    Messages,
    /// `jev*` 决策模型：v1 不做，下游 400。
    SystemOne,
}

/// 模型 id 去尾巴上的 `(deep)` 之类 effort 后缀，查表才命中。
pub fn base_model_id(model: &str) -> String {
    let s = model.trim();
    if s.len() > 1 && s.ends_with(')') {
        if let Some(i) = s[..s.len() - 1].rfind('(') {
            return s[..i].trim().to_string();
        }
    }
    s.to_string()
}

/// 模型走哪条线上协议（`provider/model` 写法先取 `/` 后面一段）。
pub fn wire_for(model_id: &str) -> Wire {
    let mut base = base_model_id(model_id);
    if let Some(i) = base.rfind('/') {
        base = base[i + 1..].to_string();
    }
    if is_muse_spark(&base) {
        return Wire::Responses;
    }
    if base == "union-alpha" {
        return Wire::Messages;
    }
    if base.starts_with("jev") {
        return Wire::SystemOne;
    }
    Wire::Chat
}

/// 上游路径（zen-gate `EndpointFor` 原样）。
pub fn endpoint_for(model_id: &str) -> &'static str {
    match wire_for(model_id) {
        Wire::Responses => "/zen/v1/responses",
        Wire::Messages => "/zen/v1/messages",
        Wire::SystemOne => "/zen/v1/systemone",
        Wire::Chat => "/zen/v1/chat/completions",
    }
}

/// v1 能接的模型：三条文本线（决策模型 `jev*` 只答题不聊天，不接单）。
pub fn is_servable(model_id: &str) -> bool {
    matches!(wire_for(model_id), Wire::Chat | Wire::Responses | Wire::Messages)
}

/// `muse-spark` 系列判定（大小写不敏感，前缀匹配）。
fn is_muse_spark(base: &str) -> bool {
    let lower = base.to_lowercase();
    let rest = lower.strip_prefix("muse").unwrap_or(&lower);
    let rest = rest.strip_prefix(['-', '_']).unwrap_or(rest);
    rest == "spark" || rest.starts_with("spark")
        && rest["spark".len()..]
            .starts_with(['-', '_', ':', '.', ' '])
        || rest.starts_with("spark")
            && (rest.len() == "spark".len() || !rest["spark".len()..].starts_with(char::is_alphanumeric))
}

fn hex6(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn base62_from(bytes: &[u8]) -> String {
    const BASE62: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
    bytes.iter().map(|b| BASE62[(b % 62) as usize] as char).collect()
}

fn rand14() -> [u8; 14] {
    use rand::RngCore;
    let mut out = [0u8; 14];
    rand::thread_rng().fill_bytes(&mut out);
    out
}

/// 现铸会话 id 的序号器：生产代码走确定性映射（`session_for_conversation`），
/// 这里只给单测钉死 id 格式（上游只认形状）。allow 是显式的，不藏 warning。
#[allow(dead_code)]
static LAST_STAMP: AtomicI64 = AtomicI64::new(0);
#[allow(dead_code)]
static SEQ: AtomicI64 = AtomicI64::new(0);

/// 铸会话 id（`ses_` + 12 位 hex + 14 位 base62）：时间前缀 + 单调序号，
/// 和上游 JS 逐字节对齐，同一毫秒内序号递增。
#[allow(dead_code)]
pub fn mint_session_id(now_ms: i64) -> String {
    let now = if now_ms <= 0 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    } else {
        now_ms
    };
    let prev = LAST_STAMP.load(Ordering::Relaxed);
    if prev != now {
        LAST_STAMP.compare_exchange(prev, now, Ordering::Relaxed, Ordering::Relaxed).ok();
        SEQ.store(0, Ordering::Relaxed);
    }
    let seq = SEQ.fetch_add(1, Ordering::Relaxed) + 1;
    let value = !(((now as u64) << 12) | ((seq as u64) & 0xFFF));
    let mut raw = [0u8; 6];
    for (i, b) in raw.iter_mut().enumerate() {
        *b = ((value >> (40 - 8 * i)) & 0xFF) as u8;
    }
    format!("ses_{}{}", hex6(&raw), base62_from(&rand14()))
}

/// 铸单轮请求 id（`msg_` + 同上格式）。
pub fn mint_request_id(now_ms: i64) -> String {
    let now = if now_ms <= 0 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    } else {
        now_ms
    };
    let value = !(((now as u64) << 12) | 1);
    let mut raw = [0u8; 6];
    for (i, b) in raw.iter_mut().enumerate() {
        *b = ((value >> (40 - 8 * i)) & 0xFF) as u8;
    }
    format!("msg_{}{}", hex6(&raw), base62_from(&rand14()))
}

fn is_session_id(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 30
        && s.starts_with("ses_")
        && b[4..16].iter().all(|c| c.is_ascii_hexdigit())
        && b[16..].iter().all(|c| c.is_ascii_alphanumeric())
}

fn is_request_id(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 30
        && s.starts_with("msg_")
        && b[4..16].iter().all(|c| c.is_ascii_hexdigit())
        && b[16..].iter().all(|c| c.is_ascii_alphanumeric())
}

fn sha256_hex_b62(data: &[u8]) -> (String, String) {
    use sha2::{Digest, Sha256};
    let sum = Sha256::digest(data);
    let hex: String = sum[..6].iter().map(|b| format!("{b:02x}")).collect();
    let b62 = base62_from(&sum[6..20]);
    (hex, b62)
}

/// 一次下游会话稳定映射到一个上游会话（免费额度按会话算，
/// 每轮新 id 会被当成刷额度直接 429）。种子过了就是它，不过按
/// `our-free-model\0seed` 哈希铸一个。
pub fn session_for_conversation(seed: &str) -> String {
    let seed = seed.trim();
    if is_session_id(seed) {
        return seed.to_string();
    }
    let seed = if seed.is_empty() { "global" } else { seed };
    let mut data = b"our-free-model\x00".to_vec();
    data.extend_from_slice(seed.as_bytes());
    let (hex, b62) = sha256_hex_b62(&data);
    format!("ses_{hex}{b62}")
}

/// 单轮种子：同轮重试共用，下一轮变化（条数 | 文本总量 | 末条前 200 字）。
/// 只定本地稳定性，上游只校验 id 格式，不认种子原文。
pub fn turn_seed(messages: &[Message]) -> String {
    let total: usize = messages.iter().map(|m| m.text_of().len() + 8).sum();
    let last: String = messages
        .last()
        .map(|m| m.text_of().chars().take(200).collect())
        .unwrap_or_default();
    format!("{}|{}|{}", messages.len(), total, last)
}

/// 单轮请求 id：同轮重试共用，空种子直接现铸。
pub fn request_id_for(session: &str, turn_seed: &str) -> String {
    if turn_seed.trim().is_empty() {
        return mint_request_id(0);
    }
    let mut data = b"our-free-model-req\x00".to_vec();
    data.extend_from_slice(session.as_bytes());
    data.push(0);
    data.extend_from_slice(turn_seed.as_bytes());
    let (hex, b62) = sha256_hex_b62(&data);
    let id = format!("msg_{hex}{b62}");
    if is_request_id(&id) {
        id
    } else {
        mint_request_id(0)
    }
}

/// 会话种子：agent + 用户字段 + 首条用户文本（500 截断），三段 `\0` 拼接。
/// 下游没 agent 概念，调用方传固定串即可。
pub fn session_seed(agent: &str, user_field: &str, messages: &[Message]) -> String {
    let mut first = String::new();
    for m in messages {
        if m.role == Role::User {
            first = m.text_of();
            break;
        }
    }
    if first.len() > 500 {
        first.truncate(500);
    }
    format!("{agent}\x00{user_field}\x00{first}\x00")
}

/// 上游指纹头：网关认桌面客户端就看这套。免费池没有用户密钥，
/// `Bearer public` 就是全部凭证。
pub fn gateway_headers(
    session: &str,
    request_id: &str,
    stream: bool,
    accept: &str,
) -> Vec<(String, String)> {
    let accept = if accept.is_empty() {
        if stream {
            "text/event-stream"
        } else {
            "*/*"
        }
    } else {
        accept
    };
    vec![
        ("content-type".to_string(), "application/json".to_string()),
        ("authorization".to_string(), "Bearer public".to_string()),
        ("user-agent".to_string(), CLIENT_UA.to_string()),
        ("x-opencode-client".to_string(), "desktop".to_string()),
        ("x-opencode-session".to_string(), session.to_string()),
        ("x-opencode-request".to_string(), request_id.to_string()),
        ("x-opencode-project".to_string(), "global".to_string()),
        ("accept".to_string(), accept.to_string()),
    ]
}

// 统一消息层：三种下游写法先转到这里，再按线上协议组装。

/// 统一角色。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

/// 统一内容块（文本 / 图 / 音频 / 文件 / 工具调用 / 工具结果）。
#[derive(Debug, Clone, PartialEq)]
pub enum Part {
    Text(String),
    Image {
        data_url: String,
    },
    Audio {
        data: String,
        format: String,
    },
    File {
        name: String,
        media_type: String,
        data: String,
    },
    ToolCall {
        id: String,
        name: String,
        arguments: String,
    },
    ToolResult {
        tool_call_id: String,
        text: String,
        is_error: bool,
    },
}

/// 统一消息：角色 + 内容块。
#[derive(Debug, Clone, PartialEq)]
pub struct Message {
    pub role: Role,
    pub parts: Vec<Part>,
}

impl Message {
    /// 纯文本拼接（会话种子取首条用户文本用）。
    pub fn text_of(&self) -> String {
        self.parts
            .iter()
            .filter_map(|p| match p {
                Part::Text(t) => Some(t.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("")
    }
}

/// 统一工具声明（parameters 是 JSON 原文字符串）。
#[derive(Debug, Clone, PartialEq)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub parameters: String,
}

fn str_field(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

fn raw_json_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        _ => serde_json::to_string(v).unwrap_or_default(),
    }
}

/// 上游发送前修配对：删掉没有后续结果的调用、没有调用的结果；
/// 纯悬空 assistant turn（无文本）整条丢弃。
pub fn repair_tool_pairing(messages: Vec<Message>) -> Vec<Message> {
    let mut with_result = HashSet::new();
    for m in &messages {
        if m.role == Role::Tool {
            for p in &m.parts {
                if let Part::ToolResult { tool_call_id, .. } = p {
                    with_result.insert(tool_call_id.clone());
                }
            }
        }
    }
    let mut out = Vec::new();
    for m in messages {
        match m.role {
            Role::Assistant => {
                let has_text = m.parts.iter().any(|p| matches!(p, Part::Text(_)));
                let calls: Vec<&Part> = m
                    .parts
                    .iter()
                    .filter(|p| matches!(p, Part::ToolCall { .. }))
                    .collect();
                if !has_text && calls.is_empty() {
                    continue;
                }
                let mut parts = Vec::new();
                for p in &m.parts {
                    match p {
                        Part::ToolCall { id, .. } if !with_result.contains(id) => {}
                        _ => parts.push(p.clone()),
                    }
                }
                if parts.is_empty() {
                    continue;
                }
                out.push(Message { role: m.role, parts });
            }
            Role::Tool => {
                // 结果块只留配得上调用的；整条没了就整条丢。
                let mut parts = Vec::new();
                for p in &m.parts {
                    match p {
                        Part::ToolResult { tool_call_id, .. } => {
                            parts.push(p.clone());
                            let _ = tool_call_id;
                        }
                        _ => parts.push(p.clone()),
                    }
                }
                out.push(Message { role: m.role, parts });
            }
            _ => out.push(m),
        }
    }
    // tool 结果没有对应调用的：反查调用 id 集合再过一遍。
    let mut call_ids = HashSet::new();
    for m in &out {
        if m.role == Role::Assistant {
            for p in &m.parts {
                if let Part::ToolCall { id, .. } = p {
                    call_ids.insert(id.clone());
                }
            }
        }
    }
    out.into_iter()
        .filter_map(|m| {
            if m.role != Role::Tool {
                return Some(m);
            }
            let parts: Vec<Part> = m
                .parts
                .into_iter()
                .filter(|p| match p {
                    Part::ToolResult { tool_call_id, .. } => call_ids.contains(tool_call_id),
                    _ => true,
                })
                .collect();
            if parts.is_empty() {
                None
            } else {
                Some(Message { role: Role::Tool, parts })
            }
        })
        .collect()
}

// 下游 → 统一：OpenAI chat 入口。

/// `content`（字符串或数组）转文本/图/音频/文件块，未知 `type` 丢弃。
fn chat_content_parts(content: &Value) -> Vec<Part> {
    match content {
        Value::String(s) => vec![Part::Text(s.clone())],
        Value::Array(items) => {
            let mut out = Vec::new();
            for item in items {
                let kind = item.get("type").and_then(Value::as_str).unwrap_or("");
                match kind {
                    "text" => {
                        if let Some(t) = item.get("text").and_then(Value::as_str) {
                            out.push(Part::Text(t.to_string()));
                        }
                    }
                    "image_url" => {
                        if let Some(url) = item.get("image_url").and_then(|u| {
                            u.as_str().map(str::to_string).or_else(|| {
                                u.get("url").and_then(Value::as_str).map(str::to_string)
                            })
                        }) {
                            out.push(Part::Image { data_url: url });
                        }
                    }
                    "input_audio" => {
                        if let Some(inner) = item.get("input_audio") {
                            out.push(Part::Audio {
                                data: str_field(inner, "data"),
                                format: {
                                    let f = str_field(inner, "format");
                                    if f.is_empty() { "wav".to_string() } else { f }
                                },
                            });
                        }
                    }
                    "file" => {
                        if let Some(inner) = item.get("file") {
                            let (media, data) = split_file_data(&str_field(inner, "file_data"));
                            out.push(Part::File {
                                name: str_field(inner, "filename"),
                                media_type: media,
                                data,
                            });
                        }
                    }
                    _ => {}
                }
            }
            out
        }
        _ => Vec::new(),
    }
}

/// `data:<media>;base64,<data>` 拆成两段，对不上整段当数据。
fn split_file_data(raw: &str) -> (String, String) {
    if let Some(rest) = raw.strip_prefix("data:") {
        if let Some(i) = rest.find(";base64,") {
            return (rest[..i].to_string(), rest[i + 8..].to_string());
        }
    }
    (String::new(), raw.to_string())
}

/// OpenAI `messages[]` 转统一消息（含 tool_calls / tool 结果）。
pub fn convert_openai_messages(messages: &Value) -> Vec<Message> {
    let mut out = Vec::new();
    let items = messages.as_array().cloned().unwrap_or_default();
    for item in &items {
        let role = item.get("role").and_then(Value::as_str).unwrap_or("");
        match role {
            "system" | "developer" => out.push(Message {
                role: Role::System,
                parts: chat_content_parts(item.get("content").unwrap_or(&Value::Null)),
            }),
            "user" => out.push(Message {
                role: Role::User,
                parts: chat_content_parts(item.get("content").unwrap_or(&Value::Null)),
            }),
            "assistant" => {
                let mut parts = chat_content_parts(item.get("content").unwrap_or(&Value::Null));
                if let Some(calls) = item.get("tool_calls").and_then(Value::as_array) {
                    for call in calls {
                        let function = call.get("function").unwrap_or(&Value::Null);
                        parts.push(Part::ToolCall {
                            id: str_field(call, "id"),
                            name: str_field(function, "name"),
                            arguments: str_field(function, "arguments"),
                        });
                    }
                }
                out.push(Message { role: Role::Assistant, parts });
            }
            "tool" => {
                let content = item.get("content").unwrap_or(&Value::Null);
                let id = str_field(item, "tool_call_id");
                match content {
                    Value::Array(_) => {
                        for part in chat_content_parts(content) {
                            if let Part::Text(text) = part {
                                out.push(Message {
                                    role: Role::Tool,
                                    parts: vec![Part::ToolResult {
                                        tool_call_id: id.clone(),
                                        text,
                                        is_error: false,
                                    }],
                                });
                            }
                        }
                    }
                    _ => out.push(Message {
                        role: Role::Tool,
                        parts: vec![Part::ToolResult {
                            tool_call_id: id,
                            text: raw_json_string(content),
                            is_error: false,
                        }],
                    }),
                }
            }
            _ => {}
        }
    }
    out
}

/// OpenAI tools（`{type:function,function:{…}}` 或扁平）转统一声明。
pub fn convert_openai_tools(tools: &Value) -> Vec<ToolDef> {
    let mut out = Vec::new();
    let items = tools.as_array().cloned().unwrap_or_default();
    for item in &items {
        if let Some(function) = item.get("function") {
            out.push(ToolDef {
                name: str_field(function, "name"),
                description: str_field(function, "description"),
                parameters: raw_json_string(function.get("parameters").unwrap_or(&Value::Null)),
            });
        } else {
            out.push(ToolDef {
                name: str_field(item, "name"),
                description: str_field(item, "description"),
                parameters: raw_json_string(item.get("parameters").unwrap_or(&Value::Null)),
            });
        }
    }
    out
}

// 下游 → 统一：OpenAI responses 入口。

/// responses 内容块转统一块（扁平写法：`input_audio{data,format}` 等）。
fn responses_content_parts(content: &Value) -> Vec<Part> {
    match content {
        Value::String(s) => vec![Part::Text(s.clone())],
        Value::Array(items) => {
            let mut out = Vec::new();
            for item in items {
                let kind = item.get("type").and_then(Value::as_str).unwrap_or("");
                match kind {
                    "text" | "input_text" | "output_text" | "summary_text" => {
                        if let Some(t) = item.get("text").and_then(Value::as_str) {
                            out.push(Part::Text(t.to_string()));
                        }
                    }
                    "image_url" | "input_image" => {
                        let url = item
                            .get("image_url")
                            .and_then(|u| {
                                u.as_str().map(str::to_string).or_else(|| {
                                    u.get("url").and_then(Value::as_str).map(str::to_string)
                                })
                            })
                            .or_else(|| item.get("url").and_then(Value::as_str).map(str::to_string));
                        if let Some(url) = url {
                            out.push(Part::Image { data_url: url });
                        }
                    }
                    "input_audio" => out.push(Part::Audio {
                        data: str_field(item, "data"),
                        format: {
                            let f = str_field(item, "format");
                            if f.is_empty() { "wav".to_string() } else { f }
                        },
                    }),
                    "input_file" => {
                        let (media, data) = split_file_data(&str_field(item, "file_data"));
                        out.push(Part::File {
                            name: str_field(item, "filename"),
                            media_type: media,
                            data,
                        });
                    }
                    _ => {}
                }
            }
            out
        }
        _ => Vec::new(),
    }
}

/// responses `input`（字符串 / 条目数组）转统一消息；
/// `instructions` 非空挂首条 system。`reasoning` 历史项直接丢弃。
pub fn convert_responses_input(input: &Value, instructions: &str) -> Vec<Message> {
    let mut out = Vec::new();
    if !instructions.is_empty() {
        out.push(Message { role: Role::System, parts: vec![Part::Text(instructions.to_string())] });
    }
    match input {
        Value::String(s) => out.push(Message { role: Role::User, parts: vec![Part::Text(s.clone())] }),
        Value::Array(items) => {
            for item in items {
                let kind = item.get("type").and_then(Value::as_str).unwrap_or("");
                match kind {
                    "" | "message" => {
                        let role = match item.get("role").and_then(Value::as_str).unwrap_or("user") {
                            "system" | "developer" => Role::System,
                            "assistant" => Role::Assistant,
                            _ => Role::User,
                        };
                        out.push(Message {
                            role,
                            parts: responses_content_parts(item.get("content").unwrap_or(&Value::Null)),
                        });
                    }
                    "function_call" => out.push(Message {
                        role: Role::Assistant,
                        parts: vec![Part::ToolCall {
                            id: str_field(item, "call_id"),
                            name: str_field(item, "name"),
                            arguments: str_field(item, "arguments"),
                        }],
                    }),
                    "function_call_output" => out.push(Message {
                        role: Role::Tool,
                        parts: vec![Part::ToolResult {
                            tool_call_id: str_field(item, "call_id"),
                            text: raw_json_string(item.get("output").unwrap_or(&Value::Null)),
                            is_error: false,
                        }],
                    }),
                    _ => {}
                }
            }
        }
        _ => {}
    }
    out
}

// 下游 → 统一：Anthropic messages 入口。

/// Anthropic 内容块转统一块（`is_error` 只有这条入口保留）。
fn anthropic_parts(content: &Value) -> Vec<Part> {
    match content {
        Value::String(s) => vec![Part::Text(s.clone())],
        Value::Array(items) => {
            let mut out = Vec::new();
            for item in items {
                let kind = item.get("type").and_then(Value::as_str).unwrap_or("");
                match kind {
                    "text" => {
                        if let Some(t) = item.get("text").and_then(Value::as_str) {
                            out.push(Part::Text(t.to_string()));
                        }
                    }
                    "image" => {
                        if let Some(source) = item.get("source") {
                            if source.get("type").and_then(Value::as_str) == Some("base64") {
                                out.push(Part::Image {
                                    data_url: format!(
                                        "data:{};base64,{}",
                                        str_field(source, "media_type"),
                                        str_field(source, "data")
                                    ),
                                });
                            }
                        }
                    }
                    "document" => {
                        if let Some(source) = item.get("source") {
                            if source.get("type").and_then(Value::as_str) == Some("base64") {
                                let media = str_field(source, "media_type");
                                out.push(Part::File {
                                    name: str_field(item, "title"),
                                    media_type: if media.is_empty() {
                                        "application/pdf".to_string()
                                    } else {
                                        media
                                    },
                                    data: str_field(source, "data"),
                                });
                            }
                        }
                    }
                    "tool_use" => {
                        let input = item.get("input").unwrap_or(&Value::Null);
                        let mut arguments = raw_json_string(input);
                        if arguments.is_empty() {
                            arguments = "{}".to_string();
                        }
                        out.push(Part::ToolCall {
                            id: str_field(item, "id"),
                            name: str_field(item, "name"),
                            arguments,
                        });
                    }
                    "tool_result" => {
                        let content = item.get("content").unwrap_or(&Value::Null);
                        let text = match content {
                            Value::String(s) => s.clone(),
                            Value::Array(blocks) => blocks
                                .iter()
                                .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
                                .filter_map(|b| b.get("text").and_then(Value::as_str))
                                .collect::<Vec<_>>()
                                .join(""),
                            _ => raw_json_string(content),
                        };
                        out.push(Part::ToolResult {
                            tool_call_id: str_field(item, "tool_use_id"),
                            text,
                            is_error: item.get("is_error").and_then(Value::as_bool).unwrap_or(false),
                        });
                    }
                    _ => {}
                }
            }
            out
        }
        _ => Vec::new(),
    }
}

/// Anthropic `system` + `messages[]` 转统一消息（`role: tool` 降级成 user）。
pub fn convert_anthropic(system: &Value, messages: &Value) -> Vec<Message> {
    let mut out = Vec::new();
    let sys_parts = anthropic_parts(system);
    if !sys_parts.is_empty() {
        out.push(Message { role: Role::System, parts: sys_parts });
    }
    let items = messages.as_array().cloned().unwrap_or_default();
    for item in &items {
        let role = match item.get("role").and_then(Value::as_str).unwrap_or("") {
            "assistant" => Role::Assistant,
            "user" | "tool" => Role::User,
            _ => continue,
        };
        out.push(Message {
            role,
            parts: anthropic_parts(item.get("content").unwrap_or(&Value::Null)),
        });
    }
    out
}

/// Anthropic tools（`{name,description,input_schema}`）转统一声明。
pub fn convert_anthropic_tools(tools: &Value) -> Vec<ToolDef> {
    tools
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|item| ToolDef {
            name: str_field(item, "name"),
            description: str_field(item, "description"),
            parameters: raw_json_string(item.get("input_schema").unwrap_or(&Value::Null)),
        })
        .collect()
}

// 统一 → 上游：三套线上写法。

fn parse_data_url(url: &str) -> Option<(String, String)> {
    let rest = url.strip_prefix("data:")?;
    let i = rest.find(";base64,")?;
    Some((rest[..i].to_string(), rest[i + 8..].to_string()))
}

fn json_or_empty_object(raw: &str) -> Value {
    if raw.trim().is_empty() {
        return Value::Object(Map::new());
    }
    serde_json::from_str(raw).unwrap_or(Value::Object(Map::new()))
}

/// chat 线上消息体：原文逻辑逐行对齐——非文本块跨消息攒进 `pending`，
/// 有文本先发文本 turn 再把攒的发成独立 turn；tool 结果整条合并成一 turn。
pub fn to_chat_messages(messages: &[Message]) -> Value {
    let mut out = Vec::new();
    let mut pending: Vec<Value> = Vec::new();
    let flush_parts = |out: &mut Vec<Value>, pending: &mut Vec<Value>| {
        if !pending.is_empty() {
            out.push(Value::Object({
                let mut o = Map::new();
                o.insert("role".to_string(), Value::String("user".to_string()));
                o.insert("content".to_string(), Value::Array(std::mem::take(pending)));
                o
            }));
        }
    };
    for m in messages {
        match m.role {
            Role::System => {
                let text = m.text_of();
                if !text.trim().is_empty() {
                    flush_parts(&mut out, &mut pending);
                    out.push(Value::Object({
                        let mut o = Map::new();
                        o.insert("role".to_string(), Value::String("system".to_string()));
                        o.insert("content".to_string(), Value::String(text));
                        o
                    }));
                }
            }
            Role::User => {
                let mut text = String::new();
                for p in &m.parts {
                    match p {
                        Part::Text(t) => text.push_str(t),
                        Part::Image { data_url } => pending.push(Value::Object({
                            let mut o = Map::new();
                            o.insert("type".to_string(), Value::String("image_url".to_string()));
                            let mut u = Map::new();
                            u.insert("url".to_string(), Value::String(data_url.clone()));
                            o.insert("image_url".to_string(), Value::Object(u));
                            o
                        })),
                        Part::Audio { data, format } => pending.push(Value::Object({
                            let mut o = Map::new();
                            o.insert("type".to_string(), Value::String("input_audio".to_string()));
                            let mut u = Map::new();
                            u.insert("data".to_string(), Value::String(data.clone()));
                            u.insert("format".to_string(), Value::String(format.clone()));
                            o.insert("input_audio".to_string(), Value::Object(u));
                            o
                        })),
                        Part::File { name, media_type, data } => pending.push(Value::Object({
                            let mut o = Map::new();
                            o.insert("type".to_string(), Value::String("file".to_string()));
                            let mut u = Map::new();
                            u.insert("filename".to_string(), Value::String(name.clone()));
                            u.insert(
                                "file_data".to_string(),
                                Value::String(format!("data:{media_type};base64,{data}")),
                            );
                            o.insert("file".to_string(), Value::Object(u));
                            o
                        })),
                        _ => {}
                    }
                }
                // 文本和攒的块共存：文本 turn 在前，攒的块随后独立成 turn；
                // 没文本就把攒的块当本 turn 内容。
                let content = if !pending.is_empty() && text.is_empty() {
                    Value::Array(std::mem::take(&mut pending))
                } else {
                    Value::String(text)
                };
                out.push(Value::Object({
                    let mut o = Map::new();
                    o.insert("role".to_string(), Value::String("user".to_string()));
                    o.insert("content".to_string(), content);
                    o
                }));
                flush_parts(&mut out, &mut pending);
            }
            Role::Assistant => {
                let mut calls = Vec::new();
                for p in &m.parts {
                    if let Part::ToolCall { id, name, arguments } = p {
                        let arguments = if arguments.trim().is_empty() {
                            "{}".to_string()
                        } else {
                            arguments.clone()
                        };
                        let mut f = Map::new();
                        f.insert("name".to_string(), Value::String(name.clone()));
                        f.insert("arguments".to_string(), Value::String(arguments));
                        let mut o = Map::new();
                        o.insert("id".to_string(), Value::String(id.clone()));
                        o.insert("type".to_string(), Value::String("function".to_string()));
                        o.insert("function".to_string(), Value::Object(f));
                        calls.push(Value::Object(o));
                    }
                }
                let mut o = Map::new();
                o.insert("role".to_string(), Value::String("assistant".to_string()));
                o.insert("content".to_string(), Value::String(m.text_of()));
                if !calls.is_empty() {
                    o.insert("tool_calls".to_string(), Value::Array(calls));
                }
                flush_parts(&mut out, &mut pending);
                out.push(Value::Object(o));
            }
            Role::Tool => {
                // 整条结果合并成一 turn：文本拼接，取第一个 id。
                let mut text = String::new();
                let mut tool_call_id = String::new();
                for p in &m.parts {
                    if let Part::ToolResult { tool_call_id: id, text: t, .. } = p {
                        text.push_str(t);
                        if tool_call_id.is_empty() {
                            tool_call_id = id.clone();
                        }
                    }
                }
                let mut o = Map::new();
                o.insert("role".to_string(), Value::String("tool".to_string()));
                o.insert("tool_call_id".to_string(), Value::String(tool_call_id));
                o.insert("content".to_string(), Value::String(text));
                out.push(Value::Object(o));
            }
        }
    }
    flush_parts(&mut out, &mut pending);
    Value::Array(out)
}

/// messages 线上写法：返回 `(system, messages)`；同 role 相邻合并；
/// user 内 tool_result 置顶；图只收 png/jpeg/webp/gif，文档只收 pdf，
/// 音频直接丢弃；assistant 空白文本跳过。
pub fn to_claude_parts(messages: &[Message]) -> (Option<String>, Value) {
    let mut system = String::new();
    let mut turns: Vec<(String, Vec<Value>)> = Vec::new();
    let push_turn = |turns: &mut Vec<(String, Vec<Value>)>, role: &str, blocks: Vec<Value>| {
        if blocks.is_empty() {
            return;
        }
        if let Some(last) = turns.last_mut() {
            if last.0 == role {
                last.1.extend(blocks);
                return;
            }
        }
        turns.push((role.to_string(), blocks));
    };
    for m in messages {
        match m.role {
            Role::System => {
                if !system.is_empty() {
                    system.push_str("\n\n");
                }
                system.push_str(&m.text_of());
            }
            Role::User => {
                let mut results = Vec::new();
                let mut rest = Vec::new();
                for p in &m.parts {
                    match p {
                        Part::ToolResult { tool_call_id, text, is_error } => {
                            let mut o = Map::new();
                            o.insert("type".to_string(), Value::String("tool_result".to_string()));
                            o.insert("tool_use_id".to_string(), Value::String(tool_call_id.clone()));
                            o.insert("content".to_string(), Value::String(text.clone()));
                            if *is_error {
                                o.insert("is_error".to_string(), Value::Bool(true));
                            }
                            results.push(Value::Object(o));
                        }
                        Part::Text(t) => {
                            let mut o = Map::new();
                            o.insert("type".to_string(), Value::String("text".to_string()));
                            o.insert("text".to_string(), Value::String(t.clone()));
                            rest.push(Value::Object(o));
                        }
                        Part::Image { data_url } => {
                            if let Some((media, data)) = parse_data_url(data_url) {
                                if ["image/png", "image/jpeg", "image/webp", "image/gif"]
                                    .contains(&media.as_str())
                                {
                                    let mut s = Map::new();
                                    s.insert("type".to_string(), Value::String("base64".to_string()));
                                    s.insert("media_type".to_string(), Value::String(media));
                                    s.insert("data".to_string(), Value::String(data));
                                    let mut o = Map::new();
                                    o.insert("type".to_string(), Value::String("image".to_string()));
                                    o.insert("source".to_string(), Value::Object(s));
                                    rest.push(Value::Object(o));
                                }
                            }
                        }
                        Part::File { name, media_type, data } => {
                            if media_type == "application/pdf" {
                                let mut s = Map::new();
                                s.insert("type".to_string(), Value::String("base64".to_string()));
                                s.insert("media_type".to_string(), Value::String(media_type.clone()));
                                s.insert("data".to_string(), Value::String(data.clone()));
                                let mut o = Map::new();
                                o.insert("type".to_string(), Value::String("document".to_string()));
                                o.insert("source".to_string(), Value::Object(s));
                                if !name.is_empty() {
                                    o.insert("title".to_string(), Value::String(name.clone()));
                                }
                                rest.push(Value::Object(o));
                            }
                        }
                        _ => {}
                    }
                }
                let mut blocks = results;
                blocks.extend(rest);
                push_turn(&mut turns, "user", blocks);
            }
            Role::Assistant => {
                let mut blocks = Vec::new();
                for p in &m.parts {
                    match p {
                        Part::Text(t) if !t.trim().is_empty() => {
                            let mut o = Map::new();
                            o.insert("type".to_string(), Value::String("text".to_string()));
                            o.insert("text".to_string(), Value::String(t.clone()));
                            blocks.push(Value::Object(o));
                        }
                        Part::ToolCall { id, name, arguments } => {
                            let mut o = Map::new();
                            o.insert("type".to_string(), Value::String("tool_use".to_string()));
                            o.insert("id".to_string(), Value::String(id.clone()));
                            o.insert("name".to_string(), Value::String(name.clone()));
                            o.insert("input".to_string(), json_or_empty_object(arguments));
                            blocks.push(Value::Object(o));
                        }
                        _ => {}
                    }
                }
                push_turn(&mut turns, "assistant", blocks);
            }
            Role::Tool => {
                let mut blocks = Vec::new();
                for p in &m.parts {
                    if let Part::ToolResult { tool_call_id, text, is_error } = p {
                        let mut o = Map::new();
                        o.insert("type".to_string(), Value::String("tool_result".to_string()));
                        o.insert("tool_use_id".to_string(), Value::String(tool_call_id.clone()));
                        o.insert("content".to_string(), Value::String(text.clone()));
                        if *is_error {
                            o.insert("is_error".to_string(), Value::Bool(true));
                        }
                        blocks.push(Value::Object(o));
                    }
                }
                push_turn(&mut turns, "user", blocks);
            }
        }
    }
    let messages = turns
        .into_iter()
        .map(|(role, content)| {
            let mut o = Map::new();
            o.insert("role".to_string(), Value::String(role));
            o.insert("content".to_string(), Value::Array(content));
            Value::Object(o)
        })
        .collect();
    let system = if system.is_empty() { None } else { Some(system) };
    (system, Value::Array(messages))
}

/// responses 线上写法：返回 `(instructions, items)`。
/// 用户内容包进 `{role:"user",content:[input_text 置顶, …]}`；
/// assistant 文本每段一条消息（空白跳过）；`input_file` 带 `data:` 前缀。
pub fn to_response_items(messages: &[Message]) -> (Option<String>, Value) {
    let mut sys_parts: Vec<String> = Vec::new();
    let mut items = Vec::new();
    for m in messages {
        match m.role {
            Role::System => sys_parts.push(m.text_of()),
            Role::User => {
                let mut text = String::new();
                let mut media = Vec::new();
                for p in &m.parts {
                    match p {
                        Part::Text(t) => text.push_str(t),
                        Part::ToolResult { tool_call_id, text, .. } => {
                            items.push(Value::Object({
                                let mut o = Map::new();
                                o.insert(
                                    "type".to_string(),
                                    Value::String("function_call_output".to_string()),
                                );
                                o.insert("call_id".to_string(), Value::String(tool_call_id.clone()));
                                o.insert("output".to_string(), Value::String(text.clone()));
                                o
                            }));
                        }
                        Part::Image { data_url } => media.push(Value::Object({
                            let mut o = Map::new();
                            o.insert("type".to_string(), Value::String("input_image".to_string()));
                            o.insert("image_url".to_string(), Value::String(data_url.clone()));
                            o
                        })),
                        Part::Audio { data, format } => media.push(Value::Object({
                            let mut o = Map::new();
                            o.insert("type".to_string(), Value::String("input_audio".to_string()));
                            o.insert("data".to_string(), Value::String(data.clone()));
                            o.insert("format".to_string(), Value::String(format.clone()));
                            o
                        })),
                        Part::File { name, media_type, data } => {
                            let mut o = Map::new();
                            o.insert("type".to_string(), Value::String("input_file".to_string()));
                            o.insert(
                                "file_data".to_string(),
                                Value::String(format!("data:{media_type};base64,{data}")),
                            );
                            if !name.is_empty() {
                                o.insert("filename".to_string(), Value::String(name.clone()));
                            }
                            media.push(Value::Object(o));
                        }
                        _ => {}
                    }
                }
                if !text.trim().is_empty() || !media.is_empty() {
                    // 文本置顶：input_text 排在媒体块前面。
                    let mut content = Vec::new();
                    if !text.trim().is_empty() {
                        content.push(Value::Object({
                            let mut o = Map::new();
                            o.insert("type".to_string(), Value::String("input_text".to_string()));
                            o.insert("text".to_string(), Value::String(text));
                            o
                        }));
                    }
                    content.extend(media);
                    items.push(Value::Object({
                        let mut o = Map::new();
                        o.insert("role".to_string(), Value::String("user".to_string()));
                        o.insert("content".to_string(), Value::Array(content));
                        o
                    }));
                }
            }
            Role::Assistant => {
                for p in &m.parts {
                    match p {
                        Part::Text(t) if !t.trim().is_empty() => {
                            items.push(Value::Object({
                                let mut o = Map::new();
                                o.insert("role".to_string(), Value::String("assistant".to_string()));
                                let mut c = Map::new();
                                c.insert("type".to_string(), Value::String("output_text".to_string()));
                                c.insert("text".to_string(), Value::String(t.clone()));
                                o.insert("content".to_string(), Value::Array(vec![Value::Object(c)]));
                                o
                            }));
                        }
                        Part::ToolCall { id, name, arguments } => {
                            let arguments = if arguments.trim().is_empty() {
                                "{}".to_string()
                            } else {
                                arguments.clone()
                            };
                            items.push(Value::Object({
                                let mut o = Map::new();
                                o.insert("type".to_string(), Value::String("function_call".to_string()));
                                o.insert("call_id".to_string(), Value::String(id.clone()));
                                o.insert("name".to_string(), Value::String(name.clone()));
                                o.insert("arguments".to_string(), Value::String(arguments));
                                o
                            }));
                        }
                        _ => {}
                    }
                }
            }
            Role::Tool => {
                for p in &m.parts {
                    if let Part::ToolResult { tool_call_id, text, .. } = p {
                        items.push(Value::Object({
                            let mut o = Map::new();
                            o.insert(
                                "type".to_string(),
                                Value::String("function_call_output".to_string()),
                            );
                            o.insert("call_id".to_string(), Value::String(tool_call_id.clone()));
                            o.insert("output".to_string(), Value::String(text.clone()));
                            o
                        }));
                    }
                }
            }
        }
    }
    let instructions = {
        let joined = sys_parts.join("\n\n");
        let trimmed = joined.trim().to_string();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed)
        }
    };
    (instructions, Value::Array(items))
}

/// 统一工具声明转三套线上写法（空 parameters 补空对象）。
/// chat / flat / claude 三种键名。
pub fn to_tool_defs(tools: &[ToolDef], style: &str) -> Value {
    tools
        .iter()
        .map(|t| {
            let params = if t.parameters.trim().is_empty() {
                serde_json::json!({"type": "object", "properties": {}})
            } else {
                json_or_empty_object(&t.parameters)
            };
            match style {
                "flat" => serde_json::json!({
                    "type": "function",
                    "name": t.name,
                    "description": t.description,
                    "parameters": params,
                }),
                "claude" => serde_json::json!({
                    "name": t.name,
                    "description": t.description,
                    "input_schema": params,
                }),
                _ => serde_json::json!({
                    "type": "function",
                    "function": {
                        "name": t.name,
                        "description": t.description,
                        "parameters": params,
                    },
                }),
            }
        })
        .collect::<Vec<_>>()
        .into()
}

// 指纹：请求体 tools 四件套强制齐全。

/// 工具名归一到槽位（大小写不敏感），四件套之外返回空。
/// 返回 `'static`：调用方拿走所有权不借用。
fn quartet_key(name: &str) -> &'static str {
    let lower = name.trim().to_lowercase();
    for t in FINGERPRINT_TOOLS {
        if *t == lower {
            return t;
        }
    }
    ""
}

fn tool_name_of(tool: &Map<String, Value>) -> String {
    if let Some(n) = tool.get("name").and_then(Value::as_str) {
        if !n.trim().is_empty() {
            return n.trim().to_string();
        }
    }
    tool.get("function")
        .and_then(|f| f.get("name"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string()
}

fn clone_tool_with_name(tool: &Map<String, Value>, name: &str) -> Map<String, Value> {
    if let Some(function) = tool.get("function").and_then(Value::as_object) {
        let mut cloned_fn = function.clone();
        cloned_fn.insert("name".to_string(), Value::String(name.to_string()));
        let mut cloned = Map::new();
        cloned.insert(
            "type".to_string(),
            tool.get("type").cloned().unwrap_or(Value::Null),
        );
        cloned.insert("function".to_string(), Value::Object(cloned_fn));
        cloned
    } else {
        let mut cloned = tool.clone();
        cloned.insert("name".to_string(), Value::String(name.to_string()));
        cloned
    }
}

fn decoy_tool(name: &str, style: &str) -> Value {
    let desc = "This tool is currently unavailable and must not be used.";
    match style {
        "claude" => serde_json::json!({
            "name": name,
            "description": desc,
            "input_schema": {"type": "object", "properties": {}},
        }),
        "flat" => serde_json::json!({
            "type": "function",
            "name": name,
            "description": desc,
            "parameters": {"type": "object", "properties": {}},
        }),
        _ => serde_json::json!({
            "type": "function",
            "function": {
                "name": name,
                "description": desc,
                "parameters": {"type": "object", "properties": {}},
            },
        }),
    }
}

/// 指纹：`body["tools"]` 四件套强制齐全。调用方已有槽位就地规范化；
/// 缺的先拿真工具顶（`pwsh`→`bash`），顶不上的塞自禁用 decoy。
/// 返回 发送名→调用方原名，回程恢复用。`body` 就地改。
/// style：`"chat"` / `"flat"`（responses）/ `"claude"`（messages）。
pub fn apply_fingerprint(body: &mut Map<String, Value>, style: &str) -> HashMap<String, String> {
    let claude = style == "claude";
    let flat = style == "flat";
    let mut rename: HashMap<String, String> = HashMap::new();
    let tools: Vec<Map<String, Value>> = match body.get("tools") {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|v| v.as_object().cloned())
            .collect(),
        _ => Vec::new(),
    };
    let had_client_tools = !tools.is_empty();

    let mut seen: HashSet<String> = HashSet::new();
    let mut out: Vec<Value> = Vec::with_capacity(tools.len() + 4);
    for tool in &tools {
        let current = tool_name_of(tool);
        let key = quartet_key(&current);
        if key.is_empty() {
            out.push(Value::Object(tool.clone()));
            continue;
        }
        if seen.contains(key) {
            continue;
        }
        seen.insert(key.to_string());
        if current != key {
            rename.insert(key.to_string(), current);
            out.push(Value::Object(clone_tool_with_name(tool, key)));
        } else {
            out.push(Value::Object(tool.clone()));
        }
    }

    let mut promoted: HashSet<String> = HashSet::new();
    for name in FINGERPRINT_TOOLS {
        if seen.contains(*name) {
            continue;
        }
        let donors: Vec<&str> = QUARTET_DONORS
            .iter()
            .filter(|(slot, _)| slot == name)
            .map(|(_, donor)| *donor)
            .collect();
        let mut idx: Option<usize> = None;
        for (i, t) in out.iter().enumerate() {
            let tm = match t.as_object() {
                Some(m) => m,
                None => continue,
            };
            let orig = tool_name_of(tm);
            if orig.is_empty() || !quartet_key(&orig).is_empty() {
                continue;
            }
            let lower = orig.to_lowercase();
            if promoted.contains(&lower) || !donors.contains(&lower.as_str()) {
                continue;
            }
            idx = Some(i);
            break;
        }
        let Some(i) = idx else { continue };
        let tm = out[i].as_object().cloned().unwrap_or_default();
        let orig = tool_name_of(&tm);
        promoted.insert(orig.to_lowercase());
        rename.insert(name.to_string(), orig);
        out[i] = Value::Object(clone_tool_with_name(&tm, name));
        seen.insert(name.to_string());
    }

    for name in FINGERPRINT_TOOLS {
        if seen.contains(*name) {
            continue;
        }
        out.push(decoy_tool(name, style));
    }

    body.insert("tools".to_string(), Value::Array(out));
    if !body.contains_key("tool_choice") {
        if flat {
            body.insert("tool_choice".to_string(), Value::String("auto".to_string()));
        } else if !had_client_tools {
            if claude {
                let mut o = Map::new();
                o.insert("type".to_string(), Value::String("none".to_string()));
                body.insert("tool_choice".to_string(), Value::Object(o));
            } else {
                body.insert("tool_choice".to_string(), Value::String("none".to_string()));
            }
        }
    }
    rename
}

/// 发送名恢复成调用方原名（回程工具名用）。
pub fn restore_tool_name(name: &str, rename: &HashMap<String, String>) -> String {
    rename.get(name).cloned().unwrap_or_else(|| name.to_string())
}

/// 按线上协议组装上游请求体（含指纹），返回 `(body, rename)`。
/// 上游永远流式；预算调用方算好传进来。
pub fn build_upstream_body(
    wire: Wire,
    model_base: &str,
    messages: &[Message],
    tools: &[ToolDef],
    budget: i64,
) -> (Map<String, Value>, HashMap<String, String>) {
    let mut body = Map::new();
    let rename = match wire {
        Wire::Chat => {
            body.insert("model".to_string(), Value::String(model_base.to_string()));
            body.insert("stream".to_string(), Value::Bool(true));
            body.insert("messages".to_string(), to_chat_messages(messages));
            body.insert("max_tokens".to_string(), Value::Number(budget.into()));
            if !tools.is_empty() {
                body.insert("tools".to_string(), to_tool_defs(tools, "chat"));
            }
            apply_fingerprint(&mut body, "chat")
        }
        Wire::Responses => {
            body.insert("model".to_string(), Value::String(model_base.to_string()));
            body.insert("stream".to_string(), Value::Bool(true));
            let (instructions, items) = to_response_items(messages);
            body.insert("input".to_string(), items);
            body.insert("max_output_tokens".to_string(), Value::Number(budget.into()));
            body.insert("store".to_string(), Value::Bool(false));
            if let Some(text) = instructions {
                body.insert("instructions".to_string(), Value::String(text));
            }
            if !tools.is_empty() {
                body.insert("tools".to_string(), to_tool_defs(tools, "flat"));
            }
            apply_fingerprint(&mut body, "flat")
        }
        Wire::Messages => {
            // Anthropic 线写法：没有顶层 `model` 字段。
            let (system, items) = to_claude_parts(messages);
            body.insert("messages".to_string(), items);
            body.insert("max_tokens".to_string(), Value::Number(budget.into()));
            body.insert(
                "anthropic_version".to_string(),
                Value::String("2023-06-01".to_string()),
            );
            if let Some(text) = system {
                body.insert("system".to_string(), Value::String(text));
            }
            if !tools.is_empty() {
                body.insert("tools".to_string(), to_tool_defs(tools, "claude"));
            }
            apply_fingerprint(&mut body, "claude")
        }
        Wire::SystemOne => HashMap::new(),
    };
    (body, rename)
}

// 上游 SSE 解码 → Chunk（三种线上写法各一套事件表）。

/// 上游内容块种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockKind {
    Text,
    Reasoning,
    ToolCall,
}

/// 上游一帧译出的内容增量（usage 走 `note_usage`，终局走 `finish`）。
#[derive(Debug, Clone, PartialEq)]
pub enum Chunk {
    TextDelta(String),
    ReasonDelta(String),
    /// 新内容块：工具块带 id/name（下游首包用），文本/思考块空着。
    BlockStart {
        kind: BlockKind,
        id: String,
        name: String,
    },
    BlockEnd,
    ToolCallDelta {
        /// 分组键（上游块序号），同块同键。
        key: String,
        delta: String,
    },
}

/// usage 累计（prompt_tokens + completion_tokens 口径）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Usage {
    pub input: i64,
    pub output: i64,
    pub cache_read: i64,
}

impl Usage {
    pub fn total(&self) -> i64 {
        self.input + self.cache_read + self.output
    }
}

/// 流终局：`finish` 为空串表示上游直接断流（无终局帧），调用方按截断处理。
#[derive(Debug, Clone, Default)]
pub struct StreamOutcome {
    pub usage: Usage,
    pub finish: String,
}

/// DeepSeek 系偶发把 `<｜DSML｜…>` 控制标记混进正文（dsh 参考实现同款
/// 洗法）：整段正则清掉，跨帧半截先攒着（`flush` 在终局时吐）。
struct DsmlScrubber {
    held: String,
}

const DSML_OPENING: &str = "<｜DSML｜";

impl DsmlScrubber {
    fn push(&mut self, delta: &str) -> String {
        let mut text = std::mem::take(&mut self.held);
        text.push_str(delta);
        // 尾巴像 opening 的前缀、或开了头还没闭合：先攒着等下帧。
        let tail_start = text.len().saturating_sub(DSML_OPENING.len() + 32);
        let tail = &text[tail_start..];
        if DSML_OPENING.starts_with(tail)
            || (tail.contains(DSML_OPENING) && !tail.contains('>'))
        {
            self.held = text;
            return String::new();
        }
        strip_dsml(&text)
    }

    fn flush(&mut self) -> String {
        let text = std::mem::take(&mut self.held);
        strip_dsml(&text)
    }
}

fn strip_dsml(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = rest.find(DSML_OPENING) {
        out.push_str(&rest[..i]);
        let after = &rest[i + DSML_OPENING.len()..];
        match after.find('>') {
            Some(j) => rest = &after[j + 1..],
            None => {
                rest = "";
                break;
            }
        }
    }
    out.push_str(rest);
    out
}

/// 上游 SSE 解码器：按线分发事件 → `Chunk`；usage 另走 `note_usage`；
/// `finish()` 吐 hold 文本、关块、坏工具调用降级。
pub struct Decoder {
    wire: Wire,
    rename: HashMap<String, String>,
    /// 已开块：key → 块种类（工具块才需要终局校验）。
    blocks: HashMap<String, BlockKind>,
    tool_args: HashMap<String, String>,
    finish: String,
    saw_finish: bool,
    broken_tool_call: bool,
    checkpoint: String,
    dsml: DsmlScrubber,
    usage: Usage,
}

impl Decoder {
    pub fn new(wire: Wire, rename: HashMap<String, String>) -> Self {
        Self {
            wire,
            rename,
            blocks: HashMap::new(),
            tool_args: HashMap::new(),
            finish: String::new(),
            saw_finish: false,
            broken_tool_call: false,
            checkpoint: String::new(),
            dsml: DsmlScrubber { held: String::new() },
            usage: Usage::default(),
        }
    }

    fn start_block(
        &mut self,
        key: String,
        kind: BlockKind,
        id: String,
        name: String,
        out: &mut Vec<Chunk>,
    ) {
        if self.blocks.contains_key(&key) {
            return;
        }
        self.blocks.insert(key, kind);
        out.push(Chunk::BlockStart { kind, id, name });
    }

    fn text_block(&mut self, key: String, out: &mut Vec<Chunk>) {
        self.start_block(key, BlockKind::Text, String::new(), String::new(), out);
    }

    fn reasoning_block(&mut self, key: String, out: &mut Vec<Chunk>) {
        self.start_block(key, BlockKind::Reasoning, String::new(), String::new(), out);
    }

    fn end_block(&mut self, key: &str, out: &mut Vec<Chunk>) {
        if self.blocks.remove(key).is_some() {
            out.push(Chunk::BlockEnd);
        }
    }

    fn end_all_blocks(&mut self, out: &mut Vec<Chunk>) {
        // 只关块不验参：工具块终局前已经验过，文本块没有参数可验。
        let keys: Vec<String> = self.blocks.keys().cloned().collect();
        for key in keys {
            self.end_block(&key, out);
        }
    }

    fn tool_delta(&mut self, key: String, delta: String, out: &mut Vec<Chunk>) {
        if delta.is_empty() {
            return;
        }
        *self.tool_args.entry(key.clone()).or_default() += &delta;
        out.push(Chunk::ToolCallDelta { key, delta });
    }

    fn set_finish(&mut self, finish: &str) {
        self.finish = finish.to_string();
        self.saw_finish = true;
    }

    /// 空 arguments 或非法 JSON 的工具块记坏块（终局降级用）。
    fn check_tool_block(&mut self, key: &str) {
        let args = self.tool_args.get(key).cloned().unwrap_or_default();
        if args.trim().is_empty() || serde_json::from_str::<Value>(&args).is_err() {
            self.broken_tool_call = true;
        }
    }

    /// 已累计的 usage（传输中断裂时读数，调用方按截断回包）。
    pub fn usage(&self) -> Usage {
        self.usage.clone()
    }

    /// usage 帧合并（`prompt_tokens/input_tokens → input` 等，http 层调）。
    pub fn note_usage(&mut self, input: i64, output: i64, cache_read: i64) {
        self.usage.input += input;
        self.usage.output += output;
        self.usage.cache_read += cache_read;
    }

    /// 一行 `data:` 负载解码（`[DONE]` 由调用方处理，这里只收 JSON）。
    /// `error` 帧调用方在外层先分类，这里忽略。
    pub fn decode_data(&mut self, payload: &str, out: &mut Vec<Chunk>) {
        let frame: Value = match serde_json::from_str(payload) {
            Ok(v) => v,
            Err(_) => return,
        };
        match self.wire {
            Wire::Chat => self.decode_chat(&frame, out),
            Wire::Messages => self.decode_messages(&frame, out),
            Wire::Responses => self.decode_responses(&frame, out),
            Wire::SystemOne => {}
        }
    }

    /// 终局：吐 DSML hold 文本、关块；有终局但工具坏了降级 `max-tokens`。
    pub fn finish(&mut self, out: &mut Vec<Chunk>) -> StreamOutcome {
        let held = self.dsml.flush();
        if !held.is_empty() {
            self.text_block("text".to_string(), out);
            out.push(Chunk::TextDelta(held));
        }
        let tool_keys: Vec<String> = self
            .blocks
            .iter()
            .filter(|(_, kind)| **kind == BlockKind::ToolCall)
            .map(|(k, _)| k.clone())
            .collect();
        for key in tool_keys {
            self.check_tool_block(&key);
        }
        self.end_all_blocks(out);
        let mut finish = self.finish.clone();
        if self.saw_finish && finish == "tool-calls" && self.broken_tool_call {
            finish = "max-tokens".to_string();
        }
        StreamOutcome { usage: self.usage.clone(), finish }
    }

    fn restore(&self, name: &str) -> String {
        restore_tool_name(name, &self.rename)
    }

    fn decode_chat(&mut self, frame: &Value, out: &mut Vec<Chunk>) {
        let choice = frame
            .get("choices")
            .and_then(|c| c.as_array())
            .and_then(|a| a.first());
        // 流请求回整 JSON 的兼容：`message` 当 `delta` 读。
        let delta = choice
            .and_then(|c| c.get("delta"))
            .or_else(|| choice.and_then(|c| c.get("message")))
            .unwrap_or(&Value::Null);
        if let Some(reason) = choice
            .and_then(|c| c.get("finish_reason"))
            .and_then(Value::as_str)
        {
            if !reason.is_empty() && reason != "null" {
                self.set_finish(&map_finish_reason(reason));
            }
        }
        if let Some(text) = delta.get("content").and_then(Value::as_str) {
            if !text.is_empty() {
                // 推理中的检查点文本（zen-gate 断流续写用，v1 只攒不读）。
                self.checkpoint.push_str(text);
                let scrubbed = self.dsml.push(text);
                if !scrubbed.is_empty() {
                    self.text_block("text".to_string(), out);
                    out.push(Chunk::TextDelta(scrubbed));
                }
            }
        }
        let reason = delta
            .get("reasoning_content")
            .and_then(Value::as_str)
            .or_else(|| delta.get("reasoning").and_then(Value::as_str))
            .unwrap_or("");
        if !reason.is_empty() {
            self.reasoning_block("reasoning".to_string(), out);
            out.push(Chunk::ReasonDelta(reason.to_string()));
        }
        if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
            for call in calls {
                self.chat_tool_call(call, out);
            }
        }
    }

    fn chat_tool_call(&mut self, call: &Value, out: &mut Vec<Chunk>) {
        // 有 index 按 `tool:<index>` 分组，否则按 id，都没有挂末尾 open 块。
        let key = if let Some(ix) = call.get("index").and_then(Value::as_i64) {
            format!("tool:{ix}")
        } else if let Some(id) = call.get("id").and_then(Value::as_str) {
            if id.is_empty() {
                String::new()
            } else {
                format!("tool-id:{id}")
            }
        } else {
            String::new()
        };
        let key = if key.is_empty() {
            match self
                .blocks
                .iter()
                .find(|(_, kind)| **kind == BlockKind::ToolCall)
                .map(|(k, _)| k.clone())
            {
                Some(k) => k,
                None => return,
            }
        } else {
            key
        };
        let function = call.get("function").unwrap_or(&Value::Null);
        let name = function.get("name").and_then(Value::as_str).unwrap_or("");
        let arguments = function.get("arguments").and_then(Value::as_str).unwrap_or("");
        let mut id = call.get("id").and_then(Value::as_str).unwrap_or("").to_string();
        if id.is_empty() {
            use rand::RngCore;
            let mut b = [0u8; 6];
            rand::thread_rng().fill_bytes(&mut b);
            id = format!("call_{}", b.iter().map(|x| format!("{x:02x}")).collect::<String>());
        }
        let name = self.restore(name);
        let id = self.restore(&id);
        // 新键先开块（带 id/name，下游首包用），再记增量。
        self.start_block(key.clone(), BlockKind::ToolCall, id, name, out);
        self.tool_delta(key, arguments.to_string(), out);
    }

    fn decode_messages(&mut self, frame: &Value, out: &mut Vec<Chunk>) {
        let kind = frame.get("type").and_then(Value::as_str).unwrap_or("");
        match kind {
            "message_start" => {}
            "content_block_start" => {
                let ix = frame.get("index").and_then(Value::as_i64).unwrap_or(0);
                let block = frame.get("content_block").unwrap_or(&Value::Null);
                let btype = block.get("type").and_then(Value::as_str).unwrap_or("");
                let key = format!("msg:{ix}");
                match btype {
                    "text" => self.text_block(key, out),
                    "thinking" => self.reasoning_block(key, out),
                    "tool_use" => {
                        let id = block.get("id").and_then(Value::as_str).unwrap_or("");
                        let name = block.get("name").and_then(Value::as_str).unwrap_or("");
                        let name = self.restore(name);
                        let id = self.restore(id);
                        self.start_block(key, BlockKind::ToolCall, id, name, out);
                    }
                    _ => {}
                }
            }
            "content_block_delta" => {
                let ix = frame.get("index").and_then(Value::as_i64).unwrap_or(0);
                let key = format!("msg:{ix}");
                let delta = frame.get("delta").unwrap_or(&Value::Null);
                let dtype = delta.get("type").and_then(Value::as_str).unwrap_or("");
                match dtype {
                    "text_delta" => {
                        if let Some(text) = delta.get("text").and_then(Value::as_str) {
                            if !text.is_empty() {
                                self.text_block(key, out);
                                out.push(Chunk::TextDelta(text.to_string()));
                            }
                        }
                    }
                    "thinking_delta" => {
                        if let Some(text) = delta.get("thinking").and_then(Value::as_str) {
                            if !text.is_empty() {
                                self.reasoning_block(key, out);
                                out.push(Chunk::ReasonDelta(text.to_string()));
                            }
                        }
                    }
                    "input_json_delta" => {
                        let partial = delta.get("partial_json").and_then(Value::as_str).unwrap_or("");
                        self.tool_delta(key, partial.to_string(), out);
                    }
                    _ => {}
                }
            }
            "content_block_stop" => {
                let ix = frame.get("index").and_then(Value::as_i64).unwrap_or(0);
                let key = format!("msg:{ix}");
                self.check_tool_block(&key);
                self.end_block(&key, out);
            }
            "message_delta" => {
                if let Some(reason) = frame
                    .get("delta")
                    .and_then(|d| d.get("stop_reason"))
                    .and_then(Value::as_str)
                {
                    self.set_finish(&map_finish_reason(reason));
                }
            }
            "message_stop" => self.set_finish("stop"),
            _ => {}
        }
    }

    fn decode_responses(&mut self, frame: &Value, out: &mut Vec<Chunk>) {
        let kind = frame.get("type").and_then(Value::as_str).unwrap_or("");
        match kind {
            "response.output_item.added" => {
                let oi = frame.get("output_index").and_then(Value::as_i64).unwrap_or(0);
                let item = frame.get("item").unwrap_or(&Value::Null);
                let itype = item.get("type").and_then(Value::as_str).unwrap_or("");
                let key = format!("resp:{oi}");
                match itype {
                    "message" => self.text_block(key, out),
                    "reasoning" => self.reasoning_block(key, out),
                    "function_call" => {
                        let id = item
                            .get("call_id")
                            .and_then(Value::as_str)
                            .or_else(|| item.get("id").and_then(Value::as_str))
                            .unwrap_or("");
                        let name = item.get("name").and_then(Value::as_str).unwrap_or("");
                        let name = self.restore(name);
                        let id = self.restore(id);
                        self.start_block(key, BlockKind::ToolCall, id, name, out);
                    }
                    _ => {}
                }
            }
            "response.output_text.delta" => {
                let oi = frame.get("output_index").and_then(Value::as_i64).unwrap_or(0);
                let key = format!("resp:{oi}");
                if let Some(text) = frame.get("delta").and_then(Value::as_str) {
                    if !text.is_empty() {
                        self.text_block(key, out);
                        out.push(Chunk::TextDelta(text.to_string()));
                    }
                }
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                let oi = frame.get("output_index").and_then(Value::as_i64).unwrap_or(0);
                let key = format!("resp:{oi}");
                if let Some(text) = frame.get("delta").and_then(Value::as_str) {
                    if !text.is_empty() {
                        self.reasoning_block(key, out);
                        out.push(Chunk::ReasonDelta(text.to_string()));
                    }
                }
            }
            "response.function_call_arguments.delta" => {
                let oi = frame.get("output_index").and_then(Value::as_i64).unwrap_or(0);
                let key = format!("resp:{oi}");
                let delta = frame.get("delta").and_then(Value::as_str).unwrap_or("");
                self.tool_delta(key, delta.to_string(), out);
            }
            "response.output_item.done" => {
                let oi = frame.get("output_index").and_then(Value::as_i64).unwrap_or(0);
                let key = format!("resp:{oi}");
                self.check_tool_block(&key);
                self.end_block(&key, out);
            }
            "response.completed" | "response.done" => self.set_finish("stop"),
            "response.incomplete" => {
                let reason = frame
                    .get("response")
                    .and_then(|r| r.get("incomplete_details"))
                    .and_then(|d| d.get("reason"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if reason == "max_output_tokens" {
                    self.set_finish("max-tokens");
                } else {
                    self.set_finish("stop");
                }
            }
            "response.failed" => self.set_finish("stop"),
            _ => {}
        }
    }
}

/// 上游终局原文统一成 `stop` / `tool-calls` / `max-tokens` 三种。
pub fn map_finish_reason(reason: &str) -> String {
    match reason {
        "tool_calls" | "tool_use" | "function_call" => "tool-calls".to_string(),
        "length" | "max_tokens" | "max_output_tokens" | "incomplete" => "max-tokens".to_string(),
        _ => "stop".to_string(),
    }
}

// 下游回写：Chunk → 各路 SSE / JSON。

/// chat 下游终局映射（`tool-calls→tool_calls` 等）。
pub fn map_finish_chat(finish: &str) -> &str {
    match finish {
        "tool-calls" => "tool_calls",
        "max-tokens" => "length",
        "" => "length",
        _ => "stop",
    }
}

/// 下游错误体（OpenAI 形）。
pub fn openai_error(message: &str, kind: &str) -> Value {
    serde_json::json!({"error": {"message": message, "type": kind}})
}

/// 下游错误体（Anthropic 形）。
pub fn anthropic_error(message: &str, kind: &str) -> Value {
    serde_json::json!({"type": "error", "error": {"type": kind, "message": message}})
}

/// 上游失败分类 →（下游 HTTP 状态码，下游错误类型）。
/// payload 正则优先于状态码（zen-gate `ClassifyFailure` 原样）。
pub fn classify_failure(status: u16, payload: &str) -> (&'static str, u16, &'static str) {
    let lower = payload.to_lowercase();
    if lower.contains("regionerror")
        || lower.contains("not available in your country")
        || lower.contains("regionblock")
        || lower.contains("region block")
        || lower.contains("region-block")
        || lower.contains("region_block")
    {
        return ("REGION_BLOCKED", 403, "authentication_error");
    }
    if status == 429
        || lower.contains("freeusagelimiterror")
        || lower.contains("usage limit")
        || lower.contains("rate limit")
    {
        return ("RATE_LIMIT", 429, "rate_limit_error");
    }
    if status == 401 || status == 403 {
        return ("INVALID_CREDENTIAL", 401, "authentication_error");
    }
    if status == 404 || status == 400 || status == 422
        || lower.contains("modelerror")
        || lower.contains("model is unavailable")
        || lower.contains("endpoint is unavailable")
    {
        return ("SERVER", 502, "api_error");
    }
    if status >= 500 {
        return ("SERVER", 502, "api_error");
    }
    ("SERVER", 502, "api_error")
}

/// chat 流式发射器：decoy 抑制 + 工具重编号 + 增量攒包。
/// 指纹 decoy（没被下游声明的四件套调用）整块吞掉，不下发。
pub struct ChatEmitter {
    declared: HashSet<String>,
    open_tool: Option<OpenTool>,
    tool_index: usize,
    has_any: bool,
}

struct OpenTool {
    id: String,
    name: String,
    suppressed: bool,
    sent_head: bool,
}

impl ChatEmitter {
    pub fn new(declared: &[String]) -> Self {
        Self {
            declared: declared.iter().cloned().collect(),
            open_tool: None,
            tool_index: 0,
            has_any: false,
        }
    }

    pub fn has_any(&self) -> bool {
        self.has_any
    }

    /// 首包（role 占位，下游 OpenAI 习惯）。
    pub fn first_chunk(id: &str, created: i64, model: &str) -> Value {
        serde_json::json!({
            "id": id, "object": "chat.completion.chunk", "created": created, "model": model,
            "choices": [{"index": 0, "delta": {"role": "assistant", "content": ""}, "finish_reason": null}],
        })
    }

    /// 终局包（空 delta + finish_reason）。
    pub fn finish_chunk(id: &str, created: i64, model: &str, finish: &str) -> Value {
        serde_json::json!({
            "id": id, "object": "chat.completion.chunk", "created": created, "model": model,
            "choices": [{"index": 0, "delta": {}, "finish_reason": map_finish_chat(finish)}],
        })
    }

    /// usage 尾包（`stream_options.include_usage` 才发）。
    pub fn usage_chunk(id: &str, created: i64, model: &str, usage: &Usage) -> Value {
        serde_json::json!({
            "id": id, "object": "chat.completion.chunk", "created": created, "model": model,
            "choices": [{"index": 0, "delta": {}, "finish_reason": null}],
            "usage": {
                "prompt_tokens": usage.input + usage.cache_read,
                "completion_tokens": usage.output,
                "total_tokens": usage.total(),
            },
        })
    }

    fn text_chunk(id: &str, created: i64, model: &str, delta: Map<String, Value>) -> Value {
        serde_json::json!({
            "id": id, "object": "chat.completion.chunk", "created": created, "model": model,
            "choices": [{"index": 0, "delta": delta, "finish_reason": null}],
        })
    }

    /// 一个上游 `Chunk` 译成 0..N 个下游 SSE 负载（调用方补 `id/created/model`）。
    /// 工具 id/name 只在块内首个增量发一次；decoy 整块抑制。
    pub fn push(&mut self, chunk: &Chunk, id: &str, created: i64, model: &str) -> Vec<Value> {
        let mut out = Vec::new();
        match chunk {
            Chunk::TextDelta(delta) => {
                self.has_any = true;
                let mut d = Map::new();
                d.insert("content".to_string(), Value::String(delta.clone()));
                out.push(Self::text_chunk(id, created, model, d));
            }
            Chunk::ReasonDelta(delta) => {
                self.has_any = true;
                // 非标准字段：下游拿 `reasoning` 读思考增量（zen-gate 同款）。
                let mut d = Map::new();
                d.insert("reasoning".to_string(), Value::String(delta.clone()));
                out.push(Self::text_chunk(id, created, model, d));
            }
            Chunk::BlockStart { kind, id: block_id, name } => {
                if *kind == BlockKind::ToolCall {
                    let suppressed = FINGERPRINT_TOOLS.contains(&name.as_str())
                        && !self.declared.contains(name);
                    self.open_tool = Some(OpenTool {
                        id: block_id.clone(),
                        name: name.clone(),
                        suppressed,
                        sent_head: false,
                    });
                }
            }
            Chunk::BlockEnd => {
                if self.open_tool.take().is_some() {
                    self.tool_index += 1;
                }
            }
            Chunk::ToolCallDelta { delta, .. } => {
                let tool = match self.open_tool.as_mut() {
                    Some(t) => t,
                    None => return out,
                };
                if tool.suppressed {
                    return out;
                }
                self.has_any = true;
                let mut function = Map::new();
                let mut call = Map::new();
                call.insert("index".to_string(), Value::Number(self.tool_index.into()));
                if !tool.sent_head {
                    function.insert("name".to_string(), Value::String(tool.name.clone()));
                    call.insert("id".to_string(), Value::String(tool.id.clone()));
                    tool.sent_head = true;
                }
                function.insert("arguments".to_string(), Value::String(delta.clone()));
                call.insert("function".to_string(), Value::Object(function));
                let mut d = Map::new();
                d.insert(
                    "tool_calls".to_string(),
                    Value::Array(vec![Value::Object(call)]),
                );
                out.push(Self::text_chunk(id, created, model, d));
            }
        }
        out
    }

    /// 非流式攒包：文本 / 思考 / 工具调用（完整 arguments，decoy 不入终态）。
    pub fn accumulate(
        &mut self,
        chunk: &Chunk,
        text: &mut String,
        reasoning: &mut String,
        tool_calls: &mut Vec<Value>,
        current_args: &mut HashMap<usize, (String, String, String)>,
    ) {
        match chunk {
            Chunk::TextDelta(delta) => {
                self.has_any = true;
                text.push_str(delta);
            }
            Chunk::ReasonDelta(delta) => {
                self.has_any = true;
                reasoning.push_str(delta);
            }
            Chunk::BlockStart { kind, id, name } => {
                if *kind == BlockKind::ToolCall {
                    let suppressed = FINGERPRINT_TOOLS.contains(&name.as_str())
                        && !self.declared.contains(name);
                    self.open_tool = Some(OpenTool {
                        id: id.clone(),
                        name: name.clone(),
                        suppressed,
                        sent_head: true,
                    });
                    if !suppressed {
                        current_args.insert(self.tool_index, (id.clone(), name.clone(), String::new()));
                    }
                }
            }
            Chunk::BlockEnd => {
                if let Some(tool) = self.open_tool.take() {
                    // 落块：decoy 不入终态（`accumulate` 建块时已跳过攒包）。
                    if !tool.suppressed {
                        if let Some((id, name, args)) = current_args.remove(&self.tool_index) {
                            let mut f = Map::new();
                            f.insert("name".to_string(), Value::String(name));
                            f.insert("arguments".to_string(), Value::String(args));
                            let mut o = Map::new();
                            o.insert("id".to_string(), Value::String(id));
                            o.insert("type".to_string(), Value::String("function".to_string()));
                            o.insert("function".to_string(), Value::Object(f));
                            tool_calls.push(Value::Object(o));
                        }
                    }
                    self.tool_index += 1;
                }
            }
            Chunk::ToolCallDelta { delta, .. } => {
                self.has_any = true;
                if let Some(entry) = current_args.get_mut(&self.tool_index) {
                    entry.2.push_str(delta);
                }
            }
        }
    }
}

/// responses / Anthropic 共用缓冲块（先攒完再重放，zen-gate 同款）。
#[derive(Debug, Clone)]
pub struct CollectBlock {
    pub kind: BlockKind,
    pub id: String,
    pub name: String,
    pub text: String,
}

/// 缓冲收集器：BlockStart 建块入序，三类增量统一拼 `text`。
#[derive(Debug, Default)]
pub struct BlockCollector {
    pub blocks: Vec<CollectBlock>,
    open_count: usize,
}

impl BlockCollector {
    /// 喂一帧：BlockStart 建块入序（id/name 自带），三类增量统一拼到
    /// 最近未关的块，BlockEnd 关最近的。上游三线都是顺序流，
    /// 块不会交错，这套语义和按 key 复用等价。
    pub fn feed(&mut self, chunk: &Chunk) {
        match chunk {
            Chunk::BlockStart { kind, id, name } => {
                self.blocks.push(CollectBlock {
                    kind: *kind,
                    id: id.clone(),
                    name: name.clone(),
                    text: String::new(),
                });
                self.open_count += 1;
            }
            Chunk::TextDelta(delta) | Chunk::ReasonDelta(delta) => {
                if self.open_count > 0 {
                    if let Some(last) = self.blocks.last_mut() {
                        last.text.push_str(delta);
                    }
                }
            }
            Chunk::ToolCallDelta { delta, .. } => {
                if self.open_count > 0 {
                    if let Some(last) = self.blocks.last_mut() {
                        last.text.push_str(delta);
                    }
                }
            }
            Chunk::BlockEnd => {
                self.open_count = self.open_count.saturating_sub(1);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    // 注意：这里不要 `use super::*`——它会把 gpui 自带的 `#[test]` 宏引进
    // 来，盖住 Rust 内置的 test 属性导致递归展开失败。只显式引入用到的项。
    use super::FINGERPRINT_TOOLS;
    use super::Wire;
    use super::apply_fingerprint;
    use super::base_model_id;
    use super::classify_failure;
    use super::endpoint_for;
    use super::gateway_headers;
    use super::is_muse_spark;
    use super::map_finish_chat;
    use super::mint_request_id;
    use super::mint_session_id;
    use super::request_id_for;
    use super::restore_tool_name;
    use super::session_for_conversation;
    use super::wire_for;
    use super::BlockCollector;
    use super::BlockKind;
    use super::ChatEmitter;
    use super::Chunk;
    use super::Decoder;
    use super::strip_dsml;
    use std::collections::HashMap;

    #[test]
    fn session_and_request_ids_match_gateway_shape() {
        // `ses_` + 12 hex + 14 base62，`msg_` 同形（上游正则照抄）。
        for id in [mint_session_id(1700000000000), session_for_conversation("abc")] {
            assert!(id.starts_with("ses_"), "会话前缀不对: {id}");
            assert_eq!(id.len(), 30, "会话长度不对: {id}");
            assert!(id[4..16].chars().all(|c| c.is_ascii_hexdigit()), "时间段非 hex: {id}");
            assert!(id[16..].chars().all(|c| c.is_ascii_alphanumeric()), "随机段非法: {id}");
        }
        for id in [mint_request_id(1700000000000), request_id_for("ses_x", "")] {
            assert!(id.starts_with("msg_"), "请求前缀不对: {id}");
            assert_eq!(id.len(), 30, "请求长度不对: {id}");
        }
        // 同一种子稳定映射（额度按会话算，不能每轮换）；过了的就是它。
        assert_eq!(
            session_for_conversation("abc"),
            session_for_conversation("abc"),
            "同一种子必须稳定"
        );
        assert_eq!(
            session_for_conversation("ses_1234567890abcdefg123456789"),
            "ses_1234567890abcdefg123456789",
            "过了格式的原样回"
        );
        // 同一毫秒连铸序号递增，不重样。
        assert_ne!(mint_session_id(1700000000000), mint_session_id(1700000000000));
    }

    #[test]
    fn wire_routing_matches_zen_gate() {
        assert_eq!(wire_for("mimo-v2.6-flash-free"), Wire::Chat);
        assert_eq!(wire_for("deepseek-v4-flash-free"), Wire::Chat);
        assert_eq!(wire_for("space-bunny-free"), Wire::Chat);
        assert_eq!(wire_for("muse-spark-1.3-contributor-free"), Wire::Responses);
        assert_eq!(wire_for("Muse_Spark-X"), Wire::Responses);
        assert_eq!(wire_for("union-alpha"), Wire::Messages);
        assert_eq!(wire_for("jev-1.13-free"), Wire::SystemOne);
        assert_eq!(endpoint_for("mimo-v2.6-flash-free"), "/zen/v1/chat/completions");
        assert_eq!(endpoint_for("muse-spark-1.3-contributor-free"), "/zen/v1/responses");
        assert_eq!(endpoint_for("union-alpha"), "/zen/v1/messages");
        assert_eq!(base_model_id("mimo-v2.6-flash-free(deep)"), "mimo-v2.6-flash-free");
        assert!(is_muse_spark("muse-spark-1.3-contributor-free"));
        assert!(!is_muse_spark("mimo-v2.6-flash-free"));
    }

    #[test]
    fn gateway_headers_carry_the_free_pool_credential() {
        // 免费池没有用户密钥：`Bearer public` + 桌面指纹头，一个不能少。
        let headers = gateway_headers("ses_x", "msg_y", true, "");
        let get = |k: &str| headers.iter().find(|(hk, _)| hk == k).map(|(_, v)| v.clone());
        assert_eq!(get("authorization").as_deref(), Some("Bearer public"));
        assert_eq!(get("user-agent").as_deref(), Some(super::CLIENT_UA));
        assert_eq!(get("x-opencode-client").as_deref(), Some("desktop"));
        assert_eq!(get("x-opencode-session").as_deref(), Some("ses_x"));
        assert_eq!(get("x-opencode-project").as_deref(), Some("global"));
        assert_eq!(get("accept").as_deref(), Some("text/event-stream"));
        let headers = gateway_headers("ses_x", "msg_y", false, "");
        let get = |k: &str| headers.iter().find(|(hk, _)| hk == k).map(|(_, v)| v.clone());
        assert_eq!(get("accept").as_deref(), Some("*/*"));
    }

    #[test]
    fn fingerprint_completes_the_tool_quartet() {
        // 空工具：四个 decoy + chat 默认 tool_choice none。
        let mut body = serde_json::Map::new();
        let rename = apply_fingerprint(&mut body, "chat");
        assert!(rename.is_empty(), "空工具没有改名");
        let tools = body["tools"].as_array().expect("tools 数组");
        assert_eq!(tools.len(), 4, "四件套必须齐");
        for name in FINGERPRINT_TOOLS {
            assert!(
                tools.iter().any(|t| t
                    .get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(|n| n.as_str())
                    == Some(name)),
                "{name} 缺失"
            );
        }
        assert_eq!(body["tool_choice"].as_str(), Some("none"));
        // `pwsh` 顶 `bash` 的班：改名记录在案，回程能恢复。
        let mut body = serde_json::Map::new();
        body.insert(
            "tools".to_string(),
            serde_json::json!([{"type": "function",
                "function": {"name": "pwsh", "description": "shell",
                    "parameters": {"type": "object"}}}]),
        );
        let rename = apply_fingerprint(&mut body, "chat");
        assert_eq!(rename.get("bash").as_deref(), Some(&"pwsh".to_string()));
        assert_eq!(restore_tool_name("bash", &rename), "pwsh");
        let tools = body["tools"].as_array().expect("tools 数组");
        assert_eq!(tools.len(), 4, "顶班后总数还是四个");
        // responses 写法默认 auto，claude 写法是对象 none。
        let mut body = serde_json::Map::new();
        apply_fingerprint(&mut body, "flat");
        assert_eq!(body["tool_choice"].as_str(), Some("auto"));
        let mut body = serde_json::Map::new();
        apply_fingerprint(&mut body, "claude");
        assert_eq!(body["tool_choice"]["type"].as_str(), Some("none"));
    }

    #[test]
    fn dsml_markup_is_scrubbed() {
        assert_eq!(strip_dsml("hello<｜DSML｜ calls>world"), "helloworld");
        assert_eq!(strip_dsml("plain"), "plain");
        // 跨帧半截：先攒着，flush 才吐。
        let mut decoder = Decoder::new(Wire::Chat, HashMap::new());
        let mut out = Vec::new();
        decoder.decode_data(
            r#"{"choices": [{"delta": {"content": "hi <｜DSML｜"}}]}"#,
            &mut out,
        );
        assert!(out.is_empty(), "半截标记先攒着不下发");
        decoder.decode_data(
            r#"{"choices": [{"delta": {"content": " calls>yo"}}]}"#,
            &mut out,
        );
        let text: String = out
            .iter()
            .filter_map(|c| match c {
                Chunk::TextDelta(t) => Some(t.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "hi yo", "标记清掉、正文保留: {text}");
    }

    #[test]
    fn chat_wire_decodes_and_emits_with_decoy_suppressed() {
        // 上游 chat 帧：文本 + 指纹 decoy 调用 + 真调用。
        let mut decoder = Decoder::new(Wire::Chat, HashMap::new());
        let mut chunks = Vec::new();
        decoder.decode_data(r#"{"choices": [{"delta": {"content": "hi"}}]}"#, &mut chunks);
        decoder.decode_data(
            r#"{"choices": [{"delta": {"tool_calls": [{"index": 0, "id": "x1",
                "function": {"name": "bash", "arguments": "{}"}}]}}]}"#,
            &mut chunks,
        );
        decoder.decode_data(
            r#"{"choices": [{"delta": {"tool_calls": [{"index": 1, "id": "x2",
                "function": {"name": "mytool", "arguments": "{\"a\":1}"}}]}}]}"#,
            &mut chunks,
        );
        decoder.decode_data(
            r#"{"choices": [{"finish_reason": "tool_calls"}]}"#,
            &mut chunks,
        );
        let outcome = decoder.finish(&mut Vec::new());
        assert_eq!(outcome.finish, "tool-calls");

        // 下游只声明了 mytool：bash 整块吞掉，tool_calls 重编号。
        let mut emit = ChatEmitter::new(&["mytool".to_string()]);
        let mut frames = vec![ChatEmitter::first_chunk("c1", 1, "m")];
        for chunk in &chunks {
            frames.extend(emit.push(chunk, "c1", 1, "m"));
        }
        frames.push(ChatEmitter::finish_chunk("c1", 1, "m", &outcome.finish));
        let dumped = serde_json::to_string(&frames).expect("能序列化");
        assert!(dumped.contains("mytool"), "真调用必须下发");
        assert!(!dumped.contains("\"bash\""), "decoy 必须整块抑制");
        assert!(dumped.contains("\"finish_reason\":\"tool_calls\""), "终局映射不对: {dumped}");
        assert_eq!(map_finish_chat(""), "length", "断流按截断");
        assert_eq!(map_finish_chat("max-tokens"), "length");
    }

    #[test]
    fn failure_classification_prefers_payload_over_status() {
        assert_eq!(classify_failure(429, "").1, 429);
        assert_eq!(classify_failure(200, "FreeUsageLimitError").1, 429);
        assert_eq!(classify_failure(200, "not available in your country").1, 403);
        assert_eq!(classify_failure(401, "").1, 401);
        assert_eq!(classify_failure(500, "").1, 502);
        assert_eq!(classify_failure(200, "ok").1, 502);
    }

    #[test]
    fn responses_upstream_wraps_user_in_message() {
        // 上游 responses 线拒过裸块：用户内容必须包进
        // `{role:"user",content:[input_text,…]}`（`input[0]` 事故复盘）。
        use super::Message;
        use super::Part;
        use super::Role;
        use super::to_response_items;
        let messages = vec![Message {
            role: Role::User,
            parts: vec![Part::Text("hi".to_string())],
        }];
        let (instructions, items) = to_response_items(&messages);
        assert!(instructions.is_none());
        let items = items.as_array().expect("数组");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["role"].as_str(), Some("user"));
        let content = items[0]["content"].as_array().expect("content 数组");
        assert_eq!(content[0]["type"].as_str(), Some("input_text"));
        assert_eq!(content[0]["text"].as_str(), Some("hi"));
    }

    #[test]
    fn response_collector_buffers_blocks_in_order() {        // responses/Anthropic 先攒块再重放：顺序和文本拼接不能乱。
        let mut collector = BlockCollector::default();
        collector.feed(&Chunk::BlockStart {
            kind: BlockKind::Text,
            id: String::new(),
            name: String::new(),
        });
        collector.feed(&Chunk::TextDelta("a".to_string()));
        collector.feed(&Chunk::TextDelta("b".to_string()));
        collector.feed(&Chunk::BlockEnd);
        collector.feed(&Chunk::BlockStart {
            kind: BlockKind::ToolCall,
            id: "c1".to_string(),
            name: "t".to_string(),
        });
        collector.feed(&Chunk::ToolCallDelta { key: "k".to_string(), delta: "{}".to_string() });
        collector.feed(&Chunk::BlockEnd);
        assert_eq!(collector.blocks.len(), 2);
        assert_eq!(collector.blocks[0].text, "ab");
        assert_eq!(collector.blocks[1].name, "t");
    }
}
