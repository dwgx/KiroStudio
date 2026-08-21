//! Admin API 路由配置

use axum::{
    Router, middleware,
    routing::{delete, get, post, put},
};
use axum::{
    body::Body,
    extract::{ConnectInfo, State},
    http::Request,
    middleware::Next,
    response::Response,
};

use super::{
    handlers::{
        add_credential, bulk_import_socks_nodes, check_update, cleanup_disabled_credentials,
        clone_credential, deep_verify_credential, delete_credential, delete_credentials_batch,
        delete_socks_node, diagnostics_snapshot, disable_overage, disable_quota_exceeded,
        enable_overage,
        export_config, export_credential, export_kam_credentials, external_idp_leg1, external_idp_leg2,
        external_idp_leg2_select, force_refresh_token, force_refresh_tokens_batch,
        get_all_credentials, get_cached_balances,
        get_config, get_credential_balance, get_error_message_defaults, get_load_balancing_mode,
        get_overage_status,
        help_web_search, import_config, import_keys, import_sso_token, list_socks_nodes, list_trash,
        perform_update,
        poll_idc_login, poll_social_login, probe_available_models, probe_models_standalone, test_models,
        probe_regions, proxy_test, probe_upstream_models, purge_credential, purge_trash_batch,
        endpoint_health, recovery_metrics, relogin_oauth, reprobe_credential_region,
        reset_credentials_batch, reset_failure_count, restart_service, restore_credential,
        set_credential_allowed_models, set_credentials_allowed_models_batch,
        set_credentials_disabled_batch,
        set_credential_api_region, set_credential_custom_api,
        set_credential_disabled, set_credential_endpoint, set_credential_model_mapping_exempt,
        set_credential_name, set_credential_priority, set_credential_proxy,
        set_credential_rpm_limit, set_credential_tag, set_load_balancing_mode, social_callback,
        start_external_idp_login, start_idc_login, start_social_login, storage_cleanup,
        storage_stats, switch_profile_region, test_socks_node, update_config, update_status,
        update_credential_refresh_token, upsert_socks_node,
    },
    middleware::{AdminState, admin_auth_middleware},
    usage_handlers::{
        logs_export, logs_poll, logs_stream, ratelimit_insights, stream_live, traces_search,
        usage_by_credential, usage_by_model, usage_by_outcome, usage_by_requested_model,
        usage_clients, usage_machines, usage_overview, usage_rate, usage_recent, usage_throughput,
        usage_timeseries,
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
/// - `POST /credentials/batch-reset` - 批量重置失败计数
/// - `POST /credentials/batch-disabled` - 批量启用/禁用
/// - `POST /credentials/batch-allowed-models` - 批量设置允许模型白名单
/// - `POST /credentials/batch-refresh` - 批量强制刷新 Token
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
        .route("/credentials/batch-reset", post(reset_credentials_batch))
        .route(
            "/credentials/batch-disabled",
            post(set_credentials_disabled_batch),
        )
        .route(
            "/credentials/batch-allowed-models",
            post(set_credentials_allowed_models_batch),
        )
        .route(
            "/credentials/batch-refresh",
            post(force_refresh_tokens_batch),
        )
        // 一键禁用所有「余额已超额」的启用号（remaining<=0，数据源=余额缓存，零上游）。
        // 同样是静态段，与 {id} 同层由 matchit 静态优先。
        .route(
            "/credentials/disable-quota-exceeded",
            post(disable_quota_exceeded),
        )
        // 批量清理已禁用号（进回收站，可恢复）。候选由服务端算并**排除代挂号**，
        // 故不收 ids —— 判据只有后端一份，前端各写一份必然漂移成误删代挂。
        // 同样是静态段，与 {id} 同层由 matchit 静态优先。
        .route(
            "/credentials/cleanup-disabled",
            post(cleanup_disabled_credentials),
        )
        // 批量导入 Kiro API Key（ksk_ 号）：兼容 items[] / keys[] / apiKey / kiroApiKey 四种体
        .route("/import/keys", post(import_keys))
        // SSO Token 导入（粘贴 AWS portal Bearer Token 静默换号，产物为标准 IdC 凭据）。
        // 静态段 import-sso 与 {id} 同层共存，matchit 静态段优先（同 /credentials/trash 先例）。
        .route("/credentials/import-sso", post(import_sso_token))
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
        // 手动重探该号可用 region 并写死（救「自动探测探错」的最后一招）。
        // 失败只报错不动禁用态 —— 服役号探失败被禁 = 把好号打掉（启动回填教训）。
        .route(
            "/credentials/{id}/reprobe-region",
            post(reprobe_credential_region),
        )
        // 凭据级模型映射豁免开关（Kiro 号与 custom_api 号都可用）。
        .route(
            "/credentials/{id}/model-mapping-exempt",
            post(set_credential_model_mapping_exempt),
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
        // OAuth 类凭据自助复活（清惩罚态重新启用；api_key/代挂拒绝）
        .route("/credentials/{id}/relogin", post(relogin_oauth))
        .route("/credentials/{id}/refresh", post(force_refresh_token))
        .route("/credentials/{id}/refresh-token", put(update_credential_refresh_token))
        .route("/credentials/{id}/verify", post(deep_verify_credential))
        // External IdP region 验活选择：列候选 region（GET）+ 切换到目标 region profile（POST，仅验活可用才写）
        .route("/credentials/{id}/regions", get(probe_regions))
        .route(
            "/credentials/{id}/switch-region",
            post(switch_profile_region),
        )
        // 选中令牌后探测可用模型（逐模型极小请求，看哪些通/哪些 INVALID_MODEL_ID）
        .route("/credentials/{id}/models", get(probe_available_models))
        .route("/models/test", post(test_models))
        .route("/credentials/{id}/balance", get(get_credential_balance))
        // 单号 overage 真开关（⚠️ enable 触发真实按量付费；仅响应显式单号请求）
        .route("/credentials/{id}/overage", get(get_overage_status))
        .route("/credentials/{id}/overage/enable", post(enable_overage))
        .route("/credentials/{id}/overage/disable", post(disable_overage))
        // 批量已缓存余额（只读缓存，不触发上游）。静态段 balances 与 {id} 同层，matchit 静态优先。
        .route("/credentials/balances/cached", get(get_cached_balances))
        .route("/credentials/{id}/export", get(export_credential))
        // KAM 批量导出（明文 token 敏感操作，只读）。静态段 export-kam 与 {id} 同层，
        // matchit 静态优先（同 /credentials/balances/cached 先例）。
        .route("/credentials/export-kam", get(export_kam_credentials))
        .route(
            "/config/load-balancing",
            get(get_load_balancing_mode).put(set_load_balancing_mode),
        )
        .route("/config", get(get_config).put(update_config))
        // 错误码/提示词**内置默认表**（只读，前端默认值预览数据源；key 集随默认表演进
        // 自动同步，运行期读取不硬编码）。静态段，与 /config 同层互不冲突。
        .route("/error-messages/defaults", get(get_error_message_defaults))
        // 配置导出/导入（2026-08-14）：导出脱敏（敏感字段省略）、导入先校验后写盘。
        .route("/config/export", get(export_config))
        .route("/config/import", post(import_config))
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
        .route("/usage/by-requested-model", get(usage_by_requested_model))
        .route("/usage/by-credential", get(usage_by_credential))
        .route("/usage/by-outcome", get(usage_by_outcome))
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
        // 端点自适应派发可观测：每(凭据,端点)的实测 EWMA 成功率 + 样本数，零上游
        .route("/endpoint-health", get(endpoint_health))
        // SSE 实时流：每 ~1.5s 推一帧轻量快照（全局 inflight/rpm + 每号状态 + 吞吐），零上游
        .route("/stream/live", get(stream_live))
        // 运维日志：内存环形缓冲拉取(增量+级别) / SSE 实时直播 / 一键导出 JSONL(附 bug 报告)
        .route("/logs", get(logs_poll))
        .route("/logs/stream", get(logs_stream))
        .route("/logs/export", get(logs_export))
        // 运维：一键重启 + 存储统计/清理 + 诊断快照聚合（纯观测）
        .route("/service/restart", post(restart_service))
        .route("/storage/stats", get(storage_stats))
        .route("/storage/cleanup", post(storage_cleanup))
        .route("/diagnostics/snapshot", get(diagnostics_snapshot))
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
        // 帮助页「联网搜索」代理：DuckDuckGo Instant Answer，空结果兜底 Bing RSS。
        // 服务器出网（前端绕开 CORS）；无额外频控 —— 面板内使用、量小，
        // 且本路由在 authed 树内，鉴权层照常拦截。
        .route("/help/web-search", get(help_web_search))
        // ⚠️ 层序（tower）：**后挂的先执行**。审计必须先挂、鉴权后挂 ⇒ 鉴权最外层先跑，
        // 未鉴权请求在鉴权层就被拦下，**不会**进审计日志（防无 key 攻击者刷审计）。
        // 顺序颠倒 = 审计在鉴权之前跑，未经鉴权的请求也会被记录。
        .layer(middleware::from_fn_with_state(
            state.clone(),
            admin_audit_middleware,
        ))
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

/// 管理操作审计中间件（2026-08-14 新增）。
///
/// 记录**鉴权通过后**的管理请求：方法 / 路径 / 客户端 IP / 时间戳。日志走
/// `tracing::info!(target: "audit", ...)`：tracing 层已接 log_buffer 内存环形缓冲
/// （面板「运维日志」可见），面板日志条目带 target 字段可直接按 target 过滤；
/// 终端 fmt 层默认也会把 target 作为前缀显示，无需消息内再带 `audit:` 字样。
/// 客户端 IP 与入口安全中间件同口径（`client_ip`：可信反代后取 XFF 最右段）。
async fn admin_audit_middleware(
    State(state): State<AdminState>,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    request: Request<Body>,
    next: Next,
) -> Response {
    let trust_forwarded = state.service.trust_forwarded_header();
    let ip = crate::common::security::client_ip(&request, Some(peer), trust_forwarded)
        .map(|ip| ip.to_string())
        .unwrap_or_else(|| "-".to_string());
    tracing::info!(
        target: "audit",
        method = %request.method(),
        path = %request.uri().path(),
        client_ip = %ip,
        "管理操作: {} {} {} client_ip={}",
        chrono::Utc::now().to_rfc3339(),
        request.method(),
        request.uri().path(),
        ip
    );
    next.run(request).await
}

#[cfg(test)]
mod guard_tests {
    /// 🔴 层序守卫：审计中间件必须先挂、鉴权后挂。
    ///
    /// tower 的 layer **后挂的先执行**：若顺序颠倒（审计挂在鉴权之后），
    /// 未鉴权请求会被鉴权层先拦下 → 不会进审计；顺序反了则未鉴权请求
    /// 也会被记录，等于给无 key 攻击者提供审计日志刷屏面。
    /// 判据：源码里审计的 layer 挂载点必须出现在鉴权 layer 挂载点**之前**。
    ///
    /// 回退即 FAIL：把两个 .layer(..) 交换顺序 / 删掉审计层。
    #[test]
    fn audit_layer_precedes_auth_layer() {
        let src = include_str!("router.rs");
        let cut = src.find("#[cfg(test)]").unwrap_or(src.len());
        let prod = &src[..cut];
        // 折叠空白再比：rustfmt 会把长调用拆多行。
        let compact: String = prod.chars().filter(|c| !c.is_whitespace()).collect();
        // needle 运行时拼接：写成完整字面量会被 include_str! 读到自己而多算一处。
        let audit = format!("from_fn_with_state(state.clone(),admin_audit{}", "_middleware");
        let auth = format!("from_fn_with_state(state.clone(),admin_auth{}", "_middleware");
        let ai = compact
            .find(&audit)
            .expect("审计中间件挂载点不该消失");
        let au = compact
            .find(&auth)
            .expect("鉴权中间件挂载点不该消失");
        assert!(
            ai < au,
            "审计层必须先挂、鉴权层后挂（tower 后挂先执行）——顺序反了未鉴权请求也会进审计"
        );
    }

    /// 配置导出/导入端点必须注册在鉴权路由树内。
    ///
    /// 回退即 FAIL：删除 /config/export 或 /config/import 路由。
    #[test]
    fn config_export_import_endpoints_are_registered() {
        let src = include_str!("router.rs");
        let cut = src.find("#[cfg(test)]").unwrap_or(src.len());
        let prod = &src[..cut];
        let compact: String = prod.chars().filter(|c| !c.is_whitespace()).collect();
        let routes = [
            format!("\"/config/export\",get(export_config{}", ")"),
            format!("\"/config/import\",post(import_config{}", ")"),
        ];
        for route in routes {
            assert!(
                compact.contains(&route),
                "配置导出/导入端点必须注册进鉴权路由树：{}",
                route
            );
        }
    }

    /// SSO Token 导入端点必须注册在鉴权路由树内。
    ///
    /// 回退即 FAIL：删除 `/credentials/import-sso` 路由。
    #[test]
    fn models_test_endpoint_is_registered() {
        let src = include_str!("router.rs");
        let cut = src.find("#[cfg(test)]").unwrap_or(src.len());
        let prod = &src[..cut];
        let compact: String = prod.chars().filter(|c| !c.is_whitespace()).collect();
        let route = format!("\"/models/test\",post(test_models{}", ")");
        assert!(
            compact.contains(&route),
            "POST /api/admin/models/test 必须挂在鉴权树上"
        );
    }

    #[test]
    fn import_sso_endpoint_is_registered() {
        let src = include_str!("router.rs");
        let cut = src.find("#[cfg(test)]").unwrap_or(src.len());
        let prod = &src[..cut];
        let compact: String = prod.chars().filter(|c| !c.is_whitespace()).collect();
        // needle 运行时拼接：字面量会被 include_str! 读到测试自身而永远为真。
        let route = format!(
            "\"/credentials/import-{sso}\",post(import_{sso}_token{close}",
            sso = "sso",
            close = ")"
        );
        assert!(
            compact.contains(&route),
            "SSO Token 导入端点必须注册进鉴权路由树：{}",
            route
        );
    }
}
