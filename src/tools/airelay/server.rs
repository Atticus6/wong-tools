//! 本地回环网关服务：最小 HTTP/1.1（标准库 TCP），三条下游路由 + 模型表。
//!
//! 阻塞线程模型（代理工具同款）：`start` 起监听线程（连接一来起线程），
//! `stop` 举旗 + 自连叫醒 accept + 等收尾。上游走 ureq（阻塞读 SSE 行），
//! 建连 15 秒、整流 15 分钟。只绑 127.0.0.1，不鉴权（回环即边界）。

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value};

use super::zen::*;

/// 默认监听端口（和代理的 10809 错开，设置里可改）。
pub const DEFAULT_PORT: u16 = 11459;

/// 目录缓存 TTL：上游目录 10 分钟拉一次，失败吃旧的，再没有吃硬编码兜底。
const CATALOG_TTL_MS: u64 = 10 * 60 * 1000;

/// 模型目录：上游 `GET /zen/v1/models` 拉活的，失败回退硬编码表。
/// `/v1/models` 和界面列表共用一份（线程安全克隆）。
#[derive(Debug, Clone)]
pub struct Catalog {
    inner: Arc<Mutex<CachedCatalog>>,
}

#[derive(Debug)]
struct CachedCatalog {
    ids: Vec<String>,
    fetched_ms: u64,
    live: bool,
}

impl Catalog {
    /// 建空缓存：首次读即拉（调用方在自己线程里调，别在 render 里调）。
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(CachedCatalog {
                ids: FALLBACK_MODELS.iter().map(|s| s.to_string()).collect(),
                fetched_ms: 0,
                live: false,
            })),
        }
    }

    /// 快照（只读不拉，界面 render 里调这个）：`(ids, 是否实时)`。
    pub fn snapshot(&self) -> (Vec<String>, bool) {
        match self.inner.lock() {
            Ok(cache) => (cache.ids.clone(), cache.live),
            Err(_) => (Vec::new(), false),
        }
    }

    /// 拉新：成功换新（记实时），失败留旧的（全空才吃兜底）。
    /// 走出站决策（代理开着就过代理拉）；阻塞 15 秒，调用方扔后台线程。
    pub fn refresh(&self) -> Result<Vec<String>, String> {
        let proxy = egress_proxy()?;
        match fetch_listing(proxy.as_deref()) {
            Ok(ids) if !ids.is_empty() => {
                if let Ok(mut cache) = self.inner.lock() {
                    cache.ids = ids.clone();
                    cache.fetched_ms = now_ms();
                    cache.live = true;
                }
                Ok(ids)
            }
            Ok(_) => Err("上游目录是空的".to_string()),
            Err(e) => {
                let mut cache = self.inner.lock().map_err(|_| "目录锁坏了".to_string())?;
                if cache.ids.is_empty() {
                    cache.ids = FALLBACK_MODELS.iter().map(|s| s.to_string()).collect();
                    cache.live = false;
                }
                Err(e)
            }
        }
    }

    /// 读：过期就拉，拉失败吃旧的。服务线程调（自己线程里等没事）。
    pub fn models(&self) -> Vec<String> {
        let stale = match self.inner.lock() {
            Ok(cache) => now_ms().saturating_sub(cache.fetched_ms) > CATALOG_TTL_MS,
            Err(_) => true,
        };
        if stale {
            let _ = self.refresh();
        }
        self.snapshot().0
    }
}

impl Default for Catalog {
    fn default() -> Self {
        Self::new()
    }
}

/// 单条请求遥测：界面每秒读快照展示（库里读出来，重启不丢）。
#[derive(Debug, Clone)]
pub struct LogEntry {
    /// 请求完成毫秒时间戳（界面转 HH:MM:SS）。
    pub at_ms: u64,
    /// `chat` / `responses` / `messages`。
    pub route: String,
    /// 实际模型（改写后记 `请求→实际`）。
    pub model: String,
    /// 终局：`stop` / `tool_calls` / `length` / `ERR`。
    pub finish: String,
    /// 整轮毫秒。
    pub ms: u64,
    /// 失败原因（成功为空）。
    pub err: String,
    /// 输入 token（不含缓存读，缓存另记）。
    pub input_tokens: i64,
    /// 输出 token。
    pub output_tokens: i64,
    /// 缓存命中 token。
    pub cache_tokens: i64,
}

/// 日志槽：写穿到库（`airelay_requests`，100 条封顶），读也从库读。
/// 无状态，可随便克隆，跨线程随便调；读写失败只记日志不崩。
#[derive(Debug, Clone, Default)]
pub struct LogSink;

impl LogSink {
    pub fn push(&self, entry: LogEntry) {
        if let Err(e) = crate::settings::log_airelay_request(&crate::settings::StoredRequest {
            at_ms: entry.at_ms,
            route: entry.route,
            model: entry.model,
            finish: entry.finish,
            ms: entry.ms,
            err: entry.err,
            input_tokens: entry.input_tokens,
            output_tokens: entry.output_tokens,
            cache_tokens: entry.cache_tokens,
        }) {
            eprintln!("写请求日志失败: {e}");
        }
    }

    /// 快照（界面读，老的在前，重启自动回来）。
    pub fn snapshot(&self) -> Vec<LogEntry> {
        crate::settings::airelay_requests()
            .into_iter()
            .map(|r| LogEntry {
                at_ms: r.at_ms,
                route: r.route,
                model: r.model,
                finish: r.finish,
                ms: r.ms,
                err: r.err,
                input_tokens: r.input_tokens,
                output_tokens: r.output_tokens,
                cache_tokens: r.cache_tokens,
            })
            .collect()
    }
}

/// 强制模型：界面永远有生效模型（没选过默认第一张），这里只做换手。
/// 空串理论上不会出现，出现则跟随客户端兜底（不断请求）。
/// 界面点卡即换（`Arc` 换手），已建连的不受影响，下个请求直接用新的。
#[derive(Debug, Clone, Default)]
pub struct ForcedModel {
    inner: Arc<Mutex<String>>,
}

impl ForcedModel {
    pub fn set(&self, model: &str) {
        if let Ok(mut slot) = self.inner.lock() {
            *slot = model.trim().to_string();
        }
    }

    /// 当前强制模型（空串 = 跟随客户端）。
    pub fn get(&self) -> String {
        self.inner.lock().map(|slot| slot.clone()).unwrap_or_default()
    }
}

/// 运行中的网关：拿着停机旗、监听线程、目录缓存和强制模型，
/// drop 不自动停，必须调 `stop`。
pub struct RelayHandle {
    port: u16,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    catalog: Catalog,
    forced: ForcedModel,
}

