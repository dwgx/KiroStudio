//! Anthropic API Handler 函数

use std::convert::Infallible;

use anyhow::Error;
use crate::kiro::model::events::Event;
use crate::kiro::model::requests::kiro::KiroRequest;
use crate::kiro::parser::decoder::EventStreamDecoder;
use crate::token;
use axum::{
    Json as JsonExtractor,
    body::Body,
    extract::State,
    http::{StatusCode, header},
    response::{IntoResponse, Json, Response},
};
use bytes::Bytes;
use futures::{Stream, StreamExt, stream};
use serde_json::json;
use std::time::Duration;
use tokio::time::interval;
use uuid::Uuid;

use super::converter::{ConversionError, convert_request};
use super::middleware::AppState;
use super::stream::{BufferedStreamContext, CompletionStatus, SseEvent, StreamContext};
use super::types::{CountTokensRequest, CountTokensResponse, ErrorResponse, MessagesRequest, Model, ModelsResponse, OutputConfig, Thinking};
use super::websearch;

/// 从入站请求头提取客户端 IP（仅头部来源，不含连接层回退）。
///
/// **安全(A1 修复)**：取 `x-forwarded-for` 的**最右**段，不是最左。XFF 是各级代理依次
/// 追加的链 `client, proxy1, proxy2, ...`——最左是**客户端可任意伪造**的值，取最左会让
/// 攻击者发 `X-Forwarded-For: <任意IP>` 来伪造身份、绕过按真实 IP 的封禁/机器码/限流。
/// 本服务部署在可信反代（openresty，`$proxy_add_x_forwarded_for` 追加式）之后：客户端伪造的
/// 前缀会被反代把真实 `$remote_addr` **追加到最右**，故最右那段才是不可伪造的真实客户端 IP。
/// 与安全中间件 [`crate::common::security::client_ip`] 的最右口径一致（消除 A1 的两套语义相反）。
///
/// 优先级：`x-forwarded-for` 最右段 → `x-real-ip` → 都没有则 `None`（直连无反代时头缺失，
/// 由 [`ClientInfo::from_headers_with_peer`] / [`security_block_response`] 回退到 TCP 对端地址）。
fn extract_client_ip(headers: &axum::http::HeaderMap) -> Option<String> {
    // x-forwarded-for: "client, proxy1, proxy2" —— 取**最右**段(反代追加的真实 IP,不可伪造)
    if let Some(xff) = headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()) {
        if let Some(last) = xff.split(',').next_back() {
            let ip = last.trim();
            if !ip.is_empty() {
                return Some(ip.to_string());
            }
        }
    }
    // x-real-ip: 单个 IP
    if let Some(real) = headers.get("x-real-ip").and_then(|v| v.to_str().ok()) {
        let ip = real.trim();
        if !ip.is_empty() {
            return Some(ip.to_string());
        }
    }
    None
}

/// 指纹采集开关的运行时镜像（`config.collect_client_fingerprint`）。
///
/// 热路径 [`ClientInfo::from_headers_with_peer`] 拿不到 config，故用一个进程级
/// AtomicBool 镜像：main 启动时按配置写入，admin 改开关时立即改写，无需重启。
/// 默认 true（与配置默认一致）。
static COLLECT_CLIENT_FINGERPRINT: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(true);

/// 设置指纹采集开关（供 main 启动接线 / admin 更新配置时立即生效调用）。
pub fn set_collect_client_fingerprint(enabled: bool) {
    COLLECT_CLIENT_FINGERPRINT.store(enabled, std::sync::atomic::Ordering::Relaxed);
}

/// IP 黑名单业务层镜像(ArcSwap 热更)。**与 security 中间件的黑名单互补**:
/// 中间件用 TCP 对端 IP(反代后=反代内网 IP,拿不到真实客户端),而对话/记账路径的
/// [`extract_client_ip`] 读 XFF/X-Real-IP 首段=**真实客户端 IP**。故在此业务层再判一次,
/// 命中即拒——这样即便部署在 openresty/nginx 反代后、未开 trust_forwarded,也能按真实 IP 封禁。
/// 启动时由 main 接线、admin 改 ip_blocklist 时热更(无需重启),存已解析的 Cidr 列表。
static IP_BLOCKLIST: std::sync::OnceLock<arc_swap::ArcSwap<Vec<crate::common::security::Cidr>>> =
    std::sync::OnceLock::new();

fn ip_blocklist_cell() -> &'static arc_swap::ArcSwap<Vec<crate::common::security::Cidr>> {
    IP_BLOCKLIST.get_or_init(|| arc_swap::ArcSwap::from_pointee(Vec::new()))
}

/// 设置业务层 IP 黑名单(启动接线 / admin 热更调用)。非法条目跳过。
pub fn set_ip_blocklist(entries: &[String]) {
    let mut cidrs = Vec::new();
    for e in entries {
        match crate::common::security::Cidr::parse(e) {
            Ok(c) => cidrs.push(c),
            Err(err) => tracing::warn!("业务层 IP 黑名单忽略非法条目 '{}': {}", e, err),
        }
    }
    ip_blocklist_cell().store(std::sync::Arc::new(cidrs));
}

/// 判断某客户端 IP 字符串是否命中黑名单(命中=应拒绝)。空黑名单恒 false。
fn ip_is_blocked(ip_str: &str) -> bool {
    let list = ip_blocklist_cell().load();
    if list.is_empty() {
        return false;
    }
    match ip_str.parse::<std::net::IpAddr>() {
        Ok(ip) => list.iter().any(|c| c.contains_ip(ip)),
        Err(_) => false,
    }
}

/// 机器码黑名单业务层镜像(ArcSwap 热更)。机器码 = `MC-` + SHA256(machine_key) 前 12 位,
/// 由运维台「按机器」视图复制。判定时按当前请求真实客户端 IP(同 IP 黑名单口径)重算机器码,
/// 精确匹配(存归一化后的大写小写无关形式)。命中即拒(403,消息 `sbsbsb！`)。
/// 启动时由 main 接线、admin 改 machine_code_blocklist 时热更(无需重启)。
static MACHINE_CODE_BLOCKLIST: std::sync::OnceLock<arc_swap::ArcSwap<Vec<String>>> =
    std::sync::OnceLock::new();

fn machine_code_blocklist_cell() -> &'static arc_swap::ArcSwap<Vec<String>> {
    MACHINE_CODE_BLOCKLIST.get_or_init(|| arc_swap::ArcSwap::from_pointee(Vec::new()))
}

/// 设置业务层机器码黑名单(启动接线 / admin 热更调用)。空串跳过,统一小写去空白存储。
pub fn set_machine_code_blocklist(entries: &[String]) {
    let cleaned: Vec<String> = entries
        .iter()
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .collect();
    machine_code_blocklist_cell().store(std::sync::Arc::new(cleaned));
}

/// 判断给定机器码是否命中黑名单(大小写不敏感精确匹配)。空黑名单恒 false。
fn machine_code_is_blocked(code: &str) -> bool {
    let list = machine_code_blocklist_cell().load();
    if list.is_empty() {
        return false;
    }
    let needle = code.trim().to_ascii_lowercase();
    list.iter().any(|c| *c == needle)
}

/// 安全封禁网关：IP 黑名单 + 机器码黑名单统一判定。命中返回 403 响应，未命中返回 None。
///
/// **F2 修复关键**：封禁判定**独立于 `collect_client_fingerprint` 隐私开关**——直接从请求头
/// 解析真实客户端 IP（[`extract_client_ip`]，回退 TCP 对端），而非复用 `ClientInfo`（后者在
/// 关闭指纹采集时返回全空 IP，会让黑名单静默失效）。安全过滤不该被可观测性开关关掉。
///
/// 机器码按当前请求真实 IP / device 重算判定（与「按机器」视图逐 IP 展示的码口径一致）。
/// device 仅在无 IP 时作兜底键；关指纹时无 UA→device 为 None，机器码回退到 IP/unknown 派生，
/// 与展示端同源。命中即拒。
/// 业务层真实客户端 IP：与安全中间件 [`crate::common::security::client_ip`] 同口径(A1+A2 统一)。
/// - 对端是可信反代(私网/环回)→ 采信 XFF **最右**段(不可伪造)/ X-Real-IP;
/// - 对端是公网(客户端直连)→ 忽略可伪造的 XFF,直接用对端 IP;
/// - 无头无对端 → None。
/// 供封禁判定与「按机器」画像共用同一身份,保证展示 IP == 封禁 IP(不再回到最左伪造/双轨)。
fn trusted_client_ip(
    headers: &axum::http::HeaderMap,
    peer: Option<std::net::SocketAddr>,
) -> Option<String> {
    let peer_is_proxy = peer
        .map(|p| crate::common::security::is_trusted_proxy_peer(p.ip()))
        .unwrap_or(false);
    // 对端是可信反代时才采信转发头(取最右,不可伪造);否则忽略 XFF 用对端。
    if peer_is_proxy {
        if let Some(ip) = extract_client_ip(headers) {
            return Some(ip);
        }
    }
    peer.map(|a| a.ip().to_string())
}

fn security_block_response(
    headers: &axum::http::HeaderMap,
    peer: Option<std::net::SocketAddr>,
) -> Option<axum::response::Response> {
    // 真实客户端 IP：XFF 最右(A1,不可伪造) → 回退 TCP 对端。不受指纹开关影响。
    // A1 修复:extract_client_ip 已改取最右段;仅当对端是可信反代(私网/环回)时才采信 XFF,
    // 公网直连客户端伪造的 XFF 被忽略(用对端 IP),与中间件 client_ip 口径统一。
    let real_ip = trusted_client_ip(headers, peer);

    if let Some(ip) = real_ip.as_deref() {
        if ip_is_blocked(ip) {
            tracing::warn!(client_ip = %ip, "IP 黑名单拦截:拒绝该来源请求(403)");
            return Some(
                (
                    StatusCode::FORBIDDEN,
                    Json(ErrorResponse::new("permission_error", "来源 IP 已被封禁")),
                )
                    .into_response(),
            );
        }
    }

    // 机器码黑名单:按真实 IP 重算(device 仅无 IP 时兜底;关指纹时 device=None 不影响 IP 派生)。
    let code = crate::usage::machine_code_of(real_ip.as_deref(), None);
    if machine_code_is_blocked(&code) {
        tracing::warn!(machine_code = %code, client_ip = ?real_ip, "机器码黑名单拦截:拒绝该机器请求(403)");
        return Some(
            (
                StatusCode::FORBIDDEN,
                Json(ErrorResponse::new("permission_error", "sbsbsb！")),
            )
                .into_response(),
        );
    }
    None
}

fn collect_client_fingerprint() -> bool {
    COLLECT_CLIENT_FINGERPRINT.load(std::sync::atomic::Ordering::Relaxed)
}

/// —— TIER3 配置热重载：AppState 曾固化的热路径开关改用进程级原子镜像 ——
///
/// `AppState` 是 `#[derive(Clone)]`、建路由时按值烘焙，一旦服务栈建成便不可变。
/// 沿用 [`COLLECT_CLIENT_FINGERPRINT`] 已验证的范式，把 admin 可热改的开关搬到
/// 进程级 static 原子镜像：main 启动写入、admin 改配置立即改写、handler 热路径读镜像，
/// 全程无需重启、无锁近零成本。initial 默认与 config 默认一致。
static EXTRACT_THINKING: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// 设置非流式 thinking 提取开关（main 启动接线 / admin 热更调用，立即生效）。
pub fn set_extract_thinking(enabled: bool) {
    EXTRACT_THINKING.store(enabled, std::sync::atomic::Ordering::Relaxed);
}

fn extract_thinking_enabled() -> bool {
    EXTRACT_THINKING.load(std::sync::atomic::Ordering::Relaxed)
}

/// Claude Code 自动切缓冲协议开关（进程级镜像，admin 热更即时生效）。默认 true。
///
/// 开启时：`/v1/messages` 若识别到请求来自 Claude Code，流式响应自动改走 buffered 分发
/// （与 `/cc/v1` 同款），使 message_start 的 input_tokens 用上游 contextUsageEvent 的准确值——
/// CC 会校验该字段。这样 CC 直接打 `/v1` 也能拿到正确行为，无需用户手动改用 `/cc/v1` 端点。
static CC_AUTO_BUFFER: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

/// 设置 CC 自动切缓冲开关（main 启动接线 / admin 热更调用，立即生效）。
pub fn set_cc_auto_buffer(enabled: bool) {
    CC_AUTO_BUFFER.store(enabled, std::sync::atomic::Ordering::Relaxed);
}

fn cc_auto_buffer_enabled() -> bool {
    CC_AUTO_BUFFER.load(std::sync::atomic::Ordering::Relaxed)
}

// ==================== 工具错误缓解开关（TIER3 进程镜像，admin 热更即时生效，默认全关）====================
// 三个开关沿用 EXTRACT_THINKING 同款范式。getter 为 pub(crate) 供 stream.rs 在工具/文本处理热路径读。
// 定性：Invalid tool parameters 病根在模型侧生成参数，网关不能根治只能缓解——这些开关是缓解手段，
// 默认关（保持现状行为），用户在设置页按需开启。

