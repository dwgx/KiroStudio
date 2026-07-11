//! Token 管理模块
//!
//! 负责 Token 过期检测和刷新，支持 Social 和 IdC 认证方式
//! 支持多凭据 (MultiTokenManager) 管理

use anyhow::bail;
use chrono::{DateTime, Duration, Utc};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex as TokioMutex;
use tokio::task::JoinHandle;
use tokio::time::sleep;

use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::sync::atomic::AtomicU32;
use std::time::{Duration as StdDuration, Instant};

use arc_swap::ArcSwap;

use crate::http_client::{ProxyConfig, build_client, build_streaming_client};
use crate::kiro::affinity::UserAffinityManager;
use crate::kiro::cooldown::{CooldownManager, CooldownReason};use crate::kiro::machine_id;
use crate::kiro::model::credentials::{KiroCredentials, TrashEntry};
use crate::kiro::model::token_refresh::{
    ExternalIdpRefreshResponse, IdcRefreshRequest, IdcRefreshResponse, RefreshRequest,
    RefreshResponse,
};
use crate::kiro::model::usage_limits::UsageLimitsResponse;
use crate::kiro::rate_limiter::{FailureKind, RateLimitConfig, RateLimiter};
use crate::kiro::scheduling::{InflightGuard, RpmTracker};
use crate::kiro::health::HealthTracker;
use crate::model::config::Config;

/// 检查 Token 是否在指定时间内过期
pub(crate) fn is_token_expiring_within(
    credentials: &KiroCredentials,
    minutes: i64,
) -> Option<bool> {
    credentials
        .expires_at
        .as_ref()
        .and_then(|expires_at| DateTime::parse_from_rfc3339(expires_at).ok())
        .map(|expires| expires <= Utc::now() + Duration::minutes(minutes))
}

/// 检查 Token 是否已过期（提前 5 分钟判断）
pub(crate) fn is_token_expired(credentials: &KiroCredentials) -> bool {
    is_token_expiring_within(credentials, 5).unwrap_or(true)
}

/// 检查 Token 是否即将过期（10分钟内）
pub(crate) fn is_token_expiring_soon(credentials: &KiroCredentials) -> bool {
    is_token_expiring_within(credentials, 10).unwrap_or(false)
}

fn sha256_hex(input: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    let result = hasher.finalize();
    format!("{:x}", result)
}

/// 生成 API Key 脱敏展示(前 4 + ... + 后 4,长度不足或非 ASCII 回退 ***)
fn mask_api_key(key: &str) -> String {
    if key.is_ascii() && key.len() > 16 {
        format!("{}...{}", &key[..4], &key[key.len() - 4..])
    } else {
        "***".to_string()
    }
}

/// 验证 refreshToken 的基本有效性
pub(crate) fn validate_refresh_token(credentials: &KiroCredentials) -> anyhow::Result<()> {
    let refresh_token = credentials
        .refresh_token
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("缺少 refreshToken"))?;

    if refresh_token.is_empty() {
        bail!("refreshToken 为空");
    }

    if refresh_token.len() < 100 || refresh_token.ends_with("...") || refresh_token.contains("...")
    {
        bail!(
            "refreshToken 已被截断（长度: {} 字符）。\n\
             这通常是 Kiro IDE 为了防止凭证被第三方工具使用而故意截断的。",
            refresh_token.len()
        );
    }

    Ok(())
}

/// 持锁刷新的结果：真正刷新了，还是因二次检查发现无需刷新而跳过。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RefreshOutcome {
    Refreshed,
    Skipped,
}

/// Refresh Token 永久失效错误
///
/// 当服务端返回 400 + `invalid_grant` 时，表示 refreshToken 已被撤销或过期，
/// 不应重试，需立即禁用对应凭据。
#[derive(Debug)]
pub(crate) struct RefreshTokenInvalidError {
    pub message: String,
}

impl fmt::Display for RefreshTokenInvalidError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for RefreshTokenInvalidError {}

/// 刷新 Token
pub(crate) async fn refresh_token(
    credentials: &KiroCredentials,
    config: &Config,
    proxy: Option<&ProxyConfig>,
) -> anyhow::Result<KiroCredentials> {
    // API Key 凭据不支持 Token 刷新：底层契约级拦截
    // 其他调用点（try_ensure_token / 活跃路径 / add_credential）在调用前已显式分流 API Key；
    // 仅 force_refresh_token_for 未分流，此处 bail 让错误自然传播为 400 BAD_REQUEST。
    if credentials.is_api_key_credential() {
        bail!("API Key 凭据不支持刷新 Token");
    }

    validate_refresh_token(credentials)?;

    // 根据 auth_method 选择刷新方式
    // 如果未指定 auth_method，根据是否有 clientId/clientSecret 自动判断
    let auth_method = credentials.auth_method.as_deref().unwrap_or_else(|| {
        if credentials.client_id.is_some() && credentials.client_secret.is_some() {
            "idc"
        } else {
            "social"
        }
    });

    if credentials.is_external_idp_credential() {
        refresh_external_idp_token(credentials, config, proxy).await
    } else if auth_method.eq_ignore_ascii_case("idc")
        || auth_method.eq_ignore_ascii_case("builder-id")
        || auth_method.eq_ignore_ascii_case("iam")
    {
        refresh_idc_token(credentials, config, proxy).await
    } else {
        refresh_social_token(credentials, config, proxy).await
    }
}

/// 刷新 Social Token
async fn refresh_social_token(
    credentials: &KiroCredentials,
    config: &Config,
    proxy: Option<&ProxyConfig>,
) -> anyhow::Result<KiroCredentials> {
    tracing::info!("正在刷新 Social Token...");

    let refresh_token = credentials.refresh_token.as_ref().unwrap();
    // 优先级：凭据.auth_region > 凭据.region > config.auth_region > config.region
    let region = credentials.effective_auth_region(config);

    let refresh_url = format!("https://prod.{}.auth.desktop.kiro.dev/refreshToken", region);
    let refresh_domain = format!("prod.{}.auth.desktop.kiro.dev", region);
    let machine_id = machine_id::generate_from_credentials(credentials, config);
    let kiro_version = &config.kiro_version;

    let client = build_client(proxy, 60, config.tls_backend)?;
    let body = RefreshRequest {
        refresh_token: refresh_token.to_string(),
    };

    let response = client
        .post(&refresh_url)
        .header("Accept", "application/json, text/plain, */*")
        .header("Content-Type", "application/json")
        .header(
            "User-Agent",
            format!("KiroIDE-{}-{}", kiro_version, machine_id),
        )
        .header("Accept-Encoding", "gzip, compress, deflate, br")
        .header("host", &refresh_domain)
        .json(&body)
        .send()
        .await?;

    let status = response.status();
    if !status.is_success() {
        let body_text = response.text().await.unwrap_or_default();

        // 400 + invalid_grant + Invalid refresh token provided → refreshToken 永久失效
        if status.as_u16() == 400
            && body_text.contains("\"invalid_grant\"")
            && body_text.contains("Invalid refresh token provided")
        {
            return Err(RefreshTokenInvalidError {
                message: format!("Social refreshToken 已失效 (invalid_grant): {}", body_text),
            }
            .into());
        }

        let error_msg = match status.as_u16() {
            401 => "OAuth 凭证已过期或无效，需要重新认证",
            403 => "权限不足，无法刷新 Token",
            429 => "请求过于频繁，已被限流",
            500..=599 => "服务器错误，AWS OAuth 服务暂时不可用",
            _ => "Token 刷新失败",
        };
        bail!("{}: {} {}", error_msg, status, body_text);
    }

    let data: RefreshResponse = response.json().await?;

    let mut new_credentials = credentials.clone();
    new_credentials.access_token = Some(data.access_token);

    if let Some(new_refresh_token) = data.refresh_token {
        new_credentials.refresh_token = Some(new_refresh_token);
    }

    if let Some(profile_arn) = data.profile_arn {
        new_credentials.profile_arn = Some(profile_arn);
    }

    if let Some(expires_in) = data.expires_in {
        let expires_at = Utc::now() + Duration::seconds(expires_in);
        new_credentials.expires_at = Some(expires_at.to_rfc3339());
    }

    Ok(new_credentials)
}

/// 校验 External IdP 的 token_endpoint 只能指向合法的 Microsoft 登录域。
///
/// token_endpoint/issuer_url 来自凭据，服务端会直接向其 POST（含 refresh_token/
/// client_secret）。若不校验，可被诱导 SSRF 或把凭据发往攻击者域。这里强制：
/// - scheme 必须是 https；
/// - host 必须是 `login.microsoftonline.com` / `.us` / `.cn`（或其子域）；
/// - 拒绝 userinfo(`@`) 混淆、IP 字面量。
pub(crate) fn validate_microsoft_token_endpoint(endpoint: &str) -> anyhow::Result<()> {
    let rest = endpoint
        .strip_prefix("https://")
        .ok_or_else(|| anyhow::anyhow!("External IdP token_endpoint 必须为 https"))?;
    // authority = 到第一个 / ? # 之前
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    // 拒绝 userinfo 混淆（user@evil.com）
    if authority.contains('@') {
        bail!("External IdP token_endpoint 含非法 userinfo: {}", endpoint);
    }
    // 去掉端口
    let host = authority.split(':').next().unwrap_or("").to_ascii_lowercase();
    if host.is_empty() {
        bail!("External IdP token_endpoint 缺少主机: {}", endpoint);
    }
    const ALLOWED_SUFFIXES: &[&str] = &[
        "login.microsoftonline.com",
        "login.microsoftonline.us",
        "login.partner.microsoftonline.cn",
        "login.chinacloudapi.cn",
    ];
    let ok = ALLOWED_SUFFIXES
        .iter()
        .any(|s| host == *s || host.ends_with(&format!(".{s}")));
    if !ok {
        bail!(
            "External IdP token_endpoint 主机不在 Microsoft 登录域白名单内: {}",
            host
        );
    }
    Ok(())
}

/// 刷新 External IdP Token（Microsoft Entra / Azure AD，OAuth2 refresh_token）
async fn refresh_external_idp_token(
    credentials: &KiroCredentials,
    config: &Config,
    proxy: Option<&ProxyConfig>,
) -> anyhow::Result<KiroCredentials> {
    tracing::info!("正在刷新 External IdP Token...");

    let refresh_token = credentials.refresh_token.as_ref().unwrap();
    let client_id = credentials
        .client_id
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("External IdP 刷新需要 clientId"))?;

    let token_endpoint = if let Some(endpoint) = credentials.token_endpoint.as_deref() {
        endpoint.to_string()
    } else {
        let issuer = credentials
            .issuer_url
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("External IdP 刷新需要 tokenEndpoint 或 issuerUrl"))?
            .trim_end_matches('/');
        if issuer.ends_with("/v2.0") {
            format!("{}/token", issuer)
        } else {
            format!("{}/oauth2/v2.0/token", issuer)
        }
    };

    // 安全（SSRF）：token_endpoint / issuer_url 来自凭据（可被写凭据的 admin 污染），
    // 服务端会直接 POST 它。限制只能指向合法的 Microsoft 登录域，防止被诱导把
    // client_id/refresh_token 之类发到攻击者服务器，或拿网关当跳板打内网。
    validate_microsoft_token_endpoint(&token_endpoint)?;

    let mut form = vec![
        ("client_id", client_id.to_string()),
        ("grant_type", "refresh_token".to_string()),
        ("refresh_token", refresh_token.to_string()),
    ];
    if let Some(scopes) = credentials.scopes.as_ref().filter(|s| !s.trim().is_empty()) {
        form.push(("scope", scopes.to_string()));
    }
    if let Some(client_secret) = credentials
        .client_secret
        .as_ref()
        .filter(|s| !s.trim().is_empty())
    {
        form.push(("client_secret", client_secret.to_string()));
    }

    let client = build_client(proxy, 60, config.tls_backend)?;
    let response = client
        .post(&token_endpoint)
        .header("Accept", "application/json")
        .form(&form)
        .send()
        .await?;

    let status = response.status();
    if !status.is_success() {
        let body_text = response.text().await.unwrap_or_default();
        if status.as_u16() == 400 && body_text.contains("invalid_grant") {
            return Err(RefreshTokenInvalidError {
                message: format!("External IdP refreshToken 已失效 (invalid_grant): {}", body_text),
            }
            .into());
        }
        let error_msg = match status.as_u16() {
            401 => "External IdP 凭证已过期或无效，需要重新认证",
            403 => "External IdP 权限不足，无法刷新 Token",
            429 => "External IdP 请求过于频繁，已被限流",
            500..=599 => "External IdP 服务暂时不可用",
            _ => "External IdP Token 刷新失败",
        };
        bail!("{}: {} {}", error_msg, status, body_text);
    }

    let data: ExternalIdpRefreshResponse = response.json().await?;
    let mut new_credentials = credentials.clone();
    new_credentials.access_token = Some(data.access_token);

    if let Some(new_refresh_token) = data.refresh_token {
        new_credentials.refresh_token = Some(new_refresh_token);
    }

    if let Some(expires_in) = data.expires_in {
        let expires_at = Utc::now() + Duration::seconds(expires_in);
        new_credentials.expires_at = Some(expires_at.to_rfc3339());
    }

    Ok(new_credentials)
}

async fn refresh_idc_token(
    credentials: &KiroCredentials,
    config: &Config,
    proxy: Option<&ProxyConfig>,
) -> anyhow::Result<KiroCredentials> {
    tracing::info!("正在刷新 IdC Token...");

    let refresh_token = credentials.refresh_token.as_ref().unwrap();
    let client_id = credentials
        .client_id
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("IdC 刷新需要 clientId"))?;
    let client_secret = credentials
        .client_secret
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("IdC 刷新需要 clientSecret"))?;

    // 优先级：凭据.auth_region > 凭据.region > config.auth_region > config.region
    let region = credentials.effective_auth_region(config);
    let refresh_url = format!("https://oidc.{}.amazonaws.com/token", region);
    let os_name = &config.system_version;
    let node_version = &config.node_version;

    let x_amz_user_agent = "aws-sdk-js/3.980.0 KiroIDE";
    let user_agent = format!(
        "aws-sdk-js/3.980.0 ua/2.1 os/{} lang/js md/nodejs#{} api/sso-oidc#3.980.0 m/E KiroIDE",
        os_name, node_version
    );

    let client = build_client(proxy, 60, config.tls_backend)?;
    let body = IdcRefreshRequest {
        client_id: client_id.to_string(),
        client_secret: client_secret.to_string(),
        refresh_token: refresh_token.to_string(),
        grant_type: "refresh_token".to_string(),
    };

    let response = client
        .post(&refresh_url)
        .header("content-type", "application/json")
        .header("x-amz-user-agent", x_amz_user_agent)
        .header("user-agent", &user_agent)
        .header("host", format!("oidc.{}.amazonaws.com", region))
        .header("amz-sdk-invocation-id", uuid::Uuid::new_v4().to_string())
        .header("amz-sdk-request", "attempt=1; max=4")
        .json(&body)
        .send()
        .await?;

    let status = response.status();
    if !status.is_success() {
        let body_text = response.text().await.unwrap_or_default();

        // 400 + invalid_grant + Invalid refresh token provided → refreshToken 永久失效
        if status.as_u16() == 400
            && body_text.contains("\"invalid_grant\"")
            && body_text.contains("Invalid refresh token provided")
        {
            return Err(RefreshTokenInvalidError {
                message: format!("IdC refreshToken 已失效 (invalid_grant): {}", body_text),
            }
            .into());
        }

        let error_msg = match status.as_u16() {
            401 => "IdC 凭证已过期或无效，需要重新认证",
            403 => "权限不足，无法刷新 Token",
            429 => "请求过于频繁，已被限流",
            500..=599 => "服务器错误，AWS OIDC 服务暂时不可用",
            _ => "IdC Token 刷新失败",
        };
        bail!("{}: {} {}", error_msg, status, body_text);
    }

    let data: IdcRefreshResponse = response.json().await?;

    let mut new_credentials = credentials.clone();
    new_credentials.access_token = Some(data.access_token);

    if let Some(new_refresh_token) = data.refresh_token {
        new_credentials.refresh_token = Some(new_refresh_token);
    }

    if let Some(expires_in) = data.expires_in {
        let expires_at = Utc::now() + Duration::seconds(expires_in);
        new_credentials.expires_at = Some(expires_at.to_rfc3339());
    }

    // 同步更新 profile_arn（如果 IdC 响应中包含）
    if let Some(profile_arn) = data.profile_arn {
        new_credentials.profile_arn = Some(profile_arn);
    }

    Ok(new_credentials)
}

/// BuilderId / IdC 账号无自带 profileArn 时的默认回退值（与 Kiro IDE 一致）。
pub(crate) const DEFAULT_BUILDER_ID_PROFILE_ARN: &str =
    "arn:aws:codewhisperer:us-east-1:638616132270:profile/AAAACCCCXXXX";

/// 获取使用额度信息
pub(crate) async fn get_usage_limits(
    credentials: &KiroCredentials,
    config: &Config,
    token: &str,
    proxy: Option<&ProxyConfig>,
) -> anyhow::Result<UsageLimitsResponse> {
    tracing::debug!("正在获取使用额度信息...");

    // Region 解析(稳健版):profileArn 第 4 段(严格校验 arn 前缀 + region 白名单)
    // > 凭据 region/auth_region > config。严格校验防污染 ARN 拼出坏 host(DNS/502)。
    let region = credentials.effective_upstream_region(config);
    // Kiro management API（已迁移，旧 q.{region}.amazonaws.com 停用）
    let host = format!("management.{}.kiro.dev", region);
    let machine_id = machine_id::generate_from_credentials(credentials, config);
    let kiro_version = &config.kiro_version;
    let os_name = &config.system_version;
    let node_version = &config.node_version;

    // 构建 URL（含 isEmailRequired=true，与 Kiro IDE 一致）
    let mut url = format!(
        "https://{}/getUsageLimits?isEmailRequired=true&origin=AI_EDITOR&resourceType=AGENTIC_REQUEST",
        host
    );

    // profileArn：统一走 effective_profile_arn（与对话/端点路径同口径）——
    // idc/social/api_key 缺 arn 回退默认 BuilderId,external_idp 用它自己租户的真实 arn。
    // 关键修复：原先此处直接读 credentials.profile_arn 并对**所有**类型回退默认 BuilderId ARN,
    // 导致 external_idp 号(带的是别的租户占位 arn)余额查询 403 Invalid token → 余额恒 null。
    // effective_profile_arn 对 external_idp 缺真实 arn 时返回 None,此时不附带 profileArn 参数。
    if let Some(arn) = credentials.effective_profile_arn() {
        url.push_str(&format!("&profileArn={}", urlencoding::encode(&arn)));
    }

    // 构建 User-Agent headers
    let user_agent = format!(
        "aws-sdk-js/1.0.0 ua/2.1 os/{} lang/js md/nodejs#{} api/codewhispererruntime#1.0.0 m/N,E KiroIDE-{}-{}",
        os_name, node_version, kiro_version, machine_id
    );
    let amz_user_agent = format!(
        "aws-sdk-js/1.0.0 KiroIDE-{}-{}",
        kiro_version, machine_id
    );

    let client = build_client(proxy, 60, config.tls_backend)?;

    let mut request = client
        .get(&url)
        .header("x-amz-user-agent", &amz_user_agent)
        .header("user-agent", &user_agent)
        .header("host", &host)
        .header("amz-sdk-invocation-id", uuid::Uuid::new_v4().to_string())
        .header("amz-sdk-request", "attempt=1; max=1")
        .header("Authorization", format!("Bearer {}", token));

    if credentials.is_api_key_credential() {
        request = request.header("tokentype", "API_KEY");
    } else if credentials.is_external_idp_credential() {
        request = request.header("tokentype", "EXTERNAL_IDP");
    }

    let response = request.send().await?;

    let status = response.status();
    if !status.is_success() {
        let body_text = response.text().await.unwrap_or_default();
        let error_msg = match status.as_u16() {
            401 => "认证失败，Token 无效或已过期",
            403 => "权限不足，无法获取使用额度",
            429 => "请求过于频繁，已被限流",
            500..=599 => "服务器错误，AWS 服务暂时不可用",
            _ => "获取使用额度失败",
        };
        bail!("{}: {} {}", error_msg, status, body_text);
    }

    let data: UsageLimitsResponse = response.json().await?;
    Ok(data)
}

/// 运行时动态解析真实 profileArn（Kiro management ControlPlane 的 ListAvailableProfiles）。
///
/// 为何需要:idc/Enterprise 号入池/刷新后常没有 profileArn(IdC 的 oidc 刷新不回传它),
/// 而对话/余额端点对这类号要求带**真实** profileArn——回退默认占位 ARN 对 Enterprise 号
/// 会被上游判 `Invalid token`/403(实测)。Kiro IDE / kiro-account-manager 的做法是运行时
/// 调 ListAvailableProfiles 拿账号真实的 profiles[0].arn。此函数复刻该 recipe(已对齐 KAM):
/// - `POST https://management.{region}.kiro.dev/`(根路径)
/// - header: `x-amz-target: KiroControlPlaneBearerService.ListAvailableProfiles`
///   + `content-type: application/x-amz-json-1.0` + Bearer + control-plane UA
/// - body: `{}`;成功取响应 `profiles[0].arn`。
///
/// 返回 Ok(Some(arn)) 拿到、Ok(None) 响应无 profile(账号无可用 profile)、Err 网络/上游错误。
pub(crate) async fn resolve_profile_arn_via_management(
    credentials: &KiroCredentials,
    config: &Config,
    token: &str,
    proxy: Option<&ProxyConfig>,
) -> anyhow::Result<Option<String>> {
    let region = credentials.effective_upstream_region(config);
    let host = format!("management.{}.kiro.dev", region);
    let url = format!("https://{}/", host);
    let machine_id = machine_id::generate_from_credentials(credentials, config);
    let user_agent = format!(
        "aws-sdk-js/1.0.0 ua/2.1 os/{} lang/js md/nodejs#{} api/kirocontrolplanebearer#1.0.0 m/N,E KiroIDE-{}-{}",
        config.system_version, config.node_version, config.kiro_version, machine_id
    );
    let client = build_client(proxy, 30, config.tls_backend)?;
    let mut request = client
        .post(&url)
        .header("content-type", "application/x-amz-json-1.0")
        .header("x-amz-target", "KiroControlPlaneBearerService.ListAvailableProfiles")
        .header("host", &host)
        .header("user-agent", &user_agent)
        .header("x-amz-user-agent", "aws-sdk-js/1.0.0")
        .header("amz-sdk-invocation-id", uuid::Uuid::new_v4().to_string())
        .header("amz-sdk-request", "attempt=1; max=3")
        .header("Authorization", format!("Bearer {}", token))
        .body("{}");
    if credentials.is_external_idp_credential() {
        request = request.header("TokenType", "EXTERNAL_IDP");
    }
    let response = request.send().await?;
    let status = response.status();
    if !status.is_success() {
        let body_text = response.text().await.unwrap_or_default();
        bail!("ListAvailableProfiles 失败: {} {}", status, body_text);
    }
    let data: serde_json::Value = response.json().await?;
    let arn = data
        .get("profiles")
        .and_then(|p| p.as_array())
        .and_then(|arr| arr.iter().find_map(|p| p.get("arn").and_then(|a| a.as_str())))
        .map(|s| s.to_string());
    Ok(arn)
}

// ============================================================================
// 多凭据 Token 管理器
// ============================================================================

/// 单个凭据条目的状态
struct CredentialEntry {
    /// 凭据唯一 ID
    id: u64,
    /// 凭据信息
    credentials: KiroCredentials,
    /// API 调用连续失败次数
    failure_count: u32,
    /// Token 刷新连续失败次数
    refresh_failure_count: u32,
    /// 是否已禁用
    disabled: bool,
    /// 禁用原因（用于区分手动禁用 vs 自动禁用，便于自愈）
    disabled_reason: Option<DisabledReason>,
    /// API 调用成功次数
    success_count: u64,
    /// 该凭据**生命周期累计**上游 credit 消耗（花费）。
    ///
    /// 由每次请求完成后上游 meteringEvent 的真实计费量累加而来（无 meteringEvent 的
    /// 请求不计）。持久化进 kiro_stats.json，**独立于 usage_retention_days**——用量
    /// 明细（JSONL/SQLite）会按保留期滚动清理，但这个累计值只增不清，反映该号从入池
    /// 至今一共花了多少 credit。
    total_credits_used: f64,
    /// 最后一次 API 调用时间（RFC3339 格式）
    last_used_at: Option<String>,
    /// 当前在途（in-flight）请求数
    ///
    /// 选号时 +1（在选号临界区内原子完成），请求真正处理完（SSE 流被下游消费完
    /// / 客户端断开 / 非流式读毕）时随 [`InflightGuard`] Drop 而 -1。
    /// balanced 选号按此升序，把并发流量分摊到在飞请求最少的号，根治惊群热点。
    /// 用 `Arc` 是为了让守卫直接持有计数器、与条目生命周期解耦（见 [`crate::kiro::scheduling`] 的 REF-1 说明）。
    inflight: Arc<AtomicU32>,
}

/// 禁用原因
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DisabledReason {
    /// Admin API 手动禁用
    Manual,
    /// 连续失败达到阈值后自动禁用
    TooManyFailures,
    /// Token 刷新连续失败达到阈值后自动禁用
    TooManyRefreshFailures,
    /// 额度已用尽（如 MONTHLY_REQUEST_COUNT）
    QuotaExceeded,
    /// 账户被上游暂停/封禁（不可自动恢复，等待人工处理）
    AccountSuspended,
    /// 持续可疑活动风控——反复被 Kiro 限流(trigger_count 高)后自动禁用,避免继续砸加重风控/触发真封禁。
    /// 属"自动禁用",可由自愈逻辑或人工重新启用。
    SuspiciousActivityAuto,
    /// Refresh Token 永久失效（服务端返回 invalid_grant）
    InvalidRefreshToken,
    /// 凭据配置无效（如 authMethod=api_key 但缺少 kiroApiKey）
    InvalidConfig,
}

/// 统计数据持久化条目
#[derive(Serialize, Deserialize)]
struct StatsEntry {
    success_count: u64,
    /// 生命周期累计 credit 花费。向后兼容：老 stats 文件无此字段时默认 0。
    #[serde(default)]
    total_credits_used: f64,
    last_used_at: Option<String>,
}

// ============================================================================
// Admin API 公开结构
// ============================================================================