impl RelayHandle {
    /// 起服务：端口被占直接报错，不动任何系统设置。
    /// 目录后台预热一次（失败下次请求时再拉，不挡启动）。
    pub fn start(port: u16, log: LogSink) -> Result<Self, String> {
        let listener = TcpListener::bind(format!("127.0.0.1:{port}"))
            .map_err(|e| format!("网关端口 {port} 占不住: {e}"))?;
        let stop = Arc::new(AtomicBool::new(false));
        let stop_flag = stop.clone();
        let catalog = Catalog::new();
        let warm = catalog.clone();
        let _ = std::thread::Builder::new()
            .name("airelay-catalog".to_string())
            .spawn(move || {
                let _ = warm.refresh();
            });
        let conn_catalog = catalog.clone();
        let forced = ForcedModel::default();
        let conn_forced = forced.clone();
        let thread = std::thread::Builder::new()
            .name("airelay-gateway".to_string())
            .spawn(move || accept_loop(listener, stop_flag, log, conn_catalog, conn_forced))
            .map_err(|e| format!("起网关线程失败: {e}"))?;
        Ok(Self { port, stop, thread: Some(thread), catalog, forced })
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// 换强制模型（空串 = 跟随客户端）：点卡即换，下个请求直接用新的。
    pub fn set_forced_model(&self, model: &str) {
        self.forced.set(model);
    }

    /// 目录快照（界面读，不阻塞）。
    pub fn catalog_snapshot(&self) -> (Vec<String>, bool) {
        self.catalog.snapshot()
    }

    /// 目录克隆（后台刷新用，`RelayHandle` 本体不能跨线程）。
    pub fn catalog(&self) -> Catalog {
        self.catalog.clone()
    }

    /// 停机：先举旗，再连一下自己把 accept 叫醒，最后等监听线程收尾。
    /// 已建立的连接不断，让它自己流完。
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

/// 出站决策：隧道没在听 → 直连；听着 → 按代理规则判 `opencode.ai`。
/// DIRECT 照样裸连；REJECT 直接报错（规则说拦，硬闯没有意义）；
/// 落节点走本地混合入口（HTTP CONNECT，入口两种协议都接）。
/// 无延迟数据时 `url-test` 组回第一顺位（确定性，够用）。
fn egress_proxy() -> Result<Option<String>, String> {
    use std::net::TcpStream;
    let addr: std::net::SocketAddr = format!("127.0.0.1:{}", crate::tools::proxy::MIXED_PORT)
        .parse()
        .map_err(|e| format!("混合入口地址非法: {e}"))?;
    // 隧道活着吗：直接连入口，最实在（毫秒级，关着立刻拒连）。
    if TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_err() {
        return Ok(None);
    }
    // 听着：订阅都没有（没拉过）当直连，别把请求憋死。
    let Some(subscription) = crate::settings::proxy_subscription() else {
        return Ok(None);
    };
    let selections = crate::settings::proxy_selections().unwrap_or_default();
    let manual = crate::settings::proxy_manual_rules();
    let (_, route) = crate::tools::proxy::lookup_route(
        &subscription,
        &manual,
        &selections,
        &std::collections::HashMap::new(),
        "opencode.ai",
    );
    match route {
        crate::tools::proxy::Route::Direct => Ok(None),
        crate::tools::proxy::Route::Reject => {
            Err("opencode.ai 被代理规则拦截（REJECT）：改规则或关代理再试".to_string())
        }
        crate::tools::proxy::Route::Node(_) => {
            Ok(Some(format!("http://127.0.0.1:{}", crate::tools::proxy::MIXED_PORT)))
        }
    }
}

/// 出站展示文案：直连 / 代理 / 规则拦截（界面心跳读）。
pub fn egress_label() -> String {
    match egress_proxy() {
        // “直连”
        Ok(None) => crate::i18n::t("app.airelay_egress_direct"),
        // “代理”
        Ok(Some(_)) => crate::i18n::t("app.airelay_egress_proxy"),
        // “规则拦截”
        Err(_) => crate::i18n::t("app.airelay_egress_blocked"),
    }
}

/// 建上游 agent：连 15 秒、整流 15 分钟；走代理则挂 CONNECT。
fn upstream_agent_with(proxy: Option<&str>) -> Result<ureq::Agent, String> {
    let mut builder = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(15))
        .timeout(Duration::from_secs(15 * 60));
    if let Some(url) = proxy {
        let proxy = ureq::Proxy::new(url).map_err(|e| format!("代理地址非法: {e}"))?;
        builder = builder.proxy(proxy);
    }
    Ok(builder.build())
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

fn now_unix() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// 毫秒时间戳转 HH:MM:SS（请求表时间列展示用）。
pub(crate) fn clock(ms: u64) -> String {
    let secs = ms / 1000 % 86400;
    format!("{:02}:{:02}:{:02}", secs / 3600, secs / 60 % 60, secs % 60)
}

fn accept_loop(
    listener: TcpListener,
    stop: Arc<AtomicBool>,
    log: LogSink,
    catalog: Catalog,
    forced: ForcedModel,
) {
    for stream in listener.incoming() {
        if stop.load(Ordering::SeqCst) {
            break;
        }
        let Ok(stream) = stream else { continue };
        let log = log.clone();
        let catalog = catalog.clone();
        let forced = forced.clone();
        let _ = std::thread::Builder::new()
            .name("airelay-conn".to_string())
            .spawn(move || {
                if let Err(e) = serve_conn(stream, &log, &catalog, &forced) {
                    eprintln!("网关连接失败: {e}");
                }
            });
    }
}

/// 最小 HTTP/1.1 解析：请求行 + 头（单行 64KB 封顶）+ Content-Length 体
/// （8MB 封顶）。下游都是 JSON 客户端，不处理 chunked 请求体。
fn read_request(stream: &mut TcpStream) -> Result<(String, String, Vec<u8>), String> {
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .map_err(|e| format!("设超时失败: {e}"))?;
    let mut reader =
        BufReader::new(stream.try_clone().map_err(|e| format!("复制流失败: {e}"))?);
    let mut request_line = String::new();
    reader.read_line(&mut request_line).map_err(|e| format!("读请求行失败: {e}"))?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("/").to_string();
    let mut content_length = 0usize;
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).map_err(|e| format!("读头失败: {e}"))?;
        if line == "\r\n" || line == "\n" || line.is_empty() {
            break;
        }
        if line.len() > 65536 {
            return Err("请求头太大".to_string());
        }
        if let Some((k, v)) = line.split_once(':') {
            if k.trim().eq_ignore_ascii_case("content-length") {
                content_length = v.trim().parse().unwrap_or(0);
            }
        }
    }
    if content_length > 8 * 1024 * 1024 {
        return Err("请求体太大".to_string());
    }
    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        reader.read_exact(&mut body).map_err(|e| format!("读请求体失败: {e}"))?;
    }
    Ok((method, path, body))
}

fn write_json(stream: &mut TcpStream, status: u16, body: &Value) -> std::io::Result<()> {
    let text = serde_json::to_string(body).unwrap_or_default();
    write_response(stream, status, &text, false)
}

fn write_response(stream: &mut TcpStream, status: u16, body: &str, sse: bool) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        500 => "Internal Server Error",
        _ => "Error",
    };
    let head = if sse {
        format!("HTTP/1.1 {status} {reason}\r\ncontent-type: text/event-stream\r\ncache-control: no-cache\r\nconnection: close\r\n\r\n")
    } else {
        format!(
            "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            body.len()
        )
    };
    stream.write_all(head.as_bytes())?;
    stream.write_all(body.as_bytes())?;
    stream.flush()
}

fn sse_frame(stream: &mut TcpStream, payload: &Value) -> std::io::Result<()> {
    let line = format!("data: {}\n\n", serde_json::to_string(payload).unwrap_or_default());
    stream.write_all(line.as_bytes())?;
    stream.flush()
}

fn sse_done(stream: &mut TcpStream) -> std::io::Result<()> {
    stream.write_all(b"data: [DONE]\n\n")?;
    stream.flush()
}

fn rand_hex12() -> String {
    use rand::RngCore;
    let mut b = [0u8; 6];
    rand::thread_rng().fill_bytes(&mut b);
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn error_kind_name(code: u16) -> &'static str {
    match code {
        429 => "rate_limit_error",
        401 | 403 => "authentication_error",
        400 => "invalid_request_error",
        _ => "api_error",
    }
}

/// 上游 SSE 负载里抠 usage（顶层 `usage`，或包在 `message`/`response` 里）。
fn extract_usage(payload: &Value) -> (i64, i64, i64) {
    let node = payload
        .get("usage")
        .or_else(|| payload.get("message").and_then(|m| m.get("usage")))
        .or_else(|| payload.get("response").and_then(|r| r.get("usage")))
        .unwrap_or(payload);
    let input = node
        .get("input_tokens")
        .or_else(|| node.get("prompt_tokens"))
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let output = node
        .get("output_tokens")
        .or_else(|| node.get("completion_tokens"))
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let cache = node
        .get("cache_read_input_tokens")
        .or_else(|| node.get("cached_tokens"))
        .or_else(|| node.get("cache_read"))
        .and_then(Value::as_i64)
        .unwrap_or(0);
    (input, output, cache)
}

