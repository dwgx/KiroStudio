//! Admin API 路由配置

use axum::{
    Router, middleware,
    routing::{delete, get, post},
};

use super::{
    handlers::{
        add_credential, bulk_import_socks_nodes, check_update, cleanup_disabled_credentials,
        clone_credential, deep_verify_credential, delete_credential, delete_credentials_batch,
        delete_socks_node, disable_overage, enable_overage, export_credential, external_idp_leg1,
        external_idp_leg2, external_idp_leg2_select, force_refresh_token, get_all_credentials,
        get_cached_balances, get_config, get_credential_balance, get_load_balancing_mode,
        get_overage_status, import_keys, list_socks_nodes, list_trash, perform_update,
        poll_idc_login, poll_social_login, probe_available_models, probe_models_standalone,
        probe_regions, proxy_test, probe_upstream_models, purge_credential, purge_trash_batch,
        recovery_metrics,
        reset_failure_count, restart_service, restore_credential, set_credential_allowed_models,
        set_credential_api_region, set_credential_custom_api,
        set_credential_deepseek_normalize, set_credential_disabled,
        set_credential_endpoint, set_credential_name, set_credential_priority,
        set_credential_proxy, set_credential_rpm_limit, set_credential_tag,
        set_load_balancing_mode, social_callback, start_external_idp_login, start_idc_login,
        start_social_login, storage_cleanup, storage_stats, switch_profile_region, test_socks_node,
        update_config, update_status, upsert_socks_node,
    },
    middleware::{AdminState, admin_auth_middleware},
    usage_handlers::{
        logs_export, logs_poll, logs_stream, ratelimit_insights, stream_live, traces_search,
        usage_by_credential, usage_by_model, usage_clients, usage_machines, usage_overview,
        usage_rate, usage_recent, usage_throughput, usage_timeseries,
    },
};