/// 凭据条目快照（用于 Admin API 读取）
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CredentialEntrySnapshot {
    /// 凭据唯一 ID
    pub id: u64,
    /// 优先级
    pub priority: u32,
    /// 凭据级 RPM 容量上限（None=继承全局）
    pub rpm_limit: Option<u32>,
    /// 是否被禁用
    pub disabled: bool,
    /// 连续失败次数
    pub failure_count: u32,
    /// 认证方式
    pub auth_method: Option<String>,
    /// 是否有 Profile ARN
    pub has_profile_arn: bool,
    /// Token 过期时间
    pub expires_at: Option<String>,
    /// refreshToken 的 SHA-256 哈希（仅 OAuth 凭据，用于前端去重）
    pub refresh_token_hash: Option<String>,
    /// kiroApiKey 的 SHA-256 哈希（仅 API Key 凭据，用于前端去重）
    pub api_key_hash: Option<String>,
    /// kiroApiKey 的脱敏展示（仅 API Key 凭据，用于前端显示）
    pub masked_api_key: Option<String>,
    /// 用户邮箱（用于前端显示）
    pub email: Option<String>,
    /// 用户自定义别名/备注（卡片展示优先于 email/#id）
    pub name: Option<String>,
    /// 订阅等级标题（如 "Kiro Pro"），随凭据持久化，重启后仍可展示
    pub subscription_title: Option<String>,
    /// API 调用成功次数
    pub success_count: u64,
    /// 生命周期累计 credit 花费（真实计费累加，独立于用量保留期，只增不清）
    pub total_credits_used: f64,
    /// 最后一次 API 调用时间（RFC3339 格式）
    pub last_used_at: Option<String>,
    /// 是否配置了凭据级代理
    pub has_proxy: bool,
    /// 代理 URL（用于前端展示）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proxy_url: Option<String>,
    /// Token 刷新连续失败次数
    pub refresh_failure_count: u32,
    /// 禁用原因
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disabled_reason: Option<String>,
    /// 端点名称（未显式配置时返回 None，由 Admin 层回退到默认值）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    /// 当前在途（in-flight）请求数（实时负载，用于观测均衡效果）
    pub inflight: u32,
    /// 最近 60 秒滚动窗口内的请求数（RPM 观测）
    pub rpm: u32,
}

/// 回收站条目快照（用于 Admin API 读取，不含敏感明文）
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TrashSnapshot {
    /// 凭据唯一 ID
    pub id: u64,
    /// 优先级
    pub priority: u32,
    /// 认证方式
    pub auth_method: Option<String>,
    /// 用户邮箱
    pub email: Option<String>,
    /// kiroApiKey 的脱敏展示（仅 API Key 凭据）
    pub masked_api_key: Option<String>,
    /// refreshToken 的 SHA-256 哈希（仅 OAuth 凭据）
    pub refresh_token_hash: Option<String>,
    /// kiroApiKey 的 SHA-256 哈希（仅 API Key 凭据）
    pub api_key_hash: Option<String>,
    /// 端点名称
    pub endpoint: Option<String>,
    /// 删除时间（RFC3339 格式）
    pub deleted_at: String,
    /// 删除前累计成功次数
    pub success_count: u64,
    /// 删除前最后一次调用时间
    pub last_used_at: Option<String>,
}

/// 凭据管理器状态快照
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ManagerSnapshot {
    /// 凭据条目列表
    pub entries: Vec<CredentialEntrySnapshot>,
    /// 当前活跃凭据 ID
    pub current_id: u64,
    /// 总凭据数量
    pub total: usize,
    /// 可用凭据数量
    pub available: usize,
}

/// 多凭据 Token 管理器
///
/// 支持多个凭据的管理，实现固定优先级 + 故障转移策略
/// 故障统计基于 API 调用结果，而非 Token 刷新结果
pub struct MultiTokenManager {
    /// 服务端配置（ArcSwap：admin 改配置后 reload_config 原子热切,读端 load() 无锁近零成本，
    /// 不重启即生效。热路径每请求读的标量另存原子镜像,避免 O(N) 次建 Guard）。
    config: ArcSwap<Config>,
    proxy: Option<ProxyConfig>,
    /// 凭据条目列表
    entries: Mutex<Vec<CredentialEntry>>,
    /// 回收站（软删除的凭据）
    ///
    /// 删除凭据时物理移出 `entries` 并推入此处，让其从调度池彻底消失，
    /// 无需在各处 filter(!disabled) 补条件；可恢复或彻底删除。
    trash: Mutex<Vec<TrashEntry>>,
    /// 当前活动凭据 ID
    current_id: Mutex<u64>,
    /// Token 刷新锁，确保同一时间只有一个刷新操作
    refresh_lock: TokioMutex<()>,
    /// 凭据文件路径（用于回写）
    credentials_path: Option<PathBuf>,
    /// 是否为多凭据格式（数组格式才回写）
    is_multiple_format: bool,
    /// 负载均衡模式（运行时可修改）
    load_balancing_mode: Mutex<String>,
    /// 最近一次统计持久化时间（用于 debounce）
    last_stats_save_at: Mutex<Option<Instant>>,
    /// 统计数据是否有未落盘更新
    stats_dirty: AtomicBool,
    /// 失败冷却管理器（反应式：凭据出错后短暂跳过）
    cooldown: CooldownManager,
    /// 是否启用冷却（原子镜像,reload 热更）
    cooldown_enabled: AtomicBool,
    /// 拟人速率限制器（防关联：每日上限 + 请求间隔）
    rate_limiter: RateLimiter,
    /// 是否启用速率限制（原子镜像,reload 热更）
    rate_limit_enabled: AtomicBool,
    /// 会话亲和性管理器（防关联：同一会话粘同一凭据）
    affinity: UserAffinityManager,
    /// 是否启用会话亲和性（原子镜像,reload 热更）
    affinity_enabled: AtomicBool,
    /// RPM 滚动窗口追踪器（balanced 选号时对接近 RPM 上限的号降权）
    rpm: RpmTracker,
    /// 模型级"该号不支持此模型"短期黑名单：key=(credential_id, kiro_model_id)，value=记录时刻。
    ///
    /// 上游对某号返回 `INVALID_MODEL_ID` 时，只记"这个号 + 这个模型"不可用（短 TTL），
    /// 选号时**仅对该模型**跳过它，该号对其它模型照常参与调度。这修正了 v0.6.0 的致命
    /// 设计缺陷：此前把 INVALID_MODEL_ID 当"整个号坏了"冷却/自动禁用，导致一个客户端请求
    /// 一个订阅不含的模型就能把能正常服务其它模型的号（乃至整池）全部打下线。
    model_blocklist: Mutex<HashMap<(u64, String), Instant>>,
    /// 号池/族级健康评分 + 熔断半开渐进放回（balanced 选号 p_avail 权重 + 429 后逐步试探放回）。
    health: HealthTracker,
    /// 每凭据 RPM 软上限（0 = 不限制）（原子镜像,reload 热更）
    rpm_limit: AtomicU32,
    /// 全池冷却时是否快速失败（立即返回 429+Retry-After 让客户端退避，而非网关内硬扛）。（原子镜像,reload 热更）
    all_cooling_fast_fail: AtomicBool,
    /// 是否在凭据持续可疑活动风控(trigger_count 达阈值)时自动禁用它。（原子镜像,reload 热更）
    auto_disable_suspicious: AtomicBool,
    /// 均衡模式下是否叠加优先级分发（原子镜像,reload 热更）。
    priority_in_balanced: AtomicBool,
    /// 主动 token 预刷新后台任务句柄（TIER2 热重载：改配置后 abort + respawn 即时生效不重启）。
    /// None = 当前未运行（proactive_token_refresh=false 或尚未启动）。
    refresh_task: Mutex<Option<JoinHandle<()>>>,
}

/// 每个凭据最大 API 调用失败次数
const MAX_FAILURES_PER_CREDENTIAL: u32 = 3;
/// 所有号只是临时冷却/限流（会自动恢复）时，单次选号最多在网关内等待多久再放弃。
/// 避免瞬时全忙就立刻返回“所有凭据均已禁用”；但也不能太长——否则一个请求的一次
/// 选号就阻塞数分钟，叠加上层重试会反复扫冷全池（雪崩）。取 20s：够扛过一次
/// burst 软限流的自愈，又不至于让单请求长期霸占等待。上层 provider 另有 45s
/// 墙钟总预算兜底。
const MAX_TRANSIENT_WAIT_SECS: u64 = 20;
/// 统计数据持久化防抖间隔
const STATS_SAVE_DEBOUNCE: StdDuration = StdDuration::from_secs(30);

/// 模型级不支持黑名单的 TTL：某号对某模型返回 INVALID_MODEL_ID 后，这段时间内选号跳过
/// "该号+该模型"组合。取中长窗（订阅权益变化是较慢的事），到期后自动允许重试探。
const MODEL_BLOCK_TTL: StdDuration = StdDuration::from_secs(1800);

/// 原子写入文件：先写同目录临时文件 + 尽力 fsync，再 rename 覆盖目标
///
/// 相比 `std::fs::write` 的裸覆盖，本函数避免"写到一半崩溃 / 磁盘满"导致
/// 目标文件被截断清空（refreshToken/clientSecret 属不可再生资产，绝不能丢）。
///
/// 关键点：
/// - 临时文件放在目标 **同目录**（保证同一文件系统，`rename` 才是原子的）；
/// - 若 `path` 是软链，先 `canonicalize` 拿到真实路径再 rename，避免把软链
///   本身替换成普通文件（canonicalize 对不存在的目标会失败，此时说明是首次
///   写入，直接用原 path）；
/// - Windows 下 `rename` 覆盖已存在文件是支持的，但目标被占用可能失败——
///   失败时回退到直接 `write` 并记 warn，绝不让整体持久化失败。
fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;

    // 解析真实目标路径：软链要写到它指向的真身；不存在（首次写入）则用原 path
    let target: PathBuf = match std::fs::canonicalize(path) {
        Ok(real) => real,
        Err(_) => path.to_path_buf(),
    };

    let dir = target.parent().unwrap_or_else(|| Path::new("."));
    let file_name = target
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("credentials");
    // 同目录下的隐藏临时文件。文件名带 pid + 进程内单调递增序号，
    // 既避免跨进程碰撞，也避免同进程内两个并发持久化争抢同一 tmp 互相截断。
    static TMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = dir.join(format!(".{}.{}.{}.tmp", file_name, std::process::id(), seq));

    // 写入临时文件并尽力落盘。
    // 安全：凭据文件含 refreshToken/clientSecret/kiroApiKey 等活凭证，绝不能 world-readable。
    // Unix 下创建即以 0600（仅属主可读写）打开临时文件，rename 后目标继承该权限，
    // 杜绝默认 umask 造成的 0644 本地泄露。
    let write_tmp = || -> std::io::Result<()> {
        let mut f = {
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                std::fs::OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .mode(0o600)
                    .open(&tmp)?
            }
            #[cfg(not(unix))]
            {
                std::fs::File::create(&tmp)?
            }
        };
        f.write_all(bytes)?;
        f.flush()?;
        // 尽力 sync，失败不致命（部分平台/文件系统可能不支持）
        let _ = f.sync_all();
        Ok(())
    };

    if let Err(e) = write_tmp() {
        // 临时文件都写不出，清理残留后回退到直接写
        let _ = std::fs::remove_file(&tmp);
        tracing::warn!("原子写临时文件失败，回退直接写: {:?}: {}", tmp, e);
        std::fs::write(&target, bytes)?;
        restrict_permissions(&target);
        return Ok(());
    }

    // rename 覆盖目标（原子替换）。目标继承临时文件的 0600 权限。
    match std::fs::rename(&tmp, &target) {
        Ok(()) => {
            restrict_permissions(&target);
            Ok(())
        }
        Err(e) => {
            // Windows 下目标被句柄占用等场景可能失败：回退直接写，别让持久化整体失败
            tracing::warn!("原子 rename 失败，回退直接写: {:?} -> {:?}: {}", tmp, target, e);
            let result = std::fs::write(&target, bytes);
            if result.is_ok() {
                restrict_permissions(&target);
            }
            // 清理残留临时文件
            let _ = std::fs::remove_file(&tmp);
            result
        }
    }
}

/// 将文件权限收紧为仅属主可读写（Unix 0600）；非 Unix 无操作。
///
/// 敏感凭据文件的纵深防护：即便走了 `fs::write` 回退路径（默认受 umask 影响可能 0644），
/// 也把最终文件权限拉回 0600，失败仅告警不致命。
fn restrict_permissions(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(e) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)) {
            tracing::warn!("收紧文件权限失败 {:?}: {}", path, e);
        }
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
}

/// API 调用上下文
///
/// 绑定特定凭据的调用上下文，确保 token、credentials 和 id 的一致性
/// 用于解决并发调用时 current_id 竞态问题
///
/// 不实现 `Clone`：持有 [`InflightGuard`]，clone 会导致在途计数被重复 +1。
/// 单次调用内独占，成功时把 guard 移交给 `CallMeta` 随响应流存活。
pub struct CallContext {
    /// 凭据 ID（用于 report_success/report_failure）
    pub id: u64,
    /// 凭据信息（用于构建请求头）
    pub credentials: KiroCredentials,
    /// 访问 Token
    pub token: String,
    /// 在途请求守卫：本上下文存活期间该凭据的 inflight 计数 +1，Drop 时 -1。
    /// 选号命中时创建；成功后随 `CallMeta` 移交给响应流，直到流真正消费完才析构。
    pub inflight: InflightGuard,
}

/// Web Portal API 调用上下文（用于 app.kiro.dev overage 接口）
///
/// 与 [`CallContext`] 的区别：本上下文携带 Web Portal 所需的 idp + profileArn，
/// 不参与负载均衡选择，仅供显式的单号 overage 开/关调用使用。
pub struct WebPortalContext {
    /// 凭据 ID（便于上层日志关联）
    #[allow(dead_code)]
    pub id: u64,
    pub token: String,
    pub idp: String,
    pub profile_arn: Option<String>,
    pub proxy: Option<ProxyConfig>,
    pub tls_backend: crate::model::config::TlsBackend,
}

impl MultiTokenManager {
    /// 创建多凭据 Token 管理器
    ///
    /// # Arguments
    /// * `config` - 应用配置
    /// * `credentials` - 凭据列表
    /// * `proxy` - 可选的代理配置
    /// * `credentials_path` - 凭据文件路径（用于回写）
    /// * `is_multiple_format` - 是否为多凭据格式（数组格式才回写）
    pub fn new(
        config: Config,
        credentials: Vec<KiroCredentials>,
        proxy: Option<ProxyConfig>,
        credentials_path: Option<PathBuf>,
        is_multiple_format: bool,
    ) -> anyhow::Result<Self> {
        // 计算当前最大 ID，为没有 ID 的凭据分配新 ID
        let max_existing_id = credentials.iter().filter_map(|c| c.id).max().unwrap_or(0);
        let mut next_id = max_existing_id + 1;
        let mut has_new_ids = false;
        let mut has_new_machine_ids = false;
        let config_ref = &config;

        let entries: Vec<CredentialEntry> = credentials
            .into_iter()
            .map(|mut cred| {
                cred.canonicalize_auth_method();
                let id = cred.id.unwrap_or_else(|| {
                    let id = next_id;
                    next_id += 1;
                    cred.id = Some(id);
                    has_new_ids = true;
                    id
                });
                if cred.machine_id.is_none() {
                    cred.machine_id =
                        Some(machine_id::generate_from_credentials(&cred, config_ref));
                    has_new_machine_ids = true;
                }
                CredentialEntry {
                    id,
                    credentials: cred.clone(),
                    failure_count: 0,
                    refresh_failure_count: 0,
                    disabled: cred.disabled, // 从配置文件读取 disabled 状态
                    disabled_reason: if cred.disabled {
                        Some(DisabledReason::Manual)
                    } else {
                        None
                    },
                    success_count: 0,
                    total_credits_used: 0.0,
                    last_used_at: None,
                    inflight: Arc::new(AtomicU32::new(0)),
                }
            })
            .collect();

        // 重复 machine_id 自动轮换(防关联):多个凭据共用同一 machineId 会让上游把它们
        // 识别为同一台设备而关联封禁。这里在入池时统计碰撞,对第 2 个及以后出现的重复
        // machineId 重新生成一个随机唯一值(64 hex),保证每个凭据独立指纹。参考
        // kiro-account-manager normalize_accounts 的 machine_id_counts 去重。
        let mut entries = entries;
        {
            let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
            for entry in &mut entries {
                let Some(mid) = entry.credentials.machine_id.clone() else {
                    continue;
                };
                if !seen.insert(mid.clone()) {
                    // 已见过 → 碰撞,重新生成唯一随机指纹(sha256(随机 UUID) → 64 hex)
                    let mut fresh = machine_id::random_machine_id();
                    while !seen.insert(fresh.clone()) {
                        fresh = machine_id::random_machine_id();
                    }
                    tracing::warn!(
                        "凭据 #{:?} machineId 与其它凭据重复,已自动轮换为独立指纹(防关联)",
                        entry.id
                    );
                    entry.credentials.machine_id = Some(fresh);
                    has_new_machine_ids = true;
                }
            }
        }

        // 校验 API Key 凭据配置完整性：authMethod=api_key 时必须提供 kiroApiKey
        for entry in &mut entries {
            if entry.credentials.kiro_api_key.is_none()
                && entry
                    .credentials
                    .auth_method
                    .as_deref()
                    .map(|m| m.eq_ignore_ascii_case("api_key") || m.eq_ignore_ascii_case("apikey"))
                    .unwrap_or(false)
            {
                tracing::warn!(
                    "凭据 #{} 配置了 authMethod=api_key 但缺少 kiroApiKey 字段，已自动禁用",
                    entry.id
                );
                entry.disabled = true;
                entry.disabled_reason = Some(DisabledReason::InvalidConfig);
            }
        }

        // 检测重复 ID
        let mut seen_ids = std::collections::HashSet::new();
        let mut duplicate_ids = Vec::new();
        for entry in &entries {
            if !seen_ids.insert(entry.id) {
                duplicate_ids.push(entry.id);
            }
        }
        if !duplicate_ids.is_empty() {
            anyhow::bail!("检测到重复的凭据 ID: {:?}", duplicate_ids);
        }

        // 选择初始凭据：优先级最高（priority 最小）的可用凭据，无可用凭据时为 0
        let initial_id = entries
            .iter()
            .filter(|e| !e.disabled)
            .min_by_key(|e| e.credentials.priority)
            .map(|e| e.id)
            .unwrap_or(0);

        let load_balancing_mode = config.load_balancing_mode.clone();
        let cooldown_enabled = config.cooldown_enabled;
        let rate_limit_enabled = config.rate_limit_enabled;
        let affinity_enabled = config.affinity_enabled;
        let rpm_limit = config.credential_rpm_limit;
        let all_cooling_fast_fail = config.all_cooling_fast_fail;
        let auto_disable_suspicious = config.auto_disable_suspicious;
        let priority_in_balanced = config.priority_in_balanced;
        let rate_limit_config = RateLimitConfig {
            daily_max_requests: config.rate_limit_daily_max,
            min_interval_ms: config.rate_limit_min_interval_ms,
            ..RateLimitConfig::default()
        };
        let manager = Self {
            config: ArcSwap::from_pointee(config),
            proxy,
            entries: Mutex::new(entries),
            trash: Mutex::new(Vec::new()),
            current_id: Mutex::new(initial_id),
            refresh_lock: TokioMutex::new(()),
            credentials_path,
            is_multiple_format,
            load_balancing_mode: Mutex::new(load_balancing_mode),
            last_stats_save_at: Mutex::new(None),
            stats_dirty: AtomicBool::new(false),
            cooldown: CooldownManager::new(),
            cooldown_enabled: AtomicBool::new(cooldown_enabled),
            rate_limiter: RateLimiter::new(rate_limit_config),
            rate_limit_enabled: AtomicBool::new(rate_limit_enabled),
            affinity: UserAffinityManager::new(),
            model_blocklist: Mutex::new(HashMap::new()),
            affinity_enabled: AtomicBool::new(affinity_enabled),
            rpm: RpmTracker::new(),
            health: HealthTracker::new(),
            rpm_limit: AtomicU32::new(rpm_limit),
            all_cooling_fast_fail: AtomicBool::new(all_cooling_fast_fail),
            auto_disable_suspicious: AtomicBool::new(auto_disable_suspicious),
            priority_in_balanced: AtomicBool::new(priority_in_balanced),
            refresh_task: Mutex::new(None),
        };

        // 如果有新分配的 ID 或新生成的 machineId，立即持久化到配置文件
        if has_new_ids || has_new_machine_ids {
            if let Err(e) = manager.persist_credentials() {
                tracing::warn!("补全凭据 ID/machineId 后持久化失败: {}", e);
            } else {
                tracing::info!("已补全凭据 ID/machineId 并写回配置文件");
            }
        }

        // 加载持久化的统计数据（success_count, last_used_at）
        manager.load_stats();

        // 加载回收站（trash.json；不存在则空）
        manager.load_trash();

        Ok(manager)
    }

    /// 获取当前配置快照（Arc<Config>，load_full 只 +1 引用计数,不深拷贝）。
    /// 字段访问经 Arc 自动 deref;把 config 当 `&Config` 传函数时用 `&*cfg` 或 `&cfg`。
    pub fn config(&self) -> Arc<Config> {
        self.config.load_full()
    }

    /// 热重载配置（admin 改配置存盘后调用）：重新解析 config 文件,原子换 ArcSwap +
    /// 刷新所有热路径原子镜像 + rate_limiter。解析失败直接返回 Err（零副作用,保留旧配置）。
    /// TIER1 运行时字段（冷却/限流/亲和/RPM上限/快失败/自动禁用/负载均衡）即时生效不重启;
    /// proxy/tls/端口/adminkey 等固化项仍需重启（见 docs/RESEARCH-HOTRELOAD-ARCH-0708）。
    pub fn reload_config(&self) -> anyhow::Result<()> {
        let path = {
            let cur = self.config.load();
            cur.config_path()
                .ok_or_else(|| anyhow::anyhow!("无 config 文件路径,无法热重载"))?
                .to_path_buf()
        };
        let new = Config::load(&path)?; // 解析失败 → return Err,不动任何状态
        // 刷新热路径原子镜像
        self.cooldown_enabled
            .store(new.cooldown_enabled, Ordering::Relaxed);
        self.rate_limit_enabled
            .store(new.rate_limit_enabled, Ordering::Relaxed);
        self.affinity_enabled
            .store(new.affinity_enabled, Ordering::Relaxed);
        self.rpm_limit
            .store(new.credential_rpm_limit, Ordering::Relaxed);
        self.all_cooling_fast_fail
            .store(new.all_cooling_fast_fail, Ordering::Relaxed);
        self.auto_disable_suspicious
            .store(new.auto_disable_suspicious, Ordering::Relaxed);
        self.priority_in_balanced
            .store(new.priority_in_balanced, Ordering::Relaxed);
        *self.load_balancing_mode.lock() = new.load_balancing_mode.clone();
        self.rate_limiter.update_config(RateLimitConfig {
            daily_max_requests: new.rate_limit_daily_max,
            min_interval_ms: new.rate_limit_min_interval_ms,
            ..RateLimitConfig::default()
        });
        // 最后原子换整份配置（源真值,供冷/温读点 load() 取新值）
        self.config.store(Arc::new(new));
        tracing::info!("配置已热重载（TIER1 运行时字段即时生效;proxy/tls/端口等固化项仍需重启）");
        Ok(())
    }

    /// 重挂主动 token 预刷新后台任务（TIER2 热重载）。
    ///
    /// 读当前 config 的 `proactive_token_refresh`/`token_refresh_lead_minutes`/
    /// `token_refresh_interval_secs`，abort 旧任务后按需 spawn 新任务：
    /// - 启动时调用一次（替代 main.rs 原内联 detached spawn，让任务"从启动即受管"）；
    /// - admin 改这三个字段后调用 → 间隔/提前量/开关即时生效，无需重启。
    ///
    /// 任务体持 `Weak<Self>`：manager 被 drop 后下一轮 upgrade 失败即自我退出，
    /// 不构成 Arc 引用环（句柄存在 self 内，闭包只借弱引用）。
    /// 幂等：重复调用先 abort 旧句柄再重建，不会累积多个循环。
    pub fn respawn_refresh_task(self: &Arc<Self>) {
        let cfg = self.config();
        let mut slot = self.refresh_task.lock();
        // 先杀旧任务（若有），无论开关如何都先停，避免旧间隔残留
        if let Some(old) = slot.take() {
            old.abort();
        }
        if !cfg.proactive_token_refresh {
            tracing::info!("主动 token 预刷新未启用（proactive_token_refresh=false），后台任务不运行");
            return;
        }
        let handle = crate::kiro::refresh_loop::spawn(
            Arc::downgrade(self),
            cfg.token_refresh_lead_minutes,
            cfg.token_refresh_interval_secs,
        );
        *slot = Some(handle);
    }

    /// 导出指定 ID 凭据的原始 KiroCredentials（用于 Admin 令牌下载）
    ///
    /// 返回可直接重新导入本系统的完整凭据（含 refreshToken/clientId 等敏感字段）。
    /// 调用方（Admin 层）必须已通过鉴权。
    pub fn export_credential(&self, id: u64) -> Option<KiroCredentials> {
        self.entries
            .lock()
            .iter()
            .find(|e| e.id == id)
            .map(|e| e.credentials.clone())
    }

    /// 获取凭据总数
    pub fn total_count(&self) -> usize {
        self.entries.lock().len()
    }

    /// 获取可用凭据数量
    pub fn available_count(&self) -> usize {
        self.entries.lock().iter().filter(|e| !e.disabled).count()
    }

    /// 获取当前所有处于冷却中的凭据快照（供 admin 面板展示 429/限流感官）。
    /// 冷却未启用时返回空。
    pub fn cooldown_snapshot(&self) -> Vec<crate::kiro::cooldown::CooldownInfo> {
        if !self.cooldown_enabled.load(Ordering::Relaxed) {
            return Vec::new();
        }
        self.cooldown.get_all_cooldowns()
    }