fn snippet(s: &str) -> String {
    let mut out: String = s.chars().take(300).collect();
    if s.chars().count() > 300 {
        out.push('…');
    }
    out
}

/// 生成预算：没带按 8192，下限 512，上限 32768（zen-gate 无目录时的量级）。
fn budget_for(requested: i64) -> i64 {
    (if requested > 0 { requested } else { 8192 }).clamp(512, 32768)
}

// 上游调用两段式：先打开（非 2xx 整包转错误），再排空（逐帧解码回调）。

/// 已打开的上游流（头已读，状态已知）。
struct UpstreamStream {
    status: u16,
    reader: Box<dyn BufRead>,
}

/// 早失败：建连失败（调用方按 502）或上游直接给了错误状态（按分类）。
enum EarlyError {
    Transport(String),
    Http(u16, String),
}

fn open_upstream(url: &str, headers: &[(String, String)], body: &str) -> Result<UpstreamStream, EarlyError> {
    // 出站先问代理：隧道开着且规则命中节点就走混合入口，否则直连；
    // REJECT 按传输失败回（下游 502 + 原因，不进重试）。
    let proxy = egress_proxy().map_err(EarlyError::Transport)?;
    let agent = upstream_agent_with(proxy.as_deref()).map_err(EarlyError::Transport)?;
    let mut req = agent.post(url);
    for (k, v) in headers {
        req = req.set(k, v);
    }
    match req.send_string(body) {
        Ok(resp) => {
            let status = resp.status();
            let reader: Box<dyn BufRead> = Box::new(BufReader::new(resp.into_reader()));
            Ok(UpstreamStream { status, reader })
        }
        Err(ureq::Error::Status(status, resp)) => {
            let text = resp.into_string().unwrap_or_default();
            let (_, code, _) = classify_failure(status, &text);
            Err(EarlyError::Http(code, snippet(&text)))
        }
        Err(ureq::Error::Transport(e)) => Err(EarlyError::Transport(format!("上游连不上: {e}"))),
    }
}

/// 排空上游 SSE：`data:` 负载先合 usage（进解码器）、再判 error 帧、
/// 再解码回调；`[DONE]`/空行/注释跳过。返回（终局串，累计 usage）；
/// `failed` 有值说明 error 帧截胡。传输层断了返回 `Err`（调用方按
/// “有无已输出”分两种收尾）。
#[allow(clippy::too_many_arguments)]
fn drain_upstream(
    reader: &mut Box<dyn BufRead>,
    http_status: u16,
    decoder: &mut Decoder,
    failed: &mut Option<(u16, String)>,
    on_chunk: &mut dyn FnMut(&Chunk) -> Result<(), String>,
) -> Result<(String, Usage), String> {
    let mut buf = String::new();
    let mut data_lines: Vec<String> = Vec::new();
    loop {
        buf.clear();
        let n = reader.read_line(&mut buf).map_err(|e| format!("读上游流失败: {e}"))?;
        if n == 0 {
            break;
        }
        let line = buf.trim_end();
        if line.starts_with("data:") {
            data_lines.push(line["data:".len()..].trim().to_string());
        } else if line.trim().is_empty() {
            let payload = data_lines.join("\n");
            data_lines.clear();
            let payload = payload.trim().to_string();
            if payload.is_empty() || payload == "[DONE]" {
                continue;
            }
            if let Ok(v) = serde_json::from_str::<Value>(&payload) {
                if v.get("error").is_some() {
                    let text = serde_json::to_string(&v).unwrap_or_default();
                    let (_, code, _) = classify_failure(http_status, &text);
                    *failed = Some((code, snippet(&text)));
                    break;
                }
                let (i, o, c) = extract_usage(&v);
                decoder.note_usage(i, o, c);
            }
            let mut chunks = Vec::new();
            decoder.decode_data(&payload, &mut chunks);
            for chunk in &chunks {
                on_chunk(chunk)?;
            }
        }
        // 非 data 行（`: 注释` 等）直接跳过。
    }
    let mut tail = Vec::new();
    let outcome = decoder.finish(&mut tail);
    for chunk in &tail {
        on_chunk(chunk)?;
    }
    Ok((outcome.finish, outcome.usage))
}

/// 下游请求公共准备：模型校验 → 统一消息 → 会话/预算 → 上游体。
/// `forced` 非空则改写模型（界面卡片选中），会话种子不受影响；
/// 返回 `(展示名, 模型原文, 去后缀模型, 线, ...)`，展示名在改写时
/// 是 `请求→实际`，日志一眼看出被改写过。
/// 模型非法/消息为空/决策模型走 `Err`（调用方按路由形状回 400）。
#[allow(clippy::too_many_lines)]
#[allow(clippy::type_complexity)]
fn prepare_turn(
    req: &Value,
    messages: Vec<Message>,
    tools: Vec<ToolDef>,
    max_tokens: i64,
    user: &str,
    forced: &str,
) -> Result<
    (
        String,
        String,
        String,
        Wire,
        Vec<Message>,
        Vec<ToolDef>,
        Map<String, Value>,
        std::collections::HashMap<String, String>,
        Vec<(String, String)>,
        String,
    ),
    String,
> {
    let requested = req.get("model").and_then(Value::as_str).unwrap_or("").to_string();
    let forced = forced.trim();
    let effective = if forced.is_empty() { requested.clone() } else { forced.to_string() };
    if effective.trim().is_empty() {
        return Err("missing model".to_string());
    }
    if messages.is_empty() {
        return Err("missing messages".to_string());
    }
    let base = base_model_id(&effective);
    let wire = wire_for(&base);
    if wire == Wire::SystemOne {
        return Err("systemone decision model".to_string());
    }
    let budget = budget_for(max_tokens);
    let session = session_for_conversation(&session_seed("wong-tools", user, &messages));
    let request_id = mint_request_id(0);
    // 发上游前修配对：悬空调用/结果不发，免得上游整轮拒收。
    let messages = repair_tool_pairing(messages);
    let (body, rename) = build_upstream_body(wire, &base, &messages, &tools, budget);
    let headers = gateway_headers(&session, &request_id, true, "");
    let url = format!("{}{}", upstream_base(), endpoint_for(&base));
    let display = if requested.is_empty() || requested == effective {
        effective.clone()
    } else {
        format!("{requested}→{effective}")
    };
    Ok((display, effective, base, wire, messages, tools, body, rename, headers, url))
}

fn log_done(
    log: &LogSink,
    t0: u64,
    route: &str,
    model: String,
    finish: String,
    err: String,
    usage: &Usage,
) {
    log.push(LogEntry {
        at_ms: t0,
        route: route.to_string(),
        model,
        finish,
        ms: now_ms().saturating_sub(t0),
        err,
        input_tokens: usage.input,
        output_tokens: usage.output,
        cache_tokens: usage.cache_read,
    });
}

/// chat 流式头懒写：首帧没出来前都能改状态码（zen-gate 同款）。
/// 独立函数（闭包借 `head_sent` 会和后面的读打架）。
fn send_chat_head(
    stream: &mut TcpStream,
    head_sent: &mut bool,
    id: &str,
    created: i64,
    model: &str,
) -> std::io::Result<()> {
    if *head_sent {
        return Ok(());
    }
    write_response(stream, 200, "", true)?;
    sse_frame(stream, &ChatEmitter::first_chunk(id, created, model))?;
    *head_sent = true;
    Ok(())
}

