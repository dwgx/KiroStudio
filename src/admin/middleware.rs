//! Admin API 中间件

use std::sync::Arc;

use axum::{
    body::Body,
    extract::State,
    http::{Request, StatusCode},
    middleware::Next,
    response::{IntoResponse, Json, Response},
};

use super::service::AdminService;
use super::types::AdminErrorResponse;
use crate::common::auth;
use crate::usage::{TraceDb, UsageStats};

/// Admin API 共享状态
#[derive(Clone)]
pub struct AdminState {
    /// Admin API 密钥
    pub admin_api_key: String,
    /// Admin 服务
    pub service: Arc<AdminService>,
    /// 用量统计（内存预聚合 + JSONL），未启用统计时为 None
    pub usage_stats: Option<Arc<UsageStats>>,
    /// 用量明细（SQLite），未启用统计时为 None
    pub trace_db: Option<Arc<TraceDb>>,
}

impl AdminState {
    pub fn new(admin_api_key: impl Into<String>, service: AdminService) -> Self {
        Self {
            admin_api_key: admin_api_key.into(),
            service: Arc::new(service),
            usage_stats: None,
            trace_db: None,
        }
    }

    /// 注入用量查询句柄（与热路径 sink 共享同一实例）
    pub fn with_usage(mut self, stats: Arc<UsageStats>, trace_db: Arc<TraceDb>) -> Self {
        self.usage_stats = Some(stats);
        self.trace_db = Some(trace_db);
        self
    }
}

/// Admin API 认证中间件
pub async fn admin_auth_middleware(
    State(state): State<AdminState>,
    request: Request<Body>,
    next: Next,
) -> Response {
    let api_key = auth::extract_api_key(&request);

    match api_key {
        Some(key) if auth::constant_time_eq(&key, &state.admin_api_key) => next.run(request).await,
        _ => {
            let error = AdminErrorResponse::authentication_error();
            (StatusCode::UNAUTHORIZED, Json(error)).into_response()
        }
    }
}