/// ①泄漏控制 token 清洗开关（course/課/count/care 之类粘连）。默认 **true**（保守高信号，正常文本零误删）。
static TOOL_CLEAN_LEAKED_TOKENS: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(true);
/// 设置泄漏 token 清洗开关（main 启动接线 / admin 热更调用，立即生效）。
pub fn set_tool_clean_leaked_tokens(enabled: bool) {
    TOOL_CLEAN_LEAKED_TOKENS.store(enabled, std::sync::atomic::Ordering::Relaxed);
}
pub(crate) fn tool_clean_leaked_tokens_enabled() -> bool {
    TOOL_CLEAN_LEAKED_TOKENS.load(std::sync::atomic::Ordering::Relaxed)
}

/// 文本化 invoke 重组开关(默认 **true**):模型把工具调用吐成 <invoke> 文本时,在四道安全门内
/// (行首 + 非围栏 + 工具名已声明 + 完整闭合)重组为结构化 tool_use。关=退回纯转发(原样吐文本)。
static TOOL_RECLAIM_TEXTIFIED_INVOKE: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(true);
pub fn set_tool_reclaim_textified_invoke(enabled: bool) {
    TOOL_RECLAIM_TEXTIFIED_INVOKE.store(enabled, std::sync::atomic::Ordering::Relaxed);
}
pub(crate) fn tool_reclaim_textified_invoke_enabled() -> bool {
    TOOL_RECLAIM_TEXTIFIED_INVOKE.load(std::sync::atomic::Ordering::Relaxed)
}

/// stray token(call/count/card/court)复读熔断开关(默认 **true**):连续独占行复读超阈值截断本轮文本。
static TOOL_STRAY_REPEAT_GUARD: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(true);
pub fn set_tool_stray_repeat_guard(enabled: bool) {
    TOOL_STRAY_REPEAT_GUARD.store(enabled, std::sync::atomic::Ordering::Relaxed);
}
pub(crate) fn tool_stray_repeat_guard_enabled() -> bool {
    TOOL_STRAY_REPEAT_GUARD.load(std::sync::atomic::Ordering::Relaxed)
}

/// ②流式工具拼装非法时对齐成失败态开关。默认 **true**（与非流式一致，配合③给干净失败信号，不连坐号）。
static TOOL_STREAM_ALIGN_FAILURE: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(true);
/// 设置流式失败态对齐开关（main 启动接线 / admin 热更调用，立即生效）。
pub fn set_tool_stream_align_failure(enabled: bool) {
    TOOL_STREAM_ALIGN_FAILURE.store(enabled, std::sync::atomic::Ordering::Relaxed);
}
pub(crate) fn tool_stream_align_failure_enabled() -> bool {
    TOOL_STREAM_ALIGN_FAILURE.load(std::sync::atomic::Ordering::Relaxed)
}

/// ③工具拼装非法时向客户端补发 SSE error 开关。默认 **true**（与②配对，修复层修不好时不发坏 JSON）。
static TOOL_EXPOSE_ERROR_TO_CLIENT: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(true);
/// 设置工具错误暴露开关（main 启动接线 / admin 热更调用，立即生效）。
pub fn set_tool_expose_error_to_client(enabled: bool) {
    TOOL_EXPOSE_ERROR_TO_CLIENT.store(enabled, std::sync::atomic::Ordering::Relaxed);
}
pub(crate) fn tool_expose_error_to_client_enabled() -> bool {
    TOOL_EXPOSE_ERROR_TO_CLIENT.load(std::sync::atomic::Ordering::Relaxed)
}

/// ④JSON 修复层开关（根治向）。默认 **true**——只在 JSON 已非法时介入 + 修复后强制复验，正常流零影响。
static TOOL_REPAIR_JSON: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);
/// 设置 JSON 修复层开关（main 启动接线 / admin 热更调用，立即生效）。
pub fn set_tool_repair_json(enabled: bool) {
    TOOL_REPAIR_JSON.store(enabled, std::sync::atomic::Ordering::Relaxed);
}
pub(crate) fn tool_repair_json_enabled() -> bool {
    TOOL_REPAIR_JSON.load(std::sync::atomic::Ordering::Relaxed)
}

/// ⑤截断跨轮恢复开关。默认 **false**（改变对话流程：不发坏参数、置失败态让客户端重试整轮）。
///
/// 只在**修复层⑤也补不回**（真截断，缺整段值）且归因为 Truncated/TruncatedAndIllegal 时触发：
/// 不发不完整的 partial_json（避免客户端把半截参数当完整调用执行），改置失败态、收尾补发 SSE error，
/// 让客户端退避后**重试整个请求**（下一轮模型可能生成更小的调用）。绝不 report_failure 连坐号
/// （工具截断≠号坏）。默认关：它改变对话行为（把截断从"发半截"变成"整轮失败重试"），需用户确认。
static TOOL_TRUNCATION_RECOVERY: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
/// 设置截断跨轮恢复开关（main 启动接线 / admin 热更调用，立即生效）。
pub fn set_tool_truncation_recovery(enabled: bool) {
    TOOL_TRUNCATION_RECOVERY.store(enabled, std::sync::atomic::Ordering::Relaxed);
}
pub(crate) fn tool_truncation_recovery_enabled() -> bool {
    TOOL_TRUNCATION_RECOVERY.load(std::sync::atomic::Ordering::Relaxed)
}

/// 从入站请求头识别请求是否来自 Claude Code。
///
/// 两个信号（任一命中即判为 CC）：
/// - `x-anthropic-billing-header`：CC 专属归因头（converter.rs 已处理该前缀），最强信号。
/// - User-Agent 经 `usage::classify_device` 判为 `claude-code` 类（唯一真源，避免此处重复
///   维护 UA 关键字列表导致与设备分类逻辑静默漂移）。
fn is_claude_code_request(headers: &axum::http::HeaderMap) -> bool {
    if headers.contains_key("x-anthropic-billing-header") {
        return true;
    }
    let ua = headers.get(header::USER_AGENT).and_then(|v| v.to_str().ok());
    crate::usage::classify_device(ua).as_deref() == Some("claude-code")
}

/// 输入压缩配置的进程级镜像（TIER3 热更）。
///
/// `CompressionConfig` 非标量（阈值 + 开关），用 `ArcSwap` 承载：admin 改配置时整份原子换、
/// handler 热路径 `load_full()` 拿 `Arc` 快照（无锁近零成本）。`OnceLock` 惰性初始化，
/// main 启动即 `set_compression` 写入真配置；未初始化时回退默认（与 config 默认一致）。
static COMPRESSION: std::sync::OnceLock<arc_swap::ArcSwap<crate::model::config::CompressionConfig>> =
    std::sync::OnceLock::new();

fn compression_cell() -> &'static arc_swap::ArcSwap<crate::model::config::CompressionConfig> {
    COMPRESSION.get_or_init(|| {
        arc_swap::ArcSwap::from_pointee(crate::model::config::CompressionConfig::default())
    })
}

/// 设置输入压缩配置（main 启动接线 / admin 热更调用，立即生效，下个请求即读到新值）。
pub fn set_compression(compression: crate::model::config::CompressionConfig) {
    compression_cell().store(std::sync::Arc::new(compression));
}

fn current_compression() -> std::sync::Arc<crate::model::config::CompressionConfig> {
    compression_cell().load_full()
}

/// 请求来源的客户端画像（设备类型 + IP + 细分 OS + 浏览器），
/// 一并沿用量埋点路径传递，避免多参数散落。
#[derive(Clone, Default)]
struct ClientInfo {
    device: Option<String>,
    ip: Option<String>,
    os: Option<String>,
    browser: Option<String>,
}

impl ClientInfo {
    /// 从入站请求头 + TCP 对端地址一次性解析设备/IP/OS/浏览器。
    ///
    /// IP 取值：[`trusted_client_ip`]（A1+A2 统一口径——可信反代后取 XFF 最右不可伪造，
    /// 公网直连用对端）。与 [`security_block_response`] 封禁判定**同一身份**，保证用量/「按机器」
    /// 视图展示的 IP == 实际封禁的 IP（不再出现展示≠拦截的漂移）。
    ///
    /// 隐私开关：`collect_client_fingerprint` 关闭时直接返回全空画像，
    /// 热路径不解析任何指纹字段，用量记录不落这些信息。
    fn from_headers_with_peer(
        headers: &axum::http::HeaderMap,
        peer: Option<std::net::SocketAddr>,
    ) -> Self {
        if !collect_client_fingerprint() {
            return Self::default();
        }
        let ua = headers.get(header::USER_AGENT).and_then(|v| v.to_str().ok());
        let ip = trusted_client_ip(headers, peer);
        Self {
            device: crate::usage::classify_device(ua),
            ip,
            os: crate::usage::parse_client_os(ua),
            browser: crate::usage::parse_client_browser(ua),
        }
    }

    /// 把画像字段写入一条用量记录
    fn apply(&self, record: &mut crate::usage::RequestRecord) {
        record.client_device = self.device.clone();
        record.client_ip = self.ip.clone();
        record.client_os = self.os.clone();
        record.client_browser = self.browser.clone();
    }
}

/// prompt 缓存记账所需的上下文（跟踪器 + 本次请求的缓存画像）
///
/// 构建发往上游的 Kiro 请求体（含输入压缩）。
///
/// 流程：先序列化测量大小；仅当启用压缩且体积超过 `trigger_bytes` 时，对
/// `ConversationState` 跑压缩管道（空白折叠 + tool_result 智能截断）再重新序列化。
///
/// 保守设计：默认阈值高（4MiB），正常小请求零处理；压缩后仍可能超上游硬限制，
/// 那种情况不再本地判死，交由上游返回 400，再由 [`map_provider_error`] 透传给客户端。
fn build_kiro_request_body(
    conversation_state: crate::kiro::model::requests::conversation::ConversationState,
    compression: &crate::model::config::CompressionConfig,
) -> Result<String, serde_json::Error> {
    let mut kiro_request = KiroRequest {
        conversation_state,
        profile_arn: None,
    };

    let body = serde_json::to_string(&kiro_request)?;

    if compression.enabled && body.len() > compression.trigger_bytes {
        let before = body.len();
        let stats = super::compressor::compress(&mut kiro_request.conversation_state, compression);
        let compressed = serde_json::to_string(&kiro_request)?;
        tracing::info!(
            before_bytes = before,
            after_bytes = compressed.len(),
            saved_bytes = stats.total_saved(),
            trigger_bytes = compression.trigger_bytes,
            "请求体超过压缩阈值，已执行输入压缩"
        );
        return Ok(compressed);
    }

    Ok(body)
}

/// 已翻译的上游错误：HTTP 状态 + Anthropic 错误类型码 + 面向用户的中文消息（含排障步骤）。
struct TranslatedError {
    status: StatusCode,
    error_type: &'static str,
    message: String,
}

/// 把上游错误串翻译成带排障步骤的可读错误。命中已知类别返回 `Some`，未知返回 `None`（调用方透传）。
/// 不处理需额外响应头的情形（429 + Retry-After 在 `map_provider_error` 单独处理）。
fn translate_upstream_error(err_str: &str) -> Option<TranslatedError> {
    translate_quota_subscription(err_str)
        .or_else(|| translate_context_input(err_str))
        .or_else(|| translate_network(err_str))
}

/// 配额/订阅/region 类（不可重试，需用户处理账号）。
fn translate_quota_subscription(err_str: &str) -> Option<TranslatedError> {
    if err_str.contains("MONTHLY_REQUEST_COUNT") || err_str.contains("QUOTA") {
        return Some(TranslatedError {
            status: StatusCode::TOO_MANY_REQUESTS,
            error_type: "rate_limit_error",
            message: "月度请求配额已耗尽。排障：①面板查看各凭据用量，切到仍有额度的账号；②等待配额周期重置；③为号池补充新凭据。".to_string(),
        });
    }
    if err_str.contains("FEATURE_NOT_SUPPORTED") {
        return Some(TranslatedError {
            status: StatusCode::BAD_GATEWAY,
            error_type: "api_error",
            message: "当前凭据所在 region 未开通该功能（profile 未激活）。排障：①网关会在刷新时自动验活重选可用 region；②如持续，右键该凭据切换 Profile ARN 到已开通 region（如 eu-central-1）；③确认该账号确在某 region 开通了 Kiro。".to_string(),
        });
    }
    if err_str.contains("Improperly formed") || err_str.contains("Invalid token") || err_str.contains("subscription") {
        return Some(TranslatedError {
            status: StatusCode::BAD_GATEWAY,
            error_type: "api_error",
            message: "上游拒绝凭据（订阅失效或 token 无效）。排障：①面板对该凭据点『刷新 Token』；②若为 Enterprise/IdC 号，确认 profileArn 已正确解析；③测活确认订阅有效，失效则更换凭据。".to_string(),
        });
    }
    None
}