/// 一轮 chat（流式 / 非流式）。
fn handle_chat(stream: &mut TcpStream, req: &Value, sse: bool, log: &LogSink, t0: u64, forced: &ForcedModel) {
    // 早失败（400/连不上）记 0 token，后面 drain 出结果 shadow 掉。
    let usage = Usage::default();
    let max_tok = req
        .get("max_tokens")
        .and_then(Value::as_i64)
        .or_else(|| req.get("max_completion_tokens").and_then(Value::as_i64))
        .unwrap_or(0);
    let user = req.get("user").and_then(Value::as_str).unwrap_or("");
    let messages = convert_openai_messages(req.get("messages").unwrap_or(&Value::Null));
    let tools = convert_openai_tools(req.get("tools").unwrap_or(&Value::Null));
    let forced = forced.get();
    let prepared = prepare_turn(req, messages, tools, max_tok, user, &forced);
    let (model_raw, _effective, base, wire, _messages, tools, body, rename, headers, url) = match prepared {
        Ok(p) => p,
        Err(e) => {
            let _ = write_json(stream, 400, &openai_error(&e, "invalid_request_error"));
            log_done(log, t0, "chat", String::new(), "ERR".to_string(), e, &usage);
            return;
        }
    };
    let body_text = match serde_json::to_string(&Value::Object(body)) {
        Ok(s) => s,
        Err(e) => {
            let _ = write_json(stream, 500, &openai_error(&e.to_string(), "api_error"));
            return;
        }
    };
    let id = format!("chatcmpl-{}", rand_hex12());
    let created = now_unix();
    let declared: Vec<String> = tools.iter().map(|t| t.name.clone()).collect();
    let include_usage = req
        .get("stream_options")
        .and_then(|o| o.get("include_usage"))
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let mut upstream = match open_upstream(&url, &headers, &body_text) {
        Ok(u) => u,
        Err(EarlyError::Transport(e)) => {
            let _ = write_json(stream, 502, &openai_error(&e, "api_error"));
            log_done(log, t0, "chat", model_raw, "ERR".to_string(), e, &usage);
            return;
        }
        Err(EarlyError::Http(code, msg)) => {
            let _ = write_json(stream, code, &openai_error(&msg, error_kind_name(code)));
            log_done(log, t0, "chat", model_raw, "ERR".to_string(), msg, &usage);
            return;
        }
    };
    let status = upstream.status;
    let mut decoder = Decoder::new(wire, rename);
    let mut failed: Option<(u16, String)> = None;

    if sse {
        // 头懒写：首帧没出来前都还能改状态码（zen-gate 同款）。
        let mut head_sent = false;
        let mut emitter = ChatEmitter::new(&declared);
        let mut on_chunk = |chunk: &Chunk| -> Result<(), String> {
            for frame in emitter.push(chunk, &id, created, &base) {
                send_chat_head(stream, &mut head_sent, &id, created, &base)
                    .map_err(|e| format!("写头失败: {e}"))?;
                sse_frame(stream, &frame).map_err(|e| format!("写帧失败: {e}"))?;
            }
            Ok(())
        };
        let drained = drain_upstream(&mut upstream.reader, status, &mut decoder, &mut failed, &mut on_chunk);
        // error 帧截胡：有输出按中途失败收尾，没有按整包错误回。
        if let Some((code, msg)) = failed.clone() {
            if emitter.has_any() {
                let _ = send_chat_head(stream, &mut head_sent, &id, created, &base);
                let _ = sse_frame(
                    stream,
                    &serde_json::json!({"error": {"message": msg, "type": error_kind_name(code), "code": code}}),
                );
                let _ = sse_done(stream);
                log_done(log, t0, "chat", model_raw, "ERR".to_string(), msg, &usage);
            } else {
                let _ = write_json(stream, code, &openai_error(&msg, error_kind_name(code)));
                log_done(log, t0, "chat", model_raw, "ERR".to_string(), msg, &usage);
            }
            return;
        }
        // 传输层断了：同上两种收尾。
        let (finish, usage) = match drained {
            Err(e) => {
                if emitter.has_any() {
                    let _ = send_chat_head(stream, &mut head_sent, &id, created, &base);
                    let _ = sse_frame(
                        stream,
                        &serde_json::json!({"error": {"message": e, "type": "api_error"}}),
                    );
                    let _ = sse_done(stream);
                    log_done(log, t0, "chat", model_raw, "ERR".to_string(), e, &usage);
                } else {
                    let _ = write_json(stream, 502, &openai_error(&e, "api_error"));
                    log_done(log, t0, "chat", model_raw, "ERR".to_string(), e, &usage);
                }
                return;
            }
            Ok(pair) => pair,
        };
        // 空流也给终局（按截断），不断连。
        let _ = send_chat_head(stream, &mut head_sent, &id, created, &base);
        let _ = sse_frame(stream, &ChatEmitter::finish_chunk(&id, created, &base, &finish));
        if include_usage {
            let _ = sse_frame(stream, &ChatEmitter::usage_chunk(&id, created, &base, &usage));
        }
        let _ = sse_done(stream);
        log_done(log, t0, "chat", model_raw, map_finish_chat(&finish).to_string(), String::new(), &usage);
        return;
    }

    // 非流式：攒完一次组装。
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut tool_calls = Vec::new();
    let mut current_args: HashMap<usize, (String, String, String)> = HashMap::new();
    let mut emitter = ChatEmitter::new(&declared);
    let mut on_chunk = |chunk: &Chunk| -> Result<(), String> {
        emitter.accumulate(chunk, &mut text, &mut reasoning, &mut tool_calls, &mut current_args);
        Ok(())
    };
    let drained = drain_upstream(&mut upstream.reader, status, &mut decoder, &mut failed, &mut on_chunk);
    if let Some((code, msg)) = failed.clone() {
        if emitter.has_any() {
            // 攒包中失败但有内容：按截断返回，内容照给（zen-gate 同款）。
            write_truncated_chat(stream, &id, created, &base, &text, &reasoning, &tool_calls, &decoder.usage());
            log_done(log, t0, "chat", model_raw, "length".to_string(), msg, &usage);
        } else {
            let _ = write_json(stream, code, &openai_error(&msg, error_kind_name(code)));
            log_done(log, t0, "chat", model_raw, "ERR".to_string(), msg, &usage);
        }
        return;
    }
    let (finish, usage) = match drained {
        Err(e) => {
            if emitter.has_any() {
                write_truncated_chat(stream, &id, created, &base, &text, &reasoning, &tool_calls, &decoder.usage());
                log_done(log, t0, "chat", model_raw, "length".to_string(), e, &usage);
            } else {
                let _ = write_json(stream, 502, &openai_error(&e, "api_error"));
                log_done(log, t0, "chat", model_raw, "ERR".to_string(), e, &usage);
            }
            return;
        }
        Ok(pair) => pair,
    };
    let out_finish = map_finish_chat(&finish);
    let mut msg = Map::new();
    msg.insert("role".to_string(), Value::String("assistant".to_string()));
    msg.insert("content".to_string(), Value::String(text));
    if !reasoning.is_empty() {
        msg.insert("reasoning_content".to_string(), Value::String(reasoning));
    }
    if !tool_calls.is_empty() {
        msg.insert("tool_calls".to_string(), Value::Array(tool_calls));
    }
    let body = serde_json::json!({
        "id": id, "object": "chat.completion", "created": created, "model": base,
        "choices": [{"index": 0, "message": msg, "finish_reason": out_finish}],
        "usage": {
            "prompt_tokens": usage.input + usage.cache_read,
            "completion_tokens": usage.output,
            "total_tokens": usage.total(),
        },
    });
    let _ = write_json(stream, 200, &body);
    log_done(log, t0, "chat", model_raw, out_finish.to_string(), String::new(), &usage);
}

