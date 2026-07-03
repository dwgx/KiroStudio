//! 请求记录数据契约
//!
//! 一次 API 请求生命周期的最终结算快照。字段设计参考 cc-switch 的 `RequestLog`
//! （farion1231/cc-switch），裁剪为 Kiro 单上游场景所需。

use serde::{Deserialize, Serialize};

/// 请求结果分类
///
/// 对齐 provider 的失败处置分类（见 [`crate::kiro::cooldown::CooldownReason`]），
/// 便于统计侧按结果聚合健康度。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestOutcome {
    /// 成功
    Success,
    /// 上游限流（429）
    RateLimited,
    /// 认证失败（401/403）
    AuthFailed,
    /// 额度用尽（402 MONTHLY_REQUEST_COUNT）
    QuotaExhausted,
    /// 账户被暂停/封禁
    AccountSuspended,
    /// 上游服务器错误（5xx）
    ServerError,
    /// 请求错误（400 等客户端错误）
    BadRequest,
    /// 网络/连接错误（未拿到响应）
    NetworkError,
    /// 其它/未分类失败
    OtherError,
}

impl RequestOutcome {
    /// 是否为成功结果
    pub fn is_success(&self) -> bool {
        matches!(self, RequestOutcome::Success)
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            RequestOutcome::Success => "success",
            RequestOutcome::RateLimited => "rate_limited",
            RequestOutcome::AuthFailed => "auth_failed",
            RequestOutcome::QuotaExhausted => "quota_exhausted",
            RequestOutcome::AccountSuspended => "account_suspended",
            RequestOutcome::ServerError => "server_error",
            RequestOutcome::BadRequest => "bad_request",
            RequestOutcome::NetworkError => "network_error",
            RequestOutcome::OtherError => "other_error",
        }
    }
}

/// 单次请求的最终结算记录
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestRecord {
    /// 请求唯一 ID（用于关联 per-attempt 明细）
    pub request_id: String,
    /// 记录生成的 Unix 毫秒时间戳
    pub ts_ms: i64,
    /// 实际服务该请求的凭据 ID（失败到无凭据可用时为 None）
    pub credential_id: Option<u64>,
    /// 请求模型名
    pub model: String,
    /// 是否流式
    pub is_streaming: bool,
    /// 输入 tokens（优先精确值，回退估算）
    pub input_tokens: i32,
    /// 输出 tokens
    pub output_tokens: i32,
    /// 上游返回的真实 credit 消耗量（无 meteringEvent 时为 None）
    pub credits_used: Option<f64>,
    /// 端到端延迟（毫秒）
    pub latency_ms: u64,
    /// 首字节/首事件延迟（毫秒，流式有意义）
    pub first_token_ms: Option<u64>,
    /// 结果分类
    pub outcome: RequestOutcome,
    /// 本次经历的重试次数（0 表示首次即成功）
    pub retries: u32,
    /// 错误信息（成功时为空）
    pub error_message: Option<String>,
    /// 会话标识（conversationId，用于亲和分析）
    pub session_id: Option<String>,
}

impl RequestRecord {
    /// 构造一条记录，时间戳取当前时刻
    pub fn new(request_id: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            request_id: request_id.into(),
            ts_ms: chrono::Utc::now().timestamp_millis(),
            credential_id: None,
            model: model.into(),
            is_streaming: false,
            input_tokens: 0,
            output_tokens: 0,
            credits_used: None,
            latency_ms: 0,
            first_token_ms: None,
            outcome: RequestOutcome::Success,
            retries: 0,
            error_message: None,
            session_id: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_outcome_success() {
        assert!(RequestOutcome::Success.is_success());
        assert!(!RequestOutcome::RateLimited.is_success());
    }

    #[test]
    fn test_record_roundtrip_json() {
        let mut rec = RequestRecord::new("req-1", "claude-sonnet-4");
        rec.credential_id = Some(3);
        rec.input_tokens = 100;
        rec.output_tokens = 50;
        rec.credits_used = Some(1.5);
        rec.latency_ms = 1234;
        rec.outcome = RequestOutcome::Success;

        let json = serde_json::to_string(&rec).unwrap();
        let back: RequestRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(back.request_id, "req-1");
        assert_eq!(back.credential_id, Some(3));
        assert_eq!(back.credits_used, Some(1.5));
        assert_eq!(back.outcome, RequestOutcome::Success);
    }

    #[test]
    fn test_outcome_serde_snake_case() {
        let json = serde_json::to_string(&RequestOutcome::AccountSuspended).unwrap();
        assert_eq!(json, "\"account_suspended\"");
    }
}