/// 上下文/输入体积类（不可重试，需减小请求）。
fn translate_context_input(err_str: &str) -> Option<TranslatedError> {
    if err_str.contains("CONTENT_LENGTH_EXCEEDS_THRESHOLD") {
        return Some(TranslatedError {
            status: StatusCode::BAD_REQUEST,
            error_type: "invalid_request_error",
            message: "上下文窗口已满（对话历史累积超出模型上下文上限）。排障：①精简对话历史或开新会话；②缩短 system prompt；③减少同时挂载的工具数量。".to_string(),
        });
    }
    if err_str.contains("Input is too long") {
        return Some(TranslatedError {
            status: StatusCode::BAD_REQUEST,
            error_type: "invalid_request_error",
            message: "单次输入过长（请求体本身超出上游限制）。排障：①拆分过大的消息或附件；②减少一次性粘贴的文件内容；③对超大工具结果先做摘要。".to_string(),
        });
    }
    None
}

/// 是否为**传输层**错误(reqwest 在 `send()`/建连阶段失败,尚未拿到任何 HTTP 响应)。
///
/// 判据:reqwest 传输错误的 Display 有稳定标志(`error sending request` / `error trying to
/// connect` / `tcp connect` / `connection refused|reset|closed` / `dns error`),而**上游 HTTP
/// 错误响应体**(provider 格式化成含 HTTP 状态码 + body 的串)**绝不含这些标志**。以此为闸门,
/// 杜绝「上游正常错误 body 里恰好含 timeout/tls/proxy 字样 → 被误判成网络故障」(review high)。
fn is_transport_error(low: &str) -> bool {
    low.contains("error sending request")
        || low.contains("error trying to connect")
        || low.contains("tcp connect")
        || low.contains("connection refused")
        || low.contains("connection reset")
        || low.contains("connection closed")
        || low.contains("dns error")
        || low.contains("failed to lookup")
        // reqwest 纯超时错误(无 HTTP 响应)的 Display,不与上游 body 里的 "timeout" 混淆:
        // 上游 body 是 JSON,不会是 reqwest 顶层超时串。此项要求整串"像"传输超时(无 HTTP 状态码语境)。
        || (low.contains("operation timed out") && !low.contains("api 请求失败"))
}

/// 网络/传输类（多为可重试的暂时故障，常与代理配置相关）。
///
/// **闸门**:仅当 [`is_transport_error`] 判定为真正的传输层错误才分类,否则返回 None——避免对
/// 上游 HTTP 错误响应体做裸子串匹配导致误判(review high 缺陷)。
fn translate_network(err_str: &str) -> Option<TranslatedError> {
    let low = err_str.to_lowercase();
    // 闸门:不是传输层错误(如上游 4xx/5xx 响应体)一律不在此翻译,交由上层诚实透传。
    if !is_transport_error(&low) {
        return None;
    }
    if low.contains("dns") || low.contains("resolve") || low.contains("name resolution")
        || low.contains("failed to lookup") {
        return Some(TranslatedError {
            status: StatusCode::BAD_GATEWAY,
            error_type: "api_error",
            message: "DNS 解析失败（无法解析上游域名）。排障：①检查本机/容器 DNS 配置；②若走代理，确认代理能解析 kiro.dev；③确认网络出口正常。".to_string(),
        });
    }
    if low.contains("timed out") || low.contains("timeout") {
        return Some(TranslatedError {
            status: StatusCode::GATEWAY_TIMEOUT,
            error_type: "api_error",
            message: "连接上游超时。排障：①上游或代理可能拥塞，稍后重试；②检查代理延迟；③大请求可拆小以缩短单次耗时。".to_string(),
        });
    }
    if low.contains("certificate") || low.contains("ssl") || low.contains("tls") {
        return Some(TranslatedError {
            status: StatusCode::BAD_GATEWAY,
            error_type: "api_error",
            message: "TLS/证书握手失败。排障：①检查系统时间是否准确；②若走中间人代理，确认其证书受信；③确认未误用被拦截的代理。".to_string(),
        });
    }
    if low.contains("proxy") {
        return Some(TranslatedError {
            status: StatusCode::BAD_GATEWAY,
            error_type: "api_error",
            message: "代理连接失败。排障：①检查代理地址/账密是否正确；②确认代理在线可达；③面板核对该凭据绑定的代理配置。".to_string(),
        });
    }
    None
}

/// 将 KiroProvider 错误映射为 HTTP 响应
fn map_provider_error(err: Error) -> Response {
    let err_str = err.to_string();

    // 全池冷却快速失败：token_manager 全池都在冷却时会带 retry_after_secs=N 快速 bail。
    // 这里透传成标准 429 + Retry-After 头，让客户端(Claude Code)按其自身退避策略重试——
    // 比网关内硬扛温和，也减少对被风控号的试探。
    if let Some(secs) = err_str
        .split("retry_after_secs=")
        .nth(1)
        .and_then(|rest| rest.split(|c: char| !c.is_ascii_digit()).next())
        .and_then(|d| d.parse::<u64>().ok())
    {
        let retry_after = secs.clamp(1, 300);
        tracing::warn!(retry_after_secs = retry_after, "全池冷却，返回 429 + Retry-After 让客户端退避");
        return (
            StatusCode::TOO_MANY_REQUESTS,
            [(header::RETRY_AFTER, retry_after.to_string())],
            Json(ErrorResponse::new(
                "rate_limit_error",
                "All credentials are temporarily cooling down. Please retry after the indicated delay.",
            )),
        )
            .into_response();
    }

    // 已确证含义的上游错误：翻译成带排障步骤的可读错误。
    if let Some(t) = translate_upstream_error(&err_str) {
        tracing::warn!(error = %err, error_type = t.error_type, "上游错误已翻译为可读排障提示");
        return (t.status, Json(ErrorResponse::new(t.error_type, t.message))).into_response();
    }

    // 未知错误:**完整原文只进服务端日志**(便于 dwgx 排障),**不回给客户端**——原始错误链可能
    // 含上游响应体里的 profileArn / AWS 账号号 / region / 内部 URL 等敏感信息(review 泄露发现)。
    // 客户端只得通用提示 + 引导查网关日志,不泄露任何上游内部细节。
    tracing::error!("Kiro API 调用失败（未识别，原文仅进日志不回客户端）: {}", err);
    (
        StatusCode::BAD_GATEWAY,
        Json(ErrorResponse::new(
            "api_error",
            "上游 API 调用失败（未识别错误）。请查看网关日志获取详情。",
        )),
    )
        .into_response()
}

/// GET /v1/models
///
/// 返回可用的模型列表
pub async fn get_models() -> impl IntoResponse {
    tracing::info!("Received GET /v1/models request");

    // 从声明式模型目录(单一真相源)派生 /v1/models，消除「广告清单 vs map_model 映射」漂移。
    // 只吐 advertised=true 的模型;thinking 变体作别名不单列。created 为 OpenAI 兼容占位字段。
    // supports_1m 的模型额外广告一条 `<id>[1m]` 变体,供只能传纯模型名的客户端选 1M 上下文。
    const ADVERTISED_CREATED: i64 = 1_759_104_000;
    let mut models: Vec<Model> = Vec::new();
    for s in crate::anthropic::model_catalog::CATALOG.iter().filter(|s| s.advertised) {
        models.push(Model {
            id: s.advertised_id().to_string(),
            object: "model".to_string(),
            created: ADVERTISED_CREATED,
            owned_by: s.owned_by.to_string(),
            display_name: s.display_name.to_string(),
            model_type: "chat".to_string(),
            max_tokens: s.max_output,
        });
        if s.supports_1m {
            models.push(Model {
                id: format!("{}[1m]", s.advertised_id()),
                object: "model".to_string(),
                created: ADVERTISED_CREATED,
                owned_by: s.owned_by.to_string(),
                display_name: format!("{} (1M)", s.display_name),
                model_type: "chat".to_string(),
                max_tokens: s.max_output,
            });
        }
    }

    Json(ModelsResponse {
        object: "list".to_string(),
        data: models,
    })
}

/// POST /v1/messages
///
/// 创建消息（对话）
pub async fn post_messages(
    State(state): State<AppState>,
    axum::extract::ConnectInfo(peer): axum::extract::ConnectInfo<std::net::SocketAddr>,
    headers: axum::http::HeaderMap,
    // 取**裸 body 字节**(而非 JsonExtractor):自定义 API 代挂需要原样透传原始请求体。
    // Kiro 路径行为不变——下面立即从同一份字节解析出 MessagesRequest,与旧 JsonExtractor 等价。
    raw_body: Bytes,
) -> Response {
    // 先按原逻辑解析请求体(解析失败=400,与旧 JsonExtractor 的行为对齐)。
    let mut payload: MessagesRequest = match serde_json::from_slice(&raw_body) {
        Ok(p) => p,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse::new("invalid_request_error", format!("请求体解析失败: {e}"))),
            )
                .into_response();
        }
    };
    tracing::info!(
        model = %payload.model,
        max_tokens = %payload.max_tokens,
        stream = %payload.stream,
        message_count = %payload.messages.len(),
        "Received POST /v1/messages request"
    );

    // 安全封禁网关(IP + 机器码黑名单,独立于指纹开关,按真实客户端 IP 判定):命中即 403。
    if let Some(resp) = security_block_response(&headers, Some(peer)) {
        return resp;
    }

    // 从入站请求头 + TCP 对端地址识别来源画像（设备/IP/OS/浏览器，用于「最近请求」展示）
    let client = ClientInfo::from_headers_with_peer(&headers, Some(peer));
    // 检查 KiroProvider 是否可用
    let provider = match &state.kiro_provider {
        Some(p) => p.clone(),
        None => {
            tracing::error!("KiroProvider 未配置");
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(ErrorResponse::new(
                    "service_unavailable",
                    "Kiro API provider not configured",
                )),
            )
                .into_response();
        }
    };

    // 混入池分流:选一次号,若命中自定义 API 凭据 → 原样透传原始请求体到其上游、直接返回。
    // 选到 Kiro 号(或池中无自定义号)→ 返回 None,继续走下方原 Kiro 路径(行为完全不变)。
    let user_id = payload.metadata.as_ref().and_then(|m| m.user_id.clone());
    if let Some((resp, meta)) = provider
        .try_custom_api_passthrough(raw_body.clone(), Some(&payload.model), user_id.as_deref())
        .await
    {
        // 透传路径也记一条 usage record → 用量统计/最近请求/号池可视化能看到 custom_api。
        // 诚实边界(隔离铁律 3):透传不解析上游 SSE,拿不到真实 output token/credit——
        // input_tokens 用**本地**估算(不走远程 count_tokens API,避免阻塞低延迟中转的 TTFB),
        // output_tokens=0,credits_used=None。
        let input_tokens = token::count_all_tokens_local(
            payload.system.as_deref(),
            &payload.messages,
            payload.tools.as_deref(),
        ) as i32;
        let mut record = crate::usage::RequestRecord::new(
            Uuid::new_v4().to_string(),
            meta.model.clone().unwrap_or_else(|| payload.model.clone()),
        );
        record.credential_id = Some(meta.credential_id);
        record.session_id = meta.session_id.clone();
        record.is_streaming = payload.stream;
        record.input_tokens = input_tokens;
        record.output_tokens = 0;
        record.latency_ms = meta.latency_ms;
        record.outcome = meta.outcome;
        client.apply(&mut record);
        crate::usage::emit_record(record);
        return resp;
    }

    // 检测模型名是否包含 "thinking" 后缀，若包含则覆写 thinking 配置
    override_thinking_from_model_name(&mut payload);

    // 检查是否应本地处理 WebSearch 请求（tool_choice 强制 / 纯 web_search 单工具 / Claude Code 前缀）
    if websearch::should_handle_websearch_request(&payload) {
        tracing::info!("检测到 WebSearch 请求，路由到本地 WebSearch 处理");

        // 估算输入 tokens（只读计数，传引用避免深拷贝整个对话历史）
        let input_tokens = token::count_all_tokens(
            &payload.model,
            payload.system.as_deref(),
            &payload.messages,
            payload.tools.as_deref(),
        ) as i32;

        return websearch::handle_websearch_request(provider, &payload, input_tokens).await;
    }

    // 混合工具场景：请求带 web_search 但未显式触发搜索，剔除 web_search 后走常规转发，
    // 避免把 web_search 原样下发给 Kiro 触发 400 Improperly formed request。
    if websearch::has_web_search_tool(&payload) {
        tracing::info!("检测到混合工具列表中的 web_search，剔除后转发上游");
        websearch::strip_web_search_tools(&mut payload);
    }

    // 转换请求
    let conversion_result = match convert_request(&payload) {
        Ok(result) => result,
        Err(e) => {
            let (error_type, message) = match &e {
                ConversionError::UnsupportedModel(model) => {
                    ("invalid_request_error", format!("模型不支持: {}", model))
                }
                ConversionError::EmptyMessages => {
                    ("invalid_request_error", "消息列表为空".to_string())
                }
            };
            tracing::warn!("请求转换失败: {}", e);
            return (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse::new(error_type, message)),
            )
                .into_response();
        }
    };

    // 构建 Kiro 请求体（发上游前，超阈值时执行输入压缩；profile_arn 由 provider 层注入）
    let request_body = match build_kiro_request_body(
        conversion_result.conversation_state,
        &current_compression(),
    ) {
        Ok(body) => body,
        Err(e) => {
            tracing::error!("序列化请求失败: {}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse::new(
                    "internal_error",
                    format!("序列化请求失败: {}", e),
                )),
            )
                .into_response();
        }
    };

    tracing::debug!("Kiro request body: {}", request_body);

    // 估算输入 tokens（只读计数，传引用避免深拷贝整个对话历史）
    let input_tokens = token::count_all_tokens(
        &payload.model,
        payload.system.as_deref(),
        &payload.messages,
        payload.tools.as_deref(),
    ) as i32;

    // 检查是否启用了thinking
    let thinking_enabled = payload
        .thinking
        .as_ref()
        .map(|t| t.is_enabled())
        .unwrap_or(false);

    let tool_name_map = conversion_result.tool_name_map;
    let known_tool_names = conversion_result.known_tool_names;

    if payload.stream {
        // 流式响应。CC 自动切协议：识别到 Claude Code 且开关开启时，改走 buffered 分发
        // （等价 /cc/v1），让 message_start 的 input_tokens 用上游准确值——CC 会校验它。
        // 这样 CC 直接打 /v1 也能拿到正确行为，无需手动改用 /cc/v1 端点。
        if cc_auto_buffer_enabled() && is_claude_code_request(&headers) {
            tracing::debug!("识别到 Claude Code 请求，/v1 流式自动切换为 buffered 分发（准确 input_tokens）");
            handle_stream_request_buffered(
                provider,
                &request_body,
                &payload.model,
                input_tokens,
                thinking_enabled,
                tool_name_map,
                known_tool_names,
                client,
            )
            .await
        } else {
            handle_stream_request(
                provider,
                &request_body,
                &payload.model,
                input_tokens,
                thinking_enabled,
                tool_name_map,
                known_tool_names,
                client,
            )
            .await
        }
    } else {
        // 非流式响应：仅在配置开启时提取 thinking 块
        let extract_thinking = extract_thinking_enabled() && thinking_enabled;
        handle_non_stream_request(provider, &request_body, &payload.model, input_tokens, extract_thinking, tool_name_map, client).await
    }
}