/// 非流式截断回包（攒了一半上游断了，内容照给，终局按截断）。
fn write_truncated_chat(
    stream: &mut TcpStream,
    id: &str,
    created: i64,
    model: &str,
    text: &str,
    reasoning: &str,
    tool_calls: &[Value],
    usage: &Usage,
) {
    let mut msg = Map::new();
    msg.insert("role".to_string(), Value::String("assistant".to_string()));
    msg.insert("content".to_string(), Value::String(text.to_string()));
    if !reasoning.is_empty() {
        msg.insert("reasoning_content".to_string(), Value::String(reasoning.to_string()));
    }
    if !tool_calls.is_empty() {
        msg.insert("tool_calls".to_string(), Value::Array(tool_calls.to_vec()));
    }
    let body = serde_json::json!({
        "id": id, "object": "chat.completion", "created": created, "model": model,
        "choices": [{"index": 0, "message": msg, "finish_reason": "length"}],
        "usage": {
            "prompt_tokens": usage.input + usage.cache_read,
            "completion_tokens": usage.output,
            "total_tokens": usage.total(),
        },
    });
    let _ = write_json(stream, 200, &body);
}

/// 一轮 responses（下游流式也要先攒完再重放，zen-gate 同款）。
fn handle_responses(stream: &mut TcpStream, req: &Value, sse: bool, log: &LogSink, t0: u64, forced: &ForcedModel) {
    // 早失败（400/连不上）记 0 token，后面 drain 出结果 shadow 掉。
    let usage = Usage::default();
    let model_raw = req.get("model").and_then(Value::as_str).unwrap_or("").to_string();
    let instructions = req.get("instructions").and_then(Value::as_str).unwrap_or("");
    let messages = convert_responses_input(req.get("input").unwrap_or(&Value::Null), instructions);
    if messages.is_empty() {
        let _ = write_json(stream, 400, &openai_error("missing input", "invalid_request_error"));
            log_done(log, t0, "responses", model_raw, "ERR".to_string(), "missing input".to_string(), &usage);
        return;
    }
    let tools = convert_openai_tools(req.get("tools").unwrap_or(&Value::Null));
    let max_tok = req.get("max_output_tokens").and_then(Value::as_i64).unwrap_or(0);
    // responses 入口 user 恒空（zen-gate 同款，会话种子只看首条文本）。
    let forced = forced.get();
    let prepared = prepare_turn(req, messages, tools, max_tok, "", &forced);
    let (model_raw, _effective, base, wire, _messages, _tools, body, rename, headers, url) = match prepared {
        Ok(p) => p,
        Err(e) => {
            let _ = write_json(stream, 400, &openai_error(&e, "invalid_request_error"));
            log_done(log, t0, "responses", model_raw, "ERR".to_string(), e, &usage);
            return;
        }
    };
    // responses 入口 model 必须在顶层（prepare 读过，这里复用原文）。
    let _ = &model_raw;
    let body_text = match serde_json::to_string(&Value::Object(body)) {
        Ok(s) => s,
        Err(e) => {
            let _ = write_json(stream, 500, &openai_error(&e.to_string(), "api_error"));
            return;
        }
    };
    let mut upstream = match open_upstream(&url, &headers, &body_text) {
        Ok(u) => u,
        Err(EarlyError::Transport(e)) => {
            let _ = write_json(stream, 502, &openai_error(&e, "api_error"));
            log_done(log, t0, "responses", model_raw, "ERR".to_string(), e, &usage);
            return;
        }
        Err(EarlyError::Http(code, msg)) => {
            let _ = write_json(stream, code, &openai_error(&msg, error_kind_name(code)));
            log_done(log, t0, "responses", model_raw, "ERR".to_string(), msg, &usage);
            return;
        }
    };
    let status = upstream.status;
    let mut decoder = Decoder::new(wire, rename);
    let mut failed: Option<(u16, String)> = None;
    let mut collector = BlockCollector::default();
    let mut on_chunk = |chunk: &Chunk| -> Result<(), String> {
        collector.feed(chunk);
        Ok(())
    };
    let drained = drain_upstream(&mut upstream.reader, status, &mut decoder, &mut failed, &mut on_chunk);
    let resp_id = format!("resp_{}", rand_hex12());
    // 中途失败且一块没有：整包错误。
    let has_any = !collector.blocks.is_empty();
    if let Some((code, msg)) = failed.clone() {
        if !has_any {
            let _ = write_json(stream, code, &openai_error(&msg, error_kind_name(code)));
            log_done(log, t0, "responses", model_raw, "ERR".to_string(), msg, &usage);
            return;
        }
    }
    let (finish, usage) = match drained {
        Err(e) => {
            if !has_any {
                let _ = write_json(stream, 502, &openai_error(&e, "api_error"));
                log_done(log, t0, "responses", model_raw, "ERR".to_string(), e, &usage);
                return;
            }
            (String::new(), decoder.usage())
        }
        Ok(pair) => pair,
    };
    let failed_msg = failed.map(|(_, m)| m).unwrap_or_default();
    if sse {
        if write_response(stream, 200, "", true).is_err() {
            return;
        }
        for event in responses_replay(&collector.blocks, &resp_id, &base, &finish, &usage) {
            if sse_frame(stream, &event).is_err() {
                return;
            }
        }
        let _ = sse_done(stream);
    } else {
        let body = responses_final(&collector.blocks, &resp_id, &base, &finish, &usage);
        let _ = write_json(stream, 200, &body);
    }
    let log_finish = if finish.as_str() == "max-tokens" { "length".to_string() } else { "stop".to_string() };
    log_done(log, t0, "responses", model_raw, log_finish, failed_msg, &usage);
}

/// responses 重放事件（created → 每块 added/delta/done → completed/incomplete）。
fn responses_replay(
    blocks: &[CollectBlock],
    resp_id: &str,
    model: &str,
    finish: &str,
    usage: &Usage,
) -> Vec<Value> {
    let usage_json = serde_json::json!({
        "input_tokens": usage.input + usage.cache_read,
        "output_tokens": usage.output,
        "total_tokens": usage.total(),
    });
    let mut events = vec![serde_json::json!({
        "type": "response.created",
        "response": {"id": resp_id, "object": "response", "status": "in_progress", "model": model},
    })];
    for (oidx, block) in blocks.iter().enumerate() {
        let oidx = oidx as i64;
        match block.kind {
            BlockKind::Text => {
                let msg_id = format!("msg_{oidx}");
                events.push(serde_json::json!({
                    "type": "response.output_item.added", "output_index": oidx,
                    "item": {"type": "message", "role": "assistant", "id": msg_id,
                        "status": "in_progress", "content": []},
                }));
                if !block.text.is_empty() {
                    events.push(serde_json::json!({
                        "type": "response.output_text.delta", "output_index": oidx,
                        "item_id": msg_id, "delta": block.text,
                    }));
                }
                events.push(serde_json::json!({
                    "type": "response.output_item.done", "output_index": oidx,
                    "item": {"type": "message", "role": "assistant", "id": msg_id,
                        "status": "completed",
                        "content": [{"type": "output_text", "text": block.text}]},
                }));
            }
            BlockKind::Reasoning => {
                let rs_id = format!("rs_{oidx}");
                events.push(serde_json::json!({
                    "type": "response.output_item.added", "output_index": oidx,
                    "item": {"type": "reasoning", "id": rs_id, "summary": []},
                }));
                if !block.text.is_empty() {
                    events.push(serde_json::json!({
                        "type": "response.reasoning_text.delta", "output_index": oidx,
                        "item_id": rs_id, "delta": block.text,
                    }));
                }
                events.push(serde_json::json!({
                    "type": "response.output_item.done", "output_index": oidx,
                    "item": {"type": "reasoning", "id": rs_id, "summary": []},
                }));
            }
            BlockKind::ToolCall => {
                let fc_id = format!("fc_{oidx}");
                events.push(serde_json::json!({
                    "type": "response.output_item.added", "output_index": oidx,
                    "item": {"type": "function_call", "id": fc_id, "call_id": block.id,
                        "name": block.name, "arguments": "", "status": "in_progress"},
                }));
                if !block.text.is_empty() {
                    events.push(serde_json::json!({
                        "type": "response.function_call_arguments.delta", "output_index": oidx,
                        "item_id": fc_id, "delta": block.text,
                    }));
                }
                events.push(serde_json::json!({
                    "type": "response.output_item.done", "output_index": oidx,
                    "item": {"type": "function_call", "id": fc_id, "call_id": block.id,
                        "name": block.name, "arguments": block.text, "status": "completed"},
                }));
            }
        }
    }
    let output: Vec<Value> = blocks
        .iter()
        .enumerate()
        .map(|(oidx, block)| responses_output_item(block, oidx as i64))
        .collect();
    if finish == "max-tokens" {
        events.push(serde_json::json!({
            "type": "response.incomplete",
            "response": {"id": resp_id, "object": "response", "status": "incomplete",
                "model": model, "output": output, "usage": usage_json},
        }));
    } else {
        events.push(serde_json::json!({
            "type": "response.completed",
            "response": {"id": resp_id, "object": "response", "status": "completed",
                "model": model, "output": output, "usage": usage_json},
        }));
    }
    events
}

