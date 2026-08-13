//! Anthropic API Handler 函数

use std::convert::Infallible;

use crate::kiro::model::events::Event;
use crate::kiro::model::requests::kiro::KiroRequest;
use crate::kiro::parser::decoder::EventStreamDecoder;
use crate::token;
use anyhow::Error;
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
use super::stream::{
    BufferedStreamContext, CacheUsageBreakdown, CompletionStatus, SseEvent, StreamContext,
};
use super::types::{
    CountTokensRequest, CountTokensResponse, ErrorResponse, MessagesRequest, Model, ModelsResponse,
    OutputConfig, Thinking,
};
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
    crate::common::security::client_ip_from_headers(headers, peer, trust).map(|ip| ip.to_string())
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
static EXTRACT_THINKING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

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
    let ua = headers
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok());
    crate::usage::classify_device(ua).as_deref() == Some("claude-code")
}

/// 输入压缩配置的进程级镜像（TIER3 热更）。
///
/// `CompressionConfig` 非标量（阈值 + 开关），用 `ArcSwap` 承载：admin 改配置时整份原子换、
/// handler 热路径 `load_full()` 拿 `Arc` 快照（无锁近零成本）。`OnceLock` 惰性初始化，
/// main 启动即 `set_compression` 写入真配置；未初始化时回退默认（与 config 默认一致）。
static COMPRESSION: std::sync::OnceLock<
    arc_swap::ArcSwap<crate::model::config::CompressionConfig>,
> = std::sync::OnceLock::new();

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

/// WebSearch 回灌循环（websearch.rs）用的薄包装：构造 Kiro 请求体并做输入压缩，
/// 与主路径 `build_kiro_request_body` 完全同源（同一压缩配置），不自己写一份。
pub(super) fn build_kiro_request_body_for_websearch(
    conversation_state: crate::kiro::model::requests::conversation::ConversationState,
    additional_model_request_fields: Option<
        crate::kiro::model::requests::kiro::AdditionalModelRequestFields,
    >,
) -> Result<String, serde_json::Error> {
    build_kiro_request_body(
        conversation_state,
        additional_model_request_fields,
        &current_compression(),
        None,
    )
}

/// WebSearch 回灌循环用的薄包装：把 provider 错误映射成 HTTP 响应，
/// 与主路径 `map_provider_error` 同口径（上游错误码/可重试性判定两边一致）。
pub(super) fn map_provider_error_for_websearch(err: anyhow::Error) -> Response {
    map_provider_error(err)
}

/// 混合工具（web_search + 其他工具）场景的 WebSearch agentic 回灌分派。
///
/// `/v1/messages` 与 `/cc/v1/messages` 两个端点**共用这一份**：两处此前是逐字复制的
/// 同一段 web_search 处理，本仓已有多次「同一逻辑各写一份 → 只改了一处 → 行为分叉」
/// 的事故（见 update.rs:246 抽公共函数的理由）。收口成一个函数，改一次两端同时生效。
///
/// 返回 `None` 表示本请求不属于该场景，调用方继续走常规转发路径（行为完全不变）。
async fn dispatch_web_search_loop(
    provider: &std::sync::Arc<crate::kiro::provider::KiroProvider>,
    payload: &MessagesRequest,
    budget: &crate::kiro::provider::SharedRetryBudget,
    client: &ClientInfo,
) -> Option<Response> {
    if !websearch::has_web_search_tool(payload) {
        return None;
    }
    tracing::info!("混合工具列表含 web_search，走常规转发 + WebSearch 回灌");

    // 估算输入 tokens 作为回灌链路的兜底口径（上游 contextUsageEvent 到达后优先用它）。
    let fallback_input_tokens = token::count_all_tokens(
        &payload.model,
        payload.system.as_deref(),
        &payload.messages,
        payload.tools.as_deref(),
    ) as i32;
    // `stream` 必须在 payload 被 move 进循环之前取（循环内会追加回灌消息、消费 payload）。
    let wants_stream = payload.stream;

    let resp = match websearch::run_web_search_loop(
        provider.clone(),
        (*payload).clone(),
        fallback_input_tokens,
        budget,
    )
    .await
    {
        Ok(success) => {
            emit_websearch_loop_usage(provider, &success, client);
            if wants_stream {
                let bytes: Vec<Result<Bytes, Infallible>> =
                    websearch::build_loop_sse_events(&success)
                        .into_iter()
                        .map(|e| Ok(Bytes::from(e.to_sse_string())))
                        .collect();
                Response::builder()
                    .status(StatusCode::OK)
                    .header(header::CONTENT_TYPE, "text/event-stream")
                    .header(header::CACHE_CONTROL, "no-cache")
                    .header(header::CONNECTION, "keep-alive")
                    .body(Body::from_stream(stream::iter(bytes)))
                    .unwrap()
            } else {
                (
                    StatusCode::OK,
                    Json(websearch::build_loop_json_body(&success)),
                )
                    .into_response()
            }
        }
        Err(mut resp) => {
            // 回灌失败响应可能带 x-kirostudio-compress-retry 内部标记（上游 400
            // CONTENT_LENGTH_EXCEEDS 时 map_provider_error 设置；回灌循环在压缩重试/
            // strip 点之前，不在这里清就会透传客户端——与 /v1、/cc/v1 的 F1b 同款，
            // 2026-08-11 对抗审查 M1）。回灌路径**无压缩重试循环**，标记无消费者。
            resp.headers_mut()
                .remove("x-kirostudio-compress-retry");
            resp
        }
    };
    Some(resp)
}

