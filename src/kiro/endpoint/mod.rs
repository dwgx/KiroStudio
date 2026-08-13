//! Kiro 端点抽象
//!
//! 不同 Kiro 端点（如 `ide` / `cli`）在 URL、请求头、请求体上存在差异，
//! 但共享凭据池、Token 刷新、重试逻辑和 AWS event-stream 响应解码。
//!
//! [`KiroEndpoint`] 抽象了请求侧的差异点；`KiroProvider` 持有一个 endpoint 注册表，
//! 按凭据的 `endpoint` 字段选择对应实现。

use std::collections::HashMap;
use std::sync::Arc;

use reqwest::RequestBuilder;

use crate::kiro::model::credentials::KiroCredentials;
use crate::model::config::Config;

pub mod amazonq;
pub mod cli;
pub mod cli_runtime;
pub mod codewhisperer;
pub mod ide;

pub use amazonq::AmazonqEndpoint;
pub use cli::CliEndpoint;
pub use cli_runtime::CliRuntimeEndpoint;
pub use codewhisperer::CodewhispererEndpoint;
pub use ide::IdeEndpoint;

/// 按端点名构造实现；未知名字返回 `None`。
///
/// **所有**端点的注册都收口在这里（[`registry`] 与 [`for_credentials`] 都走它），
/// 避免"注册表加了新端点、但某条旁路仍只认 ide/cli 两种"这类漂移。
pub fn build(name: &str) -> Option<Arc<dyn KiroEndpoint>> {
    match name {
        ide::IDE_ENDPOINT_NAME => Some(Arc::new(IdeEndpoint::new())),
        cli::CLI_ENDPOINT_NAME => Some(Arc::new(CliEndpoint::new())),
        cli_runtime::CLI_RUNTIME_ENDPOINT_NAME => Some(Arc::new(CliRuntimeEndpoint::new())),
        codewhisperer::CODEWHISPERER_ENDPOINT_NAME => Some(Arc::new(CodewhispererEndpoint::new())),
        amazonq::AMAZONQ_ENDPOINT_NAME => Some(Arc::new(AmazonqEndpoint::new())),
        _ => None,
    }
}

/// 全部已知端点的名字（新增端点时**只需**改这里和 [`build`]）。
pub const ENDPOINT_NAMES: &[&str] = &[
    ide::IDE_ENDPOINT_NAME,
    cli::CLI_ENDPOINT_NAME,
    cli_runtime::CLI_RUNTIME_ENDPOINT_NAME,
    codewhisperer::CODEWHISPERER_ENDPOINT_NAME,
    amazonq::AMAZONQ_ENDPOINT_NAME,
];

/// 全部已知端点的注册表（供 `main.rs` 启动时装配 provider）。
///
/// 键取实现自报的 [`KiroEndpoint::name`]，而非 [`ENDPOINT_NAMES`] 里的字符串：两者若
/// 不一致（改了常量忘了改实现，或反之），`endpoint_for` 会查不到自己注册的端点 →
/// 该端点的号全部在热路径上拿「未知端点」。以 name() 为准可让这种笔误不可能发生。
pub fn registry() -> HashMap<String, Arc<dyn KiroEndpoint>> {
    ENDPOINT_NAMES
        .iter()
        .filter_map(|name| build(name).map(|ep| (ep.name().to_string(), ep)))
        .collect()
}

/// 按凭据解析出该走的端点实现，供**不经 `KiroProvider`** 的旁路使用
/// （深度验活 / 模型探测等）。
///
/// 口径与 `KiroProvider::endpoint_for` 完全一致（同走
/// [`KiroCredentials::effective_endpoint`]）：显式 `endpoint` 优先 → `ksk_` 号自动路由到
/// `cli` → 回退全局默认。名字无法识别时回退 IDE 实现而非报错：旁路的职责是"验活/探测"，
/// 不该因为端点名拼错就整条失败（真正的门禁在启动校验与 provider 侧）。
pub fn for_credentials(
    credentials: &KiroCredentials,
    default_endpoint: &str,
) -> Arc<dyn KiroEndpoint> {
    let name = credentials.effective_endpoint(default_endpoint);
    build(name).unwrap_or_else(|| Arc::new(IdeEndpoint::new()))
}

/// Kiro 端点
///
/// 同一个 `KiroProvider` 可持有多个 endpoint 实现，按凭据级字段切换。
pub trait KiroEndpoint: Send + Sync {
    /// 端点名称（对应 credentials.endpoint / config.defaultEndpoint 的取值）
    fn name(&self) -> &'static str;

    /// API endpoint URL
    fn api_url(&self, ctx: &RequestContext<'_>) -> String;

    /// API 请求的 `content-type`（默认 `application/json`）。
    ///
    /// CLI 端点（Amazon Q CLI 协议）**必须**用 `application/x-amz-json-1.0`：否则上游把请求
    /// 当普通 REST 而非 X-Amz-Target 路由，返回 `UnknownOperationException` 的 JSON（非
    /// event-stream），下游解码器读到非法帧长直接中断（实测 502「消息长度超限」）。
    fn content_type(&self) -> &'static str {
        "application/json"
    }