/// 创建 Admin API 路由
///
/// # 端点
/// - `GET /credentials` - 获取所有凭据状态
/// - `POST /credentials` - 添加新凭据
/// - `POST /credentials/:id/clone` - 给已有号再加 N 份分身（key 不经前端）
/// - `POST /import/keys` - 批量导入 Kiro API Key
/// - `DELETE /credentials/:id` - 删除凭据
/// - `POST /credentials/batch-delete` - 批量删除（收 ids）
/// - `POST /credentials/cleanup-disabled` - 批量清理已禁用号（候选服务端算，排除代挂）
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
        // 批量删除（静态段 batch-delete 与 {id} 同层，matchit 静态优先）。
        // 支持 force=true 跳过「必须先禁用」门，把批量删 N 个从 2N 次往返降到 1 次。
        .route("/credentials/batch-delete", post(delete_credentials_batch))
        // 批量清理已禁用号（进回收站，可恢复）。候选由服务端算并**排除代挂号**，
        // 故不收 ids —— 判据只有后端一份，前端各写一份必然漂移成误删代挂。
        // 同样是静态段，与 {id} 同层由 matchit 静态优先。
        .route(
            "/credentials/cleanup-disabled",
            post(cleanup_disabled_credentials),
        )
        // 批量导入 Kiro API Key（ksk_ 号）：兼容 items[] / keys[] / apiKey / kiroApiKey 四种体
        .route("/import/keys", post(import_keys))
        // 凭据回收站（静态段 trash 与 {id} 同层共存，matchit 静态段优先匹配）
        .route("/credentials/trash", get(list_trash))
        // 批量清空回收站（静态段 purge 优先于 trash/{id}）
        .route("/credentials/trash/purge", post(purge_trash_batch))
        .route("/credentials/trash/{id}/restore", post(restore_credential))
        .route("/credentials/trash/{id}", delete(purge_credential))
        .route("/credentials/{id}/disabled", post(set_credential_disabled))
        .route("/credentials/{id}/priority", post(set_credential_priority))
        .route(
            "/credentials/{id}/rpm-limit",
            post(set_credential_rpm_limit),
        )
        .route(
            "/credentials/{id}/allowed-models",
            post(set_credential_allowed_models),
        )
        .route(
            "/credentials/{id}/custom-api",
            post(set_credential_custom_api),
        )
        // 固定/解除该号走的端点（ide / cli）；null=回到自动路由（ksk_ 号自动 cli）
        .route("/credentials/{id}/endpoint", post(set_credential_endpoint))
        .route(
            "/credentials/{id}/api-region",
            post(set_credential_api_region),
        )
        // 代挂凭据的 deepseek 协议归一化开关（仅 custom_api 有意义）。
        .route(
            "/credentials/{id}/deepseek-normalize",
            post(set_credential_deepseek_normalize),
        )
        // 代挂凭据探测上游可用模型（custom_api 专属）。
        .route("/credentials/{id}/upstream-models", get(probe_upstream_models))
        // 创建前临时探测上游模型（凭据还不存在；构造临时凭据打上游，不持久化）。
        .route("/credentials/probe-models", post(probe_models_standalone))
        .route("/credentials/{id}/name", post(set_credential_name))
        .route("/credentials/{id}/tag", post(set_credential_tag))
        // 给已有号再加 N 份分身。刻意按 id 而不是让前端重发 key：
        // 分身管理页只有 apiKeyHash 与掩码，key 原文一步都不该离开服务端。
        .route("/credentials/{id}/clone", post(clone_credential))
        .route("/credentials/{id}/proxy", post(set_credential_proxy))
        .route("/credentials/{id}/reset", post(reset_failure_count))
        .route("/credentials/{id}/refresh", post(force_refresh_token))
        .route("/credentials/{id}/verify", post(deep_verify_credential))
        // External IdP region 验活选择：列候选 region（GET）+ 切换到目标 region profile（POST，仅验活可用才写）
        .route("/credentials/{id}/regions", get(probe_regions))
        .route(
            "/credentials/{id}/switch-region",
            post(switch_profile_region),
        )
        // 选中令牌后探测可用模型（逐模型极小请求，看哪些通/哪些 INVALID_MODEL_ID）
        .route("/credentials/{id}/models", get(probe_available_models))
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
        .route(
            "/auth/external-idp/leg2/select",
            post(external_idp_leg2_select),
        )
        // 用量统计查询（只读）
        .route("/usage/overview", get(usage_overview))
        .route("/usage/timeseries", get(usage_timeseries))
        .route("/usage/by-model", get(usage_by_model))
        .route("/usage/by-credential", get(usage_by_credential))
        .route("/usage/recent", get(usage_recent))
        // trace 明细搜索/过滤/分页（多维 AND，参数化防注入，单页≤500）
        .route("/traces/search", get(traces_search))
        .route("/usage/rate", get(usage_rate))
        // 下游客户端 RPM 视图（谁开了几个窗口、各打多少）
        .route("/usage/clients", get(usage_clients))
        .route("/usage/machines", get(usage_machines))
        // 全局实时吞吐（最近 60 秒逐秒桶，供前端画流动粒子）
        .route("/usage/throughput", get(usage_throughput))
        // 限流 insights：每号一条限流健康快照（rpm/软上限/冷却/近期429/中文推断），零上游
        .route("/ratelimit/insights", get(ratelimit_insights))
        // 自愈机器可观测:刷新/failover/自动禁用/冷却/region重探/泄漏清洗 进程级计数器,零上游
        .route("/recovery-metrics", get(recovery_metrics))
        // SSE 实时流：每 ~1.5s 推一帧轻量快照（全局 inflight/rpm + 每号状态 + 吞吐），零上游
        .route("/stream/live", get(stream_live))
        // 运维日志：内存环形缓冲拉取(增量+级别) / SSE 实时直播 / 一键导出 JSONL(附 bug 报告)
        .route("/logs", get(logs_poll))
        .route("/logs/stream", get(logs_stream))
        .route("/logs/export", get(logs_export))
        // 运维：一键重启 + 存储统计/清理
        .route("/service/restart", post(restart_service))
        .route("/storage/stats", get(storage_stats))
        .route("/storage/cleanup", post(storage_cleanup))
        // 代理测活：通过指定代理(或直连)访问固定探针 URL,测连通性+出口 IP(SSRF:目标硬编码)
        .route("/proxy/test", post(proxy_test))
        // 可复用代理节点表（「分身管理」页）。挂在 /proxy/test 旁，同一鉴权层内。
        .route(
            "/socks/nodes",
            get(list_socks_nodes).post(upsert_socks_node),
        )
        .route("/socks/nodes/bulk-import", post(bulk_import_socks_nodes))
        .route("/socks/nodes/{id}", delete(delete_socks_node))
        .route("/socks/nodes/{id}/test", post(test_socks_node))
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

/// 批量导入的**兼容别名路由**，挂在 `/api` 而非 `/api/admin`。
///
/// 为什么需要它：外部对接方（kiro-accounting 一类）的请求路径固定为
/// `POST /api/import/keys`，改不了。而本仓的 admin 路由整树 nest 在 `/api/admin` 下
/// （`main.rs` 的 `.nest("/api/admin", ...)`），所以那个路径是 404。
///
/// 刻意只暴露这一个端点，不是把整个 admin 树也挂到 `/api`：
/// 后者会让 `/api/credentials`、`/api/config` 等全部多出一条等价入口，
/// 凭据管理面多一倍且日后新增端点会自动跟着暴露——那是隐性的攻击面扩张。
///
/// 鉴权与 `/api/admin/import/keys` **完全一致**（同一个 `admin_auth_middleware`，
/// 读同一个 adminKey），不存在"别名路径绕过鉴权"的问题。
///
/// ⚠️ 路径不带尾斜杠，且 axum 默认不做 `/api/import/keys` → `/api/import/keys/` 的
/// 重定向，因此 POST 不会被 307/308 转走（对接方明确要求这一点：重定向会让部分
/// HTTP 客户端把 POST 降级为 GET 或丢弃请求体）。
pub fn create_import_alias_router(state: AdminState) -> Router {
    Router::new()
        .route("/import/keys", post(import_keys))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            admin_auth_middleware,
        ))
        .with_state(state)
}
