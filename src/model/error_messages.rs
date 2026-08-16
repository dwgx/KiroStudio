//! 错误码/提示词覆盖表：内置默认表 + 配置覆盖的合并解析。
//!
//! 设计文档：`docs/error-codes-config-design.md`（以它为准）。
//! 默认表来源：`docs/error-codes-inventory.md`（现状文案，2026-08-15 基线）。
//!
//! 层级关系：配置表（`Config.error_messages`）命中 > 调用点内置默认。
//! 条目内字段级合并：配置字段为 `None` 时用调用点内置默认（只改 message 时 status/type 不必填）。
//! 校验见 `crate::admin::service::validate_error_messages`（失败整表拒绝，保持旧表）。
//!
//! ## 默认表的角色（B3 重写，2026-08-15 对抗审查）
//!
//! 实际渲染**不走本表**：`handlers::resolve_msg(cfg, key, default)` 未命中配置时返回
//! 调用点内置 `default`（真实渲染值 = 调用点当前行为）。本表是 **admin 前端「默认值
//! 预览」与配置校验基线**（`admin/handlers.rs` / `admin/service.rs` 运行期读取）——
//! 因此表内默认值必须与调用点内置默认**一致**，「改默认表 = 改默认文案」的机制
//! 才成立（B3 修复前表与调用点漂移，预览展示的默认值与实际返回不符）。
//!
//! 41 个 key = A(11) + B(14) + D(12) + E(1) + F(3)。与消费侧
//! （`resolve_msg(` 调用点）双向一致由守卫
//! [`error_message_keys_consumed_match_table`] 钉死（表 key ⊆ 消费 key 且
//! 消费 key ⊆ 表 key，防死 key 与孤儿 key 复发）。
//!
//! 相对 inventory §5 统计口径的取舍（均为「无渲染值 / 拆多文案 / 合并形态」的决策）：
//! - A10（translate 链入口）无自身渲染值 → 不设 key（渲染值来自 B 表）；
//! - E5 是上游原文透传（网关零构造）、E6 无 HTTP 响应（连接中断）、E7 SSE 空流
//!   兜底（200 流内形态）→ 不设 key；
//! - D6 的三种转换失败文案不同 → 拆 unsupported_model / empty_messages /
//!   tool_mapping_failed；D9 与 D10 语义不同 → 拆 empty_response_large_input /
//!   empty_response（M3 拆分）；
//! - E1-E4/E8/E9 统一 `passthrough_failed`（调用点各传各的 status/message 默认，
//!   未配置零行为变化）；F4-F9 六个回灌类形态统一 `websearch_failed`（同上）；
//! - **D11-D14 流式 in-band 不设 key（M5 决策）**：`CompletionStatus::client_message()`
//!   是流内硬编码形态（`上游返回错误: {code} - {message}` 等动态模板 + 按限流信号
//!   二选一的 type + 429/502 二选一的 status），接配置需把 `&'static str` 签名改
//!   `String` 并给流状态机传配置快照，波及 >5 处且语义纠缠——声明**流内不可配**，
//!   表内不保留 stream_inband_* 死 key；HTTP 层错误（A/B/D/E/F 表）可配，默认值
//!   与流内一致（保 H12 契约）。
//!
//! 承重字符串（改 message 前必须保留，删除 = 外挂/客户端判据静默失效，见 inventory §3.1）：
//! kiro_shield COOLING_MARKERS 三个英文哨兵（`temporarily cooling down` /
//! `All credentials are temporarily` / `inbound rate shaping`，2026-08-15 线上实测，
//! 承载点是 A5 与 A3）、prompt is too long（B9/B10，Claude Code 压缩判据）、
//! A3/A4 英文背压哨兵。校验对这些只告警不硬拒。
//! ⚠️ 勘误（2026-08-15）：`等容量` 只出现在 shield **注释**里，不是判据——
//! A1/A2 文案不承载任何 COOLING_MARKERS，可自由改，不再列入承重词表。

use serde::{Deserialize, Serialize};

use super::config::Config;

/// 单个错误形态的覆盖条目。
///
/// 所有字段可选：`None` = 用内置默认（零行为变化）。结构整体缺省 = 空条目
/// （`#[serde(default)]`，旧 config.json 无此字段时安全）。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ErrorMessageOverride {
    /// HTTP 状态码。校验白名单 [400,401,403,404,413,429,500,502,503]
    /// （对齐 `upstreamRetryAbsorbExhaustedStatus` 先例；200 等流式 in-band 形态
    /// 是默认表保留的现状值，配置侧不可选）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    /// Anthropic 协议 error.type。校验白名单（官方 9 类 + quota_exceeded_error），
    /// 见 `crate::admin::service::validate_error_messages`。
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub r#type: Option<String>,
    /// 人读的排障文案。承重字符串（shield COOLING_MARKERS 三哨兵 /
    /// prompt is too long / 英文背压哨兵）建议保留：外挂与客户端拿它们当判据，
    /// 改掉会静默失效（inventory §3.1）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Retry-After 秒数（校验 0-3600）。⚠️ 号池真值 `retry_after_secs=N`
    /// 永远优先于配置（代码层强制，见设计 §二 4），本值只是兜底。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_after_secs: Option<u64>,
}