/// 处理流式请求
async fn handle_stream_request(
    provider: std::sync::Arc<crate::kiro::provider::KiroProvider>,
    request_body: &str,
    model: &str,
    input_tokens: i32,
    thinking_enabled: bool,
    tool_name_map: std::collections::HashMap<String, String>,
    known_tool_names: std::collections::HashSet<String>,
    client: ClientInfo,
) -> Response {
    // 1M 变体:据原始模型名判定是否注入 anthropic-beta 头(仅受支持的 [1m] 变体为 true)。
    let is_1m = crate::anthropic::model_catalog::resolve_is_1m(model);
    // 调用 Kiro API（支持多凭据故障转移）
    let (response, meta) = match provider.call_api_stream(request_body, is_1m).await {
        Ok(resp) => resp,
        Err(e) => return map_provider_error(e),
    };

    // 创建流处理上下文
    let mut ctx = StreamContext::new_full(model, input_tokens, thinking_enabled, tool_name_map, known_tool_names);

    // 生成初始事件
    let initial_events = ctx.generate_initial_events();

    // 创建 SSE 流（流结束时用 meta + 最终 usage 埋点一条成功记录）
    let stream = create_sse_stream(provider, response, ctx, initial_events, meta, client);

    // 返回 SSE 响应
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONNECTION, "keep-alive")
        .body(Body::from_stream(stream))
        .unwrap()
}

/// 流结束时，用 provider 元数据 + StreamContext 最终 usage 埋点一条成功记录
fn emit_stream_usage(
    provider: &crate::kiro::provider::KiroProvider,
    ctx: &StreamContext,
    meta: &crate::kiro::provider::CallMeta,
    client: &ClientInfo,
) {
    let usage = ctx.resolved_usage();
    let mut record = crate::usage::RequestRecord::new(
        Uuid::new_v4().to_string(),
        meta.model.clone().unwrap_or_else(|| ctx.model.clone()),
    );
    record.credential_id = Some(meta.credential_id);
    record.session_id = meta.session_id.clone();
    record.is_streaming = meta.is_streaming;
    record.input_tokens = usage.input_tokens;
    record.output_tokens = usage.output_tokens;
    record.cache_read_tokens = usage.cache_read_tokens;
    record.cache_creation_tokens = usage.cache_creation_tokens;
    record.credits_used = usage.credits_used;
    record.latency_ms = meta.latency_ms;
    record.retries = meta.retries;
    // 去硬编码 Success：按本次响应的真实完成状态记账，避免截断/上游错误被记成成功污染熔断信号。
    record.outcome = ctx.completion_outcome();
    // 生命周期累计花费：把本次真实 credit 消耗累加到该凭据（独立于用量保留期，只增不清）。
    if let Some(c) = record.credits_used {
        provider.report_credits(meta.credential_id, c);
    }
    client.apply(&mut record);
    crate::usage::emit_record(record);
}

/// Ping 事件间隔（25秒）
const PING_INTERVAL_SECS: u64 = 25;

/// 创建 ping 事件的 SSE 字符串
fn create_ping_sse() -> Bytes {
    Bytes::from("event: ping\ndata: {\"type\": \"ping\"}\n\n")
}

