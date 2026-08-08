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
    /// Admin 服务
    pub service: Arc<AdminService>,
    /// 用量统计（内存预聚合 + JSONL），未启用统计时为 None
    pub usage_stats: Option<Arc<UsageStats>>,
    /// 用量明细（SQLite），未启用统计时为 None
    pub trace_db: Option<Arc<TraceDb>>,
}

impl AdminState {
    /// 创建 Admin 状态，并把 admin key 播种进进程级热更单元。
    ///
    /// key 不再存进 State：`admin_auth_middleware` 活读 [`crate::common::auth_keys`]，
    /// 这样 admin 改 adminApiKey 后即时生效，无需重启（重启会掐断在途流式请求）。
    /// 空值由 `set_admin_key` 拒绝写入，判定侧 fail-closed（见 auth_keys 模块级安全说明）。
    pub fn new(admin_api_key: impl Into<String>, service: AdminService) -> Self {
        Self::from_shared(admin_api_key, Arc::new(service))
    }

    /// 使用已有的共享服务创建 Admin 状态。
    ///
    /// Portal 的上游额度展示也依赖同一份余额缓存，因此主程序会先创建唯一的
    /// [`AdminService`]，再分别注入 Admin 与 Portal，避免两套缓存和后台刷新任务。
    pub fn from_shared(admin_api_key: impl Into<String>, service: Arc<AdminService>) -> Self {
        if let Err(e) = crate::common::auth_keys::set_admin_key(&admin_api_key.into()) {
            // 调用方（main.rs）已在挂载前判过非空，走到这里说明校验被绕过。
            // 不 panic：Admin API 会因空存储 fail-closed 全拒（401），比裸奔安全。
            tracing::error!("adminApiKey 播种失败，Admin API 将拒绝所有请求: {}", e);
        }
        Self {
            service,
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
    // State 仍需保留（`from_fn_with_state` 的签名要求），但鉴权已不读它。
    State(_state): State<AdminState>,
    request: Request<Body>,
    next: Next,
) -> Response {
    let api_key = auth::extract_api_key(&request);

    // 活读进程级热更单元而非 State 里的固化副本：admin 改 adminApiKey 后即时生效。
    // 空存储恒 false（fail-closed），见 auth_keys 模块级安全说明。
    match api_key {
        Some(key) if crate::common::auth_keys::admin_key_matches(&key) => next.run(request).await,
        _ => {
            let error = AdminErrorResponse::authentication_error();
            (StatusCode::UNAUTHORIZED, Json(error)).into_response()
        }
    }
}
