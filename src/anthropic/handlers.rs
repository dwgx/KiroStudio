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

use super::converter::ConversionError;
use super::middleware::AppState;
use super::stream::{
    BufferedStreamContext, CacheUsageBreakdown, CLIENT_TOKEN_DISPLAY_SCALE_HEADER,
    CompletionStatus, SseEvent, StreamContext,
};
use super::types::{
    CountTokensRequest, CountTokensResponse, ErrorResponse, MessagesRequest, Model, ModelsResponse,
    OutputConfig, Thinking,
};
use super::websearch;

#[path = "handlers_dispatch.rs"]
mod handlers_dispatch;
use handlers_dispatch::{
    decode_frames_into, prepare_kiro_dispatch, KiroDispatchPrep, NonStreamDecodeSink,
};

// ==================== B7 启动播种自检：镜像接线位图 ====================
// 校验语义：断言每个进程镜像 setter「被调用过」（而非「值非空」）——
// error_messages 空表合法（全走内置默认）、mock_cache 默认关也合法，
// 用值无法区分「配置没设」与「setter 没被调」。main 启动末尾调用
// [`unwired_mirrors`] 汇总告警；admin 热更 / reload_config 调同一批 setter
// 只是重复置位，无副作用。

/// 位图：第 i 位 = [`MIRROR_WIRED_NAMES`][i] 对应的 setter 执行过。
static MIRROR_WIRED_BITS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// 名字表：新增进程镜像 setter 时必须在此登记（漏登记会在 setter 内 mark 时 panic 暴露）。
/// 与「删校验必红」守卫同纪律（见本文件测试模块 mirror_wiring_source_guard）。
const MIRROR_WIRED_NAMES: [&str; 17] = [
    "collect_client_fingerprint",
    "trust_forwarded_header",
    "ip_blocklist",
    "machine_code_blocklist",
    "extract_thinking",
    "cc_auto_buffer",
    "prompt_cache_enabled",
    "mock_cache_config",
    "tool_clean_leaked_tokens",
    "tool_reclaim_textified_invoke",
    "tool_stray_repeat_guard",
    "tool_stream_align_failure",
    "tool_expose_error_to_client",
    "tool_repair_json",
    "tool_truncation_recovery",
    "compression",
    "error_messages",
];
const _: () = assert!(MIRROR_WIRED_NAMES.len() <= 64);

/// setter 内部调用：登记「本镜像已播种」。名字未登记 → panic（开发期暴露笔误/漏登记）。
fn mark_mirror_wired(name: &str) {
    let Some(idx) = MIRROR_WIRED_NAMES.iter().position(|n| *n == name) else {
        panic!("镜像 {name} 未登记进 MIRROR_WIRED_NAMES");
    };
    MIRROR_WIRED_BITS.fetch_or(1u64 << idx, std::sync::atomic::Ordering::Relaxed);
}

/// B7 启动自检（main 启动末尾调用）：返回尚未播种的镜像名列表，空 = 全部接线完成。
pub(crate) fn unwired_mirrors() -> Vec<&'static str> {
    let bits = MIRROR_WIRED_BITS.load(std::sync::atomic::Ordering::Relaxed);
    MIRROR_WIRED_NAMES
        .iter()
        .enumerate()
        .filter(|(i, _)| bits & (1u64 << i) == 0)
        .map(|(_, n)| *n)
        .collect()
}

/// 登记在案的镜像总数（main 自检日志用）。
pub(crate) fn mirror_wired_count() -> usize {
    MIRROR_WIRED_NAMES.len()
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
    mark_mirror_wired("collect_client_fingerprint");
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
    mark_mirror_wired("trust_forwarded_header");
    TRUST_FORWARDED_HEADER.store(enabled, std::sync::atomic::Ordering::Relaxed);
}

/// IP 黑名单业务层镜像(ArcSwap 热更)。**与 security 中间件的黑名单互补**:
/// 中间件用 TCP 对端 IP(反代后=反代内网 IP,拿不到真实客户端),而对话/记账路径的
/// [`trusted_client_ip`] 读 XFF/X-Real-IP 最右段=**真实客户端 IP**(委托
/// [`crate::common::security::client_ip_from_headers`]，与中间件同源同口径)。故在此业务层再判一次,
/// 命中即拒——这样即便部署在 openresty/nginx 反代后、未开 trust_forwarded,也能按真实 IP 封禁。
/// 启动时由 main 接线、admin 改 ip_blocklist 时热更(无需重启),存已解析的 Cidr 列表。
static IP_BLOCKLIST: std::sync::OnceLock<arc_swap::ArcSwap<Vec<crate::common::security::Cidr>>> =
    std::sync::OnceLock::new();

fn ip_blocklist_cell() -> &'static arc_swap::ArcSwap<Vec<crate::common::security::Cidr>> {
    IP_BLOCKLIST.get_or_init(|| arc_swap::ArcSwap::from_pointee(Vec::new()))
}

/// 设置业务层 IP 黑名单(启动接线 / admin 热更调用)。非法条目跳过。
pub fn set_ip_blocklist(entries: &[String]) {
    mark_mirror_wired("ip_blocklist");
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
    mark_mirror_wired("machine_code_blocklist");
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
/// 解析真实客户端 IP（[`trusted_client_ip`]，回退 TCP 对端），而非复用 `ClientInfo`（后者在
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
    // A1 修复:trusted_client_ip 取 XFF 最右段;仅当对端是可信反代(私网/环回)时才采信 XFF,
    // 公网直连客户端伪造的 XFF 被忽略(用对端 IP),与中间件 client_ip 口径统一。
    let real_ip = trusted_client_ip(headers, peer);

    if let Some(ip) = real_ip.as_deref() {
        if ip_is_blocked(ip) {
            tracing::warn!(client_ip = %ip, "IP 黑名单拦截:拒绝该来源请求(403)");
            // D2 接入（M2 补充）：IP 黑名单 403 走配置 key `permission_denied`
            // （A9 region 错配已改用独立 key `region_mismatch`，此 key 专属于本分支）。
            let (status, error_type, message, _) = resolve_msg(
                &current_error_messages(),
                "permission_denied",
                (
                    StatusCode::FORBIDDEN,
                    "permission_error",
                    "来源 IP 已被封禁",
                    None,
                ),
            );
            return Some((status, Json(ErrorResponse::new(error_type, message))).into_response());
        }
    }

    // 机器码黑名单:按真实 IP 重算(device 仅无 IP 时兜底;关指纹时 device=None 不影响 IP 派生)。
    let code = crate::usage::machine_code_of(real_ip.as_deref(), None);
    if machine_code_is_blocked(&code) {
        tracing::warn!(machine_code = %code, client_ip = ?real_ip, "机器码黑名单拦截:拒绝该机器请求(403)");
        // D3 接入（M2 补充）：机器码黑名单走配置 key `machine_blocked`。
        let (status, error_type, message, _) = resolve_msg(
            &current_error_messages(),
            "machine_blocked",
            (
                StatusCode::FORBIDDEN,
                "permission_error",
                "sbsbsb！",
                None,
            ),
        );
        return Some((status, Json(ErrorResponse::new(error_type, message))).into_response());
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
    mark_mirror_wired("extract_thinking");
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
    mark_mirror_wired("cc_auto_buffer");
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
    mark_mirror_wired("prompt_cache_enabled");
    PROMPT_CACHE_ENABLED.store(enabled, std::sync::atomic::Ordering::Relaxed);
}

fn prompt_cache_enabled() -> bool {
    PROMPT_CACHE_ENABLED.load(std::sync::atomic::Ordering::Relaxed)
}

/// 透传路径**模拟缓存**注入开关（`mockCacheEnabled` 的进程镜像）。
///
/// custom_api 透传路径（passthrough.rs）的响应是上游原样字节，`cache_read_input_tokens`
/// 来自上游（DeepSeek 恒 0）→ 下游（sub2api 等）看不到缓存分支。开启后透传 filter
/// 把 usage 的 `cache_read_input_tokens` 注入为 `round(input_tokens × ratio)`、
/// creation 置 0（模拟全命中不写缓存）——**伪造值，仅供下游展示，不是真实计费依据**
/// （与 [`PROMPT_CACHE_ENABLED`] 的估算下发同性质）。
///
/// 默认关；main 启动接线、admin 热更即时改写（TIER3 镜像范式，与
/// [`PROMPT_CACHE_ENABLED`] 相同：进程级 static 原子，无锁近零成本）。
static MOCK_CACHE_ENABLED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
static MOCK_CACHE_READ_RATIO: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0.7f64.to_bits());

/// 设置模拟缓存配置（main 启动接线 / admin 热更调用，立即生效）。
/// ratio 在此清洗：非有限值（NaN/±inf）归默认 0.7，其余 clamp 到 [0.0, 1.0]
/// —— 读取处拿到的恒是合法值，注入逻辑无需再防御。
pub fn set_mock_cache_config(enabled: bool, ratio: f64) {
    mark_mirror_wired("mock_cache_config");
    let ratio = sanitize_mock_cache_ratio(ratio);
    // ⚠️ 先写 ratio 再写 enabled（关优先语义）：读取处（mock_cache_config）先读
    // enabled 再读 ratio——关闭瞬间 enabled=false 一旦写入，读侧立即不再注入，
    // 不残留注入窗口；开启瞬间读侧看到 enabled=true 时 ratio 必已是新清洗值，
    // 不会用旧比例注入。
    MOCK_CACHE_READ_RATIO.store(ratio.to_bits(), std::sync::atomic::Ordering::Relaxed);
    MOCK_CACHE_ENABLED.store(enabled, std::sync::atomic::Ordering::Relaxed);
    if enabled {
        tracing::warn!("模拟缓存已启用：cache_read = input × {ratio}（伪造值，仅供下游展示）");
    }
}

/// mock ratio 清洗：非有限值 → 默认 0.7；否则 clamp 到 [0.0, 1.0]。
pub(crate) fn sanitize_mock_cache_ratio(ratio: f64) -> f64 {
    if !ratio.is_finite() {
        return 0.7;
    }
    ratio.clamp(0.0, 1.0)
}

/// 透传路径读取处：模拟缓存开关 + 清洗后的 ratio（`enabled=false` 时 ratio 无意义）。
pub(crate) fn mock_cache_config() -> (bool, f64) {
    let enabled = MOCK_CACHE_ENABLED.load(std::sync::atomic::Ordering::Relaxed);
    let ratio = f64::from_bits(MOCK_CACHE_READ_RATIO.load(std::sync::atomic::Ordering::Relaxed));
    (enabled, ratio)
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
    mark_mirror_wired("tool_clean_leaked_tokens");
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
    mark_mirror_wired("tool_reclaim_textified_invoke");
    TOOL_RECLAIM_TEXTIFIED_INVOKE.store(enabled, std::sync::atomic::Ordering::Relaxed);
}
pub(crate) fn tool_reclaim_textified_invoke_enabled() -> bool {
    TOOL_RECLAIM_TEXTIFIED_INVOKE.load(std::sync::atomic::Ordering::Relaxed)
}

/// stray token(call/count/card/court)复读熔断开关(默认 **true**):连续独占行复读超阈值截断本轮文本。
static TOOL_STRAY_REPEAT_GUARD: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(true);
pub fn set_tool_stray_repeat_guard(enabled: bool) {
    mark_mirror_wired("tool_stray_repeat_guard");
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
    mark_mirror_wired("tool_stream_align_failure");
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
    mark_mirror_wired("tool_expose_error_to_client");
    TOOL_EXPOSE_ERROR_TO_CLIENT.store(enabled, std::sync::atomic::Ordering::Relaxed);
}
pub(crate) fn tool_expose_error_to_client_enabled() -> bool {
    TOOL_EXPOSE_ERROR_TO_CLIENT.load(std::sync::atomic::Ordering::Relaxed)
}

/// ④JSON 修复层开关（根治向）。默认 **true**——只在 JSON 已非法时介入 + 修复后强制复验，正常流零影响。
static TOOL_REPAIR_JSON: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);
/// 设置 JSON 修复层开关（main 启动接线 / admin 热更调用，立即生效）。
pub fn set_tool_repair_json(enabled: bool) {
    mark_mirror_wired("tool_repair_json");
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
    mark_mirror_wired("tool_truncation_recovery");
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
    mark_mirror_wired("compression");
    compression_cell().store(std::sync::Arc::new(compression));
}

/// 读当前压缩配置快照（热路径 load_full 无锁近零成本）。
pub(crate) fn current_compression() -> std::sync::Arc<crate::model::config::CompressionConfig> {
    compression_cell().load_full()
}

/// 错误消息配置表的进程级镜像（热更，范式同 [`COMPRESSION`]）。
///
/// `Config.error_messages`（`HashMap<String, ErrorMessageOverride>`）非标量（一张表），
/// 用 `ArcSwap` 承载：admin 改配置时整份原子换、错误翻译处 `load_full()` 拿 `Arc`
/// 快照（无锁近零成本）。`OnceLock` 惰性初始化，config 结构层落地后由 reload_config
/// 接线（main 启动写入真配置 / admin 热更改写）；未初始化时回退空表 ——
/// [`resolve_msg`] 全部落到调用点内置默认，零行为变化。
///
/// ⚠️ 依赖方向纪律（结构绊脚石 #16）：本镜像与 [`COMPRESSION`]、MOCK_CACHE_* 的
/// setter/getter 是**全仓仅有的 kiro 层反向引用 anthropic 层**（调用环
/// `kiro/token_manager → anthropic/handlers`）——reload_config 的热更接线跨层依赖
/// 它们。环的另一半（anthropic → kiro 主方向；provider 仍调 `absorb_class_of`）
/// 无法归零。`AbsorbClass` 类型已下沉 `model/`。纪律为：**新增 kiro 层对
/// anthropic 层的引用必须在 review 时显式说明理由**；镜像正解是搬去中性模块
/// （如 common/runtime_state.rs）后 re-export。
pub(crate) type ErrorMessagesTable =
    std::collections::HashMap<String, crate::model::config::ErrorMessageOverride>;