/// WebSearch 回灌成功收尾时埋一条用量记录。
///
/// ⚠️ 诚实边界：回灌链路一次客户端请求对应 **N 次上游往返**，这里只记**一条**记录
/// （末轮的 credential_id + 各轮累计 credits）。所以面板上这条记录的
/// `credits_used` 会明显高于同 input_tokens 的普通请求 —— 那不是记账错误，
/// 而是回灌放大的真实成本。`retries` 借用 rounds-1 表达「多打了几轮」，
/// 它与 provider 的换号重试**不同源**，但都是"额外上游往返"的同一语义。
fn emit_websearch_loop_usage(
    provider: &crate::kiro::provider::KiroProvider,
    success: &websearch::WebSearchLoopSuccess,
    client: &ClientInfo,
) {
    let mut record =
        crate::usage::RequestRecord::new(Uuid::new_v4().to_string(), success.model.clone());
    record.requested_model = Some(success.model.clone());
    // WebSearch 回灌是 MCP 路径（请求体无 modelId，不经过模型映射），upstream 保持 None。
    record.credential_id = Some(success.credential_id);
    record.is_streaming = false;
    record.input_tokens = success.input_tokens;
    record.output_tokens = success.output_tokens;
    record.credits_used = if success.credits > 0.0 {
        Some(success.credits)
    } else {
        None
    };
    record.retries = success.rounds.saturating_sub(1);
    record.outcome = crate::usage::RequestOutcome::Success;
    if let Some(c) = record.credits_used {
        provider.report_credits(success.credential_id, c);
    }
    client.apply(&mut record);
    crate::usage::emit_record(record);
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
        let ua = headers
            .get(header::USER_AGENT)
            .and_then(|v| v.to_str().ok());
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

/// 自适应二次压缩：最大迭代次数（避免极端输入导致过长 CPU 消耗）。
/// 参考仓 ref-mjy/src/anthropic/handlers.rs:25（同值 32）。
const ADAPTIVE_COMPRESSION_MAX_ITERS: usize = 32;
/// tool_result 二次压缩的最低阈值（字符数），不再往下压（过低会破坏内容可用性）。
/// 参考仓 ref-mjy/src/anthropic/handlers.rs:27（同值 512）。
const ADAPTIVE_MIN_TOOL_RESULT_MAX_CHARS: usize = 512;
/// 历史截断保留的成对消息数（保留前 2 对 user+assistant，避免删光上下文）。
/// 参考仓 ref-mjy/src/anthropic/handlers.rs:31（同值 2）。
const ADAPTIVE_HISTORY_PRESERVE_PAIRS: usize = 2;
/// 消息内容二次压缩的最低阈值（字符数）。
/// 参考仓 ref-mjy/src/anthropic/handlers.rs:33（同值 8192）。
const ADAPTIVE_MIN_MESSAGE_CONTENT_MAX_CHARS: usize = 8192;

/// prompt 缓存记账所需的上下文（跟踪器 + 本次请求的缓存画像）
///
/// 构建发往上游的 Kiro 请求体（含输入压缩）。
///
/// 流程：先序列化测量大小；仅当启用压缩且体积超过 `trigger_bytes` 时，对
/// `ConversationState` 跑压缩管道（空白折叠 + tool_result 智能截断）再重新序列化；
/// 若压一次仍超限，则进入**自适应二次压缩循环**（最多 32 轮），逐层降级——
/// tool_result_max_chars×3/4 → 截断超长消息正文 → 清历史图片 → 成对删最老历史——
/// CONTENT_LENGTH_EXCEEDS 压缩重试的目标字节数：`trigger_bytes × (3/4)^attempt`，
/// 逐轮更紧（attempt=1 → 3/4、attempt=2 → 9/16、attempt=3 → 27/64），下限 64 KiB。
///
/// ⚠️ 2026-08-11 对抗审查修：旧公式 `3^(3-attempt+1)/4^(3-attempt+1)` 的序列是
/// 0.42 → 0.56 → 0.75（逐轮**放大**），第 2、3 次重试产出更大的 body 必然再败。
/// 独立成函数并配单元测试（`compress_retry_target_strictly_decreasing_with_floor`），
/// 防再犯。
fn compress_retry_target(trigger_bytes: usize, attempt: u32) -> usize {
    let t = (trigger_bytes as u64)
        .saturating_mul(3u64.saturating_pow(attempt))
        .saturating_div(4u64.saturating_pow(attempt));
    (t as usize).max(65536)
}

/// 每轮重跑压缩管道并重新序列化测量。
///
/// 目标 `max_body` 用 `compression.trigger_bytes`：这是网关发上游前的**出站**软上限，
/// 压到它以内即不会触发上游 ~5MiB 硬限制。刻意**不用** `config.max_body_bytes`——
/// 那是 `router.rs:106` 的**入站** axum `DefaultBodyLimit`（默认 256MiB，管客户端发来的
/// body 多大），语义与「发上游前压缩到多大」无关。
///
/// 保守设计：默认阈值高（4MiB），正常小请求零处理；循环中任何一步出错即停止并返回
/// 当前结果（宁可超限交上游，不 panic）；32 轮后仍超限则照发，由上游判死，
/// 再经 [`map_provider_error`] 透传给客户端。
/// `target_bytes`: 压缩目标字节数。None = 使用 `compression.trigger_bytes`。CONTENT_LENGTH_EXCEEDS
/// 重试时传入更小的目标值，实现渐进式压缩。
fn build_kiro_request_body(
    conversation_state: crate::kiro::model::requests::conversation::ConversationState,
    additional_model_request_fields: Option<
        crate::kiro::model::requests::kiro::AdditionalModelRequestFields,
    >,
    compression: &crate::model::config::CompressionConfig,
    target_bytes: Option<usize>,
) -> Result<String, serde_json::Error> {
    let max_body = target_bytes.unwrap_or(compression.trigger_bytes);
    let mut kiro_request = KiroRequest {
        conversation_state,
        profile_arn: None,
        additional_model_request_fields,
    };

    let body = serde_json::to_string(&kiro_request)?;

    if !compression.enabled || body.len() <= max_body {
        return Ok(body);
    }

    let before = body.len();
    let stats = super::compressor::compress(&mut kiro_request.conversation_state, compression);
    let mut request_body = serde_json::to_string(&kiro_request)?;

    if request_body.len() > max_body {
        adaptive_compress_loop(&mut kiro_request, compression, &mut request_body, target_bytes)?;
    }

    tracing::info!(
        before_bytes = before,
        after_bytes = request_body.len(),
        saved_bytes = stats.total_saved(),
        trigger_bytes = max_body,
        "请求体超过压缩阈值，已执行输入压缩"
    );

    Ok(request_body)
}

/// 自适应二次压缩：序列化后仍超 `trigger_bytes` 时，按参考仓降级顺序迭代重压，
/// 每轮递减阈值 → 重跑压缩管道（复用 [`super::compressor::compress`]）→ 重新序列化。
///
/// 降级顺序（参考 ref-mjy/src/anthropic/handlers.rs:265-270，逐条对照）：
/// 1. tool_result_max_chars ×3/4（仅当存在 tool_result/tools）
/// 2. tool_use input ×3/4 —— 我方无 `tool_use_input_max_chars` 配置，映射为继续压
///    tool_result（同一 `compress_tool_results_pass`），语义等价
/// 3. 截断超长用户消息正文（仅当单条消息本身超过阈值 / 历史已删到只剩保留对）
/// 4. 清一次历史图片（保留 current_message 图片）
/// 5. 成对删最老 user+assistant 历史（保留前 2 对）
///
/// 与参考仓的差异：3/4/5 层**并列执行**而非 else-if 单选（参考仓 ref-mjy/handlers.rs:397-434
/// 存在死角：正文短而历史图片大时，L3 `saved=0` 会让循环 `break`，L4/L5 永远轮不到）。
/// 每轮最多触发一层为 true（正文短时 L3 无 saved），故单轮放大倍数与参考仓一致。
///
/// fail-safe：循环内任何一步出错立即返回当前 `request_body`（Er），不 panic。
fn adaptive_compress_loop(
    kiro_request: &mut KiroRequest,
    compression: &crate::model::config::CompressionConfig,
    request_body: &mut String,
    target_bytes: Option<usize>,
) -> Result<(), serde_json::Error> {
    let max_body = target_bytes.unwrap_or(compression.trigger_bytes);

    // 守卫（对齐参考仓 ref-mjy/handlers.rs:251）：禁用压缩或阈值为 0（不限）时一律不动。
    // 必须在函数内部再查一次 `enabled`——L3/L4/L5（截正文/清历史图片/删历史）都在
    // `compressor::compress` **之外**，它们不看 `config.enabled`；只靠调用方守卫的话，
    // 将来任何新调用点漏查就会在用户显式关掉压缩时静默丢历史。
    if !compression.enabled || max_body == 0 {
        return Ok(());
    }

    // 是否存在任何 tool_result / tools（否则降阈值只会浪费迭代）
    let has_any_tool_results_or_tools = {
        let state = &kiro_request.conversation_state;
        let current = &state.current_message.user_input_message.user_input_message_context;
        !current.tool_results.is_empty()
            || !current.tools.is_empty()
            || state.history.iter().any(|msg| match msg {
                crate::kiro::model::requests::conversation::Message::User(u) => {
                    !u.user_input_message
                        .user_input_message_context
                        .tool_results
                        .is_empty()
                        || !u.user_input_message.user_input_message_context.tools.is_empty()
                }
                _ => false,
            })
    };
    // 是否存在历史图片（否则无需尝试图片降级）
    let has_history_images = kiro_request
        .conversation_state
        .history
        .iter()
        .any(|msg| match msg {
            crate::kiro::model::requests::conversation::Message::User(u) => {
                !u.user_input_message.images.is_empty()
            }
            _ => false,
        });
    // 是否存在历史（否则删除层无意义）
    let has_history = !kiro_request.conversation_state.history.is_empty();

    // 初始 message_content_max_chars = 最大消息字符数×3/4，下限 ADAPTIVE_MIN_MESSAGE_CONTENT_MAX_CHARS
    let max_content_chars = {
        let mut max_chars = kiro_request
            .conversation_state
            .current_message
            .user_input_message
            .content
            .chars()
            .count();
        for msg in &kiro_request.conversation_state.history {
            if let crate::kiro::model::requests::conversation::Message::User(u) = msg {
                max_chars = max_chars.max(u.user_input_message.content.chars().count());
            }
        }
        max_chars
    };
    let mut message_content_max_chars =
        (max_content_chars * 3 / 4).max(ADAPTIVE_MIN_MESSAGE_CONTENT_MAX_CHARS);

    let mut adaptive_config = compression.clone();
    let mut history_images_removed = false;

    for _ in 0..ADAPTIVE_COMPRESSION_MAX_ITERS {
        if request_body.len() <= max_body {
            break;
        }

        let mut changed = false;

        if has_any_tool_results_or_tools
            && adaptive_config.tool_result_max_chars > ADAPTIVE_MIN_TOOL_RESULT_MAX_CHARS
        {
            // 第 1 层（L2 映射）：降低 tool_result 截断阈值
            let next = (adaptive_config.tool_result_max_chars * 3 / 4)
                .max(ADAPTIVE_MIN_TOOL_RESULT_MAX_CHARS);
            if next < adaptive_config.tool_result_max_chars {
                adaptive_config.tool_result_max_chars = next;
                changed = true;
            }
        } else {
            // 若任意单条 user content 已超 max_body，删历史救不回来，必须优先截断正文。
            let max_single_user_content_bytes = {
                let state = &kiro_request.conversation_state;
                let mut max_bytes = state.current_message.user_input_message.content.len();
                for msg in &state.history {
                    if let crate::kiro::model::requests::conversation::Message::User(u) = msg {
                        max_bytes = max_bytes.max(u.user_input_message.content.len());
                    }
                }
                max_bytes
            };

            // 只取长度（值），不持 `&mut history` 长借用：L3 要传 `&mut conversation_state`，
            // 而 L5 之后还要用 history ⇒ 长借用会撞 E0499。参考仓能编过只因它是 else-if
            // 单选（L3 路径上 history 之后不再被用），我方改成并列后必须避开这个借用。
            let history_len = kiro_request.conversation_state.history.len();
            if (max_single_user_content_bytes > max_body
                || history_len <= (ADAPTIVE_HISTORY_PRESERVE_PAIRS * 2) + 2)
                && message_content_max_chars >= ADAPTIVE_MIN_MESSAGE_CONTENT_MAX_CHARS
            {
                // 第 3 层：截断超长消息正文（参考 ref-mjy/compressor.rs:690 同名函数）
                let saved = super::compressor::compress_long_messages_pass(
                    &mut kiro_request.conversation_state,
                    message_content_max_chars,
                );
                if saved > 0 {
                    changed = true;
                }
                message_content_max_chars = (message_content_max_chars * 3 / 4)
                    .max(ADAPTIVE_MIN_MESSAGE_CONTENT_MAX_CHARS);
            }
            // 第 4/5 层：清历史图片 → 删历史。与第 3 层**并列**（不是 else-if）：
            // 参考仓的 else-if 单选存在死角——当正文都很短（`saved=0`）而历史图片很大时，
            // 上一轮会命中 L3、`changed=false`、整个循环 break，L4/L5 永远轮不到。
            // 正文短（不触发 L3 的 saved）时降级链必须继续往下走，否则纯图片请求压不动。
            if !history_images_removed && has_history_images {
                // 第 4 层：仅清一次历史图片（参考 ref-mjy/conversation.rs:83 remove_history_images）
                let removed = kiro_request.conversation_state.remove_history_images();
                if removed > 0 {
                    history_images_removed = true;
                    changed = true;
                }
            }
            if has_history && history_len > ADAPTIVE_HISTORY_PRESERVE_PAIRS * 2 + 2 {
                // 第 5 层：成对删最老 user+assistant（保留前 2 对），单轮最多删 16 条。
                // 先取历史长度，再单独 `&mut`（避免跨 L3 的 `&mut conversation_state` 长借用）。
                let removable = history_len.saturating_sub(ADAPTIVE_HISTORY_PRESERVE_PAIRS * 2 + 2);
                let mut remove_msgs = removable.min(16);
                remove_msgs -= remove_msgs % 2; // 保持成对
                if remove_msgs > 0 {
                    let history = &mut kiro_request.conversation_state.history;
                    history.drain(ADAPTIVE_HISTORY_PRESERVE_PAIRS * 2..ADAPTIVE_HISTORY_PRESERVE_PAIRS * 2 + remove_msgs);
                    changed = true;
                }
            }
        }

        if !changed {
            // 没有可再降的层了，继续循环也不会变小，直接返回当前结果
            break;
        }

        // 重跑压缩管道 + 重新序列化（本仓 `compress` 内部含空 content 兜底修复，
        // 故截断正文 / 删历史后不会因空 content 触发上游 400）
        super::compressor::compress(&mut kiro_request.conversation_state, &adaptive_config);
        *request_body = serde_json::to_string(kiro_request)?;
    }

    Ok(())
}

/// 已翻译的上游错误：HTTP 状态 + Anthropic 错误类型码 + 面向用户的中文消息（含排障步骤）。
struct TranslatedError {
    status: StatusCode,
    error_type: &'static str,
    message: String,
    /// 网关可以在更激进的压缩下重试（CONTENT_LENGTH_EXCEEDS_THRESHOLD 等可自愈错误）。
    /// 对应的响应带上 `x-kirostudio-compress-retry` 头，handler 据此决定是否重新压缩并重试。
    retry_compress: bool,
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
pub(crate) fn is_upstream_rate_limited(err_str: &str) -> bool {
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
pub(crate) fn is_upstream_temporarily_suspended(err_str: &str) -> bool {
    err_str.contains("temporarily is suspended") || err_str.contains("TEMPORARILY_SUSPENDED")
}

/// 403 临时风控的建议退避秒数。
///
/// 取 20 与 `cooldown.rs` 的 `CooldownReason::SuspiciousActivity`（20s）同源 ——
/// 那是本仓对「这个状态持续多久」的既有判断，复用它而不是另立一个数字，
/// 避免同一语义在两处各有一套时长。
const UPSTREAM_SUSPENDED_RETRY_AFTER_SECS: u64 = 20;

/// provider 打在「bearer-invalid 但该号已成功过」那条 bail 串上的机器可读标记。
///
/// 逐字节与 `provider.rs` 侧一致。用标记而非中文文案，理由同
/// `pool_permanently_exhausted=1`：文案改动不该让分类失效。
pub(crate) const BEARER_INVALID_TRANSIENT_MARKER: &str = "bearer_invalid_transient=1";

/// 上游 **403 region 错配**（`The bearer token included in the request is invalid`）。
///
/// # 为什么必须单独一条
///
/// 上游原文（实测）：
/// ```text
/// 403 Forbidden {"__type":"com.amazon.aws.codewhisperer#AccessDeniedException",
///  "message":"The bearer token included in the request is invalid."}
/// ```
///
/// 这个串**匹配不上 `map_provider_error` 的任何分支**：不带 `retry_after_secs=`、
/// 不含 `USER_REQUEST_RATE_EXCEEDED` / `Too many requests` / `temporarily is suspended`，
/// 也不含 `translate_quota_subscription` 认的 `Invalid token`（那条要求首字母大写的
/// `Invalid token`，而上游写的是句末 `is invalid.`）→ 落函数末尾兜底 →
/// **502 且无 Retry-After**。实测 397 次全部走的这条路。
///
/// 而 502 对它是**错的方向**：`ksk_` token 按 region 授权，打错区恒 403，
/// 这既不是服务端故障、也不是「稍后会好」。上游/外挂（`kiro_shield.py` 的
/// `RETRYABLE={429,500,502,503,504}`）看见 5xx 会按服务器错误盲退避重打，
/// 而正确处置是**改这个号的 region**（或让网关的 region 探测重选）——
/// 重试多少次都不会变。故映射成 403 `permission_error` 且**不带 Retry-After**：
/// 4xx 不在外挂的重试集内，客户端立刻拿到诚实结论，管理员也能从文案看到真实动作。
///
/// # 判据为何要窄，以及为何复用 endpoint 侧的谓词
///
/// 字符串判据直接调 [`crate::kiro::endpoint::default_is_bearer_token_invalid`] ——
/// 那是 provider「要不要强制刷新 / 要不要判瞬态」用的**同一个**谓词
/// （`provider.rs` 的 `endpoint.is_bearer_token_invalid(&body)`）。不在这里新写一份
/// 子串匹配：新写一套必然与那侧漂移，而「同一个 403 两处结论相反」正是本仓已经
/// 发生过的事故（见 HANDOFF-2026-08-04 §2.1：`temporarily is suspended`
/// 在 handlers 认、在 endpoint 不认）。
///
/// **绝不**泛匹配 `AccessDeniedException` 或裸 403：那会把「账号真被永久封禁」
/// 也归成 region 问题，给出错误的排障动作，同时与
/// `is_upstream_temporarily_suspended` 的窄判据（`:548` 一带写明了理由）互相拆台。
///
/// # 顺序（承重）
///
/// - **401 必须让路**：同一响应体可能同时提两个码，而 401 的含义是「token 本身死了」，
///   处置是刷新/换号而不是改 region。判据显式排除 401，与 `region_probe.rs`
///   `classify_probe_result` 的「401 必须排在 403 之前判」同源 —— 那是本仓对
///   **同一个分类问题**已经定下的顺序，这里照抄而不是另立一套。
/// - **429 必须优先**：由 `map_provider_error` 的分支顺序保证
///   （`is_upstream_rate_limited` 与全池冷却都在本条之前），本条不做重复判断。
///
/// 状态码用裸 `403` 子串匹配（同 `region_probe.rs`），而非
/// `is_upstream_transient_5xx` 那种「必须带完整 HTTP 语境」的写法：这里已经有
/// bearer-invalid 那句确切文案当主判据，`403` 只是辅助定位状态码。
/// 代价是响应体里的 `requestId` 恰好含 `401` 时会**漏判**（退回旧的 502 兜底行为）——
/// 方向上是安全的那一侧：漏判只是少修一次，误判会给出错误的排障动作。
///
/// # 为什么必须排除 provider 的瞬态标记（🔴 收窄，2026-08-06）
///
/// 同一句 bearer-invalid 文案，provider 自己已经分成了两类
/// （`provider.rs` 的 `bearer_invalid_but_proven`，判据是 `has_ever_succeeded`）：
/// - **从未成功过**的号 → 大概率真 region 错配（实测 3 个号共吃 17 次）；
/// - **已成功过**的号 → token 对该端点证明有效，403 只能是抖动
///   （实测 4 个号累计 3393 次成功、共吃 42 次这种 403）。
///
/// 即按本仓自己的取证，这个串的**多数出现不是 region 错配**。此前本判据只看
/// 「bearer-invalid + 403 + 无 401」，于是把瞬态那一类也吞了，两个后果：
/// ① 排障文案让管理员去查 region，而那个号的 region 是对的；
/// ② 状态码从 502 变 403 —— 502 在外挂 `kiro_shield.py` 的
/// `RETRYABLE={429,500,502,503,504}` 内会被重试，403 是 4xx 不重试。而瞬态那一类
/// 下一次重试大概率落到别的号上成功（实测 #481 成功率 93.9%）⇒ 收窄之后这类退回
/// 兜底的 502/可重试路径，是**恢复**了本该有的重试机会。
///
/// 判据用 provider 那条 bail 串里的机器可读标记 `bearer_invalid_transient=1`
/// （与既有 `pool_permanently_exhausted=1` / `model_unsupported_by_pool=1` 同款范式），
/// 不按中文文案匹配：文案改动不该让分类失效，那正是本类缺陷反复出现的成因。
pub(crate) fn is_upstream_region_mismatch_403(err_str: &str) -> bool {
    if !crate::kiro::endpoint::default_is_bearer_token_invalid(err_str) {
        return false;
    }
    // provider 已判为瞬态抖动（该号成功过）→ 不是 region 错配，让它退回可重试路径。
    // 必须排在 403 语境判断之前：瞬态那条 bail 串本身就带 `403 Forbidden`。
    if err_str.contains(BEARER_INVALID_TRANSIENT_MARKER) {
        return false;
    }
    let low = err_str.to_ascii_lowercase();
    // 401 让路：token 死了 ≠ region 错了，两者处置动作不同。
    if low.contains("401") || low.contains("认证失败") {
        return false;
    }
    // 要求 403 语境（provider 把 `StatusCode` 原样 Display 成 `403 Forbidden`），
    // 不是只看那句 message —— 同一句话若出现在别的状态码下，含义未必是授权层拒绝。
    low.contains("403")
}

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
    // 🔴 **全池配额耗尽**：只认 provider 打的显式标记（2026-08-10 收口）。
    //
    // 改前这里是裸串 `contains("MONTHLY_REQUEST_COUNT") || contains("QUOTA")`，而那两个串
    // 来自**上游 body**：单号耗尽时 provider 走的是「换号 continue」分支，它的 `last_error`
    // 同样带着上游 body（含这两个串），且 `last_error` 是**刻意不重置**的 ⇒ 池里其余号
    // 明明健康、最终却因为链上某一跳的残留错误被判成"全部配额耗尽"，归因口径被污染。
    //
    // 现在与 `pool_permanently_exhausted=1` / `model_unsupported_by_pool=1` 同款：
    // 只信 provider 在**确认 `has_available == false`** 后才打的 `quota_exhausted_all=1`。
    if err_str.contains("quota_exhausted_all=1") {
        return Some(TranslatedError {
            retry_compress: false,
            status: StatusCode::TOO_MANY_REQUESTS,
            error_type: "rate_limit_error",
            message: "月度请求配额已耗尽（号池内所有凭据）。排障：①面板查看各凭据用量；②等待配额周期重置；③为号池补充新凭据。".to_string(),
        });
    }
    // 兜底：仍保留裸串识别，但**降级为「单号/未知范围」的配额语义**。
    //
    // 为什么不能直接删掉（这是本次收口最容易做错的地方）：并非所有配额错误都经过
    // 上面那个标记 —— MCP 路径（`call_mcp_with_retry`）、透传路径、以及未来新增的
    // 上游分支都可能把带 `MONTHLY_REQUEST_COUNT` 的 body 冒泡上来。删掉裸串会让它们
    // 落 `map_provider_error` 末尾兜底 → **502 无 Retry-After** → 客户端当永久故障、
    // 不退避、原样重发（这正是本仓反复踩过的那类回归）。
    //
    // 保留但**改文案**：不再断言"所有凭据"（那是标记分支才能确认的事实），
    // 避免面板/用户按错误的范围去排障。状态码维持 429（可退避）不变。
    if err_str.contains("MONTHLY_REQUEST_COUNT") || err_str.contains("QUOTA") {
        return Some(TranslatedError {
            retry_compress: false,
            status: StatusCode::TOO_MANY_REQUESTS,
            error_type: "rate_limit_error",
            message: "请求配额已耗尽。排障：①面板查看各凭据用量，切到仍有额度的账号；②等待配额周期重置；③为号池补充新凭据。".to_string(),
        });
    }
    // 上游容量紧张/模型短暂不可用：临时状态，稍后重试即可（常见于新模型发布初期）。
    //
    // 两个字面量是**同一语义的两种上游形态**（判据同款收口在
    // `endpoint::default_is_model_temporarily_unavailable`）：
    //   · 503 `MODEL_TEMPORARILY_UNAVAILABLE`
    //   · 400 `ThrottlingException` + `reason:INSUFFICIENT_MODEL_CAPACITY`（实测 24h 272 次）
    //
    // 后者此前不命中**任何**分支 → 落 `map_provider_error` 末尾兜底 → **502 无 Retry-After**
    // → 客户端当永久故障、不退避、原样重发。归到这里后与前者同样返 503 `overloaded_error`，
    // 那是客户端会退避重试的形态。
    if err_str.contains("MODEL_TEMPORARILY_UNAVAILABLE")
        || err_str.contains("INSUFFICIENT_MODEL_CAPACITY")
    {
        return Some(TranslatedError {
            retry_compress: false,
            status: StatusCode::SERVICE_UNAVAILABLE,
            error_type: "overloaded_error",
            message: "上游模型暂时不可用（负载过高），请稍后重试。若持续出现：①换用同族其他版本（如 claude-opus-4.8）；②新发布模型发布初期容量有限，属正常现象，等待 1~2 小时后通常恢复。".to_string(),
        });
    }
    if err_str.contains("FEATURE_NOT_SUPPORTED") {
        return Some(TranslatedError {
            retry_compress: false,
            status: StatusCode::BAD_GATEWAY,
            error_type: "api_error",
            message: "当前凭据所在 region 未开通该功能（profile 未激活）。排障：①网关会在刷新时自动验活重选可用 region；②如持续，右键该凭据切换 Profile ARN 到已开通 region（如 eu-central-1）；③确认该账号确在某 region 开通了 Kiro。".to_string(),
        });
    }
    // ⚠️ "Improperly formed" **不在**凭据分支里（2026-08-11 对抗审查 M1）：
    // 上游对**用户请求体**的格式校验失败（工具 schema 属性、工具名超限、web_search 直发
    // 等，converter.rs/websearch.rs 多处实测记录）也回 `400 Improperly formed request`，
    // 且常带 `reason=REQUEST_BODY_INVALID`。混在这里会被说成「订阅失效/token 无效」——
    // 排障方向全错。它由 `translate_context_input` 里的请求体校验分支接管（400
    // invalid_request_error）。真正的凭据信号是 403 `Invalid token`（map_provider_error
    // 更前面的 403 分支处理）与 `subscription` 类配额文案。
    if err_str.contains("Invalid token")
        || err_str.contains("subscription")
    {
        return Some(TranslatedError {
            retry_compress: false,
            status: StatusCode::BAD_GATEWAY,
            error_type: "api_error",
            message: "上游拒绝凭据（订阅失效或 token 无效）。排障：①面板对该凭据点『刷新 Token』；②若为 Enterprise/IdC 号，确认 profileArn 已正确解析；③测活确认订阅有效，失效则更换凭据。".to_string(),
        });
    }
    None
}

/// 「请求装不下」类错误对外 message 的**英文哨兵前缀** —— 这是给**外部消费者**看的契约，
/// 不是给人读的文案。
///
/// # 为什么必须存在（2026-08-06 实测，Claude Code 本机二进制 2.1.220）
///
/// Claude Code 有两条压缩路径，网关模式下**只有第二条能用**：
///
/// 1. **反应式 auto-compact**（按 token 水位主动压）—— 入口有一道门：解析「上下文窗口」时
///    若最终落到兜底档（六档优先级里的最后一档），该门直接 return false ⇒ **永不压缩**。
///    那六档里唯一可能替网关发声的一档要求把窗口写进本地 bootstrap 缓存，而网关
///    自身不实现那个 bootstrap 端点 ⇒ 该档恒空。详见
///    `docs/auto-compact-fix-2026-08-06.md`。
/// 2. **compact-and-retry**（撞到「装不下」后压缩再重试）—— 它的判据是对错误 message 做
///    **小写化子串匹配**（形如 `msg.toLowerCase().includes("prompt is too long")
///    || includes("input is too long for requested model")`），**与上面那道门无关**
///    （实测其前置条件只有「auto-compact 总开关开」+「非远端会话」两项）。
///
/// ⇒ 服务端唯一能做的补救就是让「装不下」类错误的 message **含**那个子串。前缀而非替换：
/// 后面的中文排障文案是给人读的，两者各服务一个受众。
///
/// ⚠️ **改这两条文案时必须保留这个前缀**。删掉它不会有任何编译或运行期报错，只会让用户的
/// 自动压缩静默失效（撞满上下文后直接报错而不是压缩重试）—— 正是那种「没人会注意到」的失效。
/// 承重测试 `overflow_errors_must_match_claude_code_compact_retry_predicate` 钉住它，
/// 那条测试刻意写**字面量**而不引用本常量（引用了就变成同义反复，删前缀照样绿）。
///
/// ⚠️ 上面的机制是从某一个 build 抽出来的：**符号名会随版本漂移**（故此处不记符号名），
/// 但「小写子串匹配」这个判据形态是稳定的可观测事实。
const OVERFLOW_COMPACT_HINT: &str = "prompt is too long";

/// 上下文/输入体积类（不可重试，需减小请求）。
fn translate_context_input(err_str: &str) -> Option<TranslatedError> {
    // 图片声明格式与实际字节不符（400 `IMAGE_MIME_MISMATCH`，用户线上实测）。
    //
    // 状态码保持 400 `invalid_request_error`：这确实是**请求构造**问题，重试/换号无意义
    // （与通用 400 同处置）。单列一条的价值在**度量**：`converter.rs` 已按 magic bytes
    // 校正声明的 media_type，但若仍有边缘情况漏掉，那些 400 混进通用 `bad_request` 桶
    // 后在面板上不可分辨 ⇒ 无法回答「那条修干净了没有」。判据收口在
    // `endpoint::default_is_image_mime_mismatch`（`default_is_*` 系列的家），
    // 不在此处新写子串匹配 —— 两处各写一份必然漂移。
    //
    // 位置：在 `translate_quota_subscription` **之后**（`.or_else` 链的顺序保证）。
    // 那条链里的容量判据 `INSUFFICIENT_MODEL_CAPACITY` **也是 400**，且必须拿 503
    // `overloaded_error`（可退避重试）。顺序反了就把「上游没容量」说成「你的图片格式错」，
    // 既误导用户、又让客户端不再退避。
    if crate::kiro::endpoint::default_is_image_mime_mismatch(err_str) {
        return Some(TranslatedError {
            retry_compress: false,
            status: StatusCode::BAD_REQUEST,
            error_type: "invalid_request_error",
            message: "图片声明的 media_type 与实际字节格式不符（上游 IMAGE_MIME_MISMATCH）。这是请求构造问题，重试无效。排障：①按图片真实格式填写 media_type（如 JPEG 字节不要声明 image/png）；②不要在改扩展名后沿用旧的 media_type；③重新读取并重新编码该图片后再发。".to_string(),
        });
    }
    // 请求体校验失败（400 `REQUEST_BODY_INVALID` / `Invalid tool use format`，2026-08-11 补）。
    //
    // 改前：该错误码零翻译，落「未识别兜底」502 —— 性质说错（这是请求构造问题不是网关
    // 故障），且会被外挂 RETRYABLE 集（502 在列）反复重打同一个必败的请求。
    // 翻成 400 `invalid_request_error` 与 IMAGE_MIME_MISMATCH 同款：请求构造问题，
    // 重试/换号无意义。判据收口在 `endpoint::default_is_request_body_invalid`
    // （含 region 探测边界警告，见该谓词 doc —— 探测走独立通道，不会被打到这里）。
    if crate::kiro::endpoint::default_is_request_body_invalid(err_str) {
        return Some(TranslatedError {
            retry_compress: false,
            status: StatusCode::BAD_REQUEST,
            error_type: "invalid_request_error",
            message: "请求体校验失败（上游 REQUEST_BODY_INVALID）。这是请求构造问题，重试无效。排障：①检查工具调用与工具结果的配对（上游对 tool 配对较严，截断/重排序会产生孤儿 tool_use）；②检查消息 role 与内容字段合法性；③重新构造请求后再发。".to_string(),
        });
    }
    // 两条都带 `OVERFLOW_COMPACT_HINT` 前缀：状态码与中文文案一字未改，只在最前面挂哨兵，
    // 让 Claude Code 的 compact-and-retry 认出「这是装不下，压缩后重试还有戏」。
    // 不改 400：这确实是「请求本身太大」，重试原请求无意义 —— 客户端要做的是**先压缩再重试**，
    // 而它认的正是 message 而非状态码（实测那条判据只看 message 子串）。
    if err_str.contains("CONTENT_LENGTH_EXCEEDS_THRESHOLD") {
        return Some(TranslatedError {
            retry_compress: true,
            status: StatusCode::BAD_REQUEST,
            error_type: "invalid_request_error",
            message: format!(
                "{OVERFLOW_COMPACT_HINT}: 上下文窗口已满（对话历史累积超出模型上下文上限）。排障：①精简对话历史或开新会话；②缩短 system prompt；③减少同时挂载的工具数量。"
            ),
        });
    }
    if err_str.contains("Input is too long") {
        return Some(TranslatedError {
            retry_compress: true,
            status: StatusCode::BAD_REQUEST,
            error_type: "invalid_request_error",
            message: format!(
                "{OVERFLOW_COMPACT_HINT}: 单次输入过长（请求体本身超出上游限制）。排障：①拆分过大的消息或附件；②减少一次性粘贴的文件内容；③对超大工具结果先做摘要。"
            ),
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

/// 上游 **5xx 或传输层失败** —— 明确可重试的瞬态错误。
///
/// 用途：让这两类不再落 `map_provider_error` 末尾的「未识别兜底」（502 无 Retry-After）。
///
/// 判据刻意**只认四个确切的 HTTP 5xx 字样** + [`is_transport_error`]，绝不泛匹配
/// 「含 5 开头的三位数」：上游错误体里带 `requestId` / 计数 / 时间戳时很容易出现
/// `500` 之类的片段，泛匹配会把 4xx（配额耗尽、封号、参数错）误判成可重试 →
/// 客户端对永久错误无限退避重试，正是本仓反复出现的那类缺陷。
///
/// 与 provider 内部「换不换号」的判据是**两回事**：这里只决定回给客户端的状态码。
fn is_upstream_transient_5xx(err_str: &str) -> bool {
    let low = err_str.to_ascii_lowercase();
    if is_transport_error(&low) {
        return true;
    }
    // 必须带 HTTP 语境（"500 internal server error" 这种完整形态），不裸匹配数字。
    low.contains("500 internal server error")
        || low.contains("502 bad gateway")
        || low.contains("503 service unavailable")
        || low.contains("504 gateway timeout")
        || low.contains("internalserverexception")
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
    if low.contains("dns")
        || low.contains("resolve")
        || low.contains("name resolution")
        || low.contains("failed to lookup")
    {
        return Some(TranslatedError {
            retry_compress: false,
            status: StatusCode::BAD_GATEWAY,
            error_type: "api_error",
            message: "DNS 解析失败（无法解析上游域名）。排障：①检查本机/容器 DNS 配置；②若走代理，确认代理能解析 kiro.dev；③确认网络出口正常。".to_string(),
        });
    }
    if low.contains("timed out") || low.contains("timeout") {
        return Some(TranslatedError {
            retry_compress: false,
            status: StatusCode::GATEWAY_TIMEOUT,
            error_type: "api_error",
            message: "连接上游超时。排障：①上游或代理可能拥塞，稍后重试；②检查代理延迟；③大请求可拆小以缩短单次耗时。".to_string(),
        });
    }
    if low.contains("certificate") || low.contains("ssl") || low.contains("tls") {
        return Some(TranslatedError {
            retry_compress: false,
            status: StatusCode::BAD_GATEWAY,
            error_type: "api_error",
            message: "TLS/证书握手失败。排障：①检查系统时间是否准确；②若走中间人代理，确认其证书受信；③确认未误用被拦截的代理。".to_string(),
        });
    }
    if low.contains("proxy") {
        return Some(TranslatedError {
            retry_compress: false,
            status: StatusCode::BAD_GATEWAY,
            error_type: "api_error",
            message: "代理连接失败。排障：①检查代理地址/账密是否正确；②确认代理在线可达；③面板核对该凭据绑定的代理配置。".to_string(),
        });
    }
    None
}

/// 从错误串里取 `retry_after_secs=N` 的 N。
///
/// 抽出成公共函数的理由：这段解析此前在 `map_provider_error` 内**复制了两份**（准入超时分支
/// 与全池冷却分支各一份），而内置吸收层需要第三份。同一逻辑各写一份正是本仓漏改事故的形态
/// （见 `update.rs` 的 chunked 缺口：第一轮只改了两处中的一处）。三个调用点共用一份，
/// 消掉漂移面本身，而不是靠测试去比对两份拷贝是否仍然一致。
pub(crate) fn parse_retry_after_secs(err_str: &str) -> Option<u64> {
    err_str
        .split("retry_after_secs=")
        .nth(1)
        .and_then(|rest| rest.split(|c: char| !c.is_ascii_digit()).next())
        .and_then(|d| d.parse::<u64>().ok())
}

/// 内置「上游 429 吸收层」的可吸收类别。`None` = 不可吸收（详见 [`absorb_class_of`]）。
///
/// # 与外挂 `kiro_shield.py` 的类别对照（合并的依据）
///
/// | 外挂那侧 | 本枚举 | 退避节奏 |
/// |---|---|---|
/// | `RETRYABLE` 里的 429 | [`Self::UpstreamRateLimit`] | 指数（min_delay 起） |
/// | `Retry-After` 头（它只看 HTTP 头） | [`Self::PoolCooldown`] | **号池进程内真值**，不需 HTTP 头往返 |
/// | `SWAP_WINDOW_MARKERS` | [`Self::SwapWindow`] | 长阶梯 20/40/60s（需显式开预算） |
/// | `RETRYABLE` 里的 5xx | [`Self::TransientServerError`] | 1s 起指数 |
/// | 瞬态 400 标记白名单 | [`Self::TransientCapacity400`] | 中等（2s 起） |
///
/// 外挂白名单里未被本枚举覆盖的：`ServiceUnavailable` / `InternalFailure` / `SlowDown` /
/// 裸 `ThrottlingException`。前三个本仓没有既有谓词、也没有实测样本证明它们真出现过
/// （`InternalServerException` 已被 [`is_upstream_transient_5xx`] 覆盖）；裸
/// `ThrottlingException` 是**刻意不认**——那个 `__type` 被真限流共用，详见
/// `endpoint::default_is_model_temporarily_unavailable` 处的说明。新写一套字符串匹配
/// 去覆盖它们，必然与渲染侧漂移，而那正是本仓反复出现的缺陷成因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AbsorbClass {
    /// 全池冷却 / 整池 RPM 饱和 / 池真耗尽：`retry_after_secs=N` 带号池算出的**真实**恢复秒数。
    PoolCooldown(u64),
    /// 上游账户级速率限流（`USER_REQUEST_RATE_EXCEEDED` 一类）。可重试。
    UpstreamRateLimit,
    /// 403 账户级**临时风控** = 外挂所称的「换号空窗」：账号被风控 → 网关 auto_disable →
    /// 切下一个凭据 → 推送补号，实测空窗约 **10 分钟**，期间该账号的请求全是 403。
    ///
    /// 默认**不吸收**（`upstream_retry_absorb_suspended`）；开启后若还想要长阶梯节奏，
    /// 需另设 `upstream_retry_absorb_swap_budget_secs`。
    ///
    /// ⚠️ 与 [`Self::PoolCooldown`] **必须严格分开**，且那条判据排在前面（见
    /// [`absorb_class_of`] 的顺序说明）：外挂 2026-08-04 踩过的坑正是把
    /// `"All credentials"` 挂进 `SWAP_WINDOW_MARKERS` → 全池冷却被套上长阶梯 →
    /// 本该等 10 秒（网关明确给了 `Retry-After: 10`）的等了几十秒。
    /// 号池冷却**必须听网关真值**，换号空窗才用长阶梯。
    SwapWindow,
    /// 上游 **5xx**（网关/上游抖动）。默认**不吸收**，见 `upstream_retry_absorb_server_error`。
    ///
    /// 刻意**不含传输层**失败：连不上上游时 provider 内部换号已把每个号各试一遍，
    /// 吸收层再套一层只是把同一个网络故障重打 N 遍。
    TransientServerError,
    /// 带**瞬态标记的 400**（模型容量不足）。默认**不吸收**，见 `upstream_retry_absorb_capacity_400`。
    ///
    /// 上游把这类瞬态故障塞进 400，与「请求写错了」同一个状态码，故判据必须窄到只认
    /// 既有谓词认的那两个 reason 字面量，其余 400 一律透传。
    TransientCapacity400,
}

/// 吸收层分类器：判据完全复用 `map_provider_error` 的既有谓词，不新写字符串匹配
/// （新写一套必然与渲染侧漂移，那正是「所有凭据均已禁用落 502」的成因）。
///
/// ⚠️ 分支顺序是承重的，两处不能调换：
/// 1. **准入超时必须最先判且返 `None`**。它与全池冷却共用 `retry_after_secs=` 标记，
///    但语义正好相反：全池冷却是「上游没准备好，等等真的会好」，而准入超时是「网关自己在
///    限流保护上游」—— 重试只是把同一个请求塞回同一个已经满的桶，队列更长、客户端等更久，
///    且拿不到任何额外成功概率。下沉架构下这条串结构上到不了吸收层（provider 的 bail 在
///    吸收循环之外），显式列出是为了防将来有人把准入闸门移进循环。
/// 2. **`model_unsupported_by_pool=1` 必须排在 `retry_after_secs=` 之前**。号池对该模型是
///    **永久**不可用，重试无效（吸收它等于把 404 死循环搬进网关）；而「模型级过滤但可恢复」
///    那条 bail **带** `retry_after_secs=`。顺序反了就把永久态当可恢复态吸收。
/// 3. **`PoolCooldown`（`retry_after_secs=`）必须排在 `SwapWindow` 之前**。两者都可能出现在
///    同一个 403 语境里（全池冷却的 bail 串与被风控账号的响应体都能提到 suspend 字样），
///    而处置**相反**：冷却听网关算出的真值（常是个位数秒），换号空窗走 20~60s 长阶梯。
///    外挂 2026-08-04 就是把 `"All credentials"` 挂进 `SWAP_WINDOW_MARKERS` 才踩的坑 ——
///    本该等 10 秒的等了几十秒。
/// 4. **`TransientCapacity400` 必须排在 `TransientServerError` 之前**。容量类的一种上游形态是
///    `503 Service Unavailable`（另一种是 400），而 5xx 判据认那句 `503 service unavailable`
///    字样 ⇒ 顺序反了，容量类会被 5xx 抢走，套上 1s 起的短曲线而不是容量该有的中等曲线，
///    且两个开关（`server_error` / `capacity_400`）的语义互相串台。
/// 5. **新增的三条判据一律排在上面三条 `None` 之后**。那三条是「网关自己的背压」与「永久态」，
///    任何通用判据排到它们前面都会把不该重试的东西吸收掉——本仓已有的守卫测试钉着这个顺序。
pub(crate) fn absorb_class_of(err_str: &str) -> Option<AbsorbClass> {
    if err_str.contains("inbound_admission_timeout=1") {
        return None;
    }
    if err_str.contains("model_unsupported_by_pool=1") {
        return None;
    }
    // 池**永久**耗尽：池里一个可自愈的号都没有（全是 QuotaExhausted /
    // RefreshTokenInvalid / AccountSuspended 这类需人工处置的终态）。
    // 必须排在 `retry_after_secs=` 之前 —— 它**带**那个标记（对客户端而言 429 +
    // Retry-After 是对的：人工补号后确实会好），但在**单请求的 45s 预算内**
    // 等多久都不会变，吸收它只是占着客户端连接空转满预算再返回同一个 429。
    if err_str.contains("pool_permanently_exhausted=1") {
        return None;
    }
    // 上游并发闸满（网关自己的背压，见 provider.rs 的 `upstream_gate_full=1`）。
    // 与 `inbound_admission_timeout` 同语义：它**带** `retry_after_secs=2`，若不在此排除，
    // 会被下面 `parse_retry_after_secs` 抢成 PoolCooldown 吸收 —— sleep 2s 重打整链、
    // 默认 3 轮 ≈ +6s 延迟，且计数器记成 pool_cooldown 误导面板。必须排在其前。
    // （吸收层开启即内置 shield 场景，这正是 gate-full 会出现的环境。）
    if err_str.contains("upstream_gate_full=1") {
        return None;
    }
    if let Some(secs) = parse_retry_after_secs(err_str) {
        return Some(AbsorbClass::PoolCooldown(secs));
    }
    if is_upstream_rate_limited(err_str) {
        return Some(AbsorbClass::UpstreamRateLimit);
    }
    if is_upstream_temporarily_suspended(err_str) {
        return Some(AbsorbClass::SwapWindow);
    }
    // region 错配让路：它与瞬态 5xx/容量类都不沾，但**永久封禁**那类 403 的响应体里可能
    // 带别的字样。显式排除一次，把「不可吸收的 403」全部挡在下面两条通用判据之前。
    // 判据复用既有谓词（那侧自己已排除了 provider 打的瞬态标记）。
    if is_upstream_region_mismatch_403(err_str) {
        return None;
    }
    // 容量类**必须在 5xx 之前**：它的一种上游形态就是 503（另一种是 400），
    // 而下面那条 5xx 判据认 `503 service unavailable` 字样。顺序反了容量类会被吞。
    // 判据只调既有谓词，不新写字符串匹配。
    if crate::kiro::endpoint::default_is_model_temporarily_unavailable(err_str) {
        return Some(AbsorbClass::TransientCapacity400);
    }
    // 上游 5xx。`is_upstream_transient_5xx` 同时认传输层，这里显式减掉它：
    // 传输层故障由 provider 内部换号已覆盖（每个号各试一遍），吸收层再套一层只是把
    // 同一个网络故障重打 N 遍。这也保住既有测试
    // `non_retryable_errors_are_not_absorbable` 里那条传输层用例的语义。
    if is_upstream_transient_5xx(err_str) && !is_transport_error(&err_str.to_ascii_lowercase()) {
        return Some(AbsorbClass::TransientServerError);
    }
    // 配额耗尽（MONTHLY_REQUEST_COUNT / QUOTA）/ 网络 / TLS / 其它 4xx / 未知：一律不吸收。
    // 配额类要等下个计费周期，网络类由 provider 内部的换号已覆盖，再套一层只是放大。
    None
}

/// provider 在「吸收层跑过至少一轮但仍放弃」时打在错误串上的机器可读标记。
///
/// 用途只有一个：让 [`map_provider_error`] 能把这类**且仅这类**请求的终态状态码换成 503
/// （`upstream_retry_absorb_exhausted_status=503` 时）。没进过吸收层的 429 照旧是 429。
///
/// 用标记而非按中文文案匹配，理由同 `pool_permanently_exhausted=1` / `bearer_invalid_transient=1`：
/// 文案改动不该让分类失效。
pub(crate) const ABSORB_BUDGET_EXHAUSTED_MARKER: &str = "absorb_budget_exhausted=1";

/// 吸收层耗尽后回 503 时的 Retry-After 秒数（无更精确真值时的兜底）。
///
/// 取值与 `UPSTREAM_RATE_LIMIT_RETRY_AFTER_SECS`（8）同源而非另立数字：这条路径的绝大多数
/// 来源就是上游 429，8s 是那边实测曲线上「命中率回落到接近基线」的拐点。带 `retry_after_secs=`
/// 真值时优先用真值（号池算出来的剩余秒数比任何常数都准）。
const ABSORB_EXHAUSTED_RETRY_AFTER_SECS: u64 = UPSTREAM_RATE_LIMIT_RETRY_AFTER_SECS;

/// 将 KiroProvider 错误映射为 HTTP 响应
fn map_provider_error(err: Error) -> Response {
    let err_str = err.to_string();

    // ⭐ 吸收层已尽力重试仍失败，且部署侧显式要求这类终态回 503 —— **必须是第一条分支**。
    //
    // 为什么排最前：这个标记只可能打在**已经被判为可吸收**的错误串上，而那些串必然还带着
    // 各自的原始特征（`retry_after_secs=` / `USER_REQUEST_RATE_EXCEEDED` /
    // `temporarily is suspended` / 5xx 字样）—— 下面任何一条分支都会先把它们接走并返回 429。
    // 排在后面等于这个开关静默失效。
    //
    // 为什么标记由 provider 打而不是在这里判「是不是可吸收类」：本函数拿到的错误串**分不出**
    // 「吸收层真的跑过并放弃」与「吸收层根本没开、429 原样透传」。后者改成 503 是错的
    // （网关一次都没重试，却告诉客户端「我们这边暂时不可用」）。
    //
    // 依据（外挂 `kiro_shield.py` 原注释）：Cursor 见 429 会**掐会话**，对 503 不会。
    // 即同一个「网关已尽力但没成」的事实，用 429 表达让客户端直接放弃，用 503 表达让它
    // 自己再退避重试。默认 503（2026-08-11 改：429 会让 Cursor 掐会话、用户实测全部
    // 暂停；503 触发退避、频率受 Retry-After 控制），
    // 503 是为特定客户端做的兼容让步 —— 见 `upstream_retry_absorb_exhausted_status`。
    // ⭐ 每客户端请求共享预算耗尽（2026-08-11 方案 A）：websearch 回灌轮/压缩轮/透传
    // failover 把整条请求的 ABSOLUTE_MAX_TOTAL_RETRIES 花完。**必须排第一优先**
    // （在 absorb 分支之前）：它与吸收层耗尽语义同级——「网关已尽力，请退避」，返回
    // 503 + Retry-After（503 而非 429：Cursor 见 429 掐会话；见 503 自行退避重试，
    // 重试频率受 Retry-After 控制）。改前这条落 502 兜底：客户端当服务端故障、
    // 退避逻辑不启动、立刻原样重发（拿一份全新预算再打 4 次，放大在客户端侧复活）。
    if err_str.contains("shared_budget_exhausted=1") {
        // 预算耗尽串由 provider 构造，不含 retry_after_secs= 真值——固定用吸收层同款
        // 兜底值（15s 级退避即可，语义是「请客户端退避」）。
        let retry_after = ABSORB_EXHAUSTED_RETRY_AFTER_SECS.clamp(1, 300);
        tracing::warn!(
            error = %err,
            retry_after_secs = retry_after,
            "每客户端请求的上游预算已耗尽（跨层共享），按 503 回：客户端自行退避"
        );
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            [(header::RETRY_AFTER, retry_after.to_string())],
            Json(ErrorResponse::new(
                "api_error",
                // 🔴 文案里的「等容量」是**承重字符串**，不是修辞（2026-08-11 对抗评审加）。
                //
                // 线上真实链路是 `Caddy → kiro_shield.py → KiroStudio`，而 shield 的
                // `classify()` **按 body 文案分类、不按状态码**，且只有
                // `verdict ∈ {cool, auth}` 才会读我们的 Retry-After：
                //     if verdict in ("cool","auth"): delay = cool_delay(..., Retry-After)
                //     else:                          delay = swap_delay(attempt)  # 本地阶梯
                // 「等容量」是它 `COOLING_MARKERS` 里的词。删掉它 ⇒ 落 `retry` 兜底
                // ⇒ **我们精心算的 Retry-After 被整个丢弃**，改走 20→60s 本地阶梯
                // ⇒ 等真实恢复时间的 2~6 倍（CLAUDE.md 记录：当晚 1753 次失败就是这么来的）。
                // 改文案前先 `grep COOLING_MARKERS /opt/skiapi/services/kiro_shield.py`。
                "网关已就该请求打满上游调用预算（每请求上限），上游仍不可用（等容量）。\
                 这是可重试的瞬态状态，请按 Retry-After 退避后重试。",
            )),
        )
            .into_response();
    }

    if err_str.contains(ABSORB_BUDGET_EXHAUSTED_MARKER) {
        // Retry-After 优先用号池真值（`retry_after_secs=N`），其次按类别兜底。
        let retry_after = parse_retry_after_secs(&err_str)
            .or_else(|| {
                is_upstream_temporarily_suspended(&err_str)
                    .then_some(UPSTREAM_SUSPENDED_RETRY_AFTER_SECS)
            })
            .unwrap_or(ABSORB_EXHAUSTED_RETRY_AFTER_SECS)
            .clamp(1, 300);
        tracing::warn!(
            error = %err,
            retry_after_secs = retry_after,
            "内置吸收层已用尽预算仍未成功，按配置回 503（而非透传 429）：\
             Cursor 一类客户端见 429 会掐会话，见 503 会自行退避重试"
        );
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            [(header::RETRY_AFTER, retry_after.to_string())],
            Json(ErrorResponse::new(
                "api_error",
                // 「等容量」同上，是 shield `COOLING_MARKERS` 的承重词（详见上一处 503 的注释）。
                // 少了它，shield 丢弃我们的 Retry-After 改走 20→60s 本地阶梯。
                "网关已就该请求重试至预算上限，上游仍不可用（等容量）。这是可重试的瞬态状态，\
                 请按 Retry-After 退避后重试。若持续出现：①面板『限流健康』查看号池容量与冷却分布；\
                 ②补充凭据分摊上游压力；③必要时调高 upstreamRetryAbsorb* 预算。",
            )),
        )
            .into_response();
    }

    // 入站准入超时（网关自己的背压）——**必须排在全池冷却之前**，因为它同样带
    // `retry_after_secs=`，顺序反了就会被下面那条抢走、又变回不可区分。
    //
    // 与全池冷却的语义正好相反：全池冷却是「上游没准备好，等等真的会好」，
    // 而这条是「网关在主动限流保护上游」——重试只是把同一个请求塞回同一个满桶。
    // 状态码仍是 429 + Retry-After（对**客户端**而言那是正确的：它该退避），
    // 但 message 刻意与冷却不同，好让**重试层**（内置吸收层 / 外挂 kiro_shield）
    // 能靠响应体分辨出「这是网关的背压，不该重试」。
    // 两者若共用同一句文案，任何按 body 判定的重试层都会重试网关自己的背压信号。
    if err_str.contains("inbound_admission_timeout=1") {
        let retry_after = parse_retry_after_secs(&err_str).unwrap_or(1).clamp(1, 300);
        tracing::warn!(
            retry_after_secs = retry_after,
            "入站准入排队超时（网关背压），返回 429 + Retry-After；不可吸收"
        );
        return (
            StatusCode::TOO_MANY_REQUESTS,
            [(header::RETRY_AFTER, retry_after.to_string())],
            Json(ErrorResponse::new(
                "rate_limit_error",
                "Gateway inbound rate shaping is at capacity (request admission timed out). \
                 This is gateway-side backpressure, not an upstream cooldown; retrying immediately will not help.",
            )),
        )
            .into_response();
    }

    // 上游并发闸已满（网关自己的背压，见 provider.rs 的 `upstream_gate_full=1`）。
    // 与 `inbound_admission_timeout` 同语义：必须 429 + Retry-After 让客户端退避，
    // 而不是落 502（502 会让客户端立即重发，重新灌满闸门，放大反而更凶）。
    // message 带 "gateway-side backpressure" 让重试层可区分，不当作上游问题重试。
    if err_str.contains("upstream_gate_full=1") {
        let retry_after = parse_retry_after_secs(&err_str).unwrap_or(2).clamp(1, 300);
        tracing::warn!(
            retry_after_secs = retry_after,
            "上游并发闸已满（网关背压），返回 429 + Retry-After；不可吸收"
        );
        return (
            StatusCode::TOO_MANY_REQUESTS,
            [(header::RETRY_AFTER, retry_after.to_string())],
            Json(ErrorResponse::new(
                "rate_limit_error",
                "Gateway upstream concurrency gate is full (too many in-flight upstream calls). \
                 This is gateway-side backpressure, not an upstream cooldown; retrying immediately will not help.",
            )),
        )
            .into_response();
    }

    // 全池冷却快速失败：token_manager 全池都在冷却时会带 retry_after_secs=N 快速 bail。
    // 这里透传成标准 429 + Retry-After 头，让客户端(Claude Code)按其自身退避策略重试——
    // 比网关内硬扛温和，也减少对被风控号的试探。
    if let Some(secs) = parse_retry_after_secs(&err_str) {
        let retry_after = secs.clamp(1, 300);
        tracing::warn!(
            retry_after_secs = retry_after,
            "全池冷却，返回 429 + Retry-After 让客户端退避"
        );
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

    // 上游 **403 region 错配**（`bearer token ... is invalid`）：映射成 403 `permission_error`，
    // 绝不落下方兜底的 502。实测 397 次全部落的兜底。
    //
    // 判据与理由见 `is_upstream_region_mismatch_403`。要点：这是**授权层**拒绝
    // （`ksk_` token 按 region 授权，打错区恒 403），不是服务端故障、也不是「稍后会好」。
    // 旧路径返 502 → 外挂 `kiro_shield.py`（`RETRYABLE={429,500,502,503,504}`）与客户端
    // 都按 5xx 盲退避重打，而重打多少次都不会变；正确动作是改 region / 让 region 探测重选。
    //
    // 为什么不给 Retry-After、也不返 429：给了就等于宣称「等一会儿会好」，会把
    // 一个需要人工（或探测器）介入的配置错误变成客户端侧的无限退避重试 ——
    // 与 `is_upstream_temporarily_suspended` 刻意不吞永久封禁是同一条理由。
    //
    // 位置：在 429/临时风控**之后**（同一响应体可能同时提多个码，那两类的可重试语义优先），
    // 在 `translate_upstream_error` **之前**（该链的 `Invalid token` / `subscription`
    // 宽判据将来若被放宽，会先行吞掉这条并给出「刷新 Token」的错误排障动作）。
    if is_upstream_region_mismatch_403(&err_str) {
        tracing::warn!(
            error = %err,
            "上游 403 region 错配（bearer token invalid），返回 403 permission_error（旧代码落 502 兜底致上游/外挂按 5xx 盲退避）"
        );
        return (
            StatusCode::FORBIDDEN,
            Json(ErrorResponse::new(
                "permission_error",
                "上游拒绝该凭据的授权（bearer token 对目标 region 无效）。这不是服务端故障，重试无效：\
                 `ksk_` 类 token 按 region 授权，打错 region 恒被拒。排障：①面板查看该凭据的 region 是否与签发 region 一致；\
                 ②对该凭据手动改 region（或等网关 region 探测自动重选）；③若整池同区，确认推号来源给的 region 正确。",
            )),
        )
            .into_response();
    }

    // 已确证含义的上游错误：翻译成带排障步骤的可读错误。
    if let Some(t) = translate_upstream_error(&err_str) {
        tracing::warn!(error = %err, error_type = t.error_type, "上游错误已翻译为可读排障提示");
        let mut resp = (t.status, Json(ErrorResponse::new(t.error_type, t.message))).into_response();
        if t.retry_compress {
            resp.headers_mut().insert(
                axum::http::HeaderName::from_static("x-kirostudio-compress-retry"),
                axum::http::HeaderValue::from_static("1"),
            );
        }
        return resp;
    }

    // 上游 5xx / 传输层错误：**503 + Retry-After**，不落未识别兜底的 502。
    //
    // 🔴 修复的缺陷（24h 实测）：上游 `InternalServerException`（160 条）与传输层失败
    // （148 条）匹配不上上面任何分支 → 落末尾兜底 → **502 且无 Retry-After**。
    // 后果与「所有凭据均已禁用落 502」同型：客户端（Claude Code）把 502 当服务端故障，
    // 退避逻辑压根不启动，原样重发 → 又 502。而这两类都是**明确可重试的瞬态错误**。
    //
    // 更糟的是重试预算：`compute_max_retries` 按池子大小算，池里只剩 1 个可用号时
    // 算出的是 1 —— 日志里那句 `尝试 1/1` 就是它。所以上游一次 500 **一次都没重试**
    // 就吐给客户端了（实测 `server_error` 的 retries 分布：296 个 0 次、34 个 1 次）。
    // 网关侧重试预算这条要单独修（它碰选号热路径），但**至少要让客户端知道该退避**。
    //
    // 判据复用 `is_retryable_upstream_error`（provider 决定是否换号用的同一个谓词），
    // 不新写字符串匹配 —— 新写一套必然与那侧漂移，那正是本类缺陷反复出现的成因。
    // 位置必须在兜底**之前**、在上面所有已识别分支**之后**：它只捡剩下的 5xx。
    if is_upstream_transient_5xx(&err_str) {
        const UPSTREAM_5XX_RETRY_AFTER_SECS: u64 = 3;
        tracing::warn!(
            error = %err,
            retry_after_secs = UPSTREAM_5XX_RETRY_AFTER_SECS,
            "上游 5xx/传输层瞬态错误，返回 503 + Retry-After（旧代码落 502 无 Retry-After 致客户端不退避）"
        );
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            [(
                header::RETRY_AFTER,
                UPSTREAM_5XX_RETRY_AFTER_SECS.to_string(),
            )],
            Json(ErrorResponse::new(
                "api_error",
                "上游服务暂时不可用（5xx 或连接失败），这是可重试的瞬态错误。\
                 请按 Retry-After 退避后重试；若持续出现，请查看网关日志。",
            )),
        )
            .into_response();
    }

    // 未知错误:**完整原文只进服务端日志**(便于 dwgx 排障),**不回给客户端**——原始错误链可能
    // 含上游响应体里的 profileArn / AWS 账号号 / region / 内部 URL 等敏感信息(review 泄露发现)。
    // 客户端只得通用提示 + 引导查网关日志,不泄露任何上游内部细节。
    tracing::error!(
        "Kiro API 调用失败（未识别，原文仅进日志不回客户端）: {}",
        err
    );
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
    for s in crate::anthropic::model_catalog::CATALOG
        .iter()
        .filter(|s| s.advertised)
    {
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

/// 入站整形准入闸门：整个客户端请求只过一次（在透传/Kiro/WebSearch 分叉之前）。
/// 两条 HTTP 入口（post_messages 与 post_messages_cc）都必须调用本函数。
/// 2026-08-10 前闸门在 provider.call_api_with_retry 内部（仅 Kiro 路径过闸、透传
/// 100% 绕过）；移到 handler 层后 /cc/v1 入口曾漏闸，2026-08-11 补上。
///
/// 超时语义依赖 throttle.rs：queue_timeout_passthrough=true（默认）时排队超时=放行
/// （返回 None），false 时才返回 429 + Retry-After。
///
/// ⚠️ 本函数体里的标记字面量被源码级守卫钉死（admission_timeout_bail_must_carry_
/// its_own_marker 切片本函数体断言），改文案前先看那个测试的注释。
async fn try_inbound_admission_gate(
    provider: &crate::kiro::provider::KiroProvider,
    model: &str,
    stream: bool,
    client: &ClientInfo,
) -> Option<Response> {
    if let Err(retry_after) = provider.token_manager().acquire_admission().await {
        let ra = retry_after.clamp(1, 300);
        let err_str = format!(
            "入站限速排队超时(网关目标 {} RPM 保护上游)inbound_admission_timeout=1 retry_after_secs={}",
            provider.token_manager().inbound_target_rpm(), ra);
        crate::common::recovery_metrics::bump_inbound_admission_timeout();
        let mut record = crate::usage::RequestRecord::new(
            uuid::Uuid::new_v4().to_string(), model.to_string());
        record.requested_model = Some(model.to_string());
        record.is_streaming = stream;
        record.outcome = crate::usage::RequestOutcome::RateLimited;
        record.error_message = Some(err_str);
        record.session_id = Some("admission-timeout".to_string());
        client.apply(&mut record);
        crate::usage::emit_record(record);
        tracing::warn!(retry_after_secs = ra, "入站准入排队超时（网关背压），返回 429 + Retry-After");
        return Some((
            StatusCode::TOO_MANY_REQUESTS,
            [(header::RETRY_AFTER, ra.to_string())],
            Json(ErrorResponse::new("rate_limit_error",
                "Gateway inbound rate shaping is at capacity. \
                 This is gateway-side backpressure, not an upstream cooldown; \
                 retrying immediately will not help.")),
        ).into_response());
    }
    None
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
                Json(ErrorResponse::new(
                    "invalid_request_error",
                    format!("请求体解析失败: {e}"),
                )),
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

    // 入站整形准入闸门：透传与 Kiro 两条路径之上统一过一次令牌桶。
    // 2026-08-10 修：此前闸门只在 provider.rs 的 call_api_with_retry 内部
    // （仅 Kiro 路径得过），透传路径 100% 绕过 → inboundThrottleEnabled 对
    // 代挂池完全无效。现移到 handler 层，两条路径进来之前统一过闸。
    if let Some(resp) = try_inbound_admission_gate(&provider, &payload.model, payload.stream, &client).await {
        return resp;
    }

    // 每客户端请求的共享上游预算（2026-08-11 方案 A，RPM 放大治本）：沿整条调用链传递
    // （透传 failover → websearch 回灌轮 → 压缩重试轮 → Kiro 主路径 failover → MCP），
    // 无论嵌套多少层，一次客户端请求打上游的总次数恒 ≤ ABSOLUTE_MAX_TOTAL_RETRIES。
    let retry_budget = crate::kiro::provider::SharedRetryBudget::new();

    let passthrough_result = if provider.token_manager().should_try_custom_api_first() {
        provider
            .try_custom_api_passthrough(
                raw_body.clone(),
                Some(&payload.model),
                user_id.as_deref(),
                // P3：把客户端请求头传给透传，让 forward 按白名单转发 anthropic-beta 等。
                Some(&headers),
                &retry_budget,
            )
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
        // 双口径：requested = 客户端原始名，upstream = 映射后名（PassthroughMeta 携带）。
        record.requested_model = meta.model.clone();
        record.upstream_model = meta.mapped_model.clone();
        record.credential_id = Some(meta.credential_id);
        record.session_id = meta.session_id.clone();
        record.is_streaming = payload.stream;
        record.input_tokens = input_tokens;
        record.output_tokens = 0;
        record.latency_ms = meta.latency_ms;
        record.outcome = meta.outcome;
        // 🔴 上游错误原文进 trace（此前恒空，导致 400 根因不可见）。
        // 只记截断后的开头，避免超长错误体污染 trace。
        if let Some(err) = &meta.upstream_error {
            record.error_message = Some(err.clone());
        }
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

        return websearch::handle_websearch_request(provider, &payload, input_tokens, &retry_budget).await;
    }

    // 混合工具场景：请求带 web_search 但未显式触发搜索，剔除 web_search 后走常规转发，
    // 避免把 web_search 原样下发给 Kiro 触发 400 Improperly formed request。
    // 🔴 2026-08-09：混合工具场景**不再剔除** web_search。
    //
    // 改前剔除 ⇒ 上游模型完全看不到搜索工具 ⇒ Claude Code 的 WebSearch 在
    // "web_search + 其他工具"（CC 常态）下**静默失效**。现在交给 converter 把它
    // 归一化成 Kiro 认的函数工具形态（converter.rs 的 convert_tools + 内置
    // web_search schema），模型能看到、能调用，回 tool_use 时由**网关内部消化**：
    // 上游回 web_search tool_use 且本轮无其他工具 → agentic 回灌（内部调 MCP、
    // 结果回灌重发，最多 5 轮）；一旦混入非 web_search 工具 → 整轮原样回客户端。
    // 纯 web_search 与显式触发仍走上面的本地 MCP 快路径，不受影响。
    if let Some(resp) = dispatch_web_search_loop(&provider, &payload, &retry_budget, &client).await {
        return resp;
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
                ConversionError::UnsupportedToolMapping { tool_name, reason } => {
                    ("invalid_request_error", format!("工具参数无法映射: {} — {}", tool_name, reason))
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
    // 保留原始状态的克隆，供 CONTENT_LENGTH_EXCEEDS 重试时重建（渐进式压低 target_bytes）。
    // native effort 字段随请求体一起走（压缩只作用于 conversation_state，不受影响）。
    let conv_state_for_compress_retry = conversion_result.conversation_state.clone();
    let native_fields_for_compress_retry =
        conversion_result.additional_model_request_fields.clone();
    let request_body = match build_kiro_request_body(
        conversion_result.conversation_state,
        conversion_result.additional_model_request_fields,
        &current_compression(),
        None,
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

    // 估算影子缓存
    let prefix_tokens = token::count_prefix_tokens(payload.system.as_deref(), &payload.messages);
    // Layer 3 指纹（2026-08-11 移植，cache_fingerprint.rs）：比 Layer 2 前缀估算严格更完整
    // （含 creation）。无指纹（无会话种子/无历史前缀）时回退 Layer 2 前缀估算。
    let fingerprint_usage = prompt_cache_enabled()
        .then(|| crate::anthropic::cache_fingerprint::compute_fingerprint_usage(&payload))
        .flatten();
    let cache_breakdown = fingerprint_usage
        .map(|u| u.clamp_to_total(input_tokens).to_cache_breakdown())
        .or_else(|| estimate_cache_breakdown(prompt_cache_enabled(), prefix_tokens, input_tokens));

    // 检查是否启用了thinking
    let thinking_enabled = payload
        .thinking
        .as_ref()
        .map(|t| t.is_enabled())
        .unwrap_or(false);

    let tool_name_map = conversion_result.tool_name_map;
    let known_tool_names = conversion_result.known_tool_names;
    let tool_required_fields = conversion_result.tool_required_fields;

    // 压缩重试：上游返回 CONTENT_LENGTH_EXCEEDS_THRESHOLD 时，网关用更低的压缩目标重建请求体重发。
    // 初试用配置阈值，重试时 target_bytes 按 (3/4)^attempt 逐轮压低（最多 3 次，下限 64 KiB），
    // 且受总墙钟预算约束（单轮内部有自己的 45s failover 预算，多轮叠乘需封顶）。
    const MAX_COMPRESS_RETRIES: u32 = 3;
    // 90 = 2×45s：初试一轮完整 failover 预算 + 至少一次完整重试预算（慢上游下压缩重试
    // 才有意义）。墙钟只在**轮末**检查（见下方循环的 continue 条件），一轮内部可跑满
    // 45s failover 预算 ⇒ 实际最坏 ≈ 90 + 45 = 135s（最后通过检查的那轮不可抢占）——
    // 有界即可，不给满 4×45s：压缩重试是「同一请求换个更小的 body」的低成功期望尝试，
    // 叠乘 180s 正是要压住的最坏形态。
    const MAX_COMPRESS_RETRY_BUDGET_SECS: u64 = 90;
    let compress_started = std::time::Instant::now();
    let mut compress_attempt: u32 = 0;
    let compression_cfg = current_compression();
    'compress_retry: loop {
        let response_body;

        // 仅在重试时重建请求体（初试已在上面构建好，直接复用 request_body）。
        let body_ref: &str = if compress_attempt == 0 {
            &request_body
        } else {
            let target = compress_retry_target(compression_cfg.trigger_bytes, compress_attempt);
            response_body = match build_kiro_request_body(
                conv_state_for_compress_retry.clone(),
                native_fields_for_compress_retry.clone(),
                &compression_cfg,
                Some(target),
            ) {
                Ok(b) => b,
                Err(e) => {
                    tracing::error!("压缩重试时序列化请求失败: {}", e);
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(ErrorResponse::new("internal_error", format!("序列化请求失败: {}", e))),
                    ).into_response();
                }
            };
            tracing::info!(
                attempt = compress_attempt,
                target_bytes = target,
                body_len = response_body.len(),
                "CONTENT_LENGTH_EXCEEDS: 重新压缩请求体并重试"
            );
            &response_body
        };

        let response = if payload.stream {
            if cc_auto_buffer_enabled() && is_claude_code_request(&headers) {
                tracing::debug!("识别到 Claude Code 请求，/v1 流式自动切换为 buffered 分发");
                handle_stream_request_buffered(
                    provider.clone(),
                    body_ref,
                    &payload.model,
                    input_tokens,
                    thinking_enabled,
                    tool_name_map.clone(),
                    known_tool_names.clone(),
                    tool_required_fields.clone(),
                    cache_breakdown.clone(),
                    &retry_budget,
                    client.clone(),
                ).await
            } else {
                handle_stream_request(
                    provider.clone(),
                    body_ref,
                    &payload.model,
                    input_tokens,
                    thinking_enabled,
                    tool_name_map.clone(),
                    known_tool_names.clone(),
                    tool_required_fields.clone(),
                    cache_breakdown.clone(),
                    &retry_budget,
                    client.clone(),
                ).await
            }
        } else {
            let extract_thinking = extract_thinking_enabled() && thinking_enabled;
            handle_non_stream_request(
                provider.clone(),
                body_ref,
                &payload.model,
                input_tokens,
                extract_thinking,
                tool_name_map.clone(),
                cache_breakdown.clone(),
                fingerprint_usage,
                &retry_budget,
                client.clone(),
            ).await
        };

        // 检查是否为可压缩重试的错误（CONTENT_LENGTH_EXCEEDS / Input too long）
        let is_compress_retryable = compress_attempt < MAX_COMPRESS_RETRIES
            && compress_started.elapsed()
                < std::time::Duration::from_secs(MAX_COMPRESS_RETRY_BUDGET_SECS)
            && response.headers().get("x-kirostudio-compress-retry").is_some();

        if is_compress_retryable {
            compress_attempt += 1;
            continue 'compress_retry;
        }

        // 重试已耗尽（或本轮不可重试）：x-kirostudio-compress-retry 是内部标记，
        // 不得透传给客户端（2026-08-11 对抗审查抓出）。
        let mut final_response = response;
        final_response
            .headers_mut()
            .remove("x-kirostudio-compress-retry");
        return final_response;
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
    // Bug C：工具必需参数表（工具名 → required 字段名列表）。空表 = 不校验。
    tool_required_fields: std::collections::HashMap<String, Vec<String>>,
    cache_breakdown: Option<CacheUsageBreakdown>,
    budget: &crate::kiro::provider::SharedRetryBudget,
    client: ClientInfo,
) -> Response {
    // 1M 变体:据原始模型名判定是否注入 anthropic-beta 头(仅受支持的 [1m] 变体为 true)。
    let is_1m = crate::anthropic::model_catalog::resolve_is_1m(model);
    // 调用 Kiro API（支持多凭据故障转移）
    let (response, meta) = match provider.call_api_stream(request_body, is_1m, budget).await {
        Ok(resp) => resp,
        Err(e) => return map_provider_error(e),
    };

    // 创建流处理上下文
    let mut ctx = StreamContext::new_full(
        model,
        input_tokens,
        thinking_enabled,
        tool_name_map,
        known_tool_names,
    );
    // 注入影子缓存估算（必须在 generate_initial_events 之前，message_start 才能携带 cache 字段）
    ctx.set_cache_usage(cache_breakdown);
    // Bug C：注入工具必需参数表，启用「参数 JSON 合法但缺 required 字段」校验
    // （如 Bash 只给 description 没给 command）。空表 = 不校验，行为与改前一致。
    ctx.set_tool_required_fields(tool_required_fields);

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
    // 双口径：requested = 客户端原始名（= record.model），upstream = 映射后名。
    record.requested_model = meta.model.clone();
    record.upstream_model = meta.mapped_model.clone();
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
    // 中断字节：正常收尾 None，断流时记录已收字节（与 first_token_ms 同模式读 ctx）。
    record.interrupted_bytes = ctx.interrupted_bytes();
    record.retries = meta.retries;
    // 去硬编码 Success：按本次响应的真实完成状态记账，避免截断/上游错误被记成成功污染熔断信号。
    record.outcome = ctx.completion_outcome();
    // 2026-08-11 补：失败态此前 error_message 恒 NULL（线上 38 条实测盲区）。
    // client_message() 对失败态必非空；成功态保持 NULL 不污染（与记录契约一致）。
    if !ctx.completion().is_ok() {
        record.error_message = Some(ctx.completion().client_message());
    }
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

/// 为上游空响应构造合适的 SSE error 事件。
///
/// - 大输入（疑似上下文过大）：返回 invalid_request_error，提示压缩上下文，
///   不鼓励原样重试（重试还是同样的大请求，仍会空）。
/// - 小输入（疑似偶发）：返回 overloaded_error，客户端可重试。
fn empty_response_error_event(oversized_context: bool) -> SseEvent {
    let (err_type, message) = if oversized_context {
        (
            "invalid_request_error",
            "上游返回了空响应，疑似上下文已接近窗口上限。请精简对话历史（如 /compact）、\
             缩短 system prompt 或减少工具数量后重试。",
        )
    } else {
        (
            "overloaded_error",
            "上游返回了空响应，请重试。",
        )
    };
    SseEvent::new(
        "error",
        serde_json::json!({
            "type": "error",
            "error": { "type": err_type, "message": message }
        }),
    )
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
                            // 累计上游传输字节（断流收尾时经 interrupted_bytes 落库）
                            ctx.note_received_bytes(chunk.len());
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
                            // 空响应检测：正常完成但收尾兜底后模型仍什么都没产出（或上下文压力下的
                            // 退化短响应）时，用显式 error 事件替代空 end_turn，避免客户端 agentic
                            // 循环卡住。上下文过大 → invalid_request_error 提示 /compact；偶发 → 可重试。
                            if ctx.completion().is_ok()
                                && !ctx.error_event_emitted()
                                && ctx.is_empty_response()
                            {
                                let oversized = ctx.empty_response_is_oversized_context();
                                tracing::warn!(
                                    oversized_context = oversized,
                                    "上游返回空响应（收尾兜底后仍无内容），补发 error 事件替代空 end_turn"
                                );
                                final_events.push(empty_response_error_event(oversized));
                                ctx.mark_error_event_emitted();
                            } else {
                                final_events.extend(tail);
                            }
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
        err,
        tool_use_id
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
/// 本函数是四层降级链（`src/anthropic/cache.rs`）的 **Layer 2**：`resolve_cache_chain`
/// 在拿到完整响应后先看 Layer 1 上游 `meteringEvent` 的 cache 真值（`MeteringEvent`
/// 新增的 `cacheReadInputTokens/cacheCreationInputTokens`，见 metering.rs），缺失才回落本估算。
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

/// 四层降级链的收口：把「prefix 估算 + metering 真值」收敛成最终 cache 记账。
///
/// 返回 `(cache 记账, 是否估算)`：
/// - 估算=true → 数字来自本地估算（Layer 2 prefix / Layer 4 ratio），响应头标注「估算」；
/// - 估算=false → 数字来自上游 metering 真值（Layer 1），响应头不标注。
///
/// 优先级（高→低，见 `src/anthropic/cache.rs`）：
/// 1. **metering 真值**（Layer 1）——真值不是估算，不受 `promptCacheEnabled` 开关约束；
/// 2. **fingerprint**（Layer 3，2026-08-11 移植）——**存在时跳过 prefix**（下方
///    `fingerprint_usage.is_some()` 强制 prefix 槽为 None；指纹含 creation 严格更完整）；
/// 3. **prefix 估算**（Layer 2，既有 `estimate_cache_breakdown` 产出，无指纹时兜底）；
/// 4. **ratio 兜底**（Layer 4，50% cache / 30% creation）。
///
/// 开关关且无 metering 真值时返回 `(None, false)`：保持既有行为——完全不做 cache 记账，
/// 不凭空造 cache 命中（配置在说谎的旧缺陷，见 `prompt_cache_enabled`）。
fn resolve_cache_chain(
    enabled: bool,
    final_input_tokens: i32,
    prefix_estimate: Option<CacheUsageBreakdown>,
    fingerprint_usage: Option<super::cache::PromptCacheUsage>,
    metering_read: Option<i32>,
    metering_creation: Option<i32>,
) -> (Option<CacheUsageBreakdown>, bool) {
    let metering = match (metering_read, metering_creation) {
        (Some(r), Some(c)) => Some((r, c)),
        _ => None,
    };
    // Layer 1：上游真值优先，且不受开关约束（真值不是估算）。
    if let Some(m) = metering {
        let usage = super::cache::select_final_usage(
            final_input_tokens,
            Some(m),
            None,
            None,
            super::cache::PromptCacheUsage::default(),
        );
        return (Some(usage), false);
    }
    if !enabled {
        return (None, false);
    }
    // Layer 3 存在时强制 Layer 2 槽为 None：否则 fingerprint 的 creation 会被
    // select_final_usage 的 Layer 2 分支（只读 read、creation 硬置 0）吞掉 ——
    // 非流式路径下 fingerprint 就成了生产死代码（对抗审查 MAJOR 1，2026-08-11）。
    let prefix_estimated_read = if fingerprint_usage.is_some() {
        None
    } else {
        prefix_estimate.map(|c| c.cache_read_input_tokens)
    };
    let ratio_fallback = super::cache::PromptCacheUsage::from_ratios(final_input_tokens, 0.5, 0.3);
    // Layer 3 fingerprint：2026-08-11 移植（cache_fingerprint.rs），无指纹时为 None → 落 Layer 4。
    let usage = super::cache::select_final_usage(
        final_input_tokens,
        None,
        prefix_estimated_read,
        fingerprint_usage,
        ratio_fallback,
    );
    (Some(usage), true)
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
    fingerprint_usage: Option<super::cache::PromptCacheUsage>,
    budget: &crate::kiro::provider::SharedRetryBudget,
    client: ClientInfo,
) -> Response {
    // 1M 变体:据原始模型名判定是否注入 anthropic-beta 头(仅受支持的 [1m] 变体为 true)。
    let is_1m = crate::anthropic::model_catalog::resolve_is_1m(model);
    // 调用 Kiro API（支持多凭据故障转移）
    let (response, meta) = match provider.call_api(request_body, is_1m, budget).await {
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
    // 上游 reasoningContentEvent 携带的思考签名（若有）。下发 thinking 块时优先回传 ——
    // Foxfishc 实测「伪造签名不被识别，cache_read 仍 0」；缺则回退占位符（行为与旧版一致）。
    let mut reasoning_signature: Option<String> = None;
    let mut tool_uses: Vec<serde_json::Value> = Vec::new();
    let mut has_tool_use = false;
    let mut stop_reason = "end_turn".to_string();
    // 从 contextUsageEvent 计算的实际输入 tokens
    let mut context_input_tokens: Option<i32> = None;
    // 从 meteringEvent 解析的真实 credit 消耗量
    let mut credits_used: Option<f64> = None;
    // 从 meteringEvent 解析的 cache 真值（Layer 1：上游返回的真实 cache_read/cache_creation）。
    // 缺失（None）时降级到本地 prefix 估算 / ratio 兜底。
    let mut metering_cache_read: Option<i32> = None;
    let mut metering_cache_creation: Option<i32> = None;
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
                                                if let Some(fixed) =
                                                    super::stream::repair_tool_json(buffer)
                                                {
                                                    if let Ok(v) = serde_json::from_str(&fixed) {
                                                        tracing::info!(
                                                            "非流式工具 JSON 已修复为合法(tool_use_id={})",
                                                            tool_use.tool_use_id
                                                        );
                                                        v
                                                    } else {
                                                        // 理论不可达(repair 内部已复验),兜底走失败态。
                                                        mark_invalid_tool_input(
                                                            &mut completion,
                                                            &tool_use.tool_use_id,
                                                            &e,
                                                        );
                                                        serde_json::json!({})
                                                    }
                                                } else {
                                                    // 修不好：置失败态，收尾(下方 `if !completion.is_ok()`)
                                                    // 返回非 200，绝不静默吞成空参数——空参会让客户端把失败的
                                                    // 工具调用当成"无参数成功调用"执行，比报错更危险。
                                                    mark_invalid_tool_input(
                                                        &mut completion,
                                                        &tool_use.tool_use_id,
                                                        &e,
                                                    );
                                                    serde_json::json!({})
                                                }
                                            } else {
                                                mark_invalid_tool_input(
                                                    &mut completion,
                                                    &tool_use.tool_use_id,
                                                    &e,
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

                                // 出站参数还原：Kiro 参数形态 → Claude Code 参数形态
                                // （fs_write 的 path/text → Write 的 file_path/content）。
                                // ⚠️ 仅当入站映射过（tool_name_map 有该 Kiro 名）才还原，否则
                                // 原样透传（避免把不认识的参数清空）。与流式 stop 分支同口径。
                                let client_input = if tool_name_map.contains_key(&tool_use.name) {
                                    crate::anthropic::converter::map_tool_input_from_kiro(
                                        &original_name,
                                        input,
                                    )
                                } else {
                                    input
                                };

                                tool_uses.push(json!({
                                    "type": "tool_use",
                                    "id": tool_use.tool_use_id,
                                    "name": original_name,
                                    "input": client_input
                                }));
                            }
                        }
                        Event::ContextUsage(context_usage) => {
                            let window_size = get_context_window_size(model);
                            let pct = context_usage.context_usage_percentage;
                            // ⭐ 判据与流式路径**共用同一个函数**（见其文档注释：两份独立实现
                            // 曾导致同一个上游异常在两条路径上表现不同 —— 流式忽略脏值、
                            // 非流式把计费口径的 input_tokens 写成 0 或 i32::MAX）。
                            // 由源码守卫 `context_usage_predicate_must_be_shared` 钉死。
                            match crate::anthropic::stream::context_input_tokens_from_pct(
                                pct,
                                window_size,
                            ) {
                                Some(actual_input_tokens) => {
                                    context_input_tokens = Some(actual_input_tokens);
                                    tracing::debug!(
                                        "收到 contextUsageEvent: {}%, 计算 input_tokens: {}",
                                        pct,
                                        actual_input_tokens
                                    );
                                }
                                None => {
                                    // 不覆盖 `context_input_tokens`：保留上一次有效值，或让
                                    // 下游 `unwrap_or` 退回本地估算。warn 因为这代表上游协议异常。
                                    tracing::warn!(
                                        "收到无效 contextUsageEvent（{}%，非正或非有限值），\
                                         忽略该信号、不覆盖已有 input_tokens（避免计费口径被归零）",
                                        pct
                                    );
                                }
                            }
                            // 上界判定与下界守卫**互不依赖**：即便将来下界改动，这条照旧生效。
                            if pct >= 100.0 {
                                stop_reason = "model_context_window_exceeded".to_string();
                            }
                        }
                        Event::Metering(metering) => {
                            credits_used = Some(credits_used.unwrap_or(0.0) + metering.usage);
                            // Layer 1 cache 真值：上游 metering 事件可选携带（缺失则保持 None）。
                            if let Some(r) = metering.cache_read_input_tokens {
                                metering_cache_read = Some(r);
                            }
                            if let Some(c) = metering.cache_creation_input_tokens {
                                metering_cache_creation = Some(c);
                            }
                        }
                        // E1：结构化思考增量（纯 delta，直接追加）。此前落 `_ => {}` 被丢弃，
                        // 非流式只能靠下方的 `<thinking>` 标签提取兜底。
                        Event::ReasoningContent(r) => {
                            reasoning_content.push_str(&r.text);
                            // 缓存上游真签名（若有），thinking 块组装处优先回传。
                            if let Some(sig) = r.signature.as_deref() {
                                if !sig.is_empty() {
                                    reasoning_signature = Some(sig.to_string());
                                }
                            }
                        }
                        Event::Exception {
                            exception_type,
                            message,
                        } => {
                            // 铁律：ContentLengthExceededException = max_tokens 干净收尾，绝不算失败。
                            if exception_type == "ContentLengthExceededException" {
                                stop_reason = "max_tokens".to_string();
                            } else if completion.is_ok() {
                                // 其它异常是上游真实失败，置失败态（保留首因）。
                                tracing::error!(
                                    "非流式收到 in-band 异常: {} - {}",
                                    exception_type,
                                    message
                                );
                                completion = CompletionStatus::UpstreamError {
                                    code: exception_type,
                                    message,
                                };
                            }
                        }
                        Event::Error {
                            error_code,
                            error_message,
                        } => {
                            // in-band 错误事件：落入历史的 `_ => {}` 会被静默忽略、照样返回 200，
                            // 这里显式置失败态，收尾时返回非 200 并按真实 outcome 记账。
                            if completion.is_ok() {
                                tracing::error!(
                                    "非流式收到 in-band 错误: {} - {}",
                                    error_code,
                                    error_message
                                );
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
                            tracing::warn!(
                                "非流式 toolUseEvent 帧解析失败,按响应截断处理: {}",
                                err
                            );
                            if completion.is_ok() {
                                completion = CompletionStatus::DecoderStopped {
                                    message: format!("toolUseEvent 帧解析失败: {}", err),
                                };
                            }
                        } else {
                            tracing::warn!(
                                "非流式事件帧解析失败(event_type={:?}),已忽略: {}",
                                et.as_deref(),
                                err
                            );
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
            // 双口径：requested = 客户端原始名，upstream = 映射后名。
            record.requested_model = meta.model.clone();
            record.upstream_model = meta.mapped_model.clone();
            record.credential_id = Some(meta.credential_id);
            record.session_id = meta.session_id.clone();
            record.is_streaming = meta.is_streaming;
            record.input_tokens = context_input_tokens.unwrap_or(input_tokens);
            record.credits_used = credits_used;
            record.latency_ms = meta.latency_ms;
            record.retries = meta.retries;
            record.outcome = completion.outcome();
            // 2026-08-11 补：此前的失败记录不写 error_message（恒 NULL，线上 38 条实测
            // 成因查不出 = 盲区）。这里 client_message() 对失败态必非空（见 stream.rs
            // CompletionStatus::client_message），成功态不进本分支。
            record.error_message = Some(completion.client_message());
            // 生命周期累计花费：本次真实 credit 消耗累加到该凭据（独立于用量保留期，只增不清）。
            if let Some(c) = record.credits_used {
                provider.report_credits(meta.credential_id, c);
            }
            client.apply(&mut record);
            crate::usage::emit_record(record);
        }
        let status =
            StatusCode::from_u16(completion.http_status_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
        let sse_error_type = completion.sse_error_type();
        return (
            status,
            Json(ErrorResponse::new(
                sse_error_type,
                completion.client_message(),
            )),
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
            // 优先回传上游真签名（若 reasoningContentEvent 带过 signature）：Foxfishc 实测
            // 真签名让多轮 cache 命中、伪造签名 cache_read 仍 0。缺则回退占位符 —— 客户端
            // thinking 模式本地校验要求非空，而回传时 converter 只读 thinking、signature 被
            // serde 静默丢弃，不会转发给 Kiro。详见 stream::THINKING_SIGNATURE_PLACEHOLDER。
            content.push(json!({
                "type": "thinking",
                "thinking": thinking_text,
                "signature": reasoning_signature
                    .clone()
                    .unwrap_or_else(|| super::stream::THINKING_SIGNATURE_PLACEHOLDER.to_string())
            }));
        }

        if !remaining_text.is_empty() {
            content.push(json!({
                "type": "text",
                "text": remaining_text
            }));
        }
    } else if !text_content.is_empty() {
        // 客户端没声明 thinking，但模型仍可能吐内联 `<thinking>` 标签 —— 剥掉再下发。
        // 此前这里是 `"text": text_content` 原样塞入 ⇒ 标签与模型内部推理逐字泄漏。
        // 口径与流式的 `strip_inline_thinking_when_disabled`、以及
        // `process_reasoning_content` 在 !thinking_enabled 时直接丢帧一致。
        let stripped = super::stream::strip_thinking_from_complete_text(&text_content);
        // DSML 工具协议标记剥离：DeepSeek 把 `<｜DSML｜function_calls>` 当文本吐，
        // 非流式此前零处理 → 标记逐字泄漏。与流式 `strip_dsml_markers` 对齐。
        let stripped = super::stream::strip_dsml_from_complete_text(&stripped);
        if !stripped.is_empty() {
            content.push(json!({
                "type": "text",
                "text": stripped
            }));
        }
    }

    content.extend(tool_uses);

    // 估算输出 tokens
    let output_tokens = token::estimate_output_tokens(&content);

    // 使用从 contextUsageEvent 计算的 input_tokens，如果没有则使用估算值
    let final_input_tokens = context_input_tokens.unwrap_or(input_tokens);

    // 四层降级链收敛最终 cache 记账（Layer 1 metering 真值 → Layer 2 prefix →
    // Layer 3 fingerprint（2026-08-11 移植）→ Layer 4 ratio）。入库用**未缩放真值**，
    // 对外下发放大由 scale_for_client 负责。
    let (final_cache_breakdown, cache_estimated) = resolve_cache_chain(
        prompt_cache_enabled(),
        final_input_tokens,
        cache_breakdown,
        fingerprint_usage,
        metering_cache_read,
        metering_cache_creation,
    );

    // 用量埋点：非流式成功记录
    {
        let mut record = crate::usage::RequestRecord::new(
            Uuid::new_v4().to_string(),
            meta.model.clone().unwrap_or_else(|| model.to_string()),
        );
        // 双口径：requested = 客户端原始名，upstream = 映射后名。
        record.requested_model = meta.model.clone();
        record.upstream_model = meta.mapped_model.clone();
        record.credential_id = Some(meta.credential_id);
        record.session_id = meta.session_id.clone();
        record.is_streaming = meta.is_streaming;
        // gross 口径（含 cache）；下方返回客户端的 usage.input_tokens 才是 billed 口径。
        record.input_tokens = final_input_tokens;
        record.output_tokens = output_tokens;
        // 与下方返回客户端的 usage.cache_* 同源，避免"客户端有值、面板恒 0"的矛盾数字。
        // 必须在 input_tokens 赋值之后调用（内部要按 gross 收敛 cache 上限）。
        apply_cache_breakdown(&mut record, final_cache_breakdown);
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

    // 构建 usage（注入影子缓存记账字段，让 Claude Code 显示 cache hits）
    let billed_input = if let Some(c) = final_cache_breakdown {
        super::stream::billed_input_tokens(
            final_input_tokens,
            c.cache_creation_input_tokens,
            c.cache_read_input_tokens,
        )
    } else {
        final_input_tokens
    };
    let mut usage = json!({
        // 客户端展示缩放（output_tokens 不缩放，避免影响 max_tokens 计算）
        "input_tokens": super::stream::scale_for_client(billed_input),
        "output_tokens": output_tokens
    });
    if let Some(c) = final_cache_breakdown {
        usage["cache_creation_input_tokens"] =
            json!(super::stream::scale_for_client(c.cache_creation_input_tokens));
        usage["cache_read_input_tokens"] =
            json!(super::stream::scale_for_client(c.cache_read_input_tokens));
    }
    // 是否需要标注「这些 cache 数字是网关估算」——仅当胜出层是估算（Layer 2/4）时标；
    // Layer 1 metering 真值或未下发字段时不标（头与体自相矛盾见 CACHE_ESTIMATED_HEADER）。

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

    let is_opus_4_6 = model_lower.contains("opus")
        && (model_lower.contains("4-6") || model_lower.contains("4.6"));

    let thinking_type = if is_opus_4_6 { "adaptive" } else { "enabled" };

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

    // 入站整形准入闸门（与 /v1 同闸）：2026-08-11 补。
    // 改前闸门在 provider.call_api_with_retry 内部，/cc/v1 的 Kiro 路径也过闸；
    // 移到 handler 层后曾漏掉这条入口，这里补回（websearch 与 Kiro 路径统一过闸）。
    if let Some(resp) = try_inbound_admission_gate(&provider, &payload.model, payload.stream, &client).await {
        return resp;
    }

    // 每客户端请求的共享上游预算（与 /v1 同款，2026-08-11 方案 A）。
    let retry_budget = crate::kiro::provider::SharedRetryBudget::new();

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

        return websearch::handle_websearch_request(provider, &payload, input_tokens, &retry_budget).await;
    }

    // 混合工具场景：请求带 web_search 但未显式触发搜索，剔除 web_search 后走常规转发，
    // 避免把 web_search 原样下发给 Kiro 触发 400 Improperly formed request。
    // 🔴 2026-08-09：混合工具场景**不再剔除** web_search。
    //
    // 改前剔除 ⇒ 上游模型完全看不到搜索工具 ⇒ Claude Code 的 WebSearch 在
    // "web_search + 其他工具"（CC 常态）下**静默失效**。现在交给 converter 把它
    // 归一化成 Kiro 认的函数工具形态（converter.rs 的 convert_tools + 内置
    // web_search schema），模型能看到、能调用，回 tool_use 时由**网关内部消化**：
    // 上游回 web_search tool_use 且本轮无其他工具 → agentic 回灌（内部调 MCP、
    // 结果回灌重发，最多 5 轮）；一旦混入非 web_search 工具 → 整轮原样回客户端。
    // 纯 web_search 与显式触发仍走上面的本地 MCP 快路径，不受影响。
    if let Some(resp) = dispatch_web_search_loop(&provider, &payload, &retry_budget, &client).await {
        return resp;
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
                ConversionError::UnsupportedToolMapping { tool_name, reason } => {
                    ("invalid_request_error", format!("工具参数无法映射: {} — {}", tool_name, reason))
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
    // 保留原始状态的克隆，供 CONTENT_LENGTH_EXCEEDS 重试时重建（渐进式压低 target_bytes，
    // 与 /v1 路径同款；2026-08-11 审计缺口补齐）。native effort 字段随请求体一起走，
    // 压缩只作用于 conversation_state，不受影响。
    let conv_state_for_compress_retry = conversion_result.conversation_state.clone();
    let native_fields_for_compress_retry =
        conversion_result.additional_model_request_fields.clone();
    let request_body = match build_kiro_request_body(
        conversion_result.conversation_state,
        conversion_result.additional_model_request_fields,
        &current_compression(),
        None,
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
    let prefix_tokens = token::count_prefix_tokens(payload.system.as_deref(), &payload.messages);
    // Layer 3 指纹（与 /v1 同款，2026-08-11 移植）。
    let fingerprint_usage = prompt_cache_enabled()
        .then(|| crate::anthropic::cache_fingerprint::compute_fingerprint_usage(&payload))
        .flatten();
    let cache_breakdown = fingerprint_usage
        .map(|u| u.clamp_to_total(input_tokens).to_cache_breakdown())
        .or_else(|| estimate_cache_breakdown(prompt_cache_enabled(), prefix_tokens, input_tokens));

    // 检查是否启用了thinking
    let thinking_enabled = payload
        .thinking
        .as_ref()
        .map(|t| t.is_enabled())
        .unwrap_or(false);

    let tool_name_map = conversion_result.tool_name_map;
    let known_tool_names = conversion_result.known_tool_names;
    // Bug C 校验用：工具必需参数表（转换层与 known_tool_names 同处提取、同口径短名）。
    let tool_required_fields = conversion_result.tool_required_fields;

    // 压缩重试循环（2026-08-11 补齐，与 /v1 同款语义）：上游 400 CONTENT_LENGTH_EXCEEDS
    // 时，网关用更低的压缩目标重建请求体重发。初试用配置阈值，重试时 target_bytes 按
    // (3/4)^attempt 逐轮压低（最多 3 次，下限 64 KiB），且受总墙钟预算约束（单轮内部
    // 有自己的 45s failover 预算，多轮叠乘需封顶）。常量语义与 /v1 完全一致：
    // 90 = 2×45s（初试一轮完整 failover 预算 + 至少一次完整重试预算），墙钟只在轮末
    // 检查，一轮内部可跑满 45s ⇒ 实际最坏 ≈ 135s，有界即可。
    const MAX_COMPRESS_RETRIES: u32 = 3;
    const MAX_COMPRESS_RETRY_BUDGET_SECS: u64 = 90;
    let compress_started = std::time::Instant::now();
    let mut compress_attempt: u32 = 0;
    let compression_cfg = current_compression();
    'compress_retry: loop {
        let response_body;

        // 仅在重试时重建请求体（初试已在上面构建好，直接复用 request_body）。
        let body_ref: &str = if compress_attempt == 0 {
            &request_body
        } else {
            let target = compress_retry_target(compression_cfg.trigger_bytes, compress_attempt);
            response_body = match build_kiro_request_body(
                conv_state_for_compress_retry.clone(),
                native_fields_for_compress_retry.clone(),
                &compression_cfg,
                Some(target),
            ) {
                Ok(b) => b,
                Err(e) => {
                    tracing::error!("压缩重试时序列化请求失败: {}", e);
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(ErrorResponse::new("internal_error", format!("序列化请求失败: {}", e))),
                    ).into_response();
                }
            };
            tracing::info!(
                attempt = compress_attempt,
                target_bytes = target,
                body_len = response_body.len(),
                "CONTENT_LENGTH_EXCEEDS: 重新压缩请求体并重试"
            );
            &response_body
        };

        let response = if payload.stream {
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
                    provider.clone(),
                    body_ref,
                    &payload.model,
                    input_tokens,
                    thinking_enabled,
                    tool_name_map.clone(),
                    known_tool_names.clone(),
                    tool_required_fields.clone(),
                    cache_breakdown.clone(),
                    &retry_budget,
                    client.clone(),
                )
                .await
            } else {
                tracing::debug!("/cc/v1 流式分发: 真流式（ccAutoBuffer=false，内容边到边转发）");
                handle_stream_request(
                    provider.clone(),
                    body_ref,
                    &payload.model,
                    input_tokens,
                    thinking_enabled,
                    tool_name_map.clone(),
                    known_tool_names.clone(),
                    tool_required_fields.clone(),
                    cache_breakdown.clone(),
                    &retry_budget,
                    client.clone(),
                )
                .await
            }
        } else {
            // 非流式响应：仅在配置开启时提取 thinking 块
            let extract_thinking = extract_thinking_enabled() && thinking_enabled;
            handle_non_stream_request(
                provider.clone(),
                body_ref,
                &payload.model,
                input_tokens,
                extract_thinking,
                tool_name_map.clone(),
                cache_breakdown.clone(),
                fingerprint_usage,
                &retry_budget,
                client.clone(),
            )
            .await
        };

        // 重试判定与 /v1 同款：次数未耗尽、墙钟预算内、且上游回的内部标记头在场。
        let is_compress_retryable = compress_attempt < MAX_COMPRESS_RETRIES
            && compress_started.elapsed()
                < std::time::Duration::from_secs(MAX_COMPRESS_RETRY_BUDGET_SECS)
            && response.headers().get("x-kirostudio-compress-retry").is_some();

        if is_compress_retryable {
            compress_attempt += 1;
            continue 'compress_retry;
        }

        // 重试已耗尽（或本轮不可重试）：内部标记头不得透传客户端（2026-08-11 F1b 同款，
        // 泄漏会误导客户端判据）。此前 /cc/v1 只 strip 不重试，随本次补齐一并迁移至此，
        // 超限请求现在有自愈重试，不再是「strip 后直接 400 返回」的已知缺口。
        let mut final_response = response;
        final_response
            .headers_mut()
            .remove("x-kirostudio-compress-retry");
        return final_response;
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
    // Bug C：工具必需参数表（工具名 → required 字段名列表）。空表 = 不校验。
    tool_required_fields: std::collections::HashMap<String, Vec<String>>,
    cache_breakdown: Option<CacheUsageBreakdown>,
    budget: &crate::kiro::provider::SharedRetryBudget,
    client: ClientInfo,
) -> Response {
    // 1M 变体:据原始模型名判定是否注入 anthropic-beta 头(仅受支持的 [1m] 变体为 true)。
    let is_1m = crate::anthropic::model_catalog::resolve_is_1m(model);
    // 调用 Kiro API（支持多凭据故障转移）
    let (response, meta) = match provider.call_api_stream(request_body, is_1m, budget).await {
        Ok(resp) => resp,
        Err(e) => return map_provider_error(e),
    };

    // 创建缓冲流处理上下文
    let mut ctx = BufferedStreamContext::new(
        model,
        estimated_input_tokens,
        thinking_enabled,
        tool_name_map,
        known_tool_names,
    );
    // 注入影子缓存估算（finish_and_get_all_events 回补 message_start 时会携带 cache 字段）
    ctx.set_cache_usage(cache_breakdown);
    // Bug C：注入工具必需参数表，启用「参数 JSON 合法但缺 required 字段」校验
    // （如 Bash 只给 description 没给 command）。空表 = 不校验，行为与改前一致。
    ctx.set_tool_required_fields(tool_required_fields);

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
                                // 空响应检测（buffered 路径同构）：正常完成但收尾兜底后仍无内容时，
                                // 返回显式 error 事件而非空 end_turn。
                                if ctx.completion().is_ok()
                                    && !ctx.error_event_emitted()
                                    && ctx.is_empty_response()
                                {
                                    let oversized = ctx.empty_response_is_oversized_context();
                                    tracing::warn!(
                                        oversized_context = oversized,
                                        "上游返回空响应（buffered 路径，收尾兜底后仍无内容），补发 error 事件"
                                    );
                                    all_events.push(empty_response_error_event(oversized));
                                    ctx.mark_error_event_emitted();
                                } else {
                                    all_events.extend(tail);
                                }
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
    // 双口径：requested = 客户端原始名，upstream = 映射后名。
    record.requested_model = meta.model.clone();
    record.upstream_model = meta.mapped_model.clone();
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
    // 中断字节：非流式 buffered 无「流中断」概念，恒 None（与流式埋点同模式对称）。
    record.interrupted_bytes = ctx.interrupted_bytes();
    record.retries = meta.retries;
    // 去硬编码 Success：按真实完成状态记账（截断/上游错误不再被记成成功）。
    record.outcome = ctx.completion_outcome();
    // 2026-08-11 补：与 emit_stream_usage 同款 —— 失败态补上错误详情，闭合 error_message
    // 恒 NULL 的盲区（线上 38 条实测）。成功态保持 NULL。
    if !ctx.completion().is_ok() {
        record.error_message = Some(ctx.completion().client_message());
    }
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

    /// Layer 1：上游 metering 真值优先于一切本地估算，且不标注「估算」。
    #[test]
    fn should_use_metering_truth_over_estimate() {
        let (bd, estimated) = resolve_cache_chain(true, 1000, None, None, Some(600), Some(200));
        let bd = bd.expect("metering 真值应产出记账");
        assert_eq!(bd.cache_read_input_tokens, 600);
        assert_eq!(bd.cache_creation_input_tokens, 200);
        assert_eq!(bd.cache_creation_5m_input_tokens, 200);
        assert_eq!(bd.cache_creation_1h_input_tokens, 0);
        assert!(!estimated, "真值不应标「估算」头");
    }

    /// Layer 1 真值 > total 时按 clamp_to_total 收敛（优先保留 read）。
    #[test]
    fn should_clamp_metering_truth_to_total() {
        let (bd, _) = resolve_cache_chain(true, 100, None, None, Some(80), Some(50));
        let bd = bd.expect("应有记账");
        assert_eq!(bd.cache_read_input_tokens, 80);
        assert_eq!(bd.cache_creation_input_tokens, 20);
    }

    /// Layer 1 真值不受 `promptCacheEnabled=false` 约束（真值不是估算）。
    #[test]
    fn should_record_metering_truth_even_when_disabled() {
        let (bd, estimated) = resolve_cache_chain(false, 1000, None, None, Some(400), Some(100));
        assert!(bd.is_some(), "真值应照记，即使开关关");
        assert!(!estimated);
    }

    /// 开关关且无 metering 真值 → 整体缺失（None），不凭空造 cache 命中。
    #[test]
    fn should_omit_entirely_when_disabled_and_no_metering() {
        let (bd, estimated) = resolve_cache_chain(false, 1000, Some(estimate_cache_breakdown(true, 500, 1000).unwrap()), None, None, None);
        assert!(bd.is_none(), "关闭时不得下发 cache 记账");
        assert!(!estimated);
    }

    /// Layer 2：无 metering 时回落 prefix 估算（既有行为）。
    #[test]
    fn should_fall_back_to_prefix_estimate() {
        let (bd, estimated) = resolve_cache_chain(true, 1000, Some(estimate_cache_breakdown(true, 400, 1000).unwrap()), None, None, None);
        let bd = bd.expect("prefix 估算应产出记账");
        assert_eq!(bd.cache_read_input_tokens, 400);
        assert_eq!(bd.cache_creation_input_tokens, 0);
        assert!(estimated, "估算应标「估算」头");
    }

    /// ⭐ Layer 3 回归（对抗审查 MAJOR 1，2026-08-11）：fingerprint 与 prefix 估算
    /// **同时存在**时必须走 Layer 3 —— fingerprint 的 creation 绝不能被 Layer 2 分支
    /// （只读 read、creation 硬置 0）吞掉。回退即 FAIL：把 resolve_cache_chain 里的
    /// `if fingerprint_usage.is_some() { None } else { ... }` 改回直接 map。
    #[test]
    fn fingerprint_wins_over_prefix_and_keeps_creation() {
        // 构造：prefix 估算 read=400（Layer 2 若赢：creation=0）；
        // fingerprint（Layer 3）：read=250、creation=120。
        let fp = crate::anthropic::cache::PromptCacheUsage {
            cache_creation_input_tokens: 120,
            cache_read_input_tokens: 250,
            cache_creation_5m_input_tokens: 120,
            cache_creation_1h_input_tokens: 0,
        };
        let (bd, estimated) = resolve_cache_chain(
            true,
            1000,
            Some(estimate_cache_breakdown(true, 400, 1000).unwrap()),
            Some(fp),
            None,
            None,
        );
        let bd = bd.expect("fingerprint 应产出记账");
        assert_eq!(
            bd.cache_creation_input_tokens, 120,
            "Layer 3 的 creation 不得被 Layer 2 吞成 0（非流式路径曾恒 0）"
        );
        assert_eq!(bd.cache_read_input_tokens, 250, "Layer 3 的 read 优先于 Layer 2 的 400");
        assert!(estimated);
    }

    /// Layer 4：无 metering、无 prefix 时 ratio 兜底（50% cache / 30% creation）。
    #[test]
    fn should_fall_back_to_ratio_when_no_estimate() {
        let (bd, estimated) = resolve_cache_chain(true, 1000, None, None, None, None);
        let bd = bd.expect("ratio 兜底应产出记账");
        // 50% × 1000 = 500 cache，creation = 150，read = 350。
        assert_eq!(bd.cache_read_input_tokens, 350);
        assert_eq!(bd.cache_creation_input_tokens, 150);
        assert!(estimated, "ratio 也是估算，应标「估算」头");
    }

    /// 🔴 源码级守卫：`contextUsageEvent` 的判据必须**与流式路径共用同一个函数**，
    /// 不得在本文件里重新算一遍。
    ///
    /// # 为什么需要这条
    ///
    /// 这个判据有两个调用点（流式 `StreamContext` / 非流式本文件的缓冲聚合循环）。
    /// 它们曾是两份独立实现，**只有流式那份有下界守卫** ⇒ 同一个上游异常在两条路径上
    /// 表现不同：流式忽略脏值，非流式把计费口径的 `input_tokens` 写成 0
    /// （或 NaN 经 `as i32` 饱和成的 `i32::MAX`）。
    ///
    /// 本仓已多次踩「同一判据两份实现、只修了其中一份」这个形态（`endpoint_for` 与
    /// `for_credentials`、`restart_fields` 与 reload restore 表、`cleanup_verdict` 与
    /// `batch-delete`）。给第二处也加个守卫治不了根 —— 两份实现仍会各自演化。
    /// 本守卫钉的是**物理共用**：本文件不得出现自己的百分比乘算。
    #[test]
    fn context_usage_predicate_must_be_shared() {
        let src = include_str!("handlers.rs");
        let cut = src.find("#[cfg(test)]").unwrap_or(src.len());
        // 只看生产段，且剔掉注释行（否则本守卫会匹配到注释里的说明文字或被注释掉的实现，
        // 变成「把实现注释掉守卫仍绿」的纸面测试 —— 该形态本轮实测踩过一次）。
        // ⚠️ 还要把连续空白归一成单空格。否则 rustfmt 在表达式中间插一个换行就能让
        // 下面的反向断言失配 ⇒ 守卫静默失效（本轮实测踩到：手工回退时那句乘算被格式化成
        // 三行，含换行的 needle 匹配不上，守卫报绿）。归一后断言与排版无关。
        let prod: String = src[..cut]
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");

        // needle 运行时拼接，避免 include_str! 把本测试自己的字面量算进匹配。
        let shared_call = format!("stream::context_input_tokens_from_pct{}", "(");
        assert!(
            prod.contains(&shared_call),
            "非流式路径必须调用共享判据 `context_input_tokens_from_pct`，\
             不得自行判定 —— 否则两条路径会再次分叉（只有一侧有下界守卫）"
        );

        // 反向断言：不得再出现自己的乘算。历史实现是
        // `context_usage.context_usage_percentage * (window_size as f64) / 100.0`。
        let own_math = format!("context_usage_percentage {}", "* (window_size as f64)");
        assert!(
            !prod.contains(&own_math),
            "本文件不得自行用百分比乘算 input_tokens（发现历史实现的形状）：\
             那正是下界守卫缺失的那一份。改为调用 `context_input_tokens_from_pct`。"
        );
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
            block.contains("apply_cache_breakdown(&mut record, final_cache_breakdown)"),
            "非流式成功埋点块必须写入 cache 字段(四层降级链收敛后的 final_cache_breakdown),否则落库与客户端 usage 矛盾"
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
        let _guard = BLOCKLIST_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
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
        let _guard = BLOCKLIST_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // 空黑名单:任何机器码都不拦。
        set_machine_code_blocklist(&[]);
        let code = crate::usage::machine_code_of(Some("223.73.32.14"), Some("claude-code"));
        assert!(!machine_code_is_blocked(&code));

        // 拉黑该机器码后命中。
        set_machine_code_blocklist(&[code.clone()]);
        assert!(machine_code_is_blocked(&code), "命中机器码应拦");
        // 大小写不敏感。
        assert!(
            machine_code_is_blocked(&code.to_uppercase()),
            "大写形式也应命中"
        );
        // 另一台机器(不同 IP → 不同码)不受影响。
        let other = crate::usage::machine_code_of(Some("8.8.8.8"), Some("claude-code"));
        assert!(!machine_code_is_blocked(&other), "未拉黑的机器码应放行");

        // 有 IP 时 device 不影响判定(machine_key = IP)。
        let same_ip_diff_dev = crate::usage::machine_code_of(Some("223.73.32.14"), Some("vscode"));
        assert!(
            machine_code_is_blocked(&same_ip_diff_dev),
            "同 IP 不同 device 仍应命中"
        );

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

        let _guard = BLOCKLIST_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());

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
        assert!(
            security_block_response(&headers, proxy_peer).is_none(),
            "未命中应放行"
        );

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

        let _guard = BLOCKLIST_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());

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

        let _guard = BLOCKLIST_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());

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
    /// ⭐ 回归（走真实 `map_provider_error` 出口）：400 `INSUFFICIENT_MODEL_CAPACITY`
    /// 必须映射成 **503 `overloaded_error`**，而不是兜底的 502。
    ///
    /// 这是与上面那条 429 缺陷**完全同型**的第二例，只是形态不同：
    /// 上游发的是 HTTP 400 + `ThrottlingException` + `reason:INSUFFICIENT_MODEL_CAPACITY`
    /// （实测 24h **272 次**）。它逐条落空所有分支 → 落末尾兜底 →
    /// **502 Bad Gateway 且无 Retry-After** → 客户端按永久性服务端故障处理 →
    /// 不退避、原样重发 → 在上游容量本就不足时继续加压。
    ///
    /// 断言落在**客户端实际看到的状态码**上，不是只断言谓词函数 ——
    /// 后者是纸面测试（本仓已踩过两次）：把 `translate_upstream_error` 里那条
    /// `|| err_str.contains("INSUFFICIENT_MODEL_CAPACITY")` 删掉，谓词测试仍会全绿。
    ///
    /// 删掉那条 → 本测试必 FAILED（实得 502）。
    #[test]
    fn insufficient_model_capacity_maps_to_503_not_bad_gateway() {
        let raw = r#"流式 API 请求失败: 400 Bad Request {"__type":"com.amazon.aws.codewhisperer#ThrottlingException","message":"I am experiencing high traffic, please try again shortly.","reason":"INSUFFICIENT_MODEL_CAPACITY"}"#;
        let resp = map_provider_error(anyhow::Error::msg(raw));
        assert_eq!(
            resp.status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "容量不足必须返 503（客户端会退避重试），而非兜底的 502（客户端当永久故障不退避）"
        );
        // 对照：既有的 503 形态必须仍然同样映射，不得因本次改动漂移。
        let legacy = r#"流式 API 请求失败: 503 Service Unavailable {"reason":"MODEL_TEMPORARILY_UNAVAILABLE"}"#;
        assert_eq!(
            map_provider_error(anyhow::Error::msg(legacy)).status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "既有 MODEL_TEMPORARILY_UNAVAILABLE 形态不得回退"
        );
    }

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
    /// ⭐ 上游 5xx / 传输层失败 → **503 + Retry-After**，不落未识别兜底的 502。
    ///
    /// 回退即 FAIL：删掉 `map_provider_error` 里那条 `is_upstream_transient_5xx` 分支 ——
    /// 这两类会逐条穿过所有已识别分支、落末尾兜底 → **502 且无 Retry-After** →
    /// 客户端（Claude Code）把 502 当服务端故障，退避逻辑压根不启动、原样重发。
    ///
    /// 实测量级（24h）：上游 `InternalServerException` 160 条 + 传输层失败 148 条，
    /// 其中 296 条 `retries=0` —— 因为 `compute_max_retries` 按池子大小算，
    /// 只剩 1 个可用号时算出的是 1（日志那句 `尝试 1/1`），所以上游一次 500
    /// **一次都没重试**就吐给客户端。网关侧重试预算要单独修（碰选号热路径），
    /// 但至少要让客户端知道该退避。
    #[test]
    fn upstream_5xx_and_transport_errors_map_to_503_with_retry_after() {
        for err_str in [
            // 线上原文（provider 格式化后的形态）
            r#"非流式 API 请求失败: 500 Internal Server Error {"__type":"com.amazon.aws.codewhisperer#InternalServerException","message":"Encountered an unexpected error when processing the request, please try again."}"#,
            r#"流式 API 请求失败: 502 Bad Gateway {"message":"upstream"}"#,
            r#"流式 API 请求失败: 503 Service Unavailable {"message":"x"}"#,
            r#"流式 API 请求失败: 504 Gateway Timeout {"message":"x"}"#,
            "error sending request for url (https://runtime.us-east-1.kiro.dev/generateAssistantResponse)",
        ] {
            let resp = map_provider_error(anyhow::Error::msg(err_str.to_string()));
            assert_eq!(
                resp.status(),
                StatusCode::SERVICE_UNAVAILABLE,
                "上游 5xx/传输层必须映射成 503（旧代码落兜底 502 无 Retry-After）: {err_str}"
            );
            assert!(
                resp.headers().get(header::RETRY_AFTER).is_some(),
                "必须带 Retry-After，否则客户端不退避: {err_str}"
            );
        }
    }

    /// ⭐ 顺序守卫：5xx 判据**绝不能**抢走已识别的分支，也不能误判 4xx。
    ///
    /// 回退即 FAIL：把 `is_upstream_transient_5xx` 那条 `if` 移到 `map_provider_error`
    /// 靠前的位置（例如 429/403/model-unsupported 之前）——那些本该拿 429/404 的错误
    /// 会被当成 5xx 返 503，客户端的退避语义整体错位。
    #[test]
    fn transient_5xx_branch_must_not_shadow_more_specific_ones() {
        // 429 仍必须是 429
        // 线上真实 429 原文（19855/19855 条都含小写 "Too many requests" 这句 message；
        // 判据故意不认 HTTP reason phrase 的大写 "Too Many Requests"，实测零漏判）。
        let r = map_provider_error(anyhow::Error::msg(
            r#"流式 API 请求失败: 429 Too Many Requests {"__type":"com.amazon.kiro.runtimeservice#ThrottlingException","message":"Too many requests, please wait before trying again.","reason":"USER_REQUEST_RATE_EXCEEDED"}"#.to_string(),
        ));
        assert_eq!(
            r.status(),
            StatusCode::TOO_MANY_REQUESTS,
            "429 不得被 5xx 分支抢走"
        );

        // 全池冷却（带 retry_after_secs=）仍必须是 429
        let r = map_provider_error(anyhow::Error::msg(
            "所有凭据均已禁用（0/2）retry_after_secs=10".to_string(),
        ));
        assert_eq!(r.status(), StatusCode::TOO_MANY_REQUESTS);

        // 模型永久不可用仍必须是 404 且无 Retry-After
        let r = map_provider_error(anyhow::Error::msg(
            "模型不被本号池支持 model_unsupported_by_pool=1".to_string(),
        ));
        assert_eq!(r.status(), StatusCode::NOT_FOUND);
        assert!(r.headers().get(header::RETRY_AFTER).is_none());

        // ⭐ 4xx 绝不能被误判成瞬态 5xx（判据只认确切的 5xx 字样，不裸匹配数字）
        for s in [
            r#"400 Bad Request {"requestId":"abc-500-def"}"#,
            r#"403 Forbidden {"message":"quota exceeded, 500 requests used"}"#,
        ] {
            assert!(
                !is_upstream_transient_5xx(s),
                "4xx 不得被当成可重试的 5xx（响应体里含 500 之类的数字很常见）: {s}"
            );
        }
    }

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

    /// ⭐ 致命缺陷回归（删掉分支即 FAIL）：**403 region 错配**曾落 502 兜底，实测 397 次。
    ///
    /// 旧路径：该串不带 `retry_after_secs=`、不含 `USER_REQUEST_RATE_EXCEEDED` /
    /// `Too many requests` / `temporarily is suspended`，也不含 `translate_quota_subscription`
    /// 认的首字母大写 `Invalid token`（上游写的是句末 `is invalid.`）→ 穿过所有分支 →
    /// 末尾兜底 **502 无 Retry-After** → 外挂 `kiro_shield.py`
    /// （`RETRYABLE={429,500,502,503,504}`）与客户端都按 5xx 盲退避重打，
    /// 而 `ksk_` token 按 region 授权、打错区恒 403，重打多少次都不会变。
    #[test]
    fn test_region_mismatch_403_maps_to_permission_error_not_502() {
        // provider 实际组装的错误串原文（`{api_type} API 请求失败: {status} {body}`）。
        for raw in [
            r#"流式 API 请求失败: 403 Forbidden {"__type":"com.amazon.aws.codewhisperer#AccessDeniedException","message":"The bearer token included in the request is invalid."}"#,
            r#"非流式 API 请求失败（所有凭据已用尽）: 403 Forbidden {"__type":"com.amazon.kiro.runtimeservice#AccessDeniedException","message":"The bearer token included in the request is invalid."}"#,
        ] {
            let resp = map_provider_error(anyhow::Error::msg(raw.to_string()));
            assert_eq!(
                resp.status(),
                StatusCode::FORBIDDEN,
                "region 错配型 403 必须映射成 403 permission_error（旧代码落兜底 502 → \
                 外挂按 5xx 盲退避重打一个永远不会变的授权错误）: {raw}"
            );
            assert!(
                resp.headers().get(header::RETRY_AFTER).is_none(),
                "region 错配不该带 Retry-After —— 给了就等于宣称「等一会儿会好」: {raw}"
            );
        }
    }

    /// 边界：判据必须**窄** —— 永久封禁串不得命中 region 错配分支。
    ///
    /// 若为了接住那 397 次而泛匹配 `AccessDeniedException` 或裸 403，账号真被永久封禁时
    /// 会被告知「改 region」，给出完全错误的排障动作，同时与
    /// `is_upstream_temporarily_suspended` 的窄判据互相拆台。
    #[test]
    fn test_region_mismatch_judgement_is_narrow() {
        // ① 永久封禁：同为 403 + AccessDeniedException，但不含 bearer-invalid 那句。
        let banned = r#"流式 API 请求失败: 403 Forbidden {"__type":"com.amazon.aws.codewhisperer#AccessDeniedException","message":"Your account has been permanently disabled for violating the terms of service."}"#;
        assert!(
            !is_upstream_region_mismatch_403(banned),
            "永久封禁不得被判成 region 错配（否则排障动作完全错，且掩盖真实故障）"
        );
        // ② 临时风控：同为 403 + AccessDeniedException，也不含那句。
        let suspended = r#"流式 API 请求失败: 403 Forbidden {"__type":"com.amazon.aws.codewhisperer#AccessDeniedException","message":"Your User ID (450334904897) temporarily is suspended."}"#;
        assert!(
            !is_upstream_region_mismatch_403(suspended),
            "临时风控不得被 region 分支抢走（它必须拿 429 + Retry-After）"
        );
        // ③ 401：token 本身死了 ≠ region 错了。处置是刷新/换号，不是改 region。
        //    与 `region_probe.rs::classify_probe_result` 的「401 排在 403 之前」同源。
        let dead_token = r#"流式 API 请求失败: 401 Unauthorized {"message":"The bearer token included in the request is invalid.","requestId":"403-ish-id"}"#;
        assert!(
            !is_upstream_region_mismatch_403(dead_token),
            "401 必须让路：token 死了要刷新/换号，不是改 region"
        );
        // ④ 裸 403 无任何 message：不得命中（判据要求那句确切文案）。
        assert!(!is_upstream_region_mismatch_403("403 Forbidden"));
    }

    /// provider 组装 bearer-invalid 型 403 时的**真实**错误串。
    ///
    /// 形状逐字取自 `provider.rs`：`"{api_type} API 请求失败: {status} {body}"`
    /// （`api_type` = `流式` / `非流式`，`status` 是 `StatusCode` 的 Display ⇒ `403 Forbidden`）。
    const REAL_BEARER_INVALID_403: &str = r#"流式 API 请求失败: 403 Forbidden {"__type":"com.amazon.aws.codewhisperer#AccessDeniedException","message":"The bearer token included in the request is invalid."}"#;

    /// ⭐ 顺序守卫：region 错配分支**绝不能**抢走 429 / 全池冷却 / 临时风控 / 模型不支持。
    ///
    /// 回退即 FAIL 的形态是「把 `is_upstream_region_mismatch_403` 那条 `if` 上移到
    /// `is_upstream_rate_limited` / `is_upstream_temporarily_suspended` 之前」——
    /// 那些本该拿 429 + Retry-After 的错误会变成不带退避的 403，客户端退避逻辑整体失效。
    ///
    /// 断言的是**分支顺序**而非分支内容：每个用例都先钉死「region 判据确实命中它」，
    /// 再断言 `map_provider_error` 仍返回那条更优先的分支的结果。少了前半句，
    /// 测试就退化成「本来也不会命中」的纸面断言。
    ///
    /// # 夹具的诚实说明（上一版是自己编的串，2026-08-06 重写）
    ///
    /// 每个用例的**两个半段各自都是真串**，逐字取自生产链路的 `format!`：
    /// - 上游响应型：`provider.rs` 的 `"{api_type} API 请求失败: {status} {body}"`；
    /// - 号池 bail 型：`token_manager.rs` 的 `"所有凭据均在冷却（{}/{}）retry_after_secs={}"`
    ///   与 `"模型 {:?} 不被本号池支持（{}/{} …）model_unsupported_by_pool=1"`。
    ///
    /// 但**拼接本身是测试构造的**，真实链路不会产出这种双命中串：`last_error` 每次只装
    /// 一个错误（号池 bail 与上游 body 是两条互斥来源，中间没有"；最后错误:"这种拼接）。
    /// 上一版夹具凭空造了那个拼接词和 `"detail"` 字段，这里改掉 —— 编的串与真串差一个
    /// 字段就可能让判据形同虚设，那时守卫看着绿实则没在守。
    ///
    /// 保留这条守卫的理由：承重点是**分支顺序**，而顺序在「某天上游 body 里同时提两件事」
    /// 或「某天有人把两个错误拼起来」时才暴露。用真半段拼出来的串是能触到该顺序的最小输入，
    /// 也是唯一能触到的 —— 所以拼接是刻意的，并在此写明它是构造的而非采集的。
    #[test]
    fn region_mismatch_branch_must_not_shadow_rate_limit_or_suspended() {
        // 先钉死：单独的真 bearer-invalid 串确实走 region 分支（403）。
        // 否则下面每条"不得被抢走"都可能只是因为 region 分支本来就不参与竞争。
        assert_eq!(
            map_provider_error(anyhow::Error::msg(REAL_BEARER_INVALID_403)).status(),
            StatusCode::FORBIDDEN,
            "前提：真 bearer-invalid 串单独出现时确实落 region 分支，下面的竞争才成立"
        );

        // ① 上游 429 真串（`token_manager` 之外，provider 把上游 body 原样带出）
        //    + bearer-invalid 真串：必须仍是 429 + Retry-After。
        let real_429 = r#"流式 API 请求失败: 429 Too Many Requests {"message":"Too many requests, please wait before trying again.","reason":"USER_REQUEST_RATE_EXCEEDED"}"#;
        let both_429 = format!("{real_429} / {REAL_BEARER_INVALID_403}");
        assert!(
            is_upstream_region_mismatch_403(&both_429),
            "前提：region 判据确实命中该串（否则下面的顺序断言是空的）"
        );
        let r = map_provider_error(anyhow::Error::msg(both_429));
        assert_eq!(
            r.status(),
            StatusCode::TOO_MANY_REQUESTS,
            "429 不得被 region 分支抢走：限流是可重试态，必须拿 429 + Retry-After"
        );
        assert!(r.headers().get(header::RETRY_AFTER).is_some());

        // ② 全池冷却真 bail 串（`token_manager.rs` 的 `所有凭据均在冷却（{}/{}）
        //    retry_after_secs={}`，全角括号、无任何后缀）+ bearer-invalid 真串：
        //    必须仍是 429 且用号池算出的精确秒数。
        let real_cooldown = "所有凭据均在冷却（0/3）retry_after_secs=14";
        let both_cooldown = format!("{real_cooldown} / {REAL_BEARER_INVALID_403}");
        assert!(
            is_upstream_region_mismatch_403(&both_cooldown),
            "前提：region 判据确实命中该串"
        );
        let r = map_provider_error(anyhow::Error::msg(both_cooldown));
        assert_eq!(
            r.status(),
            StatusCode::TOO_MANY_REQUESTS,
            "全池冷却不得被 region 分支抢走"
        );
        assert_eq!(
            r.headers()
                .get(header::RETRY_AFTER)
                .unwrap()
                .to_str()
                .unwrap(),
            "14",
            "全池冷却的精确 retry_after 不该被 region 分支吃掉"
        );

        // ③ 临时风控真串（线上原文，`temporarily is suspended` + `security precaution`）
        //    + bearer-invalid 真串：必须仍是 429 + Retry-After: 20。
        let real_suspended = r#"非流式 API 请求失败: 403 Forbidden {"__type":"com.amazon.aws.codewhisperer#AccessDeniedException","message":"Your User ID (186648603162) temporarily is suspended. We've locked your account as a security precaution. To restore access, please contact our support team to verify your identity: https://aws.amazon.com/contact-us/"}"#;
        let both_suspended = format!("{real_suspended} / {REAL_BEARER_INVALID_403}");
        assert!(
            is_upstream_region_mismatch_403(&both_suspended),
            "前提：region 判据确实命中该串"
        );
        let r = map_provider_error(anyhow::Error::msg(both_suspended));
        assert_eq!(
            r.status(),
            StatusCode::TOO_MANY_REQUESTS,
            "临时风控不得被 region 分支抢走：它自称 temporarily，是可恢复限时态"
        );
        assert_eq!(
            r.headers()
                .get(header::RETRY_AFTER)
                .unwrap()
                .to_str()
                .unwrap(),
            "20"
        );

        // ④ 模型永久不可用真 bail 串（`token_manager.rs` 那条，`{:?}` 让模型名带引号）
        //    + bearer-invalid 真串：必须仍是 404，不得被 region 分支抢走。
        let real_model = r#"模型 "claude-opus-5" 不被本号池支持（2/2 个号均因订阅档位或成本白名单不含该模型而被过滤，非号池耗尽，重试无效）model_unsupported_by_pool=1"#;
        let both_model = format!("{real_model} / {REAL_BEARER_INVALID_403}");
        assert!(
            is_upstream_region_mismatch_403(&both_model),
            "前提：region 判据确实命中该串"
        );
        let r = map_provider_error(anyhow::Error::msg(both_model));
        assert_eq!(
            r.status(),
            StatusCode::NOT_FOUND,
            "model_unsupported 不得被 region 分支抢走"
        );
    }

    /// ⭐ 收窄回归（删掉那条排除即 FAIL）：provider 已判为**瞬态抖动**的 bearer-invalid
    /// 不得被判成 region 错配。
    ///
    /// 依据（`provider.rs` 的 `bearer_invalid_but_proven`，判据 `has_ever_succeeded`）：
    /// - 从未成功过的号 → 真 region 错配（实测 3 个号共吃 17 次）；
    /// - 已成功过的号 → 抖动（实测 4 个号累计 3393 次成功、共吃 42 次）。
    /// 即这个串的**多数出现不是 region 错配**，而两者的上游文案逐字节相同 ——
    /// 只有 provider 分得出来，所以它把结论写成机器可读标记带出来。
    ///
    /// 收窄前的两个后果：
    /// ① 排障文案让管理员去查 region，而那个号的 region 是对的；
    /// ② 状态码 502 → 403。502 在外挂 `kiro_shield.py` 的
    ///    `RETRYABLE={429,500,502,503,504}` 内会被重试，403 是 4xx 不重试；
    ///    而这一类下一次重试大概率落到别的号上成功（实测 #481 成功率 93.9%）⇒
    ///    收窄等于把本该有的重试机会还回去。
    ///
    /// 夹具是 `provider.rs:1661` 的**真串**（`{api_type} API 请求失败（token 瞬态失效，
    /// 已冷却换号）bearer_invalid_transient=1: {status} {body}`），不是编的。
    #[test]
    fn provider_marked_transient_bearer_invalid_is_not_region_mismatch() {
        // provider 真串：流式与非流式两种 api_type 都要覆盖（只有前缀不同）。
        for raw in [
            r#"流式 API 请求失败（token 瞬态失效，已冷却换号）bearer_invalid_transient=1: 403 Forbidden {"__type":"com.amazon.aws.codewhisperer#AccessDeniedException","message":"The bearer token included in the request is invalid."}"#,
            r#"非流式 API 请求失败（token 瞬态失效，已冷却换号）bearer_invalid_transient=1: 403 Forbidden {"__type":"com.amazon.kiro.runtimeservice#AccessDeniedException","message":"The bearer token included in the request is invalid."}"#,
        ] {
            assert!(
                !is_upstream_region_mismatch_403(raw),
                "provider 已判瞬态，不得再判成 region 错配（排障方向会错，且 403 让外挂不再重试）: {raw}"
            );
            let resp = map_provider_error(anyhow::Error::msg(raw.to_string()));
            assert_ne!(
                resp.status(),
                StatusCode::FORBIDDEN,
                "瞬态抖动不得拿 403 —— 4xx 不在外挂 RETRYABLE 集内，一次抖动会固化成硬失败: {raw}"
            );
            // 落回兜底 502：它在 `RETRYABLE={429,500,502,503,504}` 内 ⇒ 会被重试，
            // 而下一跳大概率是另一个号 ⇒ 成功。这正是收窄要恢复的行为。
            assert_eq!(
                resp.status(),
                StatusCode::BAD_GATEWAY,
                "瞬态抖动应退回可重试路径（502 在外挂 RETRYABLE 集内）: {raw}"
            );
        }

        // 对照组（承重）：**不带**标记的同款上游文案仍必须判 region 错配 →
        // 证明收窄只切掉了带标记的那一类，没有把整条修复关掉。
        assert!(
            is_upstream_region_mismatch_403(REAL_BEARER_INVALID_403),
            "不带标记的 bearer-invalid（从未成功过的号）仍须判 region 错配"
        );
        assert_eq!(
            map_provider_error(anyhow::Error::msg(REAL_BEARER_INVALID_403)).status(),
            StatusCode::FORBIDDEN
        );
    }

    /// ⭐ 源码级守卫：`bearer_invalid_transient=1` 这个字面量必须在 provider 侧真的存在。
    ///
    /// 上面那条测试只证明「handlers 侧看见标记会排除」。若 provider 改名/改大小写/加空格，
    /// 排除会**静默失效**（回到误判）且编译不报错 —— 因为两侧靠字符串约定，没有类型联系。
    /// 本仓已因「判据在一层、承重点在另一层」踩过多次（见 endpoint 侧那条状态门守卫）。
    ///
    /// 锚点切掉注释行：本仓踩过五次「needle 命中注释里的散文」——
    /// provider 那处的注释里就写了这个字面量。
    #[test]
    fn provider_must_still_emit_the_transient_marker() {
        let src = include_str!("../kiro/provider.rs");
        let prod: String = src
            .split("#[cfg(test)]")
            .next()
            .expect("生产段应存在")
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            prod.contains(BEARER_INVALID_TRANSIENT_MARKER),
            "provider 必须仍在瞬态 bail 串里带 `{BEARER_INVALID_TRANSIENT_MARKER}` —— \
             改掉它 handlers 侧的排除会静默失效，回到「把健康号判成 region 错配」"
        );
        // 且必须与 `has_ever_succeeded` 那个二分在同一处：标记若被挪到别的分支，
        // 语义就从「已证明有效的号」变成别的东西，而排除逻辑不会察觉。
        let mi = prod
            .find(BEARER_INVALID_TRANSIENT_MARKER)
            .expect("上一条断言已保证存在");
        let window = &prod[mi.saturating_sub(1200)..mi];
        assert!(
            window.contains(&["has_ever_", "succeeded(ctx.id)"].concat()),
            "标记必须仍打在 `has_ever_succeeded` 那个二分的分支里 —— \
             否则它标的不再是「已证明有效的号」，而 handlers 侧照旧排除"
        );
    }

    /// 边界：region 错配**不可吸收**。
    ///
    /// 吸收层的对象是「等一会儿真的会好」的态。region 错配在单请求的 45s 预算内
    /// 等多久都不会变（要改配置或等探测器重选），吸收它只是占着客户端连接空转满预算。
    /// 这条同时防止将来有人顺手把它加进 `absorb_class_of`。
    #[test]
    fn region_mismatch_403_is_never_absorbable() {
        let raw = r#"流式 API 请求失败: 403 Forbidden {"__type":"com.amazon.aws.codewhisperer#AccessDeniedException","message":"The bearer token included in the request is invalid."}"#;
        assert!(
            is_upstream_region_mismatch_403(raw),
            "前提：region 判据确实命中该串"
        );
        assert!(
            absorb_class_of(raw).is_none(),
            "region 错配不可吸收：45s 预算内等多久都不会变（要改 region 或等探测重选）"
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

    /// 用户线上实测原文（逐字，未改一个字符）：图片声明 `image/png` 而字节是 jpeg。
    const REAL_IMAGE_MIME_MISMATCH: &str = r#"非流式 API 请求失败: 400 Bad Request {"__type":"com.amazon.aws.codewhisperer#ValidationException","message":"messages.2.content.1.image.source.base64: The image was specified using the image/png media type, but the image appears to be a image/jpeg image","reason":"IMAGE_MIME_MISMATCH"}"#;

    /// ⭐ 新增判据回归（删掉 `translate_context_input` 里那条即 FAIL）：
    /// `IMAGE_MIME_MISMATCH` 必须映射成 400 `invalid_request_error` 且带图片专属排障文案。
    ///
    /// 这个 reason 码此前**全仓零判据**，落通用兜底 → 客户端拿 502 `api_error`
    /// 「上游 API 调用失败（未识别错误）」：既说错了性质（这是客户端请求构造问题，
    /// 不是上游故障），又让它进了外挂 `kiro_shield.py` 的 `RETRYABLE` 集
    /// （`{429,500,502,503,504}`）⇒ 一个**重试永远不会变**的请求被重打到预算耗尽。
    ///
    /// 而它的主要价值是**度量**：`converter.rs` 的 `resolve_image_format` 已按 magic bytes
    /// 校正声明的 media_type，但若仍有边缘情况漏掉（magic 认不出而回退声明值等），
    /// 那些 400 会混进通用 `bad_request` 桶 ⇒ 无法回答「那条修干净了没有」。
    /// 这条判据是那条修复唯一的效果度量。
    #[test]
    fn image_mime_mismatch_maps_to_400_invalid_request_not_502() {
        // 判据本身（收口在 endpoint 侧，与 `default_is_*` 系列同处）。
        assert!(
            crate::kiro::endpoint::default_is_image_mime_mismatch(REAL_IMAGE_MIME_MISMATCH),
            "用户线上原文必须命中判据"
        );
        let resp = map_provider_error(anyhow::Error::msg(REAL_IMAGE_MIME_MISMATCH));
        assert_eq!(
            resp.status(),
            StatusCode::BAD_REQUEST,
            "IMAGE_MIME_MISMATCH 是请求构造问题，必须 400（旧路径落兜底 502 → \
             性质说错，且进外挂 RETRYABLE 集被反复重打）"
        );
        assert!(
            resp.headers().get(header::RETRY_AFTER).is_none(),
            "重试无效的错误绝不带 Retry-After"
        );
    }

    /// ⭐ 顺序守卫（承重）：`IMAGE_MIME_MISMATCH` **不得**抢走同为 400 的
    /// `INSUFFICIENT_MODEL_CAPACITY`。
    ///
    /// 两者都是 HTTP 400，但处置相反：容量不足必须拿 **503 `overloaded_error`**
    /// （可退避重试，实测 24h 272 次），图片格式错必须拿 **400**（重试无效）。
    /// 若把图片判据放到 `translate_quota_subscription` **之前**（或放宽成认
    /// `ValidationException` / 认 message 里的 `media type` 散文），容量不足会被说成
    /// 「你的图片格式错」：既误导用户，又让客户端不再退避 —— 而那正是本仓
    /// `INSUFFICIENT_MODEL_CAPACITY` 那批修复要解决的问题，等于把它退回去。
    ///
    /// 回退即 FAIL 的形态：把 `translate_upstream_error` 的 `.or_else` 链改成
    /// `translate_context_input(...).or_else(|| translate_quota_subscription(...))`。
    /// 断言的是**分支顺序**，不是判据内部形状 —— 后者对顺序缺陷完全不可见
    /// （本仓已因此让一条"三处都改对、四测全绿"的修复无效上线过）。
    #[test]
    fn image_mime_mismatch_must_not_shadow_capacity_400() {
        // 真容量串（线上原文）单独出现：必须仍是 503。
        let real_capacity = r#"流式 API 请求失败: 400 Bad Request {"__type":"com.amazon.aws.codewhisperer#ThrottlingException","message":"I am experiencing high traffic, please try again shortly.","reason":"INSUFFICIENT_MODEL_CAPACITY"}"#;
        assert_eq!(
            map_provider_error(anyhow::Error::msg(real_capacity)).status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "前提：容量 400 单独出现时确实拿 503，下面的竞争才成立"
        );

        // 双命中串（两个半段各自都是真串，拼接是测试构造的 —— 真实链路一次只带一个错误，
        // 但顺序缺陷只有这种输入触得到）：必须仍按容量处置返 503。
        let both = format!("{real_capacity} / {REAL_IMAGE_MIME_MISMATCH}");
        assert!(
            crate::kiro::endpoint::default_is_image_mime_mismatch(&both),
            "前提：图片判据确实命中该串（否则顺序断言是空的）"
        );
        assert_eq!(
            map_provider_error(anyhow::Error::msg(both)).status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "容量 400 不得被图片判据抢走：它必须拿 503 才会被客户端退避重试"
        );

        // 反向边界：图片判据只认 reason 字面量，不得因 `ValidationException` 泛匹配而
        // 吞掉别的校验错误（`TOOL_USE_RESULT_MISMATCH` 也是这个 `__type`）。
        let tool_mismatch = r#"非流式 API 请求失败: 400 Bad Request {"__type":"com.amazon.aws.codewhisperer#ValidationException","message":"...","reason":"TOOL_USE_RESULT_MISMATCH"}"#;
        assert!(
            !crate::kiro::endpoint::default_is_image_mime_mismatch(tool_mismatch),
            "同 __type 的其它校验错误不得被图片判据吞掉（处置不同，混判会给错排障方向）"
        );
    }

    /// 线上实测原文（passthrough.rs 注释里的样本，逐字）：同一 body 里 message 与
    /// reason 两个信号并存 —— 判据必须同时认，认其一漏其二。
    const REAL_REQUEST_BODY_INVALID: &str = r#"非流式 API 请求失败: 400 Bad Request {"__type":"com.amazon.aws.codewhisperer#ValidationException","message":"Invalid tool use format.","reason":"REQUEST_BODY_INVALID"}"#;

    #[test]
    fn request_body_invalid_maps_to_400_invalid_request_not_502() {
        use axum::body::to_bytes;
        // 判据本身（收口在 endpoint 侧，与 `default_is_*` 系列同处）。
        assert!(
            crate::kiro::endpoint::default_is_request_body_invalid(REAL_REQUEST_BODY_INVALID),
            "线上原文必须命中判据"
        );
        let resp = map_provider_error(anyhow::Error::msg(REAL_REQUEST_BODY_INVALID));
        assert_eq!(
            resp.status(),
            StatusCode::BAD_REQUEST,
            "请求体校验失败是请求构造问题，必须 400（旧路径落兜底 502 → 性质说错，\
             且 502 在外挂 RETRYABLE 集里会被反复重打同一个必败的请求）"
        );
        assert!(
            resp.headers().get(header::RETRY_AFTER).is_none(),
            "重试无效的错误绝不带 Retry-After"
        );
        let body = futures::executor::block_on(to_bytes(resp.into_body(), usize::MAX)).unwrap();
        let text = String::from_utf8_lossy(&body);
        assert!(
            text.contains("invalid_request_error"),
            "错误类型必须是 invalid_request_error（客户端按类型决定是否重试）。实际: {text}"
        );
    }

    /// 判据边界：`ValidationException` 是上游多种校验共用（TOOL_USE_RESULT_MISMATCH 等），
    /// 泛匹配会把处置不同的错误混成一类 —— 判据只认两个专用信号，不得泛认。
    #[test]
    fn request_body_invalid_predicate_never_matches_bare_validation_exception() {
        let bare = r#"非流式 API 请求失败: 400 Bad Request {"__type":"com.amazon.aws.codewhisperer#ValidationException","message":"anything","reason":"SOME_UNMAPPED_REASON"}"#;
        assert!(
            !crate::kiro::endpoint::default_is_request_body_invalid(bare),
            "不带两个专用信号的 body 不得命中（否则把处置不同的校验错误混成一类）"
        );
        assert_eq!(
            map_provider_error(anyhow::Error::msg(bare)).status(),
            StatusCode::BAD_GATEWAY,
            "未映射错误仍落 502 兜底 —— 证明本判据没把别的 400 抢走"
        );
    }

    /// M1 形态（对抗审查）：`Improperly formed request` + `reason=REQUEST_BODY_INVALID`
    /// 是**用户请求体**格式校验失败的常见形态（converter.rs/websearch.rs 实测：工具 schema
    /// 属性、工具名超限、web_search 直发）。改前它被凭据分类分支（`Improperly formed` 子串）
    /// 截胡成 502「上游拒绝凭据」——排障方向全错。必须 400 invalid_request_error。
    #[test]
    fn improperly_formed_body_invalid_maps_to_400_not_502_credential_rejection() {
        let raw = r#"流式 API 请求失败: 400 Bad Request {"__type":"com.amazon.aws.codewhisperer#ValidationException","message":"Improperly formed request.","reason":"REQUEST_BODY_INVALID"}"#;
        assert!(
            crate::kiro::endpoint::default_is_request_body_invalid(raw),
            "判据必须命中该形态"
        );
        let resp = map_provider_error(anyhow::Error::msg(raw));
        assert_eq!(
            resp.status(),
            StatusCode::BAD_REQUEST,
            "用户请求体的格式校验失败必须 400，不得被说成凭据/订阅问题（502）"
        );
    }

    /// 顺序守卫（承重，m2）：请求体校验分支**不得**抢走 `INSUFFICIENT_MODEL_CAPACITY`
    /// 的 503。与 `image_mime_mismatch_must_not_shadow_capacity_400` 同款——容量 400
    /// 必须拿 503 `overloaded_error` 客户端才会退避重试；顺序靠 `.or_else` 链保证
    /// （quota 链先执行），回退即 FAIL。
    #[test]
    fn request_body_invalid_must_not_steal_capacity_400_503() {
        let capacity = r#"400 Bad Request {"__type":"com.amazon.aws.codewhisperer#ThrottlingException","message":"I am experiencing high traffic, please try again shortly.","reason":"INSUFFICIENT_MODEL_CAPACITY"}"#;
        assert_eq!(
            map_provider_error(anyhow::Error::msg(capacity)).status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "前提：容量 400 单独出现时确实拿 503"
        );
        let both = format!("{capacity} / {REAL_REQUEST_BODY_INVALID}");
        assert!(
            crate::kiro::endpoint::default_is_request_body_invalid(&both),
            "前提：请求体判据确实命中该串（否则顺序断言是空的）"
        );
        assert_eq!(
            map_provider_error(anyhow::Error::msg(both)).status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "容量 400 不得被请求体校验分支抢走：它必须拿 503 才会被客户端退避重试"
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
        assert!(!is_upstream_rate_limited(
            "CONTENT_LENGTH_EXCEEDS_THRESHOLD"
        ));
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

    /// ⭐ BLOCKER 2 守卫：入站准入超时**绝不可吸收**。
    ///
    /// 回退即 FAIL：删掉 `absorb_class_of` 里第一条 `inbound_admission_timeout=1 → None`，
    /// 该串会落到下面的 `retry_after_secs=` 分支被判成 `PoolCooldown` → 吸收层去重试
    /// **网关自己的背压信号**：把同一个请求塞回同一个已经满的桶，队列更长、客户端等更久，
    /// 且拿不到任何额外成功概率（实测 2 轮 × 30s = 客户端等 60s 才拿到 429，正确是 <2s）。
    #[test]
    fn admission_timeout_is_never_absorbable() {
        // provider.rs:820 那条 bail 的原文形态（同时带两个标记）。
        let s = "入站限速排队超时(网关目标 300 RPM 保护上游)inbound_admission_timeout=1 retry_after_secs=3";
        assert!(
            absorb_class_of(s).is_none(),
            "准入超时必须不可吸收；它与全池冷却共用 retry_after_secs= 标记，\
             靠 inbound_admission_timeout=1 这道显式判据区分"
        );
        // 对照组：同样带 retry_after_secs= 但**不带**准入标记的全池冷却，必须可吸收。
        assert_eq!(
            absorb_class_of("所有凭据均在冷却（0/1）retry_after_secs=3"),
            Some(AbsorbClass::PoolCooldown(3)),
            "全池冷却是「上游稍后真的会好」，必须可吸收（否则吸收层没有任何作用对象）"
        );
    }

    /// ⭐ 顺序守卫：`model_unsupported_by_pool=1` 永久不可吸收，且判据必须排在
    /// `retry_after_secs=` **之前**。
    ///
    /// 回退即 FAIL：删掉那条 `None`，或把它移到 `retry_after_secs=` 之后 —— 后者更隐蔽：
    /// 「模型级过滤但可恢复」那条 bail **带** `retry_after_secs=`，顺序反了就会把
    /// **永久**不可用当成可恢复态反复吸收，等于把 404 死循环搬进网关。
    #[test]
    fn model_unsupported_by_pool_is_never_absorbable() {
        // token_manager 那条 bail 的原文形态（不带 retry_after_secs）。
        let permanent = "模型 \"claude-opus-5\" 不被本号池支持（0/1 个号均因订阅档位或成本白名单不含该模型而被过滤，非号池耗尽，重试无效）model_unsupported_by_pool=1";
        assert!(
            absorb_class_of(permanent).is_none(),
            "模型对号池永久不可用时重试无效，必须不可吸收"
        );
        // ⭐ 承重：两个标记同时出现时，永久态必须赢（顺序守卫）。
        let both = "模型不被本号池支持 model_unsupported_by_pool=1 retry_after_secs=30";
        assert!(
            absorb_class_of(both).is_none(),
            "同时带 model_unsupported_by_pool=1 与 retry_after_secs= 时必须判不可吸收 —— \
             说明永久态判据排在 retry_after_secs 之前"
        );
    }

    /// ⭐ 池**永久**耗尽不可吸收，且判据必须排在 `retry_after_secs=` **之前**。
    ///
    /// 回退即 FAIL：删掉 `absorb_class_of` 里 `pool_permanently_exhausted=1 → None`
    /// （或把它移到 `retry_after_secs=` 之后），该串会被判成 `PoolCooldown(10)` →
    /// 吸收层对一个**一个可自愈的号都没有**的池（全 QuotaExhausted /
    /// RefreshTokenInvalid / AccountSuspended）拿满 45s 预算空转，客户端从 <2s
    /// 拿到 429 变成 45s 才拿到，且这 45s 内它一直占着连接。
    #[test]
    /// 🔴 **跨系统契约守卫**：两处 503 的文案必须含 shield 的 `COOLING_MARKERS` 词。
    ///
    /// 线上真实链路 `Caddy → kiro_shield.py(外挂,不在本仓) → KiroStudio`。
    /// shield 的 `classify()` **按 body 文案分类而非状态码**，且只有 `verdict ∈ {cool,auth}`
    /// 才读我们的 `Retry-After`：
    /// ```text
    /// if verdict in ("cool","auth"): delay = cool_delay(attempt, Retry-After)   # 听真值
    /// else:                          delay = swap_delay(attempt)                # 本地阶梯
    /// ```
    /// 文案里少了 marker 词 ⇒ 落 `retry` 兜底 ⇒ **精心算出的 Retry-After 被整个丢弃**，
    /// 改走 20→60s 阶梯 ⇒ 等真实恢复时间的 2~6 倍。
    /// CLAUDE.md 记录同款事故：当晚 1753 次失败。
    ///
    /// 回退即 FAIL：把文案里的「等容量」删掉 → 本测试红。
    /// ⚠️ 若 shield 的 `COOLING_MARKERS` 变更，需同步本测试
    /// （核对命令：`ssh skiapi 'grep -A12 COOLING_MARKERS /opt/skiapi/services/kiro_shield.py'`）。
    #[test]
    fn absorb_503_body_must_carry_shield_cooling_marker() {
        let src = include_str!("handlers.rs");
        let prod = src.split("\n#[cfg(test)]").next().unwrap_or(src);

        // shield COOLING_MARKERS 里我们实际使用的那个词（2026-08-11 线上实读核对）。
        // 运行时拼接，避免本测试段自身的字面量让 `find` 命中测试代码而非生产代码
        // —— 本仓已发生过两次这类「守卫静默变绿」事故。
        let marker: String = ["等", "容量"].concat();

        // ⚠️ **只钉「网关自己把预算用光」这两处**，不是所有 503。
        //
        // 生产区共 6 处 503，另外 4 处刻意**不该**带这个 marker：
        //   · 上游模型过载（`MODEL_TEMPORARILY_UNAVAILABLE` / `INSUFFICIENT_MODEL_CAPACITY`）
        //     —— shield 对该文案有自己的判据，语义是"上游没容量"而非"网关在等容量"；
        //   · 兜底 5xx（`is_retryable_upstream_error` 捡剩下的）—— 语义是"上游挂了"；
        //   · Provider 未初始化 ×2（启动期）—— 与退避无关，客户端重试也没用。
        // 把它们一起钉进来会强迫无关文案携带误导词，那是过度约束。
        //
        // 锚点用「网关已就该请求」——两处 503 文案共有的前缀，语义即"网关自身预算耗尽"。
        let anchor: String = ["网关已就该请求"].concat();
        let n = prod.matches(anchor.as_str()).count();
        assert_eq!(
            n, 2,
            "预期恰好 2 处「网关自身预算耗尽」型 503（共享预算耗尽 + 吸收层耗尽），实际 {n} 处。\n\
             若你新增/删除了这类 503，请同步本守卫；若只是改了文案前缀，请改回或更新 anchor。"
        );

        // ⚠️ 用 `match_indices` 而不是「find 后 at = i + 1」：源码含大量中文，
        // `i + 1` 会落进 UTF-8 多字节字符内部，切片直接 panic
        // （`start byte index ... is not a char boundary`）。本测试初版就是这么挂的。
        let hits: Vec<usize> = prod.match_indices(anchor.as_str()).map(|(i, _)| i).collect();
        for (k0, &i) in hits.iter().enumerate() {
            let k = k0 + 1;
            // 文案是多行字符串续行拼接，marker 落在 anchor 之后数十字符内。
            // ⚠️ 不要用「字节窗口 + char_indices 求上界」那种写法：源码是 CJK 密集的，
            // 字节窗口换算成字符数会缩到 1/3，且 `.last()` 拿到的是字符**起点**，
            // 可能刚好切在 marker 前面 —— 本测试初版就是这么误红的。
            // 直接按**字符**取窗口（`chars().take(N)`），既安全又与直觉一致。
            let window: String = prod[i..].chars().take(240).collect();
            assert!(
                window.contains(marker.as_str()),
                "第 {k} 处「网关自身预算耗尽」503（字节偏移 {i}）的文案里找不到 shield \
                 COOLING_MARKERS 词「{marker}」。\n\
                 后果：shield 的 classify() 判它为 `retry` 而非 `cool` ⇒ \
                 丢弃我们的 Retry-After，改走 20→60s 本地阶梯 ⇒ 客户端等真实恢复时间的 2~6 倍。\n\
                 修法：在该 503 的 message 文案里加上「{marker}」。"
            );
        }
    }

    #[test]
    fn permanently_exhausted_pool_is_never_absorbable() {
        // token_manager 两处 bail 的原文形态（**带** retry_after_secs=，因为对客户端
        // 而言 429 + Retry-After 仍是对的：人工补号后确实会好）。
        let dead = "所有凭据均已禁用（0/2）pool_permanently_exhausted=1 retry_after_secs=10";
        assert!(
            absorb_class_of(dead).is_none(),
            "池里没有任何可自愈的号时，单请求预算内等多久都不会变，必须不可吸收"
        );
        // 对照组：同样是「全禁用」，但有可自愈的号 ⇒ 不带该标记 ⇒ 必须可吸收。
        // 这是 pool-empty 类占 24h 流量 16.5% 的那一大类，吸收层的主要作用对象。
        assert_eq!(
            absorb_class_of("所有凭据均已禁用（0/2）retry_after_secs=10"),
            Some(AbsorbClass::PoolCooldown(10)),
            "可自愈的全禁用必须可吸收，否则吸收层对最大的一类失败没有作用"
        );
    }

    /// 永久耗尽对**客户端**仍必须是 429 + Retry-After —— 只对「单请求内重试」说不。
    ///
    /// 回退即 FAIL：若有人把该标记也加进 `map_provider_error` 的早退分支、
    /// 或把它渲染成 404/502，这条断言会失败。人工补号后池子确实会恢复，
    /// 所以客户端该退避重试；不可吸收针对的只是**同一条请求内**的重试。
    #[test]
    fn permanently_exhausted_pool_still_renders_429_to_client() {
        let resp = map_provider_error(anyhow::anyhow!(
            "所有凭据均已禁用（0/2）pool_permanently_exhausted=1 retry_after_secs=10"
        ));
        assert_eq!(
            resp.status(),
            StatusCode::TOO_MANY_REQUESTS,
            "对客户端必须仍是 429（人工补号后会恢复，客户端该退避）"
        );
        assert_eq!(
            resp.headers()
                .get(header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok()),
            Some("10"),
            "必须带 Retry-After，否则客户端把它当服务端故障、退避逻辑不启动"
        );
    }

    /// 不可重试类一律不吸收：月度配额耗尽 / 传输层故障 / 普通 4xx / 未知。
    ///
    /// 回退即 FAIL：把 `is_upstream_temporarily_suspended` 放宽成裸 403 或
    /// `AccessDeniedException`，配额串与永久封禁串会被判成可吸收 → 对一个永远不会恢复的号
    /// 反复重试，同时把真实故障藏起来。
    #[test]
    fn non_retryable_errors_are_not_absorbable() {
        for s in [
            "流式 API 请求失败: 429 Too Many Requests {\"reason\":\"MONTHLY_REQUEST_COUNT\"}",
            "error sending request for url (https://runtime.eu-central-1.kiro.dev)",
            "流式 API 请求失败: 400 Bad Request {\"message\":\"Improperly formed request\"}",
            "某个谁也没见过的错误",
        ] {
            assert!(
                absorb_class_of(s).is_none(),
                "不可重试类必须不吸收，但 {s:?} 被判成了 {:?}",
                absorb_class_of(s)
            );
        }
    }

    /// 可吸收的两类正例 + 403 临时风控被单独归类（是否真吸收由配置决定，不在分类器里判）。
    #[test]
    fn retryable_upstream_errors_are_classified() {
        assert_eq!(
            absorb_class_of("流式 API 请求失败: 429 {\"reason\":\"USER_REQUEST_RATE_EXCEEDED\"}"),
            Some(AbsorbClass::UpstreamRateLimit)
        );
        assert_eq!(
            absorb_class_of(
                "403 Forbidden {\"message\":\"Your User ID (450334904897) temporarily is suspended.\"}"
            ),
            Some(AbsorbClass::SwapWindow),
            "403 临时风控（换号空窗）要能被识别出来（默认不吸收，但必须可分类，否则配置开了也没用）"
        );
    }

    /// ⭐ 合并外挂缺口 1：**上游 5xx 可分类**（此前对所有 5xx 返 None ⇒ 吸收层完全不覆盖）。
    ///
    /// 依据：外挂 `RETRYABLE={429,500,502,503,504}`，注释原文「500/502/503/504 = 网关/上游抖动，
    /// 也含『凭据全禁用』这类换号空窗」，且线上 shield 日志实见
    /// `502 -> wait 1.0s, attempt 1/60`。
    ///
    /// 回退即 FAIL：删掉 `absorb_class_of` 里那条 `is_upstream_transient_5xx` 分支。
    ///
    /// 夹具是 provider 真实组装的串：`"{api_type} API 请求失败: {status} {body}"`
    /// （provider.rs 通用 5xx 分支，`api_type` = 流式/非流式，`status` 是 `StatusCode` 的 Display）。
    #[test]
    fn transient_5xx_is_absorbable_but_transport_failure_is_not() {
        for raw in [
            "流式 API 请求失败: 502 Bad Gateway <html>502 Bad Gateway</html>",
            r#"非流式 API 请求失败: 500 Internal Server Error {"__type":"com.amazon.aws.codewhisperer#InternalServerException","message":"Internal server error"}"#,
            "流式 API 请求失败: 504 Gateway Timeout upstream timed out",
        ] {
            assert_eq!(
                absorb_class_of(raw),
                Some(AbsorbClass::TransientServerError),
                "上游 5xx 必须可分类（外挂把它们放进 RETRYABLE 且线上实见 502）: {raw}"
            );
        }
        // ⭐ 边界（承重）：**传输层**失败仍必须不可吸收。provider 内部换号已把每个号各试过
        // 一遍，吸收层再套一层只是把同一个网络故障重打 N 遍。
        // 回退即 FAIL：把分类器里那条 `&& !is_transport_error(...)` 删掉。
        for raw in [
            "error sending request for url (https://runtime.eu-central-1.kiro.dev)",
            "error trying to connect: dns error: failed to lookup address information",
        ] {
            assert!(
                absorb_class_of(raw).is_none(),
                "传输层故障不可吸收（provider 内部换号已覆盖）: {raw}"
            );
        }
    }

    /// ⭐ 合并外挂缺口 2：**带瞬态标记的 400 可分类**，其余 400 一律不吸收。
    ///
    /// 依据（外挂注释原文，带实测）：「Kiro 会把一部分瞬态故障塞进 400，跟『请求写错了』同一个
    /// 状态码。实测 6 小时样本里 400 共 165 次，其中容量类 101 次、格式错 80 次。只认这些明确的
    /// 瞬态标记，其余 400 一律透传，避免把真正的格式错误重试 60 次。」
    ///
    /// 判据**复用既有谓词** `endpoint::default_is_model_temporarily_unavailable`
    /// （认 `MODEL_TEMPORARILY_UNAVAILABLE` / `INSUFFICIENT_MODEL_CAPACITY`），不新写匹配。
    ///
    /// 回退即 FAIL：删掉分类器里那条容量分支 → 400 容量类落 None、503 容量类被 5xx 抢走。
    #[test]
    fn transient_capacity_400_is_absorbable_but_real_bad_request_is_not() {
        // provider.rs 容量分支的真实串（上游原文逐字，见 endpoint/mod.rs 该谓词处的实测记录）。
        let capacity_400 = r#"流式 API 请求失败（模型暂时不可用，建议稍后重试）: 400 Bad Request {"__type":"com.amazon.aws.codewhisperer#ThrottlingException","message":"I am experiencing high traffic, please try again shortly.","reason":"INSUFFICIENT_MODEL_CAPACITY"}"#;
        assert_eq!(
            absorb_class_of(capacity_400),
            Some(AbsorbClass::TransientCapacity400),
            "400 + INSUFFICIENT_MODEL_CAPACITY 是**瞬态**容量问题，必须可分类"
        );
        // ⭐ 顺序守卫：容量类的另一种上游形态是 **503**，必须仍判容量类而**不是** 5xx。
        // 回退即 FAIL：把容量分支移到 5xx 分支之后 —— 那条 5xx 判据认
        // `503 service unavailable` 字样，会把这一串抢走并套上 1s 起的短曲线。
        let capacity_503 = r#"非流式 API 请求失败（模型暂时不可用，建议稍后重试）: 503 Service Unavailable {"reason":"MODEL_TEMPORARILY_UNAVAILABLE"}"#;
        assert_eq!(
            absorb_class_of(capacity_503),
            Some(AbsorbClass::TransientCapacity400),
            "503 形态的容量类必须仍归容量（它与 400 形态处置相同）——说明容量判据排在 5xx 之前"
        );
        // 真格式错的 400（实测 6h 内 80 次）必须仍不可吸收：重试 60 次也永远不会成功。
        for raw in [
            r#"流式 API 请求失败: 400 Bad Request {"__type":"com.amazon.aws.codewhisperer#ValidationException","message":"Improperly formed request"}"#,
            r#"非流式 API 请求失败: 400 Bad Request {"reason":"IMAGE_MIME_MISMATCH"}"#,
        ] {
            assert!(
                absorb_class_of(raw).is_none(),
                "真格式错的 400 必须不可吸收（重试永远不会成功）: {raw}"
            );
        }
        // 裸 `ThrottlingException`（无 reason）**刻意不认**：那个 __type 被真限流共用。
        assert!(
            absorb_class_of(
                r#"流式 API 请求失败: 400 Bad Request {"__type":"ThrottlingException"}"#
            )
            .is_none(),
            "裸 ThrottlingException 不得被判成容量类（外挂白名单认它，本仓刻意不认：\
             `USER_REQUEST_RATE_EXCEEDED` 真限流共用同一个 __type）"
        );
    }

    /// ⭐ 顺序守卫（承重，外挂 2026-08-04 实测踩过的坑）：
    /// **`PoolCooldown` 判据必须排在 `SwapWindow` 之前**。
    ///
    /// 外挂原文：「全池不可用时返回 429 + `Retry-After: 10`，body 是
    /// "All credentials are temporarily cooling down..."，而 `"All credentials"` 原先挂在
    /// SWAP_WINDOW_MARKERS 里 → 判 swap → 套了长阶梯 → 本该等 10 秒的等了几十秒。」
    ///
    /// 即：号池冷却必须**听网关算出的真值**，换号空窗才用 20~60s 长阶梯。两者混判的代价是
    /// 客户端白等几十秒。
    ///
    /// 本测试有两道断言，第二道是源码级顺序守卫 —— 因为第一道只能证明「当前判据不冲突」，
    /// 证明不了「顺序对」（这正是本仓第 8 种纸面测试形态：测了分支内部，没测分支顺序）。
    #[test]
    fn pool_cooldown_wins_over_swap_window_ordering() {
        // 夹具：同时带 `retry_after_secs=`（号池真值）与 suspend 字样的串。
        // 真实来源：吸收轮之间 `last_error` 刻意不重置，某一轮拿到 403 风控、下一轮拿到全池
        // 冷却 bail 时，两种特征会先后出现在同一条请求的错误链上；而 KiroStudio 作为上游被
        // 串联时（custom_api 代挂），它自己渲染的 429 body 就带 "temporarily cooling down"。
        let both = "所有凭据均在冷却（0/4）retry_after_secs=10 \
                    上游原文: 403 Forbidden {\"message\":\"Your User ID temporarily is suspended.\"}";
        assert_eq!(
            absorb_class_of(both),
            Some(AbsorbClass::PoolCooldown(10)),
            "同时带号池真值与 suspend 字样时，**必须**判 PoolCooldown 并用真值 10s —— \
             判成 SwapWindow 会套 20~60s 长阶梯，本该等 10 秒的等几十秒（外挂实测踩过）"
        );

        // ⭐ 源码级顺序守卫：`parse_retry_after_secs`（PoolCooldown）必须出现在
        // `is_upstream_temporarily_suspended`（SwapWindow）之前。
        // 回退即 FAIL：把 SwapWindow 那条分支上移到 `retry_after_secs=` 之前。
        let body = absorb_class_of_source();
        let pool_at = body
            .find("parse_retry_after_secs")
            .expect("分类器必须仍用 parse_retry_after_secs 判 PoolCooldown");
        let swap_at = body
            .find("is_upstream_temporarily_suspended")
            .expect("分类器必须仍用既有谓词判 SwapWindow（不新写字符串匹配）");
        assert!(
            pool_at < swap_at,
            "PoolCooldown 判据必须排在 SwapWindow 之前（听网关真值 vs 套长阶梯，混判即白等）"
        );
    }

    /// ⭐ 顺序守卫（承重）：新增的三条判据必须全部排在**三条 `None`** 之后。
    ///
    /// 那三条是「网关自己的背压」（`inbound_admission_timeout=1`）与两种「永久态」
    /// （`model_unsupported_by_pool=1` / `pool_permanently_exhausted=1`）。任何通用判据排到
    /// 它们前面，都会把不该重试的东西吸收掉。
    ///
    /// 回退即 FAIL：把任一条新判据上移到那三条 `None` 之前。
    #[test]
    fn new_absorb_predicates_come_after_the_three_none_gates() {
        let body = absorb_class_of_source();
        // 三道 None 各自的位置（按机器可读标记定位，不依赖注释文案）。
        let gates = [
            "inbound_admission_timeout=1",
            "model_unsupported_by_pool=1",
            "pool_permanently_exhausted=1",
        ]
        .map(|m| {
            body.find(m)
                .unwrap_or_else(|| panic!("分类器必须仍有 {m} 这道 None 闸门"))
        });
        let last_gate = *gates.iter().max().expect("三道闸门非空");
        for needle in [
            "default_is_model_temporarily_unavailable",
            "is_upstream_transient_5xx",
            "is_upstream_region_mismatch_403",
        ] {
            let at = body
                .find(needle)
                .unwrap_or_else(|| panic!("分类器必须仍调 {needle}（复用既有谓词）"));
            assert!(
                at > last_gate,
                "{needle} 必须排在三条 None 闸门之后 —— 排前面会把网关背压/永久态当可吸收"
            );
        }

        // ⭐ 容量类必须排在 5xx 之前（容量类的一种形态是 503，会被 5xx 判据抢走）。
        assert!(
            body.find("default_is_model_temporarily_unavailable")
                .unwrap()
                < body.find("is_upstream_transient_5xx").unwrap(),
            "容量判据必须排在 5xx 之前，否则 503 形态的容量类被 5xx 抢走、套错退避曲线"
        );
    }

    /// 取 `absorb_class_of` 的**函数体**源码（供顺序守卫用）。
    ///
    /// 为什么要切片而不是直接用整个文件：`include_str!` 会把本测试模块自己的字面量也读进来，
    /// 按全文件找位置会命中测试里的字符串（本仓 `absorb_stop_reasons_are_distinguishable_in_logs`
    /// 就吃过这个坑：短名在注释里也出现，实测 left=3 right=2）。
    fn absorb_class_of_source() -> &'static str {
        let full = include_str!("handlers.rs");
        let start = full
            .find("pub(crate) fn absorb_class_of")
            .expect("函数签名必须存在");
        let rest = &full[start..];
        // 函数体结束于下一个顶层 `\n}\n`（本函数内所有 `}` 都有缩进）。
        let end = rest.find("\n}\n").expect("函数必须有结尾");
        &rest[..end]
    }

    /// ⭐ 合并外挂缺口 4：预算耗尽后按配置回 **503**（而非透传 429）。
    ///
    /// 依据（外挂注释原文）：「有总时间预算，超预算返回 **503（不是 429）** —— Cursor 对 503
    /// 不会像对 429 那样立刻停止会话。」这是产品级行为差异：同一个「网关已尽力但没成」的事实，
    /// 用 429 表达会让 Cursor 直接掐会话，用 503 表达会让它自己再退避重试。
    ///
    /// 回退即 FAIL：删掉 `map_provider_error` 的第一条分支（标记分支），该串会落到下面的
    /// 全池冷却分支返回 429 —— 即这个开关静默失效。
    #[test]
    fn absorb_exhausted_marker_renders_503_with_retry_after() {
        // provider 在 `absorb_gave_up_after_rounds && exhausted_as_503` 时组装的串形态：
        // 原错误 + 空格 + 标记。
        let raw = format!(
            "所有凭据均在冷却（0/4）retry_after_secs=10 {}",
            ABSORB_BUDGET_EXHAUSTED_MARKER
        );
        let resp = map_provider_error(anyhow::Error::msg(raw));
        assert_eq!(
            resp.status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "带耗尽标记时必须回 503（Cursor 见 429 会掐会话，见 503 会自行退避）"
        );
        assert_eq!(
            resp.headers()
                .get(header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok()),
            Some("10"),
            "必须优先用号池真值 10s 而不是常数兜底 —— 真值比任何常数都准"
        );

        // ⭐ 边界（承重）：**不带**标记的同一条串必须仍是 429。
        // 这坐实了状态码只对「吸收层真的跑过并放弃」的请求变化，没进过吸收层的 429 照旧。
        let untouched = map_provider_error(anyhow::Error::msg(
            "所有凭据均在冷却（0/4）retry_after_secs=10",
        ));
        assert_eq!(
            untouched.status(),
            StatusCode::TOO_MANY_REQUESTS,
            "没有标记就必须仍是 429（默认配置下 provider 不打标记 ⇒ 渲染路径逐字节不变）"
        );

        // 无号池真值时按类别兜底：403 风控用 20s（与 cooldown.rs 的 SuspiciousActivity 同源）。
        let swap = map_provider_error(anyhow::Error::msg(format!(
            "403 Forbidden {{\"message\":\"Your User ID temporarily is suspended.\"}} {}",
            ABSORB_BUDGET_EXHAUSTED_MARKER
        )));
        assert_eq!(swap.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            swap.headers()
                .get(header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok()),
            Some("20")
        );
    }

    /// ⭐ 承重：标记分支必须是 `map_provider_error` 的**第一条**。
    ///
    /// 这个标记只可能打在**已被判为可吸收**的错误串上，而那些串必然还带着各自的原始特征
    /// （`retry_after_secs=` / `USER_REQUEST_RATE_EXCEEDED` / `temporarily is suspended` /
    /// 5xx 字样）—— 后面任何一条分支都会先把它们接走并返回 429。排在后面等于开关静默失效。
    ///
    /// 回退即 FAIL：把标记分支下移到准入超时分支之后 —— 上面那条 `retry_after_secs=10`
    /// 的夹具会被全池冷却分支抢走返 429。这里再加一道源码级顺序断言，
    /// 因为运行时断言只能证明「当前夹具通过」，证明不了顺序本身。
    #[test]
    fn absorb_exhausted_branch_is_first_in_map_provider_error() {
        let full = include_str!("handlers.rs");
        let start = full
            .find("fn map_provider_error(err: Error) -> Response {")
            .expect("函数签名必须存在");
        let body = &full[start..];
        let marker_at = body
            .find("ABSORB_BUDGET_EXHAUSTED_MARKER")
            .expect("必须有耗尽标记分支");
        for later in [
            "inbound_admission_timeout=1",
            "parse_retry_after_secs(&err_str)",
            "is_upstream_rate_limited(&err_str)",
            "is_upstream_temporarily_suspended(&err_str)",
        ] {
            let at = body
                .find(later)
                .unwrap_or_else(|| panic!("{later} 分支必须仍存在"));
            assert!(
                marker_at < at,
                "耗尽标记分支必须排在 {later} 之前，否则那条分支会先把串接走返 429（开关静默失效）"
            );
        }
    }

    /// 抽出的 `parse_retry_after_secs` 与它替换掉的两份内联拷贝行为一致。
    ///
    /// 回退即 FAIL：若有人把解析逻辑改回各写一份并写歪一处，这里的边界断言会失败。
    #[test]
    fn parse_retry_after_secs_handles_boundaries() {
        assert_eq!(parse_retry_after_secs("x retry_after_secs=14"), Some(14));
        assert_eq!(parse_retry_after_secs("x retry_after_secs=7 y"), Some(7));
        assert_eq!(parse_retry_after_secs("x retry_after_secs=0"), Some(0));
        assert_eq!(parse_retry_after_secs("没有这个标记"), None);
        assert_eq!(parse_retry_after_secs("retry_after_secs=abc"), None);
    }

    #[test]
    fn test_inbound_admission_timeout_is_distinguishable_from_pool_cooling() {
        // ⭐ 回归（把 `inbound_admission_timeout=1` 那条分支删掉即必失败）：
        // 准入超时（网关自己的背压）与全池冷却（上游没准备好）**语义相反**，
        // 但两者都带 `retry_after_secs=`。若响应体上不可区分，任何按 body 判定的
        // 重试层（内置吸收层 / 外挂 kiro_shield）都会去重试网关自己的背压信号
        // —— 实测形态：2 轮 × 30s = 客户端等 60s 才拿到 429，而正确是 <2s。
        let admission = map_provider_error(anyhow::Error::msg(
            "入站限速排队超时(网关目标 300 RPM 保护上游)inbound_admission_timeout=1 retry_after_secs=3",
        ));
        let cooling = map_provider_error(anyhow::Error::msg(
            "所有凭据均在冷却（0/1）retry_after_secs=3",
        ));

        // 对客户端而言两者都该是 429 + Retry-After（它就该退避）。
        assert_eq!(admission.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(cooling.status(), StatusCode::TOO_MANY_REQUESTS);
        for (name, resp) in [("准入超时", &admission), ("全池冷却", &cooling)] {
            assert_eq!(
                resp.headers()
                    .get(header::RETRY_AFTER)
                    .unwrap_or_else(|| panic!("{name} 应带 Retry-After"))
                    .to_str()
                    .unwrap(),
                "3",
                "{name} 的 Retry-After 应透传上游/网关给的精确秒数"
            );
        }

        // 但对**重试层**必须可区分：响应体文案不同。
        let dump = |resp: axum::response::Response| async move {
            let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024)
                .await
                .expect("读取响应体");
            String::from_utf8_lossy(&bytes).to_string()
        };
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (a_body, c_body) = rt.block_on(async { (dump(admission).await, dump(cooling).await) });

        assert_ne!(
            a_body, c_body,
            "准入超时与全池冷却的响应体必须不同，否则重试层无法分辨（这正是缺陷本体）"
        );
        assert!(
            a_body.contains("backpressure"),
            "准入超时的文案应自述为网关背压，实际: {a_body}"
        );
        assert!(
            !a_body.contains("cooling down"),
            "准入超时绝不能复用全池冷却的 `cooling down` 文案 —— \
             kiro_shield 的 COOLING_MARKERS 命中它就会重试网关自己的背压。实际: {a_body}"
        );
        assert!(
            c_body.contains("cooling down"),
            "全池冷却的文案应保持不变（零回归），实际: {c_body}"
        );
    }

    /// 上游并发闸满（`upstream_gate_full=1`）必须：① 对客户端 429 + Retry-After（而非 502，
    /// 502 让客户端立即重发重新灌满闸门）；② 对吸收层判为**不可吸收**（不能被 `retry_after_secs=`
    /// 抢成 PoolCooldown 睡 2s 重打整链 +6s 延迟）。
    #[test]
    fn upstream_gate_full_is_429_not_absorbable() {
        // 吸收层分类：带 retry_after_secs=2 但必须返 None（网关自己的背压）。
        assert!(
            absorb_class_of("上游并发闸已满，停止本轮重试以免放大 upstream_gate_full=1 retry_after_secs=2")
                .is_none(),
            "gate-full 不得被当 PoolCooldown 吸收"
        );

        // map_provider_error：429 + Retry-After:2。
        let resp = map_provider_error(anyhow::Error::msg(
            "上游并发闸已满，停止本轮重试以免放大 upstream_gate_full=1 retry_after_secs=2",
        ));
        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(
            resp.headers().get(header::RETRY_AFTER).unwrap().to_str().unwrap(),
            "2",
            "gate-full 的 Retry-After 应为网关给的退避秒数"
        );
    }

    /// 取 `try_inbound_admission_gate` 的**函数体**源码（供准入闸门守卫用）。
    ///
    /// 为什么要切片而不是直接用整个文件：`include_str!` 会把本测试模块自己的
    /// 字面量也读进来（历史事故：同文件测试夹具里有两份带标记的完整消息副本，
    /// 全文件 `contains` 会在生产格式串丢掉标记时照样绿——本仓自述的
    /// 「源码级守卫自匹配」陷阱在迁移中被重新引入过一次）。
    fn inbound_admission_gate_source() -> &'static str {
        let full = include_str!("handlers.rs");
        let start = full
            .find("fn try_inbound_admission_gate(")
            .expect("准入闸门函数必须存在");
        let rest = &full[start..];
        // 函数体结束于下一个顶层 `\n}\n`（本函数内所有 `}` 都有缩进）。
        let end = rest.find("\n}\n").expect("准入闸门函数必须有结尾");
        &rest[..end]
    }

    #[test]
    fn admission_timeout_bail_must_carry_its_own_marker() {
        // 源码级守卫：准入闸门分支必须带标记、必须 bump 计数、必须 emit_record。
        // 只靠行为测试不够 —— 它喂的是手写字符串，而真正的风险是生产分支
        // **改了文案却没带标记 / 丢了观测**，那样行为测试照样绿、线上照样不可区分。
        // 2026-08-11：从全文件 contains 改为切片函数体，杜绝测试夹具自匹配。
        let body = inbound_admission_gate_source();
        let needle = format!("{}{}{}", "保护上游)", "inbound_admission_timeout", "=1");
        assert!(
            body.contains(&needle),
            "准入闸门格式串必须紧接 `保护上游)` 带上 inbound_admission_timeout=1 标记，\
             否则分类器无法把它与全池冷却区分开（两者都带 retry_after_secs=）"
        );
        assert!(
            body.contains("bump_inbound_admission_timeout"),
            "准入超时分支必须 bump 背压计数（面板可观测性，2026-08-11 恢复的断言）"
        );
        assert!(
            body.contains("emit_record"),
            "准入超时分支必须 emit_record（usage 统计可观测性，2026-08-11 恢复的断言）"
        );
    }

    #[test]
    fn admission_gate_placement_and_cc_coverage() {
        // 闸门必须位于 post_messages 的透传分叉**之前**，且 /cc/v1 入口也必须过闸。
        // 回退即 FAIL：把闸门移到透传块之后（透传再次 100% 绕闸，即本轮修复的缺陷
        // 本体）或删掉 post_messages_cc 的调用，本守卫都会红。
        let full = include_str!("handlers.rs");
        let pm = {
            let start = full
                .find("pub async fn post_messages(")
                .expect("post_messages 必须存在");
            let rest = &full[start..];
            let end = rest.find("\n}\n").expect("post_messages 必须有结尾");
            &rest[..end]
        };
        let gate_at = pm
            .find("try_inbound_admission_gate(")
            .expect("post_messages 必须调用入站闸门");
        let passthrough_at = pm
            .find("try_custom_api_passthrough(")
            .expect("post_messages 必须包含透传分叉");
        assert!(
            gate_at < passthrough_at,
            "准入闸门必须位于透传分叉之前，否则透传 100% 绕闸且现有守卫全部失明"
        );
        let cc = {
            let start = full
                .find("pub async fn post_messages_cc(")
                .expect("post_messages_cc 必须存在");
            let rest = &full[start..];
            let end = rest.find("\n}\n").expect("post_messages_cc 必须有结尾");
            &rest[..end]
        };
        assert!(
            cc.contains("try_inbound_admission_gate("),
            "/cc/v1 入口必须过入站闸门（闸门移到 handler 层后曾漏掉该入口，属回归）"
        );
    }

    #[test]
    fn compress_retry_loop_uses_extracted_target_fn() {
        // 压缩重试的目标必须走 compress_retry_target（防有人把公式内联回来再次写反向）。
        // ⚠️ needle 运行时拼接 + 切片锚定到循环结束（4 空格缩进的 `}`）：
        // 完整字面量若出现在源码里，include_str! 会把测试段/注释也读进来，生产被删后
        // `.find` 命中它们 → 守卫静默变绿（本仓踩过同型坑）。拼接后源码不存在完整
        // needle；循环级切片保证「移出循环但仍在函数内」的回退也会红。
        let full = include_str!("handlers.rs");
        let needle = format!("{}: loop {}", "'compress_retry", "{");
        let start = full
            .find(needle.as_str())
            .expect("压缩重试循环必须存在");
        let end = full[start..]
            .find("\n    }\n")
            .map(|i| start + i)
            .unwrap_or(full.len());
        let body = &full[start..end];
        assert!(
            body.contains("compress_retry_target("),
            "压缩重试必须用 compress_retry_target 计算目标字节数"
        );
    }

    #[test]
    fn compress_retry_loop_cc_coverage() {
        // /cc/v1 必须与 /v1 同款压缩重试循环（2026-08-11 审计缺口补齐）：
        // 循环标签、目标公式、轮末 strip 三者都必须落在 cc 函数体内。
        // ⚠️ needle 运行时拼接（同仓教训：完整字面量出现在测试/注释里会让守卫静默变绿）：
        // 循环标签拆成三段拼、strip 的头部名拆两段拼；切片锚定 cc 函数体
        // （"pub async fn post_messages_cc(" 到函数收尾 `}`），保证「循环挪进别的函数」
        // 的回退也红。
        let full = include_str!("handlers.rs");
        let cc = {
            let start = full
                .find("pub async fn post_messages_cc(")
                .expect("post_messages_cc 必须存在");
            let rest = &full[start..];
            let end = rest.find("\n}\n").expect("post_messages_cc 必须有结尾");
            &rest[..end]
        };
        let loop_needle = format!("{}: loop {}", "'compress_retry", "{");
        let start = cc
            .find(loop_needle.as_str())
            .expect("/cc/v1 必须与 /v1 同款压缩重试循环");
        // 循环收尾锚定：轮末 return 之后紧跟循环结束的 4 空格 `}`（本仓惯例循环收尾
        // 与函数收尾紧贴、无空行；cc 函数切片止于函数收尾 `}` 前，故用 return 行定位）。
        let return_at = cc[start..]
            .find("return final_response;")
            .map(|i| start + i)
            .expect("/cc/v1 压缩重试循环轮末必须 return");
        let end = cc[return_at..]
            .find("\n    }")
            .map(|i| return_at + i)
            .unwrap_or(cc.len());
        let loop_body = &cc[start..end];
        assert!(
            loop_body.contains("compress_retry_target("),
            "/cc/v1 压缩重试必须用 compress_retry_target 计算目标字节数"
        );
        let strip_needle = format!("remove(\"x-kirostudio-{}\")", "compress-retry");
        assert!(
            loop_body.contains(strip_needle.as_str()),
            "/cc/v1 循环轮末必须 strip 内部标记头（2026-08-11 F1b 同款防泄漏，不得移出循环）"
        );
        // ⚠️ 强化（2026-08-11 对抗审查 m1）：锁「strip 在循环收尾之前」。
        // 盲区：把轮末改成 `break` 出循环、在循环外 strip 再 return（行为等价但结构
        // 迁移）时，上面的 loop_body 切片扩张到函数末尾、断言照样绿。两段锁死：
        // ① 循环收尾（return 之后的 4 空格 `}`）必须存在 —— break 写法下 return 在
        //    循环外，其后没有 4 空格 `}`（函数收尾是 0 空格），此处 expect 直接红；
        // ② strip 必须位于循环收尾之前。
        let loop_end_at = cc[return_at..]
            .find("\n    }")
            .expect("循环收尾（4 空格 `}`）必须紧跟轮末 return 之后 —— 若 break 出循环\
                    再 return，此处必红");
        let strip_at = cc[start..]
            .find(strip_needle.as_str())
            .map(|i| start + i)
            .unwrap_or(usize::MAX);
        assert!(
            strip_at < return_at + loop_end_at,
            "strip 必须位于循环收尾之前（不得 break 出循环后再 strip）"
        );
    }

    #[test]
    fn compress_retry_target_strictly_decreasing_with_floor() {
        let trigger = 4 * 1024 * 1024; // 4 MiB（默认 trigger_bytes）
        let a1 = compress_retry_target(trigger, 1);
        let a2 = compress_retry_target(trigger, 2);
        let a3 = compress_retry_target(trigger, 3);
        // 0.75 → 0.5625 → 0.421875：逐轮更紧，绝不能反弹（历史 bug：序列反向）。
        assert!(
            a1 < trigger && a2 < a1 && a3 < a2,
            "target 必须逐轮递减，a1={a1} a2={a2} a3={a3}"
        );
        assert_eq!(a1, trigger * 3 / 4);
        assert_eq!(a2, trigger * 9 / 16);
        assert_eq!(a3, trigger * 27 / 64);
        // 下限 64 KiB；attempt=0 恒等（文档语义：初试用配置值，本函数只用于重试）。
        assert_eq!(compress_retry_target(1024, 3), 65536);
        assert_eq!(compress_retry_target(trigger, 0), trigger);
    }

    /// 压缩重试重建 body 的 native effort 字段携带（deep 审计补测，2026-08-11）：
    /// `build_kiro_request_body` 带 `additionalModelRequestFields` 时，初试与重试
    /// （更小 target_bytes）两次序列化都必须含该字段——P1 移植与 P0-2 压缩重试的
    /// 交叉点，丢字段 = 重试请求的 extended thinking 静默失效。
    #[test]
    fn compress_retry_rebuild_keeps_additional_model_request_fields() {
        use crate::kiro::model::requests::kiro::{AdditionalModelRequestFields, KiroOutputConfig};
        use crate::model::config::CompressionConfig;

        let state = crate::kiro::model::requests::conversation::ConversationState::new("conv-1");
        let fields = Some(AdditionalModelRequestFields {
            output_config: Some(KiroOutputConfig {
                effort: "xhigh".to_string(),
            }),
        });
        let mut cfg = CompressionConfig::default();
        cfg.enabled = true;
        cfg.trigger_bytes = 1024;

        let body_initial = build_kiro_request_body(state.clone(), fields.clone(), &cfg, None)
            .expect("初试序列化应成功");
        assert!(
            body_initial.contains("additionalModelRequestFields")
                && body_initial.contains("output_config"),
            "初试 body 必须含 native effort 字段"
        );

        let body_retry = build_kiro_request_body(state, fields, &cfg, Some(256))
            .expect("重试序列化应成功");
        assert!(
            body_retry.contains("additionalModelRequestFields")
                && body_retry.contains("output_config"),
            "压缩重试重建 body 不得丢 native effort 字段（键与 effort 都要在，\
             只保键不保 effort 同样等于静默失效）"
        );
    }

    /// 守卫：压缩重试循环重建 body 时**必须**把捕获的 native fields 传进去。
    /// 行为测试（上面那条）只证明函数本身不丢字段；这条钉的是循环调用点——
    /// 若有人把调用改回不传 fields（或注释掉捕获行），行为测试照样绿（函数能力
    /// 没变），只有这条会红。
    #[test]
    fn compress_retry_rebuild_passes_native_fields_through() {
        let full = include_str!("handlers.rs");
        let needle = format!("{}: loop {}", "'compress_retry", "{");
        let start = full
            .find(needle.as_str())
            .expect("压缩重试循环必须存在");
        let end = full[start..]
            .find("\n    }\n")
            .map(|i| start + i)
            .unwrap_or(full.len());
        let body = &full[start..end];
        let field = format!("native_fields_for_{}", "compress_retry");
        assert!(
            body.contains(field.as_str()),
            "压缩重试循环重建 body 时必须传入捕获的 native effort 字段 \
             （丢了 = 重试请求的 extended thinking 静默失效）"
        );
    }

    #[test]
    fn test_translate_quota_exhausted() {
        let t = translate_upstream_error("upstream: MONTHLY_REQUEST_COUNT limit reached").unwrap();
        assert_eq!(t.status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(t.error_type, "rate_limit_error");
        assert!(t.message.contains("配额") && t.message.contains("排障"));
    }

    /// 🔴 回归（2026-08-10）：**全池配额耗尽只由显式标记断言，裸串不得冒充**。
    ///
    /// 缺陷：`translate_quota_subscription` 原先用裸串 `MONTHLY_REQUEST_COUNT` / `QUOTA`
    /// 判「月度配额耗尽」，而那两个串来自**上游 body**。单号耗尽时 provider 走「换号
    /// continue」分支，其 `last_error` 同样带这两个串，且 `last_error` **刻意不重置**
    /// ⇒ 池里其余号健康时，最终错误仍被判成"全部凭据配额耗尽"，归因口径被污染。
    ///
    /// 现在：带 `quota_exhausted_all=1`（provider 确认 `has_available == false` 后才打）
    /// 的才断言"号池内所有凭据"；裸串降级为不断言范围的通用配额文案。
    /// 两者状态码都保持 429（可退避），因为删掉裸串会让 MCP/透传等路径的配额错误落
    /// 502 兜底 → 客户端当永久故障不退避（本仓反复踩过的回归）。
    #[test]
    fn quota_exhausted_all_marker_distinguishes_pool_wide_from_single_credential() {
        // ① 带标记 → 明确断言"所有凭据"
        let all = translate_upstream_error(
            "流式 API 请求失败（所有凭据已用尽）quota_exhausted_all=1: 402 {\"reason\":\"MONTHLY_REQUEST_COUNT\"}",
        )
        .expect("带标记应命中配额分支");
        assert_eq!(all.status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(all.error_type, "rate_limit_error");
        assert!(
            all.message.contains("所有凭据"),
            "带 quota_exhausted_all=1 时必须断言范围是整个号池，实际: {}",
            all.message
        );

        // ② 只有裸串（单号耗尽后换号，链上残留的上游 body）→ **不得**断言"所有凭据"
        let single = translate_upstream_error(
            "流式 API 请求失败: 402 {\"reason\":\"MONTHLY_REQUEST_COUNT\"}",
        )
        .expect("裸串仍须命中配额分支（不能落 502 兜底）");
        assert_eq!(
            single.status,
            StatusCode::TOO_MANY_REQUESTS,
            "裸串必须仍返 429（可退避）——落 502 会让客户端当永久故障不退避"
        );
        assert!(
            !single.message.contains("所有凭据"),
            "只有裸串时**不能**断言\"所有凭据\"（那是标记分支才能确认的事实）。\
             池里其余号可能仍健康，错误归因不该扩大范围。实际: {}",
            single.message
        );
        assert!(
            single.message.contains("配额") && single.message.contains("排障"),
            "裸串分支仍须给出可操作的排障提示，实际: {}",
            single.message
        );
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
        // 英文哨兵与中文文案**同时**存在（前缀不是替换）。子串契约本身由
        // `overflow_errors_must_match_claude_code_compact_retry_predicate` 钉死。
        assert!(t.message.starts_with(OVERFLOW_COMPACT_HINT));
    }

    #[test]
    fn test_translate_input_too_long() {
        let t = translate_upstream_error("Input is too long for the model").unwrap();
        assert_eq!(t.status, StatusCode::BAD_REQUEST);
        assert!(t.message.contains("输入过长") && t.message.contains("拆分"));
        assert!(t.message.starts_with(OVERFLOW_COMPACT_HINT));
    }

    /// ⭐ 外部契约守卫（承重）：「装不下」类错误的 message **必须**命中 Claude Code 的
    /// compact-and-retry 判据，否则用户的自动压缩静默失效。
    ///
    /// # 这条断言在保护什么
    ///
    /// Claude Code（本机 2.1.220 实测）判「该压缩后重试」的方式是对错误 message 做
    /// **小写化子串匹配**，形如：
    ///
    /// ```text
    /// msg.toLowerCase().includes("prompt is too long")
    ///   || msg.toLowerCase().includes("input is too long for requested model")
    /// ```
    ///
    /// 命中后它会压缩上下文并**自动重试**；不命中就只是把错误打给用户。而它的另一条
    /// 「按水位主动压缩」的路径在网关模式下结构性不可用（见
    /// `docs/auto-compact-fix-2026-08-06.md`）⇒ 这个子串是网关唯一能给用户的自动压缩。
    ///
    /// # 为什么必须单列一条，而不是靠上面两条整串比对
    ///
    /// 上面两条测的是**中文文案还在不在**。有人润色文案时顺手去掉英文前缀，那两条照样绿
    /// （它们断言的是"上下文"/"输入过长"这些中文词），而保险丝已经烧了。本条直接断言
    /// **外部消费者的判据能命中**，是唯一一处「改文案会立刻变红」的地方。
    ///
    /// ⚠️ 这里刻意写**字面量**而不是引用 `OVERFLOW_COMPACT_HINT`：引用了就是同义反复
    /// （把常量改成空串，断言依然成立），钉不住任何东西。契约的对面是别人的二进制，
    /// 本仓这侧只能用字面量表达。
    ///
    /// ⚠️ 大小写：判据先 `toLowerCase()`，故本断言也先 `to_lowercase()` —— 文案首字母
    /// 将来若改成大写，契约仍然成立，测试不该因此误红。
    #[test]
    fn overflow_errors_must_match_claude_code_compact_retry_predicate() {
        use axum::body::to_bytes;

        // Claude Code 侧的两个判据字面量（小写形态）。任一命中即触发 compact-and-retry。
        const CC_SENTINELS: [&str; 2] = [
            "prompt is too long",
            "input is too long for requested model",
        ];

        // 两类上游「装不下」原文（实测形态）。
        for upstream in [
            "非流式 API 请求失败: 400 Bad Request {\"reason\":\"CONTENT_LENGTH_EXCEEDS_THRESHOLD\"}",
            "流式 API 请求失败: 400 Bad Request {\"message\":\"Input is too long for the model\"}",
        ] {
            // ⭐ 走 `map_provider_error` **全路径**读真实响应体，不是只调 `translate_*`。
            //
            // 理由（本仓「纸面测试」的第 8 种形态）：只测分支内部，对**分支顺序**完全不可见。
            // 客户端真正拿到的是本函数的输出，而它前面还排着若干条 return（吸收层耗尽的 503
            // 覆盖、准入超时、全池冷却、限流、临时风控、region 错配…）。将来任何一条被放宽到
            // 能匹配这两个 400 串，客户端拿到的 message 里就不再有哨兵 —— 而只调
            // `translate_upstream_error` 的断言那时**依然全绿**。
            let resp = map_provider_error(anyhow::Error::msg(upstream));
            assert_eq!(
                resp.status(),
                StatusCode::BAD_REQUEST,
                "「装不下」必须仍是 400（重试原请求无意义，客户端要做的是压缩后重试）：{upstream}"
            );
            let body = futures::executor::block_on(to_bytes(resp.into_body(), usize::MAX)).unwrap();
            let text = String::from_utf8_lossy(&body);
            let low = text.to_lowercase();
            assert!(
                CC_SENTINELS.iter().any(|s| low.contains(s)),
                "响应体未命中 Claude Code 的 compact-and-retry 判据 ⇒ 用户撞满上下文后不会自动\
                 压缩重试，只会看到报错。改文案时必须保留英文哨兵子串。实际响应体: {text}"
            );
            // 中文排障文案不得因为加哨兵而丢失（两个受众各拿到自己那份）。
            assert!(
                text.contains("排障"),
                "英文哨兵是**前缀**不是替换，中文排障步骤必须仍在: {text}"
            );
        }
    }

    #[test]
    fn test_translate_network_dns() {
        let t = translate_upstream_error("error trying to connect: dns error: failed to resolve")
            .unwrap();
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
        assert!(
            !text.contains("123456789012"),
            "响应体泄露了 AWS 账号号: {}",
            text
        );
        assert!(
            !text.contains("SECRET"),
            "响应体泄露了 profile id: {}",
            text
        );
        assert!(
            !text.contains("eu-central-1"),
            "响应体泄露了 region: {}",
            text
        );
        // 仍给出通用引导。
        assert!(text.contains("未识别错误") && text.contains("网关日志"));
    }

    /// review high 回归:上游 HTTP 错误**响应体**里恰好含 timeout/tls/proxy/resolve 字样时,
    /// **绝不**被误判成网络故障(它不是传输层错误,无 "error sending request" 等标志)。
    #[test]
    fn test_translate_network_no_false_positive_on_upstream_body() {
        // 模拟 provider 格式化的上游错误串(含 HTTP 状态码 + body,body 里有 "timeout"/"proxy" 字样)。
        let upstream_body = "流式 API 请求失败: 400 {\"message\":\"your request proxy timeout config is invalid, tls off\"}";
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
        assert!(
            !extract_thinking_enabled(),
            "set false 后热路径应读到 false"
        );
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
                            Event::Error {
                                error_code,
                                error_message,
                            } => {
                                if completion.is_ok() {
                                    completion = CompletionStatus::UpstreamError {
                                        code: error_code,
                                        message: error_message,
                                    };
                                }
                            }
                            Event::Exception {
                                exception_type,
                                message,
                            } => {
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
            &[
                (":message-type", "error"),
                (":error-code", "InternalServerException"),
            ],
            b"upstream exploded",
        );
        let completion = decode_to_completion(&frame);

        assert!(!completion.is_ok(), "in-band error 帧应被识别为失败");
        assert_ne!(completion.http_status_u16(), 200, "失败必须返回非 200");
        assert_eq!(completion.http_status_u16(), 502);
        assert_eq!(
            completion.outcome(),
            crate::usage::RequestOutcome::ServerError
        );
    }

    #[test]
    fn test_inband_throttling_error_frame_maps_to_429() {
        let frame = build_frame(
            &[
                (":message-type", "error"),
                (":error-code", "ThrottlingException"),
            ],
            b"slow down",
        );
        let completion = decode_to_completion(&frame);
        assert_eq!(completion.http_status_u16(), 429);
        assert_eq!(
            completion.outcome(),
            crate::usage::RequestOutcome::RateLimited
        );
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
        assert_eq!(
            completion.outcome(),
            crate::usage::RequestOutcome::ServerError
        );
    }

    #[test]
    fn test_non_tool_parse_failure_stays_ok() {
        // 零倒退承诺：非 tool 帧解析失败只应告警、不置失败态。
        // 注意 AssistantResponseEvent.content 有 serde(default)，故须用非法 JSON 而非 `{}` 才能触发反序列化失败。
        let frame = build_frame(
            &[
                (":message-type", "event"),
                (":event-type", "assistantResponseEvent"),
            ],
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

#[cfg(test)]
mod ported_k2cc_empty_response_event_tests {
    //! 从 k2cc 移植的「空响应 SSE error 事件」测试：上下文过大 → invalid_request_error
    //! 且带 /compact 提示；偶发 → overloaded_error 可重试。
    use super::*;

    #[test]
    fn empty_response_error_event_oversized_hints_compact() {
        let ev = empty_response_error_event(true);
        assert_eq!(ev.event, "error");
        assert_eq!(ev.data["error"]["type"], "invalid_request_error");
        let msg = ev.data["error"]["message"].as_str().unwrap();
        assert!(msg.contains("/compact"), "提示文案必须含 /compact: {msg}");
    }

    #[test]
    fn empty_response_error_event_transient_is_retryable() {
        let ev = empty_response_error_event(false);
        assert_eq!(ev.event, "error");
        assert_eq!(ev.data["error"]["type"], "overloaded_error");
        assert!(ev.data["error"]["message"].as_str().unwrap().contains("重试"));
    }
}

#[cfg(test)]
mod adaptive_compress_loop_tests {
    //! 自适应二次压缩循环：压一次仍超限 → 迭代降级直至进阈值；以及 fail-safe 行为。
    //!
    //! ⚠️ 本机无法 `cargo build`（8GB 内存 + 编译不过的历史问题），只能静态自检：
    //! 断言围绕「最终序列化字节数必须小于阈值」与「不再调用即返回当前结果」，
    //! 逻辑自洽但未在真实编译器上验证过类型/借用。
    use super::*;
    use crate::kiro::model::requests::conversation::*;
    use crate::kiro::model::requests::tool::ToolResult;
    use crate::model::config::CompressionConfig;

    fn config(trigger_bytes: usize, tool_result_max_chars: usize) -> CompressionConfig {
        CompressionConfig {
            enabled: true,
            trigger_bytes,
            whitespace_compression: false,
            tool_result_max_chars,
            tool_result_head_lines: 3,
            tool_result_tail_lines: 3,
        }
    }

    fn run(
        conversation_state: ConversationState,
        cfg: &CompressionConfig,
    ) -> (String, ConversationState) {
        let kiro_request = KiroRequest {
            conversation_state,
            profile_arn: None,
            additional_model_request_fields: None,
        };
        let before = serde_json::to_string(&kiro_request).unwrap();
        assert!(before.len() > cfg.trigger_bytes, "前置：初始已超阈值");
        // 造一个可变的 KiroRequest 供循环使用
        let mut kiro_request = kiro_request;
        let mut body = before;
        adaptive_compress_loop(&mut kiro_request, cfg, &mut body, None).unwrap();
        (body, kiro_request.conversation_state)
    }

    #[test]
    fn converge_to_below_threshold_via_tool_result() {
        // 单一超大 tool_result：压一次（8000）仍超限，但压到 4500 就能进阈值
        let long_text = (0..400).map(|i| format!("row {}", i)).collect::<Vec<_>>().join("\n");
        let state = ConversationState::new("conv")
            .with_current_message(CurrentMessage::new(
                UserInputMessage::new("msg", "claude-sonnet-4.5").with_context(
                    UserInputMessageContext::new()
                        .with_tool_results(vec![ToolResult::success("t1", &long_text)]),
                ),
            ))
            .with_history(Vec::new());

        let cfg = config(1800, 8000);
        let (body, _state) = run(state, &cfg);
        assert!(body.len() < cfg.trigger_bytes, "最终字节 {} 仍超阈值 {}", body.len(), cfg.trigger_bytes);
    }

    #[test]
    fn converge_to_below_threshold_via_history_drop() {
        // 多轮小历史：没有 tool_result 可压，删掉若干最老轮次后进阈值
        let mut history = Vec::new();
        for i in 0..80 {
            history.push(Message::User(HistoryUserMessage::new(
                format!("long user message number {}", i),
                "claude-sonnet-4.5",
            )));
            history.push(Message::Assistant(HistoryAssistantMessage::new(
                format!("assistant answer {}", i),
            )));
        }
        let state = ConversationState::new("conv")
            .with_current_message(CurrentMessage::new(
                UserInputMessage::new("hi", "claude-sonnet-4.5"),
            ))
            .with_history(history);

        let cfg = config(2000, 0); // 关掉 tool_result 层，逼循环走历史删除
        let (body, state) = run(state, &cfg);
        assert!(body.len() < cfg.trigger_bytes, "最终字节 {} 仍超阈值 {}", body.len(), cfg.trigger_bytes);
        assert!(state.history.len() >= 4, "保留对不能低于 2 对，实际 {}", state.history.len());
    }

    #[test]
    fn single_message_huge_triggers_message_truncation() {
        // 单条 user content 本身就远超阈值：删历史救不回来 → 走正文截断层
        let huge = "x".repeat(50_000);
        let state = ConversationState::new("conv")
            .with_current_message(CurrentMessage::new(
                UserInputMessage::new(huge, "claude-sonnet-4.5"),
            ))
            .with_history(Vec::new());

        // 阈值取 20000：正文截断的下限是 ADAPTIVE_MIN_MESSAGE_CONTENT_MAX_CHARS=8192
        // （+ 省略标记约 40 字节），阈值若定得比这个地板还小，循环永远压不进去
        // ——那是「压到底仍超限，照发交上游」的预期路径，不该用它断言收敛。
        let cfg = config(20_000, 0);
        let (body, state) = run(state, &cfg);
        assert!(body.len() < cfg.trigger_bytes, "最终字节 {} 仍超阈值 {}", body.len(), cfg.trigger_bytes);
        let final_chars = state.current_message.user_input_message.content.chars().count();
        assert!(final_chars < 50_000, "正文应被截短，实际 {final_chars}");
    }

    #[test]
    fn floor_reached_still_oversized_gives_up_without_hanging() {
        // 压到地板（8192 字符）仍超阈值：必须在 32 轮内退出并照发，不挂死、不 panic
        let huge = "x".repeat(50_000);
        let state = ConversationState::new("conv")
            .with_current_message(CurrentMessage::new(
                UserInputMessage::new(huge, "claude-sonnet-4.5"),
            ))
            .with_history(Vec::new());

        let cfg = config(4000, 0); // 低于 8192 地板，永远压不进
        let (body, _state) = run(state, &cfg);
        // 仍超阈值是预期结果（交上游判死），关键是函数返回了
        assert!(body.len() > cfg.trigger_bytes);
    }

    #[test]
    fn history_images_removed_when_no_tool_result() {
        // 有历史图片、无 tool_result：应触发图片降级而不是删历史
        let img = KiroImage::from_base64("png", "a".repeat(8000));
        // HistoryUserMessage 没有 with_images，图片挂在内层 UserMessage 上
        let mut hu = HistoryUserMessage::new("u", "claude-sonnet-4.5");
        hu.user_input_message = hu.user_input_message.with_images(vec![img]);
        let history = vec![
            Message::User(hu),
            Message::Assistant(HistoryAssistantMessage::new("a")),
        ];
        let state = ConversationState::new("conv")
            .with_current_message(CurrentMessage::new(
                UserInputMessage::new("hi", "claude-sonnet-4.5"),
            ))
            .with_history(history);

        let cfg = config(3000, 0);
        let (body, state) = run(state, &cfg);
        assert!(body.len() < cfg.trigger_bytes, "最终字节 {} 仍超阈值 {}", body.len(), cfg.trigger_bytes);
        // 历史消息应保留（删的是图片不是轮次）
        assert_eq!(state.history.len(), 2);
        if let Message::User(u) = &state.history[0] {
            assert!(u.user_input_message.images.is_empty(), "历史图片应被清除");
        }
    }

    #[test]
    fn disabled_config_returns_original_body() {
        // compression.enabled = false 时循环必须原样返回（不触发任何压缩）。
        // 守卫在 `adaptive_compress_loop` 内部（对齐参考仓 ref-mjy/handlers.rs:251），
        // 因此即使 state 里存在可压缩的大 tool_result，也不得改动。
        let long_text = (0..300).map(|i| format!("row {}", i)).collect::<Vec<_>>().join("\n");
        let state = ConversationState::new("conv")
            .with_current_message(CurrentMessage::new(
                UserInputMessage::new("msg", "claude-sonnet-4.5").with_context(
                    UserInputMessageContext::new()
                        .with_tool_results(vec![ToolResult::success("t1", &long_text)]),
                ),
            ))
            .with_history(Vec::new());

        let cfg = CompressionConfig {
            enabled: false,
            trigger_bytes: 1,
            ..Default::default()
        };
        let kiro_request = KiroRequest {
            conversation_state: state,
            profile_arn: None,
            additional_model_request_fields: None,
        };
        let before = serde_json::to_string(&kiro_request).unwrap();
        let mut kiro_request = kiro_request;
        let mut body = before.clone();
        adaptive_compress_loop(&mut kiro_request, &cfg, &mut body, None).unwrap();
        assert_eq!(body, before, "禁用时不应改动请求体");
    }

    #[test]
    fn zero_trigger_returns_original_body() {
        // trigger_bytes = 0 表示不限制，循环必须原样返回
        let state = ConversationState::new("conv")
            .with_current_message(CurrentMessage::new(
                UserInputMessage::new("hello", "claude-sonnet-4.5"),
            ))
            .with_history(Vec::new());

        let cfg = config(0, 0);
        let kiro_request = KiroRequest {
            conversation_state: state,
            profile_arn: None,
            additional_model_request_fields: None,
        };
        let before = serde_json::to_string(&kiro_request).unwrap();
        let mut kiro_request = kiro_request;
        let mut body = before.clone();
        adaptive_compress_loop(&mut kiro_request, &cfg, &mut body, None).unwrap();
        assert_eq!(body, before);
    }

    #[test]
    fn max_iters_are_bounded() {
        // 即使永远压不进阈值，循环也必须在 32 轮内退出（不挂死）
        let state = ConversationState::new("conv")
            .with_current_message(CurrentMessage::new(
                UserInputMessage::new("hello", "claude-sonnet-4.5"),
            ))
            .with_history(Vec::new());

        let cfg = config(1, 0); // 极小阈值，永远达不到
        let kiro_request = KiroRequest {
            conversation_state: state,
            profile_arn: None,
            additional_model_request_fields: None,
        };
        let before = serde_json::to_string(&kiro_request).unwrap();
        let mut kiro_request = kiro_request;
        let mut body = before;
        adaptive_compress_loop(&mut kiro_request, &cfg, &mut body, None).unwrap();
        assert!(!body.is_empty());
    }
}
