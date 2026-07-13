//! Kiro 端点抽象
//!
//! 不同 Kiro 端点（如 `ide` / `cli`）在 URL、请求头、请求体上存在差异，
//! 但共享凭据池、Token 刷新、重试逻辑和 AWS event-stream 响应解码。
//!
//! [`KiroEndpoint`] 抽象了请求侧的差异点；`KiroProvider` 持有一个 endpoint 注册表，
//! 按凭据的 `endpoint` 字段选择对应实现。

use reqwest::RequestBuilder;

use crate::kiro::model::credentials::KiroCredentials;
use crate::model::config::Config;

pub mod ide;

pub use ide::IdeEndpoint;

/// Kiro 端点
///
/// 同一个 `KiroProvider` 可持有多个 endpoint 实现，按凭据级字段切换。
pub trait KiroEndpoint: Send + Sync {
    /// 端点名称（对应 credentials.endpoint / config.defaultEndpoint 的取值）
    fn name(&self) -> &'static str;

    /// API endpoint URL
    fn api_url(&self, ctx: &RequestContext<'_>) -> String;

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

    /// 判断响应体是否表示"月度配额用尽"（禁用凭据并转移）
    fn is_monthly_request_limit(&self, body: &str) -> bool {
        default_is_monthly_request_limit(body)
    }

    /// 判断响应体是否表示"上游 bearer token 失效"（触发强制刷新）
    fn is_bearer_token_invalid(&self, body: &str) -> bool {
        default_is_bearer_token_invalid(body)
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

    /// 判断响应体是否表示"该凭据不能服务此模型"（`INVALID_MODEL_ID`）。
    ///
    /// 典型成因：该号的订阅被上游取消/降级，原本能用的模型（如 opus）不再对它开放，
    /// 上游返回 `400 INVALID_MODEL_ID`。这**不是**客户端请求错误——换一个订阅仍有效的
    /// 号往往能成功。因此命中时应：给该号冷却 + failover 到别的号（而非直接把 400 透传给
    /// 客户端、坏号还留在轮转里反复命中）。若**所有**号都返回它，才是模型本身无效，透传。
    fn is_invalid_model_id(&self, body: &str) -> bool {
        default_is_invalid_model_id(body)
    }

    /// 从错误响应中提取上游给出的重置时间（秒）
    ///
    /// 某些上游把真实重置时间放在 body 里（如 `resets_in_seconds` / `resets_at` epoch），
    /// 而非 `Retry-After` 头。有则据此设定精确冷却，避免盲目退避浪费。
    fn extract_retry_after_secs(&self, body: &str) -> Option<u64> {
        default_extract_retry_after_secs(body)
    }
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

/// 默认的 MONTHLY_REQUEST_COUNT 判断逻辑
///
/// 同时识别顶层 `reason` 字段和嵌套 `error.reason` 字段。
pub fn default_is_monthly_request_limit(body: &str) -> bool {
    if body.contains("MONTHLY_REQUEST_COUNT") {
        return true;
    }

    let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else {
        return false;
    };

    if value
        .get("reason")
        .and_then(|v| v.as_str())
        .is_some_and(|v| v == "MONTHLY_REQUEST_COUNT")
    {
        return true;
    }

    value
        .pointer("/error/reason")
        .and_then(|v| v.as_str())
        .is_some_and(|v| v == "MONTHLY_REQUEST_COUNT")
}

/// 默认的 bearer token 失效判断逻辑
pub fn default_is_bearer_token_invalid(body: &str) -> bool {
    body.contains("The bearer token included in the request is invalid")
}

/// 默认的账户暂停/封禁判断逻辑
///
/// 参考 Kiro-Go `account_failover.go` 的错误分类经验：
/// 识别上游明确的 suspend/ban/disable 信号（大小写不敏感），
/// 命中即视为不可自动恢复，应直接禁用凭据等待人工处理。
pub fn default_is_account_suspended(body: &str) -> bool {
    let lower = body.to_ascii_lowercase();
    // 明确的封禁/暂停/停用信号
    const SUSPEND_KEYWORDS: &[&str] = &[
        "temporarily_suspended",
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

    // 可疑活动信号（风控触发的标志）
    const SUSPICIOUS_SIGNALS: &[&str] = &["suspicious activity", "unusual activity"];
    // 临时/限速信号（表明是限速而非永久封）
    const TEMPORARY_SIGNALS: &[&str] = &[
        "temporary limits",
        "temporary limit",
        "temporary rate",
        "temporarily limited",
        "temporarily rate",
        "rate limits applied",
        "rate limit applied",
    ];

    let has_suspicious = SUSPICIOUS_SIGNALS.iter().any(|kw| lower.contains(kw));
    let has_temporary = TEMPORARY_SIGNALS.iter().any(|kw| lower.contains(kw));
    has_suspicious && has_temporary
}

/// 默认的"客户端请求校验错误"判断逻辑（v4-2.2）
///
/// 命中即视为请求构造问题：立即终止，换号/重试都无意义。
pub fn default_is_client_validation_error(body: &str) -> bool {
    body.contains("TOOL_USE_RESULT_MISMATCH")
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
mod tests {
    use super::*;

    #[test]
    fn test_default_monthly_request_limit_detects_reason() {
        let body = r#"{"message":"You have reached the limit.","reason":"MONTHLY_REQUEST_COUNT"}"#;
        assert!(default_is_monthly_request_limit(body));
    }

    #[test]
    fn test_default_monthly_request_limit_nested_reason() {
        let body = r#"{"error":{"reason":"MONTHLY_REQUEST_COUNT"}}"#;
        assert!(default_is_monthly_request_limit(body));
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

    #[test]
    fn test_default_is_account_suspended() {
        assert!(default_is_account_suspended(
            r#"{"reason":"TEMPORARILY_SUSPENDED"}"#
        ));
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
        let body =
            "Your account has been suspended due to suspicious activity. temporary limits applied, try again later.";
        assert!(
            default_is_temporary_rate_limit(body),
            "临时风控文案必须先被识别为临时限速"
        );
        assert!(
            default_is_account_suspended(body),
            "该文案也含 suspend 关键词——正是需要靠判定顺序避免误冻的场景"
        );
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
