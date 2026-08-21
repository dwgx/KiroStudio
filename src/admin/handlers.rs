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
        AddCredentialRequest, BatchDeleteRequest, BatchDeleteResponse, BatchIdsRequest,
        BatchOpResponse, BatchSetAllowedModelsRequest, BatchSetDisabledRequest,
        CleanupDisabledRequest,
        CloneCredentialRequest, SetAllowedModelsRequest, SetApiRegionRequest,
        SetCustomApiConfigRequest, SetDisabledRequest,
        SetEndpointRequest, SetLoadBalancingModeRequest, SetModelMappingExemptRequest,
        SetPriorityRequest, SetRpmLimitRequest, RefreshTokenRequest,
        SuccessResponse, parse_import_keys_request,
    },
};

/// GET /api/admin/credentials
/// 获取所有凭据状态
pub async fn get_all_credentials(State(state): State<AdminState>) -> impl IntoResponse {
    let response = state.service.get_all_credentials();
    Json(response)
}

/// GET /api/admin/credentials/export-kam
/// 导出凭据为 KAM 兼容 JSON（含 refreshToken 等**明文敏感字段**）
///
/// ⚠️ 敏感操作：响应体含明文 token，前端拿到后应直接触发浏览器下载，
/// 不要落库/进日志。可选 query 参数 `ids`（逗号分隔）限定导出范围，
/// 省略则导出全部。鉴权沿用 admin 路由统一鉴权（本路由在 authed 树内）。
pub async fn export_kam_credentials(
    State(state): State<AdminState>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    let id_filter: Option<std::collections::HashSet<u64>> = match params
        .get("ids")
        .map(|raw| {
            // ⚠️ 2026-08-14 对抗审查 MAJOR-1：`ids` 出现但解析结果为空（全非法段/空串）
            // 必须报 400，绝不静默退化为**全量明文导出**——本端点响应含明文 token，
            // 一个笔误（?ids=abc / ?ids=）让全池 token 出站是最坏的失败模式。
            // 解析出的集合为空即视为格式错误（合法空串本身无意义：导出全量请省略
            // 该参数，这是显式契约，与 BatchDelete 的严格解析惯例一致）。
            let parsed: std::collections::HashSet<u64> = raw
                .split(',')
                .filter_map(|s| {
                    let t = s.trim();
                    if t.is_empty() {
                        None
                    } else {
                        t.parse::<u64>().ok()
                    }
                })
                .collect();
            if parsed.is_empty() {
                return Err(());
            }
            Ok(parsed)
        })
        .transpose()
    {
        Ok(v) => v,
        Err(()) => {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                Json(super::types::AdminErrorResponse::invalid_request(
                    "ids 参数格式错误：需要逗号分隔的数字 ID",
                )),
            )
                .into_response();
        }
    };

let response = state.service.export_kam_credentials(id_filter.as_ref());
    // MINOR-4（2026-08-14）：明文 token 响应禁止缓存（共享代理/浏览器不留副本）。
    (
        [("Cache-Control", "no-store")],
        Json(response),
    )
        .into_response()
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

