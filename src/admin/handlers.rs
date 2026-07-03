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

fn default_priority() -> u32 {
    100
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