/// 创建 SSE 事件流
fn create_sse_stream(
    provider: std::sync::Arc<crate::kiro::provider::KiroProvider>,
    response: reqwest::Response,
    ctx: StreamContext,
    initial_events: Vec<SseEvent>,
    meta: crate::kiro::provider::CallMeta,
    client: ClientInfo,
) -> impl Stream<Item = Result<Bytes, Infallible>> {
    // 先发送初始事件
    let initial_stream = stream::iter(
        initial_events
            .into_iter()
            .map(|e| Ok(Bytes::from(e.to_sse_string()))),
    );

    // 然后处理 Kiro 响应流，同时每25秒发送 ping 保活
    let body_stream = response.bytes_stream();

    let processing_stream = stream::unfold(
        (body_stream, ctx, EventStreamDecoder::new(), false, interval(Duration::from_secs(PING_INTERVAL_SECS)), meta, client, provider),
        |(mut body_stream, mut ctx, mut decoder, finished, mut ping_interval, meta, client, provider)| async move {
            if finished {
                return None;
            }

            // 使用 select! 同时等待数据和 ping 定时器
            tokio::select! {
                // 处理数据流
                chunk_result = body_stream.next() => {
                    match chunk_result {
                        Some(Ok(chunk)) => {
                            // 解码事件
                            if let Err(e) = decoder.feed(&chunk) {
                                tracing::warn!("缓冲区溢出: {}", e);
                            }

                            let mut events = Vec::new();
                            let mut last_decode_err: Option<String> = None;
                            for result in decoder.decode_iter() {
                                match result {
                                    Ok(frame) => {
                                        // from_frame 按值吞 frame，事件类型须在 move 前先拥有化捕获。
                                        let et = frame.event_type().map(|s| s.to_string());
                                        match Event::from_frame(frame) {
                                            Ok(event) => {
                                                // process_kiro_event 内部对 in-band Event::Error/Exception
                                                // 会置 completion 失败态并内联返回 SSE error 事件。
                                                let sse_events = ctx.process_kiro_event(&event);
                                                events.extend(sse_events);
                                            }
                                            Err(err) => {
                                                // 帧层解码成功、Frame→Event 反序列化失败：
                                                // toolUseEvent 失败意味着工具调用不可恢复丢失，置 DecoderStopped
                                                // 失败态（收尾靠 None 分支补发 SSE error），避免截断被当成功不重试；
                                                // 非 tool 帧解析失败历史上就允许被忽略，仅告警不置失败态，防误伤正常流。
                                                if et.as_deref() == Some("toolUseEvent") {
                                                    tracing::warn!("toolUseEvent 帧解析失败,按响应截断处理: {}", err);
                                                    ctx.mark_decoder_stopped(format!("toolUseEvent 帧解析失败: {}", err));
                                                } else {
                                                    tracing::warn!("事件帧解析失败(event_type={:?}),已忽略: {}", et.as_deref(), err);
                                                }
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        last_decode_err = Some(e.to_string());
                                        tracing::warn!("解码事件失败: {}", e);
                                    }
                                }
                            }

                            // 解码器连续错误超限而永久停止：响应必然截断，置失败态供收尾记账，
                            // 并内联补发一个 SSE error 事件（若尚未发过），避免截断被当成功。
                            if decoder.is_stopped() {
                                ctx.mark_decoder_stopped(
                                    last_decode_err.unwrap_or_else(|| "解码器连续错误已停止".to_string()),
                                );
                                if !ctx.error_event_emitted() {
                                    events.push(SseEvent::error_event(
                                        ctx.completion().sse_error_type(),
                                        ctx.completion().client_message(),
                                    ));
                                    ctx.mark_error_event_emitted();
                                }
                            }

                            // 转换为 SSE 字节流
                            let bytes: Vec<Result<Bytes, Infallible>> = events
                                .into_iter()
                                .map(|e| Ok(Bytes::from(e.to_sse_string())))
                                .collect();

                            Some((stream::iter(bytes), (body_stream, ctx, decoder, false, ping_interval, meta, client, provider)))
                        }
                        Some(Err(e)) => {
                            tracing::error!("读取响应流失败: {}", e);
                            // 上游流中途失败：置传输失败态（供收尾按 NetworkError 记账），
                            // 先发一个 SSE error 事件显式告知客户端"本次未正常完成"，再补最终事件收尾。
                            // 否则 Claude Code 会把截断输出当作正常 message_stop=成功，不重试。
                            // 幂等：若 in-band 错误已置过失败态，mark_transport_error 会保留首因。
                            ctx.mark_transport_error(e.to_string());
                            let mut events = Vec::new();
                            if !ctx.error_event_emitted() {
                                events.push(SseEvent::error_event(
                                    ctx.completion().sse_error_type(),
                                    ctx.completion().client_message(),
                                ));
                                ctx.mark_error_event_emitted();
                            }
                            events.extend(ctx.generate_final_events());
                            let bytes: Vec<Result<Bytes, Infallible>> = events
                                .into_iter()
                                .map(|e| Ok(Bytes::from(e.to_sse_string())))
                                .collect();
                            emit_stream_usage(&provider, &ctx, &meta, &client);
                            Some((stream::iter(bytes), (body_stream, ctx, decoder, true, ping_interval, meta, client, provider)))
                        }
                        None => {
                            // 流结束，发送最终事件。
                            // 【缺陷1 时序修复】必须**先** generate_final_events(它内部会 flush 未收到 stop 的
                            // 残留 tool 缓冲,那步才可能把 completion 置失败态),**再**据 completion 补发 error。
                            // 旧序(先查 completion 再 flush)在"无 stop 残留截断"场景漏发 error → 客户端拿
                            // input:{} 的 tool 块 + 正常 message_stop 误判成功(服务端却记失败)。默认②③开也中。
                            // 现在残留 flush 的 ③ 逻辑在置失败态时已返回空(不发坏 JSON),故 final 里无坏 delta,
                            // 把 error 事件**插到最前**(在收尾 message_delta/message_stop 之前)符合 SSE 语义。
                            let tail = ctx.generate_final_events();
                            let mut final_events = Vec::new();
                            if !ctx.completion().is_ok() && !ctx.error_event_emitted() {
                                final_events.push(SseEvent::error_event(
                                    ctx.completion().sse_error_type(),
                                    ctx.completion().client_message(),
                                ));
                                ctx.mark_error_event_emitted();
                            }
                            final_events.extend(tail);
                            let bytes: Vec<Result<Bytes, Infallible>> = final_events
                                .into_iter()
                                .map(|e| Ok(Bytes::from(e.to_sse_string())))
                                .collect();
                            emit_stream_usage(&provider, &ctx, &meta, &client);
                            Some((stream::iter(bytes), (body_stream, ctx, decoder, true, ping_interval, meta, client, provider)))
                        }
                    }
                }
                // 发送 ping 保活
                _ = ping_interval.tick() => {
                    tracing::trace!("发送 ping 保活事件");
                    let bytes: Vec<Result<Bytes, Infallible>> = vec![Ok(create_ping_sse())];
                    Some((stream::iter(bytes), (body_stream, ctx, decoder, false, ping_interval, meta, client, provider)))
                }
            }
        },
    )
    .flatten();

    initial_stream.chain(processing_stream)
}

use super::converter::get_context_window_size;

/// 非流式工具参数 JSON 非法且修复层也修不好时:置 INVALID_TOOL_INPUT 失败态(收尾返回非 200)。
/// 幂等:只在首个失败落定。绝不静默吞成空参(空参会被客户端当"无参成功调用"执行,更危险)。
fn mark_invalid_tool_input(
    completion: &mut CompletionStatus,
    tool_use_id: &str,
    err: &serde_json::Error,
) {
    tracing::warn!(
        "工具输入 JSON 解析失败: {}, tool_use_id: {}（修复层也修不好,返回错误不静默空参）",
        err, tool_use_id
    );
    if completion.is_ok() {
        *completion = CompletionStatus::UpstreamError {
            code: "INVALID_TOOL_INPUT".to_string(),
            message: format!("工具参数 JSON 非法（tool_use_id={}）: {}", tool_use_id, err),
        };
    }
}

/// 处理非流式请求
async fn handle_non_stream_request(
    provider: std::sync::Arc<crate::kiro::provider::KiroProvider>,
    request_body: &str,
    model: &str,
    input_tokens: i32,
    thinking_enabled: bool,
    tool_name_map: std::collections::HashMap<String, String>,
    client: ClientInfo,
) -> Response {
    // 1M 变体:据原始模型名判定是否注入 anthropic-beta 头(仅受支持的 [1m] 变体为 true)。
    let is_1m = crate::anthropic::model_catalog::resolve_is_1m(model);
    // 调用 Kiro API（支持多凭据故障转移）
    let (response, meta) = match provider.call_api(request_body, is_1m).await {
        Ok(resp) => resp,
        Err(e) => return map_provider_error(e),
    };

    // 读取响应体
    let body_bytes = match response.bytes().await {
        Ok(bytes) => bytes,
        Err(e) => {
            tracing::error!("读取响应体失败: {}", e);
            return (
                StatusCode::BAD_GATEWAY,
                Json(ErrorResponse::new(
                    "api_error",
                    format!("读取响应失败: {}", e),
                )),
            )
                .into_response();
        }
    };

    // 解析事件流
    let mut decoder = EventStreamDecoder::new();
    if let Err(e) = decoder.feed(&body_bytes) {
        tracing::warn!("缓冲区溢出: {}", e);
    }

    let mut text_content = String::new();
    let mut tool_uses: Vec<serde_json::Value> = Vec::new();
    let mut has_tool_use = false;
    let mut stop_reason = "end_turn".to_string();
    // 从 contextUsageEvent 计算的实际输入 tokens
    let mut context_input_tokens: Option<i32> = None;
    // 从 meteringEvent 解析的真实 credit 消耗量
    let mut credits_used: Option<f64> = None;
    // 本次响应的完成状态：默认 Ok，遇 in-band 错误/异常/解码器停止置失败态。
    // 收尾据此决定 HTTP 码与用量记账 outcome，避免截断输出被当成 200 成功。
    let mut completion = CompletionStatus::Ok;

    // 收集工具调用的增量 JSON
    let mut tool_json_buffers: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();

    let mut last_decode_err: Option<String> = None;
    for result in decoder.decode_iter() {
        match result {
            Ok(frame) => {
                // from_frame 按值吞 frame，事件类型须在 move 前先拥有化捕获。
                let et = frame.event_type().map(|s| s.to_string());
                match Event::from_frame(frame) {
                    Ok(event) => match event {
                        Event::AssistantResponse(resp) => {
                            text_content.push_str(&resp.content);
                        }
                        Event::ToolUse(tool_use) => {
                            has_tool_use = true;

                            // 累积工具的 JSON 输入（自适应累积快照 vs 纯增量，与流式路径同源修复）：
                            // Kiro 同一 tool_use_id 的 input 可能是"到目前为止的完整 JSON"（累积）
                            // 而非片段。若原样 push_str，累积模式会把 JSON 重复拼接 → 解析失败。
                            let buffer = tool_json_buffers
                                .entry(tool_use.tool_use_id.clone())
                                .or_insert_with(String::new);
                            // 与流式路径同源修复：复用 stream::merge_tool_input 完备决策表
                            // （累积快照 / 纯增量 / 重复终帧 / 迟到旧短快照 / 非前缀重写），
                            // 消灭非前缀双完整对象被 append 成 `}{` 粘连非法 JSON 的漂移。
                            *buffer = super::stream::merge_tool_input(buffer, &tool_use.input);

                            // 如果是完整的工具调用，添加到列表
                            if tool_use.stop {
                                let mut input: serde_json::Value = if buffer.is_empty() {
                                    serde_json::json!({})
                                } else {
                                    match serde_json::from_str(buffer) {
                                        Ok(v) => v,
                                        Err(e) => {
                                            // 与流式路径同源修复(洞3 对齐):非流式此前**从不**调修复层,
                                            // 流式已 repair 的坏 JSON(非法转义/裸控制符/截断)在非流式白瞎。
                                            // 先尝试 repair_tool_json,复验通过则用修复结果、不置失败态。
                                            if super::handlers::tool_repair_json_enabled() {
                                                if let Some(fixed) = super::stream::repair_tool_json(buffer) {
                                                    if let Ok(v) = serde_json::from_str(&fixed) {
                                                        tracing::info!(
                                                            "非流式工具 JSON 已修复为合法(tool_use_id={})",
                                                            tool_use.tool_use_id
                                                        );
                                                        v
                                                    } else {
                                                        // 理论不可达(repair 内部已复验),兜底走失败态。
                                                        mark_invalid_tool_input(
                                                            &mut completion, &tool_use.tool_use_id, &e,
                                                        );
                                                        serde_json::json!({})
                                                    }
                                                } else {
                                                    // 修不好：置失败态，收尾(下方 `if !completion.is_ok()`)
                                                    // 返回非 200，绝不静默吞成空参数——空参会让客户端把失败的
                                                    // 工具调用当成"无参数成功调用"执行，比报错更危险。
                                                    mark_invalid_tool_input(
                                                        &mut completion, &tool_use.tool_use_id, &e,
                                                    );
                                                    serde_json::json!({})
                                                }
                                            } else {
                                                mark_invalid_tool_input(
                                                    &mut completion, &tool_use.tool_use_id, &e,
                                                );
                                                serde_json::json!({})
                                            }
                                        }
                                    }
                                };

                                // 洞1:整包双重编码解包(非流式,与流式 flush_tool_input 同源)。
                                // input 若是被再套一层字符串编码的 object/array(顶层解出 String,
                                // 内层可 parse 成 object/array),解一层还原;只解一层、标量不动。
                                // 【P2-1 解耦】移出 tool_repair_json 开关:解包不改语义、对非 String 顶层
                                // 是 no-op,与流式路径一致独立恒开(关 repair 不应连带关它)。
                                if let Some(inner) = input.as_str() {
                                    if let Ok(reparsed) =
                                        serde_json::from_str::<serde_json::Value>(inner)
                                    {
                                        if reparsed.is_object() || reparsed.is_array() {
                                            tracing::info!(
                                                "非流式工具参数双重编码,已解一层(tool_use_id={})",
                                                tool_use.tool_use_id
                                            );
                                            input = reparsed;
                                        }
                                    }
                                }

                                let original_name = tool_name_map
                                    .get(&tool_use.name)
                                    .cloned()
                                    .unwrap_or_else(|| tool_use.name.clone());

                                tool_uses.push(json!({
                                    "type": "tool_use",
                                    "id": tool_use.tool_use_id,
                                    "name": original_name,
                                    "input": input
                                }));
                            }
                        }
                        Event::ContextUsage(context_usage) => {
                            // 从上下文使用百分比计算实际的 input_tokens
                            let window_size = get_context_window_size(model);
                            let actual_input_tokens = (context_usage.context_usage_percentage
                                * (window_size as f64)
                                / 100.0)
                                as i32;
                            context_input_tokens = Some(actual_input_tokens);
                            // 上下文使用量达到 100% 时，设置 stop_reason 为 model_context_window_exceeded
                            if context_usage.context_usage_percentage >= 100.0 {
                                stop_reason = "model_context_window_exceeded".to_string();
                            }
                            tracing::debug!(
                                "收到 contextUsageEvent: {}%, 计算 input_tokens: {}",
                                context_usage.context_usage_percentage,
                                actual_input_tokens
                            );
                        }
                        Event::Metering(metering) => {
                            credits_used = Some(credits_used.unwrap_or(0.0) + metering.usage);
                        }
                        Event::Exception { exception_type, message } => {
                            // 铁律：ContentLengthExceededException = max_tokens 干净收尾，绝不算失败。
                            if exception_type == "ContentLengthExceededException" {
                                stop_reason = "max_tokens".to_string();
                            } else if completion.is_ok() {
                                // 其它异常是上游真实失败，置失败态（保留首因）。
                                tracing::error!("非流式收到 in-band 异常: {} - {}", exception_type, message);
                                completion = CompletionStatus::UpstreamError {
                                    code: exception_type,
                                    message,
                                };
                            }
                        }
                        Event::Error { error_code, error_message } => {
                            // in-band 错误事件：落入历史的 `_ => {}` 会被静默忽略、照样返回 200，
                            // 这里显式置失败态，收尾时返回非 200 并按真实 outcome 记账。
                            if completion.is_ok() {
                                tracing::error!("非流式收到 in-band 错误: {} - {}", error_code, error_message);
                                completion = CompletionStatus::UpstreamError {
                                    code: error_code,
                                    message: error_message,
                                };
                            }
                        }
                        _ => {}
                    },
                    Err(err) => {
                        // 帧层解码成功、Frame→Event 反序列化失败：
                        // toolUseEvent 失败=工具调用不可恢复丢失，置 DecoderStopped 失败态
                        // （收尾靠下方 `if !completion.is_ok()` 返回 502+记账），避免截断被当成功。
                        // 非 tool 帧解析失败历史上就允许被忽略，仅告警不置失败态，防误伤正常流。
                        if et.as_deref() == Some("toolUseEvent") {
                            tracing::warn!("非流式 toolUseEvent 帧解析失败,按响应截断处理: {}", err);
                            if completion.is_ok() {
                                completion = CompletionStatus::DecoderStopped {
                                    message: format!("toolUseEvent 帧解析失败: {}", err),
                                };
                            }
                        } else {
                            tracing::warn!("非流式事件帧解析失败(event_type={:?}),已忽略: {}", et.as_deref(), err);
                        }
                    }
                }
            }
            Err(e) => {
                last_decode_err = Some(e.to_string());
                tracing::warn!("解码事件失败: {}", e);
            }
        }
    }

    // 解码器永久停止：单 feed 中途连续错误超限，后续帧必然丢失、响应截断。
    if decoder.is_stopped() && completion.is_ok() {
        completion = CompletionStatus::DecoderStopped {
            message: last_decode_err.unwrap_or_else(|| "解码器连续错误已停止".to_string()),
        };
    }

    // 完成状态为失败：直接返回非 200 错误响应 + 埋点真实 outcome，绝不把截断输出当 200 成功。
    // （ContentLengthExceededException 走的是 max_tokens，completion 仍为 Ok，不进此分支。）
    if !completion.is_ok() {
        {
            let mut record = crate::usage::RequestRecord::new(
                Uuid::new_v4().to_string(),
                meta.model.clone().unwrap_or_else(|| model.to_string()),
            );
            record.credential_id = Some(meta.credential_id);
            record.session_id = meta.session_id.clone();
            record.is_streaming = meta.is_streaming;
            record.input_tokens = context_input_tokens.unwrap_or(input_tokens);
            record.credits_used = credits_used;
            record.latency_ms = meta.latency_ms;
            record.retries = meta.retries;
            record.outcome = completion.outcome();
            // 生命周期累计花费：本次真实 credit 消耗累加到该凭据（独立于用量保留期，只增不清）。
            if let Some(c) = record.credits_used {
                provider.report_credits(meta.credential_id, c);
            }
            client.apply(&mut record);
            crate::usage::emit_record(record);
        }
        let status = StatusCode::from_u16(completion.http_status_u16())
            .unwrap_or(StatusCode::BAD_GATEWAY);
        let sse_error_type = completion.sse_error_type();
        return (
            status,
            Json(ErrorResponse::new(sse_error_type, completion.client_message())),
        )
            .into_response();
    }

    // 确定 stop_reason
    if has_tool_use && stop_reason == "end_turn" {
        stop_reason = "tool_use".to_string();
    }

    // 构建响应内容
    let mut content: Vec<serde_json::Value> = Vec::new();

    if thinking_enabled {
        // 从完整文本中提取 thinking 块
        let (thinking, remaining_text) =
            super::stream::extract_thinking_from_complete_text(&text_content);

        if let Some(thinking_text) = thinking {
            // 补 signature 占位符：客户端 thinking 模式下本地校验 thinking 块必须带非空
            // signature，非流式组装时同样需要（回传时 converter 只读 thinking，占位符被
            // serde 静默丢弃，不会转发给 Kiro）。详见 stream::THINKING_SIGNATURE_PLACEHOLDER。
            content.push(json!({
                "type": "thinking",
                "thinking": thinking_text,
                "signature": super::stream::THINKING_SIGNATURE_PLACEHOLDER
            }));
        }

        if !remaining_text.is_empty() {
            content.push(json!({
                "type": "text",
                "text": remaining_text
            }));
        }
    } else if !text_content.is_empty() {
        content.push(json!({
            "type": "text",
            "text": text_content
        }));
    }

    content.extend(tool_uses);

    // 估算输出 tokens
    let output_tokens = token::estimate_output_tokens(&content);

    // 使用从 contextUsageEvent 计算的 input_tokens，如果没有则使用估算值
    let final_input_tokens = context_input_tokens.unwrap_or(input_tokens);

    // 用量埋点：非流式成功记录
    {
        let mut record = crate::usage::RequestRecord::new(
            Uuid::new_v4().to_string(),
            meta.model.clone().unwrap_or_else(|| model.to_string()),
        );
        record.credential_id = Some(meta.credential_id);
        record.session_id = meta.session_id.clone();
        record.is_streaming = meta.is_streaming;
        record.input_tokens = final_input_tokens;
        record.output_tokens = output_tokens;
        record.credits_used = credits_used;
        record.latency_ms = meta.latency_ms;
        record.retries = meta.retries;
        // 去硬编码：此处 completion 必为 Ok（失败已在上方 early-return），显式读取以统一口径。
        record.outcome = completion.outcome();
        // 生命周期累计花费：本次真实 credit 消耗累加到该凭据（独立于用量保留期，只增不清）。
        if let Some(c) = record.credits_used {
            provider.report_credits(meta.credential_id, c);
        }
        client.apply(&mut record);
        crate::usage::emit_record(record);
    }

    // 构建 usage（影子缓存记账已移除，不再注入 cache_read/cache_creation 字段）
    let usage = json!({
        "input_tokens": final_input_tokens,
        "output_tokens": output_tokens
    });

    // 构建 Anthropic 响应
    let response_body = json!({
        "id": format!("msg_{}", Uuid::new_v4().to_string().replace('-', "")),
        "type": "message",
        "role": "assistant",
        "content": content,
        "model": model,
        "stop_reason": stop_reason,
        "stop_sequence": null,
        "usage": usage
    });

    (StatusCode::OK, Json(response_body)).into_response()
}

/// 检测模型名是否包含 "thinking" 后缀，若包含则覆写 thinking 配置
///
/// - Opus 4.6：覆写为 adaptive 类型
/// - 其他模型：覆写为 enabled 类型
/// - budget_tokens 固定为 20000
fn override_thinking_from_model_name(payload: &mut MessagesRequest) {
    let model_lower = payload.model.to_lowercase();
    if !model_lower.contains("thinking") {
        return;
    }

    let is_opus_4_6 =
        model_lower.contains("opus") && (model_lower.contains("4-6") || model_lower.contains("4.6"));

    let thinking_type = if is_opus_4_6 {
        "adaptive"
    } else {
        "enabled"
    };

    tracing::info!(
        model = %payload.model,
        thinking_type = thinking_type,
        "模型名包含 thinking 后缀，覆写 thinking 配置"
    );

    payload.thinking = Some(Thinking {
        thinking_type: thinking_type.to_string(),
        budget_tokens: 20000,
    });
    
    if is_opus_4_6 {
        payload.output_config = Some(OutputConfig {
            effort: "high".to_string(),
        });
    }
}

/// POST /v1/messages/count_tokens
///
/// 计算消息的 token 数量
pub async fn count_tokens(
    JsonExtractor(payload): JsonExtractor<CountTokensRequest>,
) -> impl IntoResponse {
    tracing::info!(
        model = %payload.model,
        message_count = %payload.messages.len(),
        "Received POST /v1/messages/count_tokens request"
    );

    let total_tokens = token::count_all_tokens(
        &payload.model,
        payload.system.as_deref(),
        &payload.messages,
        payload.tools.as_deref(),
    ) as i32;

    Json(CountTokensResponse {
        input_tokens: total_tokens.max(1) as i32,
    })
}

/// POST /cc/v1/messages
///
/// Claude Code 兼容端点，与 /v1/messages 的区别在于：
/// - 流式响应会等待 kiro 端返回 contextUsageEvent 后再发送 message_start
/// - message_start 中的 input_tokens 是从 contextUsageEvent 计算的准确值
pub async fn post_messages_cc(
    State(state): State<AppState>,
    axum::extract::ConnectInfo(peer): axum::extract::ConnectInfo<std::net::SocketAddr>,
    headers: axum::http::HeaderMap,
    JsonExtractor(mut payload): JsonExtractor<MessagesRequest>,
) -> Response {
    tracing::info!(
        model = %payload.model,
        max_tokens = %payload.max_tokens,
        stream = %payload.stream,
        message_count = %payload.messages.len(),
        "Received POST /cc/v1/messages request"
    );

    // 安全封禁网关(IP + 机器码黑名单,独立于指纹开关,按真实客户端 IP 判定,同 /v1/messages)。
    if let Some(resp) = security_block_response(&headers, Some(peer)) {
        return resp;
    }

    // 从入站请求头 + TCP 对端地址识别来源画像（设备/IP/OS/浏览器，用于「最近请求」展示）
    let client = ClientInfo::from_headers_with_peer(&headers, Some(peer));

    // 检查 KiroProvider 是否可用
    let provider = match &state.kiro_provider {
        Some(p) => p.clone(),
        None => {
            tracing::error!("KiroProvider 未配置");
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(ErrorResponse::new(
                    "service_unavailable",
                    "Kiro API provider not configured",
                )),
            )
                .into_response();
        }
    };

    // 检测模型名是否包含 "thinking" 后缀，若包含则覆写 thinking 配置
    override_thinking_from_model_name(&mut payload);

    // 检查是否应本地处理 WebSearch 请求（tool_choice 强制 / 纯 web_search 单工具 / Claude Code 前缀）
    if websearch::should_handle_websearch_request(&payload) {
        tracing::info!("检测到 WebSearch 请求，路由到本地 WebSearch 处理");

        // 估算输入 tokens（只读计数，传引用避免深拷贝整个对话历史）
        let input_tokens = token::count_all_tokens(
            &payload.model,
            payload.system.as_deref(),
            &payload.messages,
            payload.tools.as_deref(),
        ) as i32;

        return websearch::handle_websearch_request(provider, &payload, input_tokens).await;
    }

    // 混合工具场景：请求带 web_search 但未显式触发搜索，剔除 web_search 后走常规转发，
    // 避免把 web_search 原样下发给 Kiro 触发 400 Improperly formed request。
    if websearch::has_web_search_tool(&payload) {
        tracing::info!("检测到混合工具列表中的 web_search，剔除后转发上游");
        websearch::strip_web_search_tools(&mut payload);
    }

    // 转换请求
    let conversion_result = match convert_request(&payload) {
        Ok(result) => result,
        Err(e) => {
            let (error_type, message) = match &e {
                ConversionError::UnsupportedModel(model) => {
                    ("invalid_request_error", format!("模型不支持: {}", model))
                }
                ConversionError::EmptyMessages => {
                    ("invalid_request_error", "消息列表为空".to_string())
                }
            };
            tracing::warn!("请求转换失败: {}", e);
            return (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse::new(error_type, message)),
            )
                .into_response();
        }
    };

    // 构建 Kiro 请求体（发上游前，超阈值时执行输入压缩；profile_arn 由 provider 层注入）
    let request_body = match build_kiro_request_body(
        conversion_result.conversation_state,
        &current_compression(),
    ) {
        Ok(body) => body,
        Err(e) => {
            tracing::error!("序列化请求失败: {}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse::new(
                    "internal_error",
                    format!("序列化请求失败: {}", e),
                )),
            )
                .into_response();
        }
    };

    tracing::debug!("Kiro request body: {}", request_body);

    // 估算输入 tokens（只读计数，传引用避免深拷贝整个对话历史）
    let input_tokens = token::count_all_tokens(
        &payload.model,
        payload.system.as_deref(),
        &payload.messages,
        payload.tools.as_deref(),
    ) as i32;

    // 检查是否启用了thinking
    let thinking_enabled = payload
        .thinking
        .as_ref()
        .map(|t| t.is_enabled())
        .unwrap_or(false);

    let tool_name_map = conversion_result.tool_name_map;
    let known_tool_names = conversion_result.known_tool_names;

    if payload.stream {
        // 流式响应（缓冲模式）
        handle_stream_request_buffered(
            provider,
            &request_body,
            &payload.model,
            input_tokens,
            thinking_enabled,
            tool_name_map,
            known_tool_names,
            client,
        )
        .await
    } else {
        // 非流式响应：仅在配置开启时提取 thinking 块
        let extract_thinking = extract_thinking_enabled() && thinking_enabled;
        handle_non_stream_request(provider, &request_body, &payload.model, input_tokens, extract_thinking, tool_name_map, client).await
    }
}