    /// 根据负载均衡模式选择下一个凭据，并原子性地占用一个在途名额
    ///
    /// - priority 模式：选择优先级最高（priority 最小）的可用凭据
    /// - balanced 模式：按 `(rpm 饱和, 在途数, 成功数, 优先级)` 升序选择——
    ///   优先挑"RPM 未饱和 + 当前在飞请求最少"的号，把并发流量分摊到多个账号。
    ///
    /// **并发正确性**：候选读取（含 inflight/rpm 计数）、选中、`inflight += 1`、
    /// `rpm.record` 全部在同一把 `entries.lock()` 临界区内完成，保证两个并发请求
    /// 不会同时选中同一个"最空闲"的号（第一个在释放锁前已把它的 inflight +1，
    /// 第二个看到的就是更新后的值）。这是根治惊群/Top5 热点的关键。
    ///
    /// # 参数
    /// - `model`: 可选的模型名称，用于过滤支持该模型的凭据（如 opus 模型需要付费订阅）
    ///
    /// # 返回
    /// 命中则返回 `(id, credentials, 在途守卫)`，守卫 Drop 时把该号 inflight -1。
    fn select_next_credential(
        &self,
        model: Option<&str>,
        user_id: Option<&str>,
    ) -> Option<(u64, KiroCredentials, InflightGuard)> {
        let entries = self.entries.lock();

        // 检查是否是 opus 模型
        let is_opus = model
            .map(|m| m.to_lowercase().contains("opus"))
            .unwrap_or(false);
        // 模型级黑名单键用的 kiro modelId（与 provider extract 的 modelId 同源）
        let model_key = model.unwrap_or("");

        // 过滤可用凭据：可选性判定统一收敛到 is_entry_selectable
        // （disabled / opus 订阅 / 冷却 / 限流）。历史上此处曾在其后再挂一个
        // 逐字段重复的 filter（inflight 改动残留），锁临界区内重复判定 + config 克隆
        // 翻倍；已合并为单次 filter。
        let available: Vec<&CredentialEntry> = entries
            .iter()
            .filter(|e| self.is_entry_selectable(e, is_opus, model_key))
            .collect();

        if available.is_empty() {
            return None;
        }

        // 会话亲和性：若该会话已绑定某凭据且当前可用，优先复用，让同一对话粘同一账号
        if self.affinity_enabled.load(Ordering::Relaxed) {
            if let Some(uid) = user_id {
                if let Some(bound_id) = self.affinity.get(uid) {
                    if let Some(entry) = available.iter().find(|e| e.id == bound_id) {
                        // 亲和复用的前提:绑定号未 RPM 饱和。饱和仍死粘会把高频单会话钉在一个号上
                        // 打爆(retry 慢/雪崩),旁边空闲号却不接——故饱和时**不复用**,落到下方 balanced
                        // 分流到未饱和号(临时解绑,会话下次仍可能粘回,防关联与分流兼得)。
                        // 用无锁版:此处已持 entries 锁,直传 e.credentials.rpm_limit(per-cred 容量优先)。
                        let bound_saturated =
                            self.is_rpm_saturated_with_limit(entry.id, entry.credentials.rpm_limit);
                        if !bound_saturated {
                            tracing::debug!(user_id = %uid, credential_id = %bound_id, "亲和性复用凭据");
                            // 续期，使持续活跃的会话不因 TTL 到期而解绑
                            self.affinity.touch(uid);
                            return Some(self.commit_selection(entry));
                        }
                        tracing::debug!(
                            user_id = %uid,
                            credential_id = %bound_id,
                            "亲和性绑定号已 RPM 饱和，本次不复用，改走 balanced 分流到空闲号"
                        );
                    } else {
                        // 绑定的凭据已不可用（禁用/冷却/限流），解绑后按常规策略重选
                        tracing::debug!(
                            user_id = %uid,
                            credential_id = %bound_id,
                            "亲和性绑定的凭据当前不可用，重新选择"
                        );
                    }
                }
            }
        }

        let mode = self.load_balancing_mode.lock().clone();
        let mode = mode.as_str();

        let selected = match mode {
            "balanced" => {
                // 自适应分流排序键（升序 min_by_key）：
                // ① ⭐**健康分档 neg_p_bucket**（首要）：p_avail = 熔断门×健康×(1-RPM压力)×(1-负载),
                //    量化成 0..100 桶取负 → p_avail 越高排越前。熔断 Open 的号/族 p_avail=0 自然沉底、
                //    半开期按 admit_prob 概率软降权(试探性放回)、健康差的号被压后。族键连坐:M365 同租户
                //    共享一个 health(整族一起沉),IdC/social 各自 cred:{id} 独立(坚强兜底不受连坐)。
                // ② rpm 饱和(硬软上限)③在途④近60s RPM⑤终身成功数⑥优先级:同健康档内的精细分流兜底。
                // p_avail 已内含 rpm 压力/在途,②③④作同档兜底仍保留(粒度更细+rpm_limit=0 时 p_avail 不含压力)。
                // 是否叠加优先级分发（热更开关）。开启时:先按可用性粗分层(不可用/饱和的沉底),
                // 再按 priority 分层(越小越优先),层内仍按健康/负载均衡。这样高优先级号被优先用,
                // 但整层被打爆(p_avail=0 或饱和)时优雅溢出到下一优先级层,不死磕单个坏号。
                let prio_first = self.priority_in_balanced.load(Ordering::Relaxed);
                available
                    .iter()
                    .min_by_key(|e| {
                        let key = e.credentials.family_key(e.id);
                        // per-cred RPM 容量(>0 则用本号的,否则回退全局),供 p_avail 压力项 + 饱和判定。
                        // 用 e.credentials.rpm_limit 直接取,避免在已持 entries 锁的闭包里二次锁死锁。
                        let cred_rpm_cap = e
                            .credentials
                            .rpm_limit
                            .filter(|&v| v > 0)
                            .unwrap_or_else(|| self.rpm_limit.load(Ordering::Relaxed));
                        let p = self.health.p_avail(
                            &key,
                            self.rpm.count(e.id),
                            e.inflight.load(Ordering::Acquire),
                            cred_rpm_cap,
                        );
                        // p_avail(0..1) → 0..100 桶,取负作升序首键(高 p 排前)。同桶内再走下面细分。
                        let neg_p_bucket = -((p * 100.0) as i64);
                        let saturated = self.is_rpm_saturated_with_limit(e.id, e.credentials.rpm_limit);
                        // 溢出闸:仅当该号"真不可用"(熔断 Open→p_avail=0 或 RPM 已饱和)时置 1 沉底,
                        // 保证优先级分层不会把流量钉死在一个已打爆的高优先级号上。
                        let unusable = (p <= 0.0 || saturated) as u8;
                        // 优先级键仅在开关开启时参与首排;关闭时置 0(不影响原有纯健康均衡)。
                        let prio_key = if prio_first { e.credentials.priority } else { 0 };
                        (
                            unusable,          // ① 先把真不可用的沉底(优雅溢出到下一层)
                            prio_key,          // ② 开关开:按优先级分层(越小越优先);关:恒 0
                            neg_p_bucket,      // ③ 层内健康均衡(p_avail 高排前)
                            saturated,         // ④ rpm 饱和兜底
                            e.inflight.load(Ordering::Acquire), // ⑤ 在途
                            self.rpm.count(e.id),               // ⑥ 近 60s RPM
                            e.success_count,                    // ⑦ 终身成功数
                            e.credentials.priority,             // ⑧ 优先级末位兜底(开关关时唯一 priority 参与点)
                        )
                    })
                    .copied()
            }
            _ => {
                // priority 模式（默认）：选择优先级最高的
                available
                    .iter()
                    .min_by_key(|e| e.credentials.priority)
                    .copied()
            }
        };

        let selected = selected?;

        // 新选中的凭据与会话建立绑定，使后续同会话请求复用
        if self.affinity_enabled.load(Ordering::Relaxed) {
            if let Some(uid) = user_id {
                self.affinity.set(uid, selected.id);
            }
        }

        Some(self.commit_selection(selected))
    }

    /// 提交一次选号：在持有 `entries` 锁的前提下原子占用在途名额并记录 RPM。
    ///
    /// 必须在 `select_next_credential` 的 `entries.lock()` 临界区内调用，
    /// 以保证 `inflight += 1` 相对其它并发选号是原子可见的。
    fn is_entry_selectable(&self, entry: &CredentialEntry, is_opus: bool, model: &str) -> bool {
        if entry.disabled {
            return false;
        }
        if is_opus && !entry.credentials.supports_opus() {
            return false;
        }
        // 模型级黑名单：该号曾对此模型返回 INVALID_MODEL_ID（订阅不含）→ 仅对此模型跳过它，
        // 该号对其它模型不受影响。TTL 到期后自动放行重试探。
        if self.is_model_blocked(entry.id, model) {
            return false;
        }
        if self.cooldown_enabled.load(Ordering::Relaxed) && !self.cooldown.is_available(entry.id) {
            return false;
        }
        if self.rate_limit_enabled.load(Ordering::Relaxed) && self.rate_limiter.check_rate_limit(entry.id).is_err() {
            return false;
        }
        // ⚠️ inflight 绝不作为「可选性」的硬门槛。
        // 本项目的调度设计是：inflight（在飞请求数）只进 select_next_credential 的
        // 排序键——优先选在飞最少的号，把并发自然分摊；号不够时并发落到同一号由
        // RPM 软降权调节，而不是把请求卡在网关里排队干等。
        // （历史上曾被硬编码 inflight < 1 阻塞成"每号同时只 1 个请求"，多客户端下
        //  多余请求全排队 = 假性限流、体感极慢。此处恢复为不阻塞。）
        true
    }

    fn transient_wait_duration(&self, model: Option<&str>) -> Option<StdDuration> {
        let is_opus = model
            .map(|m| m.to_lowercase().contains("opus"))
            .unwrap_or(false);
        let entries = self.entries.lock();
        let mut has_candidate = false;
        let mut waits = Vec::new();

        for entry in entries.iter() {
            if entry.disabled {
                continue;
            }
            if is_opus && !entry.credentials.supports_opus() {
                continue;
            }

            has_candidate = true;

            if self.cooldown_enabled.load(Ordering::Relaxed) {
                if let Some((_reason, remaining)) = self.cooldown.check_cooldown(entry.id) {
                    waits.push(remaining);
                    continue;
                }
            }

            if self.rate_limit_enabled.load(Ordering::Relaxed) {
                if let Err(wait) = self.rate_limiter.check_rate_limit(entry.id) {
                    waits.push(wait);
                    continue;
                }
            }

            // 注意：不再因 inflight「繁忙」而等待——inflight 不是阻塞门槛，
            // 只要该号未禁用/未冷却/未限流，就是当下可选的候选（并发直接落它）。
            // 走到这里说明有一个立即可用的候选，无需等待。
        }

        if !has_candidate {
            return None;
        }

        // 只有当所有候选都在冷却/限流(将来会自动恢复)时才等待其中最短的那个；
        // 若存在立即可用候选(waits 为空)则不等待。
        waits.into_iter().min()
    }

    fn commit_selection(&self, entry: &CredentialEntry) -> (u64, KiroCredentials, InflightGuard) {
        let guard = InflightGuard::acquire(entry.inflight.clone());
        self.rpm.record(entry.id);
        (entry.id, entry.credentials.clone(), guard)
    }

    /// 该凭据在滚动 60 秒窗口内是否已达 RPM 软上限（rpm_limit == 0 时恒为 false）
    /// 该号近 60s RPM 是否达到容量上限（按 id,会短暂锁 entries 取 per-cred 容量）。
    ///
    /// ⚠️ 绝不能在已持 entries 锁时调用(parking_lot 非重入会死锁);选号热路径已持锁,
    /// 用 [`Self::is_rpm_saturated_with_limit`] 直接传入 per-cred 容量避免二次锁。
    /// 容量优先级:凭据级 `rpm_limit`(体质好的号可设高) > 全局 `credential_rpm_limit`。
    fn is_rpm_saturated(&self, id: u64) -> bool {
        let per_cred = {
            let entries = self.entries.lock();
            entries
                .iter()
                .find(|e| e.id == id)
                .and_then(|e| e.credentials.rpm_limit)
                .filter(|&v| v > 0)
        };
        self.is_rpm_saturated_with_limit(id, per_cred)
    }

    /// 无锁版:调用方已持 entries 锁时用,直接传该号的凭据级 rpm_limit。
    ///
    /// 容量优先级:凭据级 rpm_limit(>0) > 全局 credential_rpm_limit(>0) > **默认高水位兜底**。
    /// 默认兜底(SATURATION_FALLBACK_RPM=30)是"默认配置也最优"的关键:两者都没设时,不再
    /// "恒不饱和→affinity 死粘单号打爆"(retry 慢根因),而是在 ~30rpm/号(正好在上游
    /// USER_REQUEST_RATE_EXCEEDED 硬限之前)判饱和,让 affinity 解绑 + balanced 分流到空闲号。
    /// 体质好的号设 per-cred rpm_limit=100 即用 100,弱号/默认用 30 兜底。
    fn is_rpm_saturated_with_limit(&self, id: u64, per_cred_limit: Option<u32>) -> bool {
        let lim = self.effective_saturation_limit(per_cred_limit);
        self.rpm.count(id) >= lim
    }

    /// 有效饱和阈值:per-cred(>0) > 全局(>0) > 默认高水位兜底(30)。恒 >0,保证分流生效。
    fn effective_saturation_limit(&self, per_cred_limit: Option<u32>) -> u32 {
        const SATURATION_FALLBACK_RPM: u32 = 30;
        per_cred_limit
            .filter(|&v| v > 0)
            .or_else(|| {
                let g = self.rpm_limit.load(Ordering::Relaxed);
                (g > 0).then_some(g)
            })
            .unwrap_or(SATURATION_FALLBACK_RPM)
    }

    /// 获取 API 调用上下文
    ///
    /// 返回绑定了 id、credentials 和 token 的调用上下文
    /// 确保整个 API 调用过程中使用一致的凭据信息
    ///
    /// 如果 Token 过期或即将过期，会自动刷新
    /// Token 刷新失败会累计到当前凭据，达到阈值后禁用并切换
    ///
    /// # 参数
    /// - `model`: 可选的模型名称，用于过滤支持该模型的凭据（如 opus 模型需要付费订阅）
    /// - `user_id`: 可选的会话标识（取自请求 conversationId），用于会话亲和性
    pub async fn acquire_context(
        &self,
        model: Option<&str>,
        user_id: Option<&str>,
    ) -> anyhow::Result<CallContext> {
        let total = self.total_count();
        // 内层尝试预算需与 provider 层的外层重试预算同量级放开：
        // 以可用凭据数为下限，保证内层不会在外层遍历完所有可用号之前就先耗尽。
        // （历史上仅 total*MAX_FAILURES，当可用数因禁用波动大时可能过紧）
        let max_attempts = (total * MAX_FAILURES_PER_CREDENTIAL as usize)
            .max(self.available_count())
            .max(1);
        let mut attempt_count = 0;
        let wait_started = Instant::now();
        let is_opus = model
            .map(|m| m.to_lowercase().contains("opus"))
            .unwrap_or(false);

        loop {
            if attempt_count >= max_attempts {
                anyhow::bail!(
                    "所有凭据均无法获取有效 Token（可用: {}/{}）",
                    self.available_count(),
                    total
                );
            }

            // credentials 快照仅用于选号阶段（commit_selection 已占在途名额）；
            // token 获取改由 try_ensure_token 内部按 id 重读最新凭据，故此处不再透传。
            let (id, _credentials, inflight) = {
                let is_balanced = self.load_balancing_mode.lock().as_str() == "balanced";

                // balanced 模式：每次请求都重新均衡选择，不固定 current_id
                // priority 模式：优先使用 current_id 指向的凭据
                // 命中时在同一 entries 锁内占用在途名额（commit_selection），
                // 保证 inflight 计数相对并发选号原子可见。
                let current_hit = if is_balanced {
                    None
                } else {
                    let entries = self.entries.lock();
                    let current_id = *self.current_id.lock();
                    entries
                        .iter()
                        .find(|e| e.id == current_id && self.is_entry_selectable(e, is_opus, model.unwrap_or("")))
                        .map(|e| self.commit_selection(e))
                };

                if let Some(hit) = current_hit {
                    hit
                } else {
                    // 当前凭据不可用或 balanced 模式，根据负载均衡策略选择
                    let mut best = self.select_next_credential(model, user_id);

                    // 没有可用凭据：如果是"自动禁用导致全灭"，做一次类似重启的自愈
                    if best.is_none() {
                        let mut entries = self.entries.lock();
                        if entries.iter().any(|e| {
                            e.disabled && e.disabled_reason == Some(DisabledReason::TooManyFailures)
                        }) {
                            tracing::warn!(
                                "所有凭据均已被自动禁用，执行自愈：重置失败计数并重新启用（等价于重启）"
                            );
                            for e in entries.iter_mut() {
                                if e.disabled_reason == Some(DisabledReason::TooManyFailures) {
                                    e.disabled = false;
                                    e.disabled_reason = None;
                                    e.failure_count = 0;
                                }
                            }
                            drop(entries);
                            best = self.select_next_credential(model, user_id);
                        }
                    }

                    if let Some((new_id, new_creds, guard)) = best {
                        // 更新 current_id
                        let mut current_id = self.current_id.lock();
                        *current_id = new_id;
                        (new_id, new_creds, guard)
                    } else {
                        if let Some(wait) = self.transient_wait_duration(model) {
                            // 全池快速失败(吸收 fork 做法):当最短恢复时间较长(>2s,典型是冷却/风控)时,
                            // 不在网关内硬扛,立即带 retry_after_secs 透传,让客户端(Claude Code)自己退避重试。
                            // 客户端退避比网关反复选号温和,也减少对被风控号的零星试探。
                            // 只有"马上(≤2s)就能恢复"的瞬时繁忙才短等一下,避免把秒级抖动也甩给客户端。
                            const FAST_FAIL_THRESHOLD: StdDuration = StdDuration::from_secs(2);
                            if self.all_cooling_fast_fail.load(Ordering::Relaxed) && wait > FAST_FAIL_THRESHOLD {
                                let retry_after = wait.as_secs().max(1);
                                let entries = self.entries.lock();
                                let available = entries.iter().filter(|e| !e.disabled).count();
                                drop(entries);
                                tracing::warn!(
                                    "所有可用凭据均在冷却，最短恢复 {}s，快速返回 429+Retry-After 让客户端退避（不在网关内硬扛）",
                                    retry_after
                                );
                                anyhow::bail!(
                                    "所有凭据均在冷却（{}/{}）retry_after_secs={}",
                                    available,
                                    total,
                                    retry_after
                                );
                            }
                            if wait_started.elapsed() < StdDuration::from_secs(MAX_TRANSIENT_WAIT_SECS) {
                                let wait = wait
                                    .max(StdDuration::from_millis(250))
                                    .min(StdDuration::from_secs(2));
                                tracing::warn!(
                                    "所有可用凭据暂时繁忙，短等 {:?} 后重试",
                                    wait
                                );
                                sleep(wait).await;
                                continue;
                            }
                        }
                        let entries = self.entries.lock();
                        // 注意：必须在 bail! 之前计算 available_count，
                        // 因为 available_count() 会尝试获取 entries 锁，
                        // 而此时我们已经持有该锁，会导致死锁
                        let available = entries.iter().filter(|e| !e.disabled).count();
                        anyhow::bail!("所有凭据均已禁用（{}/{}）", available, total);
                    }
                }
            };

            // 尝试获取/刷新 Token（成功则把在途守卫移入 CallContext 随请求存活）
            match self.try_ensure_token(id, inflight).await {
                Ok(ctx) => {
                    // 记录一次速率获取（递增每日计数 + 标记本次请求时间，驱动最小间隔）
                    if self.rate_limit_enabled.load(Ordering::Relaxed) {
                        if let Err(wait) = self.rate_limiter.try_acquire(id) {
                            tracing::debug!("凭据 #{} 速率受限，需等待 {:?}，重新选择", id, wait);
                            // 该凭据本轮不可用，换下一个；select 已会过滤它
                            attempt_count += 1;
                            continue;
                        }
                    }
                    return Ok(ctx);
                }
                Err(e) => {
                    // refreshToken 永久失效 → 立即禁用，不累计重试
                    let has_available =
                        if e.downcast_ref::<RefreshTokenInvalidError>().is_some() {
                            tracing::warn!("凭据 #{} refreshToken 永久失效: {}", id, e);
                            self.report_refresh_token_invalid(id)
                        } else {
                            tracing::warn!("凭据 #{} Token 刷新失败: {}", id, e);
                            self.report_refresh_failure(id)
                        };
                    attempt_count += 1;
                    if !has_available {
                        anyhow::bail!("所有凭据均已禁用（0/{}）", total);
                    }
                }
            }
        }
    }

    /// 选择优先级最高的未禁用凭据作为当前凭据（内部方法）
    ///
    /// 纯粹按优先级选择，不排除当前凭据，用于优先级变更后立即生效
    fn select_highest_priority(&self) {
        let entries = self.entries.lock();
        let mut current_id = self.current_id.lock();

        // 选择优先级最高的未禁用凭据（不排除当前凭据）
        if let Some(best) = entries
            .iter()
            .filter(|e| !e.disabled)
            .min_by_key(|e| e.credentials.priority)
        {
            if best.id != *current_id {
                tracing::info!(
                    "优先级变更后切换凭据: #{} -> #{}（优先级 {}）",
                    *current_id,
                    best.id,
                    best.credentials.priority
                );
                *current_id = best.id;
            }
        }
    }

    /// 确保指定凭据持有有效 access token，返回 `(最新凭据快照, 可用 token)`。
    ///
    /// 收敛原先散落在 `try_ensure_token` / `get_usage_limits_for` /
    /// `web_portal_context_for` / `deep_verify_credential` 四处几乎逐字复制的
    /// 「双检刷新」块。刷新一律委托给唯一带「陈旧 refresh_token 快照守卫」的
    /// [`refresh_token_locked`]（守卫 / 持久化 / profileArn 动态解析单一真源），
    /// 杜绝各处裸调 `refresh_token` 后盲写回——那会把已被其它并发路径轮换出的新
    /// token 覆盖回旧值，导致下次刷新用作废的 refresh_token 而把活号刷死。
    ///
    /// 分流：
    /// - API Key 凭据：直接返回 kiroApiKey 作为 token，不触发刷新。
    /// - token 未过期且非即将过期：热路径直接返回，不碰 `refresh_lock`
    ///   （否则每个请求都串行化，性能回归）。
    /// - 需刷新：委托 `refresh_token_locked(id, None)`，`?` 让
    ///   [`RefreshTokenInvalidError`] 原样上抛，保住上层 downcast 后
    ///   「永久失效 → 立即禁用」的语义。
    async fn ensure_valid_token(&self, id: u64) -> anyhow::Result<(KiroCredentials, String)> {
        // 读取当前凭据快照
        let credentials = {
            let entries = self.entries.lock();
            entries
                .iter()
                .find(|e| e.id == id)
                .map(|e| e.credentials.clone())
                .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?
        };

        // API Key 凭据直接使用 kiroApiKey 作为 Bearer Token，无需刷新
        if credentials.is_api_key_credential() {
            let token = credentials
                .kiro_api_key
                .clone()
                .ok_or_else(|| anyhow::anyhow!("API Key 凭据缺少 kiroApiKey"))?;
            return Ok((credentials, token));
        }

        // 热路径：token 未过期且非即将过期 → 直接返回，不碰 refresh_lock
        if !is_token_expired(&credentials) && !is_token_expiring_soon(&credentials) {
            let token = credentials
                .access_token
                .clone()
                .ok_or_else(|| anyhow::anyhow!("凭据无 access_token"))?;
            return Ok((credentials, token));
        }

        // 需刷新：委托带守卫的唯一刷新实现（? 保 RefreshTokenInvalidError 原样上抛）
        self.refresh_token_locked(id, None).await?;

        // 重读取最新凭据（可能由本次刷新或其它并发路径刷新完成）
        let refreshed = {
            let entries = self.entries.lock();
            entries
                .iter()
                .find(|e| e.id == id)
                .map(|e| e.credentials.clone())
                .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?
        };
        let token = refreshed
            .access_token
            .clone()
            .ok_or_else(|| anyhow::anyhow!("刷新后无 access_token"))?;
        Ok((refreshed, token))
    }

    /// 尝试使用指定凭据获取有效 Token（请求热路径）
    ///
    /// token 获取 / 刷新收敛到 [`ensure_valid_token`]；本函数只保留调用点独有逻辑：
    /// 成功拿到 token 后重置该凭据的刷新失败计数。
    ///
    /// # Arguments
    /// * `id` - 凭据 ID，用于更新正确的条目
    /// * `inflight` - 选号时占用的在途守卫；成功则移入 `CallContext` 随请求存活，
    ///   失败则随本函数返回而 Drop（该次尝试不再在途，inflight -1）。
    async fn try_ensure_token(
        &self,
        id: u64,
        inflight: InflightGuard,
    ) -> anyhow::Result<CallContext> {
        let (credentials, token) = self.ensure_valid_token(id).await?;

        // 调用点独有逻辑：成功获取 token → 重置刷新失败计数
        {
            let mut entries = self.entries.lock();
            if let Some(entry) = entries.iter_mut().find(|e| e.id == id) {
                entry.refresh_failure_count = 0;
            }
        }

        Ok(CallContext {
            id,
            credentials,
            token,
            inflight,
        })
    }

    /// 将凭据列表回写到源文件
    ///
    /// 仅在以下条件满足时回写：
    /// - 源文件是多凭据格式（数组）
    /// - credentials_path 已设置
    ///
    /// # Returns
    /// - `Ok(true)` - 成功写入文件
    /// - `Ok(false)` - 跳过写入（非多凭据格式或无路径配置）
    /// - `Err(_)` - 写入失败
    fn persist_credentials(&self) -> anyhow::Result<bool> {
        use anyhow::Context;

        // 仅多凭据格式才回写
        if !self.is_multiple_format {
            return Ok(false);
        }

        let path = match &self.credentials_path {
            Some(p) => p,
            None => return Ok(false),
        };

        // 收集所有凭据
        let credentials: Vec<KiroCredentials> = {
            let entries = self.entries.lock();
            entries
                .iter()
                .map(|e| {
                    let mut cred = e.credentials.clone();
                    cred.canonicalize_auth_method();
                    // 同步 disabled 状态到凭据对象
                    cred.disabled = e.disabled;
                    cred
                })
                .collect()
        };

        // 序列化为 pretty JSON
        let json = serde_json::to_string_pretty(&credentials).context("序列化凭据失败")?;

        // 原子写入文件（在 Tokio runtime 内使用 block_in_place 避免阻塞 worker）
        if tokio::runtime::Handle::try_current().is_ok() {
            tokio::task::block_in_place(|| write_atomic(path, json.as_bytes()))
                .with_context(|| format!("回写凭据文件失败: {:?}", path))?;
        } else {
            write_atomic(path, json.as_bytes())
                .with_context(|| format!("回写凭据文件失败: {:?}", path))?;
        }

        tracing::debug!("已回写凭据到文件: {:?}", path);
        Ok(true)
    }

    /// 获取缓存目录（凭据文件所在目录）
    pub fn cache_dir(&self) -> Option<PathBuf> {
        self.credentials_path
            .as_ref()
            .and_then(|p| p.parent().map(|d| d.to_path_buf()))
    }

    /// 回收站文件路径（cache_dir/trash.json）
    fn trash_path(&self) -> Option<PathBuf> {
        self.cache_dir().map(|d| d.join("trash.json"))
    }

