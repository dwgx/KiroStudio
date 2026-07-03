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

    /// 从错误响应中提取上游给出的重置时间（秒）
    ///
    /// 某些上游把真实重置时间放在 body 里（如 `resets_in_seconds` / `resets_at` epoch），
    /// 而非 `Retry-After` 头。有则据此设定精确冷却，避免盲目退避浪费。
    fn extract_retry_after_secs(&self, body: &str) -> Option<u64> {
        default_extract_retry_after_secs(body)
    }
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