/// 处理流式请求（缓冲版本）
///
/// 与 `handle_stream_request` 不同，此函数会缓冲所有事件直到流结束，
/// 然后用从 contextUsageEvent 计算的正确 input_tokens 生成 message_start 事件。
async fn handle_stream_request_buffered(
    provider: std::sync::Arc<crate::kiro::provider::KiroProvider>,
    request_body: &str,
    model: &str,
    estimated_input_tokens: i32,
    thinking_enabled: bool,
    tool_name_map: std::collections::HashMap<String, String>,
    known_tool_names: std::collections::HashSet<String>,
    client: ClientInfo,
) -> Response {
    // 1M 变体:据原始模型名判定是否注入 anthropic-beta 头(仅受支持的 [1m] 变体为 true)。
    let is_1m = crate::anthropic::model_catalog::resolve_is_1m(model);
    // 调用 Kiro API（支持多凭据故障转移）
    let (response, meta) = match provider.call_api_stream(request_body, is_1m).await {
        Ok(resp) => resp,
        Err(e) => return map_provider_error(e),
    };

    // 创建缓冲流处理上下文
    let ctx = BufferedStreamContext::new(model, estimated_input_tokens, thinking_enabled, tool_name_map, known_tool_names);

    // 创建缓冲 SSE 流（流结束时用 meta + 最终 usage 埋点）
    let stream = create_buffered_sse_stream(provider, response, ctx, meta, client);

    // 返回 SSE 响应
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONNECTION, "keep-alive")
        .body(Body::from_stream(stream))
        .unwrap()
}

/// 创建缓冲 SSE 事件流
///
/// 工作流程：
/// 1. 等待上游流完成，期间只发送 ping 保活信号
/// 2. 使用 StreamContext 的事件处理逻辑处理所有 Kiro 事件，结果缓存
/// 3. 流结束后，用正确的 input_tokens 更正 message_start 事件
/// 4. 一次性发送所有事件
fn create_buffered_sse_stream(
    provider: std::sync::Arc<crate::kiro::provider::KiroProvider>,
    response: reqwest::Response,
    ctx: BufferedStreamContext,
    meta: crate::kiro::provider::CallMeta,
    client: ClientInfo,
) -> impl Stream<Item = Result<Bytes, Infallible>> {
    let body_stream = response.bytes_stream();

    stream::unfold(
        (
            body_stream,
            ctx,
            EventStreamDecoder::new(),
            false,
            interval(Duration::from_secs(PING_INTERVAL_SECS)),
            meta,
            client,
            provider,
        ),
        |(mut body_stream, mut ctx, mut decoder, finished, mut ping_interval, meta, client, provider)| async move {
            if finished {
                return None;
            }

            loop {
                tokio::select! {
                    // 使用 biased 模式，优先检查 ping 定时器
                    // 避免在上游 chunk 密集时 ping 被"饿死"
                    biased;

                    // 优先检查 ping 保活（等待期间唯一发送的数据）
                    _ = ping_interval.tick() => {
                        tracing::trace!("发送 ping 保活事件（缓冲模式）");
                        let bytes: Vec<Result<Bytes, Infallible>> = vec![Ok(create_ping_sse())];
                        return Some((stream::iter(bytes), (body_stream, ctx, decoder, false, ping_interval, meta, client, provider)));
                    }

                    // 然后处理数据流
                    chunk_result = body_stream.next() => {
                        match chunk_result {
                            Some(Ok(chunk)) => {
                                // 解码事件
                                if let Err(e) = decoder.feed(&chunk) {
                                    tracing::warn!("缓冲区溢出: {}", e);
                                }

                                let mut last_decode_err: Option<String> = None;
                                for result in decoder.decode_iter() {
                                    match result {
                                        Ok(frame) => {
                                            // from_frame 按值吞 frame，事件类型须在 move 前先拥有化捕获。
                                            let et = frame.event_type().map(|s| s.to_string());
                                            match Event::from_frame(frame) {
                                                Ok(event) => {
                                                    // 缓冲事件（复用 StreamContext 的处理逻辑）。
                                                    // in-band Event::Error/Exception 会在此置 completion 失败态。
                                                    ctx.process_and_buffer(&event);
                                                }
                                                Err(err) => {
                                                    // 帧层解码成功、Frame→Event 反序列化失败：
                                                    // toolUseEvent 失败=工具调用不可恢复丢失，置 DecoderStopped
                                                    // 失败态（收尾靠 None 分支补发 SSE error），避免截断被当成功不重试；
                                                    // 非 tool 帧解析失败历史上就允许被忽略，仅告警不置失败态，防误伤正常流。
                                                    if et.as_deref() == Some("toolUseEvent") {
                                                        tracing::warn!("buffered toolUseEvent 帧解析失败,按响应截断处理: {}", err);
                                                        ctx.mark_decoder_stopped(format!("toolUseEvent 帧解析失败: {}", err));
                                                    } else {
                                                        tracing::warn!("buffered 事件帧解析失败(event_type={:?}),已忽略: {}", et.as_deref(), err);
                                                    }
                                                }
                                            }
                                        }
                                        Err(e) => {
                                            last_decode_err = Some(e.to_string());
                                            tracing::warn!("解码事件失败: {}", e);
                                        }
                                    }
                                }
                                // 解码器永久停止：响应必然截断，置失败态供收尾记账。
                                if decoder.is_stopped() {
                                    ctx.mark_decoder_stopped(
                                        last_decode_err.unwrap_or_else(|| "解码器连续错误已停止".to_string()),
                                    );
                                }
                                // 继续读取下一个 chunk，不发送任何数据
                            }
                            Some(Err(e)) => {
                                tracing::error!("读取响应流失败: {}", e);
                                // 上游流中途失败：置传输失败态（供收尾按 NetworkError 记账），
                                // 先发 SSE error 事件显式告知"本次未正常完成"，再补齐已缓冲事件收尾。
                                // 否则 Claude Code 把截断输出当成功、不重试。幂等保留首因。
                                ctx.mark_transport_error(e.to_string());
                                let mut all_events = Vec::new();
                                if !ctx.error_event_emitted() {
                                    all_events.push(SseEvent::error_event(
                                        ctx.completion().sse_error_type(),
                                        ctx.completion().client_message(),
                                    ));
                                    ctx.mark_error_event_emitted();
                                }
                                all_events.extend(ctx.finish_and_get_all_events());
                                let bytes: Vec<Result<Bytes, Infallible>> = all_events
                                    .into_iter()
                                    .map(|e| Ok(Bytes::from(e.to_sse_string())))
                                    .collect();
                                emit_buffered_usage(&provider, &ctx, &meta, &client);
                                return Some((stream::iter(bytes), (body_stream, ctx, decoder, true, ping_interval, meta, client, provider)));
                            }
                            None => {
                                // 流结束，完成处理并返回所有事件（已更正 input_tokens）。
                                // 【缺陷1 时序修复·/cc/v1 同构】finish_and_get_all_events 内部调
                                // generate_final_events（含残留 tool flush，那步才置失败态）。必须**先**跑它,
                                // **再**据 completion 补 error,否则无 stop 残留截断场景漏发 error（客户端误判成功）。
                                // 残留 flush 的 ③ 逻辑置失败态时已返回空(不发坏 JSON),error 插到最前符合 SSE 语义。
                                let tail = ctx.finish_and_get_all_events();
                                let mut all_events = Vec::new();
                                if !ctx.completion().is_ok() && !ctx.error_event_emitted() {
                                    all_events.push(SseEvent::error_event(
                                        ctx.completion().sse_error_type(),
                                        ctx.completion().client_message(),
                                    ));
                                    ctx.mark_error_event_emitted();
                                }
                                all_events.extend(tail);
                                let bytes: Vec<Result<Bytes, Infallible>> = all_events
                                    .into_iter()
                                    .map(|e| Ok(Bytes::from(e.to_sse_string())))
                                    .collect();
                                emit_buffered_usage(&provider, &ctx, &meta, &client);
                                return Some((stream::iter(bytes), (body_stream, ctx, decoder, true, ping_interval, meta, client, provider)));
                            }
                        }
                    }
                }
            }
        },
    )
    .flatten()
}

