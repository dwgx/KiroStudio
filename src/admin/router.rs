//! Admin API 路由配置

use axum::{
    Router, middleware,
    routing::{delete, get, post},
};

use super::{
    handlers::{
        add_credential, deep_verify_credential, delete_credential, disable_overage,
        enable_overage, export_credential, force_refresh_token, get_all_credentials,
        get_cached_balances, get_credential_balance, get_load_balancing_mode, get_config,
        get_overage_status, list_trash, purge_credential, poll_social_login, reset_failure_count,
        restart_service, restore_credential, set_credential_disabled, set_credential_name,
        set_credential_proxy,
        set_credential_priority, set_credential_rpm_limit, purge_trash_batch,
        set_load_balancing_mode, social_callback, start_social_login, storage_cleanup,
        storage_stats, update_config, start_idc_login, poll_idc_login,
        start_external_idp_login, external_idp_leg1, external_idp_leg2,
        check_update, perform_update, update_status,
    },
    middleware::{AdminState, admin_auth_middleware},
    usage_handlers::{
        ratelimit_insights, stream_live, usage_by_credential, usage_by_model, usage_cache,
        usage_clients, usage_machines, usage_overview, usage_rate, usage_recent, usage_throughput,
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
        // 凭据回收站（静态段 trash 与 {id} 同层共存，matchit 静态段优先匹配）
        .route("/credentials/trash", get(list_trash))
        // 批量清空回收站（静态段 purge 优先于 trash/{id}）
        .route("/credentials/trash/purge", post(purge_trash_batch))
        .route("/credentials/trash/{id}/restore", post(restore_credential))
        .route("/credentials/trash/{id}", delete(purge_credential))
        .route("/credentials/{id}/disabled", post(set_credential_disabled))
        .route("/credentials/{id}/priority", post(set_credential_priority))
        .route("/credentials/{id}/rpm-limit", post(set_credential_rpm_limit))
        .route("/credentials/{id}/name", post(set_credential_name))
        .route("/credentials/{id}/proxy", post(set_credential_proxy))
        .route("/credentials/{id}/reset", post(reset_failure_count))
        .route("/credentials/{id}/refresh", post(force_refresh_token))
        .route("/credentials/{id}/verify", post(deep_verify_credential))
        .route("/credentials/{id}/balance", get(get_credential_balance))
        // 单号 overage 真开关（⚠️ enable 触发真实按量付费；仅响应显式单号请求）
        .route("/credentials/{id}/overage", get(get_overage_status))
        .route("/credentials/{id}/overage/enable", post(enable_overage))
        .route("/credentials/{id}/overage/disable", post(disable_overage))
        // 批量已缓存余额（只读缓存，不触发上游）。静态段 balances 与 {id} 同层，matchit 静态优先。
        .route("/credentials/balances/cached", get(get_cached_balances))
        .route("/credentials/{id}/export", get(export_credential))
        .route(
            "/config/load-balancing",
            get(get_load_balancing_mode).put(set_load_balancing_mode),
        )
        .route("/config", get(get_config).put(update_config))
        .route("/auth/social/start", post(start_social_login))
        .route("/auth/social/poll/{session_id}", post(poll_social_login))
        .route("/auth/idc/start", post(start_idc_login))
        .route("/auth/idc/poll/{session_id}", post(poll_idc_login))
        // 外部 IdP（Microsoft）双段粘贴引导上号
        .route("/auth/external-idp/start", post(start_external_idp_login))
        .route("/auth/external-idp/leg1", post(external_idp_leg1))
        .route("/auth/external-idp/leg2", post(external_idp_leg2))
        // 用量统计查询（只读）
        .route("/usage/overview", get(usage_overview))
        .route("/usage/timeseries", get(usage_timeseries))
        .route("/usage/by-model", get(usage_by_model))
        .route("/usage/by-credential", get(usage_by_credential))
        .route("/usage/recent", get(usage_recent))
        .route("/usage/rate", get(usage_rate))
        // 下游客户端 RPM 视图（谁开了几个窗口、各打多少）
        .route("/usage/clients", get(usage_clients))
        .route("/usage/machines", get(usage_machines))
        // 全局实时吞吐（最近 60 秒逐秒桶，供前端画流动粒子）
        .route("/usage/throughput", get(usage_throughput))
        // 影子缓存命中率累计快照（供前端展示缓存确实生效）
        .route("/usage/cache", get(usage_cache))
        // 限流 insights：每号一条限流健康快照（rpm/软上限/冷却/近期429/中文推断），零上游
        .route("/ratelimit/insights", get(ratelimit_insights))
        // SSE 实时流：每 ~1.5s 推一帧轻量快照（全局 inflight/rpm + 每号状态 + 吞吐），零上游
        .route("/stream/live", get(stream_live))
        // 运维：一键重启 + 存储统计/清理
        .route("/service/restart", post(restart_service))
        .route("/storage/stats", get(storage_stats))
        .route("/storage/cleanup", post(storage_cleanup))
        // OTA 自更新：GitHub 版本检查 + 一键升级（下载→sha256→替换→重启）
        .route("/update/check", get(check_update))
        .route("/update/perform", post(perform_update))
        // OTA 观测：读 .health/.bak/*.failed 标记，显示升级是否稳定确认 / 是否发生过回滚
        .route("/update/status", get(update_status))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            admin_auth_middleware,
        ));

    // 公开路由：远程模式 OAuth 回调（浏览器无 admin key，靠 OAuth state 关联会话）
    let public = Router::new().route("/auth/callback", get(social_callback));

    authed.merge(public).with_state(state)
}