static ERROR_MESSAGES: std::sync::OnceLock<
    arc_swap::ArcSwap<ErrorMessagesTable>,
> = std::sync::OnceLock::new();

fn error_messages_cell() -> &'static arc_swap::ArcSwap<ErrorMessagesTable> {
    ERROR_MESSAGES.get_or_init(|| arc_swap::ArcSwap::from_pointee(ErrorMessagesTable::new()))
}

/// 设置错误消息配置表（main 启动接线 / admin 热更调用，立即生效，下个请求即读到新值）。
pub fn set_error_messages(table: ErrorMessagesTable) {
    mark_mirror_wired("error_messages");
    error_messages_cell().store(std::sync::Arc::new(table));
}

/// 错误翻译处读当前表快照。
pub(crate) fn current_error_messages() -> std::sync::Arc<ErrorMessagesTable> {
    error_messages_cell().load_full()
}

/// 从错误消息配置表解析一个 key 的渲染值。
///
/// 未配置该 key（或字段为 None）→ 用调用点内置默认（= 现状文案/码/退避秒数，零行为变化）。
/// 返回 `(status, error.type, message, retry_after)`；`retry_after` 仅当配置给了值才是
/// `Some`（调用点自行决定优先级 —— **号池真值 `retry_after_secs=N` 永远优先于配置**）。
///
/// 接口约定（与并行 config 结构层对齐）：字段名 `error_messages`、
/// `ErrorMessageOverride { status, type, message, retry_after_secs }`，全部 Optional，
/// None = 用内置默认（只改 message 时 status/type 不填）。
/// default 的 `message` 允许非 'static（调用点可能是动态前缀，如透传池的
/// `err_response` 传调用点文案）；返回值与默认借用无关（全部 owned）。
pub(crate) fn resolve_msg<'a>(
    cfg: &ErrorMessagesTable,
    key: &str,
    default: (StatusCode, &'a str, &'a str, Option<u64>),
) -> (StatusCode, String, String, Option<u64>) {
    let Some(ov) = cfg.get(key) else {
        return (
            default.0,
            default.1.to_string(),
            default.2.to_string(),
            default.3,
        );
    };
    let status = ov
        .status
        .and_then(|s| StatusCode::from_u16(s).ok())
        .unwrap_or(default.0);
    let error_type = ov.r#type.clone().unwrap_or_else(|| default.1.to_string());
    let message = ov.message.clone().unwrap_or_else(|| default.2.to_string());
    let retry_after = ov.retry_after_secs.or(default.3);
    (status, error_type, message, retry_after)
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

    if wants_stream {
        let p_ok = provider.clone();
        let c_ok = client.clone();
        let p_err = provider.clone();
        let c_err = client.clone();
        let payload_err = (*payload).clone();
        return Some(
            sse_event_stream_builder()
                .body(websearch::mixed_websearch_stream_response(
                    provider.clone(),
                    (*payload).clone(),
                    fallback_input_tokens,
                    budget.clone(),
                    Box::new(move |success| {
                        tracing::info!(
                            hops = success.hops.len(),
                            rounds = success.rounds,
                            "WebSearch 回灌直播结束"
                        );
                        emit_websearch_loop_usage(&p_ok, success, &c_ok, true);
                    }),
                    Box::new(move |fail| {
                        emit_websearch_loop_error_usage(&p_err, &payload_err, fail, &c_err, true);
                    }),
                ))
                .unwrap(),
        );
    }

    let resp = match websearch::run_web_search_loop(
        provider.clone(),
        (*payload).clone(),
        fallback_input_tokens,
        budget,
        None,
    )
    .await
    {
        Ok(success) => {
            emit_websearch_loop_usage(provider, &success, client, wants_stream);
            (
                StatusCode::OK,
                Json(websearch::build_loop_json_body(&success)),
            )
                .into_response()
        }
        Err(mut fail) => {
            // 回灌失败响应可能带 x-kirostudio-compress-retry 内部标记（上游 400
            // CONTENT_LENGTH_EXCEEDS 时 map_provider_error 设置；回灌循环在压缩重试/
            // strip 点之前，不在这里清就会透传客户端——与 /v1、/cc/v1 的 F1b 同款，
            // 2026-08-11 对抗审查 M1）。回灌路径**无压缩重试循环**，标记无消费者。
            fail.response
                .headers_mut()
                .remove("x-kirostudio-compress-retry");
            emit_websearch_loop_error_usage(provider, payload, &fail, client, wants_stream);
            fail.response
        }
    };
    Some(resp)
}

/// 双入口共享：KiroProvider 未配置 → 503 服务不可用响应。
/// 与 /cc/v1 入口同 key（provider_not_configured）：双入口读同一配置。
fn render_provider_not_configured() -> Response {
    tracing::error!("KiroProvider 未配置");
    let (status, error_type, message, _) = resolve_msg(
        &current_error_messages(),
        "provider_not_configured",
        (
            StatusCode::SERVICE_UNAVAILABLE,
            "service_unavailable",
            "Kiro API provider not configured",
            None,
        ),
    );
    (status, Json(ErrorResponse::new(error_type, message))).into_response()
}

/// 双入口共享：max_tokens 本地上限校验（超上限 = 400，不超 = None）。
/// 🔴 2026-08-15 线上 smoke test 发现：超出上游上限的 max_tokens 此前被误判为
/// 瞬态错误吞进 failover+absorb（30s 延迟 + 503 误判，客户端等预算耗尽）。
/// 上限对齐上游实测（fuckopencode/deepseek 均 393216）。双入口（/v1 与 /cc/v1）同检查。
fn check_max_tokens_limit(max_tokens: i32) -> Option<Response> {
    if max_tokens > 393216 {
        let err_msgs = current_error_messages();
        let (s, t, m, _) = resolve_msg(
            &err_msgs,
            "max_tokens_exceeded",
            (
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "max_tokens 超出上限 393216",
                None,
            ),
        );
        return Some((s, Json(ErrorResponse::new(t, m))).into_response());
    }
    None
}

/// 双入口共享：转换错误三形态 → (status, error.type, message)。
/// 三种形态各读各的 key（与另一入口同 key：双入口读同一配置）。
fn render_conversion_error(e: &ConversionError) -> (StatusCode, String, String) {
    let err_msgs = current_error_messages();
    match e {
        ConversionError::UnsupportedModel(model) => {
            let (s, t, m, _) = resolve_msg(
                &err_msgs,
                "unsupported_model",
                (StatusCode::BAD_REQUEST, "invalid_request_error", "模型不支持", None),
            );
            (s, t, format!("{m}: {model}"))
        }
        ConversionError::EmptyMessages => {
            let (s, t, m, _) = resolve_msg(
                &err_msgs,
                "empty_messages",
                (StatusCode::BAD_REQUEST, "invalid_request_error", "消息列表为空", None),
            );
            (s, t, m)
        }
        ConversionError::UnsupportedToolMapping { tool_name, reason } => {
            let (s, t, m, _) = resolve_msg(
                &err_msgs,
                "tool_mapping_failed",
                (
                    StatusCode::BAD_REQUEST,
                    "invalid_request_error",
                    "工具参数无法映射",
                    None,
                ),
            );
            (s, t, format!("{m}: {tool_name} — {reason}"))
        }
    }
}

/// 双入口共享：Kiro 请求体序列化失败 → 500（key `request_serialization_failed`，
/// 双入口同 key、压缩重试轮同配置）。日志由调用点打（区分初试/重试文案）。
fn render_serialization_failed(e: &impl std::fmt::Display) -> Response {
    let (status, error_type, message, _) = resolve_msg(
        &current_error_messages(),
        "request_serialization_failed",
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            "序列化请求失败",
            None,
        ),
    );
    (status, Json(ErrorResponse::new(error_type, format!("{message}: {e}")))).into_response()
}

/// 双入口共享：WebSearch 本地处理（快路径 + agentic 回灌循环）。
/// 返回 `Some` = 已生成最终响应（调用方直接 return）；`None` = 走常规转发。
/// 收口原因同 [`dispatch_web_search_loop`]：双入口此前逐字复制同一段，只改一处
/// 即行为分叉。混合工具场景（web_search + 其他工具）请求带 web_search 但未显式
/// 触发搜索时不再剔除 web_search：交给 converter 归一化成 Kiro 认的函数工具形态，
/// 上游回 web_search tool_use 且本轮无其他工具 → agentic 回灌；混入非 web_search
/// 工具 → 整轮原样回客户端。纯 web_search 与显式触发走本地 MCP 快路径。
async fn dispatch_websearch_paths(
    provider: &std::sync::Arc<crate::kiro::provider::KiroProvider>,
    payload: &MessagesRequest,
    budget: &crate::kiro::provider::SharedRetryBudget,
    client: &ClientInfo,
) -> Option<Response> {
    // 检查是否应本地处理 WebSearch 请求（tool_choice 强制 / 纯 web_search 单工具 / Claude Code 前缀）
    // 纯透传池（无可用 Kiro 号）跳过 MCP 快路径与回灌，让请求走上游转发。
    let kiro_mcp_ok = provider.has_enabled_kiro_credential();
    if websearch::should_handle_websearch_request(payload) {
        if kiro_mcp_ok {
            tracing::info!("检测到 WebSearch 请求，路由到本地 WebSearch 处理");
            // 估算输入 tokens（只读计数，传引用避免深拷贝整个对话历史）
            let input_tokens = token::count_all_tokens(
                &payload.model,
                payload.system.as_deref(),
                &payload.messages,
                payload.tools.as_deref(),
            ) as i32;
            return Some(
                websearch::handle_websearch_request(
                    provider.clone(),
                    payload,
                    input_tokens,
                    budget,
                    client,
                )
                .await,
            );
        }
        tracing::info!(
            "WebSearch 快路径跳过：号池无可用 Kiro 凭据（纯透传池），改走上游转发"
        );
    }
    if kiro_mcp_ok {
        if let Some(resp) = dispatch_web_search_loop(provider, payload, budget, client).await {
            return Some(resp);
        }
    }
    None
}