/// 内置默认表：(key, status, type, message, retry_after_secs)。
///
/// 41 个 key = A(11) + B(14) + D(12) + E(1) + F(3)（B3 重写：以消费侧
/// `resolve_msg(` 调用点为准，删死 key、补孤儿 key、key 名与调用点一致）。
/// message 是**调用点内置默认**的现状文案：动态详情（`{e}`/`{model}` 等）
/// 由调用点 format 拼接，本表只存前缀（调用点 default 与表必须逐字一致，
/// 否则前端预览的默认值 ≠ 实际渲染值）。
///
/// 说明（相对 inventory §5 统计口径的偏差，均为「无渲染值/拆多文案/合并形态」的取舍）：
/// - A10（translate 链入口）无自身渲染值 → 不设 key（渲染值来自 B 表）；
/// - E5 是上游原文透传（网关零构造）、E6 无 HTTP 响应（连接中断）→ 不设 key；
/// - D6 的三种转换失败文案不同 → 拆 unsupported_model / empty_messages / tool_mapping_failed；
/// - D9/D10 空响应语义不同（M3 拆分）→ empty_response_large_input（400 不可重试）/
///   empty_response（429 可重试），status 均由判据承重不读配置；
/// - E1-E4/E8/E9 统一 `passthrough_failed`（调用点各传各的 status/message 默认，
///   未配置零行为变化；表存代表形态）；F4-F9 六个回灌类形态统一 `websearch_failed`；
/// - D11-D14 流式 in-band **不设 key（M5 决策）**：流内形态硬编码不可配
///   （见模块头注释），表内不保留 stream_inband_* 死 key。
///
/// 承重字符串（改 message 前必须保留，删除 = 外挂/客户端判据静默失效，见 inventory §3.1）：
/// kiro_shield COOLING_MARKERS 三个英文哨兵（`temporarily cooling down` /
/// `All credentials are temporarily` / `inbound rate shaping`，2026-08-15 线上实测，
/// 承载点是 A5 与 A3）、prompt is too long（B9/B10，Claude Code 压缩判据）、
/// A3/A4 英文背压哨兵。校验对这些只告警不硬拒。
/// ⚠️ 勘误（2026-08-15）：`等容量` 只出现在 shield **注释**里，不是判据——
/// A1/A2 文案不承载任何 COOLING_MARKERS，可自由改，不再列入承重词表。
const DEFAULT_TABLE: &[(&str, u16, &str, &str, Option<u64>)] = &[
    // ---- A. map_provider_error 主干 ----
    // A1 shared_budget_exhausted（每请求跨层共享预算耗尽）。文案**不承载** shield
    // 判据（「等容量」只出现在 shield 注释里，2026-08-15 实测），可自由改。
    // RA 调用点默认 8s（ABSORB_EXHAUSTED_RETRY_AFTER_SECS 兜底，配置可覆盖）。
    (
        "shared_budget_exhausted",
        503,
        "api_error",
        "网关已就该请求打满上游调用预算（每请求上限），上游仍不可用（等容量）。这是可重试的瞬态状态，请按 Retry-After 退避后重试。",
        Some(8),
    ),
    // A2 absorb_budget_exhausted（吸收层已尽力仍失败）。文案**不承载** shield
    // 判据（同上 A1 勘误），可自由改。
    // RA 优先级：号池真值 retry_after_secs=N → 配置 → 风控 20s → 兜底 8s。
    (
        "absorb_exhausted",
        503,
        "api_error",
        "网关已就该请求重试至预算上限，上游仍不可用（等容量）。这是可重试的瞬态状态，请按 Retry-After 退避后重试。若持续出现：①面板『限流健康』查看号池容量与冷却分布；②补充凭据分摊上游压力；③必要时调高 upstreamRetryAbsorb* 预算。",
        Some(8),
    ),
    // A3 inbound_admission_timeout（入站整形排队超时，网关背压）。承重：英文背压
    // 哨兵（Gateway inbound rate shaping is at capacity + gateway-side backpressure
    // + retrying immediately will not help，吸收层/外挂按 body 区分网关背压）。
    // ⚠️ 同时是 shield COOLING_MARKERS 的 `inbound rate shaping` 承载点
    // （2026-08-15 线上实测判据）——该子串被替换 ⇒ shield 失配，整句仍可能因
    // 背压哨兵被吸收层正确分类，但外挂会丢「cool」节奏，改前先 grep 仓外。
    // M2 合一：A3 与入站闸门（try_inbound_admission_gate）共用 key `gate_timeout`
    // （两处调用点默认文案为同族英文背压哨兵——承重词 gateway-side backpressure /
    // retrying immediately will not help 齐全，map_provider_error 分支的形态多一句
    // "(request admission timed out)"；未配置时各用各的 default，表取 A3 完整形态；
    // RA 均以串内真值优先，配置兜底）。
    (
        "gate_timeout",
        429,
        "rate_limit_error",
        "Gateway inbound rate shaping is at capacity (request admission timed out). This is gateway-side backpressure, not an upstream cooldown; retrying immediately will not help.",
        Some(1),
    ),
    // A4 upstream_gate_full（上游并发闸满）。承重：英文背压哨兵（同 A3 族）。
    // RA 串内真值优先（provider 打 retry_after_secs=2），配置兜底。
    (
        "upstream_gate_full",
        429,
        "rate_limit_error",
        "Gateway upstream concurrency gate is full (too many in-flight upstream calls). This is gateway-side backpressure, not an upstream cooldown; retrying immediately will not help.",
        Some(2),
    ),
    // A5 全池冷却/池耗尽/RPM 饱和（一切带退避真值的串）。承重：整句同时承载
    // shield COOLING_MARKERS 两个判据（`All credentials are temporarily` +
    // `temporarily cooling down`，2026-08-15 线上实测）。
    // RA 决议链：号池真值 `retry_after_secs=N` 恒优先（能进 A5 分支必然带真值），
    // 此 key 的 retryAfterSecs 配置是链上兜底（2026-08-16 修复注释-行为矛盾：
    // handlers.rs 的 A5 分支此前 `_cfg_ra` 完全不读，配置了静默无效；现改为读，
    // 行为不变 —— 真值恒存在，配置不可达）。
    (
        "rate_limited_pool",
        429,
        "rate_limit_error",
        "All credentials are temporarily cooling down. Please retry after the indicated delay.",
        None,
    ),
    // A6 model_unsupported_by_pool（号池对该模型永久不可用）。永久态：配置 RA 忽略。
    (
        "model_unsupported",
        404,
        "not_found_error",
        "请求的模型不被当前号池支持（所有凭据的订阅档位或成本白名单均不含该模型）。这不是临时故障，重试无效：请换用号池支持的模型，或为凭据开通/放开该模型。",
        None,
    ),
    // A7 上游账户级速率限流。RA 决议链：上游显式 Retry-After 真值（S2，
    // `upstream_retry_after=N` marker）> 配置兜底 > 固定 8s。
    (
        "rate_limited_credential",
        429,
        "rate_limit_error",
        "上游账户级速率限流（请求过于密集）。这是可重试的临时状态，请按 Retry-After 退避后重试。若持续出现：①降低客户端并发；②为号池补充更多凭据分摊速率；③面板『限流健康』确认是否单号承载了全部流量。",
        Some(8),
    ),
    // A8 上游 403 临时风控（账号被暂时限制，非永久封禁）。RA 配置兜底 20s。
    (
        "account_throttled",
        429,
        "rate_limit_error",
        "上游账户级临时风控（账号被暂时限制，非永久封禁）。这是可恢复的限时状态，请按 Retry-After 退避后重试。若持续出现：①降低并发与请求密度；②为号池补充更多凭据分摊风控压力；③面板『限流健康』查看是否单号承载了全部流量。",
        Some(20),
    ),
    // A9 上游 403 region 错配（bearer token 对目标 region 无效）。M2 语义错位修复：
    // 独立 key `region_mismatch`，不再占用 `permission_denied`（后者留给 D2 IP 黑名单）。
    // 永久配置错误态：配置 RA 忽略。
    (
        "region_mismatch",
        403,
        "permission_error",
        "上游拒绝该凭据的授权（bearer token 对目标 region 无效）。这不是服务端故障，重试无效：`ksk_` 类 token 按 region 授权，打错 region 恒被拒。排障：①面板查看该凭据的 region 是否与签发 region 一致；②对该凭据手动改 region（或等网关 region 探测自动重选）；③若整池同区，确认推号来源给的 region 正确。",
        None,
    ),
    // A11 上游 5xx/传输层瞬态。RA 配置兜底 3s。
    (
        "upstream_5xx",
        503,
        "api_error",
        "上游服务暂时不可用（5xx 或连接失败），这是可重试的瞬态错误。请按 Retry-After 退避后重试；若持续出现，请查看网关日志。",
        Some(3),
    ),
    // A12 兜底：未识别任何分支。
    (
        "unrecognized_upstream",
        502,
        "api_error",
        "上游 API 调用失败（未识别错误）。请查看网关日志获取详情。",
        None,
    ),
    // ---- B. translate_upstream_error 翻译链 ----
    // B1 subscription_unsupported（订阅档位不含，永久）。永久态：配置 RA 忽略。
    (
        "subscription_unsupported",
        404,
        "not_found_error",
        "当前凭据的订阅档位不支持该应用/模型（永久条件，非临时故障）。换区或重试均无效：请更换为订阅覆盖该应用/模型的凭据，或联系账号管理员开通对应档位。",
        None,
    ),
    // B2 quota_exhausted_all（全池月度配额耗尽）。
    (
        "quota_exhausted",
        429,
        "rate_limit_error",
        "月度请求配额已耗尽（号池内所有凭据）。排障：①面板查看各凭据用量；②等待配额周期重置；③为号池补充新凭据。",
        None,
    ),
    // B3 裸 MONTHLY_REQUEST_COUNT / QUOTA（单号/未知范围）。
    (
        "quota_subscription",
        429,
        "rate_limit_error",
        "请求配额已耗尽。排障：①面板查看各凭据用量，切到仍有额度的账号；②等待配额周期重置；③为号池补充新凭据。",
        None,
    ),
    // B4 MODEL_TEMPORARILY_UNAVAILABLE / INSUFFICIENT_MODEL_CAPACITY（容量紧张）。
    // ⚠️ B4 矛盾修复（设计 §五 1）：调用点默认 RA 3s（与 A11 同档，客户端退避），
    // 表默认必须与调用点一致 → Some(3)（现状表 None 是表调用点漂移，B3 修复）。
    (
        "overloaded_capacity",
        503,
        "overloaded_error",
        "上游模型暂时不可用（负载过高），请稍后重试。若持续出现：①换用同族其他版本（如 claude-opus-4.8）；②新发布模型发布初期容量有限，属正常现象，等待 1~2 小时后通常恢复。",
        Some(3),
    ),
    // B5 FEATURE_NOT_SUPPORTED（region 未开通功能）。
    (
        "feature_not_supported",
        502,
        "api_error",
        "当前凭据所在 region 未开通该功能（profile 未激活）。排障：①网关会在刷新时自动验活重选可用 region；②如持续，右键该凭据切换 Profile ARN 到已开通 region（如 eu-central-1）；③确认该账号确在某 region 开通了 Kiro。",
        None,
    ),
    // B6 裸 Invalid token / subscription（未带标记的凭据类文案）。
    (
        "invalid_credential",
        502,
        "api_error",
        "上游拒绝凭据（订阅失效或 token 无效）。排障：①面板对该凭据点『刷新 Token』；②若为 Enterprise/IdC 号，确认 profileArn 已正确解析；③测活确认订阅有效，失效则更换凭据。",
        None,
    ),
    // B7 IMAGE_MIME_MISMATCH。
    (
        "image_mime_mismatch",
        400,
        "invalid_request_error",
        "图片声明的 media_type 与实际字节格式不符（上游 IMAGE_MIME_MISMATCH）。这是请求构造问题，重试无效。排障：①按图片真实格式填写 media_type（如 JPEG 字节不要声明 image/png）；②不要在改扩展名后沿用旧的 media_type；③重新读取并重新编码该图片后再发。",
        None,
    ),
    // B8 REQUEST_BODY_INVALID / Invalid tool use format。
    (
        "request_body_invalid",
        400,
        "invalid_request_error",
        "请求体校验失败（上游 REQUEST_BODY_INVALID）。这是请求构造问题，重试无效。排障：①检查工具调用与工具结果的配对（上游对 tool 配对较严，截断/重排序会产生孤儿 tool_use）；②检查消息 role 与内容字段合法性；③重新构造请求后再发。",
        None,
    ),
    // B9 CONTENT_LENGTH_EXCEEDS_THRESHOLD（上下文窗口满）。承重：prompt is too long
    // （Claude Code compact-and-retry 的 message 小写子串判据，删除 = 自动压缩静默失效）。
    (
        "context_too_large",
        400,
        "invalid_request_error",
        "prompt is too long: 上下文窗口已满（对话历史累积超出模型上下文上限）。排障：①精简对话历史或开新会话；②缩短 system prompt；③减少同时挂载的工具数量。",
        None,
    ),
    // B10 Input is too long（单次输入超限）。承重：prompt is too long（同 B9）。
    (
        "input_too_long",
        400,
        "invalid_request_error",
        "prompt is too long: 单次输入过长（请求体本身超出上游限制）。排障：①拆分过大的消息或附件；②减少一次性粘贴的文件内容；③对超大工具结果先做摘要。",
        None,
    ),
    // B11 传输层 DNS 类（is_transport_error 闸门内）。
    (
        "upstream_dns",
        502,
        "api_error",
        "DNS 解析失败（无法解析上游域名）。排障：①检查本机/容器 DNS 配置；②若走代理，确认代理能解析 kiro.dev；③确认网络出口正常。",
        None,
    ),
    // B12 传输层超时。
    (
        "upstream_timeout",
        504,
        "api_error",
        "连接上游超时。排障：①上游或代理可能拥塞，稍后重试；②检查代理延迟；③大请求可拆小以缩短单次耗时。",
        None,
    ),
    // B13 传输层 TLS/证书。
    (
        "upstream_tls",
        502,
        "api_error",
        "TLS/证书握手失败。排障：①检查系统时间是否准确；②若走中间人代理，确认其证书受信；③确认未误用被拦截的代理。",
        None,
    ),
    // B14 传输层代理。
    (
        "upstream_proxy",
        502,
        "api_error",
        "代理连接失败。排障：①检查代理地址/账密是否正确；②确认代理在线可达；③面板核对该凭据绑定的代理配置。",
        None,
    ),
    // ---- D. 本地构造错误（两条 HTTP 入口；D11-D14 流式 in-band 不设 key，见模块头）----
    // D1 API key 不匹配（中间件，anthropic/middleware.rs 接入）。
    (
        "api_key_invalid",
        401,
        "authentication_error",
        "Invalid API key",
        None,
    ),
    // D2 IP 黑名单命中（security_block_response；A9 已改用 region_mismatch，
    // 本 key 专属于本地安全过滤）。
    (
        "permission_denied",
        403,
        "permission_error",
        "来源 IP 已被封禁",
        None,
    ),
    // D3 机器码黑名单命中。inventory §3.1：sbsbsb！无外部依赖（疑似刻意文案），可改。
    ("machine_blocked", 403, "permission_error", "sbsbsb！", None),
    // D3b max_tokens 本地上限校验（2026-08-15 smoke test 发现：超出上游上限此前被
    // 误判瞬态吞进 failover+absorb，30s 延迟 + 503）。上限对齐上游实测 393216。
    ("max_tokens_exceeded", 400, "invalid_request_error", "max_tokens 超出上限 393216", None),
    // D4 请求体 JSON 解析失败（/v1 入口）。⚠️ 动态详情（{e}）由调用点 format，
    // 表只存前缀（与调用点 default 逐字一致，B3 修正）。
    (
        "request_parse_failed",
        400,
        "invalid_request_error",
        "请求体解析失败",
        None,
    ),
    // D5 KiroProvider 未配置。⚠️ 现状 type=service_unavailable 不在配置白名单
    // （Anthropic 官方 9 类外），默认表保留现状；配置侧只能改白名单内 type。
    (
        "provider_not_configured",
        503,
        "service_unavailable",
        "Kiro API provider not configured",
        None,
    ),
    // D6 请求转换失败（UnsupportedModel）。⚠️ 动态详情（{model}）由调用点 format。
    (
        "unsupported_model",
        400,
        "invalid_request_error",
        "模型不支持",
        None,
    ),
    // D6 请求转换失败（EmptyMessages）。
    (
        "empty_messages",
        400,
        "invalid_request_error",
        "消息列表为空",
        None,
    ),
    // D6 请求转换失败（UnsupportedToolMapping）。⚠️ 动态详情（{tool_name} — {reason}）
    // 由调用点 format。
    (
        "tool_mapping_failed",
        400,
        "invalid_request_error",
        "工具参数无法映射",
        None,
    ),
    // D7 Kiro 请求体序列化失败（含压缩重试轮，/v1 与 /cc/v1 各两份共 4 调用点）。
    // ⚠️ 现状 type=internal_error 不在配置白名单（同 D5）；动态详情（{e}）由调用点 format。
    (
        "request_serialization_failed",
        500,
        "internal_error",
        "序列化请求失败",
        None,
    ),
    // D8 非流式读上游响应体失败。⚠️ 动态详情（{e}）由调用点 format。
    (
        "response_read_failed",
        502,
        "api_error",
        "读取响应失败",
        None,
    ),
    // D9 空/近空响应 + 大输入（疑似上下文超限，重试无效）。M3 拆分：与 D10 各自
    // 独立 key（status 由 oversized 判据承重，本 key 的 status 恒 400 不读配置）。
    (
        "empty_response_large_input",
        400,
        "invalid_request_error",
        "上游返回了空响应，疑似上下文已接近窗口上限。请精简对话历史（如 /compact）、缩短 system prompt 或减少工具数量后重试。",
        None,
    ),
    // D10 空/近空响应 + 小输入（疑似偶发，可重试）。⚠️ D10 矛盾修复（设计 §五 2）：
    // 调用点默认 RA 3s（EMPTY_RESPONSE_RETRY_AFTER_SECS），表默认必须与调用点一致
    // → Some(3)（现状表 None 是表调用点漂移，B3 修复）。
    (
        "empty_response",
        429,
        "overloaded_error",
        "上游返回了空响应，请重试。",
        Some(3),
    ),
    // ---- E. 透传池（custom_api，本地构造部分；E5 上游原文透传不在此列）----
    // E1 缺 base_url / E2 出站校验失败 / E3 首字节超时 / E4 连接层失败 /
    // E8 非流式读取失败 / E9 构建失败：统一 key `passthrough_failed`
    // （passthrough.rs err_response 接入）。⚠️ 调用点各传各的 status/message 默认
    // （全部 502，message 各形态不同），未配置零行为变化；表存代表形态
    // （E4「透传上游请求失败」）。E7（SSE 空流兜底，200 流内形态）不设 key。
    (
        "passthrough_failed",
        502,
        "api_error",
        "透传上游请求失败",
        None,
    ),
    // ---- F. WebSearch 快路径与回灌 ----
    // F1 无法从消息提取搜索查询。
    (
        "websearch_query_missing",
        400,
        "invalid_request_error",
        "无法从消息中提取搜索查询",
        None,
    ),
    // F2 快路径 MCP 调用失败（非预算）/ F3 快路径共享预算耗尽 / F8 回灌 MCP 失败：
    // 统一 key `mcp_failed`。⚠️ F3 预算耗尽的调用点默认是 503/api_error/预算文案/
    // RA 8（与 F2/F8 的 502/upstream_error 不同）——表存 F2/F8 主形态，F3 未配置
    // 时由调用点 default 兜底（零行为变化）。⚠️ 现状 type=upstream_error 不在白名单。
    (
        "mcp_failed",
        502,
        "upstream_error",
        "WebSearch 上游调用失败",
        None,
    ),
    // F4 回灌转换失败 / F5 回灌序列化失败（含压缩重试轮）/ F6 回灌上游 in-band
    // 错误 / F7 回灌流中断 / F9 回灌循环异常：统一 key `websearch_failed`
    // （websearch.rs run_round / run_web_search_loop 接入）。⚠️ 六个调用点默认
    // status/type/message 各不相同（400/500/502 × invalid/internal/upstream_error），
    // 未配置时各用各的 default（零行为变化）；表存 F6 主形态（502/upstream_error）。
    (
        "websearch_failed",
        502,
        "upstream_error",
        "WebSearch 回灌上游返回错误",
        None,
    ),
];

