//! Admin API HTTP 处理器

use axum::{
    Json,
    extract::{Path, Query, State},
    response::IntoResponse,
};
use serde::Deserialize;

use super::{
    middleware::AdminState,
    types::{
        AddCredentialRequest, BatchDeleteRequest, BatchDeleteResponse, CleanupDisabledRequest,
        CloneCredentialRequest, SetAllowedModelsRequest, SetApiRegionRequest,
        SetCustomApiConfigRequest, SetDisabledRequest, SetEndpointRequest,
        SetLoadBalancingModeRequest, SetPriorityRequest, SetRpmLimitRequest, SuccessResponse,
        parse_import_keys_request,
    },
};

/// GET /api/admin/credentials
/// 获取所有凭据状态
pub async fn get_all_credentials(State(state): State<AdminState>) -> impl IntoResponse {
    let response = state.service.get_all_credentials();
    Json(response)
}

/// POST /api/admin/credentials/:id/disabled
/// 设置凭据禁用状态
pub async fn set_credential_disabled(
    State(state): State<AdminState>,
    Path(id): Path<u64>,
    Json(payload): Json<SetDisabledRequest>,
) -> impl IntoResponse {
    match state.service.set_disabled(id, payload.disabled) {
        Ok(_) => {
            let action = if payload.disabled { "禁用" } else { "启用" };
            Json(SuccessResponse::new(format!("凭据 #{} 已{}", id, action))).into_response()
        }
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// POST /api/admin/credentials/:id/priority
/// 设置凭据优先级
pub async fn set_credential_priority(
    State(state): State<AdminState>,
    Path(id): Path<u64>,
    Json(payload): Json<SetPriorityRequest>,
) -> impl IntoResponse {
    match state.service.set_priority(id, payload.priority) {
        Ok(_) => Json(SuccessResponse::new(format!(
            "凭据 #{} 优先级已设置为 {}",
            id, payload.priority
        )))
        .into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// POST /api/admin/credentials/:id/rpm-limit
/// 设置凭据级 RPM 容量上限（0/null=继承全局）
pub async fn set_credential_rpm_limit(
    State(state): State<AdminState>,
    Path(id): Path<u64>,
    Json(payload): Json<SetRpmLimitRequest>,
) -> impl IntoResponse {
    match state.service.set_rpm_limit(id, payload.rpm_limit) {
        Ok(_) => Json(SuccessResponse::new(format!(
            "凭据 #{} RPM 容量已设置为 {:?}",
            id, payload.rpm_limit
        )))
        .into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// POST /api/admin/credentials/:id/endpoint
/// 固定该凭据走的端点（`ide` / `cli`）；传 null 或空串清除，回到自动路由
/// （`ksk_` API Key 号自动走 `cli`，其余回退 `config.defaultEndpoint`）。
/// POST /api/admin/credentials/:id/api-region  body: `{ "apiRegion": "eu-central-1" }`
///
/// 补运维缺口：`ksk_` 按 region 授权、打错区恒 403 且永不自愈，而此前没有任何
/// 修改它的入口（`/regions` / `/switch-region` 都是 ARN 门控）⇒ 只能删号重建。
pub async fn set_credential_api_region(
    State(state): State<AdminState>,
    Path(id): Path<u64>,
    Json(payload): Json<SetApiRegionRequest>,
) -> impl IntoResponse {
    let requested = payload.api_region.clone();
    match state
        .service
        .set_credential_api_region(id, payload.api_region)
    {
        Ok(_) => {
            let msg = match requested
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                Some(r) => format!("凭据 #{} apiRegion 已设为 {}", id, r),
                None => format!("凭据 #{} apiRegion 已清除（回退全局 region）", id),
            };
            Json(SuccessResponse::new(msg)).into_response()
        }
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

pub async fn set_credential_endpoint(
    State(state): State<AdminState>,
    Path(id): Path<u64>,
    Json(payload): Json<SetEndpointRequest>,
) -> impl IntoResponse {
    let requested = payload.endpoint.clone();
    match state.service.set_credential_endpoint(id, payload.endpoint) {
        Ok(_) => {
            let msg = match requested
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                Some(name) => format!("凭据 #{} 端点已固定为 {}", id, name),
                None => format!("凭据 #{} 已恢复自动选择端点", id),
            };
            Json(SuccessResponse::new(msg)).into_response()
        }
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// POST /api/admin/credentials/:id/custom-api
/// 修改自定义 API(代挂透传)凭据的 base_url / api_key / 请求上限。仅 custom_api 号可用(后端 gate)。
pub async fn set_credential_custom_api(
    State(state): State<AdminState>,
    Path(id): Path<u64>,
    Json(payload): Json<SetCustomApiConfigRequest>,
) -> impl IntoResponse {
    match state
        .service
        .set_custom_api_config(
            id,
            payload.base_url,
            payload.api_key,
            payload.request_limit,
            payload.reset_count,
        )
        .await
    {
        Ok(_) => Json(SuccessResponse::new(format!(
            "凭据 #{} 自定义 API 配置已更新",
            id
        )))
        .into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// POST /api/admin/credentials/:id/allowed-models
/// 设置凭据级「允许模型」白名单（成本安全硬门；空/null = 不限制）
pub async fn set_credential_allowed_models(
    State(state): State<AdminState>,
    Path(id): Path<u64>,
    Json(payload): Json<SetAllowedModelsRequest>,
) -> impl IntoResponse {
    match state
        .service
        .set_allowed_models(id, payload.allowed_models.clone())
    {
        Ok(_) => Json(SuccessResponse::new(format!(
            "凭据 #{} 允许模型白名单已更新（{} 项，空=不限制）",
            id,
            payload
                .allowed_models
                .as_ref()
                .map(|l| l.len())
                .unwrap_or(0)
        )))
        .into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// POST /api/admin/credentials/:id/name
/// 设置凭据自定义别名/备注（传空清除）
pub async fn set_credential_name(
    State(state): State<AdminState>,
    Path(id): Path<u64>,
    Json(payload): Json<SetNameRequest>,
) -> impl IntoResponse {
    match state.service.set_credential_name(id, payload.name.clone()) {
        Ok(_) => Json(SuccessResponse::new(format!("凭据 #{} 别名已更新", id))).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// POST /api/admin/credentials/:id/tag
/// 设置分身标签（这一份的用途标记，与 name 是账号别名不同）
pub async fn set_credential_tag(
    State(state): State<AdminState>,
    Path(id): Path<u64>,
    Json(payload): Json<SetTagRequest>,
) -> impl IntoResponse {
    match state.service.set_credential_tag(id, payload.tag.clone()) {
        Ok(_) => Json(SuccessResponse::new(format!("凭据 #{} 标签已更新", id))).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// POST /api/admin/credentials/:id/proxy
/// 设置单个凭据代理（立即生效、无需重启）
pub async fn set_credential_proxy(
    State(state): State<AdminState>,
    Path(id): Path<u64>,
    Json(payload): Json<SetProxyRequest>,
) -> impl IntoResponse {
    match state.service.set_credential_proxy(
        id,
        payload.proxy_url.clone(),
        payload.proxy_username.clone(),
        payload.proxy_password.clone(),
    ) {
        Ok(_) => Json(SuccessResponse::new(format!("凭据 #{} 代理已更新", id))).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// POST /api/admin/credentials/trash/purge
/// 批量清空回收站（body.ids 为空则清空全部）。不可恢复。
pub async fn purge_trash_batch(
    State(state): State<AdminState>,
    Json(payload): Json<PurgeTrashRequest>,
) -> impl IntoResponse {
    let n = state.service.purge_trash_batch(payload.ids);
    Json(SuccessResponse::new(format!(
        "已永久清除 {} 个回收站条目",
        n
    )))
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetNameRequest {
    /// 别名/备注;传 null 或空字符串清除
    pub name: Option<String>,
}

/// POST /api/admin/credentials/:id/tag 请求体。
///
/// ⚠️ 与 `SetProxyRequest` 不同，这里**带** `rename_all="camelCase"`（本仓惯例）。
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetTagRequest {
    /// 分身标签;传 null 或空字符串清除
    pub tag: Option<String>,
}

/// POST /api/admin/proxy/test 请求体。
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProxyTestRequest {
    /// 待测代理 URL；"direct"/空 表示测直连（不走代理）。可内嵌账密。
    pub proxy_url: String,
    /// 代理用户名（可选，未内嵌在 URL 时用）
    #[serde(default)]
    pub proxy_username: Option<String>,
    /// 代理密码（可选）
    #[serde(default)]
    pub proxy_password: Option<String>,
}

/// POST /api/admin/proxy/test 响应体。
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProxyTestResponse {
    /// 是否连通成功
    pub ok: bool,
    /// 端到端耗时（毫秒）
    pub latency_ms: u64,
    /// 出口 IP（成功时从 ipify 返回），失败为 None
    pub exit_ip: Option<String>,
    /// 失败原因（成功为 None）
    pub error: Option<String>,
}

/// 代理测活探针目标：**硬编码**的轻量 HTTPS 接口，返回 `{"ip":"..."}`。
///
/// SSRF 铁律：目标 URL 永远固定在此，绝不接受请求方传入——用户只能控制「用哪个代理」，
/// 不能控制「访问哪个 URL」，杜绝把本网关当跳板打内网/元数据端点。
const PROXY_TEST_PROBE_URL: &str = "https://api.ipify.org?format=json";

/// POST /api/admin/proxy/test
/// 通过指定代理（或直连）访问固定探针 URL，测连通性 + 出口 IP。
///
/// 无论代理是否可达，都以 HTTP 200 返回结构化结果（`ok=false` + `error` 描述失败），
/// 不抛 500——让前端能稳定拿到"测活失败原因"而非通用错误页。
pub async fn proxy_test(
    State(state): State<AdminState>,
    Json(payload): Json<ProxyTestRequest>,
) -> impl IntoResponse {
    Json(
        run_proxy_probe(
            &state,
            &payload.proxy_url,
            payload.proxy_username.clone(),
            payload.proxy_password.clone(),
        )
        .await,
    )
    .into_response()
}

/// 跑一次代理测活探针，返回结构化结果。
///
/// 抽出来供两个调用方共用：`/proxy/test`（临时地址）与
/// `/socks/nodes/{id}/test`（节点表里已存的地址）。**不要各写一份** ——
/// 探针 URL 是 SSRF 防线（请求方无法左右访问目标），复制一份必然漂移，
/// 而漂移的那一份就是可被指使的出站。
pub(super) async fn run_proxy_probe(
    state: &AdminState,
    proxy_url: &str,
    username: Option<String>,
    password: Option<String>,
) -> ProxyTestResponse {
    use crate::http_client::{ProxyConfig, build_client, split_proxy_credentials};

    let started = std::time::Instant::now();

    // 拆出干净 URL 与内嵌账密；显式字段优先覆盖内嵌账密。
    let (clean_url, embedded_user, embedded_pass) = split_proxy_credentials(proxy_url);
    let is_direct = clean_url.is_empty() || clean_url.eq_ignore_ascii_case("direct");

    let proxy_config = if is_direct {
        None
    } else {
        let username = username.filter(|s| !s.trim().is_empty()).or(embedded_user);
        let password = password.filter(|s| !s.is_empty()).or(embedded_pass);
        let mut cfg = ProxyConfig::new(clean_url);
        if let (Some(u), Some(p)) = (username, password) {
            cfg = cfg.with_auth(u, p);
        }
        Some(cfg)
    };

    // 复用全局 TLS 后端 + http_client 构建助手；~10s 超时（连不上/超时都算失败）。
    let client = match build_client(proxy_config.as_ref(), 10, state.service.tls_backend()) {
        Ok(c) => c,
        Err(e) => {
            return ProxyTestResponse {
                ok: false,
                latency_ms: started.elapsed().as_millis() as u64,
                exit_ip: None,
                error: Some(format!("构建代理客户端失败: {e}")),
            };
        }
    };

    // 目标固定为硬编码探针 URL（SSRF 防线：请求方无法左右访问目标）。
    match client.get(PROXY_TEST_PROBE_URL).send().await {
        Ok(resp) => {
            let status = resp.status();
            let latency_ms = started.elapsed().as_millis() as u64;
            if !status.is_success() {
                return ProxyTestResponse {
                    ok: false,
                    latency_ms,
                    exit_ip: None,
                    error: Some(format!("探针返回非 2xx 状态: {status}")),
                };
            }
            // 解析 {"ip":"..."}；解析失败不影响连通性判定，仅 exit_ip 为 None。
            let exit_ip = resp.json::<serde_json::Value>().await.ok().and_then(|v| {
                v.get("ip")
                    .and_then(|ip| ip.as_str().map(|s| s.to_string()))
            });
            ProxyTestResponse {
                ok: true,
                latency_ms,
                exit_ip,
                error: None,
            }
        }
        Err(e) => ProxyTestResponse {
            ok: false,
            latency_ms: started.elapsed().as_millis() as u64,
            exit_ip: None,
            // reqwest 错误可能含代理地址，保留原因文本便于诊断（不含用户密码，账密在 ProxyConfig 内）。
            error: Some(format!("代理连通失败: {e}")),
        },
    }
}

/// GET /api/admin/socks/nodes — 列出代理节点（密码恒不外传）
pub async fn list_socks_nodes(State(state): State<AdminState>) -> impl IntoResponse {
    let nodes = state.service.list_socks_nodes();
    Json(serde_json::json!({ "total": nodes.len(), "nodes": nodes })).into_response()
}

/// POST /api/admin/socks/nodes — 新建/更新代理节点
/// POST /api/admin/socks/nodes/bulk-import — 整段粘贴节点商文档批量导入
///
/// 节点商下发的是 `socks://base64(user:pass)@host:port#name`，混在含标题/分隔线/
/// `端口: 40002`/curl 示例的文档里，同一节点还出现两次。逐条手填 5 台 = 25 个字段，
/// 且极易把 base64 串当用户名填进去（认证失败长得像"节点不通"）。
pub async fn bulk_import_socks_nodes(
    State(state): State<AdminState>,
    Json(payload): Json<super::types::SocksNodeBulkImportRequest>,
) -> impl IntoResponse {
    match state
        .service
        .bulk_import_socks_nodes(&payload.text, payload.enabled)
        .await
    {
        Ok(out) => {
            let (added, skipped, dup, over_cap) =
                (out.added, out.skipped, out.duplicate, out.over_capacity);
            let mut msg = format!("已导入 {added} 个节点");
            if dup > 0 {
                msg.push_str(&format!("，{dup} 个已存在（按地址去重，未覆盖原有账密）"));
            }
            if over_cap > 0 {
                msg.push_str(&format!("，{over_cap} 个因超出节点数上限未导入"));
            }
            if skipped > 0 {
                msg.push_str(&format!("，跳过 {skipped} 行非链接文本"));
            }
            if !payload.enabled {
                msg.push_str("。默认未启用 —— 测活后再启用才会参与分身分配");
            }
            // 四个聚合字段逐字保留（旧客户端只读它们），`items` 是新增的逐行明细。
            Json(serde_json::json!({
                "added": added, "skipped": skipped, "duplicate": dup,
                "overCapacity": over_cap, "message": msg,
                "items": out.items,
            }))
            .into_response()
        }
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

pub async fn upsert_socks_node(
    State(state): State<AdminState>,
    Json(payload): Json<super::types::SocksNodeUpsertRequest>,
) -> impl IntoResponse {
    match state.service.upsert_socks_node(payload).await {
        Ok(id) => Json(serde_json::json!({
            "id": id,
            "message": format!("代理节点 #{} 已保存", id),
        }))
        .into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// DELETE /api/admin/socks/nodes/{id} — 删除代理节点
///
/// **不动已绑该节点的凭据**：凭据的 `proxy_*` 是独立的绑定结果，
/// 删节点只把它从候选池移除，否则删一个节点会让一批分身当场掉线。
pub async fn delete_socks_node(
    State(state): State<AdminState>,
    Path(id): Path<u64>,
) -> impl IntoResponse {
    match state.service.delete_socks_node(id) {
        Ok(deleted) => Json(serde_json::json!({ "deleted": deleted })).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// POST /api/admin/socks/nodes/{id}/test — 测活并写回结果
pub async fn test_socks_node(
    State(state): State<AdminState>,
    Path(id): Path<u64>,
) -> impl IntoResponse {
    let Some((url, username, password)) = state.service.socks_node_proxy(id) else {
        let e = super::error::AdminServiceError::NotFound { id };
        return (e.status_code(), Json(e.into_response())).into_response();
    };
    let result = run_proxy_probe(&state, &url, username, password).await;
    // 写回失败不影响本次测速结果返回（结果本身是有效信息）。
    if let Err(e) = state.service.record_socks_node_test(
        id,
        crate::kiro::model::socks_node::SocksNodeTest {
            ok: result.ok,
            latency_ms: result.latency_ms,
            exit_ip: result.exit_ip.clone(),
            error: result.error.clone(),
            tested_at: chrono::Utc::now().timestamp().max(0) as u64,
        },
    ) {
        tracing::warn!("写回节点 #{} 测速结果失败: {:?}", id, e);
    }
    Json(result).into_response()
}

/// `POST /api/admin/credentials/trash/{id}/restore` 的可选请求体。
///
/// 整个 body 可省略（`Option<Json<_>>`），此时 `force = false`。
/// 加 `rename_all = "camelCase"` 与本仓多数 Admin 类型一致；
/// 字段只有一个 `force`，两种命名恰好相同，故前端发 `{"force":true}` 即可。
#[derive(serde::Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct RestoreCredentialRequest {
    /// 跳过 key 重复校验（多开分身与主凭据必然同 key）。默认 false。
    #[serde(default)]
    pub force: bool,
}

#[derive(serde::Deserialize)]
pub struct SetProxyRequest {
    /// 代理 URL;传 null/空清除(回退全局),"direct" 表示强制不走代理
    pub proxy_url: Option<String>,
    /// 代理用户名;None 不改,空清除
    #[serde(default)]
    pub proxy_username: Option<String>,
    /// 代理密码;None 不改,空清除
    #[serde(default)]
    pub proxy_password: Option<String>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PurgeTrashRequest {
    /// 要清除的回收站条目 id;为空/缺省则清空全部
    #[serde(default)]
    pub ids: Option<Vec<u64>>,
}

/// POST /api/admin/credentials/:id/reset
/// 重置失败计数并重新启用
pub async fn reset_failure_count(
    State(state): State<AdminState>,
    Path(id): Path<u64>,
) -> impl IntoResponse {
    match state.service.reset_and_enable(id) {
        Ok(_) => Json(SuccessResponse::new(format!(
            "凭据 #{} 失败计数已重置并重新启用",
            id
        )))
        .into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// `GET /api/admin/credentials/:id/balance` 的查询参数。
#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct BalanceQuery {
    /// `force=true` 跳过 5 分钟新鲜度缓存，**真打一次上游**。
    ///
    /// 存在的理由：不带它时 TTL 内连点两次拿到同一个数字、零上游往返，用户看到的就是
    /// "刷新没反应"（这是「额度/积分刷新太慢」的一条实因）。
    /// 仅作用于显式单号请求，无批量入口 —— 详见 `AdminService::get_balance` 的文档。
    #[serde(default)]
    pub force: bool,
}

/// GET /api/admin/credentials/:id/balance[?force=true]
/// 获取指定凭据的余额（默认走 5 分钟缓存；`force=true` 强制取上游真值）
pub async fn get_credential_balance(
    State(state): State<AdminState>,
    Path(id): Path<u64>,
    Query(q): Query<BalanceQuery>,
) -> impl IntoResponse {
    match state.service.get_balance(id, q.force).await {
        Ok(response) => Json(response).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// GET /api/admin/credentials/:id/overage
/// 读取单号 overage 状态（实时查询上游，只读）
pub async fn get_overage_status(
    State(state): State<AdminState>,
    Path(id): Path<u64>,
) -> impl IntoResponse {
    match state.service.overage_status(id).await {
        Ok(status) => Json(status).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// POST /api/admin/credentials/:id/overage/enable
/// 开启单号 overage —— ⚠️ 触发真实按量付费。幂等。
///
/// 计费安全：仅响应显式的单号请求，不做自动/批量开启；操作会写审计日志。
pub async fn enable_overage(
    State(state): State<AdminState>,
    Path(id): Path<u64>,
) -> impl IntoResponse {
    match state.service.enable_overage(id).await {
        Ok(status) => Json(status).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// POST /api/admin/credentials/:id/overage/disable
/// 关闭单号 overage。幂等。
pub async fn disable_overage(
    State(state): State<AdminState>,
    Path(id): Path<u64>,
) -> impl IntoResponse {
    match state.service.disable_overage(id).await {
        Ok(status) => Json(status).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// GET /api/admin/credentials/balances/cached
/// 批量读取【已缓存】的凭据余额（只读缓存，不触发任何上游调用）
pub async fn get_cached_balances(State(state): State<AdminState>) -> impl IntoResponse {
    Json(state.service.get_cached_balances())
}

/// POST /api/admin/credentials
/// 添加新凭据
pub async fn add_credential(
    State(state): State<AdminState>,
    Json(payload): Json<AddCredentialRequest>,
) -> impl IntoResponse {
    match state.service.add_credential(payload).await {
        Ok(response) => Json(response).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// POST /api/admin/credentials/{id}/clone
/// 给**池中已有**的凭据再加 N 份分身。
///
/// 与 `POST /credentials` + `copies` 的区别只在入口：key 由服务端按 id 自己读，
/// 一步都不经前端（分身管理页只有 `apiKeyHash` 与掩码，拿不到原文）。
///
/// 请求体
/// `{ copies, enabled?, nodeIds?, assignPrimaryNode?, requireNodePerCopy?, replacePrimary? }`。
/// **`enabled` 省略时落到配置项 `cloneDefaultEnabled`**（默认 false = 建出来是禁用的，
/// 与普通上号默认启用相反，理由见 `CloneCredentialRequest::enabled`）。
/// `replacePrimary` 省略 = false（保留主份，只追加分身）；`true` = 建完 N 份后把主份
/// 软删进回收站，使组内 N 份彼此同质（见 `CloneCredentialRequest::replace_primary`）。
/// `nodeIds` 省略 = 从节点池自动分配；给了则按顺序逐份指定（见
/// `AddCredentialRequest::node_ids`）。
/// `assignPrimaryNode` 省略 = **true**（本次新建的第 1 份也从池里取节点，行为不变）；
/// `requireNodePerCopy` 省略 = false（节点不够时多出来的份直连，行为不变）。
///
/// 份数逻辑本身完全复用 `add_credential` 那一段实现，见
/// [`crate::admin::service::AdminService::clone_credential`]。
pub async fn clone_credential(
    State(state): State<AdminState>,
    Path(id): Path<u64>,
    Json(payload): Json<CloneCredentialRequest>,
) -> impl IntoResponse {
    // 份数缺失按 1（"再加一份"是最常见用法）；上限 clamp 在 service 层，与 copies 同源。
    //
    // `enabled` 与 `nodeIds` **原样下传**（不在这里 `unwrap_or` / 不在这里校验节点是否
    // 存在）：默认值与"无效 id 怎么报"的语义属于业务层，放在 service 里才有一份可测的
    // 定义，否则这些规则会散在每个调用方手上。
    match state
        .service
        .clone_credential(
            id,
            payload.copies.unwrap_or(1),
            payload.enabled,
            payload.node_ids,
            payload.assign_primary_node,
            payload.require_node_per_copy,
            payload.replace_primary,
        )
        .await
    {
        Ok(response) => Json(response).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// POST /api/admin/import/keys
/// 批量导入 Kiro API Key（`ksk_` 号）。
///
/// 认证：与其余 admin 端点同源（`Authorization: Bearer <adminKey>` 或 `x-api-key`），
/// 由 `admin_auth_middleware` 统一拦截，失败 401。
///
/// 请求体兼容 4 种格式（见 [`parse_import_keys_request`]）；格式错误 / concurrencyLimit
/// 越界 → 400；部分失败仍 200，逐条在 `results[].ok` / `results[].error` 标记。
/// 响应中的 `key` 恒为脱敏形态，不含完整 Key。
pub async fn import_keys(
    State(state): State<AdminState>,
    Json(payload): Json<serde_json::Value>,
) -> impl IntoResponse {
    // 开关闸门：**必须在解析之前**。关闭时不解析、不入池，直接 403。
    // 放在 handler 内而不是建路由时判：config 是热重载的（ArcSwap），
    // 按建树时的值决定路由存在与否会让开关只在重启后生效。
    //
    // 两个挂载点（`/api/admin/import/keys` 与外部对接方的 `/api/import/keys`）
    // 共用本 handler，所以这一道闸同时覆盖两者。
    if !state.service.import_keys_enabled() {
        return (
            axum::http::StatusCode::FORBIDDEN,
            Json(super::types::AdminErrorResponse::invalid_request(
                "批量推号入口已在设置中关闭（importKeysEnabled=false）".to_string(),
            )),
        )
            .into_response();
    }
    // 手工解析而非 #[derive(Deserialize)]：4 种互斥格式 + 越界校验要区分「字段缺失」
    // 与「类型/范围非法」，serde 的 untagged 无法给出可读的 400 原因。
    let req = match parse_import_keys_request(&payload) {
        Ok(req) => req,
        Err(msg) => {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                Json(super::types::AdminErrorResponse::invalid_request(msg)),
            )
                .into_response();
        }
    };
    Json(state.service.import_keys(req).await).into_response()
}

/// DELETE /api/admin/credentials/:id
/// 删除凭据
pub async fn delete_credential(
    State(state): State<AdminState>,
    Path(id): Path<u64>,
) -> impl IntoResponse {
    match state.service.delete_credential(id) {
        Ok(_) => Json(SuccessResponse::new(format!("凭据 #{} 已删除", id))).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// 批量删除的 ids 上限。
///
/// 200 是线上号池量级（曾达 43）的数倍余量，同时防止一个请求把整池清空
/// —— adminKey 明文存 localStorage 且全仓无 CSP，无上限的批量删除会放大 XSS 的破坏面。
const MAX_BATCH_DELETE_IDS: usize = 200;

/// POST /api/admin/credentials/batch-delete
///
/// 批量删除凭据。`force=true` 跳过「必须先禁用」这道门（仍进回收站，可恢复）。
/// **部分失败仍返 200**，逐条标 ok/error（与 import/keys 同款模式）。
pub async fn delete_credentials_batch(
    State(state): State<AdminState>,
    Json(payload): Json<BatchDeleteRequest>,
) -> impl IntoResponse {
    if payload.ids.is_empty() {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            Json(super::types::AdminErrorResponse::invalid_request(
                "ids 不能为空".to_string(),
            )),
        )
            .into_response();
    }
    if payload.ids.len() > MAX_BATCH_DELETE_IDS {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            Json(super::types::AdminErrorResponse::invalid_request(format!(
                "一次最多删除 {} 个凭据（本次 {}）",
                MAX_BATCH_DELETE_IDS,
                payload.ids.len()
            ))),
        )
            .into_response();
    }

    // 去重：同一 id 传两次时第二次必然失败（已从 entries 移出），会产生令人困惑的
    // "部分失败"。这是前端多选去重不严时的常见输入，在边界处收敛掉。
    let mut seen = std::collections::HashSet::new();
    let ids: Vec<u64> = payload
        .ids
        .iter()
        .copied()
        .filter(|id| seen.insert(*id))
        .collect();

    if payload.force {
        tracing::warn!(
            count = ids.len(),
            "批量**强制**删除凭据（绕过先禁用门，仍进回收站）"
        );
    }
    let results = state.service.delete_credentials_batch(&ids, payload.force);
    let deleted = results.iter().filter(|r| r.ok).count();
    let failed = results.len() - deleted;
    Json(BatchDeleteResponse {
        deleted,
        failed,
        results,
    })
    .into_response()
}

/// POST /api/admin/credentials/cleanup-disabled
///
/// 批量清理**已禁用**的凭据（走 `delete_credential` → 进**回收站**，可恢复）。
/// 排除代挂号（`custom_api` / `PassthroughFailed` / `PassthroughOverloaded`）——
/// 判据在服务端唯一收口，见 [`crate::admin::service::AdminService::cleanup_disabled_credentials`]。
///
/// 请求体可选：`{"dryRun": true}` 只预览不删。**体缺失/为空也接受**（等价 `dryRun=false`），
/// 与 `restore_credential` 同款宽松语义 —— 一个不带任何参数的清理请求是最常见的用法，
/// 不该因为少一个 `{}` 就 400。
///
/// 与批量删除一致：**部分失败仍返 200**，逐条标 ok/error。
pub async fn cleanup_disabled_credentials(
    State(state): State<AdminState>,
    body: Option<Json<CleanupDisabledRequest>>,
) -> impl IntoResponse {
    let dry_run = body.map(|Json(b)| b.dry_run).unwrap_or(false);
    Json(state.service.cleanup_disabled_credentials(dry_run))
}

/// GET /api/admin/credentials/trash
/// 列出回收站中的已删除凭据
pub async fn list_trash(State(state): State<AdminState>) -> impl IntoResponse {
    let response = state.service.list_trash();
    Json(response)
}

/// POST /api/admin/credentials/trash/:id/restore
/// 从回收站恢复凭据（恢复为禁用态，id 不变）
///
/// 请求体可选 `{"force": true}`：跳过 key 重复校验，用于恢复**多开分身**
/// （分身与主凭据必然同 key，不给这个出口的话删掉的分身永远恢复不了）。
/// 请求体缺失/为空时 `force = false`，保留误操作护栏。
pub async fn restore_credential(
    State(state): State<AdminState>,
    Path(id): Path<u64>,
    body: Option<Json<RestoreCredentialRequest>>,
) -> impl IntoResponse {
    let force = body.map(|Json(b)| b.force).unwrap_or(false);
    match state.service.restore_credential(id, force) {
        Ok(_) => Json(SuccessResponse::new(format!(
            "凭据 #{} 已从回收站恢复（当前为禁用态，可手动启用）",
            id
        )))
        .into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// DELETE /api/admin/credentials/trash/:id
/// 从回收站彻底删除凭据（不可恢复）
pub async fn purge_credential(
    State(state): State<AdminState>,
    Path(id): Path<u64>,
) -> impl IntoResponse {
    match state.service.purge_credential(id) {
        Ok(_) => Json(SuccessResponse::new(format!(
            "凭据 #{} 已从回收站彻底删除",
            id
        )))
        .into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// POST /api/admin/credentials/:id/refresh
/// 强制刷新凭据 Token
pub async fn force_refresh_token(
    State(state): State<AdminState>,
    Path(id): Path<u64>,
) -> impl IntoResponse {
    match state.service.force_refresh_token(id).await {
        Ok(_) => Json(SuccessResponse::new(format!(
            "凭据 #{} Token 已强制刷新",
            id
        )))
        .into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// POST /api/admin/credentials/:id/verify
/// 深度验活（发真实 API 请求检测 suspend）
pub async fn deep_verify_credential(
    State(state): State<AdminState>,
    Path(id): Path<u64>,
) -> impl IntoResponse {
    match state.service.deep_verify_credential(id).await {
        Ok(_) => Json(SuccessResponse::new(format!("凭据 #{} 验活通过", id))).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// GET /api/admin/credentials/:id/regions
/// 【F】列出 external_idp 号在候选 region 的全部 profile 及验活结果（供前端选 region）。
pub async fn probe_regions(
    State(state): State<AdminState>,
    Path(id): Path<u64>,
) -> impl IntoResponse {
    match state.service.probe_regions(id).await {
        Ok(candidates) => Json(serde_json::json!({
            "id": id,
            "regions": candidates.iter().map(|c| serde_json::json!({
                "arn": c.arn,
                "region": c.region,
                "account": c.account,
                "usable": c.usable,
                "subscriptionTitle": c.subscription_title,
                "reason": c.reason,
                "current": c.current,
            })).collect::<Vec<_>>(),
        }))
        .into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// POST /api/admin/credentials/:id/switch-region  body: { "arn": "..." }
/// 【F】切换 external_idp 号到目标 region 的 profile（仅验活可用才写入，不可用则 400 且不改）。
pub async fn switch_profile_region(
    State(state): State<AdminState>,
    Path(id): Path<u64>,
    Json(payload): Json<SwitchRegionRequest>,
) -> impl IntoResponse {
    match state.service.switch_profile_region(id, &payload.arn).await {
        Ok(title) => Json(serde_json::json!({
            "id": id,
            "arn": payload.arn,
            "subscriptionTitle": title,
            "message": format!("凭据 #{} 已切换 region profile", id),
        }))
        .into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SwitchRegionRequest {
    pub arn: String,
}

/// GET /credentials/{id}/models —— 探测该凭据当前可用的模型列表（选中令牌后手动触发）。
pub async fn probe_available_models(
    State(state): State<AdminState>,
    Path(id): Path<u64>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    // 可选 ?models=a,b,c 指定要测的模型；不传则用默认候选清单。
    let models: Option<Vec<String>> = params.get("models").map(|s| {
        s.split(',')
            .map(|m| m.trim().to_string())
            .filter(|m| !m.is_empty())
            .collect()
    });
    match state.service.probe_models(id, models).await {
        Ok((detail, total_credits)) => {
            let items: Vec<serde_json::Value> = detail
                .into_iter()
                .map(|(model, status, credits)| {
                    serde_json::json!({ "model": model, "status": status, "credits": credits })
                })
                .collect();
            Json(serde_json::json!({
                "id": id,
                "models": items,
                "totalCredits": total_credits,
            }))
            .into_response()
        }
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// GET /api/admin/credentials/:id/export
/// 导出指定凭据的原始 JSON（令牌下载，含敏感字段）
pub async fn export_credential(
    State(state): State<AdminState>,
    Path(id): Path<u64>,
) -> impl IntoResponse {
    match state.service.export_credential(id) {
        Ok(cred) => Json(cred).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// GET /api/admin/config/load-balancing
/// 获取负载均衡模式
pub async fn get_load_balancing_mode(State(state): State<AdminState>) -> impl IntoResponse {
    let response = state.service.get_load_balancing_mode();
    Json(response)
}

/// PUT /api/admin/config/load-balancing
/// 设置负载均衡模式
pub async fn set_load_balancing_mode(
    State(state): State<AdminState>,
    Json(payload): Json<SetLoadBalancingModeRequest>,
) -> impl IntoResponse {
    match state.service.set_load_balancing_mode(payload) {
        Ok(response) => Json(response).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// GET /api/admin/recovery-metrics
/// 自愈机器可观测:进程级计数器(刷新 ok/fail、failover 跳数/耗尽、自动禁用死号、冷却触发、
/// region 重探 ok/fail、泄漏 token 清洗)+ uptimeMs。不持久化(自进程启动的健康信号,重启归零)。
/// 零上游、零副作用,把刷新/failover/清洗机器从黑箱变成可查。
pub async fn recovery_metrics() -> impl IntoResponse {
    Json(crate::common::recovery_metrics::snapshot())
}

// ============ 网页上号（Social OAuth）============

use super::service::PollResult;
use super::types::{PollSocialLoginResponse, StartSocialLoginRequest, StartSocialLoginResponse};
use crate::kiro::auth::social::OAuthCallbackData;
// `Query` 已在文件顶部的 axum::extract 里导入（余额端点的 force 参数用它）。
use std::collections::HashMap;

/// POST /api/admin/auth/social/start
/// 发起网页上号，返回 portal_url 供浏览器登录
pub async fn start_social_login(
    State(state): State<AdminState>,
    Json(payload): Json<StartSocialLoginRequest>,
) -> impl IntoResponse {
    match state
        .service
        .start_social_login(payload.priority, payload.proxy_url)
    {
        Ok(result) => Json(StartSocialLoginResponse {
            session_id: result.session_id,
            portal_url: result.portal_url,
        })
        .into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// POST /api/admin/auth/social/poll/:session_id
/// 轮询登录状态；完成时凭据已自动加入池
pub async fn poll_social_login(
    State(state): State<AdminState>,
    Path(session_id): Path<String>,
) -> impl IntoResponse {
    let resp = match state.service.poll_social_login(&session_id).await {
        PollResult::Pending => PollSocialLoginResponse {
            status: "pending".to_string(),
            credential_id: None,
            email: None,
            message: None,
        },
        PollResult::Done {
            credential_id,
            email,
        } => PollSocialLoginResponse {
            status: "done".to_string(),
            credential_id: Some(credential_id),
            email,
            message: None,
        },
        PollResult::Error(msg) => PollSocialLoginResponse {
            status: "error".to_string(),
            credential_id: None,
            email: None,
            message: Some(msg),
        },
    };
    Json(resp)
}

/// GET /api/admin/auth/callback
/// 远程回调模式：浏览器 OAuth 回调落点（**无需鉴权**，由 state 关联会话）
pub async fn social_callback(
    State(state): State<AdminState>,
    Query(params): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    use axum::http::header;
    use axum::response::Html;

    // 有 error 参数 → 失败页
    if let Some(err) = params
        .get("error_description")
        .or_else(|| params.get("error"))
    {
        let body = format!(
            "<html><head><meta charset='utf-8'><title>登录失败</title></head><body style='font-family:sans-serif;text-align:center;padding:60px'><h2>&#10007; 登录失败</h2><p>{}</p><p style='color:#888;font-size:13px'>请关闭此标签页并重试。</p></body></html>",
            html_escape(err)
        );
        return (
            [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
            Html(body),
        );
    }

    let code = params.get("code").cloned().unwrap_or_default();
    let oauth_state = params.get("state").cloned().unwrap_or_default();
    let login_option = params.get("login_option").cloned().unwrap_or_default();

    let delivered = if code.is_empty() {
        false
    } else {
        state.service.deliver_social_callback(OAuthCallbackData {
            code,
            login_option,
            path: "/api/admin/auth/callback".to_string(),
            state: oauth_state,
        })
    };

    let body = if delivered {
        "<html><head><meta charset='utf-8'><title>登录成功</title></head><body style='font-family:sans-serif;text-align:center;padding:60px'><h2>&#10003; 登录成功</h2><p>Token 已更新，请返回 Kiro Admin UI。</p><p style='color:#888;font-size:13px'>此标签页可以关闭。</p></body></html>".to_string()
    } else {
        "<html><head><meta charset='utf-8'><title>登录异常</title></head><body style='font-family:sans-serif;text-align:center;padding:60px'><h2>登录会话未匹配</h2><p>可能已超时，请返回 Admin UI 重新发起。</p></body></html>".to_string()
    };
    (
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        Html(body),
    )
}

/// 极简 HTML 转义，避免回调错误信息注入
fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

// ============ IDC (AWS SSO) 上号 ============

use super::idc_login::IdcPollResult;

/// POST /api/admin/auth/idc/start
/// 发起 IDC device code 上号
pub async fn start_idc_login(
    State(state): State<AdminState>,
    Json(payload): Json<StartIdcLoginRequest>,
) -> impl IntoResponse {
    let region = payload.region.as_deref().unwrap_or("us-east-1");
    // 安全(M1):region 直接拼进 oidc.{region}.amazonaws.com 出站 host,必须白名单校验,
    // 否则持 admin key 传 region=us-east-1.attacker.com 可把 OIDC 注册/设备授权引到攻击者子域。
    // 与 external_idp 端点白名单、凭据 region 字段校验同口径。
    if !crate::kiro::model::credentials::KiroCredentials::is_supported_region(region) {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            Json(super::types::AdminErrorResponse::invalid_request(format!(
                "非法 region: {region}（不在支持的 AWS region 白名单内）"
            ))),
        )
            .into_response();
    }
    match state
        .service
        .start_idc_login(
            &payload.start_url,
            region,
            payload.priority,
            payload.proxy_url,
        )
        .await
    {
        Ok(result) => Json(serde_json::json!({
            "session_id": result.session_id,
            "verification_uri": result.verification_uri,
            "verification_uri_complete": result.verification_uri_complete,
            "user_code": result.user_code,
            "expires_in": result.expires_in,
        }))
        .into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// POST /api/admin/auth/idc/poll/:session_id
/// 轮询 IDC 上号状态
pub async fn poll_idc_login(
    State(state): State<AdminState>,
    Path(session_id): Path<String>,
) -> impl IntoResponse {
    let resp = match state.service.poll_idc_login(&session_id).await {
        IdcPollResult::Pending => serde_json::json!({
            "status": "pending",
        }),
        IdcPollResult::Done { credential_id } => serde_json::json!({
            "status": "done",
            "credential_id": credential_id,
        }),
        IdcPollResult::Expired => serde_json::json!({
            "status": "expired",
            "message": "授权已超时，请重新发起",
        }),
        IdcPollResult::Error(msg) => serde_json::json!({
            "status": "error",
            "message": msg,
        }),
    };
    Json(resp)
}

#[derive(Deserialize)]
pub struct StartIdcLoginRequest {
    pub start_url: String,
    pub region: Option<String>,
    #[serde(default = "default_priority")]
    pub priority: u32,
    pub proxy_url: Option<String>,
}

/// POST /api/admin/auth/external-idp/start
/// 外部 IdP（Microsoft）上号 · 第 1 步：返回 session_id + Kiro signin URL。
pub async fn start_external_idp_login(
    State(state): State<AdminState>,
    Json(payload): Json<StartExternalIdpLoginRequest>,
) -> impl IntoResponse {
    match state.service.start_external_idp_login(
        payload.priority,
        payload.proxy_url,
        payload.region,
    ) {
        Ok(result) => Json(serde_json::json!({
            "sessionId": result.session_id,
            "signinUrl": result.signin_url,
        }))
        .into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// POST /api/admin/auth/external-idp/leg1
/// 第 2 步：粘回 portal 回调 URL，返回 IdP authorize URL。
pub async fn external_idp_leg1(
    State(state): State<AdminState>,
    Json(payload): Json<ExternalIdpPasteRequest>,
) -> impl IntoResponse {
    match state
        .service
        .submit_external_idp_leg1(&payload.session_id, &payload.url)
        .await
    {
        Ok(result) => Json(serde_json::json!({
            "authorizeUrl": result.authorize_url,
        }))
        .into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// POST /api/admin/auth/external-idp/leg2
/// 第 3 步：粘回授权回调 URL，换 token + 探测多 region profile。
/// 返回 `{ credentialId, profiles: [{arn, region, account}] }`：
/// - profiles 多个 → 前端弹窗选 region，随后调 leg2/select 建号（credentialId 为 null）。
/// - profiles 恰 1 个 → 后端已自动建号，credentialId 有值，前端直接完成。
pub async fn external_idp_leg2(
    State(state): State<AdminState>,
    Json(payload): Json<ExternalIdpPasteRequest>,
) -> impl IntoResponse {
    match state
        .service
        .submit_external_idp_leg2(&payload.session_id, &payload.url)
        .await
    {
        Ok(result) => Json(serde_json::json!({
            "credentialId": result.credential_id,
            "profiles": result.profiles.iter().map(|p| serde_json::json!({
                "arn": p.arn,
                "region": p.region,
                "account": p.account,
                "usable": p.usable,
                "subscriptionTitle": p.subscription_title,
            })).collect::<Vec<_>>(),
        }))
        .into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// POST /api/admin/auth/external-idp/leg2/select
/// 第 3 步选定：从多 region profile 里选一个 arn，用暂存 token 建号入池。
pub async fn external_idp_leg2_select(
    State(state): State<AdminState>,
    Json(payload): Json<ExternalIdpSelectRequest>,
) -> impl IntoResponse {
    match state
        .service
        .submit_external_idp_leg2_select(&payload.session_id, &payload.arn)
        .await
    {
        Ok(result) => Json(serde_json::json!({
            "credentialId": result.credential_id,
        }))
        .into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExternalIdpSelectRequest {
    pub session_id: String,
    pub arn: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StartExternalIdpLoginRequest {
    #[serde(default = "default_priority")]
    pub priority: u32,
    pub proxy_url: Option<String>,
    /// 优先探测区域（可选）：并入授权后的多 region profile 探测候选并排头，覆盖冷门 region。
    /// 非白名单值忽略（退回默认候选表），不直接拼进上游 host（region 仍由 ARN 严格解析）。
    pub region: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExternalIdpPasteRequest {
    pub session_id: String,
    /// 用户粘回的浏览器地址栏整串 URL（或 query 片段）。
    pub url: String,
}

fn default_priority() -> u32 {
    // 默认 0:所有号平权(priority 越小越优先,0 即最高且彼此相等)。
    // dwgx:新号默认 100 没必要,都 0 就行,想区分优先级再手动改。
    0
}

// ============ 运维：一键重启 / 存储统计与清理 ============

/// POST /api/admin/service/restart
/// 一键重启本服务（detached）。先返回 200，再由脱离子进程约 1 秒后执行 systemctl restart。
///
/// ⚠️ 重启瞬间本服务断连是预期行为（网关自身流量可能也经由本端点，重启会短暂中断）。
pub async fn restart_service(State(state): State<AdminState>) -> impl IntoResponse {
    match state.service.restart_service() {
        Ok(_) => Json(SuccessResponse::new(
            "重启已发起，数秒后服务恢复（本次连接会短暂中断，属正常）",
        ))
        .into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// GET /api/admin/storage/stats
/// 分区磁盘/内存占用统计（trace.db / usage jsonl / trash / 背景图内存池）
pub async fn storage_stats(State(state): State<AdminState>) -> impl IntoResponse {
    Json(state.service.storage_stats(state.trace_db.as_ref()))
}

/// POST /api/admin/storage/cleanup
/// 按 target 白名单 + 可选时间窗口清理数据（路径全部从 config 派生，防穿越）
///
/// `purgeAll=true` 时忽略 `olderThanDays`，清空该分区全部条目（回收站的「全部清空」）。
pub async fn storage_cleanup(
    State(state): State<AdminState>,
    Json(payload): Json<super::types::StorageCleanupRequest>,
) -> impl IntoResponse {
    match state.service.storage_cleanup(
        &payload.target,
        payload.older_than_days,
        payload.purge_all,
        state.trace_db.as_ref(),
    ) {
        Ok(resp) => Json(resp).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

// ============ 服务端配置 ============

/// GET /api/admin/config
/// 返回服务端配置快照（敏感字段已脱敏）
pub async fn get_config(State(state): State<AdminState>) -> impl IntoResponse {
    Json(state.service.get_config_snapshot())
}

/// PUT /api/admin/config
/// 更新服务端配置（仅提交的字段被修改并持久化）
pub async fn update_config(
    State(state): State<AdminState>,
    Json(payload): Json<super::types::UpdateConfigRequest>,
) -> impl IntoResponse {
    match state.service.update_config(payload) {
        Ok(resp) => Json(resp).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

// ============ OTA 自更新（GitHub 版本检查 + 一键升级）============

/// GET /api/admin/update/check
/// 检查是否有新版本（多镜像回退拉 GitHub tags，semver 比较；只读、不改任何文件）。
pub async fn check_update(State(_state): State<AdminState>) -> impl IntoResponse {
    Json(super::update::check_for_updates().await)
}

/// GET /api/admin/update/status
/// OTA 观测（只读）：读 exe 同目录的 .health/.bak/*.failed 标记，报告本版是否已稳定确认、
/// 回滚点是否还在、是否发生过自动回滚。供前端在一键升级后轮询显示「已升级到 vX」或
/// 「升级失败已自动回滚」。
pub async fn update_status(State(_state): State<AdminState>) -> impl IntoResponse {
    Json(crate::common::health_marker::read_status())
}

/// POST /api/admin/update/perform
/// 一键升级：下载新二进制 + sha256 校验 + 备份 + 原子替换，成功后触发一键重启拉起新版本。
/// body 可选 `{ "version": "v1.2.3" }`（不传=升级到最新）。
pub async fn perform_update(
    State(state): State<AdminState>,
    Json(payload): Json<UpdatePerformRequest>,
) -> impl IntoResponse {
    match super::update::perform_update(payload.version).await {
        Ok(result) => {
            // 替换成功且确有更新 → 复用一键重启（exit(0)→systemd 拉起新二进制）。
            if result.updated {
                let _ = state.service.restart_service();
            }
            Json(result).into_response()
        }
        Err(e) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "success": false, "message": format!("升级失败: {e}") })),
        )
            .into_response(),
    }
}

#[derive(Deserialize, Default)]
pub struct UpdatePerformRequest {
    #[serde(default)]
    pub version: Option<String>,
}

#[cfg(test)]
mod guard_tests {
    /// ⭐ 源码级守卫：推号开关的闸门必须在 `parse_import_keys_request` **之前**。
    ///
    /// ⚠️ 名字里的 "body_parse" 指的是**解析成导入项**那一步，**不是**读请求体：
    /// handler 签名是 `Json(payload): Json<Value>`，axum 提取器先于函数体运行，
    /// 所以字节早已被读完并反序列化。本守卫保证的是「不解析成导入项、不碰号池」。
    ///
    /// 单测覆盖不到（handler 需要完整 AdminState + 真实号池），故用源码断言。
    ///
    /// 回退即 FAIL：把 `import_keys_enabled()` 那道 `if` 移到
    /// `parse_import_keys_request` 之后 —— 那样开关虽然仍会返 403，但**已经解析并
    /// 校验过一批 key**，等于关掉开关后仍为对接方做一遍工作；若将来有人在解析处
    /// 加副作用（写日志/落库/去重表），关掉的入口就会继续产生副作用。
    #[test]
    fn import_keys_gate_precedes_body_parse() {
        let src = include_str!("handlers.rs");
        // needle 运行时拼接：写成完整字面量会被 include_str! 读到自己而多算一处。
        let gate = format!("{}{}", "state.service.import_keys_enabled", "()");
        let parse = format!("{}{}", "parse_import_keys_request", "(&payload)");

        let gate_at = src.find(gate.as_str()).expect("推号开关闸门不应被改名");
        let parse_at = src.find(parse.as_str()).expect("解析调用点不应被改名");
        assert!(
            gate_at < parse_at,
            "开关闸门必须在解析请求体之前，否则关掉入口后仍会为对接方解析并校验一批 key"
        );
    }
}