    /// 从磁盘加载回收站（trash.json）
    ///
    /// 仅多凭据格式才有持久化文件；单凭据格式下回收站为纯内存态。
    /// 文件不存在或解析失败时静默回退为空。
    fn load_trash(&self) {
        if !self.is_multiple_format {
            return;
        }
        let path = match self.trash_path() {
            Some(p) => p,
            None => return,
        };
        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => return, // 首次运行时文件不存在
        };
        if content.trim().is_empty() {
            return;
        }
        let items: Vec<TrashEntry> = match serde_json::from_str(&content) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("解析回收站失败，将忽略: {}", e);
                return;
            }
        };
        let count = items.len();
        *self.trash.lock() = items;
        tracing::info!("已从回收站加载 {} 条已删除凭据", count);
    }

    /// 将回收站持久化到磁盘（仿 persist_credentials）
    ///
    /// # Returns
    /// - `Ok(true)` - 成功写入文件
    /// - `Ok(false)` - 跳过写入（非多凭据格式或无路径配置）
    /// - `Err(_)` - 写入失败
    fn persist_trash(&self) -> anyhow::Result<bool> {
        use anyhow::Context;

        // 仅多凭据格式才回写（单凭据格式下回收站仅内存态）
        if !self.is_multiple_format {
            return Ok(false);
        }

        let path = match self.trash_path() {
            Some(p) => p,
            None => return Ok(false),
        };

        let items: Vec<TrashEntry> = self.trash.lock().clone();

        // 序列化为 pretty JSON
        let json = serde_json::to_string_pretty(&items).context("序列化回收站失败")?;

        // 原子写入文件（在 Tokio runtime 内使用 block_in_place 避免阻塞 worker）
        if tokio::runtime::Handle::try_current().is_ok() {
            tokio::task::block_in_place(|| write_atomic(&path, json.as_bytes()))
                .with_context(|| format!("回写回收站文件失败: {:?}", path))?;
        } else {
            write_atomic(&path, json.as_bytes())
                .with_context(|| format!("回写回收站文件失败: {:?}", path))?;
        }

        tracing::debug!("已回写回收站到文件: {:?}", path);
        Ok(true)
    }

    /// 统计数据文件路径
    fn stats_path(&self) -> Option<PathBuf> {
        self.cache_dir().map(|d| d.join("kiro_stats.json"))
    }

    /// 从磁盘加载统计数据并应用到当前条目
    fn load_stats(&self) {
        let path = match self.stats_path() {
            Some(p) => p,
            None => return,
        };

        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => return, // 首次运行时文件不存在
        };

        let stats: HashMap<String, StatsEntry> = match serde_json::from_str(&content) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("解析统计缓存失败，将忽略: {}", e);
                return;
            }
        };

        let mut entries = self.entries.lock();
        for entry in entries.iter_mut() {
            if let Some(s) = stats.get(&entry.id.to_string()) {
                entry.success_count = s.success_count;
                entry.total_credits_used = s.total_credits_used;
                entry.last_used_at = s.last_used_at.clone();
            }
        }
        *self.last_stats_save_at.lock() = Some(Instant::now());
        self.stats_dirty.store(false, Ordering::Relaxed);
        tracing::info!("已从缓存加载 {} 条统计数据", stats.len());
    }

    /// 将当前统计数据持久化到磁盘
    fn save_stats(&self) {
        let path = match self.stats_path() {
            Some(p) => p,
            None => return,
        };

        let stats: HashMap<String, StatsEntry> = {
            let entries = self.entries.lock();
            entries
                .iter()
                .map(|e| {
                    (
                        e.id.to_string(),
                        StatsEntry {
                            success_count: e.success_count,
                            total_credits_used: e.total_credits_used,
                            last_used_at: e.last_used_at.clone(),
                        },
                    )
                })
                .collect()
        };

        match serde_json::to_string_pretty(&stats) {
            Ok(json) => {
                if let Err(e) = write_atomic(&path, json.as_bytes()) {
                    tracing::warn!("保存统计缓存失败: {}", e);
                } else {
                    *self.last_stats_save_at.lock() = Some(Instant::now());
                    self.stats_dirty.store(false, Ordering::Relaxed);
                }
            }
            Err(e) => tracing::warn!("序列化统计数据失败: {}", e),
        }
    }

    /// 标记统计数据已更新，并按 debounce 策略决定是否立即落盘
    fn save_stats_debounced(&self) {
        self.stats_dirty.store(true, Ordering::Relaxed);

        let should_flush = {
            let last = *self.last_stats_save_at.lock();
            match last {
                Some(last_saved_at) => last_saved_at.elapsed() >= STATS_SAVE_DEBOUNCE,
                None => true,
            }
        };

        if should_flush {
            self.save_stats();
        }
    }

    /// 报告指定凭据 API 调用成功
    ///
    /// 重置该凭据的失败计数
    ///
    /// # Arguments
    /// * `id` - 凭据 ID（来自 CallContext）
    pub fn report_success(&self, id: u64) {
        let fam = self.family_key_of(id);
        {
            let mut entries = self.entries.lock();
            if let Some(entry) = entries.iter_mut().find(|e| e.id == id) {
                entry.failure_count = 0;
                entry.refresh_failure_count = 0;
                entry.success_count += 1;
                entry.last_used_at = Some(Utc::now().to_rfc3339());
                tracing::debug!(
                    "凭据 #{} API 调用成功（累计 {} 次）",
                    id,
                    entry.success_count
                );
            }
        }
        // 成功：清除冷却并记录速率成功（重置连续失败/退避）
        if self.cooldown_enabled.load(Ordering::Relaxed) {
            self.cooldown.clear_cooldown(id);
        }
        if self.rate_limit_enabled.load(Ordering::Relaxed) {
            self.rate_limiter.record_success(id);
        }
        // 健康：成功抬 ewma_success、衰减 ewma_429;半开期连续成功 AIMD 逐步放回直至全开。
        // 键用 family_key（族/号同口径），锁外调用（health 独立 Mutex，避免与 entries 锁嵌套）。
        self.health.on_success(&fam);
        self.save_stats_debounced();
    }

    /// 累加一次请求的真实 credit 花费到该凭据的**生命周期累计**。
    ///
    /// 在请求完成、拿到上游 meteringEvent 的真实计费量后调用（见 anthropic/handlers.rs
    /// 的 emit_record 处）。累计值持久化进 kiro_stats.json，独立于用量明细的保留期清理，
    /// 只增不清——供凭据卡片展示"这个号从入池至今一共花了多少 credit"。
    ///
    /// `credits <= 0` 或 `credential_id` 未知时静默忽略（无 meteringEvent 的请求本就不计）。
    pub fn add_credits(&self, id: u64, credits: f64) {
        if !(credits > 0.0) {
            return;
        }
        {
            let mut entries = self.entries.lock();
            match entries.iter_mut().find(|e| e.id == id) {
                Some(entry) => entry.total_credits_used += credits,
                None => return, // 未知 id（已删除等）：不落账
            }
        }
        self.save_stats_debounced();
    }

    /// 按 id 取该凭据的 family_key（M365 号→族键连坐；IdC/social→cred:{id} 独立）。
    /// 找不到该 id（已删除等）时回退 `cred:{id}`，保证 health 键始终可用。
    fn family_key_of(&self, id: u64) -> String {
        let entries = self.entries.lock();
        entries
            .iter()
            .find(|e| e.id == id)
            .map(|e| e.credentials.family_key(e.id))
            .unwrap_or_else(|| format!("cred:{id}"))
    }

    /// 报告指定凭据 API 调用失败
    ///
    /// 增加失败计数，达到阈值时禁用凭据并切换到优先级最高的可用凭据
    /// 返回是否还有可用凭据可以重试
    ///
    /// # Arguments
    /// * `id` - 凭据 ID（来自 CallContext）
    pub fn report_failure(&self, id: u64) -> bool {
        let mut disabled_now = false;
        let result = {
            let mut entries = self.entries.lock();
            let mut current_id = self.current_id.lock();

            let entry = match entries.iter_mut().find(|e| e.id == id) {
                Some(e) => e,
                None => return entries.iter().any(|e| !e.disabled),
            };

            if entry.disabled {
                return entries.iter().any(|e| !e.disabled);
            }

            entry.failure_count += 1;
            entry.last_used_at = Some(Utc::now().to_rfc3339());
            let failure_count = entry.failure_count;

            tracing::warn!(
                "凭据 #{} API 调用失败（{}/{}）",
                id,
                failure_count,
                MAX_FAILURES_PER_CREDENTIAL
            );

            if failure_count >= MAX_FAILURES_PER_CREDENTIAL {
                entry.disabled = true;
                entry.disabled_reason = Some(DisabledReason::TooManyFailures);
                disabled_now = true;
                tracing::error!("凭据 #{} 已连续失败 {} 次，已被禁用", id, failure_count);

                // 切换到优先级最高的可用凭据
                if let Some(next) = entries
                    .iter()
                    .filter(|e| !e.disabled)
                    .min_by_key(|e| e.credentials.priority)
                {
                    *current_id = next.id;
                    tracing::info!(
                        "已切换到凭据 #{}（优先级 {}）",
                        next.id,
                        next.credentials.priority
                    );
                } else {
                    tracing::error!("所有凭据均已禁用！");
                }
            }

            entries.iter().any(|e| !e.disabled)
        };
        // 凭据被自动禁用时，清除其会话亲和性绑定，避免后续请求反复重选到已禁用凭据
        if disabled_now {
            self.affinity.remove_by_credential(id);
        }
        // 记录速率失败（瞬态：驱动指数退避，秒级自愈）
        if self.rate_limit_enabled.load(Ordering::Relaxed) {
            self.rate_limiter.record_failure(id, FailureKind::Transient);
        }
        self.save_stats_debounced();
        result
    }

    /// 报告凭据触发上游瞬态限流（429/5xx），可携带上游给出的精确重置秒数。
    ///
    /// 不禁用凭据、不计入永久失败，仅设置一段短冷却让调度暂时跳过它，
    /// 配合 provider 的退避重试，避免反复打同一个正在限流的凭据。
    ///
    /// `retry_after_secs` 来自响应头 `Retry-After` 或错误 body（如 `resets_in_seconds`）。
    /// 有则据此设定精确冷却，避免盲目指数退避浪费；无则回退到分级递增冷却。
    pub fn report_rate_limited_with_retry_after(&self, id: u64, retry_after_secs: Option<u64>) {
        if self.cooldown_enabled.load(Ordering::Relaxed) {
            // 有 Retry-After：按上游指定时长冷却，但钳制上限，避免上游给超大 resets_at
            // （如「本月配额，几天后重置」）把号冻几天——那类应走配额耗尽禁用，不该塞进短冷却。
            const MAX_RETRY_AFTER_COOLDOWN_SECS: u64 = 600;
            let dur = match retry_after_secs {
                Some(secs) if secs > 0 => self.cooldown.set_cooldown_with_duration(
                    id,
                    CooldownReason::RateLimitExceeded,
                    Some(std::time::Duration::from_secs(
                        secs.min(MAX_RETRY_AFTER_COOLDOWN_SECS),
                    )),
                ),
                // 裸 429（无 Retry-After，通常是瞬时 burst）：固定基线冷却，不指数升级。
                // 用分级递增会把几秒自愈的 burst 拖成几十秒长冷却、进而压垮小号池（自造雪崩）。
                _ => self
                    .cooldown
                    .set_transient_cooldown(id, CooldownReason::RateLimitExceeded),
            };
            tracing::warn!(
                "凭据 #{} 触发限流，冷却 {:?}{}",
                id,
                dur,
                if retry_after_secs.is_some() {
                    "（上游指定）"
                } else {
                    ""
                }
            );
        }
        if self.rate_limit_enabled.load(Ordering::Relaxed) {
            // 429 是瞬态限流，走秒级指数退避；绝不能长冻（真封号走 report_account_suspended）
            self.rate_limiter.record_failure(id, FailureKind::Transient);
        }
        // 健康：裸 429 走单号 health（family_key 对 IdC 是 cred:{id};对 M365 是族键——
        // 但普通 429 不像 suspicious 那样整族连坐,这里仍按该号自己的键累计即可,连续达阈值单号跳闸）。
        self.health.on_429(&self.family_key_of(id));
    }

    /// 报告凭据触发**账户级可疑活动风控**（`suspicious activity`+`temporary limits`）。
    ///
    /// 与普通 429 的关键区别：走 [`CooldownReason::SuspiciousActivity`] 的**分钟级冷却**
    /// （基线 3min，递增至上限 30min），而非普通限速的 15s 瞬时冷却。
    /// 原因：可疑活动是账户级风控、持续数分钟且 Kiro 正在"调查"——15s 后重新入池会被
    /// 立刻再限，反复砸只会加重可疑度、把账户推向真封禁。此处让该号**真正退避**，
    /// 冷却期内不参与选号，等调查窗口过去自愈。不禁用、不计永久失败、不改发往上游字节。
    pub fn report_suspicious_activity(&self, id: u64) {
        if self.cooldown_enabled.load(Ordering::Relaxed) {
            let dur = self
                .cooldown
                .set_cooldown(id, CooldownReason::SuspiciousActivity);
            tracing::warn!(
                "凭据 #{} 触发账户级可疑活动风控，冷却 {:?}（分钟级退避，避免反复砸加重风控/触发封禁）",
                id,
                dur
            );

            // ⭐健康/族级连坐：M365 账户族级风控——同一租户的号共享 family_key,
            // 一个号触发 suspicious 就让**整族**进熔断 Open(用 cooldown 给的硬窗 dur 作 backoff),
            // 选号时同族其它号 p_avail=0 一起沉底、不再逐个砸(治雪崩)。IdC/social 的 cred:{id}
            // 只连坐它自己(键独立),坚强兜底不受影响。冷却硬窗过后 health 走半开渐进放回。
            self.health.report_family_suspicious(&self.family_key_of(id), dur);

            // 自动禁用(dwgx:账户不行了就自动禁用)：连续可疑活动触发达阈值,说明该号已被 Kiro 盯死、
            // 冷却也顶格(30min)仍反复被限——继续放它参与调度只会不停砸、加重风控甚至触发真封禁。
            // 达阈值即自动禁用并标注 SuspiciousActivityAuto(可人工/自愈重新启用),把它移出轮转。
            const AUTO_DISABLE_TRIGGER: u32 = 10;
            if self.auto_disable_suspicious.load(Ordering::Relaxed) && self.cooldown.trigger_count(id) >= AUTO_DISABLE_TRIGGER {
                let mut entries = self.entries.lock();
                if let Some(entry) = entries.iter_mut().find(|e| e.id == id) {
                    if !entry.disabled {
                        entry.disabled = true;
                        entry.disabled_reason = Some(DisabledReason::SuspiciousActivityAuto);
                        tracing::warn!(
                            "凭据 #{} 连续可疑活动风控达 {} 次，自动禁用（SuspiciousActivityAuto），移出调度避免继续加重风控",
                            id,
                            AUTO_DISABLE_TRIGGER
                        );
                        drop(entries);
                        let _ = self.persist_credentials();
                    }
                }
            }
        }
        if self.rate_limit_enabled.load(Ordering::Relaxed) {
            // 可疑活动风控是瞬态（账户级软风控，会自愈）：限速器只需秒级退避即可，
            // 真正的分钟级退避由上面的 cooldown（SuspiciousActivity）承担；这里绝不长冻。
            self.rate_limiter.record_failure(id, FailureKind::Transient);
        }
    }

    /// 报告凭据认证失败，设置较长冷却（配合 force-refresh 失败后调用）
    pub fn report_auth_cooldown(&self, id: u64) {
        if self.cooldown_enabled.load(Ordering::Relaxed) {
            let dur = self
                .cooldown
                .set_cooldown(id, CooldownReason::AuthenticationFailed);
            tracing::warn!("凭据 #{} 认证失败，冷却 {:?}", id, dur);
        }
    }

    /// 报告"凭据 #id 对模型 model 返回 `INVALID_MODEL_ID`"（该号的订阅不含此模型）。
    ///
    /// ⭐**模型级**处置（修正 v0.6.0 致命缺陷）：只把"该号+该模型"记进短期黑名单，
    /// 选号时**仅对这个模型**跳过它——该号对其它模型（如它仍支持的 sonnet/haiku）照常参与
    /// 调度。**绝不**冷却/禁用整个号（那会让一个客户端请求一个订阅不含的模型就打垮全池）。
    ///
    /// 返回：本模型是否还有其它候选号可试（供 provider 决定 failover 还是把真 400 透传给客户端）。
    /// 当所有未禁用的号都已对该模型进黑名单时返回 false → provider 透传真实 INVALID_MODEL_ID。
    pub fn report_model_invalid(&self, id: u64, model: Option<&str>) -> bool {
        let model = model.unwrap_or("").to_string();
        {
            let mut bl = self.model_blocklist.lock();
            bl.insert((id, model.clone()), Instant::now());
        }
        tracing::warn!(
            "凭据 #{} 对模型 {:?} 返回 INVALID_MODEL_ID（该号订阅不含此模型），仅对此模型跳过该号并 failover；该号对其它模型仍可用",
            id, model
        );
        self.count_selectable_for_model(&model) > 0
    }

    /// 判断"凭据 #id + 模型 model"当前是否在模型级黑名单内（未过 TTL）。惰性清理过期项。
    fn is_model_blocked(&self, id: u64, model: &str) -> bool {
        if model.is_empty() {
            return false;
        }
        let mut bl = self.model_blocklist.lock();
        match bl.get(&(id, model.to_string())) {
            Some(&t) if t.elapsed() < MODEL_BLOCK_TTL => true,
            Some(_) => {
                bl.remove(&(id, model.to_string()));
                false
            }
            None => false,
        }
    }

    /// 统计对指定模型仍可选的凭据数（未禁用 && 未对该模型进黑名单）。
    /// model 为空串时退化为 available_count（无模型维度）。
    fn count_selectable_for_model(&self, model: &str) -> usize {
        let entries = self.entries.lock();
        entries
            .iter()
            .filter(|e| !e.disabled)
            .filter(|e| model.is_empty() || !self.is_model_blocked(e.id, model))
            .count()
    }

    /// 报告指定凭据额度已用尽
    ///
    /// 用于处理 402 Payment Required 且 reason 为 `MONTHLY_REQUEST_COUNT` 的场景：
    /// - 立即禁用该凭据（不等待连续失败阈值）
    /// - 切换到下一个可用凭据继续重试
    /// - 返回是否还有可用凭据
    pub fn report_quota_exhausted(&self, id: u64) -> bool {
        let result = {
            let mut entries = self.entries.lock();
            let mut current_id = self.current_id.lock();

            let entry = match entries.iter_mut().find(|e| e.id == id) {
                Some(e) => e,
                None => return entries.iter().any(|e| !e.disabled),
            };

            if entry.disabled {
                return entries.iter().any(|e| !e.disabled);
            }

            entry.disabled = true;
            entry.disabled_reason = Some(DisabledReason::QuotaExceeded);
            entry.last_used_at = Some(Utc::now().to_rfc3339());
            // 设为阈值，便于在管理面板中直观看到该凭据已不可用
            entry.failure_count = MAX_FAILURES_PER_CREDENTIAL;

            tracing::error!("凭据 #{} 额度已用尽（MONTHLY_REQUEST_COUNT），已被禁用", id);

            // 切换到优先级最高的可用凭据
            if let Some(next) = entries
                .iter()
                .filter(|e| !e.disabled)
                .min_by_key(|e| e.credentials.priority)
            {
                *current_id = next.id;
                tracing::info!(
                    "已切换到凭据 #{}（优先级 {}）",
                    next.id,
                    next.credentials.priority
                );
                true
            } else {
                tracing::error!("所有凭据均已禁用！");
                false
            }
        };
        // 额度用尽已禁用该凭据，清除其会话亲和性绑定
        self.affinity.remove_by_credential(id);
        self.save_stats_debounced();
        result
    }

    /// 报告指定凭据被上游暂停/封禁。
    ///
    /// 与额度用尽类似立即禁用并切换，但原因标记为 `AccountSuspended`
    /// （不可自动恢复，等待人工处理），并设置长冷却。
    /// 返回是否还有可用凭据可继续重试。
    pub fn report_account_suspended(&self, id: u64) -> bool {
        let result = {
            let mut entries = self.entries.lock();
            let mut current_id = self.current_id.lock();

            let entry = match entries.iter_mut().find(|e| e.id == id) {
                Some(e) => e,
                None => return entries.iter().any(|e| !e.disabled),
            };

            if entry.disabled {
                return entries.iter().any(|e| !e.disabled);
            }

            entry.disabled = true;
            entry.disabled_reason = Some(DisabledReason::AccountSuspended);
            entry.last_used_at = Some(Utc::now().to_rfc3339());
            entry.failure_count = MAX_FAILURES_PER_CREDENTIAL;

            tracing::error!("凭据 #{} 被上游暂停/封禁，已禁用（等待人工处理）", id);

            if let Some(next) = entries
                .iter()
                .filter(|e| !e.disabled)
                .min_by_key(|e| e.credentials.priority)
            {
                *current_id = next.id;
                tracing::info!(
                    "已切换到凭据 #{}（优先级 {}）",
                    next.id,
                    next.credentials.priority
                );
                true
            } else {
                tracing::error!("所有凭据均已禁用！");
                false
            }
        };
        // 设置长冷却（不可自动恢复原因）
        if self.cooldown_enabled.load(Ordering::Relaxed) {
            self.cooldown
                .set_cooldown(id, CooldownReason::AccountSuspended);
        }
        // 封禁已禁用该凭据，清除其会话亲和性绑定
        self.affinity.remove_by_credential(id);
        self.save_stats_debounced();
        result
    }

    /// 报告指定凭据刷新 Token 失败。
    ///
    /// 连续刷新失败达到阈值后禁用凭据并切换，阈值内保持当前凭据不切换，
    /// 与 API 401/403 的累计失败策略保持一致。
    pub fn report_refresh_failure(&self, id: u64) -> bool {
        let result = {
            let mut entries = self.entries.lock();
            let mut current_id = self.current_id.lock();

            let entry = match entries.iter_mut().find(|e| e.id == id) {
                Some(e) => e,
                None => return entries.iter().any(|e| !e.disabled),
            };

            if entry.disabled {
                return entries.iter().any(|e| !e.disabled);
            }

            entry.last_used_at = Some(Utc::now().to_rfc3339());
            entry.refresh_failure_count += 1;
            let refresh_failure_count = entry.refresh_failure_count;

            tracing::warn!(
                "凭据 #{} Token 刷新失败（{}/{}）",
                id,
                refresh_failure_count,
                MAX_FAILURES_PER_CREDENTIAL
            );

            if refresh_failure_count < MAX_FAILURES_PER_CREDENTIAL {
                return entries.iter().any(|e| !e.disabled);
            }

            entry.disabled = true;
            entry.disabled_reason = Some(DisabledReason::TooManyRefreshFailures);

            tracing::error!(
                "凭据 #{} Token 已连续刷新失败 {} 次，已被禁用",
                id,
                refresh_failure_count
            );

            if let Some(next) = entries
                .iter()
                .filter(|e| !e.disabled)
                .min_by_key(|e| e.credentials.priority)
            {
                *current_id = next.id;
                tracing::info!(
                    "已切换到凭据 #{}（优先级 {}）",
                    next.id,
                    next.credentials.priority
                );
                true
            } else {
                tracing::error!("所有凭据均已禁用！");
                false
            }
        };
        self.save_stats_debounced();
        result
    }

    /// 报告指定凭据的 refreshToken 永久失效（invalid_grant）。
    ///
    /// 立即禁用凭据，不累计、不重试。
    /// 返回是否还有可用凭据。
    pub fn report_refresh_token_invalid(&self, id: u64) -> bool {
        let result = {
            let mut entries = self.entries.lock();
            let mut current_id = self.current_id.lock();

            let entry = match entries.iter_mut().find(|e| e.id == id) {
                Some(e) => e,
                None => return entries.iter().any(|e| !e.disabled),
            };

            if entry.disabled {
                return entries.iter().any(|e| !e.disabled);
            }

            entry.last_used_at = Some(Utc::now().to_rfc3339());
            entry.disabled = true;
            entry.disabled_reason = Some(DisabledReason::InvalidRefreshToken);

            tracing::error!(
                "凭据 #{} refreshToken 已失效 (invalid_grant)，已立即禁用",
                id
            );

            if let Some(next) = entries
                .iter()
                .filter(|e| !e.disabled)
                .min_by_key(|e| e.credentials.priority)
            {
                *current_id = next.id;
                tracing::info!(
                    "已切换到凭据 #{}（优先级 {}）",
                    next.id,
                    next.credentials.priority
                );
                true
            } else {
                tracing::error!("所有凭据均已禁用！");
                false
            }
        };
        self.save_stats_debounced();
        result
    }

    /// 切换到优先级最高的可用凭据
    ///
    /// 返回是否成功切换
    pub fn switch_to_next(&self) -> bool {
        let entries = self.entries.lock();
        let mut current_id = self.current_id.lock();

        // 选择优先级最高的未禁用凭据（排除当前凭据）
        if let Some(next) = entries
            .iter()
            .filter(|e| !e.disabled && e.id != *current_id)
            .min_by_key(|e| e.credentials.priority)
        {
            *current_id = next.id;
            tracing::info!(
                "已切换到凭据 #{}（优先级 {}）",
                next.id,
                next.credentials.priority
            );
            true
        } else {
            // 没有其他可用凭据，检查当前凭据是否可用
            entries.iter().any(|e| e.id == *current_id && !e.disabled)
        }
    }

    // ========================================================================
    // Admin API 方法
    // ========================================================================

    /// 获取管理器状态快照（用于 Admin API）
    pub fn snapshot(&self) -> ManagerSnapshot {
        let entries = self.entries.lock();
        let current_id = *self.current_id.lock();
        let available = entries.iter().filter(|e| !e.disabled).count();

        ManagerSnapshot {
            entries: entries
                .iter()
                .map(|e| CredentialEntrySnapshot {
                    id: e.id,
                    priority: e.credentials.priority,
                    rpm_limit: e.credentials.rpm_limit,
                    disabled: e.disabled,
                    failure_count: e.failure_count,
                    auth_method: if e.credentials.is_api_key_credential() {
                        Some("api_key".to_string())
                    } else {
                        e.credentials.auth_method.as_deref().map(|m| {
                            if m.eq_ignore_ascii_case("builder-id") || m.eq_ignore_ascii_case("iam") {
                                "idc".to_string()
                            } else {
                                m.to_string()
                            }
                        })
                    },
                    has_profile_arn: e.credentials.profile_arn.is_some(),
                    expires_at: if e.credentials.is_api_key_credential() {
                        None // API Key 凭据本地不维护过期时间（服务端策略未知）
                    } else {
                        e.credentials.expires_at.clone()
                    },
                    refresh_token_hash: if e.credentials.is_api_key_credential() {
                        None
                    } else {
                        e.credentials.refresh_token.as_deref().map(sha256_hex)
                    },
                    api_key_hash: if e.credentials.is_api_key_credential() {
                        e.credentials.kiro_api_key.as_deref().map(sha256_hex)
                    } else {
                        None
                    },
                    masked_api_key: if e.credentials.is_api_key_credential() {
                        e.credentials.kiro_api_key.as_deref().map(mask_api_key)
                    } else {
                        None
                    },
                    email: e.credentials.email.clone(),
                    name: e.credentials.name.clone(),
                    subscription_title: e.credentials.subscription_title.clone(),
                    success_count: e.success_count,
                    total_credits_used: e.total_credits_used,
                    last_used_at: e.last_used_at.clone(),
                    has_proxy: e.credentials.proxy_url.is_some(),
                    proxy_url: e.credentials.proxy_url.clone(),
                    refresh_failure_count: e.refresh_failure_count,
                    disabled_reason: e.disabled_reason.map(|r| match r {
                        DisabledReason::Manual => "Manual",
                        DisabledReason::TooManyFailures => "TooManyFailures",
                        DisabledReason::TooManyRefreshFailures => "TooManyRefreshFailures",
                        DisabledReason::QuotaExceeded => "QuotaExceeded",
                        DisabledReason::AccountSuspended => "AccountSuspended",
                        DisabledReason::SuspiciousActivityAuto => "SuspiciousActivityAuto",
                        DisabledReason::InvalidRefreshToken => "InvalidRefreshToken",
                        DisabledReason::InvalidConfig => "InvalidConfig",
                    }.to_string()),
                    endpoint: e.credentials.endpoint.clone(),
                    inflight: e.inflight.load(Ordering::Acquire),
                    rpm: self.rpm.count(e.id),
                })
                .collect(),
            current_id,
            total: entries.len(),
            available,
        }
    }

    /// 设置凭据禁用状态（Admin API）
    pub fn set_disabled(&self, id: u64, disabled: bool) -> anyhow::Result<()> {
        {
            let mut entries = self.entries.lock();
            let entry = entries
                .iter_mut()
                .find(|e| e.id == id)
                .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?;
            entry.disabled = disabled;
            if !disabled {
                // 启用时重置失败计数
                entry.failure_count = 0;
                entry.refresh_failure_count = 0;
                entry.disabled_reason = None;
            } else {
                entry.disabled_reason = Some(DisabledReason::Manual);
            }
        }
        // 禁用凭据时清除其会话亲和性绑定，避免后续请求重选时反复尝试已禁用凭据
        if disabled {
            self.affinity.remove_by_credential(id);
        }
        // 持久化更改
        self.persist_credentials()?;
        Ok(())
    }

    /// 设置凭据优先级（Admin API）
    ///
    /// 修改优先级后会立即按新优先级重新选择当前凭据。
    /// 即使持久化失败，内存中的优先级和当前凭据选择也会生效。
    pub fn set_priority(&self, id: u64, priority: u32) -> anyhow::Result<()> {
        {
            let mut entries = self.entries.lock();
            let entry = entries
                .iter_mut()
                .find(|e| e.id == id)
                .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?;
            entry.credentials.priority = priority;
        }
        // 立即按新优先级重新选择当前凭据（无论持久化是否成功）
        self.select_highest_priority();
        // 持久化更改
        self.persist_credentials()?;
        Ok(())
    }

    /// 设置凭据级 RPM 容量上限（0/None=继承全局）。即时生效于下次选号饱和判定。
    pub fn set_rpm_limit(&self, id: u64, rpm_limit: Option<u32>) -> anyhow::Result<()> {
        {
            let mut entries = self.entries.lock();
            let entry = entries
                .iter_mut()
                .find(|e| e.id == id)
                .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?;
            // 0 归一为 None(继承全局),避免存 Some(0) 语义歧义
            entry.credentials.rpm_limit = rpm_limit.filter(|&v| v > 0);
        }
        self.persist_credentials()?;
        Ok(())
    }

    /// 设置凭据自定义别名/备注（Admin API）。传空字符串清除别名。
    pub fn set_credential_name(&self, id: u64, name: Option<String>) -> anyhow::Result<()> {
        {
            let mut entries = self.entries.lock();
            let entry = entries
                .iter_mut()
                .find(|e| e.id == id)
                .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?;
            // 去空白;空则清除
            entry.credentials.name = name
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty());
        }
        self.persist_credentials()?;
        Ok(())
    }

    /// 设置单个凭据的代理（Admin API）。proxy_url 传空/None 清除(回退全局代理);
    /// "direct" 表示该号强制不走代理。username/password 为 None 时不改动、Some("")清除。
    ///
    /// 代理**立即生效、无需重启**：provider 每次 acquire 都按 `effective_proxy` 现取现建 client
    /// （见 provider.rs），改到 entry 上即下次请求生效。
    pub fn set_credential_proxy(
        &self,
        id: u64,
        proxy_url: Option<String>,
        proxy_username: Option<String>,
        proxy_password: Option<String>,
    ) -> anyhow::Result<()> {
        {
            let mut entries = self.entries.lock();
            let entry = entries
                .iter_mut()
                .find(|e| e.id == id)
                .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?;
            // URL 里可能内嵌账密（socks5://user:pass@host:port）——落库前拆出，
            // 存干净 URL + 独立账密字段：①避免密码明文留在 proxy_url（Debug 不脱敏会泄漏）
            // ②reqwest SOCKS5 需要独立账密才能认证。URL 内嵌账密仅在显式账密参数缺省时采用。
            let (clean_url, inline_user, inline_pass) = match proxy_url
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
            {
                Some(raw) => {
                    let (u, iu, ip) = crate::http_client::split_proxy_credentials(&raw);
                    (Some(u), iu, ip)
                }
                None => (None, None, None),
            };
            entry.credentials.proxy_url = clean_url;
            // 账密:显式参数 None=不改;Some(空)=清除;Some(非空)=更新;
            // 显式参数缺省(None)时,若 URL 内嵌了账密则采用内嵌值。
            match proxy_username {
                Some(u) => entry.credentials.proxy_username = Some(u).filter(|s| !s.is_empty()),
                None if inline_user.is_some() => entry.credentials.proxy_username = inline_user,
                None => {}
            }
            match proxy_password {
                Some(p) => entry.credentials.proxy_password = Some(p).filter(|s| !s.is_empty()),
                None if inline_pass.is_some() => entry.credentials.proxy_password = inline_pass,
                None => {}
            }
        }
        self.persist_credentials()?;
        Ok(())
    }

    /// 批量清空回收站中的指定凭据（无 ids 时清空全部）。返回成功清除数。
    pub fn purge_trash_batch(&self, ids: Option<Vec<u64>>) -> usize {
        let target_ids: Vec<u64> = match ids {
            Some(list) if !list.is_empty() => list,
            _ => self.list_trash().into_iter().map(|t| t.id).collect(),
        };
        let mut purged = 0;
        for id in target_ids {
            if self.purge_credential(id).is_ok() {
                purged += 1;
            }
        }
        purged
    }

    /// 重置凭据失败计数并重新启用（Admin API）
    pub fn reset_and_enable(&self, id: u64) -> anyhow::Result<()> {
        {
            let mut entries = self.entries.lock();
            let entry = entries
                .iter_mut()
                .find(|e| e.id == id)
                .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?;
            if entry.disabled_reason == Some(DisabledReason::InvalidConfig) {
                anyhow::bail!(
                    "凭据 #{} 因配置无效被禁用，请修正配置后重启服务",
                    id
                );
            }
            entry.failure_count = 0;
            entry.refresh_failure_count = 0;
            entry.disabled = false;
            entry.disabled_reason = None;
        }
        // 持久化更改
        self.persist_credentials()?;
        Ok(())
    }

    /// 获取指定凭据的使用额度（Admin API）
    pub async fn get_usage_limits_for(&self, id: u64) -> anyhow::Result<UsageLimitsResponse> {
        // 双检刷新收敛到 ensure_valid_token：返回的 credentials 已是刷新后的最新快照，
        // 无需再单独重读一次凭据。
        let (credentials, token) = self.ensure_valid_token(id).await?;

        let effective_proxy = credentials.effective_proxy(self.proxy.as_ref());
        let cfg = self.config.load_full();
        let usage_limits = get_usage_limits(&credentials, &cfg, &token, effective_proxy.as_ref()).await?;

        // 更新订阅等级到凭据（仅在发生变化时持久化）
        if let Some(subscription_title) = usage_limits.subscription_title() {
            let changed = {
                let mut entries = self.entries.lock();
                if let Some(entry) = entries.iter_mut().find(|e| e.id == id) {
                    let old_title = entry.credentials.subscription_title.clone();
                    if old_title.as_deref() != Some(subscription_title) {
                        entry.credentials.subscription_title =
                            Some(subscription_title.to_string());
                        tracing::info!(
                            "凭据 #{} 订阅等级已更新: {:?} -> {}",
                            id,
                            old_title,
                            subscription_title
                        );
                        true
                    } else {
                        false
                    }
                } else {
                    false
                }
            };

            if changed {
                if let Err(e) = self.persist_credentials() {
                    tracing::warn!("订阅等级更新后持久化失败（不影响本次请求）: {}", e);
                }
            }
        }

        Ok(usage_limits)
    }

    /// 获取指定凭据的 Web Portal 调用上下文（token / idp / profileArn / proxy）。
    ///
    /// 只读语义：不改动凭据的业务状态，但为保证 token 有效会在过期时触发一次刷新
    /// （与 `get_usage_limits_for` 一致的刷新流程），刷新成功会持久化新 token。
    ///
    /// 仅 social 凭据支持（idp 可推断为 Google）；API Key / IdC 凭据会直接报错。
    pub async fn web_portal_context_for(&self, id: u64) -> anyhow::Result<WebPortalContext> {
        // Web Portal 仅 social 凭据支持：API Key 必须在触发刷新前先拦下
        // （ensure_valid_token 对 API Key 会直接返回 kiroApiKey，不会 bail）。
        {
            let entries = self.entries.lock();
            let is_api_key = entries
                .iter()
                .find(|e| e.id == id)
                .map(|e| e.credentials.is_api_key_credential())
                .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?;
            if is_api_key {
                anyhow::bail!(
                    "API Key 凭据不支持 Web Portal 接口（overage 开关仅限 social 凭据）"
                );
            }
        }

        // 需要有效 token：过期或即将过期则先刷新（收敛到 ensure_valid_token 的双检守卫流程）
        let (final_creds, token) = self.ensure_valid_token(id).await?;

        let profile_arn = final_creds
            .profile_arn
            .clone()
            .filter(|s| !s.trim().is_empty());
        let idp = final_creds.effective_idp().to_string();
        if idp.is_empty() {
            anyhow::bail!("凭据不支持 Web Portal（仅 social 凭据可开关 overage）");
        }
        let proxy = final_creds.effective_proxy(self.proxy.as_ref());
        Ok(WebPortalContext {
            id,
            token,
            idp,
            profile_arn,
            proxy,
            tls_backend: self.config.load().tls_backend,
        })
    }

    /// 深度验活：发送最小 generateAssistantResponse 请求检测账号 suspend 状态
    ///
    /// getUsageLimits 不检查 suspend，只有真实对话请求才能检测。
    /// 发送一个会被服务端拒绝（空 conversationState）的请求，
    /// 只要返回 400（格式错误）而非 403（suspend）即表示凭据存活。
    pub async fn deep_verify_credential(&self, id: u64) -> anyhow::Result<()> {
        // 双检刷新收敛到 ensure_valid_token：credentials 为刷新后最新快照
        // （含可能动态解析到的 profileArn），供后续 region / machine_id / 请求头使用。
        let (credentials, token) = self.ensure_valid_token(id).await?;

        let cfg = self.config.load();
        // 与主对话路径(endpoint/ide.rs)对齐:上游已迁 runtime.{region}.kiro.dev,
        // 旧 q.{region}.amazonaws.com 已停用。此处深度验活曾漏迁,旧端点若停用会导致
        // 验活恒失败 → 把活号误判成死号禁用。region 用稳健版(profileArn 严格解析)。
        let region = credentials.effective_upstream_region(&cfg);
        let host = format!("runtime.{}.kiro.dev", region);
        let url = format!("https://{}/generateAssistantResponse", host);
        let machine_id = machine_id::generate_from_credentials(&credentials, &cfg);
        let kiro_version = &cfg.kiro_version;
        let os_name = &cfg.system_version;
        let node_version = &cfg.node_version;

        let user_agent = format!(
            "aws-sdk-js/1.0.34 ua/2.1 os/{} lang/js md/nodejs#{} api/codewhispererstreaming#1.0.34 m/E KiroIDE-{}-{}",
            os_name, node_version, kiro_version, machine_id
        );
        let x_amz_user_agent = format!("aws-sdk-js/1.0.34 KiroIDE-{}-{}", kiro_version, machine_id);

        // 构建最小请求体（故意不合法，只为触发 suspend 检查）
        let mut body = serde_json::json!({
            "conversationState": {
                "conversationId": uuid::Uuid::new_v4().to_string(),
                "currentMessage": {
                    "userInputMessage": {
                        "content": "hi"
                    }
                }
            }
        });
        // 用 effective_profile_arn（与对话路径 endpoint/ide.rs 统一口径）:idc/social 缺
        // arn 回退默认 BuilderId,external_idp 用动态解析到的真实 ARN。直接读
        // profile_arn.unwrap() 会在 idc 号回退默认 ARN(profile_arn 本身仍 None)时 panic。
        if let Some(arn) = credentials.effective_profile_arn() {
            body["profileArn"] = serde_json::Value::String(arn);
        }

        let effective_proxy = credentials.effective_proxy(self.proxy.as_ref());
        let client = build_client(effective_proxy.as_ref(), 30, self.config.load().tls_backend)?;

        let mut request = client
            .post(&url)
            .header("content-type", "application/json")
            .header("x-amzn-codewhisperer-optout", "true")
            .header("x-amz-user-agent", &x_amz_user_agent)
            .header("user-agent", &user_agent)
            .header("host", &host)
            .header("amz-sdk-invocation-id", uuid::Uuid::new_v4().to_string())
            .header("amz-sdk-request", "attempt=1; max=1")
            .header("Authorization", format!("Bearer {}", token));

        if credentials.is_api_key_credential() {
            request = request.header("tokentype", "API_KEY");
        } else if credentials.is_external_idp_credential() {
            request = request.header("tokentype", "EXTERNAL_IDP");
        }

        let response = request.body(body.to_string()).send().await?;
        let status = response.status();

        // 403 = suspended 或权限问题
        if status.as_u16() == 403 {
            let body_text = response.text().await.unwrap_or_default();
            if body_text.contains("suspended") {
                bail!("账号已被封禁 (suspended): {}", body_text);
            }
            bail!("权限被拒绝 (403): {}", body_text);
        }

        // 401 = token 无效
        if status.as_u16() == 401 {
            let body_text = response.text().await.unwrap_or_default();
            bail!("Token 无效 (401): {}", body_text);
        }

        // 400 = 请求体不完整（预期的，探测体故意不含 modelId，只为触发认证/suspend 检查），
        // 说明凭据/认证本身有效。200/其它 = 凭据有效。
        //
        // 注：本函数只做**认证/封禁**层面的验活，不判"订阅是否含某模型"——后者由
        // `probe_available_models` 逐模型带 modelId 探测（因为此处探测体无 modelId，上游
        // 不会返回 INVALID_MODEL_ID，在这里判它属死代码）。分工清晰、不做假承诺。
        Ok(())
    }

    /// 探测指定凭据**可用哪些模型**（Admin API，勾选后从独立页面手动触发）。
    ///
    /// 对一组候选模型逐个发无提示词的最小请求、消费响应流判定支持与否，并累加真实 credit 花费。
    /// Kiro 无原生"列模型"接口，靠"发请求看是否 INVALID_MODEL_ID"判定；⚠️**每个 supported 的
    /// 探测都是真实计费请求**（消真实积分）。仅 admin 手动触发、逐个间隔、绝不进请求热路径。
    ///
    /// 返回 `(每模型明细, 本次总花费 credits)`；明细每项 = (model_id, status, credits)，
    /// status ∈ supported/unsupported/unknown。仅认证/账号级问题(401/403/无token)整体返回 Err。
    pub async fn probe_models(
        &self,
        id: u64,
        models: &[String],
    ) -> anyhow::Result<(Vec<(String, String, f64)>, f64)> {
        let mut detail = Vec::with_capacity(models.len());
        let mut total_credits = 0.0f64;
        for m in models {
            // 认证级错误(401/403/无token) → ? 向上抛整轮中止；单模型 5xx/网络 → None=unknown。
            let (status, credits) = match self.probe_single_model(id, m).await? {
                Some((true, c)) => ("supported", c),
                Some((false, c)) => ("unsupported", c),
                None => ("unknown", 0.0),
            };
            total_credits += credits;
            detail.push((m.clone(), status.to_string(), credits));
            // 逐个之间留一点间隔，避免密集打同一号触发风控（与批量验活一致的谨慎）。
            tokio::time::sleep(StdDuration::from_millis(600)).await;
        }
        Ok((detail, total_credits))
    }

    /// 对单个模型发一个极小探测请求，返回该号是否支持它。
    ///
    /// `Ok(true)` = 支持（200 或非 INVALID_MODEL_ID 的 400）；`Ok(false)` = INVALID_MODEL_ID；
    /// `Err` = 认证/账号级问题（401/403/网络），调用方应整体中止并提示。
    /// 探测单个模型，返回 `(supported, credits_used)`：
    /// - `Ok(Some((true, c)))`  = 支持，本次真实消耗 c credits（消费流解析 meteringEvent）；
    /// - `Ok(Some((false, 0)))` = 不支持（INVALID_MODEL_ID，无论来自 400 还是流内 error）；
    /// - `Ok(None)`             = 未知（5xx/网络/其它非 2xx，无法判定，不计费）；
    /// - `Err`                  = 认证/账号级问题（401/403，整轮应中止）。
    ///
    /// ⚠️真实计费：supported 的探测会真正消费上游 event-stream（无提示词的最小请求），
    /// 产生真实内容与真实 credit 消耗——这是"能报出本次花费"与"判定准确"的必要代价。
    async fn probe_single_model(
        &self,
        id: u64,
        model_id: &str,
    ) -> anyhow::Result<Option<(bool, f64)>> {
        let (credentials, token) = self.ensure_valid_token(id).await?;
        let cfg = self.config.load();
        let region = credentials.effective_upstream_region(&cfg);
        let host = format!("runtime.{}.kiro.dev", region);
        let url = format!("https://{}/generateAssistantResponse", host);
        let machine_id = machine_id::generate_from_credentials(&credentials, &cfg);
        let user_agent = format!(
            "aws-sdk-js/1.0.34 ua/2.1 os/{} lang/js md/nodejs#{} api/codewhispererstreaming#1.0.34 m/E KiroIDE-{}-{}",
            cfg.system_version, cfg.node_version, cfg.kiro_version, machine_id
        );
        let x_amz_user_agent =
            format!("aws-sdk-js/1.0.34 KiroIDE-{}-{}", cfg.kiro_version, machine_id);

        // 构造**与真实对话同构**的合法请求体（关键修复）：此前手搓的最小体缺 chatTriggerType/
        // origin 等必填字段，上游一律回通用 400（与"模型没权限"无关），导致探测非全绿即全红、
        // 且拿不到 credits。改为复用 converter::convert_request 生成完整 ConversationState，
        // 再把 modelId 覆盖成探测目标（探测直发原生 id，不经 map_model），这样上游才会真正走到
        // "该号能否用此模型"的判定：有权限→200+meteringEvent 计费流，无权限→INVALID_MODEL_ID。
        use crate::anthropic::converter::convert_request;
        use crate::anthropic::types::MessagesRequest;
        let probe_req = MessagesRequest {
            model: "claude-sonnet-4.5".to_string(), // 仅用于过 convert_request 合法性；下面覆盖 modelId
            max_tokens: 16,
            messages: vec![crate::anthropic::types::Message {
                role: "user".to_string(),
                content: serde_json::json!("hi"),
            }],
            stream: false,
            system: None,
            tools: None,
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: None,
        };
        let mut conv = convert_request(&probe_req)
            .map_err(|e| anyhow::anyhow!("构造探测请求失败: {:?}", e))?
            .conversation_state;
        // 覆盖为探测目标模型 id（原生 Kiro modelId，如 qwen3-coder-next / claude-opus-4.8）
        conv.current_message.user_input_message.model_id = model_id.to_string();
        let mut kiro_req = serde_json::to_value(&crate::kiro::model::requests::kiro::KiroRequest {
            conversation_state: conv,
            profile_arn: None,
        })?;
        // profileArn 注入：与对话路径统一口径（idc/social 回退默认，external_idp 用真实 arn）
        if let Some(arn) = credentials.effective_profile_arn() {
            kiro_req["profileArn"] = serde_json::Value::String(arn);
        }
        let body = kiro_req;

        let effective_proxy = credentials.effective_proxy(self.proxy.as_ref());
        // 探测要消费完整生成流,用 read_timeout(空闲间隔)而非总超时,否则慢模型生成中途被 30s
        // 总超时掐断→误判 unknown/失败(与 mid-response 同类)。空闲上限 60s:探测请求 content="hi"
        // 生成极短,只要上游在吐数据就不该超时;真卡死 60s 无数据才放弃,比对话路径更快止损。
        let client = build_streaming_client(effective_proxy.as_ref(), 60, cfg.tls_backend)?;
        let mut request = client
            .post(&url)
            .header("content-type", "application/json")
            .header("x-amzn-codewhisperer-optout", "true")
            .header("x-amz-user-agent", &x_amz_user_agent)
            .header("user-agent", &user_agent)
            .header("host", &host)
            .header("amz-sdk-invocation-id", uuid::Uuid::new_v4().to_string())
            .header("amz-sdk-request", "attempt=1; max=1")
            .header("Authorization", format!("Bearer {}", token));
        if credentials.is_api_key_credential() {
            request = request.header("tokentype", "API_KEY");
        } else if credentials.is_external_idp_credential() {
            request = request.header("tokentype", "EXTERNAL_IDP");
        }

        // 单个模型探测的网络错误不应中止整轮：吞掉转成 None(unknown) 继续探下一个。
        let response = match request.body(body.to_string()).send().await {
            Ok(r) => r,
            Err(e) => {
                tracing::debug!("探测模型 {} 网络错误(记为 unknown): {}", model_id, e);
                return Ok(None);
            }
        };
        let status = response.status();
        if matches!(status.as_u16(), 401 | 403) {
            // 认证/账号级问题：整轮探测都会失败，向上抛错让 probe_available_models 整体中止并提示。
            let body_text = response.text().await.unwrap_or_default();
            bail!("认证/账号问题（{}）：{}", status.as_u16(), body_text);
        }
        if status.as_u16() == 400 {
            let body_text = response.text().await.unwrap_or_default();
            // INVALID_MODEL_ID = 不支持；其它 400 也归"不支持/不可用"（探测请求本身合法，
            // 400 只可能是模型侧问题）——比旧逻辑"其它400=支持"更保守，杜绝假阳性。
            let _ = body_text;
            return Ok(Some((false, 0.0)));
        }
        // 5xx / 其它非 2xx：上游侧问题，无法判定 → None(unknown)，不计费。
        if !status.is_success() {
            return Ok(None);
        }

        // 2xx：真正消费 event-stream。流内可能仍出现 error/exception(INVALID_MODEL_ID 等)→ 不支持；
        // 正常则累加 meteringEvent 的真实 credit。这修正了旧逻辑"只看 200 就判 supported"的假阳性。
        use crate::kiro::model::events::Event;
        use crate::kiro::parser::decoder::EventStreamDecoder;
        use futures::StreamExt;
        let mut decoder = EventStreamDecoder::new();
        let mut stream = response.bytes_stream();
        let mut credits = 0.0f64;
        let mut invalid = false;
        while let Some(chunk) = stream.next().await {
            let chunk = match chunk {
                Ok(c) => c,
                Err(_) => break, // 传输中断：已收到的按现状判定
            };
            if decoder.feed(&chunk).is_err() {
                break;
            }
            let mut stop = false;
            for frame in decoder.decode_iter() {
                let frame = match frame {
                    Ok(f) => f,
                    Err(_) => continue,
                };
                if let Ok(ev) = Event::from_frame(frame) {
                    match ev {
                        Event::Metering(m) => credits += m.usage,
                        Event::Error { error_code, error_message } => {
                            if crate::kiro::endpoint::default_is_invalid_model_id(&error_code)
                                || crate::kiro::endpoint::default_is_invalid_model_id(&error_message)
                            {
                                invalid = true;
                            }
                            stop = true;
                        }
                        Event::Exception { exception_type, message } => {
                            if crate::kiro::endpoint::default_is_invalid_model_id(&exception_type)
                                || crate::kiro::endpoint::default_is_invalid_model_id(&message)
                            {
                                invalid = true;
                            }
                            stop = true;
                        }
                        _ => {}
                    }
                }
            }
            if stop {
                break;
            }
        }
        if invalid {
            return Ok(Some((false, credits)));
        }
        Ok(Some((true, credits)))
    }

    /// 添加新凭据（Admin API）
    ///
    /// # 流程
    /// 1. 验证凭据基本字段（API Key: kiroApiKey 不为空; OAuth: refreshToken 不为空）
    /// 2. 基于 kiroApiKey 或 refreshToken 的 SHA-256 哈希检测重复
    /// 3. OAuth: 尝试刷新 Token 验证凭据有效性; API Key: 跳过
    /// 4. 分配新 ID（当前最大 ID + 1）
    /// 5. 添加到 entries 列表
    /// 6. 持久化到配置文件
    ///
    /// # 返回
    /// - `Ok(u64)` - 新凭据 ID
    /// - `Err(_)` - 验证失败或添加失败
    pub async fn add_credential(&self, new_cred: KiroCredentials) -> anyhow::Result<u64> {
        // 1. 基本验证
        if new_cred.is_api_key_credential() {
            let api_key = new_cred
                .kiro_api_key
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("API Key 凭据缺少 kiroApiKey"))?;
            if api_key.is_empty() {
                anyhow::bail!("kiroApiKey 为空");
            }
        } else {
            validate_refresh_token(&new_cred)?;
        }

        // 2. 基于哈希检测重复
        if new_cred.is_api_key_credential() {
            let new_api_key = new_cred
                .kiro_api_key
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("缺少 kiroApiKey"))?;
            let new_api_key_hash = sha256_hex(new_api_key);
            let duplicate_exists = {
                let entries = self.entries.lock();
                entries.iter().any(|entry| {
                    entry
                        .credentials
                        .kiro_api_key
                        .as_deref()
                        .map(sha256_hex)
                        .as_deref()
                        == Some(new_api_key_hash.as_str())
                })
            };
            if duplicate_exists {
                anyhow::bail!("凭据已存在（kiroApiKey 重复）");
            }
        } else {
            let new_refresh_token = new_cred
                .refresh_token
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("缺少 refreshToken"))?;
            let new_refresh_token_hash = sha256_hex(new_refresh_token);
            let duplicate_exists = {
                let entries = self.entries.lock();
                entries.iter().any(|entry| {
                    entry
                        .credentials
                        .refresh_token
                        .as_deref()
                        .map(sha256_hex)
                        .as_deref()
                        == Some(new_refresh_token_hash.as_str())
                })
            };
            if duplicate_exists {
                anyhow::bail!("凭据已存在（refreshToken 重复）");
            }
        }

        // 3. 验证凭据有效性（API Key 无需网络刷新）
        let mut validated_cred = if new_cred.is_api_key_credential() {
            new_cred.clone()
        } else {
            let effective_proxy = new_cred.effective_proxy(self.proxy.as_ref());
            let cfg = self.config.load_full();
            refresh_token(&new_cred, &cfg, effective_proxy.as_ref()).await?
        };

        // 4. 分配新 ID
        //    【红线】必须扫描 entries ∪ trash 的 id 并集取 max+1，
        //    否则回收站里的 id 会与新加凭据撞号，恢复时冲突。
        let new_id = {
            let entries = self.entries.lock();
            let trash = self.trash.lock();
            let max_entry = entries.iter().map(|e| e.id).max().unwrap_or(0);
            let max_trash = trash
                .iter()
                .filter_map(|t| t.credentials.id)
                .max()
                .unwrap_or(0);
            max_entry.max(max_trash) + 1
        };

        // 5. 设置 ID 并保留用户输入的元数据
        validated_cred.id = Some(new_id);
        validated_cred.priority = new_cred.priority;
        validated_cred.auth_method = new_cred.auth_method.map(|m| {
            if m.eq_ignore_ascii_case("builder-id") || m.eq_ignore_ascii_case("iam") {
                "idc".to_string()
            } else {
                m
            }
        });
        validated_cred.client_id = new_cred.client_id;
        validated_cred.client_secret = new_cred.client_secret;
        validated_cred.region = new_cred.region;
        validated_cred.auth_region = new_cred.auth_region;
        validated_cred.api_region = new_cred.api_region;
        validated_cred.machine_id = new_cred.machine_id;
        validated_cred.email = new_cred.email;
        validated_cred.proxy_url = new_cred.proxy_url;
        validated_cred.proxy_username = new_cred.proxy_username;
        validated_cred.proxy_password = new_cred.proxy_password;
        validated_cred.kiro_api_key = new_cred.kiro_api_key;

        // 冻结 machineId(防关联):上号入池时 machine_id 通常为 None,若不冻结,请求路径每次都
        // 用 generate_from_credentials 现算——而它对 OAuth 号是**按 refreshToken 派生**的,
        // social/idc/external_idp 每次刷新都会轮换 refreshToken,派生出的 machineId 就随之漂移,
        // 上游看到「同一个号设备指纹一直在变」反而是可疑信号(且要等下次重启 reconcile 才会固化)。
        // 这里入池即固化一个稳定指纹,与启动 reconcile 的行为一致。
        if validated_cred.machine_id.is_none() {
            let cfg = self.config.load_full();
            validated_cred.machine_id =
                Some(machine_id::generate_from_credentials(&validated_cred, &cfg));
        }

        {
            let mut entries = self.entries.lock();
            // 指纹去重(防关联):新号指纹若与池中已有号撞车,轮换成独立随机指纹,避免上游
            // 按设备指纹把两个号关联封禁。与 reconcile 的 machine_id 碰撞轮换逻辑一致。
            if let Some(mid) = validated_cred.machine_id.clone() {
                let collides = entries
                    .iter()
                    .any(|e| e.credentials.machine_id.as_deref() == Some(mid.as_str()));
                if collides {
                    let existing: std::collections::HashSet<String> = entries
                        .iter()
                        .filter_map(|e| e.credentials.machine_id.clone())
                        .collect();
                    let mut fresh = machine_id::random_machine_id();
                    while existing.contains(&fresh) {
                        fresh = machine_id::random_machine_id();
                    }
                    tracing::warn!(
                        "新增凭据 #{} machineId 与池中已有号重复,已自动轮换为独立指纹(防关联)",
                        new_id
                    );
                    validated_cred.machine_id = Some(fresh);
                }
            }
            entries.push(CredentialEntry {
                id: new_id,
                credentials: validated_cred,
                failure_count: 0,
                refresh_failure_count: 0,
                disabled: false,
                disabled_reason: None,
                success_count: 0,
                total_credits_used: 0.0,
                last_used_at: None,
                inflight: Arc::new(AtomicU32::new(0)),
            });
        }

        // 6. 持久化
        self.persist_credentials()?;

        tracing::info!("成功添加凭据 #{}", new_id);
        Ok(new_id)
    }

    /// 删除凭据（Admin API）——软删除，移入回收站
    ///
    /// # 前置条件
    /// - 凭据必须已禁用（disabled = true）
    ///
    /// # 行为
    /// 1. 验证凭据存在且已禁用
    /// 2. 从 entries 物理移出（让其从调度池彻底消失）
    /// 3. 包成 TrashEntry 推入回收站
    /// 4. 如果删除的是当前凭据，切换到优先级最高的可用凭据；删空则 current_id 重置为 0
    /// 5. 先 persist_trash() 成功，再 persist_credentials()（双文件一致性，避免真丢号）
    /// 6. 回写统计数据
    ///
    /// # 返回
    /// - `Ok(())` - 删除成功
    /// - `Err(_)` - 凭据不存在、未禁用或持久化失败
    pub fn delete_credential(&self, id: u64) -> anyhow::Result<()> {
        let was_current = {
            let mut entries = self.entries.lock();

            // 查找凭据位置
            let idx = entries
                .iter()
                .position(|e| e.id == id)
                .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?;

            // 检查是否已禁用
            if !entries[idx].disabled {
                anyhow::bail!("只能删除已禁用的凭据（请先禁用凭据 #{}）", id);
            }

            // 记录是否是当前凭据
            let current_id = *self.current_id.lock();
            let was_current = current_id == id;

            // 物理移出 entries，包成 TrashEntry 推入回收站
            let removed = entries.remove(idx);
            let mut cred = removed.credentials;
            cred.id = Some(removed.id); // 确保 id 落在凭据内，便于恢复
            self.trash.lock().push(TrashEntry {
                credentials: cred,
                deleted_at: Utc::now().to_rfc3339(),
                success_count: removed.success_count,
                total_credits_used: removed.total_credits_used,
                last_used_at: removed.last_used_at,
            });

            was_current
        };

        // 清除被删凭据的会话亲和性绑定，避免后续重选时命中已移出的凭据
        self.affinity.remove_by_credential(id);

        // 如果删除的是当前凭据，切换到优先级最高的可用凭据
        if was_current {
            self.select_highest_priority();
        }

        // 如果删除后没有任何凭据，将 current_id 重置为 0（与初始化行为保持一致）
        {
            let entries = self.entries.lock();
            if entries.is_empty() {
                let mut current_id = self.current_id.lock();
                *current_id = 0;
                tracing::info!("所有凭据已删除，current_id 已重置为 0");
            }
        }

        // 双文件一致性：先落盘回收站，成功后再回写凭据池。
        // 若回收站落盘失败则立刻回滚（把凭据放回 entries），避免真丢号。
        if let Err(e) = self.persist_trash() {
            let restored = {
                let mut trash = self.trash.lock();
                trash.pop().map(|t| t.credentials)
            };
            if let Some(cred) = restored {
                let mut entries = self.entries.lock();
                entries.push(CredentialEntry {
                    id,
                    credentials: cred,
                    failure_count: 0,
                    refresh_failure_count: 0,
                    disabled: true,
                    disabled_reason: Some(DisabledReason::Manual),
                    success_count: 0,
                    total_credits_used: 0.0,
                    last_used_at: None,
                    inflight: Arc::new(AtomicU32::new(0)),
                });
            }
            return Err(e.context("回收站落盘失败，已回滚删除操作"));
        }

        // 持久化凭据池（移除后的结果）
        self.persist_credentials()?;

        // 立即回写统计数据，清除已删除凭据的残留条目
        self.save_stats();

        tracing::info!("已将凭据 #{} 移入回收站", id);
        Ok(())
    }

    /// 列出回收站中的所有已删除凭据（Admin API）
    pub fn list_trash(&self) -> Vec<TrashSnapshot> {
        self.trash
            .lock()
            .iter()
            .map(|t| {
                let c = &t.credentials;
                let is_api_key = c.is_api_key_credential();
                TrashSnapshot {
                    id: c.id.unwrap_or(0),
                    priority: c.priority,
                    auth_method: if is_api_key {
                        Some("api_key".to_string())
                    } else {
                        c.auth_method.as_deref().map(|m| {
                            if m.eq_ignore_ascii_case("builder-id") || m.eq_ignore_ascii_case("iam")
                            {
                                "idc".to_string()
                            } else {
                                m.to_string()
                            }
                        })
                    },
                    email: c.email.clone(),
                    masked_api_key: if is_api_key {
                        c.kiro_api_key.as_deref().map(mask_api_key)
                    } else {
                        None
                    },
                    refresh_token_hash: if is_api_key {
                        None
                    } else {
                        c.refresh_token.as_deref().map(sha256_hex)
                    },
                    api_key_hash: if is_api_key {
                        c.kiro_api_key.as_deref().map(sha256_hex)
                    } else {
                        None
                    },
                    endpoint: c.endpoint.clone(),
                    deleted_at: t.deleted_at.clone(),
                    success_count: t.success_count,
                    last_used_at: t.last_used_at.clone(),
                }
            })
            .collect()
    }

    /// 从回收站恢复凭据（Admin API）
    ///
    /// 【红线】恢复前做 refreshToken/kiroApiKey 哈希去重校验，若 entries 里
    /// 已存在同 refreshToken/apiKey 的凭据则拒绝恢复。恢复后凭据回到 entries，
    /// id 保持不变，并还原删除前的统计数据。
    pub fn restore_credential(&self, id: u64) -> anyhow::Result<()> {
        // 去重校验 + 移出回收站 + 放回凭据池，全程在同时持有两锁的临界区内完成。
        // 【锁序红线】统一为 entries → trash（与 delete_credential/add_credential 一致），
        // 避免与它们构成 ABBA 死锁。整段临界区内不做任何 .await / IO。
        {
            let mut entries = self.entries.lock();
            let mut trash = self.trash.lock();

            let idx = trash
                .iter()
                .position(|t| t.credentials.id == Some(id))
                .ok_or_else(|| anyhow::anyhow!("回收站中不存在凭据: {}", id))?;

            // 去重校验：与现有 entries 比对 refreshToken / kiroApiKey 哈希
            let cred = &trash[idx].credentials;
            if cred.is_api_key_credential() {
                if let Some(new_hash) = cred.kiro_api_key.as_deref().map(sha256_hex) {
                    let dup = entries.iter().any(|e| {
                        e.credentials.kiro_api_key.as_deref().map(sha256_hex).as_deref()
                            == Some(new_hash.as_str())
                    });
                    if dup {
                        anyhow::bail!("凭据已存在（kiroApiKey 重复），无法恢复");
                    }
                }
            } else if let Some(new_hash) = cred.refresh_token.as_deref().map(sha256_hex) {
                let dup = entries.iter().any(|e| {
                    e.credentials.refresh_token.as_deref().map(sha256_hex).as_deref()
                        == Some(new_hash.as_str())
                });
                if dup {
                    anyhow::bail!("凭据已存在（refreshToken 重复），无法恢复");
                }
            }

            // 校验通过：正式移出回收站，放回凭据池
            // id 不变，恢复为已禁用状态（避免刚恢复即被调度，交由 Admin 手动启用）
            let restored_entry = trash.remove(idx);
            let mut cred = restored_entry.credentials;
            cred.id = Some(id);
            cred.disabled = true;
            entries.push(CredentialEntry {
                id,
                credentials: cred,
                failure_count: 0,
                refresh_failure_count: 0,
                disabled: true,
                disabled_reason: Some(DisabledReason::Manual),
                success_count: restored_entry.success_count,
                total_credits_used: restored_entry.total_credits_used,
                last_used_at: restored_entry.last_used_at,
                inflight: Arc::new(AtomicU32::new(0)),
            });
        }

        // 双文件一致性：先落盘凭据池，再落盘回收站
        self.persist_credentials()?;
        if let Err(e) = self.persist_trash() {
            tracing::warn!("恢复凭据 #{} 后回写回收站失败: {}", id, e);
        }
        self.save_stats();

        tracing::info!("已从回收站恢复凭据 #{}（恢复为禁用态）", id);
        Ok(())
    }

    /// 从回收站彻底删除凭据（Admin API，不可恢复）
    pub fn purge_credential(&self, id: u64) -> anyhow::Result<()> {
        {
            let mut trash = self.trash.lock();
            let idx = trash
                .iter()
                .position(|t| t.credentials.id == Some(id))
                .ok_or_else(|| anyhow::anyhow!("回收站中不存在凭据: {}", id))?;
            trash.remove(idx);
        }
        self.persist_trash()?;
        tracing::info!("已从回收站彻底删除凭据 #{}", id);
        Ok(())
    }

    /// 清理回收站中超过保留期的条目（由后台定时任务周期调用）
    ///
    /// `retention_days == 0` 表示永久保留，直接返回 0。
    /// 返回被清理的条目数量。
    pub fn purge_expired_trash(&self, retention_days: u32) -> usize {
        if retention_days == 0 {
            return 0; // 永久保留
        }
        let cutoff = Utc::now() - Duration::days(retention_days as i64);
        let removed = {
            let mut trash = self.trash.lock();
            let before = trash.len();
            trash.retain(|t| {
                // 无法解析删除时间的条目保守保留（不误删）
                match DateTime::parse_from_rfc3339(&t.deleted_at) {
                    Ok(dt) => dt.with_timezone(&Utc) > cutoff,
                    Err(_) => true,
                }
            });
            before - trash.len()
        };
        if removed > 0 {
            if let Err(e) = self.persist_trash() {
                tracing::warn!("清理过期回收站后回写失败: {}", e);
            }
            tracing::info!("回收站保留清理：彻底删除 {} 条过期凭据", removed);
        }
        removed
    }

    /// 清理会话亲和性 map 中超过 TTL 的空闲条目（由 main 的后台定时任务周期调用）。
    ///
    /// affinity map 的 key 是客户端可控的 session id，仅靠 get() 惰性删除无法回收
    /// 「不再出现的 session」，长跑会内存泄漏。未启用亲和性时 map 恒空，调用无害。
    pub fn cleanup_affinity(&self) {
        self.affinity.cleanup();
    }

    /// 清理 RPM 滚动窗口中不再活跃的凭据 id 条目（由后台定时任务周期调用）。
    ///
    /// RPM map 的 key 是凭据 id，惰性剔除只发生在被再次选中时；长期不再被选中的
    /// 号（如已删除）其空 Vec 条目需主动回收，避免无界堆积。未配置 RPM 上限时
    /// map 仍会因每次选号 record 而增长，故无条件清理。
    pub fn cleanup_scheduling(&self) {
        self.rpm.cleanup();
        self.health.cleanup();
    }

    /// 强制刷新指定凭据的 Token（Admin API）
    ///
    /// 无条件调用上游 API 重新获取 access token，不检查是否过期。
    /// 适用于排查问题、Token 异常但未过期、主动更新凭据状态等场景。
    /// 列出需要「主动预刷新」的凭据 id（批次4.4）。
    ///
    /// 判据：未禁用 + 非 API Key（API Key 无需刷新）+ 有 refresh_token +
    /// token 将在 `lead_minutes` 分钟内过期。返回的 id 交由后台 loop 逐个刷新。
    pub fn credentials_due_for_refresh(&self, lead_minutes: i64) -> Vec<u64> {
        let entries = self.entries.lock();
        entries
            .iter()
            .filter(|e| !e.disabled)
            .filter(|e| !e.credentials.is_api_key_credential())
            .filter(|e| e.credentials.refresh_token.is_some())
            .filter(|e| is_token_expiring_within(&e.credentials, lead_minutes).unwrap_or(false))
            .map(|e| e.id)
            .collect()
    }

    /// 强制刷新指定凭据的 Token（admin 手动强刷）。
    ///
    /// 无条件刷新；错误直接返回给调用方（admin 侧）展示，不在此累计失败/禁用。
    pub async fn force_refresh_token_for(&self, id: u64) -> anyhow::Result<()> {
        self.refresh_token_locked(id, None).await.map(|_| ())
    }

    /// 后台主动预刷新指定凭据（批次4.4）。
    ///
    /// 与 [`force_refresh_token_for`] 的区别有二：
    /// 1. **条件刷新**：拿到 refresh_lock 后二次确认 token 仍将在 `lead_minutes`
    ///    内过期才刷新——请求路径的按需刷新可能在我们等锁期间已刷好，此时跳过，
    ///    避免重刷刚刷好的 token（多打一次上游 refresh、与「削峰」目标相悖）。
    /// 2. **失败处置**：刷新失败按错误类型累计失败计数 / 禁用坏凭据，与请求路径
    ///    [`try_ensure_token`] 的失败处置一致，坏号不必等真实请求命中才被处置。
    pub async fn prefetch_refresh_token_for(&self, id: u64, lead_minutes: i64) {
        match self.refresh_token_locked(id, Some(lead_minutes)).await {
            Ok(RefreshOutcome::Refreshed) => tracing::info!("预刷新凭据 #{} 成功", id),
            Ok(RefreshOutcome::Skipped) => {
                tracing::debug!("预刷新凭据 #{} 跳过（已被请求路径刷新）", id)
            }
            Err(e) => {
                if e.downcast_ref::<RefreshTokenInvalidError>().is_some() {
                    tracing::warn!("预刷新凭据 #{} refreshToken 永久失效，禁用: {}", id, e);
                    self.report_refresh_token_invalid(id);
                } else {
                    tracing::warn!("预刷新凭据 #{} 失败（交由请求路径重试）: {}", id, e);
                    self.report_refresh_failure(id);
                }
            }
        }
    }

    /// 持锁刷新的共享实现。`conditional_lead` 为 `Some(min)` 时，拿锁后二次确认
    /// token 仍将在 `min` 分钟内过期才刷新，否则返回 [`RefreshOutcome::Skipped`]；
    /// 为 `None` 时无条件刷新（admin 强刷）。
    async fn refresh_token_locked(
        &self,
        id: u64,
        conditional_lead: Option<i64>,
    ) -> anyhow::Result<RefreshOutcome> {
        // 快速存在性检查（无锁）
        {
            let entries = self.entries.lock();
            if !entries.iter().any(|e| e.id == id) {
                anyhow::bail!("凭据不存在: {}", id);
            }
        }

        // 获取刷新锁防止并发刷新
        let _guard = self.refresh_lock.lock().await;

        // 拿锁后读取当前凭据：请求路径或其它预刷新可能在等锁期间已刷新
        let credentials = {
            let entries = self.entries.lock();
            entries
                .iter()
                .find(|e| e.id == id)
                .map(|e| e.credentials.clone())
                .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?
        };

        // 陈旧刷新守卫：快照发起刷新时的 refresh_token。刷新是跨 .await 的网络调用,
        // 期间请求路径的 try_ensure_token 等可能已用同一 refresh_token 换到新 token 并写回。
        // 若写回时发现 entry 的 refresh_token 已不等于本次快照,说明别的路径抢先刷新成功,
        // 本次结果已陈旧 → 丢弃写回(否则会把已轮换的新 token 覆盖回旧的,导致下次刷新用废弃
        // 的 refresh_token 而失败)。参考 kiro-account-manager tasks/token_refresh.rs 的守卫。
        let refresh_token_snapshot = credentials.refresh_token.clone();

        // 条件刷新（后台预刷新）：token 已不再将过期 → 跳过，避免重复刷新
        if let Some(lead) = conditional_lead {
            if !is_token_expiring_within(&credentials, lead).unwrap_or(false) {
                return Ok(RefreshOutcome::Skipped);
            }
        }

        let effective_proxy = credentials.effective_proxy(self.proxy.as_ref());
        let cfg = self.config.load_full();
        let new_creds =
            refresh_token(&credentials, &cfg, effective_proxy.as_ref()).await?;

        // 更新 entries 中对应凭据（写回前校验 refresh_token 未被其它路径抢先轮换）
        {
            let mut entries = self.entries.lock();
            if let Some(entry) = entries.iter_mut().find(|e| e.id == id) {
                if entry.credentials.refresh_token != refresh_token_snapshot {
                    // 别的路径已刷新成功,本次结果陈旧,不覆盖(避免用旧 refresh_token 覆盖新的)
                    tracing::debug!(
                        "凭据 #{} 刷新结果已陈旧(refresh_token 期间被其它路径轮换),丢弃本次写回",
                        id
                    );
                    return Ok(RefreshOutcome::Skipped);
                }
                entry.credentials = new_creds;
                entry.refresh_failure_count = 0;
            }
        }

        // 动态解析 profileArn:idc/Enterprise 号常无 profileArn(oidc 刷新不回传),而对话/余额
        // 端点要求真实 profileArn(占位 ARN 对 Enterprise 号会被判 Invalid token/403)。刷新成功后
        // 若该号仍缺 profileArn 且非 external_idp(它不带),运行时调 management ListAvailableProfiles
        // 拿真实 arn 写回,一次解析后持久化缓存,后续对话/余额直接用真实值。失败仅告警不阻断。
        let (needs_arn, arn_creds, arn_token) = {
            let entries = self.entries.lock();
            match entries.iter().find(|e| e.id == id) {
                Some(e) => {
                    let c = &e.credentials;
                    let missing = c
                        .profile_arn
                        .as_deref()
                        .map(|s| s.trim().is_empty())
                        .unwrap_or(true);
                    // external_idp 也纳入动态解析:上游迁 kiro.dev 后 external_idp 号
                    // 必须带自己租户的真实 profileArn（缺了 400 profileArn is required），
                    // 而 resolve_profile_arn_via_management 本就为它设了 TokenType:EXTERNAL_IDP。
                    // 仅 api_key 号无 profile 概念,排除。
                    let eligible = missing && !c.is_api_key_credential();
                    (
                        eligible,
                        c.clone(),
                        c.access_token.clone().unwrap_or_default(),
                    )
                }
                None => (false, KiroCredentials::default(), String::new()),
            }
        };
        if needs_arn && !arn_token.is_empty() {
            let cfg2 = self.config.load_full();
            let proxy2 = arn_creds.effective_proxy(self.proxy.as_ref());
            match resolve_profile_arn_via_management(&arn_creds, &cfg2, &arn_token, proxy2.as_ref())
                .await
            {
                Ok(Some(arn)) => {
                    let mut entries = self.entries.lock();
                    if let Some(entry) = entries.iter_mut().find(|e| e.id == id) {
                        entry.credentials.profile_arn = Some(arn.clone());
                    }
                    tracing::info!("凭据 #{} 动态解析到 profileArn（ListAvailableProfiles）", id);
                }
                Ok(None) => tracing::warn!("凭据 #{} ListAvailableProfiles 无可用 profile", id),
                Err(e) => tracing::warn!("凭据 #{} 动态解析 profileArn 失败（不阻断）: {}", id, e),
            }
        }

        // 持久化
        if let Err(e) = self.persist_credentials() {
            tracing::warn!("刷新 Token 后持久化失败: {}", e);
        }

        tracing::info!("凭据 #{} Token 已刷新", id);
        Ok(RefreshOutcome::Refreshed)
    }

    /// 获取负载均衡模式（Admin API）
    pub fn get_load_balancing_mode(&self) -> String {
        self.load_balancing_mode.lock().clone()
    }

    fn persist_load_balancing_mode(&self, mode: &str) -> anyhow::Result<()> {
        use anyhow::Context;

        let config_path = match self.config.load().config_path() {
            Some(path) => path.to_path_buf(),
            None => {
                tracing::warn!("配置文件路径未知，负载均衡模式仅在当前进程生效: {}", mode);
                return Ok(());
            }
        };

        let mut config = Config::load(&config_path)
            .with_context(|| format!("重新加载配置失败: {}", config_path.display()))?;
        config.load_balancing_mode = mode.to_string();
        config
            .save()
            .with_context(|| format!("持久化负载均衡模式失败: {}", config_path.display()))?;

        Ok(())
    }

    /// 设置负载均衡模式（Admin API）
    pub fn set_load_balancing_mode(&self, mode: String) -> anyhow::Result<()> {
        // 验证模式值
        if mode != "priority" && mode != "balanced" {
            anyhow::bail!("无效的负载均衡模式: {}", mode);
        }

        let previous_mode = self.get_load_balancing_mode();
        if previous_mode == mode {
            return Ok(());
        }

        *self.load_balancing_mode.lock() = mode.clone();

        if let Err(err) = self.persist_load_balancing_mode(&mode) {
            *self.load_balancing_mode.lock() = previous_mode;
            return Err(err);
        }

        tracing::info!("负载均衡模式已设置为: {}", mode);
        Ok(())
    }
}

