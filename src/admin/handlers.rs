//! Admin API HTTP 处理器

use axum::{
    Json,
    extract::{Path, State},
    response::IntoResponse,
};
use serde::Deserialize;

use super::{
    middleware::AdminState,
    types::{
        AddCredentialRequest, SetDisabledRequest, SetLoadBalancingModeRequest, SetPriorityRequest,
        SetRpmLimitRequest,
        SuccessResponse,
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
    Json(SuccessResponse::new(format!("已永久清除 {} 个回收站条目", n)))
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetNameRequest {
    /// 别名/备注;传 null 或空字符串清除
    pub name: Option<String>,
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

/// GET /api/admin/credentials/:id/balance
/// 获取指定凭据的余额
pub async fn get_credential_balance(
    State(state): State<AdminState>,
    Path(id): Path<u64>,
) -> impl IntoResponse {
    match state.service.get_balance(id).await {
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

/// GET /api/admin/credentials/trash
/// 列出回收站中的已删除凭据
pub async fn list_trash(State(state): State<AdminState>) -> impl IntoResponse {
    let response = state.service.list_trash();
    Json(response)
}

/// POST /api/admin/credentials/trash/:id/restore
/// 从回收站恢复凭据（恢复为禁用态，id 不变）
pub async fn restore_credential(
    State(state): State<AdminState>,
    Path(id): Path<u64>,
) -> impl IntoResponse {
    match state.service.restore_credential(id) {
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
pub async fn force_refresh_token(    State(state): State<AdminState>,
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
        Ok(_) => Json(SuccessResponse::new(format!(
            "凭据 #{} 验活通过",
            id
        )))
        .into_response(),
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

// ============ 网页上号（Social OAuth）============

use super::service::PollResult;
use super::types::{
    PollSocialLoginResponse, StartSocialLoginRequest, StartSocialLoginResponse,
};
use crate::kiro::auth::social::OAuthCallbackData;
use axum::extract::Query;
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
    if let Some(err) = params.get("error_description").or_else(|| params.get("error")) {
        let body = format!(
            "<html><head><meta charset='utf-8'><title>登录失败</title></head><body style='font-family:sans-serif;text-align:center;padding:60px'><h2>&#10007; 登录失败</h2><p>{}</p><p style='color:#888;font-size:13px'>请关闭此标签页并重试。</p></body></html>",
            html_escape(err)
        );
        return ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], Html(body));
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
    ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], Html(body))
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
        .start_idc_login(&payload.start_url, region, payload.priority, payload.proxy_url)
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
    match state
        .service
        .start_external_idp_login(payload.priority, payload.proxy_url)
    {
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
/// 第 3 步：粘回授权回调 URL，换 token 入池。
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
        }))
        .into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StartExternalIdpLoginRequest {
    #[serde(default = "default_priority")]
    pub priority: u32,
    pub proxy_url: Option<String>,
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
pub async fn storage_cleanup(
    State(state): State<AdminState>,
    Json(payload): Json<super::types::StorageCleanupRequest>,
) -> impl IntoResponse {
    match state.service.storage_cleanup(
        &payload.target,
        payload.older_than_days,
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