/// 缓冲流结束时埋点一条成功记录
fn emit_buffered_usage(
    provider: &crate::kiro::provider::KiroProvider,
    ctx: &BufferedStreamContext,
    meta: &crate::kiro::provider::CallMeta,
    client: &ClientInfo,
) {
    let usage = ctx.resolved_usage();
    let mut record = crate::usage::RequestRecord::new(
        Uuid::new_v4().to_string(),
        meta.model.clone().unwrap_or_default(),
    );
    record.credential_id = Some(meta.credential_id);
    record.session_id = meta.session_id.clone();
    record.is_streaming = meta.is_streaming;
    record.input_tokens = usage.input_tokens;
    record.output_tokens = usage.output_tokens;
    record.cache_read_tokens = usage.cache_read_tokens;
    record.cache_creation_tokens = usage.cache_creation_tokens;
    record.credits_used = usage.credits_used;
    record.latency_ms = meta.latency_ms;
    record.retries = meta.retries;
    // 去硬编码 Success：按真实完成状态记账（截断/上游错误不再被记成成功）。
    record.outcome = ctx.completion_outcome();
    // 生命周期累计花费：把本次真实 credit 消耗累加到该凭据（独立于用量保留期，只增不清）。
    if let Some(c) = record.credits_used {
        provider.report_credits(meta.credential_id, c);
    }
    client.apply(&mut record);
    crate::usage::emit_record(record);
}

/// 测试串行锁:IP/机器码黑名单是进程级全局静态(ArcSwap 镜像),多个测试并行读写会互相污染
/// (一个测试清空黑名单会让另一个测试的命中断言失败)。凡改这些全局态的测试都先取此锁,串行执行。
#[cfg(test)]
static BLOCKLIST_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod ip_blocklist_tests {
    //! 业务层 IP 黑名单:按真实客户端 IP(XFF 首段)封禁,反代后也生效。
    use super::*;

    #[test]
    fn test_ip_blocklist_business_layer() {
        let _guard = BLOCKLIST_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // 空黑名单:任何 IP 都不拦。
        set_ip_blocklist(&[]);
        assert!(!ip_is_blocked("223.73.32.14"));
        // 设单 IP + 子网。
        set_ip_blocklist(&["223.73.32.14/32".to_string(), "10.0.0.0/8".to_string()]);
        assert!(ip_is_blocked("223.73.32.14"), "命中单 IP 应拦");
        assert!(ip_is_blocked("10.1.2.3"), "命中子网应拦");
        assert!(!ip_is_blocked("8.8.8.8"), "不在黑名单应放行");
        assert!(!ip_is_blocked("not-an-ip"), "非法 IP 字符串不拦(不 panic)");
        // 清空恢复(避免污染其它测试的全局镜像)。
        set_ip_blocklist(&[]);
        assert!(!ip_is_blocked("223.73.32.14"));
    }
}

#[cfg(test)]
mod machine_code_blocklist_tests {
    //! 业务层机器码黑名单:按当前请求真实客户端 IP 重算机器码,命中即拒(消息 sbsbsb！)。
    use super::*;

    #[test]
    fn test_machine_code_blocklist_business_layer() {
        let _guard = BLOCKLIST_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // 空黑名单:任何机器码都不拦。
        set_machine_code_blocklist(&[]);
        let code = crate::usage::machine_code_of(Some("223.73.32.14"), Some("claude-code"));
        assert!(!machine_code_is_blocked(&code));

        // 拉黑该机器码后命中。
        set_machine_code_blocklist(&[code.clone()]);
        assert!(machine_code_is_blocked(&code), "命中机器码应拦");
        // 大小写不敏感。
        assert!(machine_code_is_blocked(&code.to_uppercase()), "大写形式也应命中");
        // 另一台机器(不同 IP → 不同码)不受影响。
        let other = crate::usage::machine_code_of(Some("8.8.8.8"), Some("claude-code"));
        assert!(!machine_code_is_blocked(&other), "未拉黑的机器码应放行");

        // 有 IP 时 device 不影响判定(machine_key = IP)。
        let same_ip_diff_dev = crate::usage::machine_code_of(Some("223.73.32.14"), Some("vscode"));
        assert!(machine_code_is_blocked(&same_ip_diff_dev), "同 IP 不同 device 仍应命中");

        // 清空恢复(避免污染其它测试的全局镜像)。
        set_machine_code_blocklist(&[]);
        assert!(!machine_code_is_blocked(&code));
    }

    // F2 回归:安全封禁网关独立于 collect_client_fingerprint 隐私开关。
    // 网关直接从请求头解析真实 IP(不走 ClientInfo,后者关指纹时返回空 IP 会让黑名单失效)。
    #[test]
    fn test_security_gate_independent_of_fingerprint_flag() {
        use axum::http::HeaderMap;
        use std::net::SocketAddr;
        use std::sync::atomic::Ordering;

        let _guard = BLOCKLIST_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        // 反代场景:对端=本机 openresty(127.0.0.1),XFF 最右=反代追加的真实客户端 IP。
        // (A1:最右不可伪造;此处 223.73.32.14 是反代追加的真实 IP。)
        let proxy_peer: Option<SocketAddr> = Some("127.0.0.1:9999".parse().unwrap());
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", "10.9.9.9, 223.73.32.14".parse().unwrap());

        // 记录并强制关闭指纹采集(模拟 collect_client_fingerprint=false)。
        let saved = COLLECT_CLIENT_FINGERPRINT.load(Ordering::Relaxed);
        COLLECT_CLIENT_FINGERPRINT.store(false, Ordering::Relaxed);

        // 场景 A:IP 黑名单命中——即便关指纹,网关仍按 XFF 最右真实 IP 拦截(403)。
        set_ip_blocklist(&["223.73.32.14/32".to_string()]);
        set_machine_code_blocklist(&[]);
        let resp = security_block_response(&headers, proxy_peer);
        assert!(resp.is_some(), "关指纹时 IP 黑名单仍应生效(F2)");
        assert_eq!(resp.unwrap().status(), StatusCode::FORBIDDEN);

        // 场景 B:机器码黑名单命中——按真实 IP 重算的码,关指纹也拦。
        set_ip_blocklist(&[]);
        let code = crate::usage::machine_code_of(Some("223.73.32.14"), None);
        set_machine_code_blocklist(&[code.clone()]);
        let resp = security_block_response(&headers, proxy_peer);
        assert!(resp.is_some(), "关指纹时机器码黑名单仍应生效(F2)");
        assert_eq!(resp.unwrap().status(), StatusCode::FORBIDDEN);

        // 场景 C:都不命中→放行(None)。
        set_machine_code_blocklist(&[]);
        assert!(security_block_response(&headers, proxy_peer).is_none(), "未命中应放行");

        // 恢复全局状态,避免污染其它测试。
        set_ip_blocklist(&[]);
        set_machine_code_blocklist(&[]);
        COLLECT_CLIENT_FINGERPRINT.store(saved, Ordering::Relaxed);
    }

    // A1 回归:业务层客户端 IP 取 XFF **最右**(不可伪造),客户端伪造的最左前缀不改变封禁。
    // A2 回归:对端是可信反代(私网)才采信 XFF;公网直连忽略伪造 XFF 用对端。
    #[test]
    fn test_trusted_client_ip_a1_a2_forgery_resistance() {
        use axum::http::HeaderMap;
        use std::net::SocketAddr;

        let _guard = BLOCKLIST_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        let proxy_peer: Option<SocketAddr> = Some("127.0.0.1:8990".parse().unwrap());

        // A1:反代后,XFF = "<客户端伪造>, <反代追加的真实IP>",取最右=真实 IP。
        let mut h = HeaderMap::new();
        h.insert("x-forwarded-for", "8.8.8.8, 203.0.113.7".parse().unwrap());
        assert_eq!(
            trusted_client_ip(&h, proxy_peer).as_deref(),
            Some("203.0.113.7"),
            "反代后应取 XFF 最右真实 IP,不受最左伪造影响"
        );

        // A1 核心:攻击者把自己真实流量伪装成被封 IP——无论前缀怎么伪造,判定结果不变。
        set_ip_blocklist(&["203.0.113.7/32".to_string()]);
        let mut forged = HeaderMap::new();
        // 攻击者(真实 203.0.113.7)想改前缀嫁祸/绕过:仍被反代把真实 IP 追加到最右。
        forged.insert("x-forwarded-for", "1.2.3.4, 203.0.113.7".parse().unwrap());
        assert!(
            security_block_response(&forged, proxy_peer).is_some(),
            "伪造前缀不能绕过对真实最右 IP 的封禁"
        );
        set_ip_blocklist(&[]);

        // A2:对端是公网(客户端直连,非反代)→ 忽略可伪造的 XFF,用对端 IP。
        let public_peer: Option<SocketAddr> = Some("198.51.100.22:5000".parse().unwrap());
        let mut spoof = HeaderMap::new();
        spoof.insert("x-forwarded-for", "10.0.0.1, 203.0.113.7".parse().unwrap());
        assert_eq!(
            trusted_client_ip(&spoof, public_peer).as_deref(),
            Some("198.51.100.22"),
            "公网直连应忽略 XFF,用对端 IP(防直连客户端伪造 XFF)"
        );

        // 直连无 XFF → 回退对端。
        let empty = HeaderMap::new();
        assert_eq!(
            trusted_client_ip(&empty, public_peer).as_deref(),
            Some("198.51.100.22"),
            "无 XFF 应回退对端 IP"
        );
    }
}

#[cfg(test)]
mod error_translation_tests {
    //! 错误翻译层：已确证含义的上游错误 → 带排障步骤的可读错误；未知错误诚实透传（None）。
    use super::*;