/// POST /api/admin/credentials/:id/reprobe-region
///
/// 手动重探该号上游实际生效的 region 并写回凭据（救「自动探测探错」的最后一招）。
/// 失败只报错、**绝不**动禁用态（服役号探测失败被禁 = 把好号打掉，见 service 文档）。
pub async fn reprobe_credential_region(
    State(state): State<AdminState>,
    Path(id): Path<u64>,
) -> impl IntoResponse {
    match state.service.reprobe_api_region(id).await {
        Ok(resp) => Json(resp).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// PUT /api/admin/credentials/:id/refresh-token —— 手动更新 OAuth 号的 refreshToken（2026-08-11）。
///
/// ⚠️ 敏感值纪律：请求体里的 refreshToken 绝不进日志/错误消息/响应体。
pub async fn update_credential_refresh_token(
    State(state): State<AdminState>,
    Path(id): Path<u64>,
    Json(payload): Json<RefreshTokenRequest>,
) -> impl IntoResponse {
    match state.service.update_refresh_token(id, payload.refresh_token) {
        Ok(_) => Json(SuccessResponse::new(format!("凭据 #{} refreshToken 已更新", id))).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// GET /api/admin/credentials/:id/upstream-models —— 探测代挂上游可用模型列表（custom_api 专属）。
pub async fn probe_upstream_models(
    State(state): State<AdminState>,
    Path(id): Path<u64>,
) -> impl IntoResponse {
    match state.service.probe_upstream_models(id).await {
        Ok(models) => Json(serde_json::json!({ "models": models })).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// POST /api/admin/credentials/probe-models —— 创建前探测代挂上游模型列表（不依赖凭据 id）。
///
/// 与 `GET /credentials/{id}/upstream-models` 的区别：凭据还不存在时用于创建表单的临时探测。
/// body: `{ "baseUrl": "https://...", "apiKey": "sk-..." }`；不持久化任何东西。
pub async fn probe_models_standalone(
    State(state): State<AdminState>,
    Json(payload): Json<crate::admin::types::ProbeModelsRequest>,
) -> impl IntoResponse {
    let base_url = payload.base_url.as_deref().unwrap_or_default();
    match state
        .service
        .probe_models_standalone(base_url, payload.api_key.as_deref())
        .await
    {
        Ok(models) => Json(serde_json::json!({ "models": models })).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// POST /api/admin/credentials/:id/model-mapping-exempt  body: `{ "modelMappingExempt": true }`
///
/// 设置凭据的模型映射豁免开关（跳过全局 `config.model_mapping`）。
pub async fn set_credential_model_mapping_exempt(
    State(state): State<AdminState>,
    Path(id): Path<u64>,
    Json(payload): Json<SetModelMappingExemptRequest>,
) -> impl IntoResponse {
    let exempt = payload.model_mapping_exempt.unwrap_or(false);
    match state
        .service
        .set_credential_model_mapping_exempt(id, payload.model_mapping_exempt)
    {
        Ok(_) => {
            let msg = if exempt {
                format!("凭据 #{} 已豁免全局模型映射", id)
            } else {
                format!("凭据 #{} 已恢复应用全局模型映射", id)
            };
            Json(SuccessResponse::new(msg)).into_response()
        }
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// POST /api/admin/credentials/:id/endpoint
/// 固定该凭据走的端点（`ide` / `cli`）；传 null 或空串清除，回到自动路由。
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
///
/// `pub(super)` 而非私有：后台代理池健康调度（`service.rs::probe_socks_node`）复用
/// 同一个常量，保证「手动测活」与「自动调度」访问的是同一个目标——各写一份必然漂移，
/// 而漂移的那一份就是可被指使的出站。
pub(super) const PROXY_TEST_PROBE_URL: &str = "https://api.ipify.org?format=json";

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

/// POST /api/admin/credentials/:id/relogin
///
/// OAuth 类凭据（idc / social / external_idp）的「自助复活」：清空全部进程内惩罚
/// 状态（失败计数/冷却/限流器）并重新启用。api_key 与代挂号拒绝（无此概念）。
pub async fn relogin_oauth(
    State(state): State<AdminState>,
    Path(id): Path<u64>,
) -> impl IntoResponse {
    match state.service.relogin_oauth(id) {
        Ok(_) => Json(SuccessResponse::new(format!(
            "凭据 #{} 已复活（惩罚状态已清空并重新启用）",
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
/// —— adminKey 在 sessionStorage（读取时清 localStorage 残留）且文档带 CSP，
/// 无上限的批量删除仍会放大 XSS 的破坏面。
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

fn prepare_batch_ids(ids: Vec<u64>) -> Result<Vec<u64>, axum::response::Response> {
    if ids.is_empty() {
        return Err((
            axum::http::StatusCode::BAD_REQUEST,
            Json(super::types::AdminErrorResponse::invalid_request(
                "ids 不能为空".to_string(),
            )),
        )
            .into_response());
    }
    if ids.len() > MAX_BATCH_DELETE_IDS {
        return Err((
            axum::http::StatusCode::BAD_REQUEST,
            Json(super::types::AdminErrorResponse::invalid_request(format!(
                "一次最多操作 {} 个凭据（本次 {}）",
                MAX_BATCH_DELETE_IDS,
                ids.len()
            ))),
        )
            .into_response());
    }
    let mut seen = std::collections::HashSet::new();
    Ok(ids.into_iter().filter(|id| seen.insert(*id)).collect())
}

/// POST /api/admin/credentials/batch-reset
///
/// 批量重置失败计数并重新启用。**部分失败仍返 200**，逐条标 ok/error。
pub async fn reset_credentials_batch(
    State(state): State<AdminState>,
    Json(payload): Json<BatchIdsRequest>,
) -> impl IntoResponse {
    let ids = match prepare_batch_ids(payload.ids) {
        Ok(ids) => ids,
        Err(resp) => return resp,
    };
    Json(BatchOpResponse::from_results(
        state.service.reset_credentials_batch(&ids),
    ))
    .into_response()
}

/// POST /api/admin/credentials/batch-disabled
///
/// 批量启用/禁用。**部分失败仍返 200**，逐条标 ok/error。
pub async fn set_credentials_disabled_batch(
    State(state): State<AdminState>,
    Json(payload): Json<BatchSetDisabledRequest>,
) -> impl IntoResponse {
    let ids = match prepare_batch_ids(payload.ids) {
        Ok(ids) => ids,
        Err(resp) => return resp,
    };
    Json(BatchOpResponse::from_results(
        state.service.set_disabled_batch(&ids, payload.disabled),
    ))
    .into_response()
}

/// POST /api/admin/credentials/batch-allowed-models
///
/// 批量设置允许模型白名单（空/null = 不限制）。**部分失败仍返 200**，逐条标 ok/error。
pub async fn set_credentials_allowed_models_batch(
    State(state): State<AdminState>,
    Json(payload): Json<BatchSetAllowedModelsRequest>,
) -> impl IntoResponse {
    let ids = match prepare_batch_ids(payload.ids) {
        Ok(ids) => ids,
        Err(resp) => return resp,
    };
    Json(BatchOpResponse::from_results(
        state
            .service
            .set_allowed_models_batch(&ids, payload.allowed_models.clone()),
    ))
    .into_response()
}

/// POST /api/admin/credentials/batch-refresh
///
/// 批量强制刷新 Token。**部分失败仍返 200**，逐条标 ok/error。
pub async fn force_refresh_tokens_batch(
    State(state): State<AdminState>,
    Json(payload): Json<BatchIdsRequest>,
) -> impl IntoResponse {
    let ids = match prepare_batch_ids(payload.ids) {
        Ok(ids) => ids,
        Err(resp) => return resp,
    };
    Json(BatchOpResponse::from_results(
        state.service.force_refresh_tokens_batch(&ids).await,
    ))
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

/// POST /api/admin/credentials/disable-quota-exceeded
///
/// 一键禁用所有「余额已超额」的启用号（`remaining <= 0`，数据源 = 余额缓存，零上游）。
/// 排除代挂号与已禁用号；单号失败不炸整批，逐条看 `results[].ok`（与批量删除同款）。
pub async fn disable_quota_exceeded(
    State(state): State<AdminState>,
) -> impl IntoResponse {
    Json(state.service.disable_quota_exceeded())
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

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelsTestRequest {
    pub credential_id: u64,
    pub models: Option<Vec<String>>,
}

/// POST /api/admin/models/test —— 对指定凭据做模型实测（复用 `probe_models` 极小请求）。
pub async fn test_models(
    State(state): State<AdminState>,
    Json(payload): Json<ModelsTestRequest>,
) -> impl IntoResponse {
    match state
        .service
        .probe_models(payload.credential_id, payload.models)
        .await
    {
        Ok((detail, total_credits)) => {
            let items: Vec<serde_json::Value> = detail
                .into_iter()
                .map(|(model, status, credits)| {
                    serde_json::json!({ "model": model, "status": status, "credits": credits })
                })
                .collect();
            Json(serde_json::json!({
                "id": payload.credential_id,
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
        // 明文 token 响应禁止缓存（共享代理/浏览器不留副本）——与
        // export_kam_credentials 的 MINOR-4 同款（2026-08-14）。
        Ok(cred) => ([("Cache-Control", "no-store")], Json(cred)).into_response(),
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

/// GET /api/admin/endpoint-health
/// 端点自适应派发的可观测面：每 `(凭据, 端点)` 的实测 EWMA 成功率与样本数。
///
/// # 为什么这个端点是必须的
///
/// 派发决策依赖统计量，而统计量不可见就等于**不可调、不可证**。本仓有过直接的教训
/// （CLAUDE.md 记载「先修度量，再谈调参」：一个关键容量数字是配置自乘出来的假值，
/// 导致所有依赖它的自动调节都在算空气，而三层监控全绿）。所以派发上线的同时必须
/// 有这一面 —— 否则「某个号为什么总走 runtime.* 而不走 q.*」无从回答。
///
/// 零上游、零副作用，只读进程内内存表。表不持久化（重启从先验重新学习，理由见
/// `endpoint_health` 模块文档），故重启后本端点会短暂返回空数组，这是预期行为。
pub async fn endpoint_health() -> impl IntoResponse {
    let snap = crate::kiro::endpoint_health::shared().snapshot();
    #[derive(serde::Serialize)]
    #[serde(rename_all = "camelCase")]
    struct Item {
        credential_id: u64,
        endpoint: String,
        /// EWMA 成功率 [0,1]；`null` = 该组合尚无样本（与「成功率 0」语义不同）。
        success_rate: Option<f64>,
        samples: u64,
    }
    #[derive(serde::Serialize)]
    #[serde(rename_all = "camelCase")]
    struct Resp {
        items: Vec<Item>,
        /// 组合总数，便于前端判空。
        total: usize,
    }
    let items: Vec<Item> = snap
        .into_iter()
        .map(|s| Item {
            credential_id: s.credential_id,
            endpoint: s.endpoint,
            success_rate: s.success_rate,
            samples: s.samples,
        })
        .collect();
    let total = items.len();
    Json(Resp { items, total })
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
        Ok(result) => Json(StartIdcLoginResponse {
            session_id: result.session_id,
            verification_uri: result.verification_uri,
            verification_uri_complete: result.verification_uri_complete,
            user_code: result.user_code,
            expires_in: result.expires_in,
        })
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
        IdcPollResult::Pending => PollIdcLoginResponse {
            status: "pending".to_string(),
            credential_id: None,
            message: None,
        },
        IdcPollResult::Done { credential_id } => PollIdcLoginResponse {
            status: "done".to_string(),
            credential_id: Some(credential_id),
            message: None,
        },
        IdcPollResult::Expired => PollIdcLoginResponse {
            status: "expired".to_string(),
            credential_id: None,
            message: Some("授权已超时，请重新发起".to_string()),
        },
        IdcPollResult::Error(msg) => PollIdcLoginResponse {
            status: "error".to_string(),
            credential_id: None,
            message: Some(msg),
        },
    };
    Json(resp)
}

/// IDC 上号请求。`rename_all=camelCase` 与其它 admin 端点对齐；
/// `alias` 接受旧 snake_case（admin-ui 仍 post `start_url` / `proxy_url`）。
#[derive(Debug, Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StartIdcLoginRequest {
    #[serde(alias = "start_url")]
    pub start_url: String,
    pub region: Option<String>,
    #[serde(default = "default_priority")]
    pub priority: u32,
    #[serde(alias = "proxy_url")]
    pub proxy_url: Option<String>,
}

/// IDC 上号响应。线协议 camelCase；alias 让旧 snake_case JSON 仍能反序列化。
#[derive(Debug, Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StartIdcLoginResponse {
    #[serde(alias = "session_id")]
    pub session_id: String,
    #[serde(alias = "verification_uri")]
    pub verification_uri: String,
    #[serde(skip_serializing_if = "Option::is_none", alias = "verification_uri_complete")]
    pub verification_uri_complete: Option<String>,
    #[serde(alias = "user_code")]
    pub user_code: String,
    #[serde(alias = "expires_in")]
    pub expires_in: u64,
}

/// IDC 轮询响应。线协议 camelCase；alias 接受旧 `credential_id`。
#[derive(Debug, Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PollIdcLoginResponse {
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none", alias = "credential_id")]
    pub credential_id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
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
            // 回显实际建号用的 arn/region（用户选的 profile 可能被替换为同账号
            // 可用 profile，前端需要展示最终落点，见 ExternalIdpSelectResult 文档）。
            "arn": result.arn,
            "region": result.region,
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

// ============ SSO Token 导入（粘贴 AWS portal Bearer Token 静默换号）============

/// POST /api/admin/credentials/import-sso
/// 粘贴 AWS portal 的 Bearer Token（x-amz-sso_authn），服务端走完整设备授权流程
/// 换取标准 IdC 凭据入池（免浏览器授权的人工步骤，移植自 Kiro-Go）。
///
/// body: `{ token, region?, priority?, proxyUrl? }`
/// 返回: `{ credentialId, email }`
///
/// ⚠️ 安全：token 全程不落日志、不落盘（单次用途，仅本流程内使用）；region 必须
/// 在 Kiro region 白名单内（service 层校验，防污染值拼坏出站 host）。
pub async fn import_sso_token(
    State(state): State<AdminState>,
    Json(payload): Json<ImportSsoTokenRequest>,
) -> impl IntoResponse {
    match state
        .service
        .import_sso_token(payload.token, payload.region, payload.priority, payload.proxy_url)
        .await
    {
        Ok(result) => Json(serde_json::json!({
            "credentialId": result.credential_id,
            "email": result.email,
        }))
        .into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportSsoTokenRequest {
    /// AWS portal 的 Bearer Token（x-amz-sso_authn）。
    pub token: String,
    /// 导入 region（缺省 us-east-1；必须命中 Kiro region 白名单）。
    pub region: Option<String>,
    #[serde(default = "default_priority")]
    pub priority: u32,
    /// 上号时显式填的代理（仅此项持久化到新凭据）。
    pub proxy_url: Option<String>,
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

/// GET /api/admin/diagnostics/snapshot
/// 运维诊断一键聚合：版本 / 逐号状态（禁用/冷却/健康分/余额）/ 代理池健康 /
/// 关键配置摘要（脱敏）/ uptime / RSS。纯观测端点，前端不强制接。
pub async fn diagnostics_snapshot(State(state): State<AdminState>) -> impl IntoResponse {
    Json(state.service.diagnostics_snapshot().await)
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

/// GET /api/admin/error-messages/defaults
/// 返回错误码/提示词**内置默认表**全量：key → {status, type, message, retryAfterSecs}。
///
/// 数据源 = `crate::model::error_messages::default_error_messages()`（**运行期读取**，
/// key 集随默认表演进自动同步，不硬编码）。只读，供前端「默认值预览」：
/// 弹窗把默认表与 `GET /config` 的 `errorMessages`（配置覆盖）合并渲染全量 key。
/// 响应契约 camelCase（retryAfterSecs；无 Retry-After 的条目为 null）。
pub async fn get_error_message_defaults() -> impl IntoResponse {
    let table = crate::model::error_messages::default_error_messages();
    let obj = table
        .iter()
        .map(|(key, status, ty, message, retry_after)| {
            (
                key.to_string(),
                serde_json::json!({
                    "status": status,
                    "type": ty,
                    "message": message,
                    "retryAfterSecs": retry_after,
                }),
            )
        })
        .collect::<serde_json::Map<String, serde_json::Value>>();
    Json(serde_json::Value::Object(obj))
}

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

/// GET /api/admin/config/export
/// 导出当前配置（整份 JSON，敏感字段省略——脱敏清单见 AdminService::export_config）。
pub async fn export_config(State(state): State<AdminState>) -> impl IntoResponse {
    match state.service.export_config() {
        Ok(v) => Json(v).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// POST /api/admin/config/import
/// 导入整份配置（先校验后写盘，失败不破坏现有配置；敏感字段省略时继承现值）。
pub async fn import_config(
    State(state): State<AdminState>,
    Json(payload): Json<serde_json::Value>,
) -> impl IntoResponse {
    match state.service.import_config(payload) {
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
        // 分类错误码（对齐 AdminServiceError::status_code 模式）：400 入参非法 /
        // 409 环境或版本冲突 / 422 数据无效 / 502 上游不可达 / 500 内部。
        // body 形状保持既有契约（success=false + message），仅状态码分类。
        Err(e) => (
            e.status_code(),
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

// ============ 帮助页联网搜索（DuckDuckGo + Bing RSS 兜底）============

/// 单次搜索最多返回的条目数（DDG 与 Bing 兜底共用上限）。
const MAX_WEB_SEARCH_RESULTS: usize = 10;
/// Bing RSS 兜底最多取前 8 条（Bing 条目质量参差，够用即止）。
const BING_FALLBACK_MAX_ITEMS: usize = 8;
/// 结果标题截断长度（按字符截，`chars()` 避免切坏多字节字符）。
const WEB_SEARCH_TITLE_MAX_CHARS: usize = 100;
/// 查询词长度上限（字符数）。
const WEB_SEARCH_Q_MAX_CHARS: usize = 200;

/// GET /api/admin/help/web-search?q=<查询>
/// 帮助页「联网搜索」代理：调 DuckDuckGo Instant Answer（免 key、JSON 稳定），
/// 空结果时兜底 Bing RSS 搜索（见 `parse_bing_rss`）。前端经它绕开 CORS 并复用
/// 服务器出网。
///
/// 鉴权：本端点注册在 authed 路由树内，走统一 admin 鉴权。
/// 无额外频控：面板内使用、量小，只有持有 admin key 的请求能到达这里。
///
/// 失败语义：参数非法（q 空/超长）→ 400；出站请求失败（超时/非 2xx）→ 502。
pub async fn help_web_search(
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    // 参数校验：q 必填、trim 后非空、长度 ≤ 200。
    let q = match params
        .get("q")
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
    {
        Some(q) if q.chars().count() <= WEB_SEARCH_Q_MAX_CHARS => q.to_string(),
        _ => {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                Json(super::types::AdminErrorResponse::invalid_request(
                    "q 参数必填，且长度不超过 200 个字符",
                )),
            )
                .into_response();
        }
    };

    // 查询词进 query，host 固定；仍用 SSRF 守卫（禁 302 跳内网）。
    let ddg_url = format!(
        "https://api.duckduckgo.com/?q={}&format=json&no_html=1&skip_disambig=1",
        urlencoding::encode(&q)
    );
    let client = match crate::common::ssrf::build_guarded_client(
        &ddg_url,
        std::time::Duration::from_secs(8),
        &["https"],
    )
    .await
    {
        Ok(c) => c,
        Err(_) => return web_search_unavailable(),
    };
    let resp = match client.get(&ddg_url).send().await {
        Ok(r) if r.status().is_success() => r,
        _ => return web_search_unavailable(),
    };
    let raw = match resp.text().await {
        Ok(t) => t,
        Err(_) => return web_search_unavailable(),
    };
    let mut items = parse_ddg_response(&raw, &q);

    // 兜底：DDG Instant Answer 覆盖有限、空结果常见——此时**不返回空**，
    // 改调 Bing RSS（服务器在美国可达，format=rss 返回 XML）。
    if items.is_empty() {
        let bing_url = format!(
            "https://www.bing.com/search?q={}&format=rss",
            urlencoding::encode(&q)
        );
        let bing_client = match crate::common::ssrf::build_guarded_client(
            &bing_url,
            std::time::Duration::from_secs(8),
            &["https"],
        )
        .await
        {
            Ok(c) => c,
            Err(_) => return web_search_unavailable(),
        };
        let resp = match bing_client.get(&bing_url).send().await {
            Ok(r) if r.status().is_success() => r,
            _ => return web_search_unavailable(),
        };
        let raw = match resp.text().await {
            Ok(t) => t,
            Err(_) => return web_search_unavailable(),
        };
        items = parse_bing_rss(&raw);
    }

    Json(items).into_response()
}

/// 502 响应：搜索服务不可用（上游超时/失败，前端提示稍后再试）。
fn web_search_unavailable() -> axum::response::Response {
    (
        axum::http::StatusCode::BAD_GATEWAY,
        Json(super::types::AdminErrorResponse::api_error(
            "搜索服务暂时不可用",
        )),
    )
        .into_response()
}

/// 纯函数：DDG Instant Answer JSON → 条目。
///
/// 展平规则：
/// - `AbstractText` 非空 → 第一条（title=查询词、url=AbstractURL 或空、snippet=摘要）
/// - `RelatedTopics[]` 逐项展平：有 `FirstURL` 的取 Text/FirstURL；带嵌套
///   `Topics[]` 的递归展平；无 `FirstURL` 的条目跳过
///
/// 与网络调用解耦（mock 输入串可单测），见 `web_search_tests`。
fn parse_ddg_response(raw: &str, query: &str) -> Vec<super::types::WebSearchItem> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(raw) else {
        return Vec::new();
    };
    let mut out = Vec::new();

    if let Some(abstract_text) = v
        .get("AbstractText")
        .and_then(|t| t.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        out.push(super::types::WebSearchItem {
            title: truncate_search_title(query),
            url: v
                .get("AbstractURL")
                .and_then(|u| u.as_str())
                .filter(|u| !u.is_empty())
                .map(str::to_string),
            snippet: abstract_text.to_string(),
        });
    }

    if let Some(topics) = v.get("RelatedTopics").and_then(|t| t.as_array()) {
        for topic in topics {
            collect_ddg_topic(topic, &mut out);
        }
    }

    out.truncate(MAX_WEB_SEARCH_RESULTS);
    out
}

/// 递归展平单条 DDG topic（含嵌套 `Topics` 数组）。
fn collect_ddg_topic(topic: &serde_json::Value, out: &mut Vec<super::types::WebSearchItem>) {
    if let Some(nested) = topic.get("Topics").and_then(|t| t.as_array()) {
        for t in nested {
            collect_ddg_topic(t, out);
        }
        return;
    }
    let Some(text) = topic
        .get("Text")
        .and_then(|t| t.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        return;
    };
    let Some(url) = topic
        .get("FirstURL")
        .and_then(|u| u.as_str())
        .filter(|u| !u.is_empty())
    else {
        return;
    };
    out.push(super::types::WebSearchItem {
        title: truncate_search_title(text),
        url: Some(url.to_string()),
        snippet: text.to_string(),
    });
}

/// 标题截断：按字符截（`chars()` 而非 bytes，避免切坏多字节字符）。
fn truncate_search_title(s: &str) -> String {
    s.chars().take(WEB_SEARCH_TITLE_MAX_CHARS).collect()
}

/// 纯函数：Bing RSS（`format=rss` 的 XML）→ 条目。
///
/// 项目无 XML 解析依赖，选最简可靠的正则式粗解析：手写 find 扫描
/// `<item>..</item>` 块（Bing 的 title/link/description 均为无属性标签），
/// 再做基础 HTML 实体解码。条目取前 `BING_FALLBACK_MAX_ITEMS` 条。
fn parse_bing_rss(xml: &str) -> Vec<super::types::WebSearchItem> {
    let mut out = Vec::new();
    let mut rest = xml;
    while out.len() < BING_FALLBACK_MAX_ITEMS {
        let Some(open_at) = rest.find("<item>") else {
            break;
        };
        let block_start = open_at + "<item>".len();
        let Some(rel_end) = rest[block_start..].find("</item>") else {
            break;
        };
        let block = &rest[block_start..block_start + rel_end];

        let title = extract_xml_tag(block, "title").unwrap_or_default();
        let url = extract_xml_tag(block, "link");
        // 无标题或链接的条目没有点击价值，跳过。
        if !title.is_empty() && url.as_deref().is_some_and(|u| !u.is_empty()) {
            out.push(super::types::WebSearchItem {
                title: truncate_search_title(&title),
                url,
                snippet: extract_xml_tag(block, "description")
                    .unwrap_or_default()
                    .trim()
                    .to_string(),
            });
        }

        rest = &rest[block_start + rel_end + "</item>".len()..];
    }
    out
}

/// 取块内 `<tag>..</tag>` 首段内容并做基础 HTML 实体解码。
fn extract_xml_tag(block: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = block.find(&open)? + open.len();
    let end = block[start..].find(&close)? + start;
    Some(html_unescape(&block[start..end]))
}

/// 基础 HTML 实体解码（Bing RSS 标题/描述含 `&amp;` 一类实体）。
fn html_unescape(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
}

#[cfg(test)]
mod web_search_tests {
    use super::*;

    /// DDG 解析：AbstractText 打头 + RelatedTopics 展平（含嵌套、无链接跳过）。
    #[test]
    fn ddg_response_flattens_topics() {
        let json = r#"{
            "AbstractText": "Rust 是一门系统编程语言",
            "AbstractURL": "https://www.rust-lang.org/",
            "RelatedTopics": [
                {"Text": "Rust Programming Language", "FirstURL": "https://www.rust-lang.org/"},
                {"Topics": [
                    {"Text": "Nested 条目 A", "FirstURL": "https://example.com/a"},
                    {"Text": "无链接条目被跳过"}
                ]},
                {"Text": "只有文本没有链接"}
            ]
        }"#;
        let items = parse_ddg_response(json, "rust");
        assert_eq!(items.len(), 3);
        // 第一条 = AbstractText（title 用查询词）
        assert_eq!(items[0].title, "rust");
        assert_eq!(items[0].url.as_deref(), Some("https://www.rust-lang.org/"));
        assert!(items[0].snippet.contains("系统编程"));
        // 第二条 = 顶层条目
        assert_eq!(items[1].title, "Rust Programming Language");
        assert_eq!(items[1].url.as_deref(), Some("https://www.rust-lang.org/"));
        // 第三条 = 嵌套展开（无链接的条目被跳过）
        assert_eq!(items[2].title, "Nested 条目 A");
        assert_eq!(items[2].url.as_deref(), Some("https://example.com/a"));
    }

    /// DDG 解析：标题截断 100 字符（按字符截，多字节不切坏）。
    #[test]
    fn ddg_title_truncated_to_100_chars() {
        let long = "长".repeat(120);
        let json = format!(
            r#"{{"RelatedTopics":[{{"Text":"{text}","FirstURL":"https://example.com/x"}}]}}"#,
            text = long
        );
        let items = parse_ddg_response(&json, "q");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].title.chars().count(), WEB_SEARCH_TITLE_MAX_CHARS);
    }

    /// DDG 解析：空结果 / 非法 JSON → 空数组（空数组触发 Bing 兜底）。
    #[test]
    fn ddg_empty_or_invalid_yields_empty() {
        assert!(parse_ddg_response(r#"{"RelatedTopics":[]}"#, "q").is_empty());
        assert!(parse_ddg_response("not json", "q").is_empty());
    }

    /// Bing RSS 解析：条目提取 + 实体解码 + 缺 description 容错 + 无链接跳过。
    #[test]
    fn bing_rss_parses_items() {
        let xml = r#"<?xml version="1.0"?>
        <rss version="2.0"><channel>
        <item><title>Rust &amp; Cargo</title><link>https://example.com/rust</link><description>工具链介绍</description></item>
        <item><title>No Desc</title><link>https://example.com/nodesc</link></item>
        <item><title>No Link</title><description>只有描述</description></item>
        </channel></rss>"#;
        let items = parse_bing_rss(xml);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].title, "Rust & Cargo");
        assert_eq!(items[0].url.as_deref(), Some("https://example.com/rust"));
        assert_eq!(items[0].snippet, "工具链介绍");
        assert_eq!(items[1].title, "No Desc");
        assert_eq!(items[1].snippet, "");
    }

    /// Bing RSS 解析：最多取前 8 条。
    #[test]
    fn bing_rss_caps_at_8_items() {
        let mut xml = String::new();
        for i in 0..12 {
            xml.push_str(&format!(
                "<item><title>t{i}</title><link>https://example.com/{i}</link><description>d</description></item>"
            ));
        }
        let items = parse_bing_rss(&xml);
        assert_eq!(items.len(), BING_FALLBACK_MAX_ITEMS);
    }
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

    /// ⭐ 源码守卫（MINOR-4）：`export_credential` 必须带 `Cache-Control: no-store`。
    ///
    /// 该端点的响应体含**明文 token**；浏览器/共享代理若缓存它，等于把凭据留在
    /// 本不该留的地方（export_kam_credentials 已带同款头，这条锁的是本端点本身）。
    ///
    /// 回退即 FAIL：把响应改回裸 `Json(cred)`（去掉 no-store 头）——断言不命中。
    #[test]
    fn export_credential_must_be_no_store() {
        let src = include_str!("handlers.rs");
        let cut = src.find("#[cfg(test)]").unwrap_or(src.len());
        let prod = &src[..cut];
        let fname = format!("pub async fn export_credential{}", "(");
        let start = prod.find(&fname).expect("export_credential 不应被改名");
        // 切片到下一个函数的文档注释（或测试模块），避免把其它函数的
        // no-store 也算进本函数的判定。
        let tail = &prod[start..];
        let end = tail
            .find("\n/// ")
            .or_else(|| tail.find("\n#[cfg(test)]"))
            .unwrap_or(tail.len());
        let body = &tail[..end];
        let header = format!("[({}, {})]", "\"Cache-Control\"", "\"no-store\"");
        assert!(
            body.contains(header.as_str()),
            "export_credential 必须带 Cache-Control: no-store（响应体含明文 token，\
             缓存会让共享代理/浏览器留下凭据副本）"
        );
    }
}

#[cfg(test)]
mod idc_serde_tests {
    use super::*;

    #[test]
    fn start_idc_login_request_accepts_camel_and_snake() {
        let camel: StartIdcLoginRequest = serde_json::from_str(
            r#"{"startUrl":"https://view.awsapps.com/start","proxyUrl":"socks5://127.0.0.1:1080"}"#,
        )
        .unwrap();
        let snake: StartIdcLoginRequest = serde_json::from_str(
            r#"{"start_url":"https://view.awsapps.com/start","proxy_url":"socks5://127.0.0.1:1080"}"#,
        )
        .unwrap();
        assert_eq!(camel.start_url, snake.start_url);
        assert_eq!(camel.proxy_url, snake.proxy_url);
        assert_eq!(camel.priority, 0);
        assert_eq!(snake.priority, 0);
    }

    #[test]
    fn start_idc_login_response_wire_is_camel_case() {
        let resp = StartIdcLoginResponse {
            session_id: "sess-1".into(),
            verification_uri: "https://example.invalid/device".into(),
            verification_uri_complete: Some("https://example.invalid/device?code=ABCD".into()),
            user_code: "ABCD-EFGH".into(),
            expires_in: 600,
        };
        let v = serde_json::to_value(&resp).unwrap();
        assert!(v.get("sessionId").is_some());
        assert!(v.get("session_id").is_none());
        assert!(v.get("verificationUri").is_some());
        assert!(v.get("verificationUriComplete").is_some());
        assert!(v.get("userCode").is_some());
        assert!(v.get("expiresIn").is_some());

        let from_snake: StartIdcLoginResponse = serde_json::from_value(serde_json::json!({
            "session_id": "sess-2",
            "verification_uri": "https://example.invalid/device",
            "user_code": "WXYZ",
            "expires_in": 30
        }))
        .unwrap();
        assert_eq!(from_snake.session_id, "sess-2");
        assert_eq!(from_snake.user_code, "WXYZ");
        assert_eq!(from_snake.expires_in, 30);
    }

    #[test]
    fn poll_idc_login_response_accepts_credential_id_alias() {
        let from_snake: PollIdcLoginResponse = serde_json::from_value(serde_json::json!({
            "status": "done",
            "credential_id": 7
        }))
        .unwrap();
        assert_eq!(from_snake.credential_id, Some(7));

        let v = serde_json::to_value(&PollIdcLoginResponse {
            status: "done".into(),
            credential_id: Some(7),
            message: None,
        })
        .unwrap();
        assert_eq!(v.get("credentialId").and_then(|x| x.as_u64()), Some(7));
        assert!(v.get("credential_id").is_none());
        assert!(v.get("message").is_none());
    }
}