impl Drop for MultiTokenManager {
    fn drop(&mut self) {
        if self.stats_dirty.load(Ordering::Relaxed) {
            self.save_stats();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_write_atomic_writes_content_and_no_tmp_residue() {
        let dir = std::env::temp_dir().join(format!("kiro-atomic-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("credentials.json");

        write_atomic(&path, b"hello-atomic").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"hello-atomic");

        // 目录下不应残留 .credentials.json.*.tmp
        let has_tmp = std::fs::read_dir(&dir).unwrap().any(|e| {
            e.ok()
                .and_then(|e| e.file_name().into_string().ok())
                .map(|n| n.ends_with(".tmp"))
                .unwrap_or(false)
        });
        assert!(!has_tmp, "临时文件不应残留");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn test_write_atomic_overwrites_existing_file() {
        let dir = std::env::temp_dir().join(format!("kiro-atomic-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("data.json");

        std::fs::write(&path, b"old-content-longer").unwrap();
        write_atomic(&path, b"new").unwrap();
        // 覆盖后内容必须是新内容，不能残留旧内容尾巴
        assert_eq!(std::fs::read(&path).unwrap(), b"new");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn test_is_token_expired_with_expired_token() {
        let mut credentials = KiroCredentials::default();
        credentials.expires_at = Some("2020-01-01T00:00:00Z".to_string());
        assert!(is_token_expired(&credentials));
    }

    #[test]
    fn test_is_token_expired_with_valid_token() {
        let mut credentials = KiroCredentials::default();
        let future = Utc::now() + Duration::hours(1);
        credentials.expires_at = Some(future.to_rfc3339());
        assert!(!is_token_expired(&credentials));
    }

    #[test]
    fn test_is_token_expired_within_5_minutes() {
        let mut credentials = KiroCredentials::default();
        let expires = Utc::now() + Duration::minutes(3);
        credentials.expires_at = Some(expires.to_rfc3339());
        assert!(is_token_expired(&credentials));
    }

    #[test]
    fn test_is_token_expired_no_expires_at() {
        let credentials = KiroCredentials::default();
        assert!(is_token_expired(&credentials));
    }

    #[test]
    fn test_is_token_expiring_soon_within_10_minutes() {
        let mut credentials = KiroCredentials::default();
        let expires = Utc::now() + Duration::minutes(8);
        credentials.expires_at = Some(expires.to_rfc3339());
        assert!(is_token_expiring_soon(&credentials));
    }

    #[test]
    fn test_is_token_expiring_soon_beyond_10_minutes() {
        let mut credentials = KiroCredentials::default();
        let expires = Utc::now() + Duration::minutes(15);
        credentials.expires_at = Some(expires.to_rfc3339());
        assert!(!is_token_expiring_soon(&credentials));
    }

    #[test]
    fn test_validate_refresh_token_missing() {
        let credentials = KiroCredentials::default();
        let result = validate_refresh_token(&credentials);
        assert!(result.is_err());
    }

    #[test]
    fn test_validate_refresh_token_valid() {
        let mut credentials = KiroCredentials::default();
        credentials.refresh_token = Some("a".repeat(150));
        let result = validate_refresh_token(&credentials);
        assert!(result.is_ok());
    }

    #[test]
    fn test_sha256_hex() {
        let result = sha256_hex("test");
        assert_eq!(
            result,
            "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08"
        );
    }

    /// SSRF 回归：External IdP token_endpoint 只放行 Microsoft 登录域，其余一律拒绝。
    #[test]
    fn test_validate_microsoft_token_endpoint() {
        // 合法：官方域及其租户子路径
        for ok in [
            "https://login.microsoftonline.com/9d76.../oauth2/v2.0/token",
            "https://login.microsoftonline.us/tid/oauth2/v2.0/token",
            "https://login.partner.microsoftonline.cn/tid/oauth2/v2.0/token",
        ] {
            assert!(validate_microsoft_token_endpoint(ok).is_ok(), "应放行: {ok}");
        }
        // 非法：攻击者域 / 内网 / http / userinfo 混淆 / 相似域后缀伪装
        for bad in [
            "https://evil.com/token",
            "https://10.0.0.1/token", // 内网 IP（SSRF 应拒）
            "http://login.microsoftonline.com/token", // 非 https
            "https://login.microsoftonline.com@evil.com/token", // userinfo 混淆
            "https://login.microsoftonline.com.evil.com/token", // 后缀伪装
            "https://notmicrosoftonline.com/token",
        ] {
            assert!(validate_microsoft_token_endpoint(bad).is_err(), "应拒绝: {bad}");
        }
    }

    #[test]
    fn test_credentials_due_for_refresh_selects_expiring_only() {
        // 长度 >=100 的假 refresh_token，绕过 validate_refresh_token 截断判据
        let rt = "r".repeat(120);

        // #1 即将过期（8 分钟）→ 应入选
        let mut expiring = KiroCredentials::default();
        expiring.refresh_token = Some(rt.clone());
        expiring.expires_at = Some((Utc::now() + Duration::minutes(8)).to_rfc3339());

        // #2 仍充裕（1 小时）→ 不入选
        let mut fresh = KiroCredentials::default();
        fresh.refresh_token = Some(rt.clone());
        fresh.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());

        // #3 API Key 凭据 → 永不入选
        let mut api_key = KiroCredentials::default();
        api_key.kiro_api_key = Some("ksk_test_key_123".to_string());
        api_key.auth_method = Some("api_key".to_string());

        let manager = MultiTokenManager::new(
            Config::default(),
            vec![expiring, fresh, api_key],
            None,
            None,
            true,
        )
        .expect("构造 manager");

        let due = manager.credentials_due_for_refresh(10);
        // 仅 #1（id 从 1 起分配）
        assert_eq!(due, vec![1], "只应选中将在 10 分钟内过期的可刷新凭据");
    }

    #[tokio::test]
    async fn test_prefetch_skips_when_token_not_expiring() {
        // token 还有 1 小时才过期 → 预刷新的条件检查应在任何网络调用前跳过
        let rt = "r".repeat(120);
        let mut fresh = KiroCredentials::default();
        fresh.refresh_token = Some(rt);
        fresh.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());

        let manager = MultiTokenManager::new(
            Config::default(),
            vec![fresh],
            None,
            None,
            true,
        )
        .expect("构造 manager");

        // conditional_lead=Some(10)：token 不在 10 分钟内过期 → Skipped，不触发刷新
        let outcome = manager
            .refresh_token_locked(1, Some(10))
            .await
            .expect("跳过路径不应返回错误");
        assert_eq!(
            outcome,
            RefreshOutcome::Skipped,
            "token 未临近过期时预刷新应跳过而非发起刷新"
        );
    }

    #[tokio::test]
    async fn test_refresh_token_rejects_api_key_credential() {
        let config = Config::default();
        let mut credentials = KiroCredentials::default();
        credentials.kiro_api_key = Some("ksk_test_key_123".to_string());
        credentials.auth_method = Some("api_key".to_string());

        let result = refresh_token(&credentials, &config, None).await;

        assert!(result.is_err(), "API Key 凭据应被 refresh_token 拒绝");
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("API Key 凭据不支持刷新"),
            "期望错误消息包含 'API Key 凭据不支持刷新'，实际: {}",
            err_msg
        );
    }

    #[tokio::test]
    async fn test_ensure_valid_token_returns_api_key_without_refresh() {
        // API Key 凭据：ensure_valid_token 直接返回 kiroApiKey，绝不触发刷新（无网络）。
        let mut api_key_cred = KiroCredentials::default();
        api_key_cred.kiro_api_key = Some("ksk_ensure_valid_123".to_string());
        api_key_cred.auth_method = Some("api_key".to_string());

        let manager = MultiTokenManager::new(
            Config::default(),
            vec![api_key_cred],
            None,
            None,
            true,
        )
        .expect("构造 manager");

        let (creds, token) = manager
            .ensure_valid_token(1)
            .await
            .expect("API Key 凭据应直接返回，不报错");
        assert_eq!(token, "ksk_ensure_valid_123", "应返回 kiroApiKey 作为 token");
        assert!(creds.is_api_key_credential(), "返回的应是同一 API Key 凭据");
    }

    #[tokio::test]
    async fn test_ensure_valid_token_hot_path_no_refresh_for_fresh_token() {
        // token 还有 1 小时才过期：ensure_valid_token 走热路径直接返回现有 access_token，
        // 不碰 refresh_lock、不发起任何网络刷新（refresh_token 是废串，若真去刷会失败）。
        let mut fresh = KiroCredentials::default();
        fresh.refresh_token = Some("r".repeat(120));
        fresh.access_token = Some("hot_path_token".to_string());
        fresh.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());

        let manager = MultiTokenManager::new(
            Config::default(),
            vec![fresh],
            None,
            None,
            true,
        )
        .expect("构造 manager");

        let (creds, token) = manager
            .ensure_valid_token(1)
            .await
            .expect("未过期 token 热路径不应报错");
        assert_eq!(token, "hot_path_token", "未过期时应直接返回现有 access_token");
        assert_eq!(
            creds.access_token.as_deref(),
            Some("hot_path_token"),
            "返回凭据应携带原 access_token"
        );
    }

    #[tokio::test]
    async fn test_ensure_valid_token_expired_delegates_to_refresh() {
        // token 已过期：ensure_valid_token 委托 refresh_token_locked 走真实刷新实现。
        // 这里用 refresh_token 长度不足 100 的凭据，让底层 validate_refresh_token 在
        // 任何网络调用前就 bail——从而在无网络的单测里确认「过期 → 确实进入刷新委托路径」
        // （热路径/API Key 分流都不会命中该错误）。
        let mut expired = KiroCredentials::default();
        expired.refresh_token = Some("short".to_string()); // < 100 → validate 阶段即失败
        expired.access_token = Some("stale_token".to_string());
        expired.expires_at = Some("2020-01-01T00:00:00Z".to_string()); // 已过期

        let manager = MultiTokenManager::new(
            Config::default(),
            vec![expired],
            None,
            None,
            true,
        )
        .expect("构造 manager");

        let err = manager
            .ensure_valid_token(1)
            .await
            .expect_err("过期 token 应委托刷新，且因 refresh_token 被截断而失败");
        assert!(
            err.to_string().contains("refreshToken 已被截断"),
            "应命中刷新委托路径的 validate 报错（证明进入了刷新而非热路径），实际: {}",
            err
        );
    }

    #[tokio::test]
    async fn test_add_credential_reject_duplicate_refresh_token() {
        let config = Config::default();

        let mut existing = KiroCredentials::default();
        existing.refresh_token = Some("a".repeat(150));

        let manager = MultiTokenManager::new(config, vec![existing], None, None, false).unwrap();

        let mut duplicate = KiroCredentials::default();
        duplicate.refresh_token = Some("a".repeat(150));

        let result = manager.add_credential(duplicate).await;
        assert!(result.is_err());
        assert!(result.err().unwrap().to_string().contains("凭据已存在"));
    }

    #[tokio::test]
    async fn test_add_credential_api_key_success() {
        let config = Config::default();
        let manager = MultiTokenManager::new(config, vec![], None, None, false).unwrap();

        let mut api_key_cred = KiroCredentials::default();
        api_key_cred.kiro_api_key = Some("ksk_test_key_123".to_string());
        api_key_cred.auth_method = Some("api_key".to_string());

        let result = manager.add_credential(api_key_cred).await;
        assert!(result.is_ok());
        let id = result.unwrap();
        assert!(id > 0);
        assert_eq!(manager.total_count(), 1);
        assert_eq!(manager.available_count(), 1);
    }

    #[tokio::test]
    async fn test_add_credential_reject_duplicate_api_key() {
        let config = Config::default();

        let mut existing = KiroCredentials::default();
        existing.kiro_api_key = Some("ksk_existing_key".to_string());
        existing.auth_method = Some("api_key".to_string());

        let manager = MultiTokenManager::new(config, vec![existing], None, None, false).unwrap();

        let mut duplicate = KiroCredentials::default();
        duplicate.kiro_api_key = Some("ksk_existing_key".to_string());
        duplicate.auth_method = Some("api_key".to_string());

        let result = manager.add_credential(duplicate).await;
        assert!(result.is_err());
        assert!(result
            .err()
            .unwrap()
            .to_string()
            .contains("kiroApiKey 重复"));
    }

    #[tokio::test]
    async fn test_add_credential_api_key_empty_rejected() {
        let config = Config::default();
        let manager = MultiTokenManager::new(config, vec![], None, None, false).unwrap();

        let mut cred = KiroCredentials::default();
        cred.kiro_api_key = Some(String::new());
        cred.auth_method = Some("api_key".to_string());

        let result = manager.add_credential(cred).await;
        assert!(result.is_err());
        assert!(result
            .err()
            .unwrap()
            .to_string()
            .contains("kiroApiKey 为空"));
    }

    #[tokio::test]
    async fn test_add_credential_api_key_missing_key_rejected() {
        let config = Config::default();
        let manager = MultiTokenManager::new(config, vec![], None, None, false).unwrap();

        let mut cred = KiroCredentials::default();
        cred.auth_method = Some("api_key".to_string());
        // kiro_api_key is None

        let result = manager.add_credential(cred).await;
        assert!(result.is_err());
        assert!(result
            .err()
            .unwrap()
            .to_string()
            .contains("缺少 kiroApiKey"));
    }

    #[tokio::test]
    async fn test_add_credential_api_key_and_oauth_coexist() {
        let config = Config::default();

        let mut oauth_cred = KiroCredentials::default();
        oauth_cred.refresh_token = Some("a".repeat(150));

        let manager = MultiTokenManager::new(config, vec![oauth_cred], None, None, false).unwrap();

        let mut api_key_cred = KiroCredentials::default();
        api_key_cred.kiro_api_key = Some("ksk_new_key".to_string());
        api_key_cred.auth_method = Some("api_key".to_string());

        let result = manager.add_credential(api_key_cred).await;
        assert!(result.is_ok());
        assert_eq!(manager.total_count(), 2);
        assert_eq!(manager.available_count(), 2);
    }

    // MultiTokenManager 测试

    #[test]
    fn test_multi_token_manager_new() {
        let config = Config::default();
        let mut cred1 = KiroCredentials::default();
        cred1.priority = 0;
        let mut cred2 = KiroCredentials::default();
        cred2.priority = 1;

        let manager =
            MultiTokenManager::new(config, vec![cred1, cred2], None, None, false).unwrap();
        assert_eq!(manager.total_count(), 2);
        assert_eq!(manager.available_count(), 2);
    }

    #[test]
    fn test_duplicate_machine_id_auto_rotated() {
        // 两个凭据显式共用同一 machineId → 入池时应把重复者轮换成独立指纹(防关联)。
        let config = Config::default();
        let shared = "a".repeat(64); // 合法 64-hex 格式,两个凭据故意相同
        let mut c1 = KiroCredentials::default();
        c1.id = Some(1);
        c1.machine_id = Some(shared.clone());
        let mut c2 = KiroCredentials::default();
        c2.id = Some(2);
        c2.machine_id = Some(shared.clone());

        let mgr = MultiTokenManager::new(config, vec![c1, c2], None, None, false).unwrap();
        let m1 = mgr.export_credential(1).unwrap().machine_id.unwrap();
        let m2 = mgr.export_credential(2).unwrap().machine_id.unwrap();
        assert_ne!(m1, m2, "重复 machineId 应被自动轮换成不同值");
        // 第一个保留原值,第二个被轮换(64 hex)
        assert_eq!(m1, shared, "首个保留原 machineId");
        assert_eq!(m2.len(), 64, "轮换后应为 64 hex");
        assert!(m2.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn test_distinct_machine_ids_untouched() {
        // 已各自独立的 machineId 不应被改动。
        let config = Config::default();
        let mut c1 = KiroCredentials::default();
        c1.id = Some(1);
        c1.machine_id = Some("a".repeat(64));
        let mut c2 = KiroCredentials::default();
        c2.id = Some(2);
        c2.machine_id = Some("b".repeat(64));

        let mgr = MultiTokenManager::new(config, vec![c1, c2], None, None, false).unwrap();
        assert_eq!(mgr.export_credential(1).unwrap().machine_id.unwrap(), "a".repeat(64));
        assert_eq!(mgr.export_credential(2).unwrap().machine_id.unwrap(), "b".repeat(64));
    }

    #[tokio::test]
    async fn test_add_credential_freezes_machine_id() {
        // 上号入池(machine_id=None)应在 add 时固化稳定指纹,而非留 None 靠请求路径现算
        // (现算会随 refreshToken 轮换漂移,是防关联隐患)。
        let config = Config::default();
        let manager = MultiTokenManager::new(config, vec![], None, None, false).unwrap();
        let mut cred = KiroCredentials::default();
        cred.kiro_api_key = Some("ksk_freeze_test".to_string());
        cred.auth_method = Some("api_key".to_string());
        let id = manager.add_credential(cred).await.unwrap();
        let mid = manager
            .export_credential(id)
            .unwrap()
            .machine_id
            .expect("入池后 machineId 应已固化");
        assert_eq!(mid.len(), 64);
        assert!(mid.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[tokio::test]
    async fn test_add_credential_rotates_colliding_machine_id() {
        // 新号指纹与池中已有号撞车时,入池应轮换成独立指纹(防上游按设备指纹关联封禁)。
        let config = Config::default();
        let shared = "d".repeat(64);
        let mut existing = KiroCredentials::default();
        existing.kiro_api_key = Some("ksk_existing".to_string());
        existing.auth_method = Some("api_key".to_string());
        existing.machine_id = Some(shared.clone());
        let manager = MultiTokenManager::new(config, vec![existing], None, None, false).unwrap();
        let mut newcomer = KiroCredentials::default();
        newcomer.kiro_api_key = Some("ksk_newcomer".to_string());
        newcomer.auth_method = Some("api_key".to_string());
        newcomer.machine_id = Some(shared.clone());
        let id = manager.add_credential(newcomer).await.unwrap();
        let stored_mid = manager.export_credential(id).unwrap().machine_id.unwrap();
        assert_ne!(stored_mid, shared, "撞车指纹必须被轮换成独立值");
        assert_eq!(stored_mid.len(), 64);
    }

    #[test]
    fn test_multi_token_manager_empty_credentials() {
        let config = Config::default();
        let result = MultiTokenManager::new(config, vec![], None, None, false);
        // 支持 0 个凭据启动（可通过管理面板添加）
        assert!(result.is_ok());
        let manager = result.unwrap();
        assert_eq!(manager.total_count(), 0);
        assert_eq!(manager.available_count(), 0);
    }

    #[test]
    fn test_multi_token_manager_duplicate_ids() {
        let config = Config::default();
        let mut cred1 = KiroCredentials::default();
        cred1.id = Some(1);
        let mut cred2 = KiroCredentials::default();
        cred2.id = Some(1); // 重复 ID

        let result = MultiTokenManager::new(config, vec![cred1, cred2], None, None, false);
        assert!(result.is_err());
        let err_msg = result.err().unwrap().to_string();
        assert!(
            err_msg.contains("重复的凭据 ID"),
            "错误消息应包含 '重复的凭据 ID'，实际: {}",
            err_msg
        );
    }

    #[test]
    fn test_multi_token_manager_api_key_missing_kiro_api_key_auto_disabled() {
        let config = Config::default();

        // auth_method=api_key 但缺少 kiro_api_key → 应被自动禁用
        let mut bad_cred = KiroCredentials::default();
        bad_cred.auth_method = Some("api_key".to_string());
        // kiro_api_key 保持 None

        let mut good_cred = KiroCredentials::default();
        good_cred.refresh_token = Some("valid_token".to_string());

        let manager =
            MultiTokenManager::new(config, vec![bad_cred, good_cred], None, None, false).unwrap();
        assert_eq!(manager.total_count(), 2);
        assert_eq!(manager.available_count(), 1); // bad_cred 被禁用，只剩 1 个可用
    }

    #[test]
    fn test_multi_token_manager_api_key_with_kiro_api_key_not_disabled() {
        let config = Config::default();

        // auth_method=api_key 且有 kiro_api_key → 不应被禁用
        let mut cred = KiroCredentials::default();
        cred.auth_method = Some("api_key".to_string());
        cred.kiro_api_key = Some("ksk_test123".to_string());

        let manager = MultiTokenManager::new(config, vec![cred], None, None, false).unwrap();
        assert_eq!(manager.total_count(), 1);
        assert_eq!(manager.available_count(), 1);
    }

    #[test]
    fn test_multi_token_manager_report_failure() {
        let config = Config::default();
        let cred1 = KiroCredentials::default();
        let cred2 = KiroCredentials::default();

        let manager =
            MultiTokenManager::new(config, vec![cred1, cred2], None, None, false).unwrap();

        // 凭据会自动分配 ID（从 1 开始）
        // 前两次失败不会禁用（使用 ID 1）
        assert!(manager.report_failure(1));
        assert!(manager.report_failure(1));
        assert_eq!(manager.available_count(), 2);

        // 第三次失败会禁用第一个凭据
        assert!(manager.report_failure(1));
        assert_eq!(manager.available_count(), 1);

        // 继续失败第二个凭据（使用 ID 2）
        assert!(manager.report_failure(2));
        assert!(manager.report_failure(2));
        assert!(!manager.report_failure(2)); // 所有凭据都禁用了
        assert_eq!(manager.available_count(), 0);
    }

    #[test]
    fn test_multi_token_manager_report_success() {
        let config = Config::default();
        let cred = KiroCredentials::default();

        let manager = MultiTokenManager::new(config, vec![cred], None, None, false).unwrap();

        // 失败两次（使用 ID 1）
        manager.report_failure(1);
        manager.report_failure(1);

        // 成功后重置计数（使用 ID 1）
        manager.report_success(1);

        // 再失败两次不会禁用
        manager.report_failure(1);
        manager.report_failure(1);
        assert_eq!(manager.available_count(), 1);
    }

    #[test]
    fn test_multi_token_manager_switch_to_next() {
        let config = Config::default();
        let mut cred1 = KiroCredentials::default();
        cred1.refresh_token = Some("token1".to_string());
        let mut cred2 = KiroCredentials::default();
        cred2.refresh_token = Some("token2".to_string());

        let manager =
            MultiTokenManager::new(config, vec![cred1, cred2], None, None, false).unwrap();

        let initial_id = manager.snapshot().current_id;

        // 切换到下一个
        assert!(manager.switch_to_next());
        assert_ne!(manager.snapshot().current_id, initial_id);
    }

    #[test]
    fn test_set_load_balancing_mode_persists_to_config_file() {
        let config_path = std::env::temp_dir().join(format!(
            "kiro-load-balancing-{}.json",
            uuid::Uuid::new_v4()
        ));
        std::fs::write(&config_path, r#"{"loadBalancingMode":"priority"}"#).unwrap();

        let config = Config::load(&config_path).unwrap();
        let manager = MultiTokenManager::new(
            config,
            vec![KiroCredentials::default()],
            None,
            None,
            false,
        )
        .unwrap();

        manager
            .set_load_balancing_mode("balanced".to_string())
            .unwrap();

        let persisted = Config::load(&config_path).unwrap();
        assert_eq!(persisted.load_balancing_mode, "balanced");
        assert_eq!(manager.get_load_balancing_mode(), "balanced");

        std::fs::remove_file(&config_path).unwrap();
    }

    #[tokio::test]
    async fn test_multi_token_manager_acquire_context_auto_recovers_all_disabled() {
        let config = Config::default();
        let mut cred1 = KiroCredentials::default();
        cred1.access_token = Some("t1".to_string());
        cred1.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());
        let mut cred2 = KiroCredentials::default();
        cred2.access_token = Some("t2".to_string());
        cred2.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());

        let manager =
            MultiTokenManager::new(config, vec![cred1, cred2], None, None, false).unwrap();

        // 凭据会自动分配 ID（从 1 开始）
        for _ in 0..MAX_FAILURES_PER_CREDENTIAL {
            manager.report_failure(1);
        }
        for _ in 0..MAX_FAILURES_PER_CREDENTIAL {
            manager.report_failure(2);
        }

        assert_eq!(manager.available_count(), 0);

        // 应触发自愈：重置失败计数并重新启用，避免必须重启进程
        let ctx = manager.acquire_context(None, None).await.unwrap();
        assert!(ctx.token == "t1" || ctx.token == "t2");
        assert_eq!(manager.available_count(), 2);
    }

    #[tokio::test]
    async fn test_multi_token_manager_acquire_context_balanced_retries_until_bad_credential_disabled() {
        let mut config = Config::default();
        config.load_balancing_mode = "balanced".to_string();

        let mut bad_cred = KiroCredentials::default();
        bad_cred.priority = 0;
        bad_cred.refresh_token = Some("bad".to_string());

        let mut good_cred = KiroCredentials::default();
        good_cred.priority = 1;
        good_cred.access_token = Some("good-token".to_string());
        good_cred.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());

        let manager =
            MultiTokenManager::new(config, vec![bad_cred, good_cred], None, None, false).unwrap();

        let ctx = manager.acquire_context(None, None).await.unwrap();
        assert_eq!(ctx.id, 2);
        assert_eq!(ctx.token, "good-token");
    }

    /// 构造 N 个都带有效 token（无需刷新）的 balanced 管理器
    fn make_balanced_manager(n: usize) -> MultiTokenManager {
        let mut config = Config::default();
        config.load_balancing_mode = "balanced".to_string();
        // 关闭亲和性：本组测试要验证纯负载分摊，不要 session 粘性干扰
        config.affinity_enabled = false;
        let creds: Vec<KiroCredentials> = (0..n)
            .map(|i| {
                let mut c = KiroCredentials::default();
                c.priority = i as u32;
                c.access_token = Some(format!("tok-{}", i));
                c.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());
                c
            })
            .collect();
        MultiTokenManager::new(config, creds, None, None, false).unwrap()
    }

    // ===== TIER2 配置热重载：后台任务 abort+respawn 回归 =====

    /// 造一个可控 proactive_token_refresh 的单号 manager（带有效 token）。
    fn make_manager_with_proactive(proactive: bool) -> Arc<MultiTokenManager> {
        let mut config = Config::default();
        config.proactive_token_refresh = proactive;
        config.token_refresh_interval_secs = 5;
        let mut c = KiroCredentials::default();
        c.priority = 0;
        c.access_token = Some("tok-0".to_string());
        c.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());
        Arc::new(MultiTokenManager::new(config, vec![c], None, None, false).unwrap())
    }

    #[tokio::test]
    async fn test_respawn_refresh_task_disabled_stores_no_handle() {
        // proactive=false：respawn 后任务槽应为空（不起后台任务）
        let mgr = make_manager_with_proactive(false);
        mgr.respawn_refresh_task();
        assert!(
            mgr.refresh_task.lock().is_none(),
            "proactive_token_refresh=false 时不应存在任务句柄"
        );
    }

    #[tokio::test]
    async fn test_respawn_refresh_task_enabled_stores_handle() {
        // proactive=true：respawn 后任务槽应持有一个运行中的句柄
        let mgr = make_manager_with_proactive(true);
        mgr.respawn_refresh_task();
        let slot = mgr.refresh_task.lock();
        let handle = slot.as_ref().expect("proactive=true 应存在任务句柄");
        assert!(!handle.is_finished(), "新起的预刷新任务应在运行中");
    }

    #[tokio::test]
    async fn test_respawn_refresh_task_idempotent_aborts_old() {
        // 幂等：重复 respawn 应 abort 旧任务、只保留一个新句柄（不泄漏累积）
        let mgr = make_manager_with_proactive(true);
        mgr.respawn_refresh_task();
        // 取出旧句柄的克隆引用用于观测（AbortHandle 不便克隆，改为记录 abort 后 is_finished）
        let old_finished_before = {
            let slot = mgr.refresh_task.lock();
            slot.as_ref().unwrap().is_finished()
        };
        assert!(!old_finished_before, "第一次 respawn 的任务应在运行");

        // 第二次 respawn：内部会 abort 旧任务并换新句柄
        mgr.respawn_refresh_task();
        // 让被 abort 的旧任务有机会真正结束
        tokio::task::yield_now().await;
        let slot = mgr.refresh_task.lock();
        let handle = slot.as_ref().expect("重挂后应仍有一个任务句柄");
        assert!(!handle.is_finished(), "重挂后的新任务应在运行中");
    }

    #[tokio::test]
    async fn test_respawn_refresh_task_toggle_off_aborts() {
        // 开→关：先起任务，再把 proactive 改 false 并 respawn，句柄应清空
        let mgr = make_manager_with_proactive(true);
        mgr.respawn_refresh_task();
        assert!(mgr.refresh_task.lock().is_some(), "开启后应有句柄");

        // 原子换成关闭态的 config（模拟 reload_config 后再 respawn）
        let mut off = (*mgr.config()).clone();
        off.proactive_token_refresh = false;
        mgr.config.store(Arc::new(off));
        mgr.respawn_refresh_task();
        assert!(
            mgr.refresh_task.lock().is_none(),
            "关闭 proactive 后 respawn 应清空任务句柄"
        );
    }

    #[tokio::test]
    async fn test_inflight_spreads_concurrent_load_no_thundering_herd() {
        // 惊群回归：持有多个未完成请求的上下文（guard 未 Drop）时，
        // balanced 选号必须把后续请求分摊到不同的号，而不是全部扑向同一个。
        let manager = make_balanced_manager(3);

        // 连续获取 3 个上下文且都不释放（模拟 3 个并发在途请求）
        let c1 = manager.acquire_context(None, None).await.unwrap();
        let c2 = manager.acquire_context(None, None).await.unwrap();
        let c3 = manager.acquire_context(None, None).await.unwrap();

        // 三个在途请求应分别落在 3 个不同的凭据上（inflight 升序天然分摊）
        let mut ids = [c1.id, c2.id, c3.id];
        ids.sort_unstable();
        assert_eq!(ids, [1, 2, 3], "3 个并发在途请求应分摊到 3 个不同的号，实际 {:?}", ids);
    }

    #[tokio::test]
    async fn test_inflight_guard_release_frees_credential_for_reuse() {
        // 单个号：拿到上下文后 inflight=1，释放后归零，可被再次选中且负载记账正确
        let manager = make_balanced_manager(1);

        {
            let _ctx = manager.acquire_context(None, None).await.unwrap();
            let snap = manager.snapshot();
            let e = snap.entries.iter().find(|e| e.id == 1).unwrap();
            assert_eq!(e.inflight, 1, "持有上下文时 inflight 应为 1");
        }
        // 上下文出作用域 → guard Drop → inflight -1
        let snap = manager.snapshot();
        let e = snap.entries.iter().find(|e| e.id == 1).unwrap();
        assert_eq!(e.inflight, 0, "释放后 inflight 应归零");
    }

    #[tokio::test]
    async fn test_inflight_prefers_least_loaded_after_releases() {
        // 先让 #1 背上 2 个未完成请求，再取一次：应避开 #1，选到空闲的号
        let manager = make_balanced_manager(2);

        // 手动制造 #1 高在途：直接对其计数器加压（等价于两个未完成请求都落在 #1）
        // 通过连续 acquire 并保留：第一次可能落 #1 或 #2，用显式方式验证升序即可
        let held_a = manager.acquire_context(None, None).await.unwrap();
        let first_id = held_a.id;
        let held_b = manager.acquire_context(None, None).await.unwrap();
        let second_id = held_b.id;
        // 两个在途分属不同号
        assert_ne!(first_id, second_id);

        // 释放第二个号的请求 → 它变回空闲；下一次选号应命中刚释放的那个
        drop(held_b);
        let next = manager.acquire_context(None, None).await.unwrap();
        assert_eq!(
            next.id, second_id,
            "释放后应优先选回在途最少（=0）的号 #{}，实际 #{}",
            second_id, next.id
        );
    }

    #[tokio::test]
    async fn test_balanced_spreads_by_recent_rpm_not_lifetime_success() {
        // ⭐分流回归：balanced 应按**近 60s RPM**（即时负载）均衡分摊，而非终身 success_count。
        // 线上真实症状：#53/#54 终身 6000+、#56/#58/#59 终身几百，若按终身计数选号会持续
        // 只灌新号（把负载集中在 1-2 个号，老号闲置=部分号"不动"，且单号 RPM 高触发风控）。
        // 正确行为：串行放号应轮流命中不同的号（每次都选近窗 RPM 最少者），负载均匀铺开。
        let manager = make_balanced_manager(3);

        // 模拟 #1 已被大量使用（终身成功数很高），但当前窗口无负载。
        // 用 rpm.record 制造近窗负载差异，验证选号看的是"当下 RPM"而非"终身总量"。
        // 先给 #1 记 3 次近窗命中（当前最忙），#2 记 1 次，#3 记 0 次。
        manager.rpm.record(1);
        manager.rpm.record(1);
        manager.rpm.record(1);
        manager.rpm.record(2);

        // 立即放号（不保留在途，避免 inflight 干扰）：应选近窗 RPM 最少的 #3。
        let c = manager.acquire_context(None, None).await.unwrap();
        assert_eq!(
            c.id, 3,
            "应选近 60s RPM 最少的 #3（0 次），而非按终身或优先级，实际 #{}",
            c.id
        );
        drop(c); // 释放在途；此次放号已给 #3 记了 1 次 RPM（commit_selection），#3 现为 1 次

        // 现在窗口负载：#1=3、#2=1、#3=1。再放一次应命中 #2 或 #3（并列最少=1），绝不选最忙的 #1。
        let c2 = manager.acquire_context(None, None).await.unwrap();
        assert_ne!(c2.id, 1, "最忙的 #1（近窗 3 次）不应被选中，实际 #{}", c2.id);
    }

    #[tokio::test]
    async fn test_rpm_saturation_deprioritizes_credential() {
        // 配置 RPM 软上限=2：把 #1 打到饱和后，选号应降权 #1、优先未饱和的 #2
        let mut config = Config::default();
        config.load_balancing_mode = "balanced".to_string();
        config.affinity_enabled = false;
        config.credential_rpm_limit = 2;

        let mut c1 = KiroCredentials::default();
        c1.priority = 0; // 优先级更高，若无 RPM 降权会被优先选中
        c1.access_token = Some("tok1".to_string());
        c1.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());
        let mut c2 = KiroCredentials::default();
        c2.priority = 1;
        c2.access_token = Some("tok2".to_string());
        c2.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());

        let manager = MultiTokenManager::new(config, vec![c1, c2], None, None, false).unwrap();

        // 把 #1 的 RPM 打到软上限（record 2 次），并立即释放在途避免 inflight 干扰
        manager.rpm.record(1);
        manager.rpm.record(1);
        assert!(manager.is_rpm_saturated(1), "#1 应已 RPM 饱和");
        assert!(!manager.is_rpm_saturated(2), "#2 未饱和");

        // 选号：#1 虽优先级更高，但 RPM 饱和被降权 → 应选未饱和的 #2
        let ctx = manager.acquire_context(None, None).await.unwrap();
        assert_eq!(ctx.id, 2, "RPM 饱和的 #1 应被降权，改选未饱和的 #2");
    }

    #[tokio::test]
    async fn test_per_credential_rpm_capacity_overrides_global() {
        // per-cred rpm_limit 覆盖全局:#1 设自己的容量 5(体质好),全局是 2。
        // 打 3 次 RPM:按全局(2)会饱和,但 #1 自己容量 5 未到 → 不饱和。
        let mut config = Config::default();
        config.load_balancing_mode = "balanced".to_string();
        config.affinity_enabled = false;
        config.credential_rpm_limit = 2; // 全局软上限 2

        let mut c1 = KiroCredentials::default();
        c1.priority = 0;
        c1.rpm_limit = Some(5); // 本号容量 5,高于全局
        c1.access_token = Some("tok1".to_string());
        c1.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());
        let mut c2 = KiroCredentials::default();
        c2.priority = 1;
        c2.access_token = Some("tok2".to_string()); // 无 per-cred,用全局 2
        c2.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());