    #[test]
    fn test_translate_quota_exhausted() {
        let t = translate_upstream_error("upstream: MONTHLY_REQUEST_COUNT limit reached").unwrap();
        assert_eq!(t.status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(t.error_type, "rate_limit_error");
        assert!(t.message.contains("配额") && t.message.contains("排障"));
    }

    #[test]
    fn test_translate_region_not_activated() {
        let t = translate_upstream_error("403 FEATURE_NOT_SUPPORTED for this region").unwrap();
        assert_eq!(t.error_type, "api_error");
        assert!(t.message.contains("region") && t.message.contains("Profile ARN"));
    }

    #[test]
    fn test_translate_subscription_invalid() {
        let t = translate_upstream_error("Invalid token: subscription expired").unwrap();
        assert!(t.message.contains("刷新 Token") && t.message.contains("排障"));
    }

    #[test]
    fn test_translate_context_full() {
        let t = translate_upstream_error("CONTENT_LENGTH_EXCEEDS_THRESHOLD").unwrap();
        assert_eq!(t.status, StatusCode::BAD_REQUEST);
        assert!(t.message.contains("上下文") && t.message.contains("精简"));
    }

    #[test]
    fn test_translate_input_too_long() {
        let t = translate_upstream_error("Input is too long for the model").unwrap();
        assert_eq!(t.status, StatusCode::BAD_REQUEST);
        assert!(t.message.contains("输入过长") && t.message.contains("拆分"));
    }

    #[test]
    fn test_translate_network_dns() {
        let t = translate_upstream_error("error trying to connect: dns error: failed to resolve").unwrap();
        assert_eq!(t.status, StatusCode::BAD_GATEWAY);
        assert!(t.message.contains("DNS") && t.message.contains("排障"));
    }

    #[test]
    fn test_translate_network_timeout() {
        // 纯 reqwest 超时(无 HTTP 状态码语境)。
        let t = translate_upstream_error("operation timed out").unwrap();
        assert_eq!(t.status, StatusCode::GATEWAY_TIMEOUT);
        assert!(t.message.contains("超时"));
    }

    #[test]
    fn test_translate_tls() {
        // 真实 reqwest TLS 错误在建连阶段,Display 带 "error trying to connect" 传输标志。
        let t = translate_upstream_error(
            "error trying to connect: invalid certificate: SSL handshake failed",
        )
        .unwrap();
        assert!(t.message.contains("TLS") || t.message.contains("证书"));
    }

    #[test]
    fn test_translate_proxy() {
        // 真实 reqwest 代理错误同样在建连阶段包裹。
        let t = translate_upstream_error("error trying to connect: proxy CONNECT failed").unwrap();
        assert!(t.message.contains("代理"));
    }

    #[test]
    fn test_translate_unknown_returns_none() {
        // 未知错误必须返回 None（调用方诚实透传原文，不臆造排障步骤）。
        assert!(translate_upstream_error("some totally unrecognized upstream gibberish").is_none());
    }

    /// review 泄露回归:未知错误的 map_provider_error 响应体**绝不含**原始错误链里的敏感信息
    /// (profileArn / AWS 账号号 / region / 内部 URL)。只给通用提示 + 引导查日志。
    #[test]
    fn test_unknown_error_response_body_no_sensitive_leak() {
        use axum::body::to_bytes;
        // 构造一个含敏感信息的未知错误(模拟上游响应体泄露 ARN/账号)。
        let leaky = anyhow::anyhow!(
            "API 请求失败: 500 {{\"detail\":\"profile arn:aws:codewhisperer:eu-central-1:123456789012:profile/SECRET failed\"}}"
        );
        let resp = map_provider_error(leaky);
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
        let body = futures::executor::block_on(to_bytes(resp.into_body(), usize::MAX)).unwrap();
        let text = String::from_utf8_lossy(&body);
        // 客户端拿到的响应体绝不含任何敏感片段。
        assert!(!text.contains("arn:aws"), "响应体泄露了 ARN: {}", text);
        assert!(!text.contains("123456789012"), "响应体泄露了 AWS 账号号: {}", text);
        assert!(!text.contains("SECRET"), "响应体泄露了 profile id: {}", text);
        assert!(!text.contains("eu-central-1"), "响应体泄露了 region: {}", text);
        // 仍给出通用引导。
        assert!(text.contains("未识别错误") && text.contains("网关日志"));
    }

    /// review high 回归:上游 HTTP 错误**响应体**里恰好含 timeout/tls/proxy/resolve 字样时,
    /// **绝不**被误判成网络故障(它不是传输层错误,无 "error sending request" 等标志)。
    #[test]
    fn test_translate_network_no_false_positive_on_upstream_body() {
        // 模拟 provider 格式化的上游错误串(含 HTTP 状态码 + body,body 里有 "timeout"/"proxy" 字样)。
        let upstream_body =
            "流式 API 请求失败: 400 {\"message\":\"your request proxy timeout config is invalid, tls off\"}";
        // is_transport_error 应判 false → translate_network 返回 None → 整体不误翻译。
        assert!(!is_transport_error(&upstream_body.to_lowercase()));
        assert!(
            translate_network(upstream_body).is_none(),
            "上游 body 含 timeout/proxy/tls 字样不应被误判成网络故障"
        );
    }
}

#[cfg(test)]
mod tier3_hotreload_tests {
    //! TIER3 配置热重载回归：AppState 曾固化的热路径开关改用进程级镜像后，
    //! setter 写入应被对应 getter（handler 热路径读点）立即读到，证明改配置即时生效。
    //!
    //! 注意：镜像是进程级 static，测试间共享同一份。这些测试各自操作**不同的**镜像，
    //! 且末尾恢复默认，避免串扰；不并发断言同一镜像的中间态。
    use super::*;

    #[test]
    fn test_extract_thinking_mirror_roundtrip() {
        set_extract_thinking(true);
        assert!(extract_thinking_enabled(), "set true 后热路径应读到 true");
        set_extract_thinking(false);
        assert!(!extract_thinking_enabled(), "set false 后热路径应读到 false");
    }

    #[test]
    fn test_compression_mirror_roundtrip() {
        use crate::model::config::CompressionConfig;
        let mut c = CompressionConfig::default();
        // 翻转 enabled 以可观测地区分（不依赖具体默认值，只验证 setter→getter 传递）
        c.enabled = !c.enabled;
        let flipped = c.enabled;
        set_compression(c);
        assert_eq!(
            current_compression().enabled,
            flipped,
            "set_compression 后热路径应读到新的 compression 快照"
        );
        // 复位默认，避免影响其它测试
        set_compression(CompressionConfig::default());
    }
}

#[cfg(test)]
mod truncation_completion_tests {
    //! 「截断即成功」修复回归：验证非流式收尾逻辑依赖的
    //! 解码 → CompletionStatus → HTTP 状态码 链路。
    //!
    //! 非流式 handler 与实盘 provider 强耦合，无法在单测里跑完整请求；
    //! 这里用**真实构造的 event-stream 帧**驱动 handler 内部同一套解码 + 事件分类逻辑，
    //! 断言 in-band error 帧会被识别为失败态，且映射到非 200。
    use super::*;
    use crate::kiro::parser::crc::crc32;

    /// 构造一个带指定 message-type / 头部 / payload 的 event-stream 帧。
    ///
    /// 头部编码：name_len(1) + name + type(7=String) + value_len(2) + value。
    fn build_frame(headers: &[(&str, &str)], payload: &[u8]) -> Vec<u8> {
        let mut header_bytes = Vec::new();
        for (name, value) in headers {
            header_bytes.push(name.len() as u8);
            header_bytes.extend_from_slice(name.as_bytes());
            header_bytes.push(7u8); // String
            header_bytes.extend_from_slice(&(value.len() as u16).to_be_bytes());
            header_bytes.extend_from_slice(value.as_bytes());
        }
        let header_length = header_bytes.len() as u32;
        let total_length = (PRELUDE_SIZE + header_bytes.len() + payload.len() + 4) as u32;

        let mut buf = Vec::new();
        buf.extend_from_slice(&total_length.to_be_bytes());
        buf.extend_from_slice(&header_length.to_be_bytes());
        let prelude_crc = crc32(&buf[..8]);
        buf.extend_from_slice(&prelude_crc.to_be_bytes());
        buf.extend_from_slice(&header_bytes);
        buf.extend_from_slice(payload);
        let msg_crc = crc32(&buf);
        buf.extend_from_slice(&msg_crc.to_be_bytes());
        buf
    }

    // 引入 PRELUDE_SIZE
    use crate::kiro::parser::frame::PRELUDE_SIZE;

    /// 复刻非流式 handler 的解码收尾判定：drain 全部帧，遇 in-band error/非 CL 异常/
    /// 解码器停止置失败态，返回最终 CompletionStatus。
    fn decode_to_completion(data: &[u8]) -> CompletionStatus {
        let mut decoder = EventStreamDecoder::new();
        decoder.feed(data).unwrap();

        let mut completion = CompletionStatus::Ok;
        let mut last_err: Option<String> = None;
        for result in decoder.decode_iter() {
            match result {
                Ok(frame) => {
                    // 忠实镜像非流式收尾：move 前先拥有化事件类型，供 Err 分支判据用。
                    let et = frame.event_type().map(|s| s.to_string());
                    match Event::from_frame(frame) {
                        Ok(event) => match event {
                            Event::Error { error_code, error_message } => {
                                if completion.is_ok() {
                                    completion = CompletionStatus::UpstreamError {
                                        code: error_code,
                                        message: error_message,
                                    };
                                }
                            }
                            Event::Exception { exception_type, message } => {
                                if exception_type != "ContentLengthExceededException"
                                    && completion.is_ok()
                                {
                                    completion = CompletionStatus::UpstreamError {
                                        code: exception_type,
                                        message,
                                    };
                                }
                            }
                            _ => {}
                        },
                        Err(_) => {
                            // 镜像非流式：toolUseEvent 帧解析失败 → DecoderStopped 失败态。
                            if et.as_deref() == Some("toolUseEvent") && completion.is_ok() {
                                completion = CompletionStatus::DecoderStopped {
                                    message: "toolUseEvent 帧解析失败".to_string(),
                                };
                            }
                        }
                    }
                }
                Err(e) => last_err = Some(e.to_string()),
            }
        }
        if decoder.is_stopped() && completion.is_ok() {
            completion = CompletionStatus::DecoderStopped {
                message: last_err.unwrap_or_default(),
            };
        }
        completion
    }

    #[test]
    fn test_inband_error_frame_maps_to_non_200() {
        // 回归 BUG①：in-band error 帧过去落入 `_ => {}` 被忽略、照返 200。
        // 现在应被识别为 UpstreamError，映射非 200。
        let frame = build_frame(
            &[(":message-type", "error"), (":error-code", "InternalServerException")],
            b"upstream exploded",
        );
        let completion = decode_to_completion(&frame);

        assert!(!completion.is_ok(), "in-band error 帧应被识别为失败");
        assert_ne!(completion.http_status_u16(), 200, "失败必须返回非 200");
        assert_eq!(completion.http_status_u16(), 502);
        assert_eq!(completion.outcome(), crate::usage::RequestOutcome::ServerError);
    }

    #[test]
    fn test_inband_throttling_error_frame_maps_to_429() {
        let frame = build_frame(
            &[(":message-type", "error"), (":error-code", "ThrottlingException")],
            b"slow down",
        );
        let completion = decode_to_completion(&frame);
        assert_eq!(completion.http_status_u16(), 429);
        assert_eq!(completion.outcome(), crate::usage::RequestOutcome::RateLimited);
    }

    #[test]
    fn test_content_length_exception_frame_stays_ok() {
        // 铁律：ContentLengthExceededException 干净收尾，不算失败，仍走 200。
        let frame = build_frame(
            &[
                (":message-type", "exception"),
                (":exception-type", "ContentLengthExceededException"),
            ],
            b"max tokens reached",
        );
        let completion = decode_to_completion(&frame);
        assert!(completion.is_ok(), "CL 异常不应被判为失败");
        assert_eq!(completion.outcome(), crate::usage::RequestOutcome::Success);
    }

    #[test]
    fn test_toolusevent_parse_failure_maps_to_502() {
        // 回归：toolUseEvent 帧解析失败过去被静默丢弃 → 客户端按 end_turn 当成功不重试。
        // 现在应置 DecoderStopped 失败态，映射 502/ServerError，供收尾补发 error 触发重试。
        // 帧 CRC/framing 合法（decoder 不 is_stopped），仅 ToolUseEvent::from_frame 因非法 JSON 返 Err。
        let frame = build_frame(
            &[(":message-type", "event"), (":event-type", "toolUseEvent")],
            b"not valid json",
        );
        let completion = decode_to_completion(&frame);
        assert!(!completion.is_ok(), "toolUseEvent 解析失败应判失败态");
        assert_eq!(completion.http_status_u16(), 502);
        assert_eq!(completion.outcome(), crate::usage::RequestOutcome::ServerError);
    }

    #[test]
    fn test_non_tool_parse_failure_stays_ok() {
        // 零倒退承诺：非 tool 帧解析失败只应告警、不置失败态。
        // 注意 AssistantResponseEvent.content 有 serde(default)，故须用非法 JSON 而非 `{}` 才能触发反序列化失败。
        let frame = build_frame(
            &[(":message-type", "event"), (":event-type", "assistantResponseEvent")],
            b"not valid json",
        );
        let completion = decode_to_completion(&frame);
        assert!(completion.is_ok(), "非 tool 帧解析失败只应告警,不置失败态");
        assert_eq!(completion.outcome(), crate::usage::RequestOutcome::Success);
    }

    #[test]
    fn test_from_frame_toolusevent_malformed_errs() {
        // 防呆：锁死「frame 层成功、Event 层失败、event_type 在 move 前可取」三条前提，
        // 防未来 payload 结构变动悄悄使该帧变成 Ok。
        let raw = build_frame(
            &[(":message-type", "event"), (":event-type", "toolUseEvent")],
            b"not valid json",
        );
        let mut d = EventStreamDecoder::new();
        d.feed(&raw).unwrap();
        let frame = d.decode_iter().next().unwrap().unwrap();
        assert_eq!(frame.event_type(), Some("toolUseEvent"));
        assert!(Event::from_frame(frame).is_err());
    }
}