    /// MCP endpoint URL
    fn mcp_url(&self, ctx: &RequestContext<'_>) -> String;

    /// 装饰 API 请求的端点特有 header
    ///
    /// Provider 已经设置好 URL、content-type、Connection 和 body；
    /// 实现负责追加 Authorization、host、user-agent 等端点相关头。
    fn decorate_api(&self, req: RequestBuilder, ctx: &RequestContext<'_>) -> RequestBuilder;

    /// 装饰 MCP 请求的端点特有 header
    fn decorate_mcp(&self, req: RequestBuilder, ctx: &RequestContext<'_>) -> RequestBuilder;

    /// 对已序列化的 API 请求体做端点特有加工（如注入 profileArn）
    fn transform_api_body(&self, body: &str, ctx: &RequestContext<'_>) -> String;

    /// 对已序列化的 MCP 请求体做端点特有加工（默认不变）
    fn transform_mcp_body(&self, body: &str, _ctx: &RequestContext<'_>) -> String {
        body.to_string()
    }

    /// 该端点使用的 `X-Amz-Target` 头值；不走 X-Amz-Target 路由的端点（如 IDE）返回 `None`。
    ///
    /// 与 [`Self::bucket_id`] 配套：同 host 同 target 才算同一个上游限流桶。
    fn amz_target(&self) -> Option<&'static str> {
        None
    }

    /// 该端点此请求**实际命中**的上游限流桶标识。
    ///
    /// 🔴 deepseek review 修复：provider 的 `endpoint_buckets` 目前以 `(credential_id,
    /// endpoint_name)` 为 key —— 这对「非 us-east-1 的 codewhisperer 与 cli 同构」不成立：
    /// 两者 host 都回退 `q.{region}.amazonaws.com`、`X-Amz-Target` 也相同，是**同一个**上游
    /// 桶，却被按两个桶记。后果：codewhisperer 桶被 429 封后 `select_endpoint` 换到 cli 桶，
    /// 打回同一个上游 host，又 429 → 两个"桶"都被封但被当成两个独立桶，`has_unthrottled_endpoint`
    /// 误判"还有可用桶"而持续轰炸。另外 key 缺 region 维度，同一凭据跨区会被错误合并。
    ///
    /// 本方法返回的标识由**解析后的 host（含 region）+ X-Amz-Target** 共同界定：
    /// - codewhisperer 非 us-east-1 回退 q.* → 与 cli 同桶（同构端点去重）；✓
    /// - codewhisperer us-east-1 走 codewhisperer.* → 与 cli 分桶；✓
    /// - cli 的 us-east-1 与 eu-central-1 → 不同 host → 分桶（region 维度天然在 host 里）；✓
    /// - amazonq 同 q.* host 但 target 是 `SendMessage` → 与 cli 分桶（不同操作）。✓
    ///
    /// **provider 侧配合**：`endpoint_buckets` 的 key 应改为 `(credential_id, bucket_id)`，
    /// 即把 `name.to_string()` 换成 `endpoint.bucket_id(ctx)`。`select_endpoint` 处暂无完整
    /// `RequestContext`（缺 token/machine_id），需在 429 封桶写入点（provider.rs 两处
    /// `endpoint_buckets.lock().insert`）按当时持有的 ctx 计算，并把 select 侧读取键同步。
    fn bucket_id(&self, ctx: &RequestContext<'_>) -> String {
        match self.amz_target() {
            Some(t) => format!("{}|{}", self.api_url(ctx), t),
            None => self.api_url(ctx),
        }
    }

    /// 与 [`Self::bucket_id`] **等价**，但只需 `credentials` + `config` —— 不需要
    /// token / machine_id。
    ///
    /// # 为什么要这个变体（它解决了 bucket_id 一直没接线的根因）
    ///
    /// `bucket_id` 落成死代码的唯一原因是「`select_endpoint` 处拿不到完整
    /// `RequestContext`（缺 token/machine_id）」。但实测**全部 5 个端点的 `api_url`
    /// 只依赖 `api_region`**（它由 profileArn 第 4 段 / 凭据 region / config 推出），
    /// 没有任何一个读 `ctx.token` 或 `ctx.machine_id`：
    /// - `cli`        → `q.{region}.amazonaws.com/`
    /// - `cli-runtime`→ `runtime.{region}.kiro.dev/`
    /// - `ide`        → `runtime.{region}.kiro.dev/generateAssistantResponse`
    /// - `codewhisperer` / `amazonq` → `{host(ctx)}/`（host 同样只看 region）
    ///
    /// ⇒ 那个「拿不到 ctx」的障碍其实是假的：用占位 token/machine_id 构造一个临时
    /// ctx 就能算出**与真实请求逐字节相同**的 bucket_id。本方法把这件事显式化，
    /// 避免调用方各自伪造 ctx（那样一旦将来某端点真的在 api_url 里用了 token，
    /// 伪造点会散落各处而无法收口）。
    ///
    /// ⚠️ **不变量**：任何端点实现都**不得**在 `api_url` 里使用 `ctx.token` 或
    /// `ctx.machine_id`。若将来必须用，则本方法失效，必须改为在请求侧算桶键并
    /// 把它透传到 select 侧。有守卫测试钉死这条（见本文件 tests 的
    /// `bucket_key_must_not_depend_on_token_or_machine_id`）。
    fn bucket_key(&self, credentials: &KiroCredentials, config: &Config) -> String {
        // 占位值刻意用可识别的字符串而不是空串：一旦哪天真的被写进 URL，
        // 线上日志里会直接出现 `PLACEHOLDER`，比空串更容易定位。
        let ctx = RequestContext {
            credentials,
            token: "BUCKET_KEY_PLACEHOLDER_TOKEN",
            machine_id: "BUCKET_KEY_PLACEHOLDER_MACHINE_ID",
            config,
            is_1m: false,
        };
        self.bucket_id(&ctx)
    }

    /// 判断响应体是否表示"月度配额用尽"（禁用凭据并转移）
    fn is_monthly_request_limit(&self, body: &str) -> bool {
        default_is_monthly_request_limit(body)
    }

    /// 判断响应体是否表示"上游 bearer token 失效"（触发强制刷新）
    fn is_bearer_token_invalid(&self, body: &str) -> bool {
        default_is_bearer_token_invalid(body)
    }

    /// 判断响应体是否表示「订阅不覆盖本应用/模型」——永久条件，换区与重试都无效。
    ///
    /// 判据与理由见 [`default_is_subscription_unsupported`]。
    fn is_subscription_unsupported(&self, body: &str) -> bool {
        default_is_subscription_unsupported(body)
    }

    /// 判断响应体是否表示"账户被暂停/封禁"（直接禁用，不自动恢复）
    fn is_account_suspended(&self, body: &str) -> bool {
        default_is_account_suspended(body)
    }

    /// 判断响应体是否表示"账户级临时风控限速"（非永久封禁）
    ///
    /// 上游对高频/可疑活动会返回带 `suspicious activity` + `temporary` 信号的响应，
    /// 这类是**临时限速**而非永久封号。必须在 [`is_account_suspended`] 之前判定，
    /// 否则含 "account has been suspended ... suspicious activity" 的临时限速文案
    /// 会被误判成永久封禁、白冻一个还能用的号 86400 秒。命中时只设短冷却 + failover。
    fn is_temporary_rate_limit(&self, body: &str) -> bool {
        default_is_temporary_rate_limit(body)
    }

    /// 判断响应体是否表示"客户端请求校验错误"（重试/换号都无意义，立即终止）
    ///
    /// 典型如 `TOOL_USE_RESULT_MISMATCH`：多轮工具结果与上文不匹配，是请求构造
    /// 问题，换号重试只会重复失败并浪费配额。
    fn is_client_validation_error(&self, body: &str) -> bool {
        default_is_client_validation_error(body)
    }

    /// 判断响应体是否表示"模型暂时不可用"（503 MODEL_TEMPORARILY_UNAVAILABLE）。
    ///
    /// 这是**全局容量**问题（模型实例过载或预热中），**非**凭据级问题——所有凭据对
    /// 同一模型都会同等受影响。命中时应：使用慢速退避重试（1s base），且**不**调用
    /// `report_failure` / `report_rate_limited_with_retry_after`，避免健康分被无辜拖低。
    fn is_model_temporarily_unavailable(&self, body: &str) -> bool {
        default_is_model_temporarily_unavailable(body)
    }

    /// 判断响应体是否表示"该凭据不能服务此模型"（`INVALID_MODEL_ID`）。
    ///
    /// 典型成因：该号的订阅被上游取消/降级，原本能用的模型（如 opus）不再对它开放，
    /// 上游返回 `400 INVALID_MODEL_ID`。这**不是**客户端请求错误——换一个订阅仍有效的
    /// 号往往能成功。因此命中时应：给该号冷却 + failover 到别的号（而非直接把 400 透传给
    /// 客户端、坏号还留在轮转里反复命中）。若**所有**号都返回它，才是模型本身无效，透传。
    fn is_invalid_model_id(&self, body: &str) -> bool {
        default_is_invalid_model_id(body)
    }

    /// 是否为「该 region 的 profile 未开通」错误(403 FEATURE_NOT_SUPPORTED)。
    ///
    /// external_idp 号在某些 region 有 profile 但未开通 Kiro,对话打过去即返回此错。对话路径据此
    /// 触发 region 自动纠正(本地纠正 + 后台异步重探),而非当普通凭据错误冷却/换号。
    fn is_feature_not_supported(&self, body: &str) -> bool {
        default_is_feature_not_supported(body)
    }

    /// 从错误响应中提取上游给出的重置时间（秒）
    ///
    /// 某些上游把真实重置时间放在 body 里（如 `resets_in_seconds` / `resets_at` epoch），
    /// 而非 `Retry-After` 头。有则据此设定精确冷却，避免盲目退避浪费。
    fn extract_retry_after_secs(&self, body: &str) -> Option<u64> {
        default_extract_retry_after_secs(body)
    }
}

/// 默认的 FEATURE_NOT_SUPPORTED 判断逻辑（与 `classify_profile_probe` 同口径:子串命中即真）。
pub fn default_is_feature_not_supported(body: &str) -> bool {
    body.contains("FEATURE_NOT_SUPPORTED")
}

/// 默认的 INVALID_MODEL_ID 判断逻辑（识别顶层 `reason` 与嵌套 `error.reason`）。
pub fn default_is_invalid_model_id(body: &str) -> bool {
    if body.contains("INVALID_MODEL_ID") {
        return true;
    }
    let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else {
        return false;
    };
    if value
        .get("reason")
        .and_then(|v| v.as_str())
        .is_some_and(|v| v == "INVALID_MODEL_ID")
    {
        return true;
    }
    value
        .pointer("/error/reason")
        .and_then(|v| v.as_str())
        .is_some_and(|v| v == "INVALID_MODEL_ID")
}

/// 装饰请求时可用的上下文
///
/// 包含单次调用已确定的所有运行时信息。引用形式避免无谓 clone。
pub struct RequestContext<'a> {
    /// 当前凭据
    pub credentials: &'a KiroCredentials,
    /// 有效的 access token（API Key 凭据下即 kiroApiKey）
    pub token: &'a str,
    /// 当前凭据对应的 machineId
    pub machine_id: &'a str,
    /// 全局配置
    pub config: &'a Config,
    /// 本次请求是否命中受支持的 1M 上下文变体(`claude-xxx[1m]`)。
    /// 为 true 时 [`super::endpoint`] 的 `decorate_api` 注入 `anthropic-beta: context-1m-2025-08-07`。
    /// 由 handler 用 [`crate::anthropic::model_catalog::resolve_is_1m`] 从原始模型名算出、透传到此。
    pub is_1m: bool,
}

/// 上游表示"额度用尽"的 `reason` 取值。
///
/// - `MONTHLY_REQUEST_COUNT`：免费/订阅额度的月度请求数用尽。
/// - `OVERAGE_REQUEST_LIMIT_EXCEEDED`：**按量付费(overage)的上限也用尽**。
///
/// 🔴 修复的缺陷（线上 8 小时日志确证）：此前只认 `MONTHLY_REQUEST_COUNT`，
/// 而生产环境最高频的错误恰恰是 overage 那一个 —— 594 次
/// `{"message":"You have reached the limit for overages.","reason":"OVERAGE_REQUEST_LIMIT_EXCEEDED"}`。
/// 它不被识别为额度耗尽，于是走通用可重试分支、**只冷却 1.5s** 就再撞一次。
///
/// 后果是一个活锁：额度已尽的号被反复轰炸 → 上游风控判定异常
/// → 403 `TEMPORARILY_SUSPENDED`（68 次）→ 连续 6 次判死号自动禁用
/// → 自愈把它复活（193 次）→ 回到第一步。号池因此永远稳不下来。
///
/// 归类为额度耗尽后走 `QuotaExceeded`，而它**刻意不在自愈白名单里**
/// （见 `is_self_healable_reason`）—— 额度要等下个计费周期或人工提额，
/// 复活只会继续撞墙并招来风控。这正是打断活锁的关键。
const QUOTA_EXHAUSTED_REASONS: &[&str] =
    &["MONTHLY_REQUEST_COUNT", "OVERAGE_REQUEST_LIMIT_EXCEEDED"];

/// 默认的"额度用尽"判断逻辑（月度额度 + 按量付费上限）
///
/// 同时识别顶层 `reason` 字段和嵌套 `error.reason` 字段。
pub fn default_is_monthly_request_limit(body: &str) -> bool {
    // 子串兜底：部分响应不是合法 JSON（截断/包裹），先按原文匹配。
    if QUOTA_EXHAUSTED_REASONS.iter().any(|r| body.contains(r)) {
        return true;
    }

    let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else {
        return false;
    };

    let matches_reason = |v: Option<&serde_json::Value>| {
        v.and_then(|v| v.as_str())
            .is_some_and(|s| QUOTA_EXHAUSTED_REASONS.contains(&s))
    };

    matches_reason(value.get("reason")) || matches_reason(value.pointer("/error/reason"))
}

/// 默认的 bearer token 失效判断逻辑
pub fn default_is_bearer_token_invalid(body: &str) -> bool {
    body.contains("The bearer token included in the request is invalid")
}

/// 订阅不覆盖本应用/模型 —— **永久条件，换区与重试都无效**。
///
/// # 为什么需要单独一条判据（2026-08-07 实测定案）
///
/// 同一把 `ksk_` key 打两个区的数据面，403 的**文案完全不同**：
///
/// ```text
/// q.us-east-1     403 "The bearer token included in the request is invalid."
/// q.eu-central-1  403 "Your subscription does not support this application. ..."
/// ```
///
/// 前者是「该区未授权」，已有 [`default_is_bearer_token_invalid`] 认它，并由
/// `provider.rs` 的 L1 换区自纠正接住 —— 那条路径是对的。
///
/// 后者在本判据加入之前**全仓零命中**，于是落进通用 403 处置。两个后果都是实测过的：
/// ① L1 会尝试**换区重试** —— 而订阅不是按区划分的，换区必然拿到同一个 403，
///    白烧一次上游往返（同一把 key 在两个区分别验证过：换区不改变这个错误）；
/// ② 它是**永久**条件，却被当成可重试的失败，于是吃掉重试预算
///    （`ABSOLUTE_MAX_TOTAL_RETRIES` 与吸收轮次），对一个注定失败的请求反复打上游。
///
/// 判据只认 `subscription does not support`（大小写不敏感），刻意不认裸的
/// `subscription` —— 那个词在额度/计费类文案里也出现，会误伤。
///
/// ⚠️ 位置承重：调用方必须把它排在 [`default_is_bearer_token_invalid`] 之后。
/// 两者是**互斥的两种 403**，但只有本条是「换区也没用」；顺序颠倒不会误判
/// （文案不重叠），排在后面是为了让「该区未授权」这个更常见的分支先命中、
/// 保持既有 L1 行为一字不变。
pub fn default_is_subscription_unsupported(body: &str) -> bool {
    body.to_ascii_lowercase()
        .contains("subscription does not support")
}

/// 默认的账户暂停/封禁判断逻辑
///
/// 参考 Kiro-Go `account_failover.go` 的错误分类经验：
/// 识别上游明确的 suspend/ban/disable 信号（大小写不敏感），
/// 命中即视为不可自动恢复，应直接禁用凭据等待人工处理。
pub fn default_is_account_suspended(body: &str) -> bool {
    let lower = body.to_ascii_lowercase();
    // 明确的封禁/暂停/停用信号
    //
    // ⚠️ `temporarily_suspended` **刻意不在此表内**（曾在，造成生产事故）：
    // 上游 `AccessDeniedException` 的真实 body 是
    //   {"message":"Your User ID is temporarily suspended. We detected unusual user
    //    activity and locked it as a security precaution...","reason":"TEMPORARILY_SUSPENDED"}
    // 上游明确说 temporarily 且附申诉链接，是**临时**风控态。此前它命中本表 →
    // 按永久封禁处理（disabled=true + failure_count=MAX），而 is_temporary_rate_limit
    // 又因 SUSPICIOUS_SIGNALS 少一个 "unusual user activity" 变体而漏判 → 号被永久禁用。
    // 实测后果：12 小时 88 次 suspend 禁用 + 51 次「所有凭据已用尽」+ 36 次全池自愈活锁，
    // 逐小时拒绝率一路升到 100%。该形态现由 default_is_temporary_rate_limit 接管。
    const SUSPEND_KEYWORDS: &[&str] = &[
        "account suspended",
        "account_suspended",
        "account has been suspended",
        "account disabled",
        "account_disabled",
        "account is disabled",
        "permanently banned",
        "has been banned",
    ];
    SUSPEND_KEYWORDS.iter().any(|kw| lower.contains(kw))
}

/// 默认的"账户级临时风控限速"判断逻辑（v4-2.1）
///
/// 判据：body 同时命中「可疑活动信号」**且**「临时/限速信号」。两者都要求，
/// 才不会把真正的永久封禁误判成临时限速——这是防误冻的关键边界。
///
/// 上游对触发风控的高频账户常返回类似
/// `"...suspicious activity... temporary rate limits applied..."` 的文案，
/// 这类应只设短冷却 + 立即 failover，绝不当永久封禁冻 24 小时。
pub fn default_is_temporary_rate_limit(body: &str) -> bool {
    let lower = body.to_ascii_lowercase();

    // 🔴 **显式 TEMPORARILY_SUSPENDED 单独一条，不参与下面的 `&&`**（2026-08-04 修）。
    //
    // 实测漏判形态（线上 19:09/19:12 连续两次把一个只是被临时限速的号判死）：
    // ```
    // 403 {"__type":"com.amazon.aws.codewhisperer#AccessDeniedException",
    //      "message":"Your User ID (1866...) temporarily is suspended.
    //                 We've locked your account as a security precaution.
    //                 To restore access, please contact our support team..."}
    // ```
    // 两个条件**都**不命中：
    // - `has_temporary` 失败 —— 信号表里是 `"temporarily suspended"`（两词相邻），
    //   而上游写的是 `temporarily **is** suspended`，中间多一个 `is` 就漏。
    //   这与本函数下方注释警告的 `unusual **user** activity` 是**同一类**错误，
    //   那次修了 unusual 一族，这条漏了。
    // - `has_suspicious` 失败 —— 这个变体说的是 "locked your account as a security
    //   precaution"，既无 `suspicious activity` 也无 `unusual`+`activity`。
    //
    // 后果链（这才是它值得单列的原因）：漏判 → 落 `report_failure` →
    // 3 次即以 `TooManyFailures`（**永久型**标签）禁用并落盘 → 号池从 2 个掉到 1 个
    // → 剩下那个号吃下全部流量 → 撞进它自己的惩罚窗口 → 429 风暴
    // （实测 19:16–19:21：单号 152 请求/分钟、144 个 429）。
    // 即「一个号被临时限速」被放大成「整池 429 风暴」。
    //
    // 为什么可以脱离 `&&` 单独成立：`temporarily` 这个词本身就是上游对**临时性**的
    // 明确声明，永久封禁的文案不会用它（见 `SUSPEND_KEYWORDS` 那边的说明）。
    // 判据与 `anthropic::handlers::is_upstream_temporarily_suspended` 对齐 ——
    // 那侧一直认 `"temporarily is suspended"`，所以**客户端**拿到的是正确的 429，
    // 而本侧漏判让**凭据**被算成失败：同一个 403，两处结论相反。
    if lower.contains("temporarily is suspended")
        || lower.contains("temporarily suspended")
        || lower.contains("temporarily_suspended")
    {
        return true;
    }

    // 可疑活动信号（风控触发的标志）
    //
    // ⚠️ 不要退回成 `"unusual activity"` 这类整段字面量：生产实际文案是
    // "We detected unusual **user** activity"，中间多一个词就整条漏判 —— 这正是
    // 上游说「临时」而我们按「永久」处理、进而引发禁用/自愈活锁的直接原因。
    // 故 unusual 一族改为 `unusual` + `activity` 两词分别匹配，
    // 顺带覆盖 unusual login activity / unusual account activity 等未知变体。
    const SUSPICIOUS_PHRASES: &[&str] = &["suspicious activity"];
    let has_suspicious = SUSPICIOUS_PHRASES.iter().any(|kw| lower.contains(kw))
        || (lower.contains("unusual") && lower.contains("activity"));

    // 临时/限速信号（表明是限速而非永久封）
    const TEMPORARY_SIGNALS: &[&str] = &[
        "temporary limits",
        "temporary limit",
        "temporary rate",
        "temporarily limited",
        "temporarily rate",
        "rate limits applied",
        "rate limit applied",
        // 上游 TEMPORARILY_SUSPENDED 的两种书写：JSON 的 reason 字段用下划线，
        // message 正文用空格（"Your User ID is temporarily suspended"）。
        // 二者都是**临时**态（上游原文 temporarily + 附申诉链接），
        // 故归入临时信号而非 SUSPEND_KEYWORDS，详见后者的说明。
        "temporarily suspended",
        "temporarily_suspended",
    ];
    let has_temporary = TEMPORARY_SIGNALS.iter().any(|kw| lower.contains(kw));

    has_suspicious && has_temporary
}

/// 默认的"客户端请求校验错误"判断逻辑（v4-2.2）
///
/// 命中即视为请求构造问题：立即终止，换号/重试都无意义。
pub fn default_is_client_validation_error(body: &str) -> bool {
    body.contains("TOOL_USE_RESULT_MISMATCH")
}

/// 图片声明的 `media_type` 与实际字节格式不符（400 `IMAGE_MIME_MISMATCH`）。
///
/// 上游原文（用户线上实测，逐字）：
/// ```text
/// 非流式 API 请求失败: 400 Bad Request {"__type":"com.amazon.aws.codewhisperer#ValidationException",
///  "message":"messages.2.content.1.image.source.base64: The image was specified using the
///             image/png media type, but the image appears to be a image/jpeg image",
///  "reason":"IMAGE_MIME_MISMATCH"}
/// ```
///
/// # 为什么单独一条判据（它的价值是**度量**，不是处置）
///
/// 处置上它与通用 400 同类：请求构造问题，换号/重试都无意义。真正的理由是可观测性 ——
/// `converter.rs` 的 `resolve_image_format` 已按 magic bytes 校正客户端声明的
/// `media_type`（声明 png 而字节是 jpeg 时改写成 jpeg）。但那条修复**没有效果度量**：
/// 若仍有边缘情况漏掉（magic 认不出而回退声明值、上游对同一格式有更细的子类判断等），
/// 那些 400 会与所有其它 400（工具参数错、上下文超限、请求体畸形）混进同一个
/// `bad_request` 桶，面板上分辨不出来 ⇒ **无法回答「那条修干净了没有」**。
///
/// 与 `INSUFFICIENT_MODEL_CAPACITY` / `OVERAGE_REQUEST_LIMIT_EXCEEDED` /
/// `MONTHLY_REQUEST_COUNT` / `TEMPORARILY_SUSPENDED` 都各有专门分支同理：本仓的既有
/// 惯例是**给每种已确证的 reason 码一条判据**，让它在日志/面板上可数。
///
/// # 判据为何只认 reason 字面量
///
/// 不认 `ValidationException`：那个 `__type` 被上游多种校验错误共用
/// （`TOOL_USE_RESULT_MISMATCH` 一类也是它），泛匹配会把处置不同的错误混成一类。
/// 也不认 message 里的 `media type` 散文：那句话的措辞随格式组合变化
/// （`image/png` ↔ `image/jpeg` 可任意互换），按散文匹配等于赌上游文案不变。
pub fn default_is_image_mime_mismatch(body: &str) -> bool {
    body.contains("IMAGE_MIME_MISMATCH")
}

/// 请求体校验失败（`reason=REQUEST_BODY_INVALID` 或 message 含 `Invalid tool use format`）。
///
/// 2026-08-11 补：此前该错误码**零翻译**，落 `map_provider_error` 的「未识别兜底」
/// （502 + 通用文案，原文只进日志）。它是**请求构造**问题（工具配对/role/字段合法性），
/// 重试或换号无意义，应翻成 400 `invalid_request_error` 让客户端不重试、按排障修请求。
///
/// # 判据为什么同时认 reason 与 message 散文
///
/// 线上实测同一条 400 里两个信号并存（`{"message":"Invalid tool use format.",
/// "reason":"REQUEST_BODY_INVALID"}`，见 passthrough.rs 注释里的实测样本）——认其一
/// 漏其二；`Improperly formed request` 是同一校验的**第三种散文形态**（converter.rs /
/// websearch.rs 实测：工具 schema 属性、工具名超限、web_search 直发都会触发，常带
/// reason=REQUEST_BODY_INVALID），2026-08-11 对抗审查 M1 发现它此前混在凭据分类里被
/// 说成「订阅失效」——排障方向全错。与 `default_is_image_mime_mismatch` 同理，
/// **不认** `ValidationException`（该 `__type` 被多种校验共用，泛匹配会把处置不同的
/// 错误混成一类）。
///
/// # ⚠️ 与 region 探测的边界（不要动探测侧）
///
/// `region_probe.rs` 历史上曾用不完整 body 打 `q.*` 服务根探测 region，两个区都回
/// `400 REQUEST_BODY_INVALID`——那是探测的**预期响应**，不是用户错误。该探测现走
/// `management.*` 端点 + 独立 client + 独立错误通道（不经过 `map_provider_error`），
/// 与翻译体系**结构上隔离**，加本判据不会污染探测。但 `classify_probe_result` 的
/// 400 → Inconclusive 兜底是探测自己的判据，与翻译无关，**不要**因为本判据认了这个码
/// 就去改它（改坏它会让 region 自愈失效）。
pub fn default_is_request_body_invalid(body: &str) -> bool {
    body.contains("REQUEST_BODY_INVALID")
        || body.contains("Invalid tool use format")
        || body.contains("Improperly formed")
}

/// 默认的 MODEL_TEMPORARILY_UNAVAILABLE 判断逻辑。
///
/// 503 且 body 含该信号时表示**模型容量**问题，非凭据问题。
/// 命中时应走慢速退避，且不影响凭据健康分。
pub fn default_is_model_temporarily_unavailable(body: &str) -> bool {
    body.contains("MODEL_TEMPORARILY_UNAVAILABLE")
        || body.contains("model is temporarily unavailable")
        // ⭐ `INSUFFICIENT_MODEL_CAPACITY`：同一语义的**另一种上游形态**，实测 24h 内 272 次。
        //
        // 上游原文：
        // ```text
        // 400 Bad Request {"__type":"com.amazon.aws.codewhisperer#ThrottlingException",
        //  "message":"I am experiencing high traffic, please try again shortly.",
        //  "reason":"INSUFFICIENT_MODEL_CAPACITY"}
        // ```
        //
        // 归到本判据而不是新开一条分支，是因为它与 `MODEL_TEMPORARILY_UNAVAILABLE`
        // **处置完全相同**：全局容量问题、非凭据级、所有凭据对同一模型等价受影响 ⇒
        // 慢速退避重试且**绝不**惩罚凭据健康。新开分支必然要把那整套处置抄一遍，
        // 而两份处置一旦漂移，后果是「某种形态会拖低无辜凭据的健康分」。
        //
        // ⚠️ 判据只认 `reason` 字面量，**不认** `ThrottlingException`：后者 429 也在用
        // （`USER_REQUEST_RATE_EXCEEDED` 那类是真限流，该走冷却 + 换号），两者混判会
        // 让真限流走进"不惩罚健康"的路径 —— 那等于关掉对被限流号的调度规避。
        //
        // ⚠️ 它的 HTTP 状态是 **400**，而调用点原先写死 `status == 503`。只加本判据
        // 不放宽那道状态门 ⇒ 修复完全无效（见 `provider.rs` 该分支的说明）。
        || body.contains("INSUFFICIENT_MODEL_CAPACITY")
}

/// 默认的"从错误 body 提取重置秒数"逻辑
///
/// 优先识别相对秒数（`resets_in_seconds` / `retry_after`），
/// 其次识别绝对 epoch（`resets_at`，秒级时间戳）并换算为剩余秒数。
/// 同时兼容顶层与嵌套 `error.*` 两种位置。
pub fn default_extract_retry_after_secs(body: &str) -> Option<u64> {
    let value = serde_json::from_str::<serde_json::Value>(body).ok()?;

    // 相对秒数字段（顶层或 error.* 下）
    for key in ["resets_in_seconds", "retry_after", "retryAfter"] {
        if let Some(secs) = value
            .get(key)
            .or_else(|| value.pointer(&format!("/error/{key}")))
            .and_then(|v| v.as_u64().or_else(|| v.as_f64().map(|f| f as u64)))
        {
            return Some(secs);
        }
    }

    // 绝对 epoch（秒）字段
    for key in ["resets_at", "resetsAt"] {
        if let Some(epoch) = value
            .get(key)
            .or_else(|| value.pointer(&format!("/error/{key}")))
            .and_then(|v| v.as_i64())
        {
            let now = chrono::Utc::now().timestamp();
            if epoch > now {
                return Some((epoch - now) as u64);
            }
        }
    }

    None
}

#[cfg(test)]
mod subscription_unsupported_tests {
    use super::*;

    /// 2026-08-07 实测原文：同一把 `ksk_` key 打 `q.eu-central-1` 的数据面所得。
    /// 该区**已授权**（同一把 key 在 `management.eu-central-1` 拿 200，
    /// 在 `management.us-east-1` 拿 403 `Invalid token`），所以这个 403 不是区的问题。
    const REAL_SUBSCRIPTION_BODY: &str = r#"{"__type":"com.amazon.aws.codewhisperer#AccessDeniedException","message":"Your subscription does not support this application. Please contact your administrator."}"#;

    /// 同一把 key 打 `q.us-east-1`（**未授权区**）所得 —— 对照组。
    const REAL_WRONG_REGION_BODY: &str = r#"{"__type":"com.amazon.kiro.runtimeservice#AccessDeniedException","message":"The bearer token included in the request is invalid."}"#;

    /// ⭐ 承重：订阅文案必须被识别。
    ///
    /// 回退即 FAIL：删掉 [`default_is_subscription_unsupported`] 或改窄它的判据 ——
    /// 本条变红。该文案在本判据加入前**全仓零命中**，于是落进通用 403 处置：
    /// 既被 L1 拿去换区（订阅不按区划分，换了还是同一个错），又吃掉重试预算
    /// （永久条件被当成可重试）。
    #[test]
    fn real_subscription_body_is_recognized() {
        assert!(
            default_is_subscription_unsupported(REAL_SUBSCRIPTION_BODY),
            "上游订阅不支持的实测文案必须被识别，否则永久失败会被当成可重试"
        );
    }

    /// ⭐ 承重：两种 403 必须**互斥**，绝不能互相误判。
    ///
    /// 这是本改动最容易出错的地方：若订阅判据把「该区未授权」也吃进去，
    /// L1 换区自纠正就再也不会触发 —— 而那条路径有实测支撑
    /// （4 个号累计 3393 次成功、42 次瞬态 403），破坏它比不加本判据更糟。
    #[test]
    fn two_403_kinds_never_cross_match() {
        assert!(
            !default_is_subscription_unsupported(REAL_WRONG_REGION_BODY),
            "「该区未授权」绝不能被判成订阅问题——否则 L1 换区自纠正被架空"
        );
        assert!(
            !default_is_bearer_token_invalid(REAL_SUBSCRIPTION_BODY),
            "订阅问题绝不能被判成 token 失效——否则会去换区，而换区对订阅无效"
        );
    }

    /// ⭐ OVER-REACH CONTROL：判据不得宽到认裸 `subscription`。
    ///
    /// 把实现改成 `lower.contains("subscription")` —— 本条必须 FAIL。
    /// 若它仍绿，说明判据已经宽到会误伤额度/计费类文案，那类是**可重试**的，
    /// 被误判成永久终止会让本该成功的请求直接失败。
    #[test]
    fn must_not_match_bare_subscription_word() {
        for benign in [
            r#"{"message":"Your subscription quota has been exceeded, please try again later."}"#,
            r#"{"message":"subscription renewal pending"}"#,
            r#"{"message":"Free tier subscription limit reached"}"#,
        ] {
            assert!(
                !default_is_subscription_unsupported(benign),
                "判据过宽：裸 subscription 文案被误判成永久不支持: {benign}"
            );
        }
    }

    /// 大小写不敏感（上游文案大小写实测不稳定）。
    #[test]
    fn is_case_insensitive() {
        assert!(default_is_subscription_unsupported(
            "YOUR SUBSCRIPTION DOES NOT SUPPORT THIS APPLICATION"
        ));
    }
}

#[cfg(test)]
mod capacity_signature_tests {
    use super::*;

    /// ⭐ 回归：`INSUFFICIENT_MODEL_CAPACITY` 必须被判为「模型容量问题」。
    ///
    /// 上游实测原文（24h 内 272 次）：
    /// ```text
    /// 400 Bad Request {"__type":"com.amazon.aws.codewhisperer#ThrottlingException",
    ///  "message":"I am experiencing high traffic, please try again shortly.",
    ///  "reason":"INSUFFICIENT_MODEL_CAPACITY"}
    /// ```
    ///
    /// 旧判据只认 `MODEL_TEMPORARILY_UNAVAILABLE` / `model is temporarily unavailable`，
    /// 于是这 272 次逐条落空所有分支 → 走到 `map_provider_error` 末尾兜底 →
    /// **502 且无 Retry-After** → 客户端按永久故障处理、不退避、原样重发。
    ///
    /// 删掉那条 `|| body.contains("INSUFFICIENT_MODEL_CAPACITY")` → 本测试必 FAILED。
    #[test]
    fn insufficient_model_capacity_is_a_capacity_problem() {
        let real = r#"400 Bad Request {"__type":"com.amazon.aws.codewhisperer#ThrottlingException","message":"I am experiencing high traffic, please try again shortly.","reason":"INSUFFICIENT_MODEL_CAPACITY"}"#;
        assert!(
            default_is_model_temporarily_unavailable(real),
            "上游实测原文必须被判为容量问题（否则落兜底 502，客户端不退避）"
        );
        // 既有两种形态不得因本次改动失效。
        assert!(default_is_model_temporarily_unavailable(
            r#"503 {"reason":"MODEL_TEMPORARILY_UNAVAILABLE"}"#
        ));
        assert!(default_is_model_temporarily_unavailable(
            "the model is temporarily unavailable"
        ));
    }

    /// ⭐ 承重反向守卫：**真限流不得**被判成容量问题。
    ///
    /// `ThrottlingException` 这个 `__type` 被两类错误共用 —— 真限流
    /// （`USER_REQUEST_RATE_EXCEEDED`，该走冷却 + 换号）与容量不足
    /// （`INSUFFICIENT_MODEL_CAPACITY`，不该惩罚凭据）。
    ///
    /// 若把判据放宽到认 `ThrottlingException`，真限流会走进「不惩罚凭据健康」的路径 ⇒
    /// 等于**关掉了对被限流号的调度规避**：那个号会被反复选中、反复 429。
    /// 所以判据只能认 `reason` 字面量。
    ///
    /// 把判据改成 `body.contains("ThrottlingException")` → 本测试必 FAILED。
    #[test]
    fn real_rate_limit_must_not_be_mistaken_for_capacity() {
        let rate_limited = r#"429 Too Many Requests {"__type":"com.amazon.kiro.runtimeservice#ThrottlingException","message":"Too many requests, please wait before trying again.","reason":"USER_REQUEST_RATE_EXCEEDED"}"#;
        assert!(
            !default_is_model_temporarily_unavailable(rate_limited),
            "真限流不得判成容量问题 —— 那会让被限流的号不被冷却、反复被选中"
        );
        // 裸 ThrottlingException（无 reason）同样不认：无法判定是哪一类，按未知处理更安全。
        assert!(!default_is_model_temporarily_unavailable(
            r#"{"__type":"ThrottlingException"}"#
        ));
    }

    /// ⭐ 源码级守卫：provider 的容量分支状态门必须同时收 503 与 400。
    ///
    /// 判据改对了但状态门仍写死 `== 503`，则修复**完全无效**（400 那种形态永远进不来）。
    /// 这是本修复里最容易漏的一处 —— 判据在 endpoint 层、状态门在 provider 层，
    /// 改一处看不到另一处。
    ///
    /// 锚点选**代码**并切掉注释行：本仓踩过五次「needle 命中注释里的散文」。
    #[test]
    fn provider_capacity_gate_accepts_both_statuses() {
        let src = include_str!("../provider.rs");
        let prod: String = src
            .split("#[cfg(test)]")
            .next()
            .expect("生产段应存在")
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        // needle 运行时拼接，避免 include_str! 自匹配。
        let gate = ["is_model_temporarily_unavailable", "(&body)"].concat();
        // 状态门必须收 503 与 400 两个码。取**最后一处**调用点（真正的容量处置分支）
        // 之前的窗口 —— 前面还有一处只用于"别被通用 400 吃掉"的预判（见下方顺序断言）。
        let last = prod.rfind(gate.as_str()).expect("容量判据调用点不应被改名");
        let window = &prod[last.saturating_sub(400)..last];
        assert!(
            window.contains("503"),
            "状态门必须仍收 503（既有形态 MODEL_TEMPORARILY_UNAVAILABLE）"
        );
        assert!(
            window.contains("400"),
            "状态门必须同时收 400 —— INSUFFICIENT_MODEL_CAPACITY 的 HTTP 状态是 400"
        );

        // ⭐⭐ 承重：**容量判定必须排在通用 400 分支之前**。
        //
        // 这是上一版守卫**没抓住**的缺陷，代价是修复完全无效地上线了：
        // endpoint 判据改对、状态门改对、handlers 映射改对、四条测试全绿，
        // 但通用 400 分支（`if status.as_u16() == 400 { … break }`）排在容量分支
        // **之前 178 行**，先接住所有 400 并 break ⇒ 容量分支永远走不到。
        //
        // 实测：修复上线后（19:05:15）逐分钟仍全部落 `bad_request`
        // （19:19 / 19:21 / …… / 19:45），近 6h 共 590 次。
        //
        // 上一版守卫只断言"状态门里有 400 和 503"，那是**分支内部**的形状，
        // 与**分支之间的顺序**无关 —— 所以它对这个缺陷完全不可见。这条补上顺序。
        let generic400 = ["if status.as_u16() == 400 &&", " !is_capacity_400"].concat();
        let gi = prod.find(generic400.as_str()).unwrap_or_else(|| {
            panic!(
                "通用 400 分支必须显式排除容量 400（形如 `{generic400}`）——\
                 否则它会先 break，让容量分支永远走不到"
            )
        });
        let first_capacity = prod.find(gate.as_str()).expect("容量判据调用点不应被改名");
        assert!(
            first_capacity < gi,
            "容量判定必须在通用 400 分支**之前**求值。实测顺序错时 590 次/6h 全部落 \
             bad_request 且客户端拿 502 不退避，而三处判据都已改对、测试全绿 —— \
             因为它们测的是纯函数，看不见分支顺序。"
        );
    }
}

#[cfg(test)]
mod registry_tests {
    use super::*;
    use crate::kiro::model::credentials::KiroCredentials;

    #[test]
    fn should_register_both_known_endpoints() {
        let reg = registry();
        assert!(reg.contains_key(ide::IDE_ENDPOINT_NAME));
        assert!(reg.contains_key(cli::CLI_ENDPOINT_NAME));
        assert_eq!(reg.len(), ENDPOINT_NAMES.len(), "每个已知名字都应注册成功");
        // 注册表的键必须与实现自报的 name() 一致，否则 endpoint_for 查不到自己注册的端点。
        for (name, ep) in &reg {
            assert_eq!(name, ep.name(), "注册键与 name() 必须一致");
            // 反向：name() 必须也在已知名字表里（防"实现改了名字但常量表没同步"）。
            assert!(
                ENDPOINT_NAMES.contains(&ep.name()),
                "端点 {} 未登记在 ENDPOINT_NAMES",
                ep.name()
            );
        }
    }

    #[test]
    fn should_reject_unknown_endpoint_name() {
        assert!(build("nope").is_none());
        assert!(build("").is_none());
    }

    /// ⭐ 不变量守卫：`api_url` 不得依赖 `token` / `machine_id`。
    ///
    /// `bucket_key` 用占位 token/machine_id 构造临时 ctx 来算桶键，前提正是「api_url
    /// 只看 region」。若哪天某端点在 URL 里用了 token（例如放进 query），select 侧算出的
    /// 桶键就与请求侧不同 —— **封禁写进去读不到**，`has_unthrottled_endpoint` 恒判"还有
    /// 可用桶"，于是对着一个已被 429 的上游持续轰炸。那种失效不会有编译错误、也不会有
    /// 直接的运行时报错，只表现为"换桶不生效"，极难定位。故用测试钉死。
    #[test]
    fn bucket_key_must_not_depend_on_token_or_machine_id() {
        let cfg = Config::default();
        let mut cred = KiroCredentials::default();
        cred.kiro_api_key = Some("ksk_guard".to_string());
        cred.auth_method = Some("api_key".to_string());

        for name in ENDPOINT_NAMES {
            let ep = build(name).expect("已知端点应可构造");
            // 同一凭据 + 同一 config，但 token / machine_id 取两组完全不同的值。
            let a = ep.bucket_id(&RequestContext {
                credentials: &cred,
                token: "TOKEN_A",
                machine_id: "MACHINE_A",
                config: &cfg,
                is_1m: false,
            });
            let b = ep.bucket_id(&RequestContext {
                credentials: &cred,
                token: "TOKEN_B_totally_different",
                machine_id: "MACHINE_B_totally_different",
                config: &cfg,
                is_1m: true,
            });
            assert_eq!(
                a, b,
                "端点 {name} 的 bucket_id 随 token/machine_id 变化 —— bucket_key 的前提被破坏，\
                 select 侧与请求侧的桶键会分叉，导致 429 换桶静默失效"
            );
            // 且 bucket_key（占位 ctx）必须与真实 ctx 逐字节一致。
            assert_eq!(
                ep.bucket_key(&cred, &cfg),
                a,
                "端点 {name} 的 bucket_key 与 bucket_id 不一致"
            );
        }
    }

    /// 同构端点必须同桶：非 us-east-1 时 `codewhisperer` 的 host 回退成
    /// `q.{region}.amazonaws.com`，与 `cli` 的 host + X-Amz-Target 全同 ⇒ 同一个上游桶。
    ///
    /// 这正是按端点**名字**分桶的错误所在：两者被记成两个桶，于是 cw 被封后换到 cli
    /// 打回同一个 host 又 429，而 `has_unthrottled_endpoint` 误判"还有可用桶"。
    #[test]
    fn structurally_identical_endpoints_share_one_bucket() {
        let mut cfg = Config::default();
        cfg.region = "eu-central-1".to_string();
        cfg.api_region = Some("eu-central-1".to_string());
        let mut cred = KiroCredentials::default();
        cred.kiro_api_key = Some("ksk_same_bucket".to_string());
        cred.auth_method = Some("api_key".to_string());

        let cli_ep = build(cli::CLI_ENDPOINT_NAME).unwrap();
        let cw_ep = build(codewhisperer::CODEWHISPERER_ENDPOINT_NAME).unwrap();
        assert_eq!(
            cli_ep.bucket_key(&cred, &cfg),
            cw_ep.bucket_key(&cred, &cfg),
            "非 us-east-1 时 codewhisperer 回退 q.* ⇒ 必须与 cli 同桶"
        );

        // 而 us-east-1 时 codewhisperer 走独占 host ⇒ 必须分桶。
        let mut cfg_use1 = Config::default();
        cfg_use1.region = "us-east-1".to_string();
        cfg_use1.api_region = Some("us-east-1".to_string());
        assert_ne!(
            cli_ep.bucket_key(&cred, &cfg_use1),
            cw_ep.bucket_key(&cred, &cfg_use1),
            "us-east-1 时 codewhisperer 走独占 host ⇒ 应与 cli 分桶"
        );
    }

    /// 同名端点、不同 region ⇒ 必须是**不同**桶（region 维度天然在 host 里）。
    /// 按名字分桶会把两个区合并，一个区被封连带把另一个区判死，白丢可用容量。
    #[test]
    fn same_endpoint_different_region_are_distinct_buckets() {
        let mut cred = KiroCredentials::default();
        cred.kiro_api_key = Some("ksk_region".to_string());
        cred.auth_method = Some("api_key".to_string());
        let ep = build(cli::CLI_ENDPOINT_NAME).unwrap();

        let mut c1 = Config::default();
        c1.api_region = Some("us-east-1".to_string());
        let mut c2 = Config::default();
        c2.api_region = Some("eu-central-1".to_string());

        assert_ne!(
            ep.bucket_key(&cred, &c1),
            ep.bucket_key(&cred, &c2),
            "同端点不同 region 是两个独立上游桶，不得合并"
        );
    }

    /// 旁路（验活/探测）的端点解析必须与 provider 同口径：ksk_ 号拿到 CLI 实现。
    /// 这是"验活把健康 ksk_ 号打成死号"那条缺陷的守卫。
    #[test]
    fn should_resolve_cli_endpoint_for_api_key_credentials() {
        let mut ak = KiroCredentials::default();
        ak.kiro_api_key = Some("ksk_test".to_string());
        assert_eq!(for_credentials(&ak, "ide").name(), cli::CLI_ENDPOINT_NAME);

        let social = KiroCredentials::default();
        assert_eq!(
            for_credentials(&social, "ide").name(),
            ide::IDE_ENDPOINT_NAME
        );
    }

    /// 端点名无法识别时旁路回退 IDE 而非 panic/失败（职责是验活，不是做门禁）。
    #[test]
    fn should_fall_back_to_ide_when_endpoint_name_unknown() {
        let mut bad = KiroCredentials::default();
        bad.endpoint = Some("typo".to_string());
        assert_eq!(for_credentials(&bad, "ide").name(), ide::IDE_ENDPOINT_NAME);
    }

    /// CLI 与 IDE 的 content-type 必须不同：CLI 走 X-Amz-Target 路由，用错会拿到
    /// UnknownOperationException 的 JSON（非 event-stream）→ 解码器读到非法帧长而中断。
    #[test]
    fn should_use_amz_json_content_type_only_for_cli() {
        assert_eq!(
            build(cli::CLI_ENDPOINT_NAME).unwrap().content_type(),
            "application/x-amz-json-1.0"
        );
        assert_eq!(
            build(ide::IDE_ENDPOINT_NAME).unwrap().content_type(),
            "application/json"
        );
    }
}

/// 端点桶标识（deepseek review 修复）守卫。
#[cfg(test)]
mod bucket_identity_tests {
    use super::*;
    use crate::kiro::model::credentials::KiroCredentials;
    use crate::model::config::Config;

    fn ctx_from<'a>(cred: &'a KiroCredentials, config: &'a Config) -> RequestContext<'a> {
        RequestContext {
            credentials: cred,
            token: "ksk_test",
            machine_id: "mid",
            config,
            is_1m: false,
        }
    }

    fn ksk_cred(region: &str) -> KiroCredentials {
        let mut cred = KiroCredentials::default();
        cred.kiro_api_key = Some("ksk_test".to_string());
        cred.region = Some(region.to_string());
        cred
    }

    fn bucket(name: &str, ctx: &RequestContext<'_>) -> String {
        build(name).expect("已知端点").bucket_id(ctx)
    }

    /// ⭐ 承重：非 us-east-1 的 codewhisperer 与 cli **同构**（host 回退 q.*、AMZ_TARGET 相同），
    /// 必须是**同一个**上游限流桶。若 provider 用 (id, name) 当 key 会把它们记成两个桶 ——
    /// 这正是 deepseek review 指出的缺陷：codewhisperer 桶被 429 封后换到 cli 桶，打回同一个
    /// 上游 host 又 429，两个"桶"都被封却被当成独立桶而持续轰炸。
    #[test]
    fn non_us_east_1_codewhisperer_is_same_bucket_as_cli() {
        let cred = ksk_cred("eu-central-1");
        let config = Config::default();
        let ctx = ctx_from(&cred, &config);
        assert_eq!(
            bucket(cli::CLI_ENDPOINT_NAME, &ctx),
            bucket(codewhisperer::CODEWHISPERER_ENDPOINT_NAME, &ctx),
            "非 us-east-1 的 codewhisperer 回退 q.* 且 target 相同，必须与 cli 同桶"
        );
    }

    /// 桶标识必须含 region 维度：cli 的 us-east-1 与 eu-central-1 是不同 host = 不同桶。
    /// 旧 key `(id, endpoint_name)` 缺 region，同一凭据跨区会被错误合并（一区被封全区封）。
    #[test]
    fn same_endpoint_different_regions_are_different_buckets() {
        let cred_eu = ksk_cred("eu-central-1");
        let cred_use = ksk_cred("us-east-1");
        let config = Config::default();
        let ctx_eu = ctx_from(&cred_eu, &config);
        let ctx_use = ctx_from(&cred_use, &config);
        assert_ne!(
            bucket(cli::CLI_ENDPOINT_NAME, &ctx_eu),
            bucket(cli::CLI_ENDPOINT_NAME, &ctx_use),
            "不同 region 的 host 不同，必须是不同桶"
        );
    }

    /// us-east-1 的 codewhisperer 走独占主机 codewhisperer.*，与 cli 的 q.* 分桶。
    #[test]
    fn us_east_1_codewhisperer_is_separate_bucket_from_cli() {
        let cred = ksk_cred("us-east-1");
        let config = Config::default();
        let ctx = ctx_from(&cred, &config);
        assert_ne!(
            bucket(cli::CLI_ENDPOINT_NAME, &ctx),
            bucket(codewhisperer::CODEWHISPERER_ENDPOINT_NAME, &ctx),
            "us-east-1 codewhisperer 走 codewhisperer.*，必须与 cli 分桶"
        );
    }

    /// cli-runtime（runtime.*）与 cli（q.*）不同 host，分桶（既有 429 换桶机制的基础）。
    #[test]
    fn cli_runtime_is_separate_bucket_from_cli() {
        let cred = ksk_cred("us-east-1");
        let config = Config::default();
        let ctx = ctx_from(&cred, &config);
        assert_ne!(
            bucket(cli::CLI_ENDPOINT_NAME, &ctx),
            bucket(cli_runtime::CLI_RUNTIME_ENDPOINT_NAME, &ctx),
            "runtime.* 与 q.* 是不同上游桶"
        );
    }

    /// amazonq 与 cli 同 host（q.*）但 target 是 SendMessage —— 不同操作 = 不同桶。
    /// 桶标识必须含 target，否则 amazonq 会与 cli 被错误合并。
    #[test]
    fn amazonq_is_separate_bucket_from_cli_by_target() {
        let cred = ksk_cred("us-east-1");
        let config = Config::default();
        let ctx = ctx_from(&cred, &config);
        assert_ne!(
            bucket(cli::CLI_ENDPOINT_NAME, &ctx),
            bucket(amazonq::AMAZONQ_ENDPOINT_NAME, &ctx),
            "同 host 但 target 不同（SendMessage vs GenerateAssistantResponse），必须分桶"
        );
    }

    /// 四个 CLI 协议端点都必须自报 X-Amz-Target（否则默认 bucket_id 会丢 target 维度，
    /// 把 amazonq 与 cli 错误合并）；IDE 走 URL 路径寻址，target 为 None。
    #[test]
    fn cli_family_endpoints_expose_amz_target() {
        for name in [
            cli::CLI_ENDPOINT_NAME,
            cli_runtime::CLI_RUNTIME_ENDPOINT_NAME,
            codewhisperer::CODEWHISPERER_ENDPOINT_NAME,
            amazonq::AMAZONQ_ENDPOINT_NAME,
        ] {
            assert!(
                build(name).unwrap().amz_target().is_some(),
                "端点 {name} 必须自报 X-Amz-Target（否则桶标识丢失 target 维度）"
            );
        }
        assert_eq!(build(ide::IDE_ENDPOINT_NAME).unwrap().amz_target(), None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_monthly_request_limit_detects_reason() {
        let body = r#"{"message":"You have reached the limit.","reason":"MONTHLY_REQUEST_COUNT"}"#;
        assert!(default_is_monthly_request_limit(body));
    }

    #[test]
    fn test_default_feature_not_supported() {
        assert!(default_is_feature_not_supported(
            r#"{"__type":"AccessDeniedException","message":"FEATURE_NOT_SUPPORTED"}"#
        ));
        assert!(default_is_feature_not_supported(
            "403 FEATURE_NOT_SUPPORTED for region"
        ));
        // 不误命中普通错误。
        assert!(!default_is_feature_not_supported(
            r#"{"reason":"MONTHLY_REQUEST_COUNT"}"#
        ));
        assert!(!default_is_feature_not_supported("INVALID_MODEL_ID"));
    }

    #[test]
    fn test_default_monthly_request_limit_nested_reason() {
        let body = r#"{"error":{"reason":"MONTHLY_REQUEST_COUNT"}}"#;
        assert!(default_is_monthly_request_limit(body));
    }

    /// 回归（🔴 会烧号的活锁）：按量付费上限用尽必须也算"额度耗尽"。
    ///
    /// **旧代码为何 FAIL**：只认 `MONTHLY_REQUEST_COUNT`。而线上 8 小时日志里
    /// 最高频的错误是 overage 那一个（594 次），它落不进额度分支，
    /// 于是按通用可重试处理、**冷却仅 1.5s** 就再撞。
    ///
    /// **为什么严重**：额度已尽的号被持续轰炸 → 上游风控 403
    /// `TEMPORARILY_SUSPENDED`（68 次）→ 连续 6 次判死号自动禁用
    /// → 自愈复活（193 次）→ 回到轰炸。这是个自持活锁，号池永远稳不下来。
    /// 归为 `QuotaExceeded` 后不进自愈白名单，活锁才被打断。
    #[test]
    fn overage_limit_must_count_as_quota_exhausted() {
        // 生产原文（逐字取自 journalctl）
        let body = r#"{"message":"You have reached the limit for overages.","reason":"OVERAGE_REQUEST_LIMIT_EXCEEDED"}"#;
        assert!(
            default_is_monthly_request_limit(body),
            "overage 上限用尽必须判为额度耗尽，否则只冷却 1.5s 反复重试 → 招来风控 → 烧号活锁"
        );
        // 嵌套形态同样要认
        assert!(default_is_monthly_request_limit(
            r#"{"error":{"reason":"OVERAGE_REQUEST_LIMIT_EXCEEDED"}}"#
        ));
        // 非法 JSON 的子串兜底
        assert!(default_is_monthly_request_limit(
            "403 Forbidden OVERAGE_REQUEST_LIMIT_EXCEEDED (truncated"
        ));
        // 不误伤：限速与风控各有分类，绝不能被吞进额度分支
        assert!(!default_is_monthly_request_limit(
            r#"{"message":"Too many requests, please wait before trying again."}"#
        ));
        assert!(!default_is_monthly_request_limit(
            r#"{"reason":"TEMPORARILY_SUSPENDED"}"#
        ));
    }

    #[test]
    fn test_default_monthly_request_limit_false() {
        let body = r#"{"message":"nope","reason":"DAILY_REQUEST_COUNT"}"#;
        assert!(!default_is_monthly_request_limit(body));
    }

    #[test]
    fn test_default_bearer_token_invalid() {
        assert!(default_is_bearer_token_invalid(
            "The bearer token included in the request is invalid"
        ));
        assert!(!default_is_bearer_token_invalid("unrelated error"));
    }

    /// 生产事故回归：上游的 TEMPORARILY_SUSPENDED 是**临时**态，绝不能按永久封禁处理。
    ///
    /// 输入用生产日志抓到的原文（未删减关键措辞）。旧实现下：
    /// is_account_suspended=true（命中 "temporarily_suspended"）而
    /// is_temporary_rate_limit=false（SUSPICIOUS_SIGNALS 的 "unusual activity" 匹配不到
    /// "unusual **user** activity"）→ 号被 disabled=true + failure_count=MAX 永久禁用。
    /// 实测 12 小时 88 次 suspend 禁用、51 次凭据用尽、36 次全池自愈活锁。
    #[test]
    fn should_treat_production_temporarily_suspended_body_as_temporary() {
        let body = r#"{"__type":"com.amazon.kiro.runtimeservice#AccessDeniedException",
 "message":"Your User ID is temporarily suspended. We detected unusual user activity and locked it as a security precaution...",
 "reason":"TEMPORARILY_SUSPENDED"}"#;

        assert!(
            default_is_temporary_rate_limit(body),
            "上游原文说 temporarily，必须判为临时限速（只设短冷却 + failover）"
        );
        assert!(
            !default_is_account_suspended(body),
            "绝不能判为永久封禁 —— 那会把还能用的号 disabled 掉并驱动禁用/自愈活锁"
        );
    }

    /// `unusual` 与 `activity` 之间插任意词都应识别（生产漏判的根因是整段字面量匹配）。
    #[test]
    fn should_match_unusual_activity_variants_with_words_in_between() {
        for phrase in [
            "unusual activity",
            "unusual user activity",
            "unusual login activity",
            "unusual account activity",
        ] {
            let body = format!("We detected {phrase}. Your User ID is temporarily suspended.");
            assert!(
                default_is_temporary_rate_limit(&body),
                "应识别为临时限速: {phrase}"
            );
        }
    }

    /// 真正的永久封禁仍须被识别（放宽临时判据不能把永久态也漏掉）。
    #[test]
    fn should_still_detect_genuinely_permanent_suspension() {
        for body in [
            r#"{"message":"account_disabled"}"#,
            "Your account has been suspended",
            "this account was permanently banned",
        ] {
            assert!(
                default_is_account_suspended(body),
                "永久封禁必须仍被识别: {body}"
            );
            assert!(
                !default_is_temporary_rate_limit(body),
                "无临时信号，不应误判为临时: {body}"
            );
        }
    }

    #[test]
    fn test_default_is_account_suspended() {
        assert!(default_is_account_suspended(
            "Your account has been suspended due to suspicious activity"
        ));
        assert!(default_is_account_suspended(
            r#"{"message":"account_disabled"}"#
        ));
        // 普通限流不应被误判为暂停
        assert!(!default_is_account_suspended(
            r#"{"reason":"MONTHLY_REQUEST_COUNT"}"#
        ));
        assert!(!default_is_account_suspended("too many requests"));
    }

    /// ⭐ 生产漏判形态二（2026-08-04 实测）：`temporarily **is** suspended` +
    /// `security precaution`，**两个信号都不命中**。
    ///
    /// 与上面那条（`unusual user activity` 变体）是**同一类**错误的第二个实例：
    /// 上一次修了 unusual 一族，这个 `is` 插在中间的写法漏了。
    ///
    /// 回退即 FAIL：删掉 `default_is_temporary_rate_limit` 开头那个提前 return —— 该 body
    /// 会落 `has_suspicious && has_temporary` 的 `&&` 判定，两边都 false → 判成**非**临时
    /// → provider 落 `report_failure` → 3 次即以 `TooManyFailures`（永久型标签）禁用并落盘。
    ///
    /// 线上后果链（这才是它值得单列一条测试的原因）：
    /// 19:09 与 19:12 两次把 #479 判死 → 池子 2 个掉到 1 个 → 剩下的 #480 吃全部流量
    /// → 撞进它自己的惩罚窗口 → 19:16–19:21 单号 152 请求/分钟、144 个 429。
    /// 一个号被临时限速，被放大成整池 429 风暴。
    #[test]
    fn should_treat_temporarily_is_suspended_variant_as_temporary() {
        // 线上原文（逐字），注意 "temporarily is suspended" 与 "security precaution"。
        let body = r#"{"__type":"com.amazon.aws.codewhisperer#AccessDeniedException","message":"Your User ID (186648603162) temporarily is suspended. We've locked your account as a security precaution. To restore access, please contact our support team to verify your identity: https://aws.amazon.com/contact-us/"}"#;

        assert!(
            default_is_temporary_rate_limit(body),
            "上游写 temporarily 就是临时态；漏判会让它落 report_failure 并被永久型标签禁用"
        );
        assert!(!default_is_account_suspended(body), "绝不能判永久封禁");

        // 三种书写都必须命中（reason 字段用下划线、message 正文两种词序）。
        for s in [
            "Your User ID is temporarily suspended.",
            "Your User ID (1) temporarily is suspended.",
            r#"{"reason":"TEMPORARILY_SUSPENDED"}"#,
        ] {
            assert!(
                default_is_temporary_rate_limit(s),
                "TEMPORARILY_SUSPENDED 的书写变体必须全部命中: {s}"
            );
        }

        // ⭐ 与 handlers 侧的判据必须同口径 —— 同一个 403 不该在两处得出相反结论。
        // 那侧认 "temporarily is suspended"，本侧此前不认，于是客户端拿到正确的 429
        // 而凭据被算成失败。这条断言把两处钉在一起。
        assert!(
            crate::anthropic::handlers::is_upstream_temporarily_suspended(body),
            "handlers 侧判据也必须命中同一个 body（两处口径必须一致）"
        );
    }

    #[test]
    fn test_temporary_rate_limit_requires_both_signals() {
        // 同时含可疑活动 + 临时限速 → 临时风控（非永久封）
        assert!(default_is_temporary_rate_limit(
            "We detected suspicious activity and applied temporary limits to your account"
        ));
        assert!(default_is_temporary_rate_limit(
            r#"{"message":"unusual activity detected, temporary rate limits applied"}"#
        ));
        // 只有可疑活动、没有临时信号 → 不算临时限速（可能是真封禁）
        assert!(!default_is_temporary_rate_limit(
            "account suspended due to suspicious activity"
        ));
        // 只有限速信号、没有可疑活动 → 不算（普通限速走 429 路径即可）
        assert!(!default_is_temporary_rate_limit("temporary limits applied"));
        // 完全无关
        assert!(!default_is_temporary_rate_limit("too many requests"));
    }

    #[test]
    fn test_temporary_rate_limit_precedence_over_suspension() {
        // ⚠️ 防误冻核心边界：一段"临时限速但文案里带 suspended"的 body，
        // is_temporary_rate_limit 必须命中（provider 会先判它，从而只设短冷却）。
        // 同时该 body 也会被 is_account_suspended 命中——正因如此顺序才关键。
        let body = "Your account has been suspended due to suspicious activity. temporary limits applied, try again later.";
        assert!(
            default_is_temporary_rate_limit(body),
            "临时风控文案必须先被识别为临时限速"
        );
        assert!(
            default_is_account_suspended(body),
            "该文案也含 \"account suspended\" 关键词——正是需要靠判定顺序避免误冻的场景"
        );
    }

    /// 判定顺序仍然是必要防线：存在「两个判据同时命中」的 body，
    /// provider 必须先问 is_temporary_rate_limit（provider.rs:733 早于 :802）。
    ///
    /// 把 temporarily_suspended 移出永久表后，生产那条 body 已不再双命中，
    /// 但「account suspended + temporary limits」这类组合仍会双命中，
    /// 所以顺序不能因本次修复而被认为多余。
    #[test]
    fn should_keep_precedence_meaningful_for_dual_match_bodies() {
        let body = "account suspended: unusual user activity, temporary limits applied";
        assert!(default_is_temporary_rate_limit(body), "临时判据须命中");
        assert!(
            default_is_account_suspended(body),
            "永久判据也命中 —— 故 provider 的判定顺序仍是防误冻的关键"
        );
    }

    /// ⭐ 新增判据（用户线上实测原文，逐字）：`IMAGE_MIME_MISMATCH` 必须可识别。
    ///
    /// 它此前**全仓零判据**，落通用 `bad_request` 桶 ⇒ 与工具参数错、上下文超限、
    /// 请求体畸形混在一起，面板上分辨不出来。而 `converter.rs` 的 magic bytes 校正
    /// 需要一个效果度量：若仍有边缘情况漏掉，只有这条判据能把它数出来。
    ///
    /// 删掉 `default_is_image_mime_mismatch` 里那行 → 本测试必 FAILED。
    #[test]
    fn image_mime_mismatch_is_recognized() {
        let real = r#"非流式 API 请求失败: 400 Bad Request {"__type":"com.amazon.aws.codewhisperer#ValidationException","message":"messages.2.content.1.image.source.base64: The image was specified using the image/png media type, but the image appears to be a image/jpeg image","reason":"IMAGE_MIME_MISMATCH"}"#;
        assert!(
            default_is_image_mime_mismatch(real),
            "用户线上原文必须命中（否则那 400 混进通用桶，magic bytes 修复无法度量）"
        );
    }

    /// ⭐ 承重反向守卫：判据必须**窄** —— 只认 reason 字面量。
    ///
    /// `ValidationException` 这个 `__type` 被多种校验错误共用，而它们处置不同：
    /// - `TOOL_USE_RESULT_MISMATCH` → `is_client_validation_error`（立即终止）；
    /// - `IMAGE_MIME_MISMATCH` → 400 + 图片专属排障文案。
    /// 泛匹配 `__type` 会把两者混成一类，给出错误的排障方向。
    ///
    /// 同理不认 message 里的 `media type` 散文：那句话的措辞随格式组合变化
    /// （`image/png` ↔ `image/jpeg` 可任意互换），按散文匹配等于赌上游文案不变 ——
    /// 本仓已因「按整段字面量匹配上游散文」踩过两次
    /// （`unusual user activity` / `temporarily is suspended`）。
    ///
    /// 把判据改成 `body.contains("ValidationException")` → 本测试必 FAILED。
    #[test]
    fn image_mime_judgement_must_not_swallow_other_validation_errors() {
        assert!(!default_is_image_mime_mismatch(
            r#"{"__type":"com.amazon.aws.codewhisperer#ValidationException","reason":"TOOL_USE_RESULT_MISMATCH"}"#
        ));
        // 容量不足同为 400，但必须拿 503（可退避重试），绝不能被图片判据吞掉。
        assert!(!default_is_image_mime_mismatch(
            r#"400 Bad Request {"__type":"com.amazon.aws.codewhisperer#ThrottlingException","message":"I am experiencing high traffic, please try again shortly.","reason":"INSUFFICIENT_MODEL_CAPACITY"}"#
        ));
        assert!(!default_is_image_mime_mismatch(
            r#"{"reason":"CONTENT_LENGTH_EXCEEDS_THRESHOLD"}"#
        ));
        // 裸 ValidationException（无 reason）：判不出是哪一类，不认更安全。
        assert!(!default_is_image_mime_mismatch(
            r#"{"__type":"ValidationException"}"#
        ));
    }

    #[test]
    fn test_client_validation_error_detects_tool_mismatch() {
        assert!(default_is_client_validation_error(
            r#"{"reason":"TOOL_USE_RESULT_MISMATCH","message":"..."}"#
        ));
        assert!(!default_is_client_validation_error(
            r#"{"reason":"MONTHLY_REQUEST_COUNT"}"#
        ));
        assert!(!default_is_client_validation_error("some other error"));
    }

    #[test]
    fn test_extract_retry_after_relative_seconds() {
        assert_eq!(
            default_extract_retry_after_secs(r#"{"resets_in_seconds":120}"#),
            Some(120)
        );
        assert_eq!(
            default_extract_retry_after_secs(r#"{"error":{"retry_after":45}}"#),
            Some(45)
        );
    }

    #[test]
    fn test_extract_retry_after_absolute_epoch() {
        let future = chrono::Utc::now().timestamp() + 300;
        let body = format!(r#"{{"resets_at":{future}}}"#);
        let got = default_extract_retry_after_secs(&body).unwrap();
        // 允许少量执行耗时误差
        assert!((295..=300).contains(&got), "got {got}");
    }

    #[test]
    fn test_extract_retry_after_absent() {
        assert_eq!(default_extract_retry_after_secs(r#"{"message":"x"}"#), None);
        assert_eq!(default_extract_retry_after_secs("not json"), None);
        // 过去的 epoch 不返回
        assert_eq!(
            default_extract_retry_after_secs(r#"{"resets_at":1000}"#),
            None
        );
    }
}
