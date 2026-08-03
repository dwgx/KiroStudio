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
use super::stream::{BufferedStreamContext, CacheUsageBreakdown, CompletionStatus, SseEvent, StreamContext};
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

/// `trust_forwarded_header` 的进程级镜像（TIER3 热重载，与上面的指纹开关同款范式）。
///
/// # 修的是什么（已知问题 #6）
///
/// 这个配置项此前**只喂给 `SecurityState`**（`main.rs` 里），业务层 handler 拿不到它，
/// 于是 handler 自己写了一份只看"对端是否私网"的近似判定 → 两层口径分叉。
///
/// 真实受害场景：反代在**公网** IP（CDN 直连 / 跨网段 LB）且管理员开了
/// `trustForwardedHeader=true` 时，security 中间件按 XFF 最右段判定真实客户端，
/// 而 handler 层退回 `peer` = 反代公网 IP → 业务层 IP 黑名单封的是**反代自己**
/// （一封封掉全部用户）；且所有客户端共享同一个机器码，机器码黑名单同样一封封全部。
///
/// 默认 false，与 `Config::default()` 及线上刻意保持的值一致
/// （sub2api 的透传白名单不转发 XFF，开了也拿不到真实 IP）。
static TRUST_FORWARDED_HEADER: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// 设置是否信任转发头（供 main 启动接线 / admin 更新配置时立即生效调用）。
pub fn set_trust_forwarded_header(enabled: bool) {
    TRUST_FORWARDED_HEADER.store(enabled, std::sync::atomic::Ordering::Relaxed);
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
/// ⭐ 直接委托给 [`crate::common::security::client_ip_from_headers`] —— **一份判定逻辑，两层共用**。
///
/// 修复已知问题 #6：此处原先自己实现了一份近似判定（只看 `is_trusted_proxy_peer(peer)`），
/// **完全没有读 `config.trust_forwarded_header`**，与 security 中间件的口径分叉。
/// 分叉的代价见 [`TRUST_FORWARDED_HEADER`] 的说明（黑名单会封掉反代自己 = 全部用户）。
///
/// 保留本函数而不是让调用方直接调 common：调用点需要 `String`（用于黑名单比对与机器码派生），
/// 而 common 返回 `IpAddr`；这层薄封装只做类型转换，不再持有任何判定逻辑。
fn trusted_client_ip(
    headers: &axum::http::HeaderMap,
    peer: Option<std::net::SocketAddr>,
) -> Option<String> {
    let trust = TRUST_FORWARDED_HEADER.load(std::sync::atomic::Ordering::Relaxed);
    crate::common::security::client_ip_from_headers(headers, peer, trust)
        .map(|ip| ip.to_string())
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

/// 是否把**估算的** prompt cache 记账下发给客户端（`promptCacheEnabled` 的进程镜像）。
///
/// 此前 `prompt_cache_enabled` 是**死配置**：全仓零读取点，而注入行为一直无条件发生
/// ——用户显式写 `"promptCacheEnabled": false` 也照样注入，配置在说谎。这里把它接上。
/// 默认 true 以保持既有可观测行为（详见 `config.rs` 的 `default_prompt_cache_enabled`）。
static PROMPT_CACHE_ENABLED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(true);

/// 设置 prompt cache 记账下发开关（main 启动接线 / admin 热更调用，立即生效）。
pub fn set_prompt_cache_enabled(enabled: bool) {
    PROMPT_CACHE_ENABLED.store(enabled, std::sync::atomic::Ordering::Relaxed);
}

fn prompt_cache_enabled() -> bool {
    PROMPT_CACHE_ENABLED.load(std::sync::atomic::Ordering::Relaxed)
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

/// 上游账户级限流的客户端退避建议秒数（`Retry-After` 头取值）。
///
/// 取值依据（2026-07-27 实测，5339 条请求样本）：上游 `USER_REQUEST_RATE_EXCEEDED` 是
/// **状态型惩罚窗口**而非速率阈值——一旦触发就进入被罚态，窗口内继续打会持续被拒，
/// 静置约 2 分钟自愈。「距上次 429 的间隔 → 新请求再被 429 的概率」实测衰减曲线：
///   <1s 47.2% | 1-2s 35.7% | 2-3s 31.4% | 3-5s 26.8% | **5-8s 19.0%** | 12-20s 15.6%
///   | 30-45s 12.3% | 60-120s 6.3% | >120s 0.9%（整体基线 13.3%）
/// 取 8s：曲线上「命中率回落到接近基线」的拐点。再短退避无效（仍在高危档），
/// 再长则白等吞吐。同期实测速率/并发/token 与 429 率的 spearman 仅 +0.09/-0.07/-0.02，
/// 即**退避时长而非降低速率**才是有效手段。
const UPSTREAM_RATE_LIMIT_RETRY_AFTER_SECS: u64 = 8;

/// 是否为上游**账户级速率限流**（可重试，需退避）。
///
/// 判据（只匹配速率类，绝不吞配额类）：
/// - `USER_REQUEST_RATE_EXCEEDED`：Kiro 账户级速率限流的 reason 码（实测当天 595 条）
/// - `INSUFFICIENT_THROUGHPUT`：上游吞吐不足（`I am experiencing high traffic...`，实测 8 条）
/// - `Too many requests`：兜底文案匹配，覆盖未来新增/变更的 reason 码
///
/// 刻意**不匹配** `MONTHLY_REQUEST_COUNT` / `QUOTA`：那是不可重试的月度配额耗尽，
/// 虽同为 429 但不该带 `Retry-After`（要等下个计费周期，给秒数会诱导客户端反复砸死号）。
fn is_upstream_rate_limited(err_str: &str) -> bool {
    err_str.contains("USER_REQUEST_RATE_EXCEEDED")
        || err_str.contains("INSUFFICIENT_THROUGHPUT")
        || err_str.contains("Too many requests")
}

/// 上游 **403 账户级临时风控**（`temporarily is suspended`）。
///
/// # 为什么必须单独分类
///
/// 上游原文（实测）：
/// ```text
/// 403 Forbidden {"__type":"com.amazon.aws.codewhisperer#AccessDeniedException",
///  "message":"Your User ID (450334904897) temporarily is suspended. ..."}
/// ```
///
/// 这个串**匹配不上 `map_provider_error` 的任何分支**：无 `retry_after_secs=`、
/// 无 `model_unsupported_by_pool=1`、不含 `USER_REQUEST_RATE_EXCEEDED` /
/// `INSUFFICIENT_THROUGHPUT` / `Too many requests` / `MONTHLY_REQUEST_COUNT` / `QUOTA`，
/// `is_transport_error` 也不认 → 落函数末尾兜底 → **502 且无 Retry-After**。
///
/// 而它是**限时态** —— 上游自己在文案里写了 `temporarily`，本仓也到处按限时态处理它
/// （`cooldown.rs` 的 `SuspiciousActivity` 给 20s、`is_self_healable_reason` 把
/// `SuspiciousActivityAuto` 列为可自愈、族级退避上限对齐 30min）。唯独**回给客户端时
/// 表达成了永久性服务端故障**，客户端因此不退避、原样重发。
///
/// 线上实测量级：近 2 小时 `auth_failed` 占 **22.3%**（1485/6662），全部是这一种，
/// 且呈**突发**形态（13:50 一次 928 条、14:50 一次 516 条，中间为 0）——
/// 即典型的风控窗口开合，而非账号真被封。
///
/// # 判据为何要窄
///
/// 只匹配 `temporarily is suspended` / `TEMPORARILY_SUSPENDED`，**绝不**泛匹配
/// `AccessDeniedException` 或裸 403：后者会把「账号真被永久封禁」也吞成可重试，
/// 让客户端对一个永远不会恢复的号无限退避重试，同时把真实故障藏起来
/// （与 `translate_quota_subscription` 刻意不吞配额类同理）。
fn is_upstream_temporarily_suspended(err_str: &str) -> bool {
    err_str.contains("temporarily is suspended") || err_str.contains("TEMPORARILY_SUSPENDED")
}

/// 403 临时风控的建议退避秒数。
///
/// 取 20 与 `cooldown.rs` 的 `CooldownReason::SuspiciousActivity`（20s）同源 ——
/// 那是本仓对「这个状态持续多久」的既有判断，复用它而不是另立一个数字，
/// 避免同一语义在两处各有一套时长。
const UPSTREAM_SUSPENDED_RETRY_AFTER_SECS: u64 = 20;

/// 把上游错误串翻译成带排障步骤的可读错误。命中已知类别返回 `Some`，未知返回 `None`（调用方透传）。
/// 不处理需额外响应头的情形（429 + Retry-After 在 `map_provider_error` 单独处理，
/// 含全池冷却与上游账户级限流两类）。
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
    // 上游容量紧张/模型短暂不可用：临时状态，稍后重试即可（常见于新模型发布初期）。
    if err_str.contains("MODEL_TEMPORARILY_UNAVAILABLE") {
        return Some(TranslatedError {
            status: StatusCode::SERVICE_UNAVAILABLE,
            error_type: "overloaded_error",
            message: "上游模型暂时不可用（负载过高），请稍后重试。若持续出现：①换用同族其他版本（如 claude-opus-4.8）；②新发布模型发布初期容量有限，属正常现象，等待 1~2 小时后通常恢复。".to_string(),
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

    // 模型对本号池**永久**不可用（订阅档位不含 / 成本白名单未列）：映射成 404，**绝不带 Retry-After**。
    //
    // 为什么单列一条：号池里有可用号、只是没有一个支持这个模型 —— 这既不是"池子耗尽"(502)
    // 也不是"稍后重试"(429)。给它 Retry-After 会让客户端（Claude Code）每 5 分钟重试一次
    // 直到永远（等多久都不会变），那只是把 502 死循环换成 429 死循环。
    //
    // 用显式标记而非中文文案匹配：文案改动不该让分类失效（这正是"所有凭据均已禁用"落 502 的成因）。
    if err_str.contains("model_unsupported_by_pool=1") {
        tracing::warn!(error = %err, "请求的模型不被本号池支持（永久，重试无效），返回 404");
        return (
            StatusCode::NOT_FOUND,
            Json(ErrorResponse::new(
                "not_found_error",
                "请求的模型不被当前号池支持（所有凭据的订阅档位或成本白名单均不含该模型）。这不是临时故障，重试无效：请换用号池支持的模型，或为凭据开通/放开该模型。",
            )),
        )
            .into_response();
    }

    // 上游**账户级速率限流**：必须映射成 429 + Retry-After，绝不能落到下方兜底的 502。
    //
    // 🔴 修复的致命缺陷：此分支不存在时，上游 429 的错误串
    // （`流式 API 请求失败: 429 Too Many Requests {...USER_REQUEST_RATE_EXCEEDED...}`）
    // 匹配不上任何 translate_* 分支（translate_network 有 is_transport_error 闸门挡住），
    // 于是落到本函数末尾的兜底 → 返回 502 BAD_GATEWAY 且无 Retry-After。
    // 后果链（实测复现）：客户端（Claude Code）把 502 当「服务端故障」而非「太快了」，
    // 其限流退避逻辑压根不启动 → 立刻原样重发 → 撞进上游惩罚窗口 → 又 502。
    //
    // 为什么放在此处而不是 translate_upstream_error 链里：该链的返回类型 TranslatedError
    // 不携带响应头，而本分支的核心价值恰恰是 Retry-After 头（没有它客户端不会退避）。
    // 与上方全池冷却分支同款处理，保持「需要额外响应头的情形都在本函数内联」的既有约定。
    //
    // 判据只匹配**速率**类，绝不吞配额类：MONTHLY_REQUEST_COUNT / QUOTA 是不可重试的月度
    // 配额耗尽（要等下个计费周期），由下方 translate_quota_subscription 处理成不带
    // Retry-After 的 429 —— 给配额耗尽发退避秒数会让客户端做无意义的短退避反复砸死号。
    if is_upstream_rate_limited(&err_str) {
        tracing::warn!(
            error = %err,
            retry_after_secs = UPSTREAM_RATE_LIMIT_RETRY_AFTER_SECS,
            "上游账户级速率限流，返回 429 + Retry-After 让客户端退避（旧代码此处返 502 致客户端不退避）"
        );
        return (
            StatusCode::TOO_MANY_REQUESTS,
            [(
                header::RETRY_AFTER,
                UPSTREAM_RATE_LIMIT_RETRY_AFTER_SECS.to_string(),
            )],
            Json(ErrorResponse::new(
                "rate_limit_error",
                "上游账户级速率限流（请求过于密集）。这是可重试的临时状态，请按 Retry-After 退避后重试。若持续出现：①降低客户端并发；②为号池补充更多凭据分摊速率；③面板『限流健康』确认是否单号承载了全部流量。",
            )),
        )
            .into_response();
    }

    // 上游 **403 账户级临时风控**：映射成 429 + Retry-After，绝不落下方兜底的 502。
    //
    // 判据与理由见 `is_upstream_temporarily_suspended`。要点：上游文案自称 `temporarily`，
    // 本仓各处也按限时态处理，但此前回给客户端的是 502（未识别兜底）→ 客户端把它当
    // 服务端故障、退避逻辑不启动、原样重发。线上近 2h 占 **22.3%** 流量。
    //
    // 放在 `translate_upstream_error` **之前**：那条链的 `translate_quota_subscription`
    // 会用 `QUOTA` 之类的宽判据先行命中一部分 403 文案，而配额类是**不可重试**的
    // （不带 Retry-After）。临时风控必须拿到 Retry-After，故先判。
    if is_upstream_temporarily_suspended(&err_str) {
        tracing::warn!(
            error = %err,
            retry_after_secs = UPSTREAM_SUSPENDED_RETRY_AFTER_SECS,
            "上游账户级临时风控（403 temporarily suspended），返回 429 + Retry-After（旧代码落 502 兜底致客户端不退避）"
        );
        return (
            StatusCode::TOO_MANY_REQUESTS,
            [(
                header::RETRY_AFTER,
                UPSTREAM_SUSPENDED_RETRY_AFTER_SECS.to_string(),
            )],
            Json(ErrorResponse::new(
                "rate_limit_error",
                "上游账户级临时风控（账号被暂时限制，非永久封禁）。这是可恢复的限时状态，请按 Retry-After 退避后重试。若持续出现：①降低并发与请求密度；②为号池补充更多凭据分摊风控压力；③面板『限流健康』查看是否单号承载了全部流量。",
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
    //
    // ⭐ 跨池优先级仲裁（should_try_custom_api_first）：历史实现无条件先试透传，导致用户设的
    //    priority 在跨池维度完全无效（Kiro 号 priority=0 也抢不过代挂号）。现在先仲裁一次：
    //    默认按 priority 公平比较，Kiro 更优时**跳过**透传直接走 Kiro；
    //    即便跳过，Kiro 全失败后 provider 的 failover 依然会落回代挂池，兜底能力不减。
    let user_id = payload.metadata.as_ref().and_then(|m| m.user_id.clone());
    let passthrough_result = if provider.token_manager().should_try_custom_api_first() {
        provider
            .try_custom_api_passthrough(raw_body.clone(), Some(&payload.model), user_id.as_deref())
            .await
    } else {
        None
    };
    if let Some((resp, meta)) = passthrough_result {
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

    // 估算影子缓存：系统提示 + 历史轮次已被 Bedrock prefix cache 缓存（通过 agentContinuationId）。
    // 仅在有历史轮次时（messages.len() > 1）估算；首轮返回 0 保守不注入。
    let prefix_tokens = token::count_prefix_tokens(
        payload.system.as_deref(),
        &payload.messages,
    );
    let cache_breakdown =
        estimate_cache_breakdown(prompt_cache_enabled(), prefix_tokens, input_tokens);

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
                cache_breakdown,
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
                cache_breakdown,
                client,
            )
            .await
        }
    } else {
        // 非流式响应：仅在配置开启时提取 thinking 块
        let extract_thinking = extract_thinking_enabled() && thinking_enabled;
        handle_non_stream_request(provider, &request_body, &payload.model, input_tokens, extract_thinking, tool_name_map, cache_breakdown, client).await
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
    cache_breakdown: Option<CacheUsageBreakdown>,
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
    // 注入影子缓存估算（必须在 generate_initial_events 之前，message_start 才能携带 cache 字段）
    ctx.set_cache_usage(cache_breakdown);

    // 生成初始事件
    let initial_events = ctx.generate_initial_events();

    // 响应头必须在第一个 chunk 之前定稿，故在建流前先读 ctx（消费 ctx 后就拿不到了）。
    // 该头标注 SSE 里的 cache_* 数字是网关估算，见 CACHE_ESTIMATED_HEADER。
    let cache_estimated = ctx.cache_usage.is_some();

    // 创建 SSE 流（流结束时用 meta + 最终 usage 埋点一条成功记录）
    let stream = create_sse_stream(provider, response, ctx, initial_events, meta, client);

    // 返回 SSE 响应
    let mut builder = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONNECTION, "keep-alive");
    if cache_estimated {
        builder = builder.header(CACHE_ESTIMATED_HEADER, CACHE_ESTIMATED_VALUE);
    }
    builder.body(Body::from_stream(stream)).unwrap()
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
    // 注意：record.input_tokens 是 **gross 口径**（含 cache），与发给客户端的
    // message_start/message_delta 里 billed 口径的同名字段不同源，详见 RequestRecord::input_tokens。
    record.input_tokens = usage.input_tokens;
    record.output_tokens = usage.output_tokens;
    record.cache_read_tokens = usage.cache_read_tokens;
    record.cache_creation_tokens = usage.cache_creation_tokens;
    // cache 由本地前缀估算、input 优先取上游百分比反推，两者不同源 → 防御性收敛不变量。
    record.clamp_cache_to_input();
    record.credits_used = usage.credits_used;
    record.latency_ms = meta.latency_ms;
    // TTFB：与 latency_ms 同源起点（meta.started_at），故两者可直接相减得
    // 「响应头 → 首 token」。无内容的响应（纯错误/空）保持 None → 落库 NULL。
    record.first_token_ms = ctx
        .first_token_at()
        .map(|t| t.saturating_duration_since(meta.started_at).as_millis() as u64);
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

/// 标注响应里的 `cache_read_input_tokens` / `cache_creation_input_tokens` 是**网关估算**
/// 而非上游真值的响应头。
///
/// 为什么需要：EXP-0 已实测确证上游 `metadataEvent` 只有 `stopReason`，从不回传
/// `tokenUsage` / `cacheReadInputTokens`（见 `docs/CACHE-EXP0-RESULT.md`）。因此我们下发的
/// 数字来自 `token::count_prefix_tokens` 的本地前缀估算 —— Claude Code 显示的
/// 「缓存命中 N tokens」是我们算的，不是上游说的。
///
/// `docs/CACHE-RFC.md` 的 L2-1 曾建议**停止下发**，但那会让客户端缓存显示与面板统计
/// 一起归零（一次没人要求的可观测性回退）。折中方案是继续下发 + 显式标注，
/// 让需要分辨真伪的调用方有据可依，而不必去读源码或文档。
///
/// 只在**实际下发了** cache 字段时出现（`promptCacheEnabled=true` 且有前缀命中）；
/// 字段缺失时不加，否则头与体自相矛盾。
///
/// 用自定义 `X-` 头而不是塞进 `usage` 对象：Anthropic 的 SDK 会对 usage 做结构化解析，
/// 加未知字段有被严格校验拒绝的风险；而未知响应头对所有 HTTP 客户端都是安全可忽略的。
pub(crate) const CACHE_ESTIMATED_HEADER: &str = "x-kirostudio-cache-estimated";

/// [`CACHE_ESTIMATED_HEADER`] 的值。固定 `"true"` —— 该头存在即表示估算，
/// 不存在即表示未下发 cache 字段，不需要 false 这个取值。
pub(crate) const CACHE_ESTIMATED_VALUE: &str = "true";

/// [`CACHE_ESTIMATED_VALUE`] 的 `HeaderValue` 形态（`headers_mut().insert` 需要它，
/// 而 `Response::builder().header` 接受 `&str`，故两种形态都留着）。
fn cache_estimated_header_value() -> axum::http::HeaderValue {
    axum::http::HeaderValue::from_static(CACHE_ESTIMATED_VALUE)
}

/// 估算本次请求的 prompt cache 记账（供下发给客户端的 usage 字段）。
///
/// **这是本地估算，不是上游真值**：`docs/CACHE-EXP0-RESULT.md` 的 EXP-0 已实测确证上游
/// `metadataEvent` 只有 `stopReason`，从不回传 `tokenUsage` / `cacheReadInputTokens`。
/// 这里的 `cache_read_input_tokens` 就是 `count_prefix_tokens` 的前缀 token 估算值。
///
/// `enabled=false`（`promptCacheEnabled`）时返回 `None`，使**所有**下游注入点自然跳过
/// ——注入点分散在 stream.rs 的五处，全部从 `cache_usage` 读，所以在源头收口比逐个加
/// 判断更不容易漏。返回 `None` 而非 `Some(全 0)` 是刻意的：对 Anthropic 客户端来说
/// `cache_read_input_tokens: 0` 表示"确实没命中"，字段缺失表示"本网关不做该记账"。
///
/// 两条路径（`/v1` 与 `/cc/v1`）此前各自内联一份完全相同的逻辑，收口到这里避免
/// 「改了一处忘了另一处」——那会让开关在其中一条路径上静默失效。
fn estimate_cache_breakdown(
    enabled: bool,
    prefix_tokens: i32,
    input_tokens: i32,
) -> Option<CacheUsageBreakdown> {
    if !enabled || prefix_tokens <= 0 {
        return None;
    }
    Some(CacheUsageBreakdown {
        cache_creation_input_tokens: 0,
        // 估算值按本地 count_all_tokens 收敛，防止前缀估算超过总输入。
        cache_read_input_tokens: prefix_tokens.min(input_tokens),
        cache_creation_5m_input_tokens: 0,
        cache_creation_1h_input_tokens: 0,
    })
}

/// 把影子缓存估算写入用量记录（None 视为无缓存命中 → 记 0）
///
/// 非流式路径没有 `StreamContext`，拿不到 `resolved_usage()`，只有原始的
/// `Option<CacheUsageBreakdown>`。此处收口两个字段的赋值，保证落库数字与
/// 返回给客户端的 `usage.cache_*` 同源（历史缺陷：埋点漏写这两列，
/// 客户端显示 cache_read=12000 而面板恒 0）。
///
/// 写入后即收敛「cache ⊆ gross input」不变量：cache 来自本地前缀估算并按**本地**
/// `count_all_tokens` 估算值 clamp，而 `record.input_tokens` 优先取 `contextUsageEvent`
/// 百分比反推值，二者不同源；反推值偏小时会产出 `cache_read > input_tokens` 的矛盾记录。
/// 故调用前请先设置好 `record.input_tokens`。
fn apply_cache_breakdown(
    record: &mut crate::usage::RequestRecord,
    cache_breakdown: Option<CacheUsageBreakdown>,
) {
    let (read, creation) = match cache_breakdown {
        Some(c) => (c.cache_read_input_tokens, c.cache_creation_input_tokens),
        None => (0, 0),
    };
    record.cache_read_tokens = read;
    record.cache_creation_tokens = creation;
    record.clamp_cache_to_input();
}

/// 处理非流式请求
async fn handle_non_stream_request(
    provider: std::sync::Arc<crate::kiro::provider::KiroProvider>,
    request_body: &str,
    model: &str,
    input_tokens: i32,
    thinking_enabled: bool,
    tool_name_map: std::collections::HashMap<String, String>,
    cache_breakdown: Option<CacheUsageBreakdown>,
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
    // E1：上游结构化 thinking 流（reasoningContentEvent）的累积。与正文分开攒 ——
    // 混进 text_content 会让它被当成用户可见回答，而且下面的标签提取还会再解析一遍。
    let mut reasoning_content = String::new();
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
                        // E1：结构化思考增量（纯 delta，直接追加）。此前落 `_ => {}` 被丢弃，
                        // 非流式只能靠下方的 `<thinking>` 标签提取兜底。
                        Event::ReasoningContent(r) => {
                            reasoning_content.push_str(&r.text);
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
        // 从完整文本中提取 thinking 块（兜底路径：上游走内联 <thinking> 标签时用它）
        let (sniffed_thinking, remaining_text) =
            super::stream::extract_thinking_from_complete_text(&text_content);

        // E1：**优先用上游的结构化流**，标签嗅探仅在结构化流为空时兜底。
        // 与生态实现同款优先级（Kiro-Go：`if thinking && reasoningOutput == "" && extracted != ""`）。
        let thinking = if !reasoning_content.is_empty() {
            Some(reasoning_content.clone())
        } else {
            sniffed_thinking
        };

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
        // gross 口径（含 cache）；下方返回客户端的 usage.input_tokens 才是 billed 口径。
        record.input_tokens = final_input_tokens;
        record.output_tokens = output_tokens;
        // 与下方返回客户端的 usage.cache_* 同源，避免"客户端有值、面板恒 0"的矛盾数字。
        // 必须在 input_tokens 赋值之后调用（内部要按 gross 收敛 cache 上限）。
        apply_cache_breakdown(&mut record, cache_breakdown);
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

    // 构建 usage（注入影子缓存估算字段，让 Claude Code 显示 cache hits）
    let billed_input = if let Some(c) = cache_breakdown {
        super::stream::billed_input_tokens(
            final_input_tokens,
            c.cache_creation_input_tokens,
            c.cache_read_input_tokens,
        )
    } else {
        final_input_tokens
    };
    let mut usage = json!({
        "input_tokens": billed_input,
        "output_tokens": output_tokens
    });
    if let Some(c) = cache_breakdown {
        usage["cache_creation_input_tokens"] = json!(c.cache_creation_input_tokens);
        usage["cache_read_input_tokens"] = json!(c.cache_read_input_tokens);
    }
    // 是否需要标注「这些 cache 数字是网关估算」——仅在真的下发了字段时标，
    // 否则响应头与响应体自相矛盾（见 CACHE_ESTIMATED_HEADER 的说明）。
    let cache_estimated = cache_breakdown.is_some();

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

    let mut resp = (StatusCode::OK, Json(response_body)).into_response();
    if cache_estimated {
        resp.headers_mut()
            .insert(CACHE_ESTIMATED_HEADER, cache_estimated_header_value());
    }
    resp
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

    // 估算影子缓存（与 /v1 路径逻辑一致）
    let prefix_tokens = token::count_prefix_tokens(
        payload.system.as_deref(),
        &payload.messages,
    );
    let cache_breakdown =
        estimate_cache_breakdown(prompt_cache_enabled(), prefix_tokens, input_tokens);

    // 检查是否启用了thinking
    let thinking_enabled = payload
        .thinking
        .as_ref()
        .map(|t| t.is_enabled())
        .unwrap_or(false);

    let tool_name_map = conversion_result.tool_name_map;
    let known_tool_names = conversion_result.known_tool_names;

    if payload.stream {
        // ⭐ /cc/v1 也必须尊重 ccAutoBuffer（历史缺陷：此处曾**无条件** buffered）。
        //
        // 背景：buffered 分发会把整轮回答憋到上游流结束才一次性吐，期间对客户端**只发 ping**。
        // 项目已在 `default_cc_auto_buffer()` 里坐实它的两个代价并因此把 /v1 的默认改成真流式：
        //   ① contextUsageEvent 结尾才到 → 整轮看不到进度，模型越慢**越像卡死**
        //      （客户端侧表现为 "Stream idle timeout - no chunks received"）；
        //   ② CC 的 steering（执行途中插消息引导）依赖观察流式增量，buffered 把整轮变成
        //      不可打断的黑盒 → 途中发消息要等整轮憋完才被处理。
        // 但那次修正只落在 /v1，本端点仍强制 buffered —— 于是把 CC 指向 /cc/v1 的用户
        // 拿到的是旧的有害行为，且**把 ccAutoBuffer 设成 false 也关不掉**（开关对本路径无效）。
        //
        // 现在两个端点由同一个开关统一语义：
        //   ccAutoBuffer=false（默认）→ 两端都真流式（内容边到边转发）
        //   ccAutoBuffer=true          → 两端都 buffered（换取 message_start 即精确 input_tokens）
        if cc_auto_buffer_enabled() {
            tracing::debug!(
                "/cc/v1 流式分发: buffered（ccAutoBuffer=true；整轮只发 ping 直到上游流结束）"
            );
            handle_stream_request_buffered(
                provider,
                &request_body,
                &payload.model,
                input_tokens,
                thinking_enabled,
                tool_name_map,
                known_tool_names,
                cache_breakdown,
                client,
            )
            .await
        } else {
            tracing::debug!("/cc/v1 流式分发: 真流式（ccAutoBuffer=false，内容边到边转发）");
            handle_stream_request(
                provider,
                &request_body,
                &payload.model,
                input_tokens,
                thinking_enabled,
                tool_name_map,
                known_tool_names,
                cache_breakdown,
                client,
            )
            .await
        }
    } else {
        // 非流式响应：仅在配置开启时提取 thinking 块
        let extract_thinking = extract_thinking_enabled() && thinking_enabled;
        handle_non_stream_request(provider, &request_body, &payload.model, input_tokens, extract_thinking, tool_name_map, cache_breakdown, client).await
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
    cache_breakdown: Option<CacheUsageBreakdown>,
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
    let mut ctx = BufferedStreamContext::new(model, estimated_input_tokens, thinking_enabled, tool_name_map, known_tool_names);
    // 注入影子缓存估算（finish_and_get_all_events 回补 message_start 时会携带 cache 字段）
    ctx.set_cache_usage(cache_breakdown);

    // 响应头须在首个 chunk 前定稿，故在建流（消费 ctx）之前先取。
    // 这条 buffered 路径是线上默认（ccAutoBuffer=true），标注不能只做在流式路径上。
    let cache_estimated = cache_breakdown.is_some();

    // 创建缓冲 SSE 流（流结束时用 meta + 最终 usage 埋点）
    let stream = create_buffered_sse_stream(provider, response, ctx, meta, client);

    // 返回 SSE 响应
    let mut builder = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONNECTION, "keep-alive");
    if cache_estimated {
        builder = builder.header(CACHE_ESTIMATED_HEADER, CACHE_ESTIMATED_VALUE);
    }
    builder.body(Body::from_stream(stream)).unwrap()
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
    // 同 emit_stream_usage：这里的 input_tokens 是 gross 口径（含 cache），
    // 与 message_start 里 billed 口径的同名字段不是一回事。
    record.input_tokens = usage.input_tokens;
    record.output_tokens = usage.output_tokens;
    record.cache_read_tokens = usage.cache_read_tokens;
    record.cache_creation_tokens = usage.cache_creation_tokens;
    // cache 由本地前缀估算、input 优先取上游百分比反推，两者不同源 → 防御性收敛不变量。
    record.clamp_cache_to_input();
    record.credits_used = usage.credits_used;
    record.latency_ms = meta.latency_ms;
    // TTFB：与 latency_ms 同源起点（meta.started_at），故两者可直接相减得
    // 「响应头 → 首 token」。无内容的响应（纯错误/空）保持 None → 落库 NULL。
    record.first_token_ms = ctx
        .first_token_at()
        .map(|t| t.saturating_duration_since(meta.started_at).as_millis() as u64);
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

#[cfg(test)]
mod non_stream_cache_accounting_tests {
    //! 非流式路径的 cache 记账：埋点必须与返回客户端的 usage.cache_* 同源。
    //! 历史缺陷：埋点块漏写 cache_read_tokens/cache_creation_tokens，
    //! 客户端拿到 cache_read=12000 而落库恒 0。
    use super::*;

    fn new_record() -> crate::usage::RequestRecord {
        crate::usage::RequestRecord::new("req-1", "claude-sonnet-5")
    }

    #[test]
    fn should_write_cache_read_and_creation_from_breakdown() {
        let mut record = new_record();
        // 契约：先设 gross input_tokens，再写 cache（apply 内部按 gross 收敛上限）
        record.input_tokens = 20000;
        apply_cache_breakdown(
            &mut record,
            Some(CacheUsageBreakdown {
                cache_creation_input_tokens: 300,
                cache_read_input_tokens: 12000,
                cache_creation_5m_input_tokens: 300,
                cache_creation_1h_input_tokens: 0,
            }),
        );
        assert_eq!(record.cache_read_tokens, 12000, "cache_read 必须落库");
        assert_eq!(record.cache_creation_tokens, 300, "cache_creation 必须落库");
    }

    #[test]
    fn should_clamp_cache_read_to_gross_input_when_context_estimate_is_lower() {
        // cache_read 由本地前缀估算并按本地 count_all_tokens clamp（=12000），
        // 而落库 input_tokens 取 contextUsageEvent 百分比反推值（=5000）。
        // 两者不同源，反推值偏小时会产出 cache_read > input_tokens 的矛盾记录。
        let mut record = new_record();
        record.input_tokens = 5000;
        apply_cache_breakdown(
            &mut record,
            Some(CacheUsageBreakdown {
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 12000,
                cache_creation_5m_input_tokens: 0,
                cache_creation_1h_input_tokens: 0,
            }),
        );
        assert_eq!(
            record.cache_read_tokens, 5000,
            "cache_read 不得超过 gross input_tokens"
        );
        assert_eq!(record.billed_input_tokens(), 0, "billed 不得为负");
    }

    /// `promptCacheEnabled=false` 必须让记账**整体缺失**（None），不是 Some(全 0)。
    ///
    /// 这个区别对客户端是实质性的：`cache_read_input_tokens: 0` 表示"确实一次都没命中"，
    /// 字段缺失表示"本网关不做该记账"。注入 0 会把"未记账"误报成"缓存全未命中"。
    #[test]
    fn should_omit_cache_breakdown_entirely_when_disabled() {
        assert!(
            estimate_cache_breakdown(false, 12_000, 20_000).is_none(),
            "关闭时必须返回 None（字段缺失），不能是 Some(0)"
        );
        // 开启且有前缀 → 正常记账
        let on = estimate_cache_breakdown(true, 12_000, 20_000).expect("开启时应有记账");
        assert_eq!(on.cache_read_input_tokens, 12_000);
    }

    /// 首轮请求（无历史前缀）在开启时也应为 None —— 没有可复用前缀就不该声称命中。
    #[test]
    fn should_omit_cache_breakdown_when_no_prefix_tokens() {
        assert!(estimate_cache_breakdown(true, 0, 20_000).is_none());
        assert!(estimate_cache_breakdown(true, -1, 20_000).is_none());
    }

    /// 标注头只在**真的下发了** cache 字段时出现，否则头与响应体自相矛盾。
    ///
    /// 三条响应路径（非流式 / 流式 SSE / buffered SSE）都用同一个判据
    /// `cache_breakdown.is_some()` —— 与 estimate_cache_breakdown 的返回一致。
    /// 这条测试守的是「判据同源」：只要下发条件变了，标注条件必须跟着变。
    #[test]
    fn should_mark_estimated_only_when_cache_fields_are_sent() {
        // 开启且有前缀 → 下发字段 → 应标注
        let sent = estimate_cache_breakdown(true, 12_000, 20_000);
        assert!(sent.is_some(), "应下发 cache 字段");

        // 开关关闭 → 不下发 → 不应标注
        assert!(
            estimate_cache_breakdown(false, 12_000, 20_000).is_none(),
            "关闭时不下发，故不应加标注头"
        );
        // 首轮无前缀 → 不下发 → 不应标注
        assert!(
            estimate_cache_breakdown(true, 0, 20_000).is_none(),
            "无前缀命中时不下发，故不应加标注头"
        );
    }

    /// 头名与值必须是合法 HTTP 头（大小写、非法字符会在运行时 panic 而非编译期报错）。
    #[test]
    fn should_use_valid_lowercase_header_name_and_value() {
        assert_eq!(
            CACHE_ESTIMATED_HEADER,
            CACHE_ESTIMATED_HEADER.to_ascii_lowercase(),
            "HTTP/2 要求头名小写，写成大写会在某些客户端上出问题"
        );
        // from_static 对非法值会 panic —— 这里显式构造一次，把 panic 暴露在测试而非生产
        let v = cache_estimated_header_value();
        assert_eq!(v.to_str().unwrap(), "true");
        assert!(
            axum::http::HeaderName::try_from(CACHE_ESTIMATED_HEADER).is_ok(),
            "头名必须是合法 HeaderName"
        );
    }

    /// 前缀估算超过总输入时必须收敛到总输入（两个数字不同源，见 clamp_cache_to_input）。
    #[test]
    fn should_clamp_estimated_prefix_to_input_tokens() {
        let c = estimate_cache_breakdown(true, 99_000, 4_000).expect("应有记账");
        assert_eq!(
            c.cache_read_input_tokens, 4_000,
            "cache_read 不得超过本次输入总量"
        );
    }

    #[test]
    fn should_write_zero_when_no_cache_breakdown() {
        let mut record = new_record();
        record.cache_read_tokens = 999;
        record.cache_creation_tokens = 999;
        apply_cache_breakdown(&mut record, None);
        assert_eq!(record.cache_read_tokens, 0, "首轮无前缀缓存应记 0");
        assert_eq!(record.cache_creation_tokens, 0, "首轮无前缀缓存应记 0");
    }

    /// 源码级守卫：非流式成功埋点块必须调用 [`apply_cache_breakdown`]。
    /// 纯单测覆盖不到 `handle_non_stream_request`（需真实上游 + `CallMeta`/`InflightGuard`），
    /// 故用本文件源码断言把"埋点块漏写 cache 字段"这一具体回归钉死。
    #[test]
    fn should_call_apply_cache_breakdown_in_non_stream_emit_block() {
        let src = include_str!("handlers.rs");
        let block = src
            .split("// 用量埋点：非流式成功记录")
            .nth(1)
            .expect("非流式成功埋点块的定位注释不应被删改");
        let block = block
            .split("crate::usage::emit_record(record);")
            .next()
            .expect("埋点块应以 emit_record 收尾");
        assert!(
            block.contains("apply_cache_breakdown(&mut record, cache_breakdown)"),
            "非流式成功埋点块必须写入 cache 字段,否则落库与客户端 usage 矛盾"
        );
    }
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

    /// 回归（已知问题 #6）：handler 层必须遵守 `trust_forwarded_header`。
    ///
    /// **旧代码为何 FAIL**：`trusted_client_ip` 自己实现了一份判定，只看
    /// `is_trusted_proxy_peer(peer)`（对端是否私网/环回），**根本没有读**
    /// `config.trust_forwarded_header` —— 该 flag 在 `main.rs` 里只喂给了 `SecurityState`。
    /// 于是对端是**公网**反代时，无论开关开没开，handler 都退回 `peer`，
    /// 本测试第二段断言（应取 XFF 最右段）必然 FAIL。
    ///
    /// 生产后果：反代在公网 IP（CDN 直连 / 跨网段 LB）且管理员开了 `trustForwardedHeader=true` 时，
    /// security 中间件按 XFF 最右段判真实客户端，而业务层退回反代公网 IP →
    /// **IP 黑名单实际封的是反代自己**，一封就封掉全部用户；且所有客户端共享同一个机器码，
    /// 机器码黑名单同样一封封全部。
    #[test]
    fn test_trusted_client_ip_respects_trust_forwarded_header_config() {
        use axum::http::HeaderMap;
        use std::net::SocketAddr;

        let _guard = BLOCKLIST_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        // 反代在**公网** IP —— 这是本缺陷唯一的受害场景（私网对端两种实现结果相同，测不出差异）。
        let public_proxy: Option<SocketAddr> = Some("203.0.113.99:443".parse().unwrap());
        let mut h = HeaderMap::new();
        h.insert("x-forwarded-for", "1.2.3.4, 198.51.100.7".parse().unwrap());

        // 开关关（默认）：忽略 XFF，用对端 —— 直连客户端伪造 XFF 时这是正确行为。
        set_trust_forwarded_header(false);
        assert_eq!(
            trusted_client_ip(&h, public_proxy).as_deref(),
            Some("203.0.113.99"),
            "开关关闭时应忽略公网对端的 XFF（防伪造）"
        );

        // 开关开：应采信 XFF **最右**段（反代追加的、不可伪造的那段），与 security 中间件同口径。
        set_trust_forwarded_header(true);
        assert_eq!(
            trusted_client_ip(&h, public_proxy).as_deref(),
            Some("198.51.100.7"),
            "开关开启时必须采信 XFF 最右段（旧代码无视该配置，恒返回反代 IP → 黑名单封掉反代自己）"
        );

        // 复位，避免污染同进程内其它测试（进程级 atomic 是全局状态）。
        set_trust_forwarded_header(false);
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

    /// ⭐ 致命缺陷回归（旧代码必失败）：上游账户级 429 曾被映射成 502 且无 Retry-After。
    ///
    /// 旧代码路径：该错误串匹配不上任何 translate_* 分支（translate_network 有
    /// is_transport_error 闸门挡住）→ translate_upstream_error 返 None → map_provider_error
    /// 落到兜底 → 502 BAD_GATEWAY。客户端（Claude Code）把 502 当服务故障、退避逻辑不启动、
    /// 立刻重发 → 撞进上游惩罚窗口（实测窗口内命中率 47.2%）→ 单次拒绝被放大成
    /// 最长 52min/431 次的持续发作（当天 3 个长发作占全部 429 的 84%）。
    #[test]
    fn test_upstream_429_maps_to_429_with_retry_after() {
        // provider 实际组装的错误串原文（含 HTTP 状态码 + 上游 body）。
        let raw = r#"流式 API 请求失败: 429 Too Many Requests {"message":"Too many requests, please wait before trying again.","reason":"USER_REQUEST_RATE_EXCEEDED"}"#;
        let err = anyhow::Error::msg(raw);
        let resp = map_provider_error(err);
        assert_eq!(
            resp.status(),
            StatusCode::TOO_MANY_REQUESTS,
            "上游速率限流必须映射成 429（旧代码返 502 → 客户端不退避）"
        );
        let hv = resp
            .headers()
            .get(header::RETRY_AFTER)
            .expect("上游 429 必须带 Retry-After 头，否则客户端退避逻辑不启动");
        assert_eq!(hv.to_str().unwrap(), "8");
    }

    /// ⭐ 致命缺陷回归（去掉 `retry_after_secs=` 标记即必失败）：**号池真耗尽**曾落 502 无 Retry-After。
    ///
    /// 与上面那条是同一类缺陷的不同实例。0.7.45 只修了情形②（模型硬门，加
    /// `model_unsupported_by_pool=1` 标记），情形①「available == 0 真耗尽」当时未处理，
    /// 而它才是量最大的那个：
    ///
    /// 线上 2026-08-03 01:55–02:10 号池被烧空的 15 分钟窗口里，`所有凭据均已禁用（0/0）`
    /// 产生 2082 次，单个 5 分钟桶峰值 937 次 —— 且该窗口内**未识别兜底 502 全部是这一种**。
    ///
    /// 旧路径：该串既无 `retry_after_secs=`、也无 `model_unsupported_by_pool=1`、不含
    /// QUOTA 等上游关键词、`is_transport_error` 也不认 → 逐条穿过所有分支 → 落
    /// `map_provider_error` 末尾兜底 → 502 且无 Retry-After → 客户端不退避、原样重发。
    ///
    /// 为什么"真耗尽"该给退避而不是当永久故障：它**会自愈**（全池自愈实测 41 分钟触发 36 次），
    /// 403 `TEMPORARILY_SUSPENDED` 本身也是限时态。
    #[test]
    fn test_pool_truly_exhausted_maps_to_429_with_retry_after_not_502() {
        // token_manager 的两个 bail 点实际组装的错误串原文。
        let err = anyhow::Error::msg("所有凭据均已禁用（0/0）retry_after_secs=10");
        let resp = map_provider_error(err);
        assert_eq!(
            resp.status(),
            StatusCode::TOO_MANY_REQUESTS,
            "号池真耗尽必须映射成可重试的 429（旧代码落兜底 → 502 且无 Retry-After → \
             客户端把它当服务故障、退避不启动、原样重发；实测 15 分钟内 2082 次）"
        );
        let hv = resp
            .headers()
            .get(header::RETRY_AFTER)
            .expect("号池耗尽必须带 Retry-After，否则客户端退避逻辑不启动");
        assert_eq!(hv.to_str().unwrap(), "10");
    }

    /// ⭐ 致命缺陷回归（去掉分类分支即 FAIL）：**403 账户级临时风控**曾落 502 无 Retry-After。
    ///
    /// 与「上游 429」「号池真耗尽」是同一类缺陷的第三个实例，也是**量最大**的一个：
    /// 线上近 2 小时 `auth_failed` 占 **22.3%**（1485/6662），全部是这一种，
    /// 且呈突发形态（13:50 一次 928 条、14:50 一次 516 条，中间为 0）= 风控窗口开合。
    ///
    /// 旧路径：该串不含任何已知关键词 → 逐条穿过所有分支 → 末尾兜底 502 无 Retry-After
    /// → 客户端把限时风控当服务端故障、不退避、原样重发 → 加深上游风控判定。
    #[test]
    fn test_upstream_temporarily_suspended_maps_to_429_with_retry_after() {
        // provider 实际组装的错误串原文（线上 traces.db 取出，账号 id 已改）。
        let raw = r#"流式 API 请求失败: 403 Forbidden {"__type":"com.amazon.aws.codewhisperer#AccessDeniedException","message":"Your User ID (450334904897) temporarily is suspended. We've locked your account as a security precaution. To restore access, please contact our support team to verify your identity: https://aws.amazon.com/contact-us/"}"#;
        let resp = map_provider_error(anyhow::Error::msg(raw));
        assert_eq!(
            resp.status(),
            StatusCode::TOO_MANY_REQUESTS,
            "403 临时风控必须映射成可重试的 429（旧代码落兜底 → 502 无 Retry-After → \
             客户端不退避、原样重发；实测占 22.3% 流量）"
        );
        let hv = resp
            .headers()
            .get(header::RETRY_AFTER)
            .expect("403 临时风控必须带 Retry-After，否则客户端退避逻辑不启动");
        assert_eq!(hv.to_str().unwrap(), "20");
    }

    /// 边界：判据必须**窄** —— 不带 `temporarily` 的 403 不得被吞成可重试。
    ///
    /// 若泛匹配 `AccessDeniedException` 或裸 403，账号**真被永久封禁**时也会返回
    /// 429 + Retry-After，客户端会对一个永远不会恢复的号无限退避重试，
    /// 同时把真实故障藏起来。与 `translate_quota_subscription` 刻意不吞配额类同理。
    #[test]
    fn test_permanent_access_denied_is_not_absorbed_as_retryable() {
        let raw = r#"流式 API 请求失败: 403 Forbidden {"__type":"com.amazon.aws.codewhisperer#AccessDeniedException","message":"Your account has been permanently disabled for violating the terms of service."}"#;
        let resp = map_provider_error(anyhow::Error::msg(raw));
        assert_ne!(
            resp.status(),
            StatusCode::TOO_MANY_REQUESTS,
            "永久封禁不得被判成可重试的 429（会让客户端对死号无限重试并掩盖真实故障）"
        );
        assert!(
            resp.headers().get(header::RETRY_AFTER).is_none(),
            "永久封禁不该带 Retry-After"
        );
    }

    /// 边界：坐实上面那条测的是**标记**而非中文文案 —— 不带标记的同款文案仍落 502 兜底。
    ///
    /// 这条的作用是防止将来有人"顺手"改成按 `所有凭据均已禁用` 文案匹配：那正是本类缺陷
    /// 反复出现的成因（文案一改分类就失效）。它同时证明修复的承重点在 token_manager
    /// 那两个 bail 串上，而不在本函数里。
    #[test]
    fn test_pool_exhausted_without_marker_still_falls_through_to_502() {
        let err = anyhow::Error::msg("所有凭据均已禁用（0/0）");
        let resp = map_provider_error(err);
        assert_eq!(
            resp.status(),
            StatusCode::BAD_GATEWAY,
            "不带 retry_after_secs 标记时应仍落兜底 —— 说明分类判据是标记而非中文文案"
        );
    }

    #[test]
    fn test_insufficient_throughput_also_maps_to_429() {
        // 另一种上游限流文案（实测 8 条）：high traffic / INSUFFICIENT_THROUGHPUT。
        let raw = r#"流式 API 请求失败: 429 Too Many Requests {"message":"I am experiencing high traffic, please try again shortly.","reason":"INSUFFICIENT_THROUGHPUT"}"#;
        let err = anyhow::Error::msg(raw);
        let resp = map_provider_error(err);
        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
        assert!(resp.headers().get(header::RETRY_AFTER).is_some());
    }

    #[test]
    fn test_quota_exhausted_stays_429_without_retry_after() {
        // 边界：配额耗尽同为 429 但**不可重试**（要等下个计费周期）→ 绝不能带 Retry-After，
        // 否则客户端会做无意义的 8s 退避后反复砸一个本月已无额度的号。
        // 同时验证限流判据没有误吞它（is_upstream_rate_limited 不匹配 MONTHLY_REQUEST_COUNT）。
        let err = anyhow::Error::msg("upstream: MONTHLY_REQUEST_COUNT limit reached");
        let resp = map_provider_error(err);
        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
        assert!(
            resp.headers().get(header::RETRY_AFTER).is_none(),
            "配额耗尽不该带 Retry-After（不可重试）"
        );
    }

    #[test]
    fn test_rate_limit_judgement_does_not_swallow_quota() {
        // 判据单测：速率类命中、配额类不命中。防止后续有人放宽判据把配额也吞进来。
        assert!(is_upstream_rate_limited(
            r#"{"reason":"USER_REQUEST_RATE_EXCEEDED"}"#
        ));
        assert!(is_upstream_rate_limited(
            r#"{"reason":"INSUFFICIENT_THROUGHPUT"}"#
        ));
        assert!(is_upstream_rate_limited("429 Too many requests"));
        assert!(!is_upstream_rate_limited(
            "upstream: MONTHLY_REQUEST_COUNT limit reached"
        ));
        assert!(!is_upstream_rate_limited("403 FEATURE_NOT_SUPPORTED"));
        assert!(!is_upstream_rate_limited("CONTENT_LENGTH_EXCEEDS_THRESHOLD"));
    }

    #[test]
    fn test_pool_cooling_retry_after_still_takes_precedence() {
        // 零回归：全池冷却分支（带 retry_after_secs=N 标记）在限流判据之前，
        // 其上游给定的精确秒数不该被本次新增的固定 8s 覆盖。
        let err = anyhow::Error::msg("所有凭据均在冷却（1/5）retry_after_secs=14");
        let resp = map_provider_error(err);
        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(
            resp.headers()
                .get(header::RETRY_AFTER)
                .unwrap()
                .to_str()
                .unwrap(),
            "14",
            "全池冷却的精确 retry_after 不该被固定 8s 覆盖"
        );
    }

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
    fn cc_auto_buffer_static_matches_config_default() {
        // ccAutoBuffer 的默认值散落三处，历史上长期不一致（config 默认 false，而本文件的
        // static 初值与 admin 快照 Default 都是 true）。运行时 static 会被 main 启动播种覆盖，
        // 所以不一致不会立刻出错——但会让单元测试、以及任何绕过 create_router_with_provider
        // 的代码路径读到错的默认值，排障时极易误判。此处把两者钉死。
        //
        // ⚠️ 本测试必须在任何 set_cc_auto_buffer 之前读取，故不与其它 TIER3 测试共用镜像。
        assert_eq!(
            cc_auto_buffer_enabled(),
            crate::model::config::Config::default().cc_auto_buffer,
            "CC_AUTO_BUFFER static 初值与 config 默认不一致：改任一处都必须同步另一处\
             （src/anthropic/handlers.rs 的 static、src/model/config.rs 的 default_cc_auto_buffer）"
        );
    }

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