fn responses_output_item(block: &CollectBlock, oidx: i64) -> Value {
    match block.kind {
        BlockKind::Text => serde_json::json!({
            "type": "message", "role": "assistant", "id": format!("msg_{oidx}"),
            "content": [{"type": "output_text", "text": block.text}],
        }),
        BlockKind::Reasoning => serde_json::json!({
            "type": "reasoning", "id": format!("rs_{oidx}"), "summary": [],
        }),
        BlockKind::ToolCall => {
            let arguments = if block.text.trim().is_empty() { "{}".to_string() } else { block.text.clone() };
            serde_json::json!({
                "type": "function_call", "id": format!("fc_{oidx}"),
                "call_id": block.id, "name": block.name, "arguments": arguments,
            })
        }
    }
}

/// responses 非流式终包（推理文本丢弃，只留空 summary，zen-gate 同款）。
fn responses_final(
    blocks: &[CollectBlock],
    resp_id: &str,
    model: &str,
    finish: &str,
    usage: &Usage,
) -> Value {
    let output: Vec<Value> = blocks
        .iter()
        .enumerate()
        .map(|(oidx, block)| responses_output_item(block, oidx as i64))
        .collect();
    let status = if finish == "max-tokens" { "incomplete" } else { "completed" };
    serde_json::json!({
        "id": resp_id, "object": "response", "status": status, "model": model,
        "output": output,
        "usage": {
            "input_tokens": usage.input + usage.cache_read,
            "output_tokens": usage.output,
            "total_tokens": usage.total(),
        },
    })
}

fn anthropic_stop_reason(finish: &str) -> &'static str {
    match finish {
        "tool-calls" => "tool_use",
        "max-tokens" | "" => "max_tokens",
        _ => "end_turn",
    }
}

/// 一轮 Anthropic messages（流式逐帧翻译 / 非流式组装）。
fn handle_messages(stream: &mut TcpStream, req: &Value, sse: bool, log: &LogSink, t0: u64, forced: &ForcedModel) {
    // 早失败（400/连不上）记 0 token，后面 drain 出结果 shadow 掉。
    let usage = Usage::default();
    let model_raw = req.get("model").and_then(Value::as_str).unwrap_or("").to_string();
    let messages = convert_anthropic(
        req.get("system").unwrap_or(&Value::Null),
        req.get("messages").unwrap_or(&Value::Null),
    );
    // 空消息直接 400；空模型要看强制改写（prepare 里判，选中卡片时空模型也放行）。
    if messages.is_empty() {
        let _ = write_json(
            stream,
            400,
            &anthropic_error("missing messages", "invalid_request_error"),
        );
            log_done(log, t0, "messages", model_raw, "ERR".to_string(), "missing messages".to_string(), &usage);
        return;
    }
    let tools = convert_anthropic_tools(req.get("tools").unwrap_or(&Value::Null));
    let max_tok = req.get("max_tokens").and_then(Value::as_i64).unwrap_or(0);
    let max_tok = if max_tok > 0 { max_tok } else { 8192 };
    let user = req
        .get("metadata")
        .and_then(|m| m.get("user_id"))
        .and_then(Value::as_str)
        .unwrap_or("");
    // thinking 预算映射 effort（只影响预算下限，v1 预算直通 max_tokens）。
    let _thinking = req.get("thinking");
    let forced = forced.get();
    let prepared = prepare_turn(req, messages, tools, max_tok, user, &forced);
    let (model_raw, _effective, base, wire, _messages, _tools, body, rename, headers, url) = match prepared {
        Ok(p) => p,
        Err(e) => {
            let _ = write_json(stream, 400, &anthropic_error(&e, "invalid_request_error"));
            log_done(log, t0, "messages", model_raw, "ERR".to_string(), e, &usage);
            return;
        }
    };
    let body_text = match serde_json::to_string(&Value::Object(body)) {
        Ok(s) => s,
        Err(e) => {
            let _ = write_json(stream, 500, &anthropic_error(&e.to_string(), "api_error"));
            return;
        }
    };
    let msg_id = format!("msg_{}", rand_hex12());
    let mut upstream = match open_upstream(&url, &headers, &body_text) {
        Ok(u) => u,
        Err(EarlyError::Transport(e)) => {
            let _ = write_json(stream, 502, &anthropic_error(&e, "api_error"));
            log_done(log, t0, "messages", model_raw, "ERR".to_string(), e, &usage);
            return;
        }
        Err(EarlyError::Http(code, msg)) => {
            let kind = if code == 429 { "rate_limit_error" } else { "api_error" };
            let _ = write_json(stream, code, &anthropic_error(&msg, kind));
            log_done(log, t0, "messages", model_raw, "ERR".to_string(), msg, &usage);
            return;
        }
    };
    let status = upstream.status;
    let mut decoder = Decoder::new(wire, rename);
    let mut failed: Option<(u16, String)> = None;

    if sse {
        if write_response(stream, 200, "", true).is_err() {
            return;
        }
        let _ = sse_frame(
            stream,
            &serde_json::json!({
                "type": "message_start",
                "message": {"id": msg_id, "type": "message", "role": "assistant",
                    "model": base, "content": [], "stop_sequence": null, "stop_reason": null,
                    "usage": {"input_tokens": 0, "output_tokens": 0}},
            }),
        );
        let mut emitter = AnthropicEmitter::default();
        let mut saw_any = false;
        let mut on_chunk = |chunk: &Chunk| -> Result<(), String> {
            for frame in emitter.push(chunk) {
                saw_any = true;
                sse_frame(stream, &frame).map_err(|e| format!("写帧失败: {e}"))?;
            }
            Ok(())
        };
        let drained = drain_upstream(&mut upstream.reader, status, &mut decoder, &mut failed, &mut on_chunk);
        if let Some((_, msg)) = failed.clone() {
            if !saw_any {
                let _ = sse_frame(stream, &serde_json::json!({"type": "error", "error": {"type": "api_error", "message": msg}}));
                let _ = sse_done(stream);
                log_done(log, t0, "messages", model_raw, "ERR".to_string(), msg, &usage);
                return;
            }
        }
        let (finish, usage) = match drained {
            Err(e) => {
                if !saw_any {
                    let _ = sse_frame(stream, &serde_json::json!({"type": "error", "error": {"type": "api_error", "message": e}}));
                    let _ = sse_done(stream);
                    log_done(log, t0, "messages", model_raw, "ERR".to_string(), e, &usage);
                    return;
                }
                (String::new(), decoder.usage())
            }
            Ok(pair) => pair,
        };
        let stop = anthropic_stop_reason(&finish);
        let _ = sse_frame(
            stream,
            &serde_json::json!({
                "type": "message_delta",
                "delta": {"stop_reason": stop, "stop_sequence": null},
                "usage": {"output_tokens": usage.output},
            }),
        );
        let _ = sse_done(stream);
        // message_stop 按官方形状补一个（zen-gate 发 stop 后不再补，这里多补无害）。
        let _ = sse_frame(stream, &serde_json::json!({"type": "message_stop"}));
        log_done(log, t0, "messages", model_raw, stop.to_string(), failed.map(|(_, m)| m).unwrap_or_default(), &usage);
        return;
    }

    // 非流式：缓冲收集器攒块再组装。
    let mut collector = BlockCollector::default();
    let mut on_chunk = |chunk: &Chunk| -> Result<(), String> {
        collector.feed(chunk);
        Ok(())
    };
    let drained = drain_upstream(&mut upstream.reader, status, &mut decoder, &mut failed, &mut on_chunk);
    if let Some((code, msg)) = failed.clone() {
        if collector.blocks.is_empty() {
            let kind = if code == 429 { "rate_limit_error" } else { "api_error" };
            let _ = write_json(stream, code, &anthropic_error(&msg, kind));
            log_done(log, t0, "messages", model_raw, "ERR".to_string(), msg, &usage);
            return;
        }
    }
    let (finish, usage) = match drained {
        Err(e) => {
            if collector.blocks.is_empty() {
                let _ = write_json(stream, 502, &anthropic_error(&e, "api_error"));
                log_done(log, t0, "messages", model_raw, "ERR".to_string(), e, &usage);
                return;
            }
            (String::new(), decoder.usage())
        }
        Ok(pair) => pair,
    };
    let stop = anthropic_stop_reason(&finish);
    let mut content = Vec::new();
    for block in &collector.blocks {
        match block.kind {
            BlockKind::Text => content.push(serde_json::json!({"type": "text", "text": block.text})),
            BlockKind::Reasoning => content.push(serde_json::json!({
                "type": "thinking", "thinking": block.text, "signature": "",
            })),
            BlockKind::ToolCall => {
                let input: Value = if block.text.trim().is_empty() {
                    Value::Object(Map::new())
                } else {
                    serde_json::from_str(&block.text).unwrap_or(Value::Object(Map::new()))
                };
                content.push(serde_json::json!({
                    "type": "tool_use", "id": block.id, "name": block.name, "input": input,
                }));
            }
        }
    }
    let body = serde_json::json!({
        "id": msg_id, "type": "message", "role": "assistant", "model": base,
        "content": content, "stop_reason": stop, "stop_sequence": null,
        "usage": {
            "input_tokens": usage.input + usage.cache_read,
            "output_tokens": usage.output,
        },
        "created": now_unix(),
    });
    let _ = write_json(stream, 200, &body);
    log_done(
        log,
        t0,
        "messages",
        model_raw,
        stop.to_string(),
        failed.map(|(_, m)| m).unwrap_or_default(),
        &usage,
    );
}