        let manager = MultiTokenManager::new(config, vec![c1, c2], None, None, false).unwrap();

        // #1 打 3 次:全局阈值 2 会判饱和,但 #1 自己容量 5 → 不饱和
        manager.rpm.record(1);
        manager.rpm.record(1);
        manager.rpm.record(1);
        assert!(!manager.is_rpm_saturated(1), "#1 有 per-cred 容量 5,打 3 次不应饱和");
        // #2 无 per-cred,用全局 2:打 2 次即饱和
        manager.rpm.record(2);
        manager.rpm.record(2);
        assert!(manager.is_rpm_saturated(2), "#2 用全局容量 2,打 2 次应饱和");
    }

    #[tokio::test]
    async fn test_affinity_sticks_session_to_same_credential_in_balanced() {
        // balanced 模式下，同一 session 的连续请求应粘在同一凭据上
        let mut config = Config::default();
        config.load_balancing_mode = "balanced".to_string();
        config.affinity_enabled = true;

        let mut c1 = KiroCredentials::default();
        c1.priority = 0;
        c1.access_token = Some("tok1".to_string());
        c1.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());
        let mut c2 = KiroCredentials::default();
        c2.priority = 1;
        c2.access_token = Some("tok2".to_string());
        c2.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());

        let manager = MultiTokenManager::new(config, vec![c1, c2], None, None, false).unwrap();

        // 首次请求绑定某凭据
        let first = manager
            .acquire_context(None, Some("session-A"))
            .await
            .unwrap();
        let bound = first.id;
        drop(first);
        // 同会话后续多次请求应始终命中同一凭据，即便 balanced 的 least-used 会倾向另一个
        for _ in 0..5 {
            let ctx = manager
                .acquire_context(None, Some("session-A"))
                .await
                .unwrap();
            assert_eq!(ctx.id, bound, "同会话应粘在同一凭据");
        }
    }

    #[tokio::test]
    async fn test_affinity_spills_to_idle_when_bound_saturated() {
        // 亲和绑定号 RPM 饱和时不再死粘,改走 balanced 分流到空闲号(retry 慢根因的修复)。
        let mut config = Config::default();
        config.load_balancing_mode = "balanced".to_string();
        config.affinity_enabled = true;
        config.credential_rpm_limit = 3; // 显式软上限 3,便于打饱和

        let mut c1 = KiroCredentials::default();
        c1.priority = 0;
        c1.access_token = Some("tok1".to_string());
        c1.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());
        let mut c2 = KiroCredentials::default();
        c2.priority = 1;
        c2.access_token = Some("tok2".to_string());
        c2.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());
        let manager = MultiTokenManager::new(config, vec![c1, c2], None, None, false).unwrap();

        // session-A 首次绑定某号
        let first = manager.acquire_context(None, Some("session-A")).await.unwrap();
        let bound = first.id;
        drop(first);
        // 把绑定号打到 RPM 饱和(软上限 3)
        for _ in 0..3 {
            manager.rpm.record(bound);
        }
        assert!(manager.is_rpm_saturated(bound), "绑定号应已饱和");
        // 同会话再来:绑定号饱和 → 应溢出到另一个空闲号,而非死粘饱和号
        let ctx = manager.acquire_context(None, Some("session-A")).await.unwrap();
        assert_ne!(ctx.id, bound, "绑定号饱和时应溢出到空闲号,不再死粘");
    }

    #[tokio::test]
    async fn test_default_saturation_fallback_spreads_load() {
        // 默认配置(credential_rpm_limit=0 未设)也要最优:回退高水位 30 判饱和,不再恒不饱和。
        let config = Config::default(); // credential_rpm_limit=0
        let mut c1 = KiroCredentials::default();
        c1.access_token = Some("tok1".to_string());
        c1.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());
        let manager = MultiTokenManager::new(config, vec![c1], None, None, false).unwrap();
        // 默认未设限,打 29 次不饱和,30 次达兜底阈值饱和
        for _ in 0..29 {
            manager.rpm.record(1);
        }
        assert!(!manager.is_rpm_saturated(1), "默认兜底 30,打 29 次不应饱和");
        manager.rpm.record(1);
        assert!(manager.is_rpm_saturated(1), "打到 30 应触发默认兜底饱和(默认配置也分流)");
    }

    #[tokio::test]
    async fn test_affinity_disabled_falls_back_to_normal_selection() {
        // 关闭亲和性时不应固定，balanced 的 least-used 应能切换凭据
        let mut config = Config::default();
        config.load_balancing_mode = "balanced".to_string();
        config.affinity_enabled = false;

        let mut c1 = KiroCredentials::default();
        c1.priority = 0;
        c1.access_token = Some("tok1".to_string());
        c1.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());
        let mut c2 = KiroCredentials::default();
        c2.priority = 1;
        c2.access_token = Some("tok2".to_string());
        c2.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());

        let manager = MultiTokenManager::new(config, vec![c1, c2], None, None, false).unwrap();

        // 第一次成功后 success_count 增加，least-used 应在第二次切到另一个凭据
        let first = manager
            .acquire_context(None, Some("session-A"))
            .await
            .unwrap();
        manager.report_success(first.id);
        let second = manager
            .acquire_context(None, Some("session-A"))
            .await
            .unwrap();
        assert_ne!(first.id, second.id, "关闭亲和性后应按 least-used 切换");
    }

    #[test]
    fn test_multi_token_manager_report_refresh_failure() {
        let config = Config::default();
        let cred1 = KiroCredentials::default();
        let cred2 = KiroCredentials::default();

        let manager =
            MultiTokenManager::new(config, vec![cred1, cred2], None, None, false).unwrap();

        assert_eq!(manager.available_count(), 2);
        for _ in 0..(MAX_FAILURES_PER_CREDENTIAL - 1) {
            assert!(manager.report_refresh_failure(1));
        }
        assert_eq!(manager.available_count(), 2);

        assert!(manager.report_refresh_failure(1));
        assert_eq!(manager.available_count(), 1);

        let snapshot = manager.snapshot();
        let first = snapshot.entries.iter().find(|e| e.id == 1).unwrap();
        assert!(first.disabled);
        assert_eq!(first.refresh_failure_count, MAX_FAILURES_PER_CREDENTIAL);
        assert_eq!(snapshot.current_id, 2);
    }

    #[tokio::test]
    async fn test_multi_token_manager_refresh_failure_disabled_is_not_auto_recovered() {
        let config = Config::default();
        let cred1 = KiroCredentials::default();
        let cred2 = KiroCredentials::default();

        let manager =
            MultiTokenManager::new(config, vec![cred1, cred2], None, None, false).unwrap();

        for _ in 0..MAX_FAILURES_PER_CREDENTIAL {
            manager.report_refresh_failure(1);
            manager.report_refresh_failure(2);
        }
        assert_eq!(manager.available_count(), 0);

        let err = manager.acquire_context(None, None).await.err().unwrap().to_string();
        assert!(
            err.contains("所有凭据均已禁用"),
            "错误应提示所有凭据禁用，实际: {}",
            err
        );
    }

    #[test]
    fn test_multi_token_manager_report_quota_exhausted() {
        let config = Config::default();
        let cred1 = KiroCredentials::default();
        let cred2 = KiroCredentials::default();

        let manager =
            MultiTokenManager::new(config, vec![cred1, cred2], None, None, false).unwrap();

        // 凭据会自动分配 ID（从 1 开始）
        assert_eq!(manager.available_count(), 2);
        assert!(manager.report_quota_exhausted(1));
        assert_eq!(manager.available_count(), 1);

        // 再禁用第二个后，无可用凭据
        assert!(!manager.report_quota_exhausted(2));
        assert_eq!(manager.available_count(), 0);
    }

    #[test]
    fn test_multi_token_manager_report_account_suspended() {
        let config = Config::default();
        let cred1 = KiroCredentials::default();
        let cred2 = KiroCredentials::default();

        let manager =
            MultiTokenManager::new(config, vec![cred1, cred2], None, None, false).unwrap();

        assert_eq!(manager.available_count(), 2);

        // 封禁凭据 1：立即禁用并切换，仍有凭据 2 可用
        assert!(manager.report_account_suspended(1));
        assert_eq!(manager.available_count(), 1);

        // 封禁凭据 2 后无可用凭据
        assert!(!manager.report_account_suspended(2));
        assert_eq!(manager.available_count(), 0);
    }

    #[tokio::test]
    async fn test_account_suspended_is_not_auto_recovered() {
        // 封禁属不可自动恢复原因：即使全部凭据被封，acquire_context 也不应把它们复活
        let config = Config::default();
        let cred1 = KiroCredentials::default();

        let manager = MultiTokenManager::new(config, vec![cred1], None, None, false).unwrap();
        assert!(!manager.report_account_suspended(1));
        assert_eq!(manager.available_count(), 0);

        // 封禁的凭据不应被自动恢复机制复活
        let ctx = manager.acquire_context(None, None).await;
        assert!(
            ctx.is_err(),
            "被封禁的凭据不应自动恢复为可用（AccountSuspended 不可自动恢复）"
        );
    }

    #[test]
    fn test_account_suspended_clears_affinity() {
        // 验证 G-7 闭环：封禁凭据时清除其会话亲和性绑定
        let config = Config::default();
        let cred1 = KiroCredentials::default();
        let cred2 = KiroCredentials::default();

        let manager =
            MultiTokenManager::new(config, vec![cred1, cred2], None, None, false).unwrap();

        // 建立会话 -> 凭据1 的亲和绑定
        manager.affinity.set("session-abc", 1);
        assert_eq!(manager.affinity.get("session-abc"), Some(1));

        // 封禁凭据1后，亲和绑定应被清除，不再指向已封的号
        manager.report_account_suspended(1);
        assert_eq!(
            manager.affinity.get("session-abc"),
            None,
            "封禁凭据后应清除其会话亲和性绑定"
        );
    }

    #[tokio::test]
    async fn test_multi_token_manager_quota_disabled_is_not_auto_recovered() {
        let config = Config::default();
        let cred1 = KiroCredentials::default();
        let cred2 = KiroCredentials::default();

        let manager =
            MultiTokenManager::new(config, vec![cred1, cred2], None, None, false).unwrap();

        manager.report_quota_exhausted(1);
        manager.report_quota_exhausted(2);
        assert_eq!(manager.available_count(), 0);

        let err = manager.acquire_context(None, None).await.err().unwrap().to_string();
        assert!(
            err.contains("所有凭据均已禁用"),
            "错误应提示所有凭据禁用，实际: {}",
            err
        );
        assert_eq!(manager.available_count(), 0);
    }

    // ============ 凭据级 Region 优先级测试 ============

    #[test]
    fn test_credential_region_priority_uses_credential_auth_region() {
        // 凭据配置了 auth_region 时，应使用凭据的 auth_region
        let mut config = Config::default();
        config.region = "us-west-2".to_string();

        let mut credentials = KiroCredentials::default();
        credentials.auth_region = Some("eu-west-1".to_string());

        let region = credentials.effective_auth_region(&config);
        assert_eq!(region, "eu-west-1");
    }

    #[test]
    fn test_credential_region_priority_fallback_to_credential_region() {
        // 凭据未配置 auth_region 但配置了 region 时，应回退到凭据.region
        let mut config = Config::default();
        config.region = "us-west-2".to_string();

        let mut credentials = KiroCredentials::default();
        credentials.region = Some("eu-central-1".to_string());

        let region = credentials.effective_auth_region(&config);
        assert_eq!(region, "eu-central-1");
    }

    #[test]
    fn test_credential_region_priority_fallback_to_config() {
        // 凭据未配置 auth_region 和 region 时，应回退到 config
        let mut config = Config::default();
        config.region = "us-west-2".to_string();

        let credentials = KiroCredentials::default();
        assert!(credentials.auth_region.is_none());
        assert!(credentials.region.is_none());

        let region = credentials.effective_auth_region(&config);
        assert_eq!(region, "us-west-2");
    }

    #[test]
    fn test_multiple_credentials_use_respective_regions() {
        // 多凭据场景下，不同凭据使用各自的 auth_region
        let mut config = Config::default();
        config.region = "ap-northeast-1".to_string();

        let mut cred1 = KiroCredentials::default();
        cred1.auth_region = Some("us-east-1".to_string());

        let mut cred2 = KiroCredentials::default();
        cred2.region = Some("eu-west-1".to_string());

        let cred3 = KiroCredentials::default(); // 无 region，使用 config

        assert_eq!(cred1.effective_auth_region(&config), "us-east-1");
        assert_eq!(cred2.effective_auth_region(&config), "eu-west-1");
        assert_eq!(cred3.effective_auth_region(&config), "ap-northeast-1");
    }

    #[test]
    fn test_idc_oidc_endpoint_uses_credential_auth_region() {
        // 验证 IdC OIDC endpoint URL 使用凭据 auth_region
        let mut config = Config::default();
        config.region = "us-west-2".to_string();

        let mut credentials = KiroCredentials::default();
        credentials.auth_region = Some("eu-central-1".to_string());

        let region = credentials.effective_auth_region(&config);
        let refresh_url = format!("https://oidc.{}.amazonaws.com/token", region);

        assert_eq!(refresh_url, "https://oidc.eu-central-1.amazonaws.com/token");
    }

    #[test]
    fn test_social_refresh_endpoint_uses_credential_auth_region() {
        // 验证 Social refresh endpoint URL 使用凭据 auth_region
        let mut config = Config::default();
        config.region = "us-west-2".to_string();

        let mut credentials = KiroCredentials::default();
        credentials.auth_region = Some("ap-southeast-1".to_string());

        let region = credentials.effective_auth_region(&config);
        let refresh_url = format!("https://prod.{}.auth.desktop.kiro.dev/refreshToken", region);

        assert_eq!(
            refresh_url,
            "https://prod.ap-southeast-1.auth.desktop.kiro.dev/refreshToken"
        );
    }

    #[test]
    fn test_api_call_uses_effective_api_region() {
        // 验证 API 调用使用 effective_api_region
        let mut config = Config::default();
        config.region = "us-west-2".to_string();

        let mut credentials = KiroCredentials::default();
        credentials.region = Some("eu-west-1".to_string());

        // 凭据.region 不参与 api_region 回退链
        let api_region = credentials.effective_api_region(&config);
        let api_host = format!("q.{}.amazonaws.com", api_region);

        assert_eq!(api_host, "q.us-west-2.amazonaws.com");
    }

    #[test]
    fn test_api_call_uses_credential_api_region() {
        // 凭据配置了 api_region 时，API 调用应使用凭据的 api_region
        let mut config = Config::default();
        config.region = "us-west-2".to_string();

        let mut credentials = KiroCredentials::default();
        credentials.api_region = Some("eu-central-1".to_string());

        let api_region = credentials.effective_api_region(&config);
        let api_host = format!("q.{}.amazonaws.com", api_region);

        assert_eq!(api_host, "q.eu-central-1.amazonaws.com");
    }

    #[test]
    fn test_credential_region_empty_string_treated_as_set() {
        // 安全(H3)行为变更:空字符串/非法 region 不再被"视为已设置",而是过白名单不命中
        // → 回退到 config。旧行为让空串拼出坏 host(runtime..kiro.dev),现修正为回退可信 config。
        let mut config = Config::default();
        config.region = "us-west-2".to_string();

        let mut credentials = KiroCredentials::default();
        credentials.auth_region = Some("".to_string());

        let region = credentials.effective_auth_region(&config);
        // 空字符串不命中白名单 → 回退 config.region
        assert_eq!(region, "us-west-2");
    }

    #[test]
    fn test_auth_and_api_region_independent() {
        // auth_region 和 api_region 互不影响（用真实 AWS region,过白名单）
        let mut config = Config::default();
        config.region = "us-east-1".to_string();

        let mut credentials = KiroCredentials::default();
        credentials.auth_region = Some("eu-west-1".to_string());
        credentials.api_region = Some("ap-northeast-1".to_string());

        assert_eq!(credentials.effective_auth_region(&config), "eu-west-1");
        assert_eq!(credentials.effective_api_region(&config), "ap-northeast-1");
    }

    // ============ 凭据回收站测试 ============

    /// 软删除后：凭据不在 entries、在 trash
    #[test]
    fn test_delete_moves_credential_to_trash() {
        let config = Config::default();
        let mut c1 = KiroCredentials::default();
        c1.refresh_token = Some("refresh-1".to_string());
        let mut c2 = KiroCredentials::default();
        c2.refresh_token = Some("refresh-2".to_string());

        let manager = MultiTokenManager::new(config, vec![c1, c2], None, None, false).unwrap();

        // 必须先禁用才能删除
        manager.set_disabled(1, true).unwrap();
        manager.delete_credential(1).unwrap();

        // 不在 entries
        let snapshot = manager.snapshot();
        assert_eq!(snapshot.total, 1);
        assert!(snapshot.entries.iter().all(|e| e.id != 1));

        // 在 trash
        let trash = manager.list_trash();
        assert_eq!(trash.len(), 1);
        assert_eq!(trash[0].id, 1);
        assert!(!trash[0].deleted_at.is_empty());
    }

    /// 删除未禁用凭据应被拒绝，且不进入回收站
    #[test]
    fn test_delete_requires_disabled() {
        let config = Config::default();
        let mut c1 = KiroCredentials::default();
        c1.refresh_token = Some("refresh-1".to_string());

        let manager = MultiTokenManager::new(config, vec![c1], None, None, false).unwrap();

        let err = manager.delete_credential(1).unwrap_err().to_string();
        assert!(err.contains("只能删除已禁用的凭据"), "实际: {}", err);
        assert_eq!(manager.list_trash().len(), 0);
        assert_eq!(manager.total_count(), 1);
    }

    /// 恢复后：回 entries 且 id 不变
    #[test]
    fn test_restore_returns_to_entries_id_unchanged() {
        let config = Config::default();
        let mut c1 = KiroCredentials::default();
        c1.refresh_token = Some("refresh-1".to_string());
        let mut c2 = KiroCredentials::default();
        c2.refresh_token = Some("refresh-2".to_string());

        let manager = MultiTokenManager::new(config, vec![c1, c2], None, None, false).unwrap();

        manager.set_disabled(2, true).unwrap();
        manager.delete_credential(2).unwrap();
        assert_eq!(manager.list_trash().len(), 1);

        // 恢复
        manager.restore_credential(2).unwrap();

        // 回到 entries，id 保持 2
        let snapshot = manager.snapshot();
        assert_eq!(snapshot.total, 2);
        let restored = snapshot.entries.iter().find(|e| e.id == 2);
        assert!(restored.is_some(), "id=2 应回到 entries");
        // 恢复为禁用态
        assert!(restored.unwrap().disabled);
        // 回收站已清空该条目
        assert_eq!(manager.list_trash().len(), 0);
    }

    /// 恢复重复 refreshToken 被拒
    #[test]
    fn test_restore_duplicate_refresh_token_rejected() {
        let config = Config::default();
        // 两个凭据故意使用相同 refreshToken
        let mut c1 = KiroCredentials::default();
        c1.refresh_token = Some("same-refresh".to_string());
        let mut c2 = KiroCredentials::default();
        c2.refresh_token = Some("same-refresh".to_string());

        let manager = MultiTokenManager::new(config, vec![c1, c2], None, None, false).unwrap();

        // 删除 id=1（进入回收站）；id=2 仍在 entries，持有相同 refreshToken
        manager.set_disabled(1, true).unwrap();
        manager.delete_credential(1).unwrap();

        // 恢复 id=1 应因 refreshToken 与 id=2 重复而被拒
        let err = manager.restore_credential(1).unwrap_err().to_string();
        assert!(err.contains("refreshToken 重复"), "实际: {}", err);
        // 仍留在回收站，未误入 entries
        assert_eq!(manager.list_trash().len(), 1);
        assert_eq!(manager.total_count(), 1);
    }

    /// new_id 分配跳过 trash 里的 id，防撞号
    #[tokio::test]
    async fn test_new_id_skips_trash_id() {
        let config = Config::default();
        // 用 API Key 凭据，add_credential 无需网络刷新
        let mut c1 = KiroCredentials::default();
        c1.auth_method = Some("api_key".to_string());
        c1.kiro_api_key = Some("ksk_first_credential_key".to_string());

        let manager = MultiTokenManager::new(config, vec![c1], None, None, false).unwrap();

        // 删除 id=1 → 进回收站，entries 空
        manager.set_disabled(1, true).unwrap();
        manager.delete_credential(1).unwrap();
        assert_eq!(manager.total_count(), 0);
        assert_eq!(manager.list_trash().len(), 1);

        // 新增凭据：即便 entries 为空，new_id 也须跳过回收站里的 id=1
        let mut new_cred = KiroCredentials::default();
        new_cred.auth_method = Some("api_key".to_string());
        new_cred.kiro_api_key = Some("ksk_second_credential_key".to_string());
        let new_id = manager.add_credential(new_cred).await.unwrap();

        assert_eq!(new_id, 2, "new_id 必须跳过回收站里的 id=1");
    }

    /// purge：从回收站彻底删除后不可恢复
    #[test]
    fn test_purge_removes_from_trash() {
        let config = Config::default();
        let mut c1 = KiroCredentials::default();
        c1.refresh_token = Some("refresh-1".to_string());
        let mut c2 = KiroCredentials::default();
        c2.refresh_token = Some("refresh-2".to_string());

        let manager = MultiTokenManager::new(config, vec![c1, c2], None, None, false).unwrap();

        manager.set_disabled(1, true).unwrap();
        manager.delete_credential(1).unwrap();
        assert_eq!(manager.list_trash().len(), 1);

        manager.purge_credential(1).unwrap();
        assert_eq!(manager.list_trash().len(), 0);

        // 已彻底删除，恢复应报不存在
        let err = manager.restore_credential(1).unwrap_err().to_string();
        assert!(err.contains("回收站中不存在"), "实际: {}", err);
    }

    /// purge_expired_trash：按保留期清理，0 表示永久保留
    #[test]
    fn test_purge_expired_trash_retention() {
        let config = Config::default();
        let mut c1 = KiroCredentials::default();
        c1.refresh_token = Some("refresh-1".to_string());

        let manager = MultiTokenManager::new(config, vec![c1], None, None, false).unwrap();

        manager.set_disabled(1, true).unwrap();
        manager.delete_credential(1).unwrap();

        // 把删除时间改成 40 天前
        {
            let mut trash = manager.trash.lock();
            trash[0].deleted_at = (Utc::now() - Duration::days(40)).to_rfc3339();
        }

        // retention=0：永久保留，不清理
        assert_eq!(manager.purge_expired_trash(0), 0);
        assert_eq!(manager.list_trash().len(), 1);

        // retention=30：40 天前的条目应被清理
        assert_eq!(manager.purge_expired_trash(30), 1);
        assert_eq!(manager.list_trash().len(), 0);
    }

    /// trash.json 持久化往返：多凭据格式下删除落盘，重建后回收站仍在
    #[test]
    fn test_trash_persists_and_reloads() {
        let dir = std::env::temp_dir().join(format!("kiro-trash-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let cred_path = dir.join("credentials.json");
        std::fs::write(
            &cred_path,
            r#"[{"id":1,"refreshToken":"refresh-1"},{"id":2,"refreshToken":"refresh-2"}]"#,
        )
        .unwrap();

        let creds = vec![
            {
                let mut c = KiroCredentials::default();
                c.id = Some(1);
                c.refresh_token = Some("refresh-1".to_string());
                c
            },
            {
                let mut c = KiroCredentials::default();
                c.id = Some(2);
                c.refresh_token = Some("refresh-2".to_string());
                c
            },
        ];

        let manager = MultiTokenManager::new(
            Config::default(),
            creds,
            None,
            Some(cred_path.clone()),
            true,
        )
        .unwrap();

        manager.set_disabled(1, true).unwrap();
        manager.delete_credential(1).unwrap();

        // trash.json 应已写入
        let trash_file = dir.join("trash.json");
        assert!(trash_file.exists(), "trash.json 应已落盘");

        // 用同一凭据文件重建 manager（此时 credentials.json 已移除 id=1）
        let reload_creds =
            crate::kiro::model::credentials::CredentialsConfig::load(&cred_path)
                .unwrap()
                .into_sorted_credentials();
        let manager2 = MultiTokenManager::new(
            Config::default(),
            reload_creds,
            None,
            Some(cred_path.clone()),
            true,
        )
        .unwrap();

        // 回收站从磁盘恢复
        let trash = manager2.list_trash();
        assert_eq!(trash.len(), 1);
        assert_eq!(trash[0].id, 1);
        // entries 只剩 id=2
        assert_eq!(manager2.total_count(), 1);

        std::fs::remove_dir_all(&dir).ok();
    }
}
