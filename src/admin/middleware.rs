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
    /// 创建 Admin 状态。
    ///
    /// adminApiKey 已迁入 `common::auth_keys` 进程级热更单元（main.rs 启动播种，
    /// admin 改配置调 setter 即时生效、无需重启——重启会掐断在途流式请求）。
    /// 这里不再存、也不播——避免测试里随意构造 `AdminState` 污染进程级全局 cell。
    /// 空值由 `set_admin_key` 拒绝写入，判定侧 fail-closed（见 auth_keys 模块级安全说明）。
    ///
    /// 此前还留着一个 `_admin_api_key` 形参：它既不存也不播，纯粹是迁移后的遗留 ——
    /// 但调用点（`main.rs`、各处测试）会把 key 传进来，读代码的人会以为「传进去就生效」，
    /// 实际鉴权只认 `auth_keys` 那个全局 cell。故删掉形参，让签名与真实语义一致：
    /// **key 不从这里进**，只从 `auth_keys::set_admin_key` 进。
    pub fn new(service: AdminService) -> Self {
        Self {
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

#[cfg(test)]
mod tests {
    /// E-2 守卫：`AdminState::new` 只收 service。
    ///
    /// 回退即 FAIL：把那个既不存也不播的 `_admin_api_key` 形参加回来（迁移遗留）——
    /// 调用点会把 key 传进来，读代码的人便以为「传进去就生效」，而实际鉴权只认
    /// `auth_keys` 那个进程级 cell。签名必须与真实语义一致：key 只从
    /// `auth_keys::set_admin_key` 进。
    #[test]
    fn admin_state_new_takes_service_only() {
        let src = include_str!("middleware.rs");
        let cut = src.find("#[cfg(test)]").unwrap_or(src.len());
        let prod = &src[..cut];
        assert!(
            prod.contains("pub fn new(service: AdminService) -> Self {"),
            "AdminState::new 必须只收 service（key 只从 auth_keys::set_admin_key 进）"
        );
    }
}