/// Anthropic 流式发射器：块序号自增，id/name 首包带出。
#[derive(Debug, Default)]
struct AnthropicEmitter {
    index: usize,
    open: bool,
}

impl AnthropicEmitter {
    fn push(&mut self, chunk: &Chunk) -> Vec<Value> {
        let mut out = Vec::new();
        match chunk {
            Chunk::TextDelta(text) => out.push(serde_json::json!({
                "type": "content_block_delta", "index": self.index,
                "delta": {"type": "text_delta", "text": text},
            })),
            Chunk::ReasonDelta(text) => out.push(serde_json::json!({
                "type": "content_block_delta", "index": self.index,
                "delta": {"type": "thinking_delta", "thinking": text},
            })),
            Chunk::BlockStart { kind, id, name } => {
                let block = match kind {
                    BlockKind::Text => serde_json::json!({"type": "text", "text": ""}),
                    BlockKind::Reasoning => {
                        serde_json::json!({"type": "thinking", "thinking": ""})
                    }
                    BlockKind::ToolCall => serde_json::json!({
                        "type": "tool_use", "id": id, "name": name, "input": {},
                    }),
                };
                out.push(serde_json::json!({
                    "type": "content_block_start", "index": self.index,
                    "content_block": block,
                }));
                self.open = true;
            }
            Chunk::BlockEnd => {
                out.push(serde_json::json!({"type": "content_block_stop", "index": self.index}));
                self.index += 1;
                self.open = false;
            }
            Chunk::ToolCallDelta { delta, .. } => {
                out.push(serde_json::json!({
                    "type": "content_block_delta", "index": self.index,
                    "delta": {"type": "input_json_delta", "partial_json": delta},
                }));
            }
        }
        // 文本增量先开块：解码器保证 BlockStart 在前，这里只做防御。
        let _ = self.open;
        out
    }
}

fn models_body(ids: &[String]) -> Value {
    serde_json::json!({
        "object": "list",
        "data": ids.iter().map(|id| {
            serde_json::json!({"id": id, "object": "model", "created": 0, "owned_by": "zen"})
        }).collect::<Vec<_>>(),
    })
}

