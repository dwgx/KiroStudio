//! Admin API 错误类型定义

use std::fmt;

use axum::http::StatusCode;

use super::types::AdminErrorResponse;

/// Admin 服务错误类型
#[derive(Debug)]
pub enum AdminServiceError {
    /// 凭据不存在
    NotFound { id: u64 },

    /// 上游服务调用失败（网络、API 错误等）
    UpstreamError(String),

    /// 内部状态错误
    InternalError(String),

    /// 凭据无效（验证失败）
    InvalidCredential(String),

    /// 上游查询超时**且无历史缓存可降级**（当前只用于余额查询）。
    ///
    /// 与 `UpstreamError` 分开是为了给前端一个明确可区分的语义：
    /// 这是"上游慢"而非"上游报错"，可重试且**不代表凭据有问题** ——
    /// 前端不该据此把号标成异常。
    UpstreamTimeout(u64),

    /// 结构化上号诊断（归因+引导）——取代裸字符串错误，前端渲染诊断卡片。
    Diagnosed(crate::kiro::diagnosis::OnboardingDiagnosis),
}

impl fmt::Display for AdminServiceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AdminServiceError::NotFound { id } => {
                write!(f, "凭据不存在: {}", id)
            }
            AdminServiceError::UpstreamError(msg) => write!(f, "上游服务错误: {}", msg),
            AdminServiceError::InternalError(msg) => write!(f, "内部错误: {}", msg),
            AdminServiceError::InvalidCredential(msg) => write!(f, "凭据无效: {}", msg),
            AdminServiceError::UpstreamTimeout(id) => {
                write!(f, "凭据 #{} 余额查询上游超时（无历史值可降级）", id)
            }
            AdminServiceError::Diagnosed(d) => write!(f, "{}", d.log_line()),
        }
    }
}

impl std::error::Error for AdminServiceError {}

impl AdminServiceError {
    /// 获取对应的 HTTP 状态码
    pub fn status_code(&self) -> StatusCode {
        match self {
            AdminServiceError::NotFound { .. } => StatusCode::NOT_FOUND,
            AdminServiceError::UpstreamError(_) => StatusCode::BAD_GATEWAY,
            AdminServiceError::InternalError(_) => StatusCode::INTERNAL_SERVER_ERROR,
            AdminServiceError::InvalidCredential(_) => StatusCode::BAD_REQUEST,
            // 上游超时：504 而非 502 —— 语义上是"等不到"而非"上游报错"，
            // 前端据此显示"稍后重试"而不是把凭据标成异常。
            AdminServiceError::UpstreamTimeout(_) => StatusCode::GATEWAY_TIMEOUT,
            // 诊断类:可重试(上游/瞬时)→502 让客户端知道是上游侧;不可重试(账号/用户/网关)→400
            // (账号需重新上号、用户需改输入,不是"服务器错",给 400 更准 + 前端据 diagnosis 渲染引导)。
            AdminServiceError::Diagnosed(d) => {
                if d.retriable {
                    StatusCode::BAD_GATEWAY
                } else {
                    StatusCode::BAD_REQUEST
                }
            }
        }
    }

    /// 转换为 API 错误响应
    pub fn into_response(self) -> AdminErrorResponse {
        match &self {
            AdminServiceError::NotFound { .. } => AdminErrorResponse::not_found(self.to_string()),
            AdminServiceError::UpstreamError(_) => AdminErrorResponse::api_error(self.to_string()),
            AdminServiceError::InternalError(_) => {
                AdminErrorResponse::internal_error(self.to_string())
            }
            AdminServiceError::InvalidCredential(_) => {
                AdminErrorResponse::invalid_request(self.to_string())
            }
            AdminServiceError::UpstreamTimeout(_) => AdminErrorResponse::api_error(self.to_string()),
            AdminServiceError::Diagnosed(d) => AdminErrorResponse::diagnosed(d.clone()),
        }
    }
}
