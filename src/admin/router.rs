//! Admin API 路由配置

use axum::{
    Router, middleware,
    routing::{delete, get, post},
};

use super::{
    handlers::{
        add_credential, delete_credential, force_refresh_token, get_all_credentials,
        get_credential_balance, get_load_balancing_mode, get_config, poll_social_login,
        reset_failure_count, set_credential_disabled, set_credential_priority,
        set_load_balancing_mode, social_callback, start_social_login, update_config,
    },
    middleware::{AdminState, admin_auth_middleware},
    usage_handlers::{
        usage_by_credential, usage_by_model, usage_overview, usage_rate, usage_recent,
        usage_timeseries,
    },
};

/// 创建 Admin API 路由
///
/// # 端点
/// - `GET /credentials` - 获取所有凭据状态
/// - `POST /credentials` - 添加新凭据
/// - `DELETE /credentials/:id` - 删除凭据
/// - `POST /credentials/:id/disabled` - 设置凭据禁用状态
/// - `POST /credentials/:id/priority` - 设置凭据优先级
/// - `POST /credentials/:id/reset` - 重置失败计数
/// - `POST /credentials/:id/refresh` - 强制刷新 Token
/// - `GET /credentials/:id/balance` - 获取凭据余额
/// - `GET /config/load-balancing` - 获取负载均衡模式
/// - `PUT /config/load-balancing` - 设置负载均衡模式
///
/// # 认证
/// 需要 Admin API Key 认证，支持：
/// - `x-api-key` header
/// - `Authorization: Bearer <token>` header
pub fn create_admin_router(state: AdminState) -> Router {
    // 鉴权路由：所有管理操作 + 网页上号的 start/poll
    let authed = Router::new()
        .route(
            "/credentials",
            get(get_all_credentials).post(add_credential),
        )
        .route("/credentials/{id}", delete(delete_credential))
        .route("/credentials/{id}/disabled", post(set_credential_disabled))
        .route("/credentials/{id}/priority", post(set_credential_priority))
        .route("/credentials/{id}/reset", post(reset_failure_count))
        .route("/credentials/{id}/refresh", post(force_refresh_token))
        .route("/credentials/{id}/balance", get(get_credential_balance))
        .route(
            "/config/load-balancing",
            get(get_load_balancing_mode).put(set_load_balancing_mode),
        )
        .route("/config", get(get_config).put(update_config))
        .route("/auth/social/start", post(start_social_login))
        .route("/auth/social/poll/{session_id}", post(poll_social_login))
        // 用量统计查询（只读）
        .route("/usage/overview", get(usage_overview))
        .route("/usage/timeseries", get(usage_timeseries))
        .route("/usage/by-model", get(usage_by_model))
        .route("/usage/by-credential", get(usage_by_credential))
        .route("/usage/recent", get(usage_recent))
        .route("/usage/rate", get(usage_rate))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            admin_auth_middleware,
        ));

    // 公开路由：远程模式 OAuth 回调（浏览器无 admin key，靠 OAuth state 关联会话）
    let public = Router::new().route("/auth/callback", get(social_callback));

    authed.merge(public).with_state(state)
}