fn serve_conn(mut stream: TcpStream, log: &LogSink, catalog: &Catalog, forced: &ForcedModel) -> Result<(), String> {
    let t0 = now_ms();
    let (method, path, body) = read_request(&mut stream)?;
    match (method.as_str(), path.as_str()) {
        ("GET", "/") | ("GET", "/v1") => {
            let _ = write_json(&mut stream, 200, &serde_json::json!({"ok": true, "gateway": "wong-tools airelay"}));
            Ok(())
        }
        ("GET", "/v1/models") => {
            // 目录走缓存（过期自动拉，失败吃旧的/兜底，不断粮）；
            // 接不了的线（决策模型）不进表，免得客户端选中直接 400。
            let ids: Vec<String> = catalog
                .models()
                .into_iter()
                .filter(|id| is_servable(id))
                .collect();
            let _ = write_json(&mut stream, 200, &models_body(&ids));
            Ok(())
        }
        ("POST", "/v1/chat/completions") => {
            let req: Value =
                serde_json::from_slice(&body).map_err(|_| "请求体不是 JSON".to_string())?;
            let sse = req.get("stream").and_then(Value::as_bool).unwrap_or(false);
            handle_chat(&mut stream, &req, sse, log, t0, forced);
            Ok(())
        }
        ("POST", "/v1/responses") => {
            let req: Value =
                serde_json::from_slice(&body).map_err(|_| "请求体不是 JSON".to_string())?;
            let sse = req.get("stream").and_then(Value::as_bool).unwrap_or(false);
            handle_responses(&mut stream, &req, sse, log, t0, forced);
            Ok(())
        }
        ("POST", "/v1/messages") => {
            let req: Value =
                serde_json::from_slice(&body).map_err(|_| "请求体不是 JSON".to_string())?;
            let sse = req.get("stream").and_then(Value::as_bool).unwrap_or(false);
            handle_messages(&mut stream, &req, sse, log, t0, forced);
            Ok(())
        }
        _ => {
            let _ = write_json(&mut stream, 404, &openai_error("not found", "invalid_request_error"));
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    // 注意：这里不要 `use super::*`——它会把 gpui 自带的 `#[test]` 宏引进
    // 来，盖住 Rust 内置的 test 属性导致递归展开失败。只显式引入用到的项。
    use super::budget_for;
    use super::extract_usage;
    use super::models_body;
    use super::prepare_turn;
    use super::LogSink;
    use crate::tools::airelay::zen::FALLBACK_MODELS;
    use crate::tools::airelay::zen::convert_openai_messages;
    use crate::tools::airelay::zen::is_free_lane;
    use crate::tools::airelay::zen::parse_listing;
    use crate::tools::airelay::zen::wire_for;
    use crate::tools::airelay::zen::Wire;

    #[test]
    fn models_table_only_serves_supported_wires() {
        // 兜底表：`jev*` 决策模型只答题不聊天，进表就误导客户端。
        for id in FALLBACK_MODELS {
            assert!(
                matches!(wire_for(id), Wire::Chat | Wire::Responses | Wire::Messages),
                "{id} 的线 v1 接着才会进表"
            );
        }
        let body = models_body(&FALLBACK_MODELS.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert_eq!(body["object"].as_str(), Some("list"));
        assert_eq!(
            body["data"].as_array().map(|a| a.len()),
            Some(FALLBACK_MODELS.len())
        );
    }

    #[test]
    fn turn_preparation_rejects_bad_requests() {
        // 空模型、空消息、决策模型：调用方按 400 回。
        let empty = serde_json::json!({"model": "", "messages": []});
        let messages = convert_openai_messages(empty.get("messages").unwrap_or(&serde_json::Value::Null));
        assert!(prepare_turn(&empty, messages, vec![], 0, "", "").is_err());
        let sysone = serde_json::json!({"model": "jev-1.13-free", "messages": [{"role": "user", "content": "hi"}]});
        let messages = convert_openai_messages(sysone.get("messages").unwrap_or(&serde_json::Value::Null));
        assert!(prepare_turn(&sysone, messages, vec![], 0, "", "").is_err());
    }

    #[test]
    fn forced_model_rewrites_the_request() {
        // 选中卡片后：下游要什么都改写成它，日志展示 `请求→实际`。
        let req = serde_json::json!({"model": "gpt-4", "messages": [{"role": "user", "content": "hi"}]});
        let messages = convert_openai_messages(req.get("messages").unwrap_or(&serde_json::Value::Null));
        let (display, effective, base, wire, ..) =
            prepare_turn(&req, messages, vec![], 0, "", "mimo-v2.6-flash-free")
                .expect("改写后合法");
        assert_eq!(effective, "mimo-v2.6-flash-free");
        assert_eq!(base, "mimo-v2.6-flash-free");
        assert_eq!(wire, Wire::Chat);
        assert_eq!(display, "gpt-4→mimo-v2.6-flash-free");
        // 跟随模式：展示名就是原文。
        let req = serde_json::json!({"model": "union-alpha", "messages": [{"role": "user", "content": "hi"}]});
        let messages = convert_openai_messages(req.get("messages").unwrap_or(&serde_json::Value::Null));
        let (display, _, _, wire, ..) =
            prepare_turn(&req, messages, vec![], 0, "", "").expect("跟随合法");
        assert_eq!(display, "union-alpha");
        assert_eq!(wire, Wire::Messages);
    }

    #[test]
    fn budget_defaults_and_clamps() {
        assert_eq!(budget_for(0), 8192);
        assert_eq!(budget_for(-5), 8192);
        assert_eq!(budget_for(100), 512);
        assert_eq!(budget_for(100000), 32768);
        assert_eq!(budget_for(4000), 4000);
    }

    #[test]
    fn usage_extraction_covers_all_three_wires() {
        let chat = serde_json::json!({"usage": {"prompt_tokens": 10, "completion_tokens": 5}});
        assert_eq!(extract_usage(&chat), (10, 5, 0));
        let msg = serde_json::json!({"type": "message_start",
            "message": {"usage": {"input_tokens": 7, "output_tokens": 3}}});
        // usage 包在 message 里：同样合并（上游 message_start 形状）。
        assert_eq!(extract_usage(&msg), (7, 3, 0));
        let delta = serde_json::json!({"type": "message_delta",
            "usage": {"output_tokens": 4}});
        assert_eq!(extract_usage(&delta), (0, 4, 0));
    }

    #[test]
    fn listing_parses_and_filters_to_free_lane() {
        // 上游目录混着付费 id：只有免费进表，去重保序。
        let payload = serde_json::json!({
            "data": [
                {"id": "mimo-v2.6-flash-free"},
                "union-alpha",
                {"id": "gpt-5-paid"},
                {"id": "mimo-v2.6-flash-free"},
                {"id": ""},
            ],
        });
        assert_eq!(parse_listing(&payload), vec!["mimo-v2.6-flash-free", "union-alpha"]);
        assert!(is_free_lane("space-bunny-free"));
        assert!(is_free_lane("deepseek-v4-flash-free"));
        assert!(!is_free_lane("gpt-5"));
        assert!(!is_free_lane("freebird-paid"));
    }

            #[test]
    fn gateway_serves_models_and_rejects_bad_requests_locally() {
        // 本地回环实测（不碰上游）：起服务 → 模型表有形状 → 非法请求 400 → 关得掉。
        // 端口固定 18080：CI 占用概率极低，被占则起服报错直接失败。
        use super::LogSink;
        use super::RelayHandle;
        let log = LogSink::default();
        let relay = RelayHandle::start(18080, log).expect("本地网关能起");
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(std::time::Duration::from_secs(5))
            .timeout(std::time::Duration::from_secs(20))
            .build();
        // 健康检查。
        let health: serde_json::Value = agent
            .get("http://127.0.0.1:18080/")
            .call()
            .expect("根路径能通")
            .into_json()
            .expect("回包是 JSON");
        assert_eq!(health["ok"], true);
        // 模型表：实时拉不到就吃兜底，反正非空且有形状。
        let models: serde_json::Value = agent
            .get("http://127.0.0.1:18080/v1/models")
            .call()
            .expect("模型表能通")
            .into_json()
            .expect("回包是 JSON");
        assert_eq!(models["object"].as_str(), Some("list"));
        assert!(
            models["data"].as_array().map(|a| !a.is_empty()).unwrap_or(false),
            "模型表不能为空"
        );
        // 空模型 400（OpenAI 形状）。
        let err = agent
            .post("http://127.0.0.1:18080/v1/chat/completions")
            .send_json(serde_json::json!({"model": "", "messages": []}))
            .err()
            .expect("空模型必须失败");
        match err {
            ureq::Error::Status(400, resp) => {
                let body: serde_json::Value = resp.into_json().expect("错误体是 JSON");
                assert_eq!(body["error"]["type"].as_str(), Some("invalid_request_error"));
            }
            other => panic!("要的是 400：{other}"),
        }
        // 决策模型 400（v1 不接单）。
        let err = agent
            .post("http://127.0.0.1:18080/v1/chat/completions")
            .send_json(serde_json::json!({
                "model": "jev-1.13-free",
                "messages": [{"role": "user", "content": "hi"}],
            }))
            .err()
            .expect("决策模型必须失败");
        assert!(matches!(err, ureq::Error::Status(400, _)), "要的是 400：{err}");
        relay.stop();
    }

    #[test]
    fn log_sink_caps_and_snapshots() {
        // 写穿到库：隔离库，120 条进 100 条留，重启读回还在。
        let _guard = crate::i18n::lock_locale();
        let _settings = crate::settings::TempSettings::install("airelay-log");
        let sink = LogSink::default();
        for i in 0..120 {
            sink.push(super::LogEntry {
                at_ms: 1000 + i,
                route: "chat".to_string(),
                model: format!("m{i}"),
                finish: "stop".to_string(),
                ms: 1,
                err: String::new(),
                input_tokens: 10,
                output_tokens: 5,
                cache_tokens: 2,
            });
        }
        let snap = sink.snapshot();
        assert_eq!(snap.len(), 100, "只留最新 100 条");
        assert_eq!(snap[0].model, "m20");
        assert_eq!(snap[0].at_ms, 1020);
        assert_eq!(snap[0].input_tokens, 10, "token 进库");
        assert_eq!(snap[0].output_tokens, 5, "token 进库");
        assert_eq!(snap[0].cache_tokens, 2, "缓存 token 进库");
        // 持久化：新槽读回同一批（重启不丢）。
        let snap2 = LogSink::default().snapshot();
        assert_eq!(snap2.len(), 100, "换槽重读还在");
        assert_eq!(snap2[99].model, "m119");
    }
}