/// 双入口共享：压缩重试循环的每一跳上游分发（buffered 流式 / 真流式 / 非流式）。
/// `use_buffered` 由入口决定并保留入口差异：/v1 = ccAutoBuffer ∧ Claude Code 请求；
/// /cc/v1 = 仅 ccAutoBuffer。分发后的压缩重试判定（内部标记头）仍在入口循环内。
async fn dispatch_kiro_attempt(
    provider: std::sync::Arc<crate::kiro::provider::KiroProvider>,
    body_ref: &str,
    payload: &MessagesRequest,
    input_tokens: i32,
    thinking_enabled: bool,
    tool_name_map: std::collections::HashMap<String, String>,
    known_tool_names: std::collections::HashSet<String>,
    tool_required_fields: std::collections::HashMap<String, Vec<String>>,
    cache_breakdown: Option<CacheUsageBreakdown>,
    fingerprint_usage: Option<crate::anthropic::cache::PromptCacheUsage>,
    budget: &crate::kiro::provider::SharedRetryBudget,
    client: ClientInfo,
    use_buffered: bool,
) -> Response {
    if payload.stream {
        if use_buffered {
            handle_stream_request_buffered(
                provider,
                body_ref,
                &payload.model,
                input_tokens,
                thinking_enabled,
                tool_name_map,
                known_tool_names,
                tool_required_fields,
                cache_breakdown,
                budget,
                client,
            )
            .await
        } else {
            handle_stream_request(
                provider,
                body_ref,
                &payload.model,
                input_tokens,
                thinking_enabled,
                tool_name_map,
                known_tool_names,
                tool_required_fields,
                cache_breakdown,
                budget,
                client,
            )
            .await
        }
    } else {
        // 非流式响应：仅在配置开启时提取 thinking 块
        let extract_thinking = extract_thinking_enabled() && thinking_enabled;
        handle_non_stream_request(
            provider,
            body_ref,
            &payload.model,
            input_tokens,
            extract_thinking,
            tool_name_map,
            tool_required_fields,
            cache_breakdown,
            fingerprint_usage,
            budget,
            client,
        )
        .await
    }
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
    wants_stream: bool,
) {
    let mut record =
        crate::usage::RequestRecord::new(Uuid::new_v4().to_string(), success.model.clone());
    record.requested_model = Some(success.model.clone());
    // 双口径补齐：回灌每轮走 `call_api_stream`（Kiro 主链路，请求体带 modelId、
    // 经过全局模型映射），末轮的映射结果由 run_round 从 CallMeta.mapped_model 带出，
    // 与 credential_id 同源（同为末轮）。None = 末轮未命中映射/凭据豁免。
    record.upstream_model = success.mapped_model.clone();
    record.credential_id = Some(success.credential_id);
    // 恒 false 的旧实现：面板上所有回灌记录都显示非流式，无法按客户端形态过滤。
    record.is_streaming = wants_stream;
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

/// 回灌循环失败时的客户端请求埋点（与成功路径成对：一次客户端请求一条记录）。
///
/// 失败时没有客户端可见的输出，`output_tokens` 记 0；credits 只写循环已计量到的值
/// （没有就 None，不编 0 成功）。outcome 按响应状态归到既有 [`RequestOutcome`] 变体。
fn emit_websearch_loop_error_usage(
    provider: &crate::kiro::provider::KiroProvider,
    payload: &MessagesRequest,
    fail: &websearch::WebSearchLoopError,
    client: &ClientInfo,
    wants_stream: bool,
) {
    let status = fail.response.status();
    let mut record =
        crate::usage::RequestRecord::new(Uuid::new_v4().to_string(), payload.model.clone());
    record.requested_model = Some(payload.model.clone());
    record.upstream_model = fail.mapped_model.clone();
    record.credential_id = fail.credential_id;
    record.is_streaming = wants_stream;
    record.input_tokens = fail.input_tokens;
    record.output_tokens = 0;
    record.credits_used = fail.credits;
    record.retries = fail.rounds.saturating_sub(1);
    record.outcome = websearch_loop_error_outcome(status);
    record.error_message = Some(format!(
        "web_search loop failed (status {})",
        status.as_u16()
    ));
    if let (Some(c), Some(id)) = (record.credits_used, record.credential_id) {
        provider.report_credits(id, c);
    }
    client.apply(&mut record);
    crate::usage::emit_record(record);
}

/// 回灌失败 HTTP 状态 → 用量 outcome。对齐 `map_provider_error` 落给客户端的码，
/// 不是 Success。503 预算/吸收耗尽按 ServerError（ModelUnavailable 只用于上游容量标记）。
fn websearch_loop_error_outcome(status: StatusCode) -> crate::usage::RequestOutcome {
    match status.as_u16() {
        429 => crate::usage::RequestOutcome::RateLimited,
        401 | 403 => crate::usage::RequestOutcome::AuthFailed,
        400 | 413 => crate::usage::RequestOutcome::BadRequest,
        500 | 502 | 503 | 504 => crate::usage::RequestOutcome::ServerError,
        _ => crate::usage::RequestOutcome::OtherError,
    }
}

/// WebSearch **快路径**（`handle_websearch_request` 本地 MCP 单轮搜索）成功时的用量埋点。
///
/// 与 [`emit_websearch_loop_usage`]（回灌循环）互补：快路径此前**零埋点**，面板上
/// 纯 web_search 请求的流量完全不可见。差异（诚实边界）：
/// - `credential_id` 来自 MCP 调用成功时 provider 返回的实账凭据（拿不到时为 None）；
/// - MCP 响应无 meteringEvent → `credits_used` 不写（与 build_mcp_record 同口径）；
/// - `retries` 恒 0（单轮无重试语义）。
///
/// `pub(super)`：websearch.rs 在成功收尾处调用（两条路径各埋各的，不混用）。
pub(super) fn emit_websearch_fast_path_usage(
    model: &str,
    credential_id: Option<u64>,
    input_tokens: i32,
    output_tokens: i32,
    wants_stream: bool,
    client: &ClientInfo,
) {
    let mut record = crate::usage::RequestRecord::new(Uuid::new_v4().to_string(), model.to_string());
    record.requested_model = Some(model.to_string());
    record.credential_id = credential_id;
    record.is_streaming = wants_stream;
    record.input_tokens = input_tokens;
    record.output_tokens = output_tokens;
    record.outcome = crate::usage::RequestOutcome::Success;
    client.apply(&mut record);
    crate::usage::emit_record(record);
}

/// 请求来源的客户端画像（设备类型 + IP + 细分 OS + 浏览器），
/// 一并沿用量埋点路径传递，避免多参数散落。
///
/// `pub(super)`：websearch.rs 的快路径埋点（emit_websearch_fast_path_usage）也要
/// 消费同一份画像，避免在两处各存一份客户端信息结构。
#[derive(Clone, Default)]
pub(super) struct ClientInfo {
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

/// 压缩重试轮重建请求体：字节兜底先行，再以更紧的 target 走 token 压缩管道。
///
/// 量纲对齐：上游按**字节**拒绝（400 CONTENT_LENGTH_EXCEEDS_THRESHOLD），而 token
/// 压缩压不到目标时（长会话字节先撞线），先执行
/// [`crate::anthropic::converter::apply_byte_overflow_guard`]——扁平化历史里除活跃
/// 轮次外的结构化工具轮次 + 按字节截断丢最旧历史（占位说明 + 保 user/assistant
/// 交替 + 剥孤立 toolResult），再走 `build_kiro_request_body` 的 token 压缩。
/// 比 token 压缩更激进，但**只在 400 触发压缩重试的路径执行**，正常路径（初试）
/// 不受影响，与 `adaptive_compress_loop` 不重复打架。
fn rebuild_body_for_compress_retry(
    conv_state: &mut crate::kiro::model::requests::conversation::ConversationState,
    native_fields: &Option<crate::kiro::model::requests::kiro::AdditionalModelRequestFields>,
    compression_cfg: &crate::model::config::CompressionConfig,
    attempt: u32,
) -> Result<String, serde_json::Error> {
    // ⭐ 显式压缩关闭时跳过字节兜底（与 adaptive_compress_loop 的 enabled 守卫同款）：
    // 用户关闭压缩 = 明确要求不干预请求，400 超限应透传而非静默丢历史。
    if compression_cfg.enabled {
        crate::anthropic::converter::apply_byte_overflow_guard(conv_state);
    } else {
        tracing::warn!(
            attempt = attempt,
            "CONTENT_LENGTH_EXCEEDS 重试但压缩已显式关闭：跳过字节兜底，不丢历史"
        );
    }
    let target = compress_retry_target(compression_cfg.trigger_bytes, attempt);
    let body = build_kiro_request_body(
        conv_state.clone(),
        native_fields.clone(),
        compression_cfg,
        Some(target),
    )?;
    tracing::info!(
        attempt = attempt,
        target_bytes = target,
        body_len = body.len(),
        "CONTENT_LENGTH_EXCEEDS: 重新压缩请求体并重试"
    );
    Ok(body)
}

/// 已翻译的上游错误：HTTP 状态 + Anthropic 错误类型码 + 面向用户的中文消息（含排障步骤）。
///
/// `error_type` 用 `String`（非 `&'static str`）：错误消息可配置化后 type 可能来自
/// 配置表（运行时字符串），无法静态借用。message 同理本就为 String。
struct TranslatedError {
    status: StatusCode,
    error_type: String,
    message: String,
    /// 网关可以在更激进的压缩下重试（CONTENT_LENGTH_EXCEEDS_THRESHOLD 等可自愈错误）。
    /// 对应的响应带上 `x-kirostudio-compress-retry` 头，handler 据此决定是否重新压缩并重试。
    retry_compress: bool,
    /// 配置表给的 `retryAfterSecs`（默认 None = 不带 Retry-After）。仅适用于**可重试语义**
    /// 的分支；永久态（404 subscription_unsupported 等）与 400 请求错误分支不产出。
    retry_after_override: Option<u64>,
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
    // 🔴 `subscription_unsupported=1`：provider 打在「订阅不覆盖本应用/模型」bail 串上的
    // 机器可读标记（provider.rs 注释明言：永久条件，不换区、不重试、不计凭据失败）。
    // **必须排在最前**：旧代码被下方 `contains("subscription")` 宽匹配先命中 → 译成
    // 502 `api_error`（可重试语义）+「刷新 Token」误导文案 —— 而订阅档位缺失刷新多少次
    // 都不会变，客户端还会按 5xx 盲退避重打。与 `model_unsupported_by_pool=1` 同范式：
    // 404 `not_found_error`，绝不带 Retry-After（给了就等于宣称「等一会儿会好」）。
    if err_str.contains("subscription_unsupported=1") {
        // ⚠️ 永久态：配置的 retry_afterSecs 刻意**忽略**（带了等于宣称「等一会儿会好」，
        // 客户端按 5xx 盲退避重打一个重试永远不会变的请求）。
        let (status, error_type, message, _) = resolve_msg(
            &current_error_messages(),
            "subscription_unsupported",
            (
                StatusCode::NOT_FOUND,
                "not_found_error",
                "当前凭据的订阅档位不支持该应用/模型（永久条件，非临时故障）。换区或重试均无效：请更换为订阅覆盖该应用/模型的凭据，或联系账号管理员开通对应档位。",
                None,
            ),
        );
        return Some(TranslatedError {
            retry_compress: false,
            retry_after_override: None,
            status,
            error_type,
            message,
        });
    }
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
        let (_, _, message, _) = resolve_msg(
            &current_error_messages(),
            "quota_exhausted",
            (
                StatusCode::PAYMENT_REQUIRED,
                "billing_error",
                "号池内所有凭据的月度请求配额均已耗尽（当月内不可恢复）。\
                 排障：①面板查看各凭据用量；②等待配额周期重置——跨月后自动恢复，无需人工介入；\
                 ③为号池补充新凭据可立即恢复。",
                None,
            ),
        );
        return Some(TranslatedError {
            retry_compress: false,
            status: StatusCode::PAYMENT_REQUIRED,
            error_type: "billing_error".to_string(),
            message,
            retry_after_override: None,
        });
    }
    // 兜底：仍保留配额 reason 码识别（判据收口在
    // `endpoint::default_is_monthly_request_limit`），但**降级为「单号/未知范围」的配额语义**。
    //
    // 为什么不能直接删掉（这是本次收口最容易做错的地方）：并非所有配额错误都经过
    // 上面那个标记 —— MCP 路径（`call_mcp_with_retry`）、透传路径、以及未来新增的
    // 上游分支都可能把带配额 reason 码的 body 冒泡上来。删掉该兜底会让它们
    // 落 `map_provider_error` 末尾兜底 → **502 无 Retry-After** → 客户端当永久故障、
    // 不退避、原样重发（这正是本仓反复踩过的那类回归）。
    //
    // 🔴 2026-08-15 收窄：原判据 `contains("QUOTA")` 是**宽判据**——上游 body 任意含
    // 大写 QUOTA 即判配额耗尽（错误码里恰好带 QUOTA 的无关文案会被 429 误导退避）。
    // 现收口到 endpoint 侧词表（QUOTA_EXHAUSTED_REASONS = MONTHLY_REQUEST_COUNT /
    // OVERAGE_REQUEST_LIMIT_EXCEEDED，JSON reason 精确值 + 子串兜底），与
    // endpoint/主路径侧（endpoint/mod.rs:190 的配额分类）共用一份判据，
    // 不再在此处新写子串匹配。
    //
    // 保留但**改文案**：不再断言"所有凭据"（那是标记分支才能确认的事实），
    // 避免面板/用户按错误的范围去排障。状态码维持 429（可退避）不变。
    if crate::kiro::endpoint::default_is_monthly_request_limit(err_str) {
        let (status, error_type, message, cfg_ra) = resolve_msg(
            &current_error_messages(),
            "quota_subscription",
            (
                StatusCode::TOO_MANY_REQUESTS,
                "rate_limit_error",
                "请求配额已耗尽。排障：①面板查看各凭据用量，切到仍有额度的账号；②等待配额周期重置；③为号池补充新凭据。",
                None,
            ),
        );
        return Some(TranslatedError {
            retry_compress: false,
            status,
            error_type,
            message,
            retry_after_override: cfg_ra,
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
        // B4 矛盾修复（设计 §五 1）：容量 503 现状无 Retry-After（客户端不退避），
        // 补默认 3s（与 A11 的 5xx 退避同档）；配置的 retryAfterSecs 可覆盖。
        let (status, error_type, message, cfg_ra) = resolve_msg(
            &current_error_messages(),
            "overloaded_capacity",
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "overloaded_error",
                "上游模型暂时不可用（负载过高），请稍后重试。若持续出现：①换用同族其他版本（如 claude-opus-4.8）；②新发布模型发布初期容量有限，属正常现象，等待 1~2 小时后通常恢复。",
                Some(3),
            ),
        );
        return Some(TranslatedError {
            retry_compress: false,
            status,
            error_type,
            message,
            retry_after_override: cfg_ra,
        });
    }
    if err_str.contains("FEATURE_NOT_SUPPORTED") {
        let (status, error_type, message, cfg_ra) = resolve_msg(
            &current_error_messages(),
            "feature_not_supported",
            (
                StatusCode::BAD_GATEWAY,
                "api_error",
                "当前凭据所在 region 未开通该功能（profile 未激活）。排障：①网关会在刷新时自动验活重选可用 region；②如持续，右键该凭据切换 Profile ARN 到已开通 region（如 eu-central-1）；③确认该账号确在某 region 开通了 Kiro。",
                None,
            ),
        );
        return Some(TranslatedError {
            retry_compress: false,
            status,
            error_type,
            message,
            retry_after_override: cfg_ra,
        });
    }
    // ⚠️ "Improperly formed" **不在**凭据分支里（2026-08-11 对抗审查 M1）：
    // 上游对**用户请求体**的格式校验失败（工具 schema 属性、工具名超限、web_search 直发
    // 等，converter.rs/websearch.rs 多处实测记录）也回 `400 Improperly formed request`，
    // 且常带 `reason=REQUEST_BODY_INVALID`。混在这里会被说成「订阅失效/token 无效」——
    // 排障方向全错。它由 `translate_context_input` 里的请求体校验分支接管（400
    // invalid_request_error）。真正的凭据信号是 403 `Invalid token`（map_provider_error
    // 更前面的 403 分支处理）与 `subscription` 失效类文案。
    //
    // 🔴 2026-08-15 收窄：`subscription` 裸词是**宽判据**——「subscription does not
    // support」（订阅档位，语义应由 `subscription_unsupported=1` 标记处理）、
    // 「subscription rate limit / quota」（限流/配额）等**非凭据失效**文案也会命中，
    // 得到 502 +「刷新 Token」误导排障。只认订阅**失效**的连续形态
    // （expired / invalid / revoked / not found / no longer active 等）。
    if err_str.contains("Invalid token")
        || err_str.contains("subscription expired")
        || err_str.contains("subscription has expired")
        || err_str.contains("subscription is expired")
        || err_str.contains("subscription is invalid")
        || err_str.contains("invalid subscription")
        || err_str.contains("no active subscription")
        || err_str.contains("subscription not found")
        || err_str.contains("subscription revoked")
        || err_str.contains("subscription is no longer active")
    {
        let (status, error_type, message, cfg_ra) = resolve_msg(
            &current_error_messages(),
            "invalid_credential",
            (
                StatusCode::BAD_GATEWAY,
                "api_error",
                "上游拒绝凭据（订阅失效或 token 无效）。排障：①面板对该凭据点『刷新 Token』；②若为 Enterprise/IdC 号，确认 profileArn 已正确解析；③测活确认订阅有效，失效则更换凭据。",
                None,
            ),
        );
        return Some(TranslatedError {
            retry_compress: false,
            status,
            error_type,
            message,
            retry_after_override: cfg_ra,
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
        // 400 请求构造问题：配置的 retry_afterSecs 刻意**忽略**（重试原请求无意义）。
        let (status, error_type, message, _) = resolve_msg(
            &current_error_messages(),
            "image_mime_mismatch",
            (
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "图片声明的 media_type 与实际字节格式不符（上游 IMAGE_MIME_MISMATCH）。这是请求构造问题，重试无效。排障：①按图片真实格式填写 media_type（如 JPEG 字节不要声明 image/png）；②不要在改扩展名后沿用旧的 media_type；③重新读取并重新编码该图片后再发。",
                None,
            ),
        );
        return Some(TranslatedError {
            retry_compress: false,
            status,
            error_type,
            message,
            retry_after_override: None,
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
        let (status, error_type, message, _) = resolve_msg(
            &current_error_messages(),
            "request_body_invalid",
            (
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "请求体校验失败（上游 REQUEST_BODY_INVALID）。这是请求构造问题，重试无效。排障：①检查工具调用与工具结果的配对（上游对 tool 配对较严，截断/重排序会产生孤儿 tool_use）；②检查消息 role 与内容字段合法性；③重新构造请求后再发。",
                None,
            ),
        );
        return Some(TranslatedError {
            retry_compress: false,
            status,
            error_type,
            message,
            retry_after_override: None,
        });
    }
    // 两条都带 `OVERFLOW_COMPACT_HINT` 前缀：状态码与中文文案一字未改，只在最前面挂哨兵，
    // 让 Claude Code 的 compact-and-retry 认出「这是装不下，压缩后重试还有戏」。
    // 不改 400：这确实是「请求本身太大」，重试原请求无意义 —— 客户端要做的是**先压缩再重试**，
    // 而它认的正是 message 而非状态码（实测那条判据只看 message 子串）。
    // ⚠️ 配置 message 时由管理员自行保证前缀（「prompt is too long」是承重字符串，
    // 删了 Claude Code 的自动压缩静默失效——配置校验层对此告警，见设计 §3.1）。
    if err_str.contains("CONTENT_LENGTH_EXCEEDS_THRESHOLD") {
        let (status, error_type, message, _) = resolve_msg(
            &current_error_messages(),
            "context_too_large",
            (
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "prompt is too long: 上下文窗口已满（对话历史累积超出模型上下文上限）。排障：①精简对话历史或开新会话；②缩短 system prompt；③减少同时挂载的工具数量。",
                None,
            ),
        );
        return Some(TranslatedError {
            retry_compress: true,
            status,
            error_type,
            message,
            retry_after_override: None,
        });
    }
    if err_str.contains("Input is too long") {
        let (status, error_type, message, _) = resolve_msg(
            &current_error_messages(),
            "input_too_long",
            (
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "prompt is too long: 单次输入过长（请求体本身超出上游限制）。排障：①拆分过大的消息或附件；②减少一次性粘贴的文件内容；③对超大工具结果先做摘要。",
                None,
            ),
        );
        return Some(TranslatedError {
            retry_compress: true,
            status,
            error_type,
            message,
            retry_after_override: None,
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
        // 524 是 Cloudflare 边缘网关超时。http crate 没有 524 的标准 reason phrase，
        // 状态行只有裸 "524"，故用两条**形态**判据（仍不裸匹配数字）：
        // 1. 状态行形态 `: 524`：provider 组装的错误串统一是「{api_type} API 请求失败:
        //    {status} {body}」（provider.rs:3476 等），冒号+空格后紧跟的 524 即状态行；
        // 2. Cloudflare 错误页正文特征词的连续形态 "524 a timeout occurred"。
        // 🔴 2026-08-15（对抗审查 MINOR-5）删掉旧宽组合「数字 524 + timeout/gateway
        // 干扰词」：4xx 错误 body 里 "gateway abuse detected, 524 requests blocked"
        // 这类句子同时含 524 与 gateway 会被误判成可重试瞬态 5xx → 客户端对永久
        // 错误无限退避重试（见 `gateway_interference_words_with_524_are_not_5xx`）。
        || low.contains(": 524")
        || low.contains("524 a timeout occurred")
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
        let (status, error_type, message, cfg_ra) = resolve_msg(
            &current_error_messages(),
            "upstream_dns",
            (
                StatusCode::BAD_GATEWAY,
                "api_error",
                "DNS 解析失败（无法解析上游域名）。排障：①检查本机/容器 DNS 配置；②若走代理，确认代理能解析 kiro.dev；③确认网络出口正常。",
                None,
            ),
        );
        return Some(TranslatedError {
            retry_compress: false,
            status,
            error_type,
            message,
            retry_after_override: cfg_ra,
        });
    }
    if low.contains("timed out") || low.contains("timeout") {
        let (status, error_type, message, cfg_ra) = resolve_msg(
            &current_error_messages(),
            "upstream_timeout",
            (
                StatusCode::GATEWAY_TIMEOUT,
                "api_error",
                "连接上游超时。排障：①上游或代理可能拥塞，稍后重试；②检查代理延迟；③大请求可拆小以缩短单次耗时。",
                None,
            ),
        );
        return Some(TranslatedError {
            retry_compress: false,
            status,
            error_type,
            message,
            retry_after_override: cfg_ra,
        });
    }
    if low.contains("certificate") || low.contains("ssl") || low.contains("tls") {
        let (status, error_type, message, cfg_ra) = resolve_msg(
            &current_error_messages(),
            "upstream_tls",
            (
                StatusCode::BAD_GATEWAY,
                "api_error",
                "TLS/证书握手失败。排障：①检查系统时间是否准确；②若走中间人代理，确认其证书受信；③确认未误用被拦截的代理。",
                None,
            ),
        );
        return Some(TranslatedError {
            retry_compress: false,
            status,
            error_type,
            message,
            retry_after_override: cfg_ra,
        });
    }
    if low.contains("proxy") {
        let (status, error_type, message, cfg_ra) = resolve_msg(
            &current_error_messages(),
            "upstream_proxy",
            (
                StatusCode::BAD_GATEWAY,
                "api_error",
                "代理连接失败。排障：①检查代理地址/账密是否正确；②确认代理在线可达；③面板核对该凭据绑定的代理配置。",
                None,
            ),
        );
        return Some(TranslatedError {
            retry_compress: false,
            status,
            error_type,
            message,
            retry_after_override: cfg_ra,
        });
    }
    None
}

/// 上游显式 Retry-After 透传 marker 前缀（S2）。
///
/// provider 在 Kiro 主路径 429 分支把解析出的上游 Retry-After（响应头 `Retry-After`
/// 或 body `resets_*`）打进错误串（`upstream_retry_after=N`），`map_provider_error`
/// 的 A7 分支据此让客户端拿到上游真值（优先级：上游真值 > 配置 > 默认 8s）。
///
/// 与 `retry_after_secs=` 刻意**不同名**：那是**号池**冷却真值（A5 全池语义，
/// 走「所有凭据冷却」文案）；单凭据上游 429 若复用它会落 A5 的文案，语义错位。
///
/// 它是**网关自己打的串**（拼在错误串末尾），不是对上游 body 的子串判定 ——
/// 与 `retry_after_secs=` 同款误伤面（blockers §2.2.2 记录），本串特征足够独特，
/// 且解析用 `rsplit` 取**最后一次**出现（网关恒追加在末尾，body 噪声只会出现在前）。
pub(crate) const UPSTREAM_RETRY_AFTER_MARKER_PREFIX: &str = "upstream_retry_after=";

/// 从错误串里取 `upstream_retry_after=N` 的 N（上游显式 Retry-After 的秒数）。
///
/// 与 [`parse_retry_after_secs`] 同构但取**最后一次**出现（见上面 const 的说明）：
/// 网关恒把 marker 追加在错误串末尾，`rsplit` 天然跳过 body 里可能出现的同名字样。
pub(crate) fn parse_upstream_retry_after(err_str: &str) -> Option<u64> {
    err_str
        .rsplit(UPSTREAM_RETRY_AFTER_MARKER_PREFIX)
        .next()
        .and_then(|rest| rest.split(|c: char| !c.is_ascii_digit()).next())
        .and_then(|d| d.parse::<u64>().ok())
}

/// A7 判据的锚定版：`upstream_retry_after=N` 必须落在错误串**末尾**（trim 后）
/// 才算网关自己打的串。
///
/// 网关恒把 marker 追加在错误串末尾（429 分支与 `assemble_final_error` 都是
/// 追加），body 噪声里的同名字样只会出现在前。裸 `contains` 会把噪声误判成
/// 网关真值（Review3 m1）——锚定后只有「marker 之后直到串尾全是数字」才进
/// A7 分支（`parse_upstream_retry_after` 的解析结果与锚定判据天然一致）。
fn upstream_retry_after_anchored(err_str: &str) -> bool {
    match err_str.trim_end().rsplit_once(UPSTREAM_RETRY_AFTER_MARKER_PREFIX) {
        Some((_, tail)) => !tail.is_empty() && tail.bytes().all(|b| b.is_ascii_digit()),
        None => false,
    }
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

/// 吸收类别定义在 [`crate::model::AbsorbClass`]；本模块 re-export，供 `absorb_class_of`
/// 与 `use super::*` 测试沿用原名。
pub(crate) use crate::model::AbsorbClass;

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
    // 错误消息配置表快照（每请求一次，HashMap get O(1)；未配置 key → 内置默认 = 现状）。
    let err_msgs = current_error_messages();

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
        // 兜底值（15s 级退避即可，语义是「请客户端退避」）；配置的 retry_afterSecs 可覆盖。
        let (status, error_type, message, cfg_ra) = resolve_msg(
            &err_msgs,
            "shared_budget_exhausted",
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "api_error",
                "网关已就该请求打满上游调用预算（每请求上限），上游仍不可用（等容量）。\
                 这是可重试的瞬态状态，请按 Retry-After 退避后重试。",
                None,
            ),
        );
        let retry_after = cfg_ra
            .unwrap_or(ABSORB_EXHAUSTED_RETRY_AFTER_SECS)
            .clamp(1, 300);
        tracing::warn!(
            error = %err,
            retry_after_secs = retry_after,
            "每客户端请求的上游预算已耗尽（跨层共享），按 503 回：客户端自行退避"
        );
        return (
            status,
            [(header::RETRY_AFTER, retry_after.to_string())],
            Json(ErrorResponse::new(
                error_type,
                // ⚠️ 2026-08-15 认知纠错：本句里的「等容量」**不是** shield 判据 ——
                // 它只出现在 shield 的**注释**里（kiro_shield.py 337 行），不在
                // `COOLING_MARKERS` 判据表中（线上实测只有 3 个英文串，清单见
                // 文件下方守卫测试 `shield_cooling_markers_stay_in_production_text`）。
                // 因此 A1/A2 这两条 503 文案**不承载任何判据词**，属纯展示文案，
                // 可自由改（旧的「删掉它 ⇒ 丢弃 Retry-After」说法不成立）。
                // 改文案前仍建议先 `grep COOLING_MARKERS /opt/skiapi/services/kiro_shield.py`。
                message,
            )),
        )
            .into_response();
    }

    if err_str.contains(ABSORB_BUDGET_EXHAUSTED_MARKER) {
        let (status, error_type, message, cfg_ra) = resolve_msg(
            &err_msgs,
            "absorb_exhausted",
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "api_error",
                "网关已就该请求重试至预算上限，上游仍不可用（等容量）。这是可重试的瞬态状态，\
                 请按 Retry-After 退避后重试。若持续出现：①面板『限流健康』查看号池容量与冷却分布；\
                 ②补充凭据分摊上游压力；③必要时调高 upstreamRetryAbsorb* 预算。",
                None,
            ),
        );
        // Retry-After 优先用号池真值（`retry_after_secs=N`），其次按配置兜底，最后按类别兜底。
        let retry_after = parse_retry_after_secs(&err_str)
            .or(cfg_ra)
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
            status,
            [(header::RETRY_AFTER, retry_after.to_string())],
            Json(ErrorResponse::new(
                error_type,
                // 「等容量」同上：不是 shield 判据（2026-08-15 实测仅注释出现），
                // 本 503 文案不承载判据词，可自由改。
                message,
            )),
        )
            .into_response();
    }

    // 全池月度配额耗尽 → 402 billing_error（docs/quota-402-design.md）。
    // **必须排在 inbound_admission_timeout / parse_retry_after_secs / 速率 429 /
    // 临时风控之前**：形态 b 的 bail 同时带 quota_exhausted_all=1 与
    // retry_after_secs=，顺序反了会被 429+RA 接走，402 停手信号失效（k2cc H2）。
    // 只认显式标记；裸 MONTHLY_REQUEST_COUNT 仍走后面 429（单号/未知范围）。
    // 不带 Retry-After：给秒数会诱导当月反复砸死号。
    if err_str.contains("quota_exhausted_all=1") {
        let (_, _, message, _) = resolve_msg(
            &err_msgs,
            "quota_exhausted",
            (
                StatusCode::PAYMENT_REQUIRED,
                "billing_error",
                "号池内所有凭据的月度请求配额均已耗尽（当月内不可恢复）。\
                 排障：①面板查看各凭据用量；②等待配额周期重置——跨月后自动恢复，无需人工介入；\
                 ③为号池补充新凭据可立即恢复。",
                None,
            ),
        );
        tracing::warn!(error = %err, "全池配额耗尽，返回 402 billing_error（客户端应停手，跨月后自动回池）");
        return (
            StatusCode::PAYMENT_REQUIRED,
            Json(ErrorResponse::new("billing_error", message)),
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
        let (status, error_type, message, cfg_ra) = resolve_msg(
            &err_msgs,
            "gate_timeout",
            (
                StatusCode::TOO_MANY_REQUESTS,
                "rate_limit_error",
                "Gateway inbound rate shaping is at capacity (request admission timed out). \
                 This is gateway-side backpressure, not an upstream cooldown; retrying immediately will not help.",
                None,
            ),
        );
        // 串内真值优先（准入超时串自带 retry_after_secs=），配置值兜底。
        let retry_after = parse_retry_after_secs(&err_str).or(cfg_ra).unwrap_or(1).clamp(1, 300);
        tracing::warn!(
            retry_after_secs = retry_after,
            "入站准入排队超时（网关背压），返回 429 + Retry-After；不可吸收"
        );
        return (
            status,
            [(header::RETRY_AFTER, retry_after.to_string())],
            Json(ErrorResponse::new(error_type, message)),
        )
            .into_response();
    }

    // 上游并发闸已满（网关自己的背压，见 provider.rs 的 `upstream_gate_full=1`）。
    // 与 `inbound_admission_timeout` 同语义：必须 429 + Retry-After 让客户端退避，
    // 而不是落 502（502 会让客户端立即重发，重新灌满闸门，放大反而更凶）。
    // message 带 "gateway-side backpressure" 让重试层可区分，不当作上游问题重试。
    if err_str.contains("upstream_gate_full=1") {
        let (status, error_type, message, cfg_ra) = resolve_msg(
            &err_msgs,
            "upstream_gate_full",
            (
                StatusCode::TOO_MANY_REQUESTS,
                "rate_limit_error",
                "Gateway upstream concurrency gate is full (too many in-flight upstream calls). \
                 This is gateway-side backpressure, not an upstream cooldown; retrying immediately will not help.",
                None,
            ),
        );
        let retry_after = parse_retry_after_secs(&err_str).or(cfg_ra).unwrap_or(2).clamp(1, 300);
        tracing::warn!(
            retry_after_secs = retry_after,
            "上游并发闸已满（网关背压），返回 429 + Retry-After；不可吸收"
        );
        return (
            status,
            [(header::RETRY_AFTER, retry_after.to_string())],
            Json(ErrorResponse::new(error_type, message)),
        )
            .into_response();
    }

    // 全池冷却快速失败：token_manager 全池都在冷却时会带 retry_after_secs=N 快速 bail。
    // 这里透传成标准 429 + Retry-After 头，让客户端(Claude Code)按其自身退避策略重试——
    // 比网关内硬扛温和，也减少对被风控号的试探。
    //
    // 顺序承重：必须排在 translate_upstream_error / 通用 5xx 503 之前。
    // 本串只有 `retry_after_secs=`、没有 absorb/budget 标记；落到后面的 503
    // 会让 shield/客户端丢掉 429+号池真值（T5 夹具 `retry_after_secs=14`）。
    if let Some(secs) = parse_retry_after_secs(&err_str) {
        // ⭐ 号池真值永远优先：能进此分支必然带真值，配置的 retryAfterSecs 不可覆盖它
        // （号池算出的剩余秒数比任何常数/配置都准，见 A2 分支注释）。
        // 决议链与 A2/A3/A4 同构：真值 → 配置兜底 → 类别兜底。此前 `_cfg_ra` 完全
        // 不读，与 error_messages.rs 的「配置只是兜底」注释矛盾（配置了
        // rate_limited_pool.retryAfterSecs 的管理员会发现它静默无效）；现改为读
        // —— 分支守卫保证真值恒存在，配置在链上完整但实际不可达，行为不变。
        let (status, error_type, message, cfg_ra) = resolve_msg(
            &err_msgs,
            "rate_limited_pool",
            (
                StatusCode::TOO_MANY_REQUESTS,
                "rate_limit_error",
                "All credentials are temporarily cooling down. Please retry after the indicated delay.",
                None,
            ),
        );
        let retry_after = Some(secs)
            .or(cfg_ra)
            .unwrap_or(UPSTREAM_RATE_LIMIT_RETRY_AFTER_SECS)
            .clamp(1, 300);
        tracing::warn!(
            retry_after_secs = retry_after,
            "全池冷却，返回 429 + Retry-After 让客户端退避"
        );
        return (
            status,
            [(header::RETRY_AFTER, retry_after.to_string())],
            Json(ErrorResponse::new(error_type, message)),
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
        // ⚠️ 永久态：配置的 retry_afterSecs 刻意**忽略**（带了等于宣称「等一会儿会好」，
        // 诱导客户端每 5 分钟重试直到永远 —— 见下方注释的 404 语义）。
        let (status, error_type, message, _) = resolve_msg(
            &err_msgs,
            "model_unsupported",
            (
                StatusCode::NOT_FOUND,
                "not_found_error",
                "请求的模型不被当前号池支持（所有凭据的订阅档位或成本白名单均不含该模型）。这不是临时故障，重试无效：请换用号池支持的模型，或为凭据开通/放开该模型。",
                None,
            ),
        );
        tracing::warn!(error = %err, "请求的模型不被本号池支持（永久，重试无效），返回 404");
        return (
            status,
            Json(ErrorResponse::new(error_type, message)),
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
    //
    // 第二路判据 `upstream_retry_after=`（S2/S3）：provider 把上游显式 Retry-After 打进
    // 错误串的**网关自己的** marker。它只可能出现在两类串上 —— ① 上游 429（速率类，
    // 配额类被 provider 的 monthly-limit 分支先接走）；② S3 把最早 429 的 RA 并入的
    // generic 终态（5xx/传输层，见 provider 的 assemble_final_error，限定集已排除
    // 永久态/配额/背压）。故本路判据不破坏「只匹配速率类」的既有契约；配额字样的串
    // 即使出现 marker 也显式排除（双保险，防未来 provider 侧守卫被放宽后静默破坏 H2）。
    // ⭐ 锚定判定（Review3 m1）：裸 contains 会把 body 噪声误判成网关真值，判据改为
    // trim_end 后「marker 落在串尾且后跟纯数字」才命中（网关恒追加在末尾）。
    if is_upstream_rate_limited(&err_str)
        || (upstream_retry_after_anchored(&err_str)
            && !crate::kiro::endpoint::default_is_monthly_request_limit(&err_str))
    {
        let (status, error_type, message, cfg_ra) = resolve_msg(
            &err_msgs,
            "rate_limited_credential",
            (
                StatusCode::TOO_MANY_REQUESTS,
                "rate_limit_error",
                "上游账户级速率限流（请求过于密集）。这是可重试的临时状态，请按 Retry-After 退避后重试。若持续出现：①降低客户端并发；②为号池补充更多凭据分摊速率；③面板『限流健康』确认是否单号承载了全部流量。",
                None,
            ),
        );
        // Retry-After 决议链：上游显式真值（`upstream_retry_after=N`）> 配置 > 固定 8s。
        // ⭐ S2：上游明说「30s 后恢复」时客户端就该等 30s —— 此前 A7 恒 8s（上游 RA
        // 只进凭据冷却、不进客户端响应），客户端 8s 就重打，白打一轮又被冷却。
        // clamp 1-300：防上游给超大值（resets_at 到月底等）让客户端直接放弃重试。
        let retry_after = parse_upstream_retry_after(&err_str)
            .or(cfg_ra)
            .unwrap_or(UPSTREAM_RATE_LIMIT_RETRY_AFTER_SECS)
            .clamp(1, 300);
        tracing::warn!(
            error = %err,
            retry_after_secs = retry_after,
            "上游账户级速率限流，返回 429 + Retry-After 让客户端退避（旧代码此处返 502 致客户端不退避）"
        );
        return (
            status,
            [(header::RETRY_AFTER, retry_after.to_string())],
            Json(ErrorResponse::new(error_type, message)),
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
        let (status, error_type, message, cfg_ra) = resolve_msg(
            &err_msgs,
            "account_throttled",
            (
                StatusCode::TOO_MANY_REQUESTS,
                "rate_limit_error",
                "上游账户级临时风控（账号被暂时限制，非永久封禁）。这是可恢复的限时状态，请按 Retry-After 退避后重试。若持续出现：①降低并发与请求密度；②为号池补充更多凭据分摊风控压力；③面板『限流健康』查看是否单号承载了全部流量。",
                None,
            ),
        );
        let retry_after = cfg_ra
            .unwrap_or(UPSTREAM_SUSPENDED_RETRY_AFTER_SECS)
            .clamp(1, 300);
        tracing::warn!(
            error = %err,
            retry_after_secs = retry_after,
            "上游账户级临时风控（403 temporarily suspended），返回 429 + Retry-After（旧代码落 502 兜底致客户端不退避）"
        );
        return (
            status,
            [(header::RETRY_AFTER, retry_after.to_string())],
            Json(ErrorResponse::new(error_type, message)),
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
        // ⚠️ 永久配置错误态：配置的 retry_afterSecs 刻意**忽略**（重试不会改变 region
        // 错配，给了等于宣称「等一会儿会好」，把需要人工介入的配置错误变成无限退避重试）。
        // M2 语义错位修复（对抗审查）：region 错配用独立 key `region_mismatch`，
        // 不再占用 `permission_denied`——后者留给 D2 IP 黑名单（本地安全过滤）。
        let (status, error_type, message, _) = resolve_msg(
            &err_msgs,
            "region_mismatch",
            (
                StatusCode::FORBIDDEN,
                "permission_error",
                "上游拒绝该凭据的授权（bearer token 对目标 region 无效）。这不是服务端故障，重试无效：\
                 `ksk_` 类 token 按 region 授权，打错 region 恒被拒。排障：①面板查看该凭据的 region 是否与签发 region 一致；\
                 ②对该凭据手动改 region（或等网关 region 探测自动重选）；③若整池同区，确认推号来源给的 region 正确。",
                None,
            ),
        );
        tracing::warn!(
            error = %err,
            "上游 403 region 错配（bearer token invalid），返回 403 permission_error（旧代码落 502 兜底致上游/外挂按 5xx 盲退避）"
        );
        return (
            status,
            Json(ErrorResponse::new(error_type, message)),
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
        // 配置表允许的可重试分支（容量 503 默认 3s、配额类可选）挂 Retry-After 头；
        // None（永久态/请求错误分支）不挂，与现状一致。
        if let Some(ra) = t.retry_after_override {
            let ra = ra.clamp(1, 300);
            resp.headers_mut().insert(
                header::RETRY_AFTER,
                ra.to_string().parse().expect("u64 to_string 恒为合法 HeaderValue"),
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
        let (status, error_type, message, cfg_ra) = resolve_msg(
            &err_msgs,
            "upstream_5xx",
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "api_error",
                "上游服务暂时不可用（5xx 或连接失败），这是可重试的瞬态错误。\
                 请按 Retry-After 退避后重试；若持续出现，请查看网关日志。",
                None,
            ),
        );
        let retry_after = cfg_ra
            .unwrap_or(UPSTREAM_5XX_RETRY_AFTER_SECS)
            .clamp(1, 300);
        tracing::warn!(
            error = %err,
            retry_after_secs = retry_after,
            "上游 5xx/传输层瞬态错误，返回 503 + Retry-After（旧代码落 502 无 Retry-After 致客户端不退避）"
        );
        return (
            status,
            [(header::RETRY_AFTER, retry_after.to_string())],
            Json(ErrorResponse::new(error_type, message)),
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
    let (status, error_type, message, _) = resolve_msg(
        &err_msgs,
        "unrecognized_upstream",
        (
            StatusCode::BAD_GATEWAY,
            "api_error",
            "上游 API 调用失败（未识别错误）。请查看网关日志获取详情。",
            None,
        ),
    );
    (
        status,
        Json(ErrorResponse::new(error_type, message)),
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
            context_window: s.context_window,
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
                context_window: 1_000_000,
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
        // 错误消息配置表（key `gate_timeout`，与 map_provider_error 的
        // inbound_admission_timeout 分支同 key：同一错误形态的两个渲染点）。
        // 状态码/type/message 可配；Retry-After 用真值（配置值不适用——本分支的
        // 退避秒数由闸门队列超时算出，比配置任何常数都准）。
        let (status, error_type, message, _cfg_ra) = resolve_msg(
            &current_error_messages(),
            "gate_timeout",
            (
                StatusCode::TOO_MANY_REQUESTS,
                "rate_limit_error",
                "Gateway inbound rate shaping is at capacity. \
                 This is gateway-side backpressure, not an upstream cooldown; \
                 retrying immediately will not help.",
                None,
            ),
        );
        tracing::warn!(retry_after_secs = ra, "入站准入排队超时（网关背压），返回 429 + Retry-After");
        return Some((
            status,
            [(header::RETRY_AFTER, ra.to_string())],
            Json(ErrorResponse::new(error_type, message)),
        ).into_response());
    }
    None
}

/// Bind model/stream onto the request-level tracing span (no-op without a span).
fn record_request_span_model_stream(model: &str, stream: bool) {
    let span = tracing::Span::current();
    span.record("model", model);
    span.record("stream", stream);
}

/// Bind the selected credential onto the request-level tracing span (no-op without a span).
fn record_request_span_credential_id(id: u64) {
    tracing::Span::current().record("credential_id", id);
}

/// POST /v1/messages
///
/// 创建消息（对话）
#[tracing::instrument(
    skip_all,
    fields(
        model = tracing::field::Empty,
        stream = tracing::field::Empty,
        credential_id = tracing::field::Empty,
    )
)]
pub async fn post_messages(
    State(state): State<AppState>,
    axum::extract::ConnectInfo(peer): axum::extract::ConnectInfo<std::net::SocketAddr>,
    headers: axum::http::HeaderMap,
    // 取**裸 body 字节**(而非 JsonExtractor):自定义 API 代挂需要原样透传原始请求体。
    // Kiro 路径行为不变——下面立即从同一份字节解析出 MessagesRequest,与旧 JsonExtractor 等价。
    raw_body: Bytes,
) -> Response {
    // 先按原逻辑解析请求体(解析失败=400,与旧 JsonExtractor 的行为对齐)。
    let payload: MessagesRequest = match serde_json::from_slice(&raw_body) {
        Ok(p) => p,
        Err(e) => {
            // 配置 message 只覆盖「前缀」，动态详情（{e}）恒保留（排障必需）。
            let (status, error_type, message, _) = resolve_msg(
                &current_error_messages(),
                "request_parse_failed",
                (
                    StatusCode::BAD_REQUEST,
                    "invalid_request_error",
                    "请求体解析失败",
                    None,
                ),
            );
            return (
                status,
                Json(ErrorResponse::new(error_type, format!("{message}: {e}"))),
            )
                .into_response();
        }
    };
    record_request_span_model_stream(&payload.model, payload.stream);
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
    // 检查 KiroProvider 是否可用（未配置渲染双入口共享，key `provider_not_configured`）。
    let provider = match &state.kiro_provider {
        Some(p) => p.clone(),
        None => return render_provider_not_configured(),
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

    // 🔴 max_tokens 本地上限校验（2026-08-15 线上 smoke test 发现）：超出上游上限的
    // max_tokens 此前被误判为瞬态错误吞进 failover+absorb（30s 延迟 + 503 误判，
    // 客户端等预算耗尽）。校验放在**透传尝试之前**。上限对齐上游实测
    // （fuckopencode/deepseek 均 393216）。双入口（/v1 与 /cc/v1）同检查。
    if let Some(resp) = check_max_tokens_limit(payload.max_tokens) {
        return resp;
    }

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
        record_request_span_credential_id(meta.credential_id);
        // 首选号（N4）：透传 failover 链最先尝试的号（None = 首跳即成功/无换号）。
        // 与 credential_id（最终服务号）成对后，面板能看出「死号恒选」——首选号恒为
        // 某号而最终号恒为另一号时，说明该号每次都被换掉，需要处理其上游。
        record.first_attempted_credential_id = meta.first_attempted_credential_id;
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

    let prep = match prepare_kiro_dispatch(payload, &provider, &retry_budget, &client).await {
        Ok(p) => p,
        Err(resp) => return resp,
    };
    let KiroDispatchPrep {
        payload,
        request_body,
        mut conv_state_for_compress_retry,
        native_fields_for_compress_retry,
        input_tokens,
        cache_breakdown,
        fingerprint_usage,
        thinking_enabled,
        tool_name_map,
        known_tool_names,
        tool_required_fields,
    } = prep;

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
            response_body = match rebuild_body_for_compress_retry(
                &mut conv_state_for_compress_retry,
                &native_fields_for_compress_retry,
                &compression_cfg,
                compress_attempt,
            ) {
                Ok(b) => b,
                Err(e) => {
                    tracing::error!("压缩重试时序列化请求失败: {}", e);
                    // 渲染双入口共享（request_serialization_failed：压缩重试轮同配置）。
                    return render_serialization_failed(&e);
                }
            };
            &response_body
        };

        let response = if payload.stream {
            if cc_auto_buffer_enabled() && is_claude_code_request(&headers) {
                tracing::debug!("识别到 Claude Code 请求，/v1 流式自动切换为 buffered 分发");
                dispatch_kiro_attempt(
                    provider.clone(),
                    body_ref,
                    &payload,
                    input_tokens,
                    thinking_enabled,
                    tool_name_map.clone(),
                    known_tool_names.clone(),
                    tool_required_fields.clone(),
                    cache_breakdown.clone(),
                    fingerprint_usage,
                    &retry_budget,
                    client.clone(),
                    true,
                )
                .await
            } else {
                dispatch_kiro_attempt(
                    provider.clone(),
                    body_ref,
                    &payload,
                    input_tokens,
                    thinking_enabled,
                    tool_name_map.clone(),
                    known_tool_names.clone(),
                    tool_required_fields.clone(),
                    cache_breakdown.clone(),
                    fingerprint_usage,
                    &retry_budget,
                    client.clone(),
                    false,
                )
                .await
            }
        } else {
            dispatch_kiro_attempt(
                provider.clone(),
                body_ref,
                &payload,
                input_tokens,
                thinking_enabled,
                tool_name_map.clone(),
                known_tool_names.clone(),
                tool_required_fields.clone(),
                cache_breakdown.clone(),
                fingerprint_usage,
                &retry_budget,
                client.clone(),
                false,
            )
            .await
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
    let (response, meta) = match provider.call_api_stream(request_body, is_1m, budget, Some(model)).await {
        Ok(resp) => resp,
        Err(e) => return map_provider_error(e),
    };
    record_request_span_credential_id(meta.credential_id);

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
    let mut builder = sse_event_stream_builder();
    if cache_estimated {
        builder = builder.header(CACHE_ESTIMATED_HEADER, CACHE_ESTIMATED_VALUE);
    }
    builder.body(Body::from_stream(stream)).unwrap()
}

/// 流结束时，用 provider 元数据 + StreamContext 最终 usage 埋点一条记录
fn emit_stream_usage(
    provider: &crate::kiro::provider::KiroProvider,
    ctx: &StreamContext,
    meta: &crate::kiro::provider::CallMeta,
    client: &ClientInfo,
    disconnected: bool,
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
    apply_usage_outcome(
        &mut record,
        ctx.completion().is_ok(),
        ctx.completion_outcome(),
        (!ctx.completion().is_ok()).then(|| ctx.completion().client_message()),
        ctx.is_empty_response(),
        ctx.empty_response_is_oversized_context(),
        disconnected,
    );
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

/// 空响应的错误形态（类型 + 文案 + 可选 Retry-After），流式 SSE error 与非流式 HTTP 错误共用。
///
/// - 大输入（疑似上下文过大）：`invalid_request_error`，提示压缩上下文，
///   不鼓励原样重试（重试还是同样的大请求，仍会空）。不产出 Retry-After
///   （400 请求错误带退避秒数会让客户端反复重发同一个必败的大请求）。
/// - 小输入（疑似偶发）：`overloaded_error`，客户端可重试。**D10 矛盾修复**（设计 §五 2）：
///   现状 429 不带 Retry-After（客户端只能瞎重试），补默认 3s；配置的
///   `retryAfterSecs`（key `empty_response`）可覆盖。
///
/// M3 共享 key 拆分（对抗审查）：D9 与 D10 是两个**不同语义**的 key——
/// D9（大输入 400，重试无效）用 `empty_response_large_input`，D10（小输入 429，
/// 可重试）用 `empty_response`。status 恒按判据 400/429（`oversized_context`
/// 判据承重，**不读配置**——一个 key 无法同时表达两个形态的 status，
/// 配置校验层同样约束）；type/message 各自可配。
/// D10 的 Retry-After 由调用方据第三元决定是否挂头。
/// D10 空响应 429 的默认退避秒数（设计 §五 2 修复：现状无 Retry-After）。
/// 取值与 A11 的 5xx 退避（3s）同档：「稍后重试」级瞬态。
const EMPTY_RESPONSE_RETRY_AFTER_SECS: u64 = 3;
fn empty_response_error_shape(oversized_context: bool) -> (String, String, Option<u64>) {
    let (err_type, message, cfg_ra) = if oversized_context {
        // D9：大输入（疑似上下文过大），重试原请求无意义 → 永不带 Retry-After
        // （配置的 retryAfterSecs 刻意忽略，与 B9/B10 永久态同策略）。
        let (_, t, m, _) = resolve_msg(
            &current_error_messages(),
            "empty_response_large_input",
            (
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "上游返回了空响应，疑似上下文已接近窗口上限。请精简对话历史（如 /compact）、\
                 缩短 system prompt 或减少工具数量后重试。",
                None,
            ),
        );
        (t, m, None)
    } else {
        // D10：小输入（疑似偶发），可重试 → 配置 RA 或默认 3s。
        let (_, t, m, cfg_ra) = resolve_msg(
            &current_error_messages(),
            "empty_response",
            (
                StatusCode::TOO_MANY_REQUESTS,
                "overloaded_error",
                "上游返回了空响应，请重试。",
                Some(EMPTY_RESPONSE_RETRY_AFTER_SECS),
            ),
        );
        let ra = cfg_ra.map(|ra| ra.clamp(1, 300));
        (t, m, ra)
    };
    (err_type, message, cfg_ra)
}

/// 为上游空响应构造合适的 SSE error 事件。
///
/// - 大输入（疑似上下文过大）：返回 invalid_request_error，提示压缩上下文，
///   不鼓励原样重试（重试还是同样的大请求，仍会空）。
/// - 小输入（疑似偶发）：返回 overloaded_error，客户端可重试。
fn empty_response_error_event(oversized_context: bool) -> SseEvent {
    let (err_type, message, _retry_after) = empty_response_error_shape(oversized_context);
    SseEvent::new(
        "error",
        serde_json::json!({
            "type": "error",
            "error": { "type": err_type, "message": message }
        }),
    )
}

/// 客户端在流完成前断开时写入 `error_message` 的固定文案。
const CLIENT_DISCONNECTED_MESSAGE: &str = "client disconnected";

/// 把 completion / 空响应 / 客户端断连收敛成最终 usage outcome。
///
/// `disconnected` 优先：Drop 兜底时即使 ctx 仍是 Ok（甚至已判空响应）也记 Interrupted。
/// 空响应只在 completion Ok 时覆盖为 EmptyResponse，不改 completion 层，故不进
/// report_failure / 熔断 / absorb。
fn apply_usage_outcome(
    record: &mut crate::usage::RequestRecord,
    completion_ok: bool,
    completion_outcome: crate::usage::RequestOutcome,
    completion_error: Option<String>,
    is_empty_response: bool,
    oversized_context: bool,
    disconnected: bool,
) {
    if disconnected {
        record.outcome = crate::usage::RequestOutcome::Interrupted;
        record.error_message = Some(CLIENT_DISCONNECTED_MESSAGE.to_string());
        return;
    }
    record.outcome = completion_outcome;
    if !completion_ok {
        record.error_message = completion_error;
        return;
    }
    if is_empty_response {
        record.outcome = crate::usage::RequestOutcome::EmptyResponse;
        let (_ty, message, _ra) = empty_response_error_shape(oversized_context);
        record.error_message = Some(message);
    }
}

/// 流式 usage 发射守卫：unfold 被客户端掐掉时 Drop 按 Interrupted 补记。
///
/// `emitted` 在自然结束 / 上游断流的 emit 点置位；Drop 仅在未 emit 时走
/// `disconnected=true`。catch_unwind 保证 Drop 不 panic。
struct UsageEmitGuard<C> {
    ctx: C,
    meta: crate::kiro::provider::CallMeta,
    client: ClientInfo,
    provider: std::sync::Arc<crate::kiro::provider::KiroProvider>,
    emitted: bool,
    emit_fn: fn(
        &crate::kiro::provider::KiroProvider,
        &C,
        &crate::kiro::provider::CallMeta,
        &ClientInfo,
        bool,
    ),
}

impl<C> UsageEmitGuard<C> {
    fn new(
        ctx: C,
        meta: crate::kiro::provider::CallMeta,
        client: ClientInfo,
        provider: std::sync::Arc<crate::kiro::provider::KiroProvider>,
        emit_fn: fn(
            &crate::kiro::provider::KiroProvider,
            &C,
            &crate::kiro::provider::CallMeta,
            &ClientInfo,
            bool,
        ),
    ) -> Self {
        Self {
            ctx,
            meta,
            client,
            provider,
            emitted: false,
            emit_fn,
        }
    }

    fn emit(&mut self) {
        if self.emitted {
            return;
        }
        (self.emit_fn)(
            &self.provider,
            &self.ctx,
            &self.meta,
            &self.client,
            false,
        );
        self.emitted = true;
    }
}

impl<C> Drop for UsageEmitGuard<C> {
    fn drop(&mut self) {
        if self.emitted {
            return;
        }
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            (self.emit_fn)(
                &self.provider,
                &self.ctx,
                &self.meta,
                &self.client,
                true,
            );
        }));
        if result.is_err() {
            tracing::error!("stream usage Drop 补记 panic（已隔离）");
        }
        self.emitted = true;
    }
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
    let guard = UsageEmitGuard::new(ctx, meta, client, provider, emit_stream_usage);

    let processing_stream = stream::unfold(
        (body_stream, EventStreamDecoder::new(), false, interval(Duration::from_secs(PING_INTERVAL_SECS)), guard),
        |(mut body_stream, mut decoder, finished, mut ping_interval, mut guard)| async move {
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
                            guard.ctx.note_received_bytes(chunk.len());
                            let mut events =
                                decode_frames_into(&mut decoder, &chunk, &mut guard.ctx);
                            // 解码器连续错误超限：helper 已 mark_decoder_stopped；流式路径
                            // 还要内联补发 SSE error（buffered 等到流结束再发）。
                            if decoder.is_stopped() && !guard.ctx.error_event_emitted() {
                                events.push(SseEvent::error_event(
                                    guard.ctx.completion().sse_error_type(),
                                    guard.ctx.completion().client_message(),
                                ));
                                guard.ctx.mark_error_event_emitted();
                            }

                            // 转换为 SSE 字节流
                            let bytes: Vec<Result<Bytes, Infallible>> = events
                                .into_iter()
                                .map(|e| Ok(Bytes::from(e.to_sse_string())))
                                .collect();

                            Some((stream::iter(bytes), (body_stream, decoder, false, ping_interval, guard)))
                        }
                        Some(Err(e)) => {
                            tracing::error!("读取响应流失败: {}", e);
                            // 上游流中途失败：置传输失败态（供收尾按 NetworkError 记账），
                            // 先发一个 SSE error 事件显式告知客户端"本次未正常完成"，再补最终事件收尾。
                            // 否则 Claude Code 会把截断输出当作正常 message_stop=成功，不重试。
                            // 幂等：若 in-band 错误已置过失败态，mark_transport_error 会保留首因。
                            guard.ctx.mark_transport_error(e.to_string());
                            let mut events = Vec::new();
                            if !guard.ctx.error_event_emitted() {
                                events.push(SseEvent::error_event(
                                    guard.ctx.completion().sse_error_type(),
                                    guard.ctx.completion().client_message(),
                                ));
                                guard.ctx.mark_error_event_emitted();
                            }
                            events.extend(guard.ctx.generate_final_events());
                            let bytes: Vec<Result<Bytes, Infallible>> = events
                                .into_iter()
                                .map(|e| Ok(Bytes::from(e.to_sse_string())))
                                .collect();
                            guard.emit();
                            Some((stream::iter(bytes), (body_stream, decoder, true, ping_interval, guard)))
                        }
                        None => {
                            // 流结束，发送最终事件。
                            // 【缺陷1 时序修复】必须**先** generate_final_events(它内部会 flush 未收到 stop 的
                            // 残留 tool 缓冲,那步才可能把 completion 置失败态),**再**据 completion 补发 error。
                            // 旧序(先查 completion 再 flush)在"无 stop 残留截断"场景漏发 error → 客户端拿
                            // input:{} 的 tool 块 + 正常 message_stop 误判成功(服务端却记失败)。默认②③开也中。
                            // 现在残留 flush 的 ③ 逻辑在置失败态时已返回空(不发坏 JSON),故 final 里无坏 delta,
                            // 把 error 事件**插到最前**(在收尾 message_delta/message_stop 之前)符合 SSE 语义。
                            let tail = guard.ctx.generate_final_events();
                            let mut final_events = Vec::new();
                            if !guard.ctx.completion().is_ok() && !guard.ctx.error_event_emitted() {
                                final_events.push(SseEvent::error_event(
                                    guard.ctx.completion().sse_error_type(),
                                    guard.ctx.completion().client_message(),
                                ));
                                guard.ctx.mark_error_event_emitted();
                            }
                            // 空响应检测：正常完成但收尾兜底后模型仍什么都没产出（或上下文压力下的
                            // 退化短响应）时，用显式 error 事件替代空 end_turn，避免客户端 agentic
                            // 循环卡住。上下文过大 → invalid_request_error 提示 /compact；偶发 → 可重试。
                            if guard.ctx.completion().is_ok()
                                && !guard.ctx.error_event_emitted()
                                && guard.ctx.is_empty_response()
                            {
                                let oversized = guard.ctx.empty_response_is_oversized_context();
                                tracing::warn!(
                                    oversized_context = oversized,
                                    "上游返回空响应（收尾兜底后仍无内容），补发 error 事件替代空 end_turn"
                                );
                                final_events.push(empty_response_error_event(oversized));
                                guard.ctx.mark_error_event_emitted();
                            } else {
                                final_events.extend(tail);
                            }
                            let bytes: Vec<Result<Bytes, Infallible>> = final_events
                                .into_iter()
                                .map(|e| Ok(Bytes::from(e.to_sse_string())))
                                .collect();
                            guard.emit();
                            Some((stream::iter(bytes), (body_stream, decoder, true, ping_interval, guard)))
                        }
                    }
                }
                // 发送 ping 保活
                _ = ping_interval.tick() => {
                    tracing::trace!("发送 ping 保活事件");
                    let bytes: Vec<Result<Bytes, Infallible>> = vec![Ok(create_ping_sse())];
                    Some((stream::iter(bytes), (body_stream, decoder, false, ping_interval, guard)))
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

/// 告知客户端：本响应 `usage.input_tokens` / `cache_*` 已按 `CLIENT_TOKEN_DISPLAY_SCALE` 缩放。
/// HTTP/2 要求小写；值见 [`CLIENT_TOKEN_DISPLAY_SCALE_HEADER`]。
pub(crate) const INPUT_TOKEN_SCALE_HEADER: &str = "x-kirostudio-input-token-scale";

/// [`CACHE_ESTIMATED_VALUE`] 的 `HeaderValue` 形态（`headers_mut().insert` 需要它，
/// 而 `Response::builder().header` 接受 `&str`，故两种形态都留着）。
fn cache_estimated_header_value() -> axum::http::HeaderValue {
    axum::http::HeaderValue::from_static(CACHE_ESTIMATED_VALUE)
}

/// 三条 `text/event-stream` 路径共用：websearch 回灌 / 直播 / buffered（线上默认）。
fn sse_event_stream_builder() -> axum::http::response::Builder {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONNECTION, "keep-alive")
        .header(INPUT_TOKEN_SCALE_HEADER, CLIENT_TOKEN_DISPLAY_SCALE_HEADER)
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

/// 非流式收尾的 content 是否「只有 thinking 块、无 text/tool_use 等正文内容」。
///
/// 对应流式 `SseStateManager::has_non_thinking_blocks() == false` 的口径：内容
/// 数组里除 thinking 外没有任何块。任何其它类型（text / tool_use / server_tool_use /
/// web_search_tool_result / 未知类型）都视为「有正文内容」。
fn content_is_thinking_only(content: &[serde_json::Value]) -> bool {
    let mut has_thinking = false;
    for block in content {
        match block.get("type").and_then(|v| v.as_str()) {
            Some("thinking") => has_thinking = true,
            _ => return false,
        }
    }
    has_thinking
}

/// 非流式 content 是否含用户可见正文（非空 text / tool_use）。
/// thinking-only 补的空格 text 经 trim 后为空，不算可见（该路径已显式 max_tokens）。
fn nonstream_content_has_visible_body(content: &[serde_json::Value]) -> bool {
    content.iter().any(|block| match block.get("type").and_then(|v| v.as_str()) {
        Some("text") => block
            .get("text")
            .and_then(|v| v.as_str())
            .is_some_and(|s| !s.trim().is_empty()),
        Some("tool_use") => true,
        _ => false,
    })
}

/// 与流式 `is_clean_eof_without_terminal` 同口径：传输干净、有部分产出、无任何终止信号。
fn nonstream_clean_eof_without_terminal(
    completion_ok: bool,
    explicit_stop: Option<&str>,
    saw_upstream_stop: bool,
    saw_tool_stop: bool,
    has_visible_body: bool,
    has_tool_use: bool,
) -> bool {
    if !completion_ok {
        return false;
    }
    if explicit_stop.is_some() || saw_upstream_stop || saw_tool_stop {
        return false;
    }
    has_visible_body || has_tool_use
}

/// 与流式 `SseStateManager::get_stop_reason` 同口径。
fn resolve_nonstream_stop_reason(explicit: Option<String>, has_tool_use: bool) -> String {
    match explicit {
        Some(reason) => reason,
        None if has_tool_use => "tool_use".to_string(),
        None => "end_turn".to_string(),
    }
}

/// 非流式失败收尾：埋点真实 outcome + 非 200。in-band / 解码停止 / 不完整 EOF 共用。
fn emit_nonstream_completion_failure(
    completion: &CompletionStatus,
    meta: &crate::kiro::provider::CallMeta,
    model: &str,
    context_input_tokens: Option<i32>,
    input_tokens: i32,
    credits_used: Option<f64>,
    provider: &crate::kiro::provider::KiroProvider,
    client: &ClientInfo,
) -> Response {
    let mut record = crate::usage::RequestRecord::new(
        Uuid::new_v4().to_string(),
        meta.model.clone().unwrap_or_else(|| model.to_string()),
    );
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
    record.error_message = Some(completion.client_message());
    if let Some(c) = record.credits_used {
        provider.report_credits(meta.credential_id, c);
    }
    client.apply(&mut record);
    crate::usage::emit_record(record);
    let status =
        StatusCode::from_u16(completion.http_status_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    (
        status,
        Json(ErrorResponse::new(
            completion.sse_error_type(),
            completion.client_message(),
        )),
    )
        .into_response()
}

/// 处理非流式请求
async fn handle_non_stream_request(
    provider: std::sync::Arc<crate::kiro::provider::KiroProvider>,
    request_body: &str,
    model: &str,
    input_tokens: i32,
    thinking_enabled: bool,
    tool_name_map: std::collections::HashMap<String, String>,
    tool_required_fields: std::collections::HashMap<String, Vec<String>>,
    cache_breakdown: Option<CacheUsageBreakdown>,
    fingerprint_usage: Option<super::cache::PromptCacheUsage>,
    budget: &crate::kiro::provider::SharedRetryBudget,
    client: ClientInfo,
) -> Response {
    // 1M 变体:据原始模型名判定是否注入 anthropic-beta 头(仅受支持的 [1m] 变体为 true)。
    let is_1m = crate::anthropic::model_catalog::resolve_is_1m(model);
    // 调用 Kiro API（支持多凭据故障转移）
    let (response, meta) = match provider.call_api(request_body, is_1m, budget, Some(model)).await {
        Ok(resp) => resp,
        Err(e) => return map_provider_error(e),
    };
    record_request_span_credential_id(meta.credential_id);

    // 读取响应体
    let body_bytes: bytes::Bytes = match response.bytes().await {
        Ok(bytes) => bytes,
        Err(e) => {
            tracing::error!("读取响应体失败: {}", e);
            let (status, error_type, message, _) = resolve_msg(
                &current_error_messages(),
                "response_read_failed",
                (
                    StatusCode::BAD_GATEWAY,
                    "api_error",
                    "读取响应失败",
                    None,
                ),
            );
            return (
                status,
                Json(ErrorResponse::new(error_type, format!("{message}: {e}"))),
            )
                .into_response();
        }
    };

    // 解析事件流
    let mut text_content = String::new();
    // E1：上游结构化 thinking 流（reasoningContentEvent）的累积。与正文分开攒 ——
    // 混进 text_content 会让它被当成用户可见回答，而且下面的标签提取还会再解析一遍。
    let mut reasoning_content = String::new();
    // 上游 reasoningContentEvent 携带的思考签名（若有）。下发 thinking 块时优先回传 ——
    // Foxfishc 实测「伪造签名不被识别，cache_read 仍 0」；缺则回退占位符（行为与旧版一致）。
    let mut reasoning_signature: Option<String> = None;
    let mut tool_uses: Vec<serde_json::Value> = Vec::new();
    let mut has_tool_use = false;
    // 不再默认 end_turn：缺终止信号时由 resolve / 不完整 EOF 判定，避免把截断当成功。
    let mut stop_reason: Option<String> = None;
    let mut saw_upstream_stop = false;
    let mut saw_tool_stop = false;
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

    let mut decoded_events = Vec::new();
    {
        let mut decoder = EventStreamDecoder::new();
        let mut sink = NonStreamDecodeSink {
            events: &mut decoded_events,
            completion: &mut completion,
        };
        let _ = decode_frames_into(&mut decoder, &body_bytes, &mut sink);
    }
    for event in decoded_events {
        match event {
                        Event::AssistantResponse(resp) => {
                            text_content.push_str(&resp.content);
                        }
                        Event::ToolUse(tool_use) => {
                            has_tool_use = true;
                            if tool_use.stop {
                                saw_tool_stop = true;
                            }

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

                                // Bug C：在出站还原之前对 **Kiro 形态** 判缺（与流式 stop 同口径）。
                                // 旧路径只修 JSON 非法，缺 required 仍 200 下发空/残参。
                                if tool_stream_align_failure_enabled()
                                    && completion.is_ok()
                                    && !tool_required_fields.is_empty()
                                {
                                    if let Some(required) =
                                        tool_required_fields.get(&tool_use.name)
                                    {
                                        if let Some(missing) =
                                            super::stream::missing_required_keys(&input, required)
                                        {
                                            tracing::warn!(
                                                missing = %missing.join(","),
                                                "非流式 tool_use 缺必需字段（Bug C）：置失败态，不下发残参"
                                            );
                                            completion = CompletionStatus::UpstreamError {
                                                code: "INVALID_TOOL_INPUT".to_string(),
                                                message: format!(
                                                    "工具调用缺少必需参数：{}（模型侧生成异常），请重试。",
                                                    missing.join("、")
                                                ),
                                            };
                                            continue;
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
                                stop_reason = Some("model_context_window_exceeded".to_string());
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
                                stop_reason = Some("max_tokens".to_string());
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
                        Event::Metadata(meta) => {
                            // 与流式 process 路径同源：复用 map_metadata_stop_reason，禁止第二张表。
                            if let Some(mapped) =
                                crate::kiro::model::events::map_metadata_stop_reason(
                                    meta.stop_reason.as_deref(),
                                )
                            {
                                saw_upstream_stop = true;
                                // 已有 tool_use 时客户端 stop_reason 仍走 tool_use；本帧只标记流完整。
                                if !has_tool_use {
                                    stop_reason = Some(mapped);
                                }
                            }
                        }
                        _ => {}
        }
    }

    // 完成状态为失败：直接返回非 200 错误响应 + 埋点真实 outcome，绝不把截断输出当 200 成功。
    // （ContentLengthExceededException 走的是 max_tokens，completion 仍为 Ok，不进此分支。）
    if !completion.is_ok() {
        return emit_nonstream_completion_failure(
            &completion,
            &meta,
            model,
            context_input_tokens,
            input_tokens,
            credits_used,
            &provider,
            &client,
        );
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

    // 与流式 generate_final_events 的 thinking-only 兜底（stream.rs:3926）对齐：
    // thinking 开启、content 只有 thinking 块（无 text/tool_use）时，模型把预算
    // 全花在思考上 —— 补一个空格 text 块并置 stop_reason=max_tokens，否则客户端
    // 拿到「只有 thinking 无正文」的响应，Claude Code 会视作空回答。
    if thinking_enabled && content_is_thinking_only(&content) {
        stop_reason = Some("max_tokens".to_string());
        content.push(json!({"type": "text", "text": " "}));
    }

    // 干净 EOF：有部分正文/未 stop 的 tool，却没有任何终止信号。不得 200 + end_turn。
    // 空响应当下方 near_empty_response 处理（completion 保持 Ok）。
    // thinking-only 已在上面显式置 max_tokens，不会进本分支。
    if nonstream_clean_eof_without_terminal(
        completion.is_ok(),
        stop_reason.as_deref(),
        saw_upstream_stop,
        saw_tool_stop,
        nonstream_content_has_visible_body(&content),
        has_tool_use,
    ) {
        completion = CompletionStatus::Incomplete {
            message: "传输层干净结束，但缺少 stopReason 或 tool_use 终止信号".to_string(),
        };
        return emit_nonstream_completion_failure(
            &completion,
            &meta,
            model,
            context_input_tokens,
            input_tokens,
            credits_used,
            &provider,
            &client,
        );
    }

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

    // 空响应判据先算：埋点要写成 EmptyResponse，HTTP 再返回 400/429。
    let empty_resp = super::stream::near_empty_response(
        output_tokens,
        has_tool_use,
        final_input_tokens,
        model,
    );
    let empty_oversized = final_input_tokens
        >= super::stream::empty_response_oversized_threshold(&model);

    // 用量埋点：非流式成功或空响应（completion 保持 Ok，不进 report_failure）
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
        apply_usage_outcome(
            &mut record,
            completion.is_ok(),
            completion.outcome(),
            (!completion.is_ok()).then(|| completion.client_message()),
            empty_resp,
            empty_oversized,
            false,
        );
        // 生命周期累计花费：本次真实 credit 消耗累加到该凭据（独立于用量保留期，只增不清）。
        if let Some(c) = record.credits_used {
            provider.report_credits(meta.credential_id, c);
        }
        client.apply(&mut record);
        crate::usage::emit_record(record);
    }

    // 非流式空响应兜底（与流式 create_sse_stream / buffered 路径的 is_empty_response
    // 补发 error 同构）：正常完成但收尾兜底后 content 仍空（thinking/text/tool_use 全无）
    // 时，返回显式错误而非 200 `content: []` —— 客户端把空 content 当 end_turn 正常
    // 结束继续对话，agentic 循环会反复卡住。大输入 → 400 invalid_request_error
    // 提示 /compact（重试还是同样的大请求）；偶发 → 429 overloaded_error 可重试。
    // 判据与流式共用同一个函数（near_empty_response）：近空（output_tokens < 30 且
    // 无工具调用）且输入超过「上下文过大」阈值时同样返回 400 —— 大上下文 + 上游只回
    // 几个 token 的情形此前漏判（只判 content 完全空），200 当成功。阈值常量也共用
    // empty_response_oversized_threshold，两条路径分界一致。
    // 埋点已在上面完成：空响应记 EmptyResponse + error_message（completion 仍 Ok）。
    if empty_resp {
        let oversized = empty_oversized;
        let (err_type, message, retry_after) = empty_response_error_shape(oversized);
        let status = if oversized {
            StatusCode::BAD_REQUEST
        } else {
            StatusCode::TOO_MANY_REQUESTS
        };
        tracing::warn!(
            oversized_context = oversized,
            "上游返回空/近空响应（非流式收尾兜底后 content 仍空或仅数 token），返回显式错误替代 200 content:[]"
        );
        let mut resp = (status, Json(ErrorResponse::new(err_type, message))).into_response();
        // D10（偶发小输入 429）补 Retry-After（默认 3s / 配置覆盖）；D9（大输入 400）不带。
        if let Some(ra) = retry_after {
            resp.headers_mut().insert(
                header::RETRY_AFTER,
                ra.to_string().parse().expect("u64 to_string 恒为合法 HeaderValue"),
            );
        }
        return resp;
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
        "stop_reason": resolve_nonstream_stop_reason(stop_reason, has_tool_use),
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
#[tracing::instrument(
    skip_all,
    fields(
        model = tracing::field::Empty,
        stream = tracing::field::Empty,
        credential_id = tracing::field::Empty,
    )
)]
pub async fn post_messages_cc(
    State(state): State<AppState>,
    axum::extract::ConnectInfo(peer): axum::extract::ConnectInfo<std::net::SocketAddr>,
    headers: axum::http::HeaderMap,
    JsonExtractor(payload): JsonExtractor<MessagesRequest>,
) -> Response {
    record_request_span_model_stream(&payload.model, payload.stream);
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

    // 检查 KiroProvider 是否可用（未配置渲染双入口共享，key `provider_not_configured`）。
    let provider = match &state.kiro_provider {
        Some(p) => p.clone(),
        None => return render_provider_not_configured(),
    };

    // 入站整形准入闸门（与 /v1 同闸）：2026-08-11 补。
    // 改前闸门在 provider.call_api_with_retry 内部，/cc/v1 的 Kiro 路径也过闸；
    // 移到 handler 层后曾漏掉这条入口，这里补回（websearch 与 Kiro 路径统一过闸）。
    if let Some(resp) = try_inbound_admission_gate(&provider, &payload.model, payload.stream, &client).await {
        return resp;
    }

    // 每客户端请求的共享上游预算（与 /v1 同款，2026-08-11 方案 A）。
    let retry_budget = crate::kiro::provider::SharedRetryBudget::new();

    // 🔴 max_tokens 本地上限校验（2026-08-15 线上 smoke test 发现）：超出上游上限的
    // max_tokens 此前被误判为瞬态错误吞进 failover+absorb（30s 延迟 + 503 误判，
    // 客户端等预算耗尽）。校验放在**透传尝试之前**。上限对齐上游实测
    // （fuckopencode/deepseek 均 393216）。双入口（/v1 与 /cc/v1）同检查。
    if let Some(resp) = check_max_tokens_limit(payload.max_tokens) {
        return resp;
    }

    let prep = match prepare_kiro_dispatch(payload, &provider, &retry_budget, &client).await {
        Ok(p) => p,
        Err(resp) => return resp,
    };
    let KiroDispatchPrep {
        payload,
        request_body,
        mut conv_state_for_compress_retry,
        native_fields_for_compress_retry,
        input_tokens,
        cache_breakdown,
        fingerprint_usage,
        thinking_enabled,
        tool_name_map,
        known_tool_names,
        tool_required_fields,
    } = prep;

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
            response_body = match rebuild_body_for_compress_retry(
                &mut conv_state_for_compress_retry,
                &native_fields_for_compress_retry,
                &compression_cfg,
                compress_attempt,
            ) {
                Ok(b) => b,
                Err(e) => {
                    tracing::error!("压缩重试时序列化请求失败: {}", e);
                    // 渲染双入口共享（request_serialization_failed：压缩重试轮同配置）。
                    return render_serialization_failed(&e);
                }
            };
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
                dispatch_kiro_attempt(
                    provider.clone(),
                    body_ref,
                    &payload,
                    input_tokens,
                    thinking_enabled,
                    tool_name_map.clone(),
                    known_tool_names.clone(),
                    tool_required_fields.clone(),
                    cache_breakdown.clone(),
                    fingerprint_usage,
                    &retry_budget,
                    client.clone(),
                    true,
                )
                .await
            } else {
                tracing::debug!("/cc/v1 流式分发: 真流式（ccAutoBuffer=false，内容边到边转发）");
                dispatch_kiro_attempt(
                    provider.clone(),
                    body_ref,
                    &payload,
                    input_tokens,
                    thinking_enabled,
                    tool_name_map.clone(),
                    known_tool_names.clone(),
                    tool_required_fields.clone(),
                    cache_breakdown.clone(),
                    fingerprint_usage,
                    &retry_budget,
                    client.clone(),
                    false,
                )
                .await
            }
        } else {
            dispatch_kiro_attempt(
                provider.clone(),
                body_ref,
                &payload,
                input_tokens,
                thinking_enabled,
                tool_name_map.clone(),
                known_tool_names.clone(),
                tool_required_fields.clone(),
                cache_breakdown.clone(),
                fingerprint_usage,
                &retry_budget,
                client.clone(),
                false,
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
    let (response, meta) = match provider.call_api_stream(request_body, is_1m, budget, Some(model)).await {
        Ok(resp) => resp,
        Err(e) => return map_provider_error(e),
    };
    record_request_span_credential_id(meta.credential_id);

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
    let mut builder = sse_event_stream_builder();
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
    let guard = UsageEmitGuard::new(ctx, meta, client, provider, emit_buffered_usage);

    stream::unfold(
        (
            body_stream,
            EventStreamDecoder::new(),
            false,
            interval(Duration::from_secs(PING_INTERVAL_SECS)),
            guard,
        ),
        |(mut body_stream, mut decoder, finished, mut ping_interval, mut guard)| async move {
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
                        return Some((stream::iter(bytes), (body_stream, decoder, false, ping_interval, guard)));
                    }

                    // 然后处理数据流
                    chunk_result = body_stream.next() => {
                        match chunk_result {
                            Some(Ok(chunk)) => {
                                let _ = decode_frames_into(&mut decoder, &chunk, &mut guard.ctx);
                                // 继续读取下一个 chunk，不发送任何数据
                            }
                            Some(Err(e)) => {
                                tracing::error!("读取响应流失败: {}", e);
                                // 上游流中途失败：置传输失败态（供收尾按 NetworkError 记账），
                                // 先发 SSE error 事件显式告知"本次未正常完成"，再补齐已缓冲事件收尾。
                                // 否则 Claude Code 把截断输出当成功、不重试。幂等保留首因。
                                guard.ctx.mark_transport_error(e.to_string());
                                let mut all_events = Vec::new();
                                if !guard.ctx.error_event_emitted() {
                                    all_events.push(SseEvent::error_event(
                                        guard.ctx.completion().sse_error_type(),
                                        guard.ctx.completion().client_message(),
                                    ));
                                    guard.ctx.mark_error_event_emitted();
                                }
                                all_events.extend(guard.ctx.finish_and_get_all_events());
                                let bytes: Vec<Result<Bytes, Infallible>> = all_events
                                    .into_iter()
                                    .map(|e| Ok(Bytes::from(e.to_sse_string())))
                                    .collect();
                                guard.emit();
                                return Some((stream::iter(bytes), (body_stream, decoder, true, ping_interval, guard)));
                            }
                            None => {
                                // 流结束，完成处理并返回所有事件（已更正 input_tokens）。
                                // 【缺陷1 时序修复·/cc/v1 同构】finish_and_get_all_events 内部调
                                // generate_final_events（含残留 tool flush，那步才置失败态）。必须**先**跑它,
                                // **再**据 completion 补 error,否则无 stop 残留截断场景漏发 error（客户端误判成功）。
                                // 残留 flush 的 ③ 逻辑置失败态时已返回空(不发坏 JSON),error 插到最前符合 SSE 语义。
                                let tail = guard.ctx.finish_and_get_all_events();
                                let mut all_events = Vec::new();
                                if !guard.ctx.completion().is_ok() && !guard.ctx.error_event_emitted() {
                                    all_events.push(SseEvent::error_event(
                                        guard.ctx.completion().sse_error_type(),
                                        guard.ctx.completion().client_message(),
                                    ));
                                    guard.ctx.mark_error_event_emitted();
                                }
                                // 空响应检测（buffered 路径同构）：正常完成但收尾兜底后仍无内容时，
                                // 返回显式 error 事件而非空 end_turn。
                                if guard.ctx.completion().is_ok()
                                    && !guard.ctx.error_event_emitted()
                                    && guard.ctx.is_empty_response()
                                {
                                    let oversized = guard.ctx.empty_response_is_oversized_context();
                                    tracing::warn!(
                                        oversized_context = oversized,
                                        "上游返回空响应（buffered 路径，收尾兜底后仍无内容），补发 error 事件"
                                    );
                                    all_events.push(empty_response_error_event(oversized));
                                    guard.ctx.mark_error_event_emitted();
                                } else {
                                    all_events.extend(tail);
                                }
                                let bytes: Vec<Result<Bytes, Infallible>> = all_events
                                    .into_iter()
                                    .map(|e| Ok(Bytes::from(e.to_sse_string())))
                                    .collect();
                                guard.emit();
                                return Some((stream::iter(bytes), (body_stream, decoder, true, ping_interval, guard)));
                            }
                        }
                    }
                }
            }
        },
    )
    .flatten()
}

/// 缓冲流结束时埋点一条记录
fn emit_buffered_usage(
    provider: &crate::kiro::provider::KiroProvider,
    ctx: &BufferedStreamContext,
    meta: &crate::kiro::provider::CallMeta,
    client: &ClientInfo,
    disconnected: bool,
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
    apply_usage_outcome(
        &mut record,
        ctx.completion().is_ok(),
        ctx.completion_outcome(),
        (!ctx.completion().is_ok()).then(|| ctx.completion().client_message()),
        ctx.is_empty_response(),
        ctx.empty_response_is_oversized_context(),
        disconnected,
    );
    // 生命周期累计花费：把本次真实 credit 消耗累加到该凭据（独立于用量保留期，只增不清）。
    if let Some(c) = record.credits_used {
        provider.report_credits(meta.credential_id, c);
    }
    client.apply(&mut record);
    crate::usage::emit_record(record);
}

#[cfg(test)]
#[path = "handlers_tests.rs"]
mod handlers_tests;

#[cfg(test)]
pub(crate) use handlers_tests::error_translation_tests;