/// 内置默认表：(key, status, type, message, retry_after_secs)。
///
/// 兜底语义：配置表命中 > 内置默认；未配置的 key 用这里（删掉配置键行为不变）。
pub fn default_error_messages()
-> &'static [(&'static str, u16, &'static str, &'static str, Option<u64>)] {
    DEFAULT_TABLE
}

/// 解析单个错误形态的渲染值：`(status, type, message, retry_after_secs)`。
///
/// 签名见设计文档 §四：`resolve_error_message(config, key, default)`。
/// - key 不在默认表 → `None`（调用方用自己的 default 兜底）；
/// - 配置表命中 > 内置默认；条目内字段级合并：`None` 字段用内置默认
///   （只配 message 时 status/type 自动落默认，零行为变化）。
pub(crate) fn resolve_error_message(
    cfg: &Config,
    key: &str,
) -> Option<(u16, String, String, Option<u64>)> {
    let entry = default_error_messages().iter().find(|(k, ..)| *k == key)?;
    let (default_status, default_type, default_message, default_ra) =
        (entry.1, entry.2, entry.3, entry.4);
    let o = cfg.error_messages.get(key);
    let status = o.and_then(|o| o.status).unwrap_or(default_status);
    let ty = o.and_then(|o| o.r#type.as_deref()).unwrap_or(default_type);
    let message = o
        .and_then(|o| o.message.as_deref())
        .unwrap_or(default_message);
    let retry_after = o.and_then(|o| o.retry_after_secs).or(default_ra);
    Some((status, ty.to_string(), message.to_string(), retry_after))
}

/// 承重字符串检测（inventory §3.1）：返回命中的哨兵说明，`None` = 无。
///
/// 命中只告警不硬拒（`validate_error_messages` 调此函数打 warn）——这些串被外挂
/// （kiro_shield COOLING_MARKERS）与客户端（Claude Code 压缩判据）当判据，
/// 改掉会静默失效，但管理员显式要改时仍允许。
///
/// 词表 = shield 实测判据全集（2026-08-15 线上核对 `COOLING_MARKERS` 只有
/// `temporarily cooling down` / `All credentials are temporarily` /
/// `inbound rate shaping` 三个英文串；⚠️ `等容量` 只出现在 shield **注释**里、
/// 不是判据，故不在本表——A1/A2 文案可自由改）+ Claude Code 压缩判据
/// （`prompt is too long`）+ A3/A4 背压语义串。
pub(crate) fn check_load_bearing_message(message: &str) -> Option<&'static str> {
    let m = message.to_lowercase();
    // 词条 = shield COOLING_MARKERS 逐条镜像（前缀子串即判据本体，与 shield 的
    // contains 匹配同口径；A5 全句「All credentials are temporarily cooling down」
    // 命中第一、二条，不单独列）。
    if m.contains("all credentials are temporarily") {
        return Some("All credentials are temporarily（kiro_shield COOLING_MARKERS 判据）");
    }
    if m.contains("temporarily cooling down") {
        return Some("temporarily cooling down（kiro_shield COOLING_MARKERS 判据）");
    }
    if m.contains("inbound rate shaping") {
        return Some("inbound rate shaping（kiro_shield COOLING_MARKERS 判据）");
    }
    if m.contains("prompt is too long") {
        return Some("prompt is too long（Claude Code compact-and-retry 判据）");
    }
    if m.contains("gateway-side backpressure") || m.contains("retrying immediately will not help") {
        return Some("英文背压哨兵（A3/A4 网关背压判据）");
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::config::Config;
    use std::collections::HashMap;

    #[test]
    fn default_table_is_complete_unique_and_well_named() {
        let table = default_error_messages();
        let key_names: Vec<&str> = table.iter().map(|(k, ..)| *k).collect();
        // 42 = A(11) + B(14) + D(13) + E(1) + F(3)（B3 重写：以消费 key 为准，
        // 含 max_tokens_exceeded）。加 key 时同步更新此数——守卫
        // `error_message_keys_consumed_match_table` 会钉死与消费集的双向一致。
        assert_eq!(
            key_names.len(),
            42,
            "默认表 key 数必须是 42（A11+B14+D13+E1+F3，含 max_tokens_exceeded）"
        );
        let mut seen = HashMap::new();
        for (key, status, ty, message, _ra) in table {
            assert!(seen.insert(*key, ()).is_none(), "默认表 key 重复: {key}");
            assert!(!key.is_empty(), "key 不能为空");
            assert!(
                key.chars().next().is_some_and(|c| c.is_ascii_lowercase())
                    && key
                        .chars()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'),
                "key 命名不规范（只允许小写字母/数字/下划线）: {key}"
            );
            assert!(
                (400..=599).contains(status) || *status == 200,
                "默认表 status 必须在 200 或 400-599: {key} = {status}"
            );
            assert!(!ty.is_empty(), "默认表 type 不能为空: {key}");
            assert!(!message.is_empty(), "默认表 message 不能为空: {key}");
        }
    }

    /// B3 防复发守卫：默认表 key 集合与全仓 `resolve_msg(` 调用点的消费 key 集合
    /// **双向一致**（表 key ⊆ 消费 key 且 消费 key ⊆ 表 key）。
    ///
    /// 实现：`include_str!` 读消费侧源码文件（handlers / websearch / passthrough /
    /// middleware），纯 `find` 提取每个调用点括号后的第一个字符串字面量（= key）。
    /// 防自证：守卫只读**别的**文件（本文件不参与 include_str），测试代码不写
    /// 任何 key 字面量；提取到的 key 再做格式校验（小写字母/数字/下划线），
    /// 防止把注释或动态内容误当 key 吞掉。
    ///
    /// 漂移后果：表加 key 没接调用点（死 key）或调用点用表外 key（孤儿 key，
    /// 配置永不生效）都会红——B3 修复前 61→37 的漂移就是这两种形态。
    #[test]
    fn error_message_keys_consumed_match_table() {
        let table_keys: std::collections::HashSet<&str> = default_error_messages()
            .iter()
            .map(|(k, ..)| *k)
            .collect();

        let sources = [
            include_str!("../anthropic/handlers.rs"),
            include_str!("../anthropic/websearch.rs"),
            include_str!("../anthropic/middleware.rs"),
            include_str!("../kiro/passthrough.rs"),
        ];
        let needle = "resolve_msg(";
        let mut consumed: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for src in sources {
            let mut rest = src;
            while let Some(pos) = rest.find(needle) {
                let after = &rest[pos + needle.len()..];
                // 跳过 `&current_error_messages(),` / `&err_msgs,` 等参数前缀，
                // 取括号后的第一个字符串字面量（= key）。
                let Some(q1) = after.find('"') else { break };
                let key = &after[q1 + 1..];
                let Some(q2) = key.find('"') else { break };
                let key = &key[..q2];
                assert!(
                    !key.is_empty()
                        && key
                            .chars()
                            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'),
                    "提取到的 key 字面量格式非法（疑似提取错位）: {key:?}"
                );
                consumed.insert(key);
                rest = after;
            }
        }

        let orphans: Vec<&str> = consumed
            .iter()
            .copied()
            .filter(|k| !table_keys.contains(k))
            .collect();
        assert!(
            orphans.is_empty(),
            "调用点使用了默认表没有的 key（孤儿 key：配置永不生效，B3 防复发）：{orphans:?}"
        );
        let dead: Vec<&str> = table_keys
            .iter()
            .copied()
            .filter(|k| !consumed.contains(k))
            .collect();
        assert!(
            dead.is_empty(),
            "默认表存在无调用点的 key（死 key：改了默认值也不会影响任何渲染，B3 防复发）：{dead:?}"
        );
    }

    /// B3 表-调用点默认值一致抽查（防「表展示默认值 ≠ 实际渲染值」漂移）：
    /// 表默认值必须与调用点内置默认逐字一致（改动处：B4/D10 的 RA、
    /// D4/D6/D7/D8 的占位符前缀、A9 的 region_mismatch 文案、D2 的 permission_denied
    /// 文案）。抽查关键 key 的默认值，整表一致性由
    /// `error_message_keys_consumed_match_table` 钉 key 集、本测试钉值。
    #[test]
    fn table_defaults_match_call_site_defaults() {
        let table = default_error_messages();
        let get = |key: &str| {
            table
                .iter()
                .find(|(k, ..)| *k == key)
                .unwrap_or_else(|| panic!("key 必须在默认表: {key}"))
        };

        // B4 容量 503：调用点 default 带 RA Some(3)（B4 矛盾修复）。
        assert_eq!(get("overloaded_capacity").4, Some(3));
        // D10 空响应 429：调用点 default RA Some(3)（EMPTY_RESPONSE_RETRY_AFTER_SECS）。
        assert_eq!(get("empty_response").4, Some(3));
        // D4/D6/D7/D8：动态详情由调用点 format，表只存前缀（与调用点 default 一致）。
        assert_eq!(get("request_parse_failed").3, "请求体解析失败");
        assert_eq!(get("unsupported_model").3, "模型不支持");
        assert_eq!(get("tool_mapping_failed").3, "工具参数无法映射");
        assert_eq!(get("request_serialization_failed").3, "序列化请求失败");
        assert_eq!(get("response_read_failed").3, "读取响应失败");
        // A9 region 错配独立 key：403 permission_error + 中文排障文案。
        let (status, ty, message, ra) = (get("region_mismatch").1, get("region_mismatch").2, get("region_mismatch").3, get("region_mismatch").4);
        assert_eq!((status, ty, ra), (403, "permission_error", None));
        assert!(message.contains("region"), "region_mismatch 文案必须点明 region 语义");
        // D2 permission_denied 回归：本 key 只承载 IP 黑名单语义（A9 已移走）。
        assert_eq!(get("permission_denied").3, "来源 IP 已被封禁");
        // D1/D3 接入后的表默认与构造点一致。
        assert_eq!(get("api_key_invalid").3, "Invalid API key");
        assert_eq!(get("machine_blocked").3, "sbsbsb！");
    }

    /// M3 拆分：D9 与 D10 各占独立 key，status 由判据承重（表默认 400/429），
    /// message/RA 各自可配。
    #[test]
    fn empty_response_keys_are_split_and_distinct() {
        let table = default_error_messages();
        let d9 = table
            .iter()
            .find(|(k, ..)| *k == "empty_response_large_input")
            .expect("D9 key 必须存在");
        let d10 = table
            .iter()
            .find(|(k, ..)| *k == "empty_response")
            .expect("D10 key 必须存在");
        assert_eq!((d9.1, d9.2, d9.4), (400, "invalid_request_error", None));
        assert_eq!((d10.1, d10.2), (429, "overloaded_error"));
        assert_ne!(d9.3, d10.3, "D9/D10 文案必须不同（压缩提示 vs 可重试）");
    }

    #[test]
    fn resolve_partial_override_merges_with_defaults() {
        // 只配 message → status/type/RA 全用内置默认（零行为变化的字段级合并）。
        let mut cfg = Config::default();
        let mut table = HashMap::new();
        let mut entry = ErrorMessageOverride::default();
        entry.message = Some("自定义文案".to_string());
        table.insert("quota_exhausted".to_string(), entry);
        cfg.error_messages = table;

        let (status, ty, message, ra) =
            resolve_error_message(&cfg, "quota_exhausted").expect("key 在默认表必须能解析");
        assert_eq!(status, 429);
        assert_eq!(ty, "rate_limit_error");
        assert_eq!(message, "自定义文案");
        assert_eq!(ra, None);
    }

    #[test]
    fn resolve_full_override_wins_over_default() {
        let mut cfg = Config::default();
        let mut table = HashMap::new();
        table.insert(
            "quota_exhausted".to_string(),
            ErrorMessageOverride {
                status: Some(503),
                r#type: Some("overloaded_error".to_string()),
                message: Some("自定义".to_string()),
                retry_after_secs: Some(3),
            },
        );
        cfg.error_messages = table;

        let (status, ty, message, ra) = resolve_error_message(&cfg, "quota_exhausted").unwrap();
        assert_eq!(
            (status, ty.as_str(), message.as_str(), ra),
            (503, "overloaded_error", "自定义", Some(3))
        );
    }

    #[test]
    fn resolve_unknown_key_returns_none() {
        let cfg = Config::default();
        assert_eq!(
            resolve_error_message(&cfg, "not_a_real_key"),
            None,
            "不在默认表的 key 必须返回 None（调用方用自己的 default 兜底）"
        );
    }

    #[test]
    fn error_messages_serde_roundtrip_camel_case_and_skips_none() {
        // 出向：camelCase + None 字段不序列化 + 空表整体不序列化。
        let cfg = Config::default();
        let s = serde_json::to_string(&cfg).expect("序列化应成功");
        assert!(
            !s.contains("errorMessages"),
            "空表必须 skip_serializing_if 掉（默认不写盘）"
        );

        let mut cfg2 = Config::default();
        let mut table = HashMap::new();
        table.insert(
            "quota_exhausted".to_string(),
            ErrorMessageOverride {
                status: None,
                r#type: None,
                message: Some("只改文案".to_string()),
                retry_after_secs: None,
            },
        );
        cfg2.error_messages = table;
        let s2 = serde_json::to_string(&cfg2).expect("序列化应成功");
        assert!(
            s2.contains("\"errorMessages\":{\"quota_exhausted\":{\"message\":\"只改文案\"}}"),
            "部分字段 None 必须不序列化（只留 message），实际: {s2}"
        );

        // 入向：camelCase 显式值（含 type/retryAfterSecs）必须反序列化。
        let back: Config = serde_json::from_str(
            r#"{"errorMessages":{"quota_exhausted":{"status":429,"type":"rate_limit_error",
                "message":"m","retryAfterSecs":5}}}"#,
        )
        .expect("camelCase 显式值必须能反序列化");
        let entry = &back.error_messages["quota_exhausted"];
        assert_eq!(entry.status, Some(429));
        assert_eq!(entry.r#type.as_deref(), Some("rate_limit_error"));
        assert_eq!(entry.message.as_deref(), Some("m"));
        assert_eq!(entry.retry_after_secs, Some(5));
    }

    #[test]
    fn load_bearing_detection_covers_inventory_markers() {
        // shield COOLING_MARKERS 实测三哨兵（2026-08-15 线上核对）：
        // `temporarily cooling down` / `All credentials are temporarily` /
        // `inbound rate shaping` —— 改掉 ⇒ shield classify() 失配 ⇒ 1753 次事故形态。
        assert!(
            check_load_bearing_message("All credentials are temporarily cooling down").is_some()
        );
        assert!(check_load_bearing_message("...temporarily cooling down. Please retry...").is_some());
        assert!(
            check_load_bearing_message("Gateway inbound rate shaping is at capacity").is_some()
        );
        assert!(check_load_bearing_message("prompt is too long: 上下文窗口已满").is_some());
        assert!(check_load_bearing_message("This is gateway-side backpressure").is_some());
        assert!(check_load_bearing_message("retrying immediately will not help.").is_some());
        // ⚠️ 勘误（2026-08-15）：`等容量` 只出现在 shield **注释**里，不是判据——
        // 含它的文案（A1/A2）必须能自由改，不得再告警。
        assert!(
            check_load_bearing_message("上游仍不可用（等容量）。").is_none(),
            "「等容量」不是 shield COOLING_MARKERS 判据（仅注释出现），必须放行"
        );
        assert!(check_load_bearing_message("普通文案").is_none());
    }
}
