//! Token 管理模块
//!
//! 负责 Token 过期检测和刷新，支持 Social 和 IdC 认证方式
//! 支持多凭据 (MultiTokenManager) 管理

use anyhow::bail;
use chrono::{DateTime, Duration, Utc};
use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex as TokioMutex;
use tokio::task::JoinHandle;
use tokio::time::sleep;

use std::collections::HashMap;
use std::fmt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64};
use std::time::{Duration as StdDuration, Instant};

use arc_swap::ArcSwap;

use crate::http_client::{ProxyConfig, build_client, build_client_no_redirect, build_streaming_client};
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

/// Returns whether the credential's token expires within the given number of minutes.
///
/// Returns `None` in two cases:
/// - `expires_at` is `None` — the credential carries no expiry field (API key credentials,
///   or social/IdC credentials whose expiry data is missing or was never set).
/// - `expires_at` is `Some` but its value fails RFC 3339 parsing (malformed or corrupted
///   timestamp string). `DateTime::parse_from_rfc3339` returns `Err`; `.ok()` maps that
///   to `None`, which propagates out through `and_then`.
///
/// Callers are responsible for deciding what `None` means in their context via `unwrap_or`.
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

/// 检查 Token 是否即将（5分钟内）过期，非真正已过期。名称有误导性，实为提前刷新触发条件。
///
/// Returns whether the credential's token is currently expired (using a 5-minute early window).
///
/// # `expires_at = None` semantics
///
/// `is_token_expiring_within` returns `None` when `expires_at` is absent or unparseable.
/// This function applies different handling depending on credential type:
///
/// - **API key credentials**: `expires_at` is never set because API keys do not expire.
///   Returning `true` here would trigger a pointless refresh attempt on every request;
///   `refresh_token()` would immediately fail with "API keys do not support refresh".
///   Therefore API key credentials short-circuit to `false` before reaching the `unwrap_or`.
///
/// - **Social / IdC credentials with missing or corrupted `expires_at`**: after the API key
///   guard, a `None` result (missing field or RFC 3339 parse failure) is treated as expired
///   via `unwrap_or(true)`. The fail-safe assumption is that an unreadable expiry means the
///   token should be refreshed rather than blindly trusted, which is the safer default.
pub(crate) fn is_token_expired(credentials: &KiroCredentials) -> bool {
    if credentials.is_api_key_credential() {
        return false;
    }
    is_token_expiring_within(credentials, 5).unwrap_or(true)
}

/// Returns whether the credential's token will expire within the next 10 minutes.
///
/// # `expires_at = None` semantics
///
/// `is_token_expiring_within` returns `None` when `expires_at` is absent or unparseable.
/// This function resolves `None` as `false` via `unwrap_or(false)`, meaning:
///
/// - **API key credentials**: no `expires_at` field exists; `None` correctly maps to
///   "not expiring soon" — there is no expiry to proactively refresh against.
///
/// - **Social / IdC credentials with missing or corrupted `expires_at`**: a `None` result
///   is also treated as "not expiring soon". Unlike `is_token_expired` (which defaults to
///   `true` as a safety measure), proactive background refresh is a best-effort optimisation
///   and the cost of skipping it is low. Defaulting to `false` avoids spurious refresh
///   scheduling when expiry information is unavailable.
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

/// 携带结构化上号诊断的错误：贯穿刷新/探测路径，供 service 层 downcast 取出诊断，
/// 序列化成 (归因+引导) 给前端，取代裸字符串 → 502。见 [`crate::kiro::diagnosis`]。
#[derive(Debug)]
pub(crate) struct DiagnosedError {
    pub(crate) diagnosis: crate::kiro::diagnosis::OnboardingDiagnosis,
}

impl fmt::Display for DiagnosedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Display 用诊断的 summary（供日志/兜底文本），结构化信息经 downcast 取 diagnosis。
        write!(f, "{}", self.diagnosis.summary)
    }
}

impl std::error::Error for DiagnosedError {}

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

    let result = if credentials.is_external_idp_credential() {
        refresh_external_idp_token(credentials, config, proxy).await
    } else if auth_method.eq_ignore_ascii_case("idc")
        || auth_method.eq_ignore_ascii_case("builder-id")
        || auth_method.eq_ignore_ascii_case("iam")
    {
        refresh_idc_token(credentials, config, proxy).await
    } else {
        refresh_social_token(credentials, config, proxy).await
    };
    // 可观测:真实刷新分发的成败(early bail 的 api_key/validate 不计,那不是网络刷新)。
    if result.is_ok() {
        crate::common::recovery_metrics::bump_refresh_ok();
    } else {
        crate::common::recovery_metrics::bump_refresh_fail();
    }
    result
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

        // 400 + invalid_grant → refreshToken 永久失效
        if status.as_u16() == 400
            && body_text.contains("invalid_grant")
        {
            return Err(RefreshTokenInvalidError {
                message: format!("Social refreshToken 已失效 (invalid_grant): {}", body_text),
            }
            .into());
        }

        // 401 = Cognito token 被吊销或已永久过期 → 立即禁用凭据。
        if status.as_u16() == 401 {
            return Err(RefreshTokenInvalidError {
                message: format!("Social refreshToken 已失效 (401 Unauthorized): {}", body_text),
            }
            .into());
        }

        let error_msg = match status.as_u16() {
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
            401 => {
                // 401 = Microsoft IdP token 被吊销或已永久过期 → 立即禁用凭据（与 invalid_grant 同语义）。
                return Err(RefreshTokenInvalidError {
                    message: format!(
                        "External IdP refreshToken 已失效 (401 Unauthorized): {}",
                        body_text
                    ),
                }
                .into());
            }
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

        // 400 + invalid_grant → refreshToken 永久失效
        // （保留 RefreshTokenInvalidError:调度层据它禁用/标记该号,语义强于通用诊断）。
        if status.as_u16() == 400
            && body_text.contains("invalid_grant")
        {
            return Err(RefreshTokenInvalidError {
                message: format!("IdC refreshToken 已失效 (invalid_grant): {}", body_text),
            }
            .into());
        }

        // 401 = token 被吊销或已永久过期，不应再重试 → 立即禁用凭据（与 invalid_grant 同语义）。
        if status.as_u16() == 401 {
            return Err(RefreshTokenInvalidError {
                message: format!("IdC refreshToken 已失效 (401 Unauthorized): {}", body_text),
            }
            .into());
        }

        // 其余非 2xx：交结构化诊断（含 #98 实测的 invalid_request/Invalid token provided →
        // CLIENT_OR_TOKEN_MISMATCH，此前落兜底裸 502）。诊断带归因+引导，service 层 downcast 透传前端。
        let diagnosis = crate::kiro::diagnosis::diagnose_refresh(status.as_u16(), &body_text);
        tracing::warn!("IdC 刷新失败：{}", diagnosis.log_line());
        return Err(DiagnosedError { diagnosis }.into());
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

/// custom_api 写入 base_url 时的 SSRF 主防线：拼出与 [`passthrough::forward`] /
/// [`deep_verify_custom_api`] **完全一致**的最终透传 URL，校验其目标 IP 不落
/// 内网/环回/链路本地/元数据/保留段。校验**最终 URL**而非裸 base，防 `https://ok@169.254.x`
/// 之类 userinfo 混淆（ssrf::parse_host_port 已剥 userinfo 取真实 host）。
///
/// `allow_http=true`（dwgx 定：允许明文 http 中转站）——scheme 放宽，但 IP 层禁止段仍拦截，
/// 元数据端点 169.254.x 一律挡下。出站另有禁重定向做纵深。
///
/// 策略取 [`SsrfPolicy::AdminConfigured`]：这个 base_url 是管理员过了 adminKey 鉴权后
/// 亲手填的，与匿名可达的背景图代理不是同一威胁模型。它只额外豁免 198.18.0.0/15
/// （代理软件 fake-IP 池默认段）——否则开着 Clash fake-IP 的机器上**任何**中转站域名
/// 都会解析到该段而无法添加（实测 api.uu6.top → 198.18.0.46）。私有段、环回、
/// 云元数据端点仍然全部拦下。理由详见 `ssrf::is_forbidden_ipv4_with` 的文档。
async fn validate_custom_api_base_url(base_raw: &str) -> anyhow::Result<()> {
    let base = base_raw.trim().trim_end_matches('/');
    let url = if base.ends_with("/v1") || base.contains("/v1/") {
        format!("{base}/messages")
    } else {
        format!("{base}/v1/messages")
    };
    crate::common::ssrf::validate_outbound_url_with(
        &url,
        /*allow_http=*/ true,
        crate::common::ssrf::SsrfPolicy::AdminConfigured,
    )
    .await
    .map_err(|e| anyhow::anyhow!("自定义 API base_url 校验失败: {e}"))
}

/// 运行时动态解析真实 profileArn（Kiro management ControlPlane 的 ListAvailableProfiles）。
///
/// 为何需要:idc/Enterprise/external_idp 号入池/刷新后常没有 profileArn(oidc 刷新不回传它),
/// 而对话/余额端点对这类号要求带**真实** profileArn——回退默认占位 ARN 对 Enterprise 号
/// 会被上游判 `Invalid token`/403(实测)。Kiro IDE / kiro-account-manager 的做法是运行时
/// 调 ListAvailableProfiles 拿账号真实的 profile arn。此函数复刻该 recipe:
/// - `POST https://management.{region}.kiro.dev/`(根路径)
/// - header: `x-amz-target: KiroControlPlaneBearerService.ListAvailableProfiles`
///   + `content-type: application/x-amz-json-1.0` + Bearer + control-plane UA
/// - body: `{}`;成功取响应 `profiles` 里的 arn。
///
/// ⭐ **region 修正(2026-07-12 真 token 实测,推翻旧「固定 us-east-1」规则)**:
/// External IdP 账号可在多 region 各有独立 profile。实测同一账号:
///   - management.us-east-1.kiro.dev  → 只返回 us-east-1 的 profile
///   - management.eu-central-1.kiro.dev → 只返回 eu-central-1 的 profile
/// **每个 region 端点只返回本 region 的 profile**(旧注释说 eu 返回空 `[]` 是误判:那是当时
/// 账号在 eu 无 profile,非端点不行)。故必须打**号自己 region** 的端点,固定 us-east-1 会让
/// eu/ap 号拿到 us 的 ARN 覆盖写回 → region 与真实 profile 错配 → 400 Improperly formed
/// (这正是「导入成功但刷新不了/ARN 不匹配」的根因)。
///
/// 本函数按 `preferred_region` 优先探测;拿到即返回(带 region 自洽的 arn)。
///
/// 返回 Ok(Some(arn)) 拿到、Ok(None) 该 region 无 profile、Err 网络/上游错误。
pub(crate) async fn resolve_profile_arn_via_management(
    credentials: &KiroCredentials,
    config: &Config,
    token: &str,
    proxy: Option<&ProxyConfig>,
    preferred_region: &str,
) -> anyhow::Result<Option<String>> {
    let host = format!("management.{}.kiro.dev", preferred_region);
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

/// External IdP / Enterprise 号动态解析 profileArn 时的多 region 探测候选。
/// 单一真相源见 [`crate::kiro::regions::PROFILE_PROBE_REGIONS`]（此处 re-export，调用点不变）。
/// 先探测号自己的 region，再按此表兜底（去重）。
pub(crate) use crate::kiro::regions::PROFILE_PROBE_REGIONS;

/// 全 region 都探测不到可用 profile 的号，两次全坏 reprobe 之间的最小冷却间隔（成本护栏）。
/// 见 [`CredentialEntry::last_full_reprobe_at`]。6 小时足够稀释「每 token TTL 白跑一轮」的浪费，
/// 又不至于长到 dwgx 在别 region 开通后要等太久才自动纠正（届时手动刷新/切 region 也能立即生效）。
const REPROBE_ALL_BAD_COOLDOWN: StdDuration = StdDuration::from_secs(6 * 3600);

/// 多 region 探测 profileArn:优先号自己的 region,拿到就用(region 与 ARN 自洽);
/// 该 region 无 profile 再依次探测候选 region 兜底。任一命中即返回,全部无则 Ok(None)。
///
/// 每个 management 端点只返回本 region 的 profile(实测),所以优先探测号 region 能拿到
/// region 完全匹配的 ARN——从根上杜绝「拿到别 region ARN 覆盖导致错配」。
pub(crate) async fn resolve_profile_arn_multi_region(
    credentials: &KiroCredentials,
    config: &Config,
    token: &str,
    proxy: Option<&ProxyConfig>,
    preferred_region: &str,
) -> anyhow::Result<Option<String>> {
    // 探测顺序:号自己的 region 打头,后接候选表(去重)。
    let mut order: Vec<&str> = vec![preferred_region];
    for r in PROFILE_PROBE_REGIONS {
        if !order.contains(r) {
            order.push(r);
        }
    }
    let mut last_err: Option<anyhow::Error> = None;
    for region in order {
        match resolve_profile_arn_via_management(credentials, config, token, proxy, region).await {
            Ok(Some(arn)) => return Ok(Some(arn)),
            Ok(None) => continue, // 该 region 无 profile,试下一个
            Err(e) => {
                tracing::debug!("profileArn 探测 region={} 失败(继续试其它): {}", region, e);
                last_err = Some(e);
            }
        }
    }
    // 全部 region 都无 profile:若中途有网络错误则上报最后一个,否则 Ok(None)(账号确无 profile)。
    match last_err {
        Some(e) => Err(e),
        None => Ok(None),
    }
}

// ============================================================================
// External IdP「验活」层：某 region 的 profile 是否真开通可用
// ============================================================================
//
// 背景（2026-07-12 真 token 实测）：同一 external_idp（M365）账号可在多 region 各有
// 独立 profile，但**只有部分 region 真正开通可用**：
//   - us-east-1(account 617485799832) → getUsageLimits 403 FEATURE_NOT_SUPPORTED
//   - eu-central-1(account 155119901513) → 200, subscriptionTitle="KIRO POWER"
// 现有 region 解析只保证「ARN region 自洽」,从不验证「这个 region 的 profile 是否真开通」。
// 本层在既有解析之上补「验活选择」：真发一次 getUsageLimits 探测,只有 200 才算 usable。

/// External IdP 某 region profile 的「验活」结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ProfileProbeOutcome {
    /// 该 region profile 真开通可用（getUsageLimits 2xx）。附带解析到的订阅标题（便于择优）。
    Usable { subscription_title: Option<String> },
    /// 403 FEATURE_NOT_SUPPORTED —— profile 存在但该 region 未开通（本 bug 的核心症状）。
    FeatureNotSupported,
    /// 401 —— token 无效/过期（与 region 无关，调用方不应据此判死 region）。
    Unauthorized,
    /// 其它错误（含 429 限流 / 5xx / 网络错误 / 非法响应）——视为「暂时不可用」，不据此判死 region。
    OtherError(String),
}

/// 纯逻辑：按 HTTP status + body 把一次 getUsageLimits 验活分类。
///
/// 单独抽出便于单测（无网络）。200 返回 `Usable{subscription_title:None}`——真实标题由
/// [`probe_profile_usable`] 解析响应体后填入；此处只做 status/body 语义分类。
/// **铁律**：429 归 OtherError（暂时不可用，绝不因限流判死一个 region）。
fn classify_profile_probe(status: u16, body: &str) -> ProfileProbeOutcome {
    if (200..300).contains(&status) {
        return ProfileProbeOutcome::Usable {
            subscription_title: None,
        };
    }
    if status == 403 && body.contains("FEATURE_NOT_SUPPORTED") {
        return ProfileProbeOutcome::FeatureNotSupported;
    }
    if status == 401 {
        return ProfileProbeOutcome::Unauthorized;
    }
    let snippet: String = body.chars().take(200).collect();
    ProfileProbeOutcome::OtherError(format!("HTTP {}: {}", status, snippet))
}

/// 验活单个候选 profileArn：clone base → 强制 `profile_arn=candidate_arn` → `sync_region_from_arn()`
/// 保证 host region 与 ARN 一致 → 自己发一次 getUsageLimits（复刻 [`get_usage_limits`] 的请求构造，
/// 30s 超时）→ 按 status+body 分类。**只读探测**，不改任何持久化状态。
pub(crate) async fn probe_profile_usable(
    base: &KiroCredentials,
    config: &Config,
    token: &str,
    proxy: Option<&ProxyConfig>,
    candidate_arn: &str,
) -> ProfileProbeOutcome {
    // 强制候选 arn，并让 region/auth_region 随 ARN 物理绑定（防呆铁律：region 与 ARN 自洽）。
    let mut cred = base.clone();
    cred.profile_arn = Some(candidate_arn.to_string());
    cred.sync_region_from_arn();
    let region = cred.effective_upstream_region(config);
    let host = format!("management.{}.kiro.dev", region);
    let machine_id = machine_id::generate_from_credentials(&cred, config);

    let mut url = format!(
        "https://{}/getUsageLimits?isEmailRequired=true&origin=AI_EDITOR&resourceType=AGENTIC_REQUEST",
        host
    );
    // 验活必须带候选 arn（external_idp 缺 arn 会 400 profileArn is required）。
    url.push_str(&format!("&profileArn={}", urlencoding::encode(candidate_arn)));

    let user_agent = format!(
        "aws-sdk-js/1.0.0 ua/2.1 os/{} lang/js md/nodejs#{} api/codewhispererruntime#1.0.0 m/N,E KiroIDE-{}-{}",
        config.system_version, config.node_version, config.kiro_version, machine_id
    );
    let amz_user_agent = format!(
        "aws-sdk-js/1.0.0 KiroIDE-{}-{}",
        config.kiro_version, machine_id
    );

    let client = match build_client(proxy, 30, config.tls_backend) {
        Ok(c) => c,
        Err(e) => return ProfileProbeOutcome::OtherError(format!("构建 HTTP 客户端失败: {}", e)),
    };
    let mut request = client
        .get(&url)
        .header("x-amz-user-agent", &amz_user_agent)
        .header("user-agent", &user_agent)
        .header("host", &host)
        .header("amz-sdk-invocation-id", uuid::Uuid::new_v4().to_string())
        .header("amz-sdk-request", "attempt=1; max=1")
        .header("Authorization", format!("Bearer {}", token));
    if cred.is_api_key_credential() {
        request = request.header("tokentype", "API_KEY");
    } else if cred.is_external_idp_credential() {
        request = request.header("tokentype", "EXTERNAL_IDP");
    }

    let response = match request.send().await {
        Ok(r) => r,
        Err(e) => return ProfileProbeOutcome::OtherError(format!("请求失败: {}", e)),
    };
    let status = response.status().as_u16();
    let body = response.text().await.unwrap_or_default();
    match classify_profile_probe(status, &body) {
        ProfileProbeOutcome::Usable { .. } => {
            // 解析订阅标题（供择优：优先选非 FREE 的 region）。
            let title = serde_json::from_str::<UsageLimitsResponse>(&body)
                .ok()
                .and_then(|u| u.subscription_title().map(|s| s.to_string()));
            ProfileProbeOutcome::Usable {
                subscription_title: title,
            }
        }
        other => other,
    }
}

/// 一个验活过的候选 profile（arn + region + account + 是否可用 + 订阅标题 + 原因标签）。
#[derive(Debug, Clone)]
pub struct ProfileCandidate {
    pub arn: String,
    pub region: String,
    pub account: String,
    pub usable: bool,
    pub subscription_title: Option<String>,
    /// 分类原因（"usable" / "feature_not_supported" / "unauthorized" / "error"）。
    pub reason: &'static str,
    /// 是否为该号**当前**绑定的 profileArn(前端标「当前」绿标 + 禁点,省一次冗余 switch)。
    pub current: bool,
}

/// 候选排序键（越小越靠前）：usable 优先；usable 内订阅标题非空非 FREE 更优。
/// 纯逻辑，便于单测。
fn candidate_rank(c: &ProfileCandidate) -> (u8, u8) {
    let usable_key = if c.usable { 0 } else { 1 };
    let title_key = match c.subscription_title.as_deref() {
        Some(t) if !t.trim().is_empty() && !t.to_uppercase().contains("FREE") => 0,
        _ => 1,
    };
    (usable_key, title_key)
}

/// 从 `arn:aws:codewhisperer:{region}:{account}:profile/{id}` 提取 account（index 4）。
fn account_from_arn(arn: &str) -> String {
    arn.split(':')
        .nth(4)
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// 与 [`resolve_profile_arn_via_management`] 同构，但返回该 region 端点的**全部** arn（原单值
/// 函数用 `find_map` 只取第一个，保留不动）。供验活层枚举候选。
pub(crate) async fn list_region_profile_arns_mgmt(
    credentials: &KiroCredentials,
    config: &Config,
    token: &str,
    proxy: Option<&ProxyConfig>,
    region: &str,
) -> anyhow::Result<Vec<String>> {
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
    let arns = data
        .get("profiles")
        .and_then(|p| p.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|p| p.get("arn").and_then(|a| a.as_str()))
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    Ok(arns)
}

/// 枚举该账号在（号自己 region + [`PROFILE_PROBE_REGIONS`]）的**全部** arn（去重），逐个
/// [`probe_profile_usable`] 验活，构成候选列表。usable=true 排前面（再按订阅标题优先非 FREE）。
///
/// 每个 management 端点只返回本 region 的 profile（实测），故逐 region 枚举再合并。
pub(crate) async fn probe_all_usable_profiles(
    base: &KiroCredentials,
    config: &Config,
    token: &str,
    proxy: Option<&ProxyConfig>,
) -> Vec<ProfileCandidate> {
    // 该号当前绑定的 profileArn(用于给候选标 current;建号前 base.profile_arn=None → 全 false)。
    let current_arn = base
        .profile_arn
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());

    // region 探测顺序：号自己 region 打头 + 候选表（去重）。
    let preferred = base.effective_upstream_region(config).to_string();
    let mut regions: Vec<String> = vec![preferred];
    for r in PROFILE_PROBE_REGIONS {
        if !regions.iter().any(|x| x == r) {
            regions.push(r.to_string());
        }
    }

    // 枚举全部 region 的全部 arn，去重。
    let mut seen = std::collections::HashSet::new();
    let mut arns: Vec<String> = Vec::new();
    for region in &regions {
        match list_region_profile_arns_mgmt(base, config, token, proxy, region).await {
            Ok(list) => {
                for arn in list {
                    if seen.insert(arn.clone()) {
                        arns.push(arn);
                    }
                }
            }
            Err(e) => tracing::debug!("列 region={} profile 失败（继续）: {}", region, e),
        }
    }
    // base 自带的 arn 也纳入（可能不在任何 list 里，防御性补全）。
    if let Some(a) = base.profile_arn.as_deref() {
        let a = a.trim().to_string();
        if !a.is_empty() && seen.insert(a.clone()) {
            arns.push(a);
        }
    }

    // 逐个验活（顺序探测，避免密集打同族触发风控）。
    let mut out: Vec<ProfileCandidate> = Vec::new();
    for arn in arns {
        let region = KiroCredentials::region_from_profile_arn(&arn)
            .map(|s| s.to_string())
            .unwrap_or_default();
        let account = account_from_arn(&arn);
        let (usable, subscription_title, reason) =
            match probe_profile_usable(base, config, token, proxy, &arn).await {
                ProfileProbeOutcome::Usable { subscription_title } => {
                    (true, subscription_title, "usable")
                }
                ProfileProbeOutcome::FeatureNotSupported => (false, None, "feature_not_supported"),
                ProfileProbeOutcome::Unauthorized => (false, None, "unauthorized"),
                ProfileProbeOutcome::OtherError(_) => (false, None, "error"),
            };
        let current = current_arn == Some(arn.trim());
        out.push(ProfileCandidate {
            arn,
            region,
            account,
            usable,
            subscription_title,
            reason,
            current,
        });
    }
    out.sort_by_key(candidate_rank);
    out
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
    /// 账户级风控（403 `TEMPORARILY_SUSPENDED` 等）**连续无成功**次数。
    ///
    /// ## 为什么不复用 `CooldownManager::trigger_count`
    ///
    /// 曾经就是复用它，结果自动禁用**从未生效过**：`report_success` 调
    /// `clear_cooldown` → `entries.remove()` 把整个冷却条目删掉，`trigger_count`
    /// 随之归零。半死号（偶尔成功一次）因此永远回不到禁用阈值，实测线上 8 个
    /// 成功率恒 0% 的死号跑了几小时仍全部 `disabled=false` / `failureCount=0`，
    /// 每条客户端请求都要在它们身上白撞一遍（最坏 43 次 / 45 秒墙钟）。
    ///
    /// 所以计数必须挂在**凭据条目**上，与冷却条目的生命周期解耦：
    /// - 风控命中 → +1（无论 `cooldown_enabled` 开关如何，见 `report_suspicious_activity`）
    /// - 任意一次成功 → 归零（`report_success`），保证健康号永不误禁
    ///
    /// 非持久化：进程内计数。重启后从 0 开始重新累计，至多多撞几次即再次禁用；
    /// 而**禁用状态本身**是持久化的（`persist_disabled_state`），不会重启复活。
    consecutive_suspicious: u32,
    /// **代挂号专用**：连续「非瞬态」失败次数（任一次成功即归零）。
    ///
    /// # 为什么代挂号需要独立的判据
    ///
    /// 代挂号（`auth_method=custom_api`）是用户自购的第三方 Anthropic 兼容中转站，
    /// 与 Kiro 号的失败语义**根本不同**：
    /// - Kiro 号有**封号风险**，砸多了会真死 → 需要冷却、退避、族级连坐。
    /// - 代挂号只是一个付费上游，429 只代表「它现在忙」，**没有"被风控"这个状态**。
    ///   对它退避是白等（用户已经付过钱），而 failover 到下一个代挂号才是正解。
    ///
    /// 所以 dwgx 定的语义是：**瞬态信号完全不留痕**（429/5xx/超时 → 只 failover，
    /// 不冷却不计数），**只有非瞬态失败才累加本计数**，达阈值即自动禁用。
    ///
    /// # 为什么用「连续 + 成功归零」而不是累计
    ///
    /// 项目实测过死号与好号「分界干净、无过渡态」（8 个死号成功率恒 0%，9 个好号 90~100%），
    /// 因此「连续失败且期间零成功」抓得准。反例是旧的 `cooldown.trigger_count >= 10`：
    /// 它在数学上**不可达**（`report_success` 会删掉整个冷却条目使计数归零），
    /// 导致自动禁用从未生效过。本字段挂在凭据条目上，与冷却条目生命周期解耦。
    ///
    /// 非持久化：进程内计数，重启从 0 累计（至多多撞几次即再次禁用）；
    /// 而**禁用状态本身**持久化，不会重启复活。
    consecutive_passthrough_failures: u32,
    /// **代挂号专用**：进入「持续过载」观察窗的起点（首次 429 且此后零成功的时刻）。
    ///
    /// 用于兼顾两条看似矛盾的要求：「偶尔 429 绝不冷却」与「一直错误就自动禁用」。
    /// 单次乃至一阵 429 完全放行（不冷却不计数）；但若**整个窗口内零成功且一直在 429**
    /// （见 `PASSTHROUGH_OVERLOAD_WINDOW`），说明这个中转站已经不能用了，
    /// 而不是"偶尔忙" → 按独立原因禁用，便于运维区分「key 失效」与「站点持续过载」。
    ///
    /// 任一次成功即清空（`None`），所以只要站点还能出活就永远不会命中。
    passthrough_overload_since: Option<Instant>,
    /// **代挂号专用**：最近一次 429 的时刻，用于判断 429 是否「连续」。
    ///
    /// 没有它就无法区分「窗口内密集 429」与「相隔十分钟的两次 429」——
    /// 后者中间可能整段空闲（零请求，因此也没有成功来重置观察窗），
    /// 不该被判成"持续过载"。见 `record_passthrough_result` 的 RateLimited 分支。
    last_passthrough_429_at: Option<Instant>,
    /// 上次被选号**真正选中**的时刻（`commit_selection` 里更新）。
    ///
    /// 用于 [`STARVATION_PROBE_SECS`] 的反饥饿强制探测。**不能用 `RpmTracker` 代替**：
    /// 它是 60s 滑窗，超窗即无数据，无法区分"61 秒没选中"与"20 分钟没选中"，
    /// 而饥饿自锁的典型时长是分钟级（实测 192 秒）。
    ///
    /// 非持久化：进程内状态。重启后视为"刚被选中"（乐观），至多延迟一个探测窗口。
    last_selected_at: std::cell::Cell<Instant>,
    /// 是否已禁用
    disabled: bool,
    /// 禁用原因（用于区分手动禁用 vs 自动禁用，便于自愈）。**持久化**（见 `persist_credentials`）。
    disabled_reason: Option<DisabledReason>,
    /// 被禁用的时刻（RFC3339）。**持久化**，供运维判断"这号坏了多久"。
    disabled_at: Option<String>,
    /// API 调用成功次数
    success_count: u64,
    /// 累计请求数（用于 `request_limit` 上限自动禁用；主要自定义 API 代挂计数）。
    /// **持久化**进 kiro_stats.json(随 success_count 一起)——请求上限是「终身预算」护栏,
    /// 重启不能清零,否则达上限被禁的号一重启就额度归零、重新可用,防超预算失效。
    request_count: u64,
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
    /// external_idp 号上次 getUsageLimits 是否返回过 403 FEATURE_NOT_SUPPORTED
    /// （该 region 的 profile 未开通）。刷新时据此**只对确认坏的号**触发 reprobe 重选 region，
    /// 健康号不额外探测（省成本）。非持久化：进程内状态，重启后由首次余额查询重新置位。
    last_usage_403_feature_not_supported: AtomicBool,
    /// external_idp 号上次「全 region 探测都没找到可用 profile」的时间戳（成本护栏）。
    ///
    /// 全 region 都坏的号（微软账号在所有候选 region 都未开通 Kiro）：reprobe 每次都白跑
    /// 一整轮 getUsageLimits 探测，而余额环每 ~30min 又会把 `last_usage_403_feature_not_supported`
    /// 重新置位 → 每个 token TTL 都重复全 region 探测，纯浪费上游调用。此处记录上次全坏探测
    /// 时间，`REPROBE_ALL_BAD_COOLDOWN` 冷却期内跳过 reprobe。找到可用 profile 时清空（恢复灵敏）。
    /// 非持久化：进程内成本护栏，重启清零可接受（重启后至多多探一轮）。
    last_full_reprobe_at: Mutex<Option<Instant>>,
    /// 对话路径撞 403 FEATURE_NOT_SUPPORTED 时触发的**后台异步重探**是否在飞(per-id 去重守卫)。
    ///
    /// N 个并发对话请求同撞同一坏号时,只允许 1 个真正 spawn 重探(compare_exchange 抢占),其余直接
    /// failover——防止各起一轮 probe_all_usable_profiles(一整轮 getUsageLimits)打爆上游、自造
    /// suspicious-activity 风控。重探任务结束(成功/失败/panic)由 guard Drop 清回 false。
    /// 非持久化:进程内并发守卫,重启清零可接受。
    reprobe_in_flight: AtomicBool,
    /// 凭据级 Token 刷新锁（per-credential）。
    ///
    /// 替代 MultiTokenManager 上的全局 refresh_lock：N 个凭据并发刷新时，每个凭据只
    /// 串行化自己的刷新，彼此不再互相阻塞（消除"凭据 #1 预刷新挂起时，凭据 #N 的按需
    /// 刷新在全局锁后排队"的队头阻塞问题）。双检守卫（stale-snapshot guard）仍通过
    /// 「拿锁后二次确认 refresh_token 是否已被他人轮换」实现，语义与旧全局锁完全一致。
    refresh_lock: Arc<TokioMutex<()>>,
}

impl CredentialEntry {
    /// 复活凭据（从禁用态恢复）时必须清零的**全部进程内惩罚计数**，单一收口。
    ///
    /// # 为什么要收口成一个方法
    ///
    /// 复活路径有三条（`reset_and_enable` / `set_disabled(false)` / 全池 `TooManyFailures`
    /// 自愈块），此前**各自手写清零列表**，于是三处都漏了同一个字段
    /// `consecutive_suspicious` —— 而它唯一的清零点是 `report_success`。
    ///
    /// 后果：被 `SuspiciousActivityAuto` 禁用的号（计数已达阈值 6）人工「重置并启用」后，
    /// 计数仍是 6，**下一次风控命中即秒禁**，「重置」形同虚设。且自动禁用落盘后
    /// （`persist_disabled_state`）这个秒禁**重启也回不来**，代价从"重启即恢复"
    /// 恶化成"永久死号"。
    ///
    /// 与 `RequestLimitReached` 的 `request_count` 清零是同型 bug（那处已修），
    /// 差别只是当时只想到了一个字段。收口后新增惩罚计数只需改这一个地方。
    ///
    /// # 不在此清零的东西
    ///
    /// `cooldown` / `rate_limiter` 是**独立锁下的旁挂结构**，不属于本 entry，
    /// 由调用方在 entries 锁外单独清（见 `reset_and_enable`）。
    /// `request_count` 只在 `RequestLimitReached` 语义下才该清，由调用方按原因判定。
    fn clear_transient_counters(&mut self) {
        self.failure_count = 0;
        self.refresh_failure_count = 0;
        // 账户级风控计数：漏清它会让复活的号一次风控即秒禁（见方法文档）。
        self.consecutive_suspicious = 0;
    }
}

/// 禁用原因。
///
/// ## 为什么要可序列化（本轮修复的缺陷）
///
/// 此前 `KiroCredentials` **只有 `disabled: bool`**、没有原因字段，`persist_credentials`
/// 也只写 `cred.disabled = e.disabled`。而加载时对所有 `disabled` 号一律回填
/// `Some(DisabledReason::Manual)` —— 于是 `SuspiciousActivityAuto` / `QuotaExceeded` /
/// `AccountSuspended` / `TooManyFailures` 等**自动禁用原因在重启后全部变成「手动禁用」**。
///
/// 后果不只是展示不准：还连带击穿以 reason 为判据的自愈逻辑
/// （把自动禁用误判成人工禁用 → 自愈不敢重新启用 → 整池禁用后永久死锁）。
///
/// `serde` 用 **camelCase 稳定线格式**。
///
/// # ⚠️ 为什么必须有 `#[serde(other)]` 兜底变体（回滚安全）
///
/// 原注释只说了「新增变体时**旧文件**仍可读」——那是**前向**兼容，方向说反了一半。
/// 真正会炸的是**反向**：新版本写进 `credentials.json` 的新变体，**旧版本读不了**。
///
/// 实测坐实（正确复现下）：旧枚举反序列化 `"passthroughFailed"` 报
/// `unknown variant 'passthroughFailed', expected one of ...`。而
/// `CredentialsConfig::load` 失败会让 `main.rs` 直接 `std::process::exit(1)`
/// （那是刻意的 fail-safe：宁可拒绝启动也不用空池覆盖真实凭据）。
/// 于是**回滚到旧二进制 = 服务起不来**，且必须手工编辑 `credentials.json`
/// 删掉那个字段才能恢复 —— 在生产回滚的时间压力下这是最糟的失败形态。
///
/// `#[serde(other)]` 让任何**未知**变体退化成 `Unknown` 而不是解析失败。
/// 它对**当前**版本没有行为影响（我们永远不会主动写出 `Unknown`），
/// 价值全在「未来任一版本都能读其它版本写的文件」——
/// 这是保住零空窗回滚能力的前提。
///
/// ⚠️ 新增变体时无需再动这里，但**绝不要删掉 `Unknown`**。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DisabledReason {
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
    /// 累计请求数达到 `request_limit` 上限后自动禁用（主要用于自定义 API 代挂计费防护）。
    /// 属"自动禁用",重置计数(reset)或人工可重新启用。
    RequestLimitReached,
    /// **代挂号**连续多次「非瞬态」失败后自动禁用（key 失效 / 额度耗尽 / base_url 配错等
    /// 再试无用的信号，阈值 [`MAX_PASSTHROUGH_FAILURES`]）。属"自动禁用"，人工修好配置后可重新启用。
    ///
    /// 与 `TooManyFailures` 分开是因为处置动作完全不同：那条指 Kiro 号（要查是否被风控），
    /// 这条指第三方中转站（要查 key/余额/地址）。运维看到原因就知道该去哪儿排查。
    PassthroughFailed,
    /// **代挂号**在 [`PASSTHROUGH_OVERLOAD_WINDOW`] 内零成功且持续 429 → 判定该中转站已不可用。
    ///
    /// 独立于 `PassthroughFailed`：偶尔 429 是正常的（绝不惩罚），只有"一直 429 且完全出不了活"
    /// 才是站点挂了。分开记录让运维能区分「站点过载」与「凭据本身有问题」。
    PassthroughOverloaded,
    /// 未知原因（**仅**用于反序列化兜底：读到本版本不认识的变体时落这里）。
    ///
    /// 绝不主动写出。存在的唯一目的是让**旧版本能读新版本写的文件**，
    /// 从而不破坏回滚路径（见枚举文档）。
    #[serde(other)]
    #[default]
    Unknown,
}

impl DisabledReason {
    /// 稳定的字符串枚举名，供 Admin API 下发、前端按它做 i18n 映射。
    ///
    /// 单一收口：此前 `snapshot()` 内联了一份 match，回收站要展示原因时若各写一份，
    /// 新增变体就得改 N 处且漏一处只会静默少个标签。改动这些字面量等于改 API 契约
    /// （前端 `lib/i18n-labels.ts` 按同名键查表），务必与前端同步。
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Manual => "Manual",
            Self::TooManyFailures => "TooManyFailures",
            Self::TooManyRefreshFailures => "TooManyRefreshFailures",
            Self::QuotaExceeded => "QuotaExceeded",
            Self::AccountSuspended => "AccountSuspended",
            Self::SuspiciousActivityAuto => "SuspiciousActivityAuto",
            Self::InvalidRefreshToken => "InvalidRefreshToken",
            Self::InvalidConfig => "InvalidConfig",
            Self::RequestLimitReached => "RequestLimitReached",
            Self::PassthroughFailed => "PassthroughFailed",
            Self::PassthroughOverloaded => "PassthroughOverloaded",
            // 读到本版本不认识的变体（例如回滚后读新版写的文件）时落这里。
            Self::Unknown => "Unknown",
        }
    }
}

/// 该禁用原因是否属于**可自愈**（全池被自动禁用时允许一次性复活重试）。
///
/// # 判据
///
/// 只包含**瞬时/可恢复**的自动禁用原因：
/// - `TooManyFailures`：连续失败达阈值。失败可能是上游抖动，值得再试。
/// - `SuspiciousActivityAuto`：403 账户级风控。**这是整池瞬时风控**，
///   历史事故明确记录 403 `TEMPORARILY_SUSPENDED` 是**临时态**
///   （曾被当永久封禁处理 → 12h 内 88 次误禁 + 36 次全池自愈活锁 → 拒绝率升到 100%）。
///
/// # 刻意排除的（复活只会白撞，且掩盖真实问题）
///
/// - `AccountSuspended`：真被封，需人工处理（有专门的回归测试锁住这条）
/// - `QuotaExceeded`：额度耗尽，要等下个计费周期
/// - `InvalidRefreshToken` / `InvalidConfig`：凭据本身坏了
/// - `RequestLimitReached`：终身预算护栏，复活等于绕过它
/// - `PassthroughFailed` / `PassthroughOverloaded`：第三方中转站的 key/额度/站点问题
/// - `Manual`：人工禁用，绝不能被自动复活
/// - `Unknown`：读不懂的原因，保守不动
fn is_self_healable_reason(reason: Option<DisabledReason>) -> bool {
    matches!(
        reason,
        Some(DisabledReason::TooManyFailures) | Some(DisabledReason::SuspiciousActivityAuto)
    )
}

/// 统计数据持久化条目
#[derive(Serialize, Deserialize)]
struct StatsEntry {
    success_count: u64,
    /// 生命周期累计 credit 花费。向后兼容：老 stats 文件无此字段时默认 0。
    #[serde(default)]
    total_credits_used: f64,
    /// 累计请求数(request_limit 终身预算计数)。向后兼容：老 stats 文件无此字段时默认 0。
    #[serde(default)]
    request_count: u64,
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
    /// 凭据级「允许模型」白名单（None/空=不限制）
    pub allowed_models: Option<Vec<String>>,
    /// 「测试可用模型」历史结果（探测打的标签）
    pub tested_models: Option<Vec<crate::kiro::model::credentials::TestedModel>>,
    /// 是否被禁用
    pub disabled: bool,
    /// 连续失败次数
    pub failure_count: u32,
    /// 认证方式
    pub auth_method: Option<String>,
    /// 自定义 API 代挂:上游 base_url(展示用,api_key 绝不下发)
    pub base_url: Option<String>,
    /// 自定义 API 代挂:请求上限(None/0=不限)
    pub request_limit: Option<u64>,
    /// 自定义 API 代挂:累计已发请求数
    pub request_count: u64,
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
    /// 被禁用的时刻（RFC3339）。与 `disabled_reason` 是一对：运维靠它判断"这号坏了多久"，
    /// 从而区分「刚坏，可能只是瞬时风控」与「坏了很久，基本可以确认要换号」。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disabled_at: Option<String>,
    /// 端点名称（未显式配置时返回 None，由 Admin 层回退到默认值）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    /// **实际生效**的端点名（显式配置 > 按凭据类型自动路由 > 全局默认，恒有值）。
    ///
    /// 与 [`Self::endpoint`] 的区别正是"是否被人工固定"：`endpoint=None` 且
    /// `effective_endpoint="cli"` 表示这是 `ksk_` 号被自动路由的结果。
    pub effective_endpoint: String,
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
    /// 删除前的禁用原因（`None` = 老回收站数据或手动删除未记录）。
    ///
    /// 用户要求「认定封号必须标明原因」，而号被判死后往往紧接着就被删除——回收站不带原因时，
    /// 恰恰在最需要它的时刻（判断该换号还是该申诉）信息就丢了。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disabled_reason: Option<DisabledReason>,
    /// 删除前被禁用的时刻（RFC3339）。用于区分「刚坏就删」与「坏了很久才删」。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disabled_at: Option<String>,
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
    /// 下一个待分配的凭据 ID（进程内单调递增计数器，永不回退、永不复用）。
    ///
    /// 【为何不用 `max(entries ∪ trash).id + 1`】旧算法在「删号 → 从回收站彻底清除(purge)
    /// → 再加新号」时，`max+1` 会**回落到刚被清除的号的 id**，于是新号复用了旧号的 id。
    /// 而 cooldown / rpm / model_blocklist 这些 per-id 内存态在删号时并不随号消失（HashMap<u64,_>
    /// 里旧条目还在），复用 id 的全新健康号就会**静默继承死号的冷却/模型黑名单**，被选号跳过
    /// 直到旧冷却到期——低概率但真实的正确性地雷，且随将来新增 per-id 表而放大。
    ///
    /// 单调计数器让「id 永不复用」由构造保证：不管现在/将来有几张 per-id 表，新号都拿全新 id，
    /// 结构上不可能撞上任何遗留内存态。启动时初始化为 `max(entries ∪ trash).id + 1`，之后每次
    /// 分配只 `fetch_add(1)`。restore(按原 id 恢复) 恒复用 < 计数器的旧 id，不与新号冲突；
    /// 重启后内存态(cooldown/rpm/...)本就全空，计数器从持久化的 max 重新起算，一致且安全。
    next_id: AtomicU64,
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
    /// 入站请求整形 + RPM 自动挡（主动：入口削平突发,让号不被上游打爆;治 429 雪崩）
    throttle: std::sync::Arc<crate::kiro::throttle::GlobalThrottle>,
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
    /// RPM headroom 系数(整百分比 0..100;85=预留15%)。饱和阈值 = base × factor/100。（原子镜像,reload 热更）
    rpm_headroom_factor: AtomicU32,
    /// RPM 预留名额(headroom 折扣后再扣 N)。（原子镜像,reload 热更）
    rpm_reserve_slots: AtomicU32,
    /// 整池 RPM 饱和时是否走背压等待(默认 false=回退软门)。（原子镜像,reload 热更）
    rpm_hard_gate_overload_wait: AtomicBool,
    /// 全池冷却时是否快速失败（立即返回 429+Retry-After 让客户端退避，而非网关内硬扛）。（原子镜像,reload 热更）
    all_cooling_fast_fail: AtomicBool,
    /// 是否在凭据持续可疑活动风控(trigger_count 达阈值)时自动禁用它。（原子镜像,reload 热更）
    auto_disable_suspicious: AtomicBool,
    /// 均衡模式下是否叠加优先级分发（原子镜像,reload 热更）。
    priority_in_balanced: AtomicBool,
    /// 余额加权分流开关（原子镜像,reload 热更）。true=同档内按剩余额度微调选号评分。
    balance_weight_enabled: AtomicBool,
    /// 余额加权 FLOOR(整百分比 0..100;50=因子下限 0.5)。（原子镜像,reload 热更）
    balance_weight_floor: AtomicU32,
    /// 余额快照(每 30 分钟由 AdminService 余额刷新任务回推)。key=cred id。
    /// balanced 选号时 balance_factor 用它 + 本地实时 total_credits_used 累加修正估当前剩余。
    /// 读多写少(30 分钟写一次,每次选号读),用 RwLock。缺表(新号/未刷)= 中性因子 1.0 不惩罚。
    balance_snapshots: RwLock<HashMap<u64, BalanceSnapshot>>,
    /// 主动 token 预刷新后台任务句柄（TIER2 热重载：改配置后 abort + respawn 即时生效不重启）。
    /// None = 当前未运行（proactive_token_refresh=false 或尚未启动）。
    refresh_task: Mutex<Option<JoinHandle<()>>>,
}

/// 单号余额快照(供余额加权分流)。由 AdminService 每 30 分钟刷新后回推。
#[derive(Debug, Clone, Copy)]
pub struct BalanceSnapshot {
    /// 快照时刻的剩余额度(overage 感知:含 overage cap)。
    pub remaining_at_cache: f64,
    /// 有效额度上限(base + overage cap),用于归一成剩余比例。<=0 时因子回退中性。
    pub effective_limit: f64,
    /// 快照时刻该号的 total_credits_used(本地累加修正基线:当前用量 - 此基线 = 快照后新增花费)。
    pub credits_used_at_cache: f64,
}

/// 反饥饿强制探测窗口（秒）：任何**可选**凭据超过这么久没被选中，
/// 下一轮选号无条件排到最前，强制给它一次探测机会。
///
/// ## 这一条是结构性兜底，不是修某个具体 bug
///
/// 本仓已实测过两次"单向状态无法自行恢复 → 号被永久排除"的缺陷：
/// ① `ewma_429`/`ewma_success` 只在成功时更新 → 拿不到请求就永不回升
///    （实测 6 号池 4 个进 T2 坏档、`rpm=0 inflight=0` 空转，有效容量 6→3，全程零 429）；
/// ② `open_count`/`admit_prob_seed` 只在"半开内连续 5 次成功"才恢复 →
///    seed 收缩到 0.02 后拿不到那 2% 试探 → 凑不齐成功 → 永久化。
/// 两者都已加时间衰减修掉，但**靠逐个字段审查保证"每个惩罚状态都有下降路径"已证明不可靠**
/// （上一版的衰减修复本身就带着测试上线、却因时钟被读路径刷新而完全失效）。
///
/// 本探测的价值在于：**它不依赖任何具体字段是否有衰减路径**。
/// 即使将来又引入一个新的单向惩罚状态，最坏后果也只是"偶尔损失一个探测请求"，
/// 而不是号池有效容量不可逆缩水。
///
/// ## 为什么是 180 秒
///
/// 实测饥饿自锁的观测时长是 192 秒（#211 连续零请求）。取 180s 略小于它，
/// 保证同类情形能被兜住；同时远大于正常轮转间隔（满负载下每号每秒都可能被选中），
/// 所以健康池里这条键**恒不生效**，零性能与零行为影响。
///
/// ## 探测不绕过任何硬门
///
/// 排序键里它排在 `unusable` **之后**：真不可用的号（熔断 Open → p_avail=0、
/// RPM 已饱和）仍然沉底。探测只在"本来就可选、只是被健康分档压住"时起作用。
const STARVATION_PROBE_SECS: u64 = 180;

/// 每个凭据最大 API 调用失败次数
const MAX_FAILURES_PER_CREDENTIAL: u32 = 3;
/// 账户级风控（403 `TEMPORARILY_SUSPENDED` 等）**连续无成功**多少次后自动禁用该号。
///
/// ## 为什么需要独立于 `MAX_FAILURES_PER_CREDENTIAL`
///
/// 403 风控是**临时**态（上游原文 temporarily + 附申诉链接），历史上曾被当永久封禁
/// 处理并造成生产事故（12 小时 88 次误禁 + 逐小时拒绝率升到 100%，见
/// `endpoint/mod.rs::default_is_account_suspended` 的说明）。所以判据**不能是"见过
/// 403"**，而必须是"连续 403 且期间一次都没成功过"——后者才能区分：
///
/// - 真死号：实测成功率恒 **0%**（线上 8 个号 n=4~48 全部 403，无一次成功）
/// - 健康号：成功率 90~100%，但同样会**偶发**命中 403
///
/// 取 6 而非 3：403 常伴随同出口 IP 的整池瞬时风控，健康号也可能连吃 2~3 次。
/// 6 次连续零成功足以把"真死"与"偶发"分开，而任意一次成功都会清零（见
/// `report_success`），所以健康号永远到不了 6。
const MAX_CONSECUTIVE_SUSPICIOUS_BEFORE_DISABLE: u32 = 6;
/// 凭据自定义字段的服务端防呆上界(不信任前端校验,直打 admin API 的越界值也自动修补)。
/// priority:优先级(越小越优先),上界够大覆盖任意分层又防 u32 极值污染排序。
const MAX_PRIORITY: u32 = 9999;
/// rpm_limit:单号 RPM 软上限,上界远超真实吞吐(防 u32 极值,0 另归一为 None=继承全局)。
const MAX_RPM_LIMIT: u32 = 100_000;
/// name:自定义别名/备注最大字符数(与前端 maxLength 一致,按 char 截断防切坏多字节)。
const MAX_NAME_CHARS: usize = 64;
/// 所有号只是临时冷却/限流（会自动恢复）时，单次选号最多在网关内等待多久再放弃。
/// 避免瞬时全忙就立刻返回“所有凭据均已禁用”；但也不能太长——否则一个请求的一次
/// 选号就阻塞数分钟，叠加上层重试会反复扫冷全池（雪崩）。取 20s：够扛过一次
/// burst 软限流的自愈，又不至于让单请求长期霸占等待。上层 provider 另有 45s
/// 墙钟总预算兜底。
const MAX_TRANSIENT_WAIT_SECS: u64 = 20;
/// 统计数据持久化防抖间隔
const STATS_SAVE_DEBOUNCE: StdDuration = StdDuration::from_secs(30);
/// 池子真耗尽（available == 0）时回给客户端的建议退避秒数。
///
/// 为什么"真耗尽"也需要退避秒数而不是当永久故障：这个状态**会自愈**。
/// `is_self_healable_reason` 覆盖的原因（`TooManyFailures` / `SuspiciousActivityAuto` 等）
/// 会触发全池自愈"重置失败计数并重新启用"，线上实测 41 分钟内触发 36 次
/// （≈68s 一次）。而 403 `TEMPORARILY_SUSPENDED` 本身也是限时态。
///
/// 取 10s 的理由：与自愈触发间隔同量级但明显更短，让客户端在池子恢复后尽快回来；
/// 又足够长到不等于"不退避"。⚠️ 这是个**未经控制实验的可调参数**，不是实测结论——
/// 若要精确化，需要按"耗尽窗口时长分布"来定，而那需要先积累样本。
const POOL_EXHAUSTED_RETRY_AFTER_SECS: u64 = 10;

/// 全池无立即可用候选时,一个候选为何在等待——决定调用方终态处理与文案类别。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WaitReason {
    /// 冷却/风控/速率限制:典型秒~分钟级,长时应 fast-fail 让客户端退避。
    Cooling,
    /// RPM 饱和,滑窗过期即恢复(L4 背压):属"限流/繁忙"可重试类别,绝不报"已禁用"。
    RpmRecovery,
}

/// select 返 None 时的等待判定结果(见 transient_wait_outcome)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WaitOutcome {
    /// 无任何可用候选(全禁用/被硬门过滤)→ 终态报"已禁用"。
    NoCandidate,
    /// 存在立即可用候选(select 却返 None,竞态)→ 应重选,绝不 bail/等待。
    Available,
    /// 所有候选都在等待恢复:最短等待 + 原因。
    Wait(StdDuration, WaitReason),
}

/// `load_balancing_mode` 归一化后的生效调度语义（见 `effective_scheduling`）。
///
/// 存在的意义：把"配置字符串 → 实际调度行为"的映射收敛到**一处**。历史上 `priority` 与
/// `balanced` 是两套并列实现，其中 priority 那套缺失全部保护且无人测试覆盖，
/// 归一化后只剩一套排序键，差异只体现为本结构体的字段。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SchedulingSemantics {
    /// 是否按 priority **分层**分发（层内仍按健康/负载均衡，整层打爆才溢出到下一层）。
    priority_layered: bool,
}

/// 模型级不支持黑名单的 TTL：某号对某模型返回 INVALID_MODEL_ID 后，这段时间内选号跳过
/// "该号+该模型"组合。取中长窗（订阅权益变化是较慢的事），到期后自动允许重试探。
const MODEL_BLOCK_TTL: StdDuration = StdDuration::from_secs(1800);

/// 代挂号（`custom_api`）连续「非瞬态」失败多少次后自动禁用。
///
/// 与 Kiro 侧 `MAX_FAILURES_PER_CREDENTIAL` 取同一量级（少一个需要记的概念）。
/// 非瞬态 = 401/403（key 失效）、402（额度耗尽）、404（base_url 错）等**再试也一样**的信号：
/// 等 N 次纯属浪费，3 次足够排除「一次性抖动」。瞬态信号（429/5xx/超时）**不进这个计数**。
const MAX_PASSTHROUGH_FAILURES: u32 = 3;

/// 代挂号「持续过载」判定窗口：窗口内**零成功且一直 429** 才判定该站不可用。
///
/// 存在理由是同时满足两条要求：「偶尔 429 绝不冷却/不惩罚」+「一直错误就自动禁用」。
/// 单次或一阵 429 完全放行；只有连续 5 分钟一次都没成功过，才认定不是"偶尔忙"。
/// 任一次成功即清空计时器，所以还能出活的站永远不会命中。
const PASSTHROUGH_OVERLOAD_WINDOW: StdDuration = StdDuration::from_secs(300);

// 原子写 + 权限收紧已提取为共享单一真相源 `common::fs_atomic`(供 config.rs 等复用,
// 并补了 Windows 句柄占用的 rename 重试)。此处 re-import 保持调用点不变。
use crate::common::fs_atomic::write_atomic;

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
                    consecutive_suspicious: 0,
                    consecutive_passthrough_failures: 0,
                    passthrough_overload_since: None,
                    last_passthrough_429_at: None,
                    last_selected_at: std::cell::Cell::new(Instant::now()),
                    disabled: cred.disabled, // 从配置文件读取 disabled 状态
                    // ⭐ 优先用持久化的真实原因；只有旧文件（无该字段）才回落 Manual。
                    // 旧代码无条件回落 Manual，导致自动禁用原因重启即丢失（见 DisabledReason 说明）。
                    disabled_reason: if cred.disabled {
                        Some(cred.disabled_reason.unwrap_or(DisabledReason::Manual))
                    } else {
                        None
                    },
                    disabled_at: cred.disabled_at.clone(),
                    success_count: 0,
                    request_count: 0,
                    total_credits_used: 0.0,
                    last_used_at: None,
                    inflight: Arc::new(AtomicU32::new(0)),
                    last_usage_403_feature_not_supported: AtomicBool::new(false),
                    last_full_reprobe_at: Mutex::new(None),
                    reprobe_in_flight: AtomicBool::new(false),
                    refresh_lock: Arc::new(TokioMutex::new(())),
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
                entry.disabled_at = Some(Utc::now().to_rfc3339());
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
        let cooldown_scale_pct = config.cooldown_scale_pct;
        let throttle = std::sync::Arc::new(crate::kiro::throttle::GlobalThrottle::new(
            config.inbound_throttle_enabled,
            config.inbound_rpm_auto,
            config.inbound_target_rpm,
            config.inbound_rpm_min,
            config.inbound_rpm_max,
            config.inbound_burst_secs,
            config.inbound_queue_max_wait_secs,
            config.inbound_queue_timeout_passthrough,
        ));
        let rpm_limit = config.credential_rpm_limit;
        let rpm_headroom_factor = config.rpm_headroom_factor;
        let rpm_reserve_slots = config.rpm_reserve_slots;
        let rpm_hard_gate_overload_wait = config.rpm_hard_gate_overload_wait;
        let all_cooling_fast_fail = config.all_cooling_fast_fail;
        let auto_disable_suspicious = config.auto_disable_suspicious;
        let priority_in_balanced = config.priority_in_balanced;
        let balance_weight_enabled = config.balance_weight_enabled;
        let balance_weight_floor = config.balance_weight_floor;
        let health_429_weight_enabled = config.health_429_weight_enabled;
        let rate_limit_config = RateLimitConfig {
            daily_max_requests: config.rate_limit_daily_max,
            min_interval_ms: config.rate_limit_min_interval_ms,
            // 抖动百分比:config 的 0..50 整数 → 0.0..0.5 小数,喂给已有的 jitter 机制(拟人节奏)。
            jitter_percent: (config.rate_limit_jitter_pct.min(50) as f64) / 100.0,
            ..RateLimitConfig::default()
        };
        let manager = Self {
            config: ArcSwap::from_pointee(config),
            proxy,
            entries: Mutex::new(entries),
            trash: Mutex::new(Vec::new()),
            current_id: Mutex::new(initial_id),
            // 计数器起点 = 现有 entries 的 max id + 1（local next_id 已含 id-less 补全后的值）。
            // 回收站(trash)此刻尚未加载，其可能更高的 id 在下方 load_trash() 后再 reconcile。
            next_id: AtomicU64::new(next_id),
            credentials_path,
            is_multiple_format,
            load_balancing_mode: Mutex::new(load_balancing_mode),
            last_stats_save_at: Mutex::new(None),
            stats_dirty: AtomicBool::new(false),
            cooldown: CooldownManager::new(),
            cooldown_enabled: AtomicBool::new(cooldown_enabled),
            throttle,
            rate_limiter: RateLimiter::new(rate_limit_config),
            rate_limit_enabled: AtomicBool::new(rate_limit_enabled),
            affinity: UserAffinityManager::new(),
            model_blocklist: Mutex::new(HashMap::new()),
            affinity_enabled: AtomicBool::new(affinity_enabled),
            rpm: RpmTracker::new(),
            health: {
                let h = HealthTracker::new();
                // 429 降权开关:config 存"是否启用降权",HealthTracker 存"是否关闭降权",取反装配。
                h.set_disable_429_weight(!health_429_weight_enabled);
                h
            },
            rpm_limit: AtomicU32::new(rpm_limit),
            rpm_headroom_factor: AtomicU32::new(rpm_headroom_factor),
            rpm_reserve_slots: AtomicU32::new(rpm_reserve_slots),
            rpm_hard_gate_overload_wait: AtomicBool::new(rpm_hard_gate_overload_wait),
            all_cooling_fast_fail: AtomicBool::new(all_cooling_fast_fail),
            auto_disable_suspicious: AtomicBool::new(auto_disable_suspicious),
            priority_in_balanced: AtomicBool::new(priority_in_balanced),
            balance_weight_enabled: AtomicBool::new(balance_weight_enabled),
            balance_weight_floor: AtomicU32::new(balance_weight_floor),
            balance_snapshots: RwLock::new(HashMap::new()),
            refresh_task: Mutex::new(None),
        };
        // 播种冷却时长缩放(启动即用 config 值)。
        manager.cooldown.set_cooldown_scale_pct(cooldown_scale_pct);

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

        // reconcile id 计数器：回收站里的号可能有比现存 entries 更高的 id（删了高 id 号后
        // 该号进 trash）。计数器必须 ≥ max(entries ∪ trash) + 1，否则从 trash 恢复的高 id 号
        // 会与后续新分配的 id 撞号。取当前值与 trash max+1 的较大者，单调只增不减。
        {
            let trash = manager.trash.lock();
            if let Some(max_trash) = trash.iter().filter_map(|t| t.credentials.id).max() {
                // fetch_max：仅当 trash max+1 更大时才抬高，保持单调。
                manager.next_id.fetch_max(max_trash + 1, Ordering::AcqRel);
            }
        }

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
        let mut new = Config::load(&path)?; // 解析失败 → return Err,不动任何状态
        // ⚠️【proxy split-brain 根治】restart-only 固化项(proxy/tls/端口/host/callback/adminkey 等)
        // 在启动时已固化进运行态:KiroProvider.global_proxy 由 new() 一次性赋值,对话/token刷新路径
        // 全程用它;而登录流(social/idc/external_idp)却**活读 config().proxy_url**。
        // 若 reload 把磁盘上的新 proxy 换进 ArcSwap(哪怕只是因为同批改了热字段而顺带 reload),
        // 登录流立刻走新 proxy、对话流仍走启动旧 proxy = split-brain,持续到重启。
        // 修法:reload 只热更运行时字段,把 restart-only 字段用**当前 ArcSwap 里的旧值**覆盖回 new,
        // 使 ArcSwap 的这些字段永远 == 启动固化值,与对话路径全局一致(改这些要生效仍靠重启)。
        {
            let old = self.config.load();
            new.proxy_url = old.proxy_url.clone();
            new.proxy_username = old.proxy_username.clone();
            new.proxy_password = old.proxy_password.clone();
            new.tls_backend = old.tls_backend.clone();
            new.host = old.host.clone();
            new.port = old.port;
            new.region = old.region.clone();
            new.callback_base_url = old.callback_base_url.clone();
            new.admin_api_key = old.admin_api_key.clone();
            new.api_key = old.api_key.clone();
        }
        // 刷新热路径原子镜像
        self.cooldown_enabled
            .store(new.cooldown_enabled, Ordering::Relaxed);
        self.rate_limit_enabled
            .store(new.rate_limit_enabled, Ordering::Relaxed);
        self.affinity_enabled
            .store(new.affinity_enabled, Ordering::Relaxed);
        self.rpm_limit
            .store(new.credential_rpm_limit, Ordering::Relaxed);
        self.rpm_headroom_factor
            .store(new.rpm_headroom_factor, Ordering::Relaxed);
        self.rpm_reserve_slots
            .store(new.rpm_reserve_slots, Ordering::Relaxed);
        self.rpm_hard_gate_overload_wait
            .store(new.rpm_hard_gate_overload_wait, Ordering::Relaxed);
        self.all_cooling_fast_fail
            .store(new.all_cooling_fast_fail, Ordering::Relaxed);
        self.auto_disable_suspicious
            .store(new.auto_disable_suspicious, Ordering::Relaxed);
        self.priority_in_balanced
            .store(new.priority_in_balanced, Ordering::Relaxed);
        self.balance_weight_enabled
            .store(new.balance_weight_enabled, Ordering::Relaxed);
        self.balance_weight_floor
            .store(new.balance_weight_floor, Ordering::Relaxed);
        // 429 降权:config 存"启用",HealthTracker 存"关闭",取反。
        self.health
            .set_disable_429_weight(!new.health_429_weight_enabled);
        *self.load_balancing_mode.lock() = new.load_balancing_mode.clone();
        self.rate_limiter.update_config(RateLimitConfig {
            daily_max_requests: new.rate_limit_daily_max,
            min_interval_ms: new.rate_limit_min_interval_ms,
            jitter_percent: (new.rate_limit_jitter_pct.min(50) as f64) / 100.0,
            ..RateLimitConfig::default()
        });
        // 冷却时长缩放热更(即时生效)。
        self.cooldown.set_cooldown_scale_pct(new.cooldown_scale_pct);
        // 入站整形热更。
        self.throttle.update(
            new.inbound_throttle_enabled,
            new.inbound_rpm_auto,
            new.inbound_target_rpm,
            new.inbound_rpm_min,
            new.inbound_rpm_max,
            new.inbound_burst_secs,
            new.inbound_queue_max_wait_secs,
            new.inbound_queue_timeout_passthrough,
        );
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

    /// 池中是否存在「自定义 API」凭据（未禁用）。供分流快速判断:无则直接走 Kiro 路径零开销。
    pub fn has_custom_api_credential(&self) -> bool {
        self.entries
            .lock()
            .iter()
            .any(|e| !e.disabled && e.credentials.is_custom_api_credential())
    }

    /// 跨池仲裁：本次请求**是否应该先尝试 custom_api 透传**。
    ///
    /// 这是修正「用户设的 priority 在跨池维度完全无效」的关键收敛点。
    ///
    /// ## 背景（历史缺陷）
    /// 分派顺序被写死在 handlers 里：一进来就先 `try_custom_api_passthrough`，只有它返回
    /// `None`（代挂池全部冷却/失败）才落 Kiro 主路径。而 `select_custom_api` 又只在代挂号
    /// **子集内**比较 priority。两者叠加的结果是：**custom_api 隐含享有绝对最高优先级**，
    /// 哪怕 Kiro 号 priority=0、代挂号 priority=99 也永远先走代挂 —— 与「priority 越小越优先」
    /// 的产品直觉直接冲突（用户实测反馈："优先级设置了 kiro 更小，还是会优先调度上游 apikey"）。
    ///
    /// ## 现在的语义
    /// 逐个候选代挂号看它的**生效开关**（凭据级 `custom_api_first` 覆盖全局 `config.custom_api_first`）：
    /// - 任一可用代挂号显式 `first=true` → 先走透传（保留历史行为，供"就是要中转兜底在前"的部署）。
    /// - 否则（默认 `false`）→ 取两池各自的**最优 priority** 比较：
    ///   代挂池最优 `<=` Kiro 池最优时才先走透传；Kiro 更优则先走 Kiro，
    ///   Kiro 全失败后 provider 的 failover 仍会落回代挂池（不损失兜底能力）。
    ///
    /// 用 `<=` 而非 `<`：priority 相同时维持"代挂在前"的既有习惯，避免纯升级场景行为突变。
    ///
    /// 注意这里只做**一次性的路径选择**，不改动两池各自的选号逻辑，
    /// 因此「两池隔离铁律」（Kiro 选号永不返回 custom_api、透传结果永不进 health/family 连坐）完全不变。
    pub fn should_try_custom_api_first(&self) -> bool {
        let global_first = self.config.load().custom_api_first;
        let cooldown_on = self.cooldown_enabled.load(Ordering::Relaxed);
        let entries = self.entries.lock();

        let mut best_custom: Option<u32> = None;
        let mut best_kiro: Option<u32> = None;

        for e in entries.iter() {
            if e.disabled {
                continue;
            }
            let is_custom = e.credentials.is_custom_api_credential();
            // 冷却中的号不参与本次仲裁（它此刻选不出来，不该影响路径决策）。
            if cooldown_on && !self.cooldown.is_available(e.id) {
                continue;
            }
            if is_custom {
                // 凭据级开关优先于全局；任一可用代挂号要求"无条件优先"即立刻先走透传。
                if e.credentials.custom_api_first.unwrap_or(global_first) {
                    return true;
                }
                best_custom = Some(best_custom.map_or(e.credentials.priority, |p: u32| {
                    p.min(e.credentials.priority)
                }));
            } else {
                best_kiro = Some(best_kiro.map_or(e.credentials.priority, |p: u32| {
                    p.min(e.credentials.priority)
                }));
            }
        }

        match (best_custom, best_kiro) {
            // 没有可用代挂号 → 无需尝试透传（也省掉一次无谓的 select_custom_api）。
            (None, _) => false,
            // 有代挂号但没有可用 Kiro 号 → 只能走透传。
            (Some(_), None) => true,
            // 两池都有 → 比 priority，代挂不劣于 Kiro 才先走透传。
            (Some(c), Some(k)) => c <= k,
        }
    }

    /// 为透传选一个可用的「自定义 API」凭据(独立于 Kiro 选号池,守两池隔离铁律)。
    ///
    /// 选号素质对齐 Kiro 的 balanced,但只在 custom_api 池内:
    /// ① **优先级**(priority 小先)——你用它控制哪个中转站优先/当备份;
    /// ② 同优先级 **按近 60s RPM 均衡分流**(RPM 最低的先,让多个同级 API 号轮流用,不再只压第一个);
    /// ③ 再按在途细分兜底。
    /// 跳过:禁用 / 冷却中(failover 时失败号被设了短冷却会被自动跳过)/ `exclude` 内(本请求已试过的号)。
    /// 命中返回 (id, credentials) 供透传;无可用返回 None → 调用方 failover 完毕落 Kiro 主力路径。
    /// 与 Kiro 的 is_entry_selectable 彻底分离——Kiro 选号已排除 custom_api,此处只管 custom_api。
    pub fn select_custom_api(
        &self,
        exclude: &std::collections::HashSet<u64>,
    ) -> Option<(u64, KiroCredentials)> {
        let entries = self.entries.lock();
        let cooldown_on = self.cooldown_enabled.load(Ordering::Relaxed);
        entries
            .iter()
            .filter(|e| {
                !e.disabled
                    && e.credentials.is_custom_api_credential()
                    && !exclude.contains(&e.id)
                    && (!cooldown_on || self.cooldown.is_available(e.id))
            })
            // 均衡分流键(升序):优先级 → 近 60s RPM → 在途。rpm 用独立 mutex,与 Kiro balanced 同款模式。
            .min_by_key(|e| {
                (
                    e.credentials.priority,
                    self.rpm.count(e.id),
                    e.inflight.load(Ordering::Acquire),
                )
            })
            .map(|e| (e.id, e.credentials.clone()))
    }

    /// 给 custom_api 透传号设一段冷却(**仅操作 cooldown,不碰 health/family/report_success/failure**,
    /// 守两池隔离铁律)。供透传 failover:某号 403 额度满 / 401 key 失效 / 429 / 5xx 时暂时跳过它,
    /// 让 select_custom_api 下次(及本请求循环 exclude)避开,换下一个号。
    pub fn cooldown_custom_api(&self, id: u64, secs: u64) {
        if self.cooldown_enabled.load(Ordering::Relaxed) {
            self.cooldown.set_cooldown_with_duration(
                id,
                CooldownReason::RateLimitExceeded,
                Some(std::time::Duration::from_secs(secs)),
            );
        }
    }

    /// 记录一次自定义 API 透传的**结果**(dwgx 定:只计成功口径)。
    ///
    /// 与 Kiro 主路径的 [`report_success`](Self::report_success)/[`report_failure`](Self::report_failure)
    /// **彻底隔离**:透传号是独立选号池(`select_custom_api`),绝不能触碰
    /// cooldown / rate_limiter / health(family_key 连坐)/ auto-disable —— 那些会误冷却透传号、
    /// 甚至连坐真 Kiro 号。这里只做轻量计数 + 速率环记录,供号池可视化(流动/成功失败/RPM)与
    /// 用量展示用。
    ///
    /// 三态计数(据上游 outcome 决定,dwgx 定的口径):
    /// - `Success`(2xx):`success_count += 1` + `request_count += 1`(只计成功口径,计入请求上限,
    ///   达上限自动禁用防超预算)。
    /// - `ServerError`/`NetworkError`(5xx/连接错误):`failure_count += 1`(供展示号"不健康"),
    ///   **不**计 request_count、**不**禁用(透传失败多为上游临时问题,客户端自行重试/退避)。
    /// - 其余(429 RateLimited / 401·403 AuthFailed / 4xx BadRequest 等):**既不计成功也不计失败**
    ///   ——透传给客户端由其处理,不误判号健康(dwgx:4xx/429 不计号失败)。
    ///
    /// 三态都会 `rpm.record` + 更新 `last_used_at`,让号池状态条/发光网格能"流动"(反映真实活跃)。
    pub fn record_passthrough_result(&self, id: u64, outcome: crate::usage::RequestOutcome) {
        use crate::usage::RequestOutcome as RO;
        // 速率环:任何结果都记一次活跃,驱动可视化流动(rpm 用独立 mutex,与 entries 无关)。
        self.rpm.record(id);
        // ⚠️ entries 锁必须在内层块内释放,再调 save_stats/persist_credentials ——
        //    两者都会二次加 entries 锁(parking_lot 非可重入),同函数持锁调用即同线程自死锁。
        //    照 report_success/add_credits 同款范式:块结束释放锁后再落盘。
        let mut limit_just_reached = false;
        // 自动禁用(非请求上限那条)也必须立即落盘：禁用状态是持久化字段，
        // 若只改内存则重启后死站以 enabled 回池，重新走一遍失败流程（#13 同型）。
        let mut disable_just_happened = false;
        {
            let mut entries = self.entries.lock();
            if let Some(e) = entries.iter_mut().find(|e| e.id == id) {
                e.last_used_at = Some(Utc::now().to_rfc3339());
                match outcome {
                    RO::Success => {
                        // 成功即清空两个「持续坏」判据：健康号永不误禁（与 report_success 同款保证）。
                        e.consecutive_passthrough_failures = 0;
                        e.passthrough_overload_since = None;
                        e.last_passthrough_429_at = None;
                        e.success_count = e.success_count.saturating_add(1);
                        e.request_count = e.request_count.saturating_add(1);
                        if let Some(limit) = e.credentials.request_limit {
                            if limit > 0 && e.request_count >= limit && !e.disabled {
                                e.disabled = true;
                                e.disabled_reason = Some(DisabledReason::RequestLimitReached);
                                e.disabled_at = Some(Utc::now().to_rfc3339());
                                limit_just_reached = true;
                                tracing::warn!(
                                    credential_id = id,
                                    count = e.request_count,
                                    limit,
                                    "自定义 API 凭据已达请求上限,自动禁用"
                                );
                            }
                        }
                    }
                    // 🟢 **瞬态**失败:5xx / 连接错误 / 超时。仅计数供展示,**绝不**进
                    // consecutive_passthrough_failures、绝不 auto-disable。
                    // 中转站抖一下不代表它坏了,failover 到下一个号即可(见 provider 的透传循环)。
                    RO::ServerError | RO::NetworkError => {
                        e.failure_count = e.failure_count.saturating_add(1);
                    }
                    // 🟢 **429 限流**:dwgx 明确定过——「偶尔 429 绝不需要冷却」。
                    // 所以这里既不冷却、不计失败、也不动 consecutive 计数,只启动/维持
                    // 「持续过载」观察窗。单次或一阵 429 完全放行;只有整个
                    // PASSTHROUGH_OVERLOAD_WINDOW 内**零成功且一直 429**(任一次成功都会把
                    // since 清成 None)才判定该站已不可用 → 按独立原因禁用,便于运维区分
                    // 「key 失效」与「站点持续过载」。
                    RO::RateLimited => {
                        let now = Instant::now();
                        // ⚠️ 必须先判「上一次 429 距今是否太久」再累积，否则**空闲时间会被算成过载**。
                        //
                        // 审查发现的缺陷：原实现只 `get_or_insert(now)` 且除成功外永不重置，
                        // 于是判据实际是「距第一次未被成功打断的 429 已过 300s」。
                        // 一个只在突发时偶尔 429 的健康站：T=0 一次 429 → 打上戳；随后该号
                        // **完全空闲** 10 分钟（零请求，所以也没有成功来清零）→ 再来一次 429
                        // → `now - since = 600s ≥ 300s` → 仅凭**相隔十分钟的两次 429** 就被禁用并落盘。
                        //
                        // 修法：429 之间的间隔超过窗口即视为「不连续」，重新起算。
                        // 这样"持续过载"才真的要求窗口内**密集**的 429，而不只是首尾两个点。
                        let stale = e
                            .passthrough_overload_since
                            .zip(e.last_passthrough_429_at)
                            .map(|(_, last)| now.duration_since(last) >= PASSTHROUGH_OVERLOAD_WINDOW)
                            .unwrap_or(false);
                        if stale {
                            e.passthrough_overload_since = None;
                        }
                        let since = *e.passthrough_overload_since.get_or_insert(now);
                        e.last_passthrough_429_at = Some(now);
                        if now.duration_since(since) >= PASSTHROUGH_OVERLOAD_WINDOW && !e.disabled {
                            e.disabled = true;
                            e.disabled_reason = Some(DisabledReason::PassthroughOverloaded);
                            e.disabled_at = Some(Utc::now().to_rfc3339());
                            disable_just_happened = true;
                            tracing::error!(
                                credential_id = id,
                                window_secs = PASSTHROUGH_OVERLOAD_WINDOW.as_secs(),
                                "代挂号连续 {}s 内零成功且持续 429,判定该中转站不可用并自动禁用\
                                 (偶尔 429 不会触发:任一次成功即重置观察窗)",
                                PASSTHROUGH_OVERLOAD_WINDOW.as_secs()
                            );
                        }
                    }
                    // 🔴 **非瞬态**失败:401/403(key 失效/被拒)、402(额度耗尽)、
                    // 4xx(base_url 配错、404 等)。这些**再试一万次也一样**,累加连续计数,
                    // 达 MAX_PASSTHROUGH_FAILURES 即自动禁用——等更多次纯属浪费用户的请求。
                    // 任一次成功归零,所以健康号不会被偶发 4xx 误禁。
                    // 🟢 **客户端请求错误**（400/404/422）：**绝不计入号的健康**。
                    //
                    // 这是坏的请求，不是坏的号 —— 换任何号都一样错。
                    // 实测依据（线上 traces.db）：代挂号 #216 成功率 80.3%（2910/3622）是个**健康号**，
                    // 但它历史上有 712 次 bad_request、其中 **119 次是 ≥3 连**、最长 6 连。
                    // 若把 BadRequest 计入连续失败计数（阈值 3），这个健康号会被误禁 119 次。
                    // 而且代挂号历史 429 次数为 **0**、失败形态**全是** bad_request ——
                    // 也就是说"把 bad_request 当号坏了"恰好会命中唯一真实存在的失败形态。
                    RO::BadRequest => {}
                    // 🔴 **非瞬态的凭据/账户问题**：401/403（key 失效/被拒）、402（额度耗尽）、
                    // 站点封号、站点不供该模型。这些**再试一万次也一样**，累加连续计数，
                    // 达 MAX_PASSTHROUGH_FAILURES 即自动禁用——等更多次纯属浪费用户的请求。
                    // 任一次成功归零，所以健康号不会被偶发失败误禁。
                    //
                    // OtherError 也归此类但要注意：它是兜底桶（含未分类状态码），
                    // 若将来发现它混进了瞬态信号，应把那类单独摘出去而不是放宽阈值。
                    RO::AuthFailed
                    | RO::QuotaExhausted
                    | RO::OtherError
                    | RO::AccountSuspended
                    | RO::ModelUnavailable => {
                        e.failure_count = e.failure_count.saturating_add(1);
                        e.consecutive_passthrough_failures =
                            e.consecutive_passthrough_failures.saturating_add(1);
                        if e.consecutive_passthrough_failures >= MAX_PASSTHROUGH_FAILURES
                            && !e.disabled
                        {
                            e.disabled = true;
                            e.disabled_reason = Some(DisabledReason::PassthroughFailed);
                            e.disabled_at = Some(Utc::now().to_rfc3339());
                            disable_just_happened = true;
                            tracing::error!(
                                credential_id = id,
                                count = e.consecutive_passthrough_failures,
                                "代挂号连续 {} 次非瞬态失败(key 失效/额度耗尽/配置错等再试无用的信号),\
                                 自动禁用",
                                e.consecutive_passthrough_failures
                            );
                        }
                    }
                }
            }
        }
        if limit_just_reached {
            // 达上限是「终身预算」硬边界:必须立即落盘,不能等 debounce/下次写。
            //   ① request_count 进 stats 文件——立即 save_stats 而非 debounced,防命中上限后进程
            //      崩溃/重启丢计数导致额度回退。
            //   ② disabled=true 进凭据文件——persist_credentials 立即落盘,否则重启前若无其它写操作
            //      则禁用状态丢失,号重新可用。两者叠加才真正杜绝「重启即额度归零」漏洞。
            self.save_stats();
            if let Err(e) = self.persist_credentials() {
                tracing::warn!(credential_id = id, "达请求上限后持久化禁用状态失败: {}", e);
            }
        } else if disable_just_happened {
            // 代挂号自动禁用（key 失效 / 持续过载）→ 立即落盘，否则重启即复活。
            self.save_stats_debounced();
            if let Err(e) = self.persist_credentials() {
                tracing::warn!(credential_id = id, "代挂号自动禁用后持久化失败: {}", e);
            }
        } else {
            self.save_stats_debounced();
        }
    }

    /// 获取可用凭据数量
    pub fn available_count(&self) -> usize {
        self.entries.lock().iter().filter(|e| !e.disabled).count()
    }

    /// **Kiro 路径**实际可选的凭据数（供重试预算计算）。
    ///
    /// 与 [`Self::total_count`]（= `entries.len()`）的区别是排除两类永不可选的条目：
    /// - `disabled` 的号；
    /// - `custom_api` 代挂号 —— `is_entry_selectable` 明确拒绝它们，Kiro 路径**永远**
    ///   选不到（它们走独立的 passthrough 路径）。
    ///
    /// 为什么重要：重试预算此前按 `total_count * 3` 算，于是禁用号与 custom_api 号会把
    /// 预算凭空抬高（生产日志里的 `尝试 8/36`、`27 = 9×3` 就是这么来的）。预算越大，
    /// 一条请求就能连打越多号、烧掉越多账号，叠加 sub2api 侧的重试后单请求最坏可放大到
    /// 约 70~108 次上游调用。按真正可选的号数算，预算才与「每个可用号各摸一次」对齐。
    pub fn kiro_selectable_count(&self) -> usize {
        self.entries
            .lock()
            .iter()
            .filter(|e| !e.disabled && !e.credentials.is_custom_api_credential())
            .count()
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
                        // L5:饱和判定含 L3 headroom 折扣(effective_saturation_limit),即绑定号达 headroom
                        // 后阈值(如 25 而非硬限 30)就让路,防单会话高吞吐把一个号顶到贴顶再让路。
                        // ⭐ 复用前提收敛到 is_sticky_reuse_healthy：**未饱和 且 熔断未 Open**
                        //   （半开期按 admit_prob 概率放行）。
                        //   此前只查饱和、不查熔断 —— 而亲和命中同样是 `return` 直接跳过下方排序键的
                        //   旁路，于是熔断 Open（p_avail=0）的号只要 rpm 未饱和就会被会话死粘，
                        //   排序键里那道"熔断沉底"完全够不着它。与 sticky current_id 是同一类漏洞，
                        //   故共用同一个判据函数，避免将来再次各自漂移。
                        if self.is_sticky_reuse_healthy(entry) {
                            tracing::debug!(user_id = %uid, credential_id = %bound_id, "亲和性复用凭据");
                            // 续期，使持续活跃的会话不因 TTL 到期而解绑
                            self.affinity.touch(uid);
                            return Some(self.commit_selection(entry));
                        }
                        // 注：归一化后两种模式都走下方同一套排序键，所以这条"落到下方分流"
                        // 现在**真的会分流**。此前 priority 模式下下方是裸 min_by_key(priority)，
                        // 解绑后重选常常又选回同一个饱和号并 affinity.set 重绑 → 解绑每次白做（活锁）。
                        tracing::debug!(
                            user_id = %uid,
                            credential_id = %bound_id,
                            "亲和性绑定号已饱和或熔断，本次不复用，改走均衡分流到健康空闲号"
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

        // ⭐ 归一化：`priority` 模式走与 `balanced` **完全同一套**排序键逻辑（见 effective_scheduling）。
        // 裸 priority 分支（仅 min_by_key(priority)）已删除——它缺失 balanced 的全部 5 项保护
        // （RPM 饱和硬门 / 熔断沉底 / inflight 均衡 / 余额加权 / 族级连坐），且平局恒选下标最小的号。
        let sched = self.effective_scheduling();

        let selected = {
            {
                // 自适应分流排序键（升序 min_by_key）——**共 10 位**，完整定义见本闭包末尾的元组，
                // 那里是唯一权威；本概览只说明各位的作用与次序，改元组时必须同步这里。
                //
                // ① unusable                真不可用(p_avail=0 或 RPM 饱和)沉底 —— 优雅溢出
                // ② starved                 ⭐反饥饿强制探测(0=已饥饿排最前)；排在 ① 之后故不绕硬门
                // ③ prio_key                开关开则按 priority 分层；关时恒 0
                // ④ health_tier             健康 3 档粗门。p_avail = 熔断门×健康×(1-RPM压力)×(1-负载)。
                //                           熔断 Open 的号/族 p_avail=0 自然沉底、半开期按 admit_prob
                //                           软降权。族键连坐：M365 同租户共享一个 health(整族一起沉)，
                //                           IdC/social/api_key 各自 cred:{id} 独立(坚强兜底不受连坐)。
                // ⑤ inflight_now            ⭐同档内在途最少优先 —— 治惊群核心
                // ⑥ slot_pressure_permille  ⭐在途/自身容量千分比(大池高基数区分)
                // ⑦ rpm_usage_permille      RPM 已用率低的先选(按容量比例分流)
                // ⑧ neg_p_fine              p_avail 精细兜底(含余额加权)
                // ⑨ success_count           终身成功数
                // ⑩ priority_tiebreaker     优先级末位兜底(同开关门控，关时恒 0)
                //
                // ⚠️ 健康分档**不是**首要键（历史注释曾如此描述，已不成立）：①② 先于它。
                // 这是刻意的 —— 真不可用的号必须沉底，饥饿号必须能拿到探测机会，
                // 二者都优先于"谁更健康"。
                // p_avail 已内含 rpm 压力/在途，⑤⑥⑦ 作同档兜底仍保留
                // （粒度更细 + rpm_limit=0 时 p_avail 不含压力）。
                // 是否叠加优先级分发（热更开关）。开启时:先按可用性粗分层(不可用/饱和的沉底),
                // 再按 priority 分层(越小越优先),层内仍按健康/负载均衡。这样高优先级号被优先用,
                // 但整层被打爆(p_avail=0 或饱和)时优雅溢出到下一优先级层,不死磕单个坏号。
                // 归一化后 prio_first 由 effective_scheduling 决定：
                // - 配置 "priority" → 恒 true（priority 语义 = 按优先级分层，层内均衡，整层打爆才溢出）
                // - 配置 "balanced" → 沿用 priority_in_balanced 开关（默认 false，纯健康/负载均衡）
                let prio_first = sched.priority_layered;
                // ⭐ 自适应 inflight 归一基准：**本轮选号只算一次**，所有候选共用同一分母。
                //
                // 必须一次算定（而非每个候选各算）：否则各候选分母不同 → 排序键非传递 →
                // min_by_key 的比较器失去全序 → 偶发选到负载更高的号（与下方"快照后是稳定全序"
                // 的既有约定一致）。
                //
                // 为什么需要自适应（实测确证的企业级真断点）：固定 LOAD_REF=8 时，
                // p90 延迟(17.1s)下 6000 RPM/200 号 → 每号常态在途 8.6 → 全池 load 同时 clamp 到 1.0
                // → p_avail 的 (1-0.5*load) 退化成常数 → 负载维度整体失效。
                // adaptive_load_ref 用 max(8.0, 平均在途×2)：小池恒取地板 8.0（零回归），
                // 高负载时随平均放大，保证池内**相对**负载差异始终可分辨。
                let total_inflight: u64 = available
                    .iter()
                    .map(|e| e.inflight.load(Ordering::Acquire) as u64)
                    .sum();
                let load_ref =
                    crate::kiro::health::adaptive_load_ref(total_inflight, available.len());
                // ⭐ RPM 计数**一次加锁批量取回**（企业级并发前提）。
                //
                // 此前排序键闭包对每个候选各调一次 `self.rpm.count(e.id)`（键里 2 处 +
                // 饱和判定 1 处），每次都独立加 `RpmTracker` 的锁 —— 43 号池一次选号
                // 最多 129 次加锁，而这整段都在 `entries` 锁临界区内。1000 RPM
                // （≈17 次选号/秒）下锁竞争与临界区时长被成倍放大，选号成为串行瓶颈。
                //
                // 现在一次取回全部候选的计数：锁获取 O(n) → O(1)，且闭包内变成纯
                // HashMap 查表（无锁、无扫描）。配合 `RpmTracker` 内部改用 VecDeque
                // 前缀弹出（摊还 O(1) 而非 O(w) 全扫），选号临界区从
                // O(n×w) 降到 O(n)。
                let cand_ids: Vec<u64> = available.iter().map(|e| e.id).collect();
                let rpm_counts = self.rpm.counts_for(&cand_ids);
                let rpm_of = |id: u64| rpm_counts.get(&id).copied().unwrap_or(0);
                // L4 排序键闭包(供两趟复用):入参 &CredentialEntry,升序 min_by_key。
                let sort_key = |e: &CredentialEntry| {
                        let key = e.credentials.family_key(e.id);
                        // per-cred RPM 容量:复用 effective_saturation_limit,与饱和判定口径统一
                        // (含兜底 30 + headroom 折扣)——修 L1:此前 unwrap_or 全局默认 0 会让
                        // p_avail 的 rpm_pressure 恒 0、速率维度从主键被剔除。effective_saturation_limit
                        // 只读原子镜像不锁 entries,闭包内调用安全。
                        let cred_rpm_cap = self.effective_saturation_limit(e.credentials.rpm_limit);
                        // inflight 在**整个排序键内只读一次**。
                        //
                        // 此前这里与下方 `slot_pressure_permille` 各自 `load(Acquire)` 一次，
                        // 于是同一个候选的排序键内部两组位可能来自不同时刻的 inflight：
                        // 喂给 p_avail 的那次决定第④位 health_tier 与第⑧位 neg_p_fine，
                        // 另一次决定第⑤位 inflight_now 与第⑥位 slot_pressure_permille。
                        // 并发下（inflight 由 InflightGuard 无锁增减）两者可以相差 ±1，
                        // 即键内自相矛盾。
                        //
                        // 而本闭包末尾的注释已明确「每个候选的排序键**只求值一次**快照」——
                        // 双读违背该意图。收口成单次读既消除不一致，也少一次原子加载。
                        //
                        // ⚠️ 无法用测试覆盖：这是并发时序，构造不出「移除即失败」的确定性用例。
                        // 依据是"与周边代码自述意图一致"而非实测收益，故刻意不改任何数值语义。
                        let inflight_now = e.inflight.load(Ordering::Acquire);
                        let p = self.health.p_avail_with_load_ref(
                            &key,
                            rpm_of(e.id),
                            inflight_now,
                            cred_rpm_cap,
                            load_ref,
                        );
                        // L2:健康降为 3 档粗门(不再是首要连续键),让"负载"成为同档内一等分流键。
                        // 治惠群根因——旧代码 neg_p_bucket 首排,最健康的号哪怕背 7 个在途也压过空闲的
                        // 稍弱号,突发全被它吸走。现同档内按 在途 + 剩余名额 分流。
                        // ⚠️健康分档用**原始 p**(不含余额加权):余额绝不该把健康号打进坏档,只在同档细分。
                        let health_tier = crate::kiro::health::health_tier(p);
                        // 饱和判定复用批量计数,避免又一次独立加锁(口径与 effective_saturation_limit 一致)
                        let saturated = rpm_of(e.id) >= cred_rpm_cap;
                        // 溢出闸:仅当该号"真不可用"(熔断 Open→p_avail=0 或 RPM 已饱和)时置 1 沉底,
                        // 保证优先级分层不会把流量钉死在一个已打爆的高优先级号上。用原始 p(余额不该把
                        // 健康号判成不可用)。
                        let unusable = (p <= 0.0 || saturated) as u8;
                        // 已用率(升序 min:已用率低的先选)——按 RPM 占容量的比例分流,比裸 rpm_count
                        // 更贴"容量差异化的号"(大容量号能接更多)。用整数千分比避免浮点排序不确定。
                        // cred_rpm_cap 恒 >0(effective_saturation_limit 保证),不会除零。
                        let rpm_usage_permille =
                            ((rpm_of(e.id) as u64 * 1000) / cred_rpm_cap as u64) as u32;
                        // 余额加权(软偏置微调):同档、同在途、同 RPM 已用率时,按剩余额度比例细分——
                        // 余额多的号 p_weighted 略高(neg 更小)→ 先选,长期拉平号池余额。开关关/缺快照=因子1.0。
                        // 只作用在 neg_p_fine(第 6 位末位兜底键),前面在途/已用率相等才轮到 → 不掀翻 0.7.23 分流。
                        let p_weighted = p * self.balance_factor(e.id, e.total_credits_used);
                        // p_avail 精细值(含余额加权)降为末位兜底,保留确定性 + 避免同档抖动。
                        let neg_p_fine = -((p_weighted * 1000.0) as i64);
                        // 优先级键仅在开关开启时参与首排;关闭时置 0(不影响原有均衡)。
                        let prio_key = if prio_first { e.credentials.priority } else { 0 };
                        // 末位兜底同样受开关门控:关闭时置 0,确保 priority_in_balanced=false 时
                        // 优先级在整个排序键中完全不起作用,均衡纯粹由健康/在途/用率决定。
                        let priority_tiebreaker = if prio_first { e.credentials.priority } else { 0u32 };
                        // ⭐ 高基数负载维度（企业级分流精度）：在途占**该号自身容量**的千分比。
                        //
                        // 为什么需要它：第④位是**裸 inflight**（绝对值）。400 号池中同 health_tier
                        // 的号 inflight 普遍落在 5~7，只有 3 档区分度 → 大量候选在 ④ 上完全平局；
                        // 若各号 rpm_limit 相同，第⑤位 rpm_usage_permille 也高度重合 →
                        // 一路平局到第⑥位 neg_p_fine。实测（5 号小池即已如此）：
                        // gini(inflight) median 0.524、最热/最冷 2.4x。
                        //
                        // 为什么不替换④：④的语义是"绝对在途最少优先"，是治惊群的核心
                        // （突发涌入时必须优先给完全空闲的号，不管它容量多大）。本维度是
                        // **容量归一**后的相对压力，回答的是"按各自体量谁更该接"——两者互补：
                        //   同 inflight 但容量不同的号，本维度把大容量号排前（它更能扛）；
                        //   同容量不同 inflight 的号，④已先分开，本维度不改变其相对次序。
                        //
                        // 与改动④（自适应 LOAD_REF）不重复惩罚：那一项作用在 p_avail 内部、
                        // 归一分母是**池平均在途**（跨号横向比较）；本项归一分母是**该号自身容量**
                        // （纵向比较自身余量），信息源不同、且分别落在排序键的第⑥位与第⑤位，
                        // 不构成同一信号的二次放大。
                        //
                        // 整数千分比（非浮点）：保证排序确定性，不引入浮点比较的不稳定性。
                        // cred_rpm_cap 恒 >0（effective_saturation_limit 保证），不会除零。
                        // 复用上方那次读（键内单一快照），不再重复 load。
                        let slot_pressure_permille =
                            ((inflight_now as u64 * 1000) / cred_rpm_cap as u64) as u32;
                        // ⭐ 第②位 starved —— 反饥饿强制探测。
                        //
                        // ⚠️ 以下"实测缺陷"一节记录的是**低负载全平局导致偏斜**这个问题本身，
                        // 它仍然真实。但当年针对它引入的那个**随机打散键 `tie_break_jitter`
                        // 已被删除**（理由：拿不出"移除它即失败"的测试，属无证据支撑的改动），
                        // 现在真正兜住这个问题的是本键 `starved` + 下方 STARVATION_PROBE_SECS。
                        // 读到下面"为什么用随机数"那一段时请注意：**排序键里已没有随机项**。
                        //
                        // ## 实测缺陷（问题描述，仍有效）
                        //
                        // 线上 6 号池、52 次请求全部成功（无坏号）实测：
                        //   gini 0.378、最热/最冷 **6.67x**（idx0 的 #208 拿 20 次，idx5 的 #213 只 3 次）。
                        //
                        // 根因是**低负载下前六个键会全部平局**：
                        //   - 全池健康 → ①②③ 恒等；
                        //   - 每秒几个请求、响应即归零 → `inflight` 大部分时刻恒 0 → ④⑤ 恒 0；
                        //   - `rpm_usage_permille` 用的是 60s 滑窗，**过期即归零** → 刚服务过的号
                        //     一分钟后又变回 0，⑥ 也追不上；
                        //   - ⑦ 的 p_avail 在全健康时高度重合。
                        // 而 `min_by_key` 在全平局时**恒返回第一个元素**，于是流量持续偏向
                        // `entries` 里下标靠前的号 —— 与观测到的 idx0 最热完全一致。
                        //
                        // ⑧ `success_count`（终身成功数）本该纠正这个，但它排在 ⑥ 之后：
                        // 刚服务过的号一旦滑窗过期，⑥ 就回到 0 并再次胜出，⑧ 根本轮不到。
                        //
                        // ## 为什么不把 ⑨ success_count 提前来纠正
                        //
                        // 把终身成功数提前会造成**新号饥饿的反向倾斜**：新入池的号 success=0，
                        // 会被连续灌满直到追平全池，期间既打爆新号又浪费老号容量。
                        // （历史上这里曾用随机打散键解决，那个键已删除，见本块开头的说明。）
                        //
                        // ⭐ 反饥饿强制探测（结构性兜底，见 STARVATION_PROBE_SECS）：
                        // 0 = 已饥饿(超窗未被选中) → 排到最前；1 = 正常。
                        // 排在 unusable 之后：真不可用的号仍沉底，探测不绕过硬门。
                        // 排在 prio_key 之前：代价是"偶尔一个低优先级饥饿号插队一次"，
                        // 换来的是任何单向状态缺陷都不会让号被永久排除 —— 这个交换是刻意的。
                        let starved = u8::from(
                            e.last_selected_at.get().elapsed().as_secs() < STARVATION_PROBE_SECS,
                        );
                        (
                            unusable,                // ① 真不可用沉底(优雅溢出)
                            starved,                 // ② ⭐饥饿号强制探测(0=饥饿排前)
                            prio_key,                // ③ 开关开:按优先级分层;关:恒 0
                            health_tier,             // ④ 健康 3 档粗门(坏号沉档)
                            inflight_now,            // ⑤ ⭐同档内在途最少优先(治惠群核心)
                            slot_pressure_permille,  // ⑥ ⭐在途/自身容量千分比(大池高基数区分)
                            rpm_usage_permille,      // ⑦ RPM 已用率低的先选(按容量比例分流)
                            neg_p_fine,              // ⑧ p_avail 精细兜底(含余额加权)
                            e.success_count,         // ⑨ 终身成功数
                            priority_tiebreaker,     // ⑩ 优先级末位兜底(同开关门控,关:恒 0)
                        )
                };
                // L4:两趟选号。第一趟只在**非饱和**候选里选(硬门,RPM 成真天花板);
                // 若整池饱和(第一趟空),按开关决定:false(默认)=回退软门对全体选"最不坏"(不阻塞,
                // 保留旧行为);true=返回 None 让 acquire_context 走背压等待最短 RPM 恢复窗口。
                // 每个候选的排序键**只求值一次**快照到 (key, entry),再 min_by_key。
                // 不用 min_by(闭包每次比较重算 sort_key):sort_key 读 inflight(无锁 fetch_update)、
                // p_avail(独立 Mutex,锁外调用)等并发可变态,重算会让比较器非传递——中途某号被 429
                // 降档时,已被早期淘汰的真最优号不会回来,偶发选到负载更高的号。快照后是稳定全序。
                // 第一趟的饱和过滤同样复用批量计数（原先每个候选再各加一次 RpmTracker 锁）。
                let non_saturated: Vec<_> = available
                    .iter()
                    .copied()
                    .filter(|e| {
                        rpm_of(e.id)
                            < self.effective_saturation_limit(e.credentials.rpm_limit)
                    })
                    .map(|e| (sort_key(e), e))
                    .collect();
                if !non_saturated.is_empty() {
                    non_saturated.into_iter().min_by_key(|(k, _)| *k).map(|(_, e)| e)
                } else if self.rpm_hard_gate_overload_wait.load(Ordering::Relaxed) {
                    // 整池饱和 + 背压开:返回 None,上游 acquire_context 等待恢复(受 MAX_TRANSIENT_WAIT 限)。
                    None
                } else {
                    // 整池饱和 + 背压关(默认):回退软门,对全体选"最不坏"(排序键里 unusable/已用率会
                    // 让最不坏的浮上来),不阻塞——保留旧行为,零回归。同样单次求值快照。
                    available
                        .iter()
                        .copied()
                        .map(|e| (sort_key(e), e))
                        .min_by_key(|(k, _)| *k)
                        .map(|(_, e)| e)
                }
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
        // ⭐自定义 API 代挂号**绝不进 Kiro 选号**:它不是 Kiro 号,当 Kiro 号打 CodeWhisperer 端点
        // 会 403 认证失败→被误冷却(实测 #87 就这样被冷却 86400s)。它只由透传路径(select_custom_api)
        // 单独选号。这是"混入池但分流"的关键隔离:Kiro 路径永不碰 custom_api,custom_api 路径永不碰 Kiro。
        if entry.credentials.is_custom_api_credential() {
            return false;
        }
        if is_opus && !entry.credentials.supports_opus() {
            return false;
        }
        // 成本安全白名单硬门：该号设了 allowed_models 且当前模型不在其中 → 过滤掉。
        // 把便宜模型（国产）的流量锁死在指定号上，杜绝溢出到未列该模型的（更贵）号按贵号计费。
        // model 为空串（无模型信息，如 MCP 工具调用不带 modelId）时**跳过本检查**（不因白名单过滤），
        // 因为白名单约束的是"对话模型"，不该误伤无模型语义的 MCP 调用。
        if !model.is_empty() && !entry.credentials.allows_model(model) {
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

    /// 全池无立即可用候选时的等待判定(带类型化原因,供调用方区分终态处理):
    /// - `NoCandidate`:无任何可用候选(全禁用/被 opus/模型白名单等硬门过滤)→ 终态应报"已禁用"。
    /// - `Available`:存在**立即可用**候选(select 却返 None,多为去饱和/并发释放的竞态)→ 应重选,绝不 bail。
    /// - `Wait(dur, reason)`:所有候选都在等待恢复,取最短等待 + 其原因(Cooling=冷却/风控;RpmRecovery=
    ///   RPM 饱和将恢复)。原因决定调用方是否 fast-fail、终态文案用哪类(RPM 饱和绝不报"已禁用")。
    fn transient_wait_outcome(&self, model: Option<&str>) -> WaitOutcome {
        let is_opus = model
            .map(|m| m.to_lowercase().contains("opus"))
            .unwrap_or(false);
        let model_key = model.unwrap_or("");
        let entries = self.entries.lock();
        let mut has_candidate = false;
        let mut immediate_available = false;
        let mut waits: Vec<(StdDuration, WaitReason)> = Vec::new();

        for entry in entries.iter() {
            if entry.disabled {
                continue;
            }
            // ⚠️ 下面这组硬门**必须与 is_entry_selectable 逐条对齐**。
            // 任何 is_entry_selectable 会过滤、而这里不过滤的条件，都会让
            // 「select_next_credential 返 None」与「本函数判定 immediate_available」同时成立，
            // 于是 acquire_context 的 `WaitOutcome::Available => continue` 分支既不 sleep 也不
            // 递增 attempt_count（那条分支的语义是"竞态，立刻重选"）→ 形成**无退出条件的忙等
            // 热循环**：请求永不返回且烧满一个 CPU 核。
            // 历史缺口：漏了 custom_api 与 model_blocklist 两道，触发路径真实存在——
            //   ① 池中只有 custom_api 代挂号（未禁用无冷却），任何走 Kiro 主路径的调用
            //      （如 try_custom_api_passthrough 全冷却后回落、MCP/WebSearch）即命中；
            //   ② 某模型被池中所有号加进 model_blocklist（TTL 1800s）后再来同模型请求。
            if entry.credentials.is_custom_api_credential() {
                continue;
            }
            if is_opus && !entry.credentials.supports_opus() {
                continue;
            }
            // 成本安全白名单硬门（与 is_entry_selectable 保持一致，否则等待估算与实际可选号不符）
            if !model_key.is_empty() && !entry.credentials.allows_model(model_key) {
                continue;
            }
            if self.is_model_blocked(entry.id, model_key) {
                continue;
            }

            has_candidate = true;

            if self.cooldown_enabled.load(Ordering::Relaxed) {
                if let Some((_reason, remaining)) = self.cooldown.check_cooldown(entry.id) {
                    waits.push((remaining, WaitReason::Cooling));
                    continue;
                }
            }

            if self.rate_limit_enabled.load(Ordering::Relaxed) {
                if let Err(wait) = self.rate_limiter.check_rate_limit(entry.id) {
                    waits.push((wait, WaitReason::Cooling));
                    continue;
                }
            }

            // L4 背压:仅当开启硬门背压时,RPM 饱和号才算"将恢复的等待候选"(而非立即可用)。
            // 恢复窗口 = 该号最老命中再过 (60s - oldest_age) 就过期腾名额。背压关时不计(RPM 饱和号
            // 在软门下仍是立即可选候选,不等待——保持默认行为)。
            if self.rpm_hard_gate_overload_wait.load(Ordering::Relaxed)
                && self.is_rpm_saturated_with_limit(entry.id, entry.credentials.rpm_limit)
            {
                let recover = self
                    .rpm
                    .oldest_age(entry.id)
                    .map(|age| self.rpm.window().saturating_sub(age))
                    .unwrap_or_else(|| StdDuration::from_secs(1));
                // 至少等 250ms,避免 0 等待空转;上限由外层 MAX_TRANSIENT_WAIT 兜底。
                waits.push((recover.max(StdDuration::from_millis(250)), WaitReason::RpmRecovery));
                continue;
            }

            // 走到这里说明该号既未冷却/限流、也未被背压计为饱和 → 立即可用候选。
            // inflight 绝不作为阻塞门槛(在途只进排序键,并发直接落它)。
            immediate_available = true;
        }

        if !has_candidate {
            return WaitOutcome::NoCandidate;
        }
        // 有立即可用候选(select 却返 None)= 竞态,应重选而非 bail/等待。
        if immediate_available {
            return WaitOutcome::Available;
        }
        // 所有候选都在等待:取最短,连同其原因返回。
        match waits.into_iter().min_by_key(|(d, _)| *d) {
            Some((d, reason)) => WaitOutcome::Wait(d, reason),
            // has_candidate 但既非立即可用又无等待项:理论不可达,保守当竞态重选。
            None => WaitOutcome::Available,
        }
    }

    fn commit_selection(&self, entry: &CredentialEntry) -> (u64, KiroCredentials, InflightGuard) {
        let guard = InflightGuard::acquire(entry.inflight.clone());
        self.rpm.record(entry.id);
        // 反饥饿探测的时间基准（见 STARVATION_PROBE_SECS）。用 Cell 而非 &mut：
        // 本函数按既有约定收 &CredentialEntry（选号闭包里持的是不可变引用）。
        entry.last_selected_at.set(Instant::now());
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

    /// 有效饱和阈值:per-cred(>0) > 全局(>0) > 默认高水位兜底(30),再应用 headroom 折扣。
    /// 恒 >0,保证分流生效。**优先级不破坏**:折扣作用在选定 base 之后,per_cred/global/兜底的选取不变。
    ///
    /// pub:运维观测(ratelimit_insights)复用此真相源判饱和,避免 UI 侧重算不含 headroom 的阈值
    /// 导致"调度早已硬门拦下、UI 仍显示畅通"的观测口径漂移。只读原子镜像,不锁 entries,可任意调用。
    pub fn effective_saturation_limit(&self, per_cred_limit: Option<u32>) -> u32 {
        const SATURATION_FALLBACK_RPM: u32 = 30;
        let base = per_cred_limit
            .filter(|&v| v > 0)
            .or_else(|| {
                let g = self.rpm_limit.load(Ordering::Relaxed);
                (g > 0).then_some(g)
            })
            .unwrap_or(SATURATION_FALLBACK_RPM);
        self.apply_rpm_headroom(base)
    }

    /// 对 base 容量应用 headroom:base × factor/100 再减预留名额,下限 1(绝不 0,否则恒饱和)。
    /// factor=0 或 100 且 reserve=0 时 = base(旧行为,零回归)。
    fn apply_rpm_headroom(&self, base: u32) -> u32 {
        let factor = self.rpm_headroom_factor.load(Ordering::Relaxed);
        // factor=0 视为"不打折"(=100),避免误配 0 把所有号打成恒饱和。
        let discounted = if factor == 0 || factor >= 100 {
            base
        } else {
            ((base as u64 * factor as u64) / 100) as u32
        };
        let reserve = self.rpm_reserve_slots.load(Ordering::Relaxed);
        discounted.saturating_sub(reserve).max(1)
    }

    /// RPM 硬门在当前配置下是否**真的**对调度生效(而非仅仅是一个数字)。
    ///
    /// 只报告"是否生效"，绝不改变 `effective_saturation_limit` 的返回值语义——那是
    /// balanced 排序键 `rpm_usage_permille` 与 `health::p_avail` 的 rpm_pressure 共用的
    /// 调度真相源，改它会掀翻分流。这里只回答"对外要不要把 rpm>=阈值 报告为饱和"。
    ///
    /// 推导依据(见 select_next_credential / transient_wait_outcome 逐路径读码):
    /// - `balanced` 模式下 `non_saturated` 两趟选号硬门(2194-2214)才真正按饱和降权候选；
    ///   `priority` 模式的 `min_by_key(priority)` 分支完全不读饱和，饱和判定对结果零影响。
    /// - 亲和解绑(2085-2118)即便判定 `bound_saturated` 解绑，解绑后仍落进上面的 mode 分支，
    ///   在 priority 模式下同样不受影响——解绑本身不改变最终选中的凭据。
    /// - `transient_wait_outcome` 的背压分支(2340-2354)只在 `select_next_credential` 返回
    ///   `None` 时才可能被读到；priority 模式下 `is_entry_selectable` 不含 RPM 判定，只要还有
    ///   未冷却未限流的候选就必返 `Some`，该分支实际不可达。
    /// 因此"硬门生效"当且仅当 `balanced` 模式——`rpm_hard_gate_overload_wait` 单独存在时
    /// 不改变 priority 模式下的调度结果，故不纳入本判据。
    ///
    /// 另外:只有 1 个凭据时"分流"概念不适用(无处可分)，恒不算生效。
    ///
    /// pub:供 admin/service.rs 的 `ratelimit_insights` 复用，避免 UI 侧另起一套判据
    /// 与调度真实生效条件失配(那正是本次要修的"虚假饱和"问题本身)。
    pub fn rpm_saturation_gate_active(&self) -> bool {
        if self.total_count() <= 1 {
            return false;
        }
        // ⚠️ 归一化后**两种模式都走同一套排序键**（见 effective_scheduling），
        // 故 RPM 饱和硬门在两种模式下都真实生效——不能再只认 "balanced"，
        // 否则 priority 模式下调度已按饱和拦下、面板却报 rpmSaturated=false（观测口径反向漂移）。
        true
    }

    /// sticky `current_id` 复用的**健康前提**：未 RPM 饱和 且 熔断未 Open。
    ///
    /// 为什么需要（见 acquire_context 里 current_hit 处的完整根因说明）：sticky 命中会**整段跳过**
    /// `select_next_credential`，而 `is_entry_selectable` 不含熔断/饱和判定，导致坏号被无限复用。
    ///
    /// 半开期（`HalfOpen { admit_prob }`）按概率放行而非一律拒绝：熔断退避到期后必须允许
    /// 试探性流量回到该号，否则它永远回不到 current 位置、也永远无法通过连续成功恢复 Closed。
    ///
    /// 随机性说明：本函数在一次请求里**只被调用一次**（只判 current_id 那一个号），
    /// 不像 `is_entry_selectable` 会在一轮选号里对每个候选各调一次——故用随机数不会造成
    /// 同一轮内自相矛盾的判定。
    ///
    /// 调用约定：调用方已持 `entries` 锁；本函数只读原子镜像与 health/rpm 的独立锁，不重入 entries。
    fn is_sticky_reuse_healthy(&self, entry: &CredentialEntry) -> bool {
        if self.is_rpm_saturated_with_limit(entry.id, entry.credentials.rpm_limit) {
            return false;
        }
        let fam = entry.credentials.family_key(entry.id);
        match self.health.snapshot(&fam) {
            // 无 health 记录 = 从未出问题，视为满血（与 p_avail 的 or_default 语义一致）。
            None => true,
            Some(s) => {
                if s.circuit_open {
                    return false;
                }
                if s.half_open {
                    // 半开：按 admit_prob 概率放行，给恢复留通路。
                    return fastrand::f64() < s.admit_prob;
                }
                true
            }
        }
    }

    /// 生效的调度语义（`load_balancing_mode` 归一化的**唯一真相源**）。
    ///
    /// ## 为什么归一化
    /// 历史上 `priority` 模式（**出厂默认**，`config.rs` 的 `default_load_balancing_mode`）的选号
    /// 只有一行 `min_by_key(|e| e.credentials.priority)`，与 `balanced` 那套完整排序键**并列存在**，
    /// 于是 balanced 独有的 5 项保护在默认部署下全部失效：
    ///   ① RPM 饱和硬门（两趟选号）② health 熔断 Open 沉底 ③ inflight 负载均衡
    ///   ④ 余额加权 ⑤ 族级连坐
    /// 且 `min_by_key` 平局取第一个 → 同优先级多号**恒选 entries 里下标最小（最早创建）那个**。
    ///
    /// 实测后果（5 号池、priority 全为 0）：某号 rpm=23 而另一号 rpm=1（负载差 23 倍）；
    /// 5 个号里 4 个熔断 `Open`（`admit_prob=0`）却照样接流量（熔断只经 `p_avail` 进 balanced
    /// 排序键，而 `is_entry_selectable` 不含熔断判定）；亲和饱和解绑后重选又回到同一个饱和号
    /// （解绑逻辑每次正确触发、每次白做——注释里写的"落到下方 balanced 分流"在 priority 模式并不存在）。
    ///
    /// ## 归一化语义
    /// `priority` ≡ `balanced` + `priority_in_balanced=true`。后者的既有语义（见 `config.rs` 注释）
    /// 正是"先按 priority 分层（越小越优先），**层内**仍按健康/负载均衡，整层饱和/熔断才优雅溢出
    /// 到下一优先级层"——功能上是裸 priority 的**严格超集**：优先级语义完整保留，且不再死磕单个坏号。
    ///
    /// ## 兼容性
    /// **只在读取时归一化，绝不改写用户 `config.json`**：配置里的 `"priority"` 字符串保持不动，
    /// 面板照常显示原值，`set_load_balancing_mode` 的合法值校验不变。用户无需改配置即受益。
    fn effective_scheduling(&self) -> SchedulingSemantics {
        let is_priority_mode = self.load_balancing_mode.lock().as_str() != "balanced";
        SchedulingSemantics {
            // priority 模式恒按优先级分层；balanced 模式沿用开关（默认 false = 纯健康/负载均衡）。
            priority_layered: is_priority_mode
                || self.priority_in_balanced.load(Ordering::Relaxed),
        }
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
    /// 入站整形准入:**每个客户端请求进 failover 循环前调用一次**(不在 acquire_context 里,
    /// 避免每跳重复扣令牌)。突发被排队削平成受控 RPM;超时返回 Err(retry_after 秒)让客户端退避。
    pub async fn acquire_admission(&self) -> Result<(), u64> {
        let r = self.throttle.acquire().await;
        if r.is_ok() {
            // 有请求真正获准 → 触发一次自动挡升档探测(内部判周期)。
            self.throttle.maybe_step_up();
        }
        r
    }

    /// 上游 429 反馈:让入站整形的 RPM 自动挡乘性降档(provider 检到上游限流时调用)。
    pub fn report_upstream_rate_limited(&self) {
        self.throttle.report_upstream_429();
    }

    /// 报告**非 429 的上游压力信号**（账户被暂停 / 5xx 风暴），同样触发入站 AIMD 降档。
    ///
    /// 为什么需要：AIMD 此前只由 429 驱动（`report_upstream_429` 仅在 provider 的两处
    /// 429 分支被调），于是 403 suspend 风暴与 500 风暴**完全不会**让入站 RPM 降档 ——
    /// 网关继续按原速率往已经在拒绝我们的上游灌流量，把风控进一步激化。
    /// 实测：一小时 408 次 500 + 12 小时 88 次 suspend 期间，入站整形毫无反应。
    ///
    /// 复用 `report_upstream_429` 的降档路径（含去抖与「升档饿死」死锁修复），
    /// 不另造一套乘性降档逻辑 —— 那会绕开已经修好的 `last_md_nanos` 语义。
    pub fn report_upstream_pressure(&self) {
        self.throttle.report_upstream_429();
    }

    /// 当前入站整形目标 RPM(可观测/运维页展示)。
    pub fn inbound_target_rpm(&self) -> u32 {
        self.throttle.current_target_rpm()
    }

    pub async fn acquire_context(
        &self,
        model: Option<&str>,
        user_id: Option<&str>,
    ) -> anyhow::Result<CallContext> {
        // 注意:入站整形闸门**不在这里**。acquire_context 会被 failover 循环每跳调用,
        // 若在此扣令牌 → 一个客户端请求 failover N 次就扣 N 个令牌 + fast-fail 空转白扣(review Finding 1)。
        // 整形应针对"客户端请求"一次,故闸门上移到 provider 调用入口(acquire_admission,循环外只过一次)。
        let total = self.total_count();
        // 内层尝试预算需与 provider 层的外层重试预算同量级放开：
        // 以可用凭据数为下限，保证内层不会在外层遍历完所有可用号之前就先耗尽。
        // （历史上仅 total*MAX_FAILURES，当可用数因禁用波动大时可能过紧）
        let max_attempts = (total * MAX_FAILURES_PER_CREDENTIAL as usize)
            .max(self.available_count())
            .max(1);
        let mut attempt_count = 0;
        // 纵深防御：`WaitOutcome::Available` 分支的语义是"选号与等待判定之间发生了竞态,
        // 立刻重选"，它刻意不递增 attempt_count（竞态重选不该消耗重试预算）也不 sleep。
        // 代价是：若两处硬门条件一旦不对齐，该分支会变成无退出条件的忙等热循环（烧满一核、
        // 请求永不返回）。这里独立计数并设上限，保证**即使将来再次出现条件不对齐，也只是
        // 快速失败而非挂死**——把"逻辑 bug"降级成"可观测的错误"。
        let mut race_reselect_count = 0usize;
        const MAX_RACE_RESELECT: usize = 64;
        let wait_started = Instant::now();

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
                // 🔴 已删除的 sticky `current_id` 选号捷径（实测负载失衡的真正元凶）
                //
                // 旧逻辑：`priority` 模式下（出厂默认）只要 `current_id` 指向的号通过
                // `is_entry_selectable`，就直接 `commit_selection` 复用，
                // **`select_next_credential` 压根不执行** → 排序键、两趟饱和硬门、亲和解绑全部不运行。
                // 而 `is_entry_selectable` 只查 disabled / custom_api / opus / allowed_models /
                // model_blocklist / cooldown / rate_limiter —— **不含熔断、不含饱和、不含 inflight**。
                // 于是一个熔断 Open（admit_prob=0）+ rpm 饱和 + inflight 爆满的号会被**无限复用**，
                // 直到它恰好进入 cooldown 或被禁用才换人。
                //
                // 实测（5 号池）：currentId 指向的号被钉住吃流量、同池另一号 rpm=0 完全空转；
                // gini(rpm) 随时间从 0.06 单调恶化到 0.41；5 个号里 4 个熔断 Open 却仍在接流量。
                //
                // 为什么是**删除**而非"加健康前提"：先前尝试过只在"饱和或熔断"时放弃粘性，但
                // 回归测试 `test_priority_mode_same_priority_spreads_load` 证明这不够——号**健康时**
                // 粘性合法生效，于是 6 个连续请求全落同一号（`{1: 6}`），负载压根不分摊。
                // sticky 的语义（钉住一个号直到它坏掉）与"按在途/RPM 均衡分摊"在根本上互斥，
                // 而它唯一存在的分支正是刚被归一化掉的 priority 模式（见 effective_scheduling）。
                // 保留它就等于保留"归一化前的旧行为"，故整段移除，两种模式都每次走完整均衡选号。
                //
                // `current_id` 字段本身**保留**：它仍是有意义的可观测状态（面板 currentId、
                // 优先级变更后的 select_highest_priority 切换、failover 轨迹），只是不再作为选号捷径。
                {
                    // 每次请求都走完整的均衡选号（两种模式统一）。
                    let mut best = self.select_next_credential(model, user_id);

                    // 没有可用凭据：如果是"自动禁用导致全灭"，做一次类似重启的自愈
                    if best.is_none() {
                        let mut entries = self.entries.lock();
                        // ⭐ 可自愈的原因集合（**刻意不含** AccountSuspended / QuotaExceeded /
                        // InvalidRefreshToken / RequestLimitReached / Passthrough* ——
                        // 那些要么真被封、要么额度耗尽、要么配置坏，复活只会白撞）。
                        //
                        // 🔴 修复的缺陷（另一位 review 抓到，线上数据确证）：此前只匹配
                        // `TooManyFailures`，而 403 风控走的是 `SuspiciousActivityAuto` ——
                        // 它自己的注释明写 403 是**整池瞬时风控**（历史事故：403 曾被当永久封禁
                        // → 12h 内 88 次误禁 + 36 次全池活锁）。于是一次 IP 级风控把全池打成
                        // SuspiciousActivityAuto 后**没有任何自动恢复路径**。
                        //
                        // 线上实测（48h）：`判定为死号并自动禁用` 46 次，而
                        // `执行自愈` **0 次** —— 自愈从未对这个原因生效过。
                        if entries.iter().any(|e| {
                            e.disabled && is_self_healable_reason(e.disabled_reason)
                        }) {
                            tracing::warn!(
                                "所有凭据均已被自动禁用，执行自愈：重置失败计数并重新启用（等价于重启）"
                            );
                            // 被自愈复活的号 id，供放锁后清旁挂结构 + 落盘。
                            let mut healed_ids: Vec<u64> = Vec::new();
                            for e in entries.iter_mut() {
                                if is_self_healable_reason(e.disabled_reason) {
                                    e.disabled = false;
                                    e.disabled_reason = None;
                                    e.disabled_at = None;
                                    // 走单一收口：原先只清 failure_count，漏了
                                    // refresh_failure_count 与 consecutive_suspicious
                                    // → 复活的号一次风控/刷新失败即再次禁用。
                                    e.clear_transient_counters();
                                    healed_ids.push(e.id);
                                }
                            }
                            drop(entries);

                            // 清旁挂结构（各有独立锁，必须在 entries 锁外）：残留冷却/退避
                            // 会让刚复活的号立刻又被选号硬门跳过，自愈等于没做。
                            for id in &healed_ids {
                                self.cooldown.clear_cooldown(*id);
                                self.rate_limiter.reset(*id);
                            }

                            // 落盘：自愈此前**只改内存**，磁盘仍是 disabled=true。
                            // 自动禁用落盘（persist_disabled_state）上线后，这个洞从
                            // "重启即恢复"恶化成"重启回死态"——面板显示可用、磁盘却全死，
                            // 重启后整池以 disabled 读回，且没有任何请求能触发下一次自愈
                            // （自愈的前提是"池中存在 TooManyFailures 禁用号"，重启后
                            // disabled_reason 仍在，但用户看到的是整池不可用）。
                            // 落盘失败不能中断请求：内存已复活，记 error 让运维可见即可。
                            if let Err(e) = self.persist_credentials() {
                                tracing::error!(
                                    "全池自愈已在内存生效，但落盘失败（重启后将回到禁用态）: {}",
                                    e
                                );
                            }

                            best = self.select_next_credential(model, user_id);
                        }
                    }

                    if let Some((new_id, new_creds, guard)) = best {
                        // 更新 current_id
                        let mut current_id = self.current_id.lock();
                        *current_id = new_id;
                        (new_id, new_creds, guard)
                    } else {
                        // 只有"马上(≤2s)就能恢复"的瞬时繁忙才短等一下,避免把秒级抖动也甩给客户端。
                        const FAST_FAIL_THRESHOLD: StdDuration = StdDuration::from_secs(2);
                        match self.transient_wait_outcome(model) {
                            // 竞态:select 返 None 但此刻已有立即可用候选(去饱和/并发释放)→ 重选,绝不 bail。
                            // 计数兜底见 race_reselect_count 的声明处：正常竞态只需 1~2 次重选即可命中,
                            // 连续 64 次仍不命中说明两处硬门条件不对齐（逻辑 bug），此时快速失败而非挂死。
                            WaitOutcome::Available => {
                                race_reselect_count += 1;
                                if race_reselect_count > MAX_RACE_RESELECT {
                                    tracing::error!(
                                        "选号竞态重选已达 {} 次仍无法命中：说明 is_entry_selectable 与 \
                                         transient_wait_outcome 的硬门条件不对齐（逻辑 bug，请检查两处过滤是否一致）。\
                                         为避免忙等挂死，此处快速失败。",
                                        MAX_RACE_RESELECT
                                    );
                                    anyhow::bail!(
                                        "选号竞态无法收敛（可用: {}/{}），已中止以避免忙等",
                                        self.available_count(),
                                        total
                                    );
                                }
                                continue;
                            }
                            // 冷却/风控:长恢复窗口走 fast-fail(仅当 all_cooling_fast_fail 开),让客户端退避;
                            // 否则网关内短等重试。
                            WaitOutcome::Wait(wait, WaitReason::Cooling) => {
                                if self.all_cooling_fast_fail.load(Ordering::Relaxed)
                                    && wait > FAST_FAIL_THRESHOLD
                                {
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
                                    let w = wait.max(StdDuration::from_millis(250)).min(StdDuration::from_secs(2));
                                    tracing::warn!("所有可用凭据暂时繁忙，短等 {:?} 后重试", w);
                                    sleep(w).await;
                                    continue;
                                }
                                // 冷却等待超总预算:带 retry_after 报可重试的"冷却"类别(非"已禁用")。
                                let retry_after = wait.as_secs().max(1);
                                anyhow::bail!(
                                    "所有凭据均在冷却，等待超时（0/{}）retry_after_secs={}",
                                    total,
                                    retry_after
                                );
                            }
                            // L4 背压:RPM 饱和将恢复。绝不 cooling-fast-fail、绝不报"已禁用"——网关内等到
                            // 恢复窗口(受 MAX_TRANSIENT_WAIT 上限);超上限带 retry_after 报可重试的"繁忙"类别。
                            WaitOutcome::Wait(wait, WaitReason::RpmRecovery) => {
                                if wait_started.elapsed() < StdDuration::from_secs(MAX_TRANSIENT_WAIT_SECS) {
                                    // RPM 恢复窗口可长达 ~60s,等待封顶到剩余总预算内,不空转也不超墙钟。
                                    let remaining = StdDuration::from_secs(MAX_TRANSIENT_WAIT_SECS)
                                        .saturating_sub(wait_started.elapsed());
                                    let w = wait.max(StdDuration::from_millis(250)).min(remaining.max(StdDuration::from_millis(250)));
                                    tracing::warn!("整池 RPM 饱和(背压),等待恢复窗口 {:?} 后重试", w);
                                    sleep(w).await;
                                    continue;
                                }
                                let retry_after = wait.as_secs().max(1);
                                anyhow::bail!(
                                    "整池 RPM 已饱和，等待恢复超时（{}/{}）retry_after_secs={}",
                                    total,
                                    total,
                                    retry_after
                                );
                            }
                            // 无任何可用候选。**必须区分两种成因**,否则把可重试的临时态报成永久态:
                            //
                            // ① `available == 0`:池子真的全禁用 → 报"已禁用",这是终态。
                            // ② `available > 0`:号没被禁用,是被**模型级硬门**挡掉的
                            //    (model_blocklist / allows_model 白名单 / supports_opus)。
                            //
                            // 🔴 修复的缺陷:旧代码两种情形都报同一句"所有凭据均已禁用({available}/{total})"。
                            // 于是 ② 会产出自相矛盾的 `所有凭据均已禁用（2/2）`——2 个可用却说全禁用。更糟的是
                            // 该串匹配不上 `map_provider_error` 的任何分支(既无 429/QUOTA 等关键词,也无
                            // retry_after_secs 标记),落到末尾兜底 → **502 BAD_GATEWAY 且无 Retry-After**。
                            // 而 ② 的绝大多数是 `model_blocklist`(某号对某模型返 INVALID_MODEL_ID 后加黑,
                            // TTL 1800s),它是**限时的临时态**:TTL 到期即自动放行重试探。
                            //
                            // 后果链(线上 24h 实测):577 次 ② 类假报,集中在订阅不含的模型
                            // (gpt-5.6-sol/luna/terra 各 ~87、deepseek-3.2 84、glm-5 83)以及
                            // claude-opus-5 88。客户端(Claude Code)把 502 当"服务端故障"而非"这个模型现在
                            // 不可用",既不退避也不换模型,原样重发 → 再 502。同时这 577 条污染了
                            // "池子耗尽"的统计口径,让真实耗尽(3221 次 available=0)的严重度无法评估。
                            //
                            // 修法:② 带 `retry_after_secs=` 标记走 `map_provider_error` 的既有 429 分支
                            // (与"全池冷却"同款语义:可重试 + 明确退避秒数)。不新增字符串判据——那正是
                            // 本缺陷的成因;复用已有的 retry_after_secs 协议,中文文案改动不会再让它失效。
                            WaitOutcome::NoCandidate => {
                                let available = {
                                    let entries = self.entries.lock();
                                    // 注意:必须在 bail! 之前算完并放锁,available_count() 会再锁 entries 致死锁。
                                    entries.iter().filter(|e| !e.disabled).count()
                                };
                                if available == 0 {
                                    // 🔴 必须带 `retry_after_secs=`：不带的话这个串**匹配不上
                                    // `map_provider_error` 的任何分支**（既无该标记、也无
                                    // model_unsupported_by_pool=1、也不含 QUOTA 等上游关键词）
                                    // → 落到函数末尾兜底 → **502 且无 Retry-After**。
                                    //
                                    // 后果链（线上实测）：客户端（Claude Code）把 502 当"服务端
                                    // 故障"而非"稍后重试"，其退避逻辑压根不启动 → 原样重发 →
                                    // 又 502。2026-08-03 01:55–02:10 的耗尽窗口里，单个 5 分钟
                                    // 桶就产生 937 次，全部是这一种。
                                    //
                                    // 0.7.45 修的是同一函数里的情形②（模型硬门，用
                                    // model_unsupported_by_pool=1 标记），情形①（真耗尽）当时
                                    // 未处理 —— 而它才是量最大的那个。
                                    //
                                    // 复用既有 retry_after_secs 协议而不新增字符串判据：后者正是
                                    // 本类缺陷反复出现的成因（中文文案一改分类就失效）。
                                    anyhow::bail!(
                                        "所有凭据均已禁用（0/{}）retry_after_secs={}",
                                        total,
                                        POOL_EXHAUSTED_RETRY_AFTER_SECS
                                    );
                                }
                                // ⚠️ 必须区分「限时」与「永久」两类模型硬门，否则只是把 502 死循环
                                // 换成 429 死循环：
                                // - `model_blocklist` 命中 → **限时**（TTL 到期自动放行）→ 可重试，带 retry_after。
                                // - 拿不到 TTL（`allowed_models` 白名单不含该模型 / FREE 档不支持 opus）
                                //   → 对这个模型是**永久**的，等多久都不会变 → 报不可重试的错误。
                                //   若也带 retry_after，客户端（Claude Code）会每 5 分钟重试一次直到永远
                                //   （下游 `map_provider_error` 还会把秒数 clamp 到 300）。
                                let Some(remaining) = self.model_block_min_remaining(model) else {
                                    anyhow::bail!(
                                        "模型 {:?} 不被本号池支持（{}/{} 个号均因订阅档位或成本白名单不含该模型而被过滤，非号池耗尽，重试无效）model_unsupported_by_pool=1",
                                        model.unwrap_or(""),
                                        available,
                                        total
                                    );
                                };
                                let retry_after = remaining.as_secs().max(1);
                                anyhow::bail!(
                                    "模型 {:?} 当前无可用凭据（{}/{} 个号均被模型级过滤，非号池耗尽）retry_after_secs={}",
                                    model.unwrap_or(""),
                                    available,
                                    total,
                                    retry_after
                                );
                            }
                        }
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
                        // 与上面 NoCandidate 那处同理：不带标记就落 502 无 Retry-After。
                        // 这条路径是"刷新失败把最后一个号也禁用了"，同样可自愈。
                        anyhow::bail!(
                            "所有凭据均已禁用（0/{}）retry_after_secs={}",
                            total,
                            POOL_EXHAUSTED_RETRY_AFTER_SECS
                        );
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

        // 自定义 API 代挂凭据:不是 Kiro 号,无 Kiro token 概念——直接放行(token 用其 api_key 或空占位),
        // 真正的鉴权在透传时用 base_url + api_key 打上游。绝不进 Kiro 的 refresh/IdC 逻辑。
        if credentials.is_custom_api_credential() {
            let token = credentials.api_key.clone().unwrap_or_default();
            return Ok((credentials, token));
        }

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

        // 需刷新：委托带守卫的唯一刷新实现（? 保 RefreshTokenInvalidError 原样上抛）。
        // A3/C2 修复：传 Some(10)(与上面热路径进入条件 expiring_within(10) 同阈值)。这样
        // 过期风暴下多个请求排队等 refresh_lock 时,出队者拿锁后会二次检查——若前一个 waiter
        // 已刷新成功(token 不再 10min 内到期),直接返回 Skipped,**不再各自重打一次上游 refresh**
        // (消除惊群放大 429/refresh_failure_count)。传 None 会跳过该重检导致逐个重刷。
        self.refresh_token_locked(id, Some(10)).await?;

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
    /// 立即按当前 config 的 at-rest 加密开关重写凭据 + 回收站文件(明文↔密文)。
    /// 供 admin 改加密开关后即时落盘用。两文件都写,任一失败返回 Err。
    ///
    /// 返回 `Ok(true)`=真的重写了;`Ok(false)`=单对象(Single)格式,persist 是 no-op(加密对该格式
    /// 不生效)——调用方据此提示用户"当前为单凭据格式,加密未生效"。
    pub fn repersist_secrets(&self) -> anyhow::Result<bool> {
        // is_multiple_format=false 时 persist_credentials 直接 return Ok(false),加密对其无效。
        let wrote = self.persist_credentials()?;
        self.persist_trash()?;
        Ok(wrote)
    }

    fn persist_credentials(&self) -> anyhow::Result<bool> {
        use anyhow::Context;

        // 仅多凭据格式才回写
        if !self.is_multiple_format {
            // Single 格式 persist 是 no-op:若此时开着加密,磁盘凭据其实仍是明文——置健康标志 false,
            // 让 UI 红条如实告警"开了加密但未生效"(review MEDIUM:否则标志停在 true 误报健康)。
            if self.config().encrypt_credentials_at_rest {
                crate::common::recovery_metrics::set_at_rest_healthy(false);
            }
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
                    // ⭐ 同步禁用原因与时刻。不落盘这两项时，重启后加载路径会把所有禁用号
                    // 一律当成"手动禁用"，自动禁用原因（配额耗尽/被封/风控/连续失败）全部丢失，
                    // 且以 reason 为判据的自愈逻辑被一并击穿。
                    cred.disabled_reason = e.disabled_reason;
                    cred.disabled_at = e.disabled_at.clone();
                    cred
                })
                .collect()
        };

        // 序列化为 pretty JSON
        let json = serde_json::to_string_pretty(&credentials).context("序列化凭据失败")?;

        // at-rest 加密(开关关=明文原样;开=持久化密钥文件加密。加密失败自动回退明文不丢数据,
        // 并置健康标志=false 让 UI 可观测"开了加密但这次实为明文")。
        let enc = self.config().encrypt_credentials_at_rest;
        let key_path = crate::common::secret_store::key_path_for(path);
        let (bytes, encrypted) =
            crate::common::secret_store::encode_for_disk(json.as_bytes(), enc, &key_path);
        // 可观测:开了加密但实际没加密成(密钥文件读写失败等)→ 记健康标志,消除安全预期偏差。
        crate::common::recovery_metrics::set_at_rest_healthy(!enc || encrypted);

        // 原子写入文件（在 Tokio runtime 内使用 block_in_place 避免阻塞 worker）
        if tokio::runtime::Handle::try_current().is_ok() {
            tokio::task::block_in_place(|| write_atomic(path, &bytes))
                .with_context(|| format!("回写凭据文件失败: {:?}", path))?;
        } else {
            write_atomic(path, &bytes)
                .with_context(|| format!("回写凭据文件失败: {:?}", path))?;
        }

        tracing::debug!("已回写凭据到文件: {:?}(加密开关={} 实际加密={})", path, enc, encrypted);
        Ok(true)
    }

    /// 获取缓存目录（凭据文件所在目录）
    pub fn cache_dir(&self) -> Option<PathBuf> {
        self.credentials_path
            .as_ref()
            .and_then(|p| p.parent().map(|d| d.to_path_buf()))
    }

    /// 获取凭据文件完整路径（供 OTA 自重启把 `--credentials` 原样传给新进程）。
    pub fn credentials_path(&self) -> Option<PathBuf> {
        self.credentials_path.clone()
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
        let raw = match std::fs::read(&path) {
            Ok(c) => c,
            Err(_) => return, // 首次运行时文件不存在
        };
        if raw.iter().all(|b| b.is_ascii_whitespace()) {
            return;
        }
        // 透明解密(明文直通/密文解密),与 credentials 同口径;失败静默回退空(trash 非关键路径)。
        let key_path = crate::common::secret_store::key_path_for(&path);
        let content = match crate::common::secret_store::maybe_decrypt_to_string(&raw, &key_path) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("解密回收站失败,将忽略: {}", e);
                return;
            }
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

        // at-rest 加密(trash 也含完整凭据敏感字段,与 credentials 同开关同口径,共用同一密钥文件)。
        let enc = self.config().encrypt_credentials_at_rest;
        let key_path = crate::common::secret_store::key_path_for(&path);
        let (bytes, encrypted) =
            crate::common::secret_store::encode_for_disk(json.as_bytes(), enc, &key_path);

        // 原子写入文件（在 Tokio runtime 内使用 block_in_place 避免阻塞 worker）
        if tokio::runtime::Handle::try_current().is_ok() {
            tokio::task::block_in_place(|| write_atomic(&path, &bytes))
                .with_context(|| format!("回写回收站文件失败: {:?}", path))?;
        } else {
            write_atomic(&path, &bytes)
                .with_context(|| format!("回写回收站文件失败: {:?}", path))?;
        }

        tracing::debug!("已回写回收站到文件: {:?}(加密开关={} 实际加密={})", path, enc, encrypted);
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
                entry.request_count = s.request_count;
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
                            request_count: e.request_count,
                            last_used_at: e.last_used_at.clone(),
                        },
                    )
                })
                .collect()
        };

        match serde_json::to_string_pretty(&stats) {
            Ok(json) => {
                // 原子写(在 Tokio runtime 内用 block_in_place 避免 rename 重试 sleep 阻塞 worker,
                // 与 persist_credentials 同一惯例)。save_stats 常从 report_success/failure 的异步计费路径调。
                let write_result = if tokio::runtime::Handle::try_current().is_ok() {
                    tokio::task::block_in_place(|| write_atomic(&path, json.as_bytes()))
                } else {
                    write_atomic(&path, json.as_bytes())
                };
                if let Err(e) = write_result {
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
                // 账户级风控计数归零：这正是"连续零成功"判据的另一半——健康号哪怕
                // 偶发命中几次 403，一次成功就把计数清掉，永远到不了禁用阈值；
                // 只有**真死号**（实测成功率恒 0%）才能一路累加到阈值。
                entry.consecutive_suspicious = 0;
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

    /// 各号当前的生命周期累计 credit 花费（供面板做余额乐观修正）。
    ///
    /// 与 `balance_baselines()` 配对使用：两者之差 = 上次取余额真值**之后**新花掉的量。
    /// 只读快照，不持锁外泄。
    pub fn credits_used_snapshot(&self) -> HashMap<u64, f64> {
        self.entries
            .lock()
            .iter()
            .map(|e| (e.id, e.total_credits_used))
            .collect()
    }

    /// 各号在「上次取到余额真值」那一刻的 credit 花费基线。
    ///
    /// 即 `BalanceSnapshot::credits_used_at_cache`。余额加权分流已在用它
    /// （见 `balance_factor`），此处复用同一份数据供面板做乐观修正，
    /// 避免为展示再造一条并行的基线链路。
    pub fn balance_baselines(&self) -> HashMap<u64, f64> {
        self.balance_snapshots
            .read()
            .iter()
            .map(|(id, s)| (*id, s.credits_used_at_cache))
            .collect()
    }

    /// 回推余额快照(AdminService 每 30 分钟余额刷新后调用)。一次性替换全表(读多写少)。
    /// 号被删/禁用则不在表里,balance_factor 缺表 → 中性因子 1.0(不惩罚)。
    pub fn set_balance_snapshots(&self, snaps: HashMap<u64, BalanceSnapshot>) {
        *self.balance_snapshots.write() = snaps;
    }

    /// 余额加权因子 ∈ [floor, 1.0](balanced 选号微调用)。软偏置:余额多的号因子高(略多分),
    /// 少的低(略少分),长期把号池剩余额度拉平。**本地累加修正**:以快照剩余为基线,减去快照后
    /// 本地记的新增花费(total_credits_used 增量),估当前剩余,比纯 30 分钟旧快照准。
    /// 缺快照/上限<=0/加权关 → 返回 1.0 中性(不影响选号,退回纯 0.7.23 行为)。
    fn balance_factor(&self, id: u64, credits_used_now: f64) -> f64 {
        if !self.balance_weight_enabled.load(Ordering::Relaxed) {
            return 1.0;
        }
        let snap = {
            let map = self.balance_snapshots.read();
            match map.get(&id) {
                Some(s) => *s,
                None => return 1.0, // 缺快照(新号/未刷)→ 中性
            }
        };
        if !(snap.effective_limit > 0.0) {
            return 1.0;
        }
        // 本地累加修正:当前用量 - 快照基线 = 快照后新增花费(负数=快照更旧或已重置,钳到 0)。
        let spent_since = (credits_used_now - snap.credits_used_at_cache).max(0.0);
        let est_remaining = (snap.remaining_at_cache - spent_since).max(0.0);
        let frac = (est_remaining / snap.effective_limit).clamp(0.0, 1.0);
        // FLOOR 映射:factor = floor + (1-floor)×frac。floor=0.5:满额 1.0、半额 0.75、耗尽 0.5。
        // floor=100 → 因子恒 1.0(等于关闭)。整百分比转 [0,1]。
        let floor = (self.balance_weight_floor.load(Ordering::Relaxed).min(100) as f64) / 100.0;
        floor + (1.0 - floor) * frac
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

    /// 每个凭据的熔断/健康只读快照(供 admin 运维观测:circuit Open/HalfOpen + EWMA 健康分等)。
    /// 键=凭据 id。family_key 是族级(M365 同租户共享),故同族多号会拿到同一份快照(符合连坐语义)。
    /// 无健康记录(从未被选过/已淘汰)的号不在返回表中——调用方按缺省=Closed 满血处理。零上游只读内存。
    pub fn health_snapshots(&self) -> std::collections::HashMap<u64, crate::kiro::health::HealthSnapshot> {
        // 先在 entries 锁内只收集 (id, family_key) 轻量对,立即释放锁;再逐个查 health(独立 Mutex)。
        // 避免持 entries 锁跨多次 health.snapshot() 调用形成锁嵌套(与既有"health 锁外调用"约定一致)。
        let pairs: Vec<(u64, String)> = {
            let entries = self.entries.lock();
            entries.iter().map(|e| (e.id, e.credentials.family_key(e.id))).collect()
        };
        pairs
            .into_iter()
            .filter_map(|(id, key)| self.health.snapshot(&key).map(|snap| (id, snap)))
            .collect()
    }

    /// 把「凭据已被自动禁用」这一状态立即落盘到 credentials.json。
    ///
    /// 为什么必须单独做这件事：`save_stats_debounced()` 只写 `kiro_stats.json`，而 `StatsEntry`
    /// 仅含 success_count / total_credits_used / request_count / last_used_at —— **不含
    /// disabled / disabled_reason**。因此凡是"自动禁用"的路径（配额耗尽 / 账户封禁 /
    /// refreshToken 永久失效 / 连续失败 / 连续刷新失败）都必须额外调本函数，否则重启后
    /// 这些死号会以 enabled 状态回池，网关重新拿它们打上游、再走一遍禁用流程：
    /// invalid_grant 号会白白多消耗一次刷新往返，配额耗尽号会多打一次 402。
    ///
    /// 失败只告警不抛错：禁用已在内存生效，落盘失败不该影响本次请求的 failover 决策。
    /// （Single 对象格式的 credentials.json 下 persist_credentials 本身是 no-op，属预期。）
    fn persist_disabled_state(&self, id: u64) {
        if let Err(e) = self.persist_credentials() {
            tracing::warn!(
                "凭据 #{} 已自动禁用，但持久化失败：{}。重启后该号会以启用状态回池并重新走一遍禁用流程。",
                id,
                e
            );
        }
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
                entry.disabled_at = Some(Utc::now().to_rfc3339());
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
        // 自动禁用必须落盘：save_stats_debounced 写的是 kiro_stats.json，其 StatsEntry 只含
        // success_count/total_credits_used/request_count/last_used_at，**不含 disabled/disabled_reason**。
        // 不落盘则重启后该号以 enabled 回池、重新走一遍失败→禁用流程（白耗配额与上游请求）。
        if disabled_now {
            self.persist_disabled_state(id);
        }
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
        // 健康：必须用 family_key —— 这是 HealthTracker 的**唯一合法键**。
        //
        // ⚠️ 历史 bug：这里曾硬编码 format!("cred:{}", id)，而**读侧**全部用 family_key：
        //   选号 sort_key(p_avail) / report_success / report_family_suspicious / health_snapshots。
        // social/idc/api_key 的 family_key 恰好就是 "cred:{id}"，所以看起来正常；但 external_idp
        // (M365) 的 family_key 是 "m365:{tenant}"、AWS 兜底是 "aws:{account}"，于是这些号的裸 429
        // 全部写进一个**从不被任何人读取**的影子条目：ewma_429 / consecutive_429 / TRIP_THRESHOLD
        // 跳闸统统失效 → M365 号被 429 打爆也永远不会被熔断或降权，且面板 health 快照恒显示
        // consecutive_429=0。现有测试用的是 social 默认凭据（两键恰好相等），所以测不出来。
        //
        // 注意 family_key_of 内部会取 entries 锁，故必须在**锁外**调用（本函数此处已在锁外）。
        self.health.on_429(&self.family_key_of(id));
    }

    /// 报告凭据遇到上游 **5xx**，设 [`CooldownReason::ServerError`] 短冷却（30s，自动恢复）。
    ///
    /// 为什么需要它：此前非 429 的 5xx 只在 provider 里 sleep 200ms~2s 就换号，**不设任何
    /// 冷却**，失败的号下一轮立刻又可能被选中。于是上游 500 风暴时（实测一小时 408 次 500）
    /// 请求在同一批坏号之间来回打，把重试预算烧光却始终打在同一批号上。
    /// `CooldownReason::ServerError` 这个枚举早就定义好了（cooldown.rs 含 30s 时长与
    /// is_auto_recoverable=true），但在生产路径上**从未被设置过**——唯一调用方是 admin
    /// 的手工冷却接口。这里把它接上。
    ///
    /// 与 429 的区别：5xx 多为上游整体故障而非该号的问题，故只设短冷却、不计永久失败、
    /// 不碰 rate_limiter，也不动 health 的 429 计数（那是限流信号，不该被 5xx 污染）。
    pub fn report_server_error(&self, id: u64) {
        if self.cooldown_enabled.load(Ordering::Relaxed) {
            let dur = self
                .cooldown
                .set_transient_cooldown(id, CooldownReason::ServerError);
            tracing::warn!("凭据 #{} 遇上游 5xx，冷却 {:?}（自动恢复）", id, dur);
        }
    }

    /// 报告凭据触发**账户级可疑活动风控**（`suspicious activity`+`temporary limits`，
    /// 即上游 403 `TEMPORARILY_SUSPENDED`）。
    ///
    /// 三件事，**职责各自独立、互不 gate**：
    /// 1. **计数**（恒执行）：`consecutive_suspicious += 1`，达
    ///    [`MAX_CONSECUTIVE_SUSPICIOUS_BEFORE_DISABLE`] 即自动禁用该号。
    /// 2. **冷却 + 族级健康惩罚**（受 `cooldown_enabled` 门控）：走
    ///    [`CooldownReason::SuspiciousActivity`] 的递增退避（基线 20s → 上限 30min）。
    /// 3. **限速器退避**（受 `rate_limit_enabled` 门控）。
    ///
    /// ## 为什么计数与自动禁用**必须**在 `cooldown_enabled` 之外（本轮修复的核心）
    ///
    /// 此前整个函数体被一个 `if self.cooldown_enabled` 包住，于是关掉"冷却"这个
    /// 开关会连带关掉**识别坏号**的能力——三种不相关的职责被一个开关耦合。线上
    /// `cooldownEnabled=false` 时实测：8 个成功率恒 0% 的死号跑了几小时，仍全部
    /// `disabled=false` / `failureCount=0` / `healthStatus=null`，巡检显示"43 个号
    /// 全可用"（假绿），而每条客户端请求都要在它们身上白撞一遍 → 最坏 43 次上游
    /// 调用、耗尽 45s 墙钟预算才失败。用户体感就是"网关很慢"。
    ///
    /// 而且旧判据 `cooldown.trigger_count(id) >= 10` 本身也不可达：`report_success`
    /// 调 `clear_cooldown` 会 `entries.remove()` 删掉冷却条目、`trigger_count` 归零，
    /// 半死号（偶尔成功）永远到不了 10。故改用挂在凭据条目上的
    /// `consecutive_suspicious`（详见其字段说明）。
    ///
    /// ## 为什么阈值是"连续零成功"而非"见过 403"
    ///
    /// 403 是**临时**态，历史上按永久封禁处理造成过生产事故（见
    /// `endpoint/mod.rs::default_is_account_suspended`）。实测健康号（成功率
    /// 90~100%）也会偶发命中 403，只有真死号才**连续**命中且期间零成功。
    /// 任意一次成功即清零 → 健康号永不误禁。
    pub fn report_suspicious_activity(&self, id: u64) {
        // ── 1. 计数 + 自动禁用（恒执行，不受任何冷却/限速开关影响）
        let mut disabled_now = false;
        let mut hit_count = 0u32;
        {
            let mut entries = self.entries.lock();
            if let Some(entry) = entries.iter_mut().find(|e| e.id == id) {
                if !entry.disabled {
                    entry.consecutive_suspicious += 1;
                    hit_count = entry.consecutive_suspicious;
                    if self.auto_disable_suspicious.load(Ordering::Relaxed)
                        && hit_count >= MAX_CONSECUTIVE_SUSPICIOUS_BEFORE_DISABLE
                    {
                        entry.disabled = true;
                        entry.disabled_reason = Some(DisabledReason::SuspiciousActivityAuto);
                        entry.disabled_at = Some(Utc::now().to_rfc3339());
                        disabled_now = true;
                        crate::common::recovery_metrics::bump_dead_token_disabled();
                    }
                }
            }
        }
        if disabled_now {
            tracing::error!(
                "凭据 #{} 连续 {} 次账户级风控且期间零成功，判定为死号并自动禁用\
                 （SuspiciousActivityAuto）；移出调度以免每个请求都在它身上白撞",
                id,
                hit_count
            );
            // 清亲和：否则绑定该号的会话会反复重选到已禁用凭据。
            self.affinity.remove_by_credential(id);
            // 必须落盘：save_stats_debounced 写的 StatsEntry 不含 disabled/disabled_reason，
            // 不落盘则重启后死号以 enabled 回池、重走一遍禁用流程（白耗上游请求）。
            self.persist_disabled_state(id);
        } else if hit_count > 0 {
            tracing::debug!(
                "凭据 #{} 账户级风控连续第 {}/{} 次（一次成功即清零）",
                id,
                hit_count,
                MAX_CONSECUTIVE_SUSPICIOUS_BEFORE_DISABLE
            );
        }

        // ── 2. 冷却 + 族级健康惩罚（受 cooldown_enabled 门控）
        if self.cooldown_enabled.load(Ordering::Relaxed) {
            let dur = self
                .cooldown
                .set_cooldown(id, CooldownReason::SuspiciousActivity);
            crate::common::recovery_metrics::bump_cooldown_triggered();
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
        }

        // ── 3. 限速器退避（受 rate_limit_enabled 门控）
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

    /// 该模型在**任意**凭据上的模型级黑名单最短剩余 TTL。全池都没被该模型加黑时返回 `None`。
    ///
    /// 用途：`WaitOutcome::NoCandidate` 且仍有未禁用号时，需要给客户端一个**准确的**退避秒数。
    /// 黑名单是限时态（`MODEL_BLOCK_TTL`），最短剩余即"最早有号重新可试探"的时刻——比固定值更贴合
    /// 实际恢复时间，避免客户端过早重发（撞进同一道硬门）或过晚重发（白等）。
    ///
    /// 只读不清理：调用点在错误路径上，惰性清理由 `is_model_blocked` 负责，此处避免额外写锁语义。
    fn model_block_min_remaining(&self, model: Option<&str>) -> Option<StdDuration> {
        let model = model.filter(|m| !m.is_empty())?;
        let bl = self.model_blocklist.lock();
        bl.iter()
            .filter(|((_, m), _)| m == model)
            .filter_map(|(_, &t)| MODEL_BLOCK_TTL.checked_sub(t.elapsed()))
            .min()
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
            entry.disabled_at = Some(Utc::now().to_rfc3339());
            entry.last_used_at = Some(Utc::now().to_rfc3339());
            // 设为阈值，便于在管理面板中直观看到该凭据已不可用
            entry.failure_count = MAX_FAILURES_PER_CREDENTIAL;
            crate::common::recovery_metrics::bump_dead_token_disabled();

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
        // 立即落盘禁用状态（见 persist_disabled_state 的说明）：否则重启后该号回池，
        // 又会打一次 402 MONTHLY_REQUEST_COUNT 才重新被禁用。
        self.persist_disabled_state(id);
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
            entry.disabled_at = Some(Utc::now().to_rfc3339());
            entry.last_used_at = Some(Utc::now().to_rfc3339());
            entry.failure_count = MAX_FAILURES_PER_CREDENTIAL;
            crate::common::recovery_metrics::bump_dead_token_disabled();

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
        // 封禁已禁用该凭据，清除其会话亲和性绑定
        self.affinity.remove_by_credential(id);
        self.save_stats_debounced();
        // 立即落盘禁用状态：AccountSuspended 不可自动恢复（等人工），重启后回池毫无意义，
        // 只会再打一次上游确认被封。
        self.persist_disabled_state(id);
        result
    }

    /// 报告指定凭据刷新 Token 失败。
    ///
    /// 连续刷新失败达到阈值后禁用凭据并切换，阈值内保持当前凭据不切换，
    /// 与 API 401/403 的累计失败策略保持一致。
    pub fn report_refresh_failure(&self, id: u64) -> bool {
        let disabled_now;
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
            disabled_now = true;

            entry.disabled = true;
            entry.disabled_reason = Some(DisabledReason::TooManyRefreshFailures);
            entry.disabled_at = Some(Utc::now().to_rfc3339());

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
        // 连续刷新失败达阈值而被禁用时落盘（阈值内的失败只累计计数、不落盘，避免高频写）。
        if disabled_now {
            self.affinity.remove_by_credential(id);
            self.persist_disabled_state(id);
        }
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
            entry.disabled_at = Some(Utc::now().to_rfc3339());
            crate::common::recovery_metrics::bump_dead_token_disabled();

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
        // refreshToken 永久失效（invalid_grant）不可能自愈，必须落盘：否则每次重启都会
        // 拿着已作废的 refreshToken 再打一次上游刷新，白耗一次往返且在上游留下失败记录。
        self.affinity.remove_by_credential(id);
        self.persist_disabled_state(id);
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
        // 自动路由需要全局默认端点名参与解析（见 effective_endpoint）。
        let cfg = self.config.load();

        ManagerSnapshot {
            entries: entries
                .iter()
                .map(|e| CredentialEntrySnapshot {
                    id: e.id,
                    priority: e.credentials.priority,
                    rpm_limit: e.credentials.rpm_limit,
                    allowed_models: e.credentials.allowed_models.clone(),
                    tested_models: e.credentials.tested_models.clone(),
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
                    base_url: e.credentials.base_url.clone(),
                    request_limit: e.credentials.request_limit,
                    request_count: e.request_count,
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
                    // 走单一收口 DisabledReason::as_str（回收站展示也用它，避免两份 match 漂移）。
                    disabled_reason: e.disabled_reason.map(|r| r.as_str().to_string()),
                    // 与 disabled_reason 成对下发（此前整条链都没接，面板恒 null）。
                    disabled_at: e.disabled_at.clone(),
                    endpoint: e.credentials.endpoint.clone(),
                    // 实际生效的端点（含自动路由结果）：面板要能区分"我固定了 cli"与
                    // "系统替我自动选了 cli"，否则用户看不出 ksk_ 号究竟走了哪条协议。
                    effective_endpoint: e
                        .credentials
                        .effective_endpoint(&cfg.default_endpoint)
                        .to_string(),
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
                // 启用时重置全部进程内惩罚计数（单一收口，含 consecutive_suspicious）。
                entry.clear_transient_counters();
                entry.disabled_reason = None;
            } else {
                entry.disabled_reason = Some(DisabledReason::Manual);
                entry.disabled_at = Some(Utc::now().to_rfc3339());
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
            // 服务端防呆:clamp 到合理上界(不信任前端校验,直打 API 的负值/极值也自动修补)。
            entry.credentials.priority = priority.min(MAX_PRIORITY);
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
            // 0 归一为 None(继承全局),避免存 Some(0) 语义歧义;非 0 clamp 到合理上界
            // (服务端防呆:直打 API 的 u32 极值也自动修补,不信任前端校验)。
            entry.credentials.rpm_limit = rpm_limit.filter(|&v| v > 0).map(|v| v.min(MAX_RPM_LIMIT));
        }
        self.persist_credentials()?;
        Ok(())
    }

    /// 设置凭据级端点（走哪套 Kiro 协议）。`None`/空串 = 清除显式固定，回到**自动路由**
    /// （`ksk_` 号 → `cli`，其余 → `config.defaultEndpoint`）。即时生效于下次请求。
    ///
    /// 端点名必须已注册，否则拒绝——存了不存在的名字会让该号的每个请求都在热路径上
    /// 拿到「未知端点」错误（provider 的 `endpoint_for` 返 Err），等于静默废号。
    pub fn set_credential_endpoint(&self, id: u64, endpoint: Option<String>) -> anyhow::Result<()> {
        let cleaned = endpoint
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        if let Some(ref name) = cleaned {
            if crate::kiro::endpoint::build(name).is_none() {
                anyhow::bail!("未知端点: {}", name);
            }
        }
        {
            let mut entries = self.entries.lock();
            let entry = entries
                .iter_mut()
                .find(|e| e.id == id)
                .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?;
            entry.credentials.endpoint = cleaned;
        }
        self.persist_credentials()?;
        Ok(())
    }

    /// 设置凭据级「允许模型」白名单（成本安全硬门）。空列表归一为 None（不限制）。
    /// 值为 kiro modelId（如 `deepseek-3.2`/`claude-opus-4.8`）。持久化到凭据源文件。
    pub fn set_allowed_models(&self, id: u64, models: Option<Vec<String>>) -> anyhow::Result<()> {
        {
            let mut entries = self.entries.lock();
            let entry = entries
                .iter_mut()
                .find(|e| e.id == id)
                .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?;
            // 去空白 + 去空项；空列表归一为 None（= 不限制，兼容"清空白名单"操作）
            let cleaned = models.map(|list| {
                list.into_iter()
                    .map(|m| m.trim().to_string())
                    .filter(|m| !m.is_empty())
                    .collect::<Vec<_>>()
            });
            entry.credentials.allowed_models = cleaned.filter(|l| !l.is_empty());
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
            // 去空白;空则清除;超长按字符截断到上界(服务端防呆:与前端 maxLength 一致,
            // 直打 API 的超长值也自动修补,按 char 边界截断避免切坏多字节 UTF-8)。
            entry.credentials.name = name
                .map(|s| s.trim().chars().take(MAX_NAME_CHARS).collect::<String>())
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

    /// 修改自定义 API(代挂透传)凭据的 base_url / api_key / request_limit(Admin API)。
    ///
    /// ⚠️ 安全命门(隔离铁律 4):第一行必须 gate `is_custom_api_credential()`——给 Kiro 号写
    /// base_url 会让它被 `is_custom_api_credential`(判定含 `base_url.is_some()`)误判进透传池
    /// `select_custom_api`,彻底破坏两选号池隔离。非 custom_api 号直接 bail。
    ///
    /// 三态语义(对齐 set_credential_proxy):
    /// - `base_url`:None=不改 / Some(非空)=trim 更新 / Some(空)=bail(base_url 是透传必填,不许清空)。
    /// - `api_key`:None=不改 / Some(空)=清除 / Some(非空)=更新。
    /// - `request_limit`:None=不改 / Some(0)=归一为「不限」(存 None) / Some(>0)=更新。
    /// - `reset_count`:true 时把 request_count 归零(换上游/换 key 时由前端勾选,避免旧计数残留触顶)。
    pub async fn set_custom_api_config(
        &self,
        id: u64,
        base_url: Option<String>,
        api_key: Option<String>,
        request_limit: Option<u64>,
        reset_count: bool,
    ) -> anyhow::Result<()> {
        // SSRF 写入校验(主防线)：DNS 解析不能在 entries 锁临界区内做，故取锁前先校验。
        // 仅当传入了新 base_url 才校验（None=不改）。
        if let Some(url) = base_url.as_deref() {
            let trimmed = url.trim();
            if trimmed.is_empty() {
                anyhow::bail!("base_url(上游地址)不能为空");
            }
            validate_custom_api_base_url(trimmed).await?;
        }
        {
            let mut entries = self.entries.lock();
            let entry = entries
                .iter_mut()
                .find(|e| e.id == id)
                .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?;
            // 隔离命门:仅自定义 API 凭据可改这些字段。
            if !entry.credentials.is_custom_api_credential() {
                anyhow::bail!("仅自定义 API(代挂透传)凭据可修改 base_url / api_key / 请求上限");
            }
            // base_url:必填,不许清空;None 表示不改。（空/SSRF 校验已在锁外完成）
            if let Some(url) = base_url {
                let trimmed = url.trim();
                entry.credentials.base_url = Some(trimmed.trim_end_matches('/').to_string());
            }
            // api_key:None 不改 / 空清除 / 非空更新。
            if let Some(key) = api_key {
                entry.credentials.api_key = Some(key.trim().to_string()).filter(|s| !s.is_empty());
            }
            // request_limit:None 不改 / 0 归一为不限 / >0 更新。
            if let Some(limit) = request_limit {
                entry.credentials.request_limit = Some(limit).filter(|&v| v > 0);
            }
            // 换上游/换 key 后可选归零调用次数,避免旧计数残留立即触发请求上限。
            if reset_count {
                entry.request_count = 0;
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
            // 若该号是因达到「请求上限」被自动禁用(custom_api 代挂计数,见 RequestLimitReached),
            // 复活时必须同时把 request_count 清零——否则计数仍 >= limit,下一次成功调用即再次触顶
            // 自动禁用,"重置并启用"形同虚设(复活即秒禁)。其它禁用原因不动计数。
            if entry.disabled_reason == Some(DisabledReason::RequestLimitReached) {
                entry.request_count = 0;
            }
            // 全部进程内惩罚计数走单一收口（含此前漏清的 consecutive_suspicious）。
            entry.clear_transient_counters();
            entry.disabled = false;
            entry.disabled_reason = None;
        }
        // 重置并启用时一并清 per-id 冷却/退避残留,让「重置」名副其实(否则刚启用又被残留退避跳过)。
        // 在 entries 锁外调用(rate_limiter/cooldown 各有独立锁)。
        self.cooldown.clear_cooldown(id);
        self.rate_limiter.reset(id);
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
        let usage_limits = match get_usage_limits(&credentials, &cfg, &token, effective_proxy.as_ref()).await {
            Ok(u) => u,
            Err(e) => {
                // 验活标记（E）：external_idp 号若 403 FEATURE_NOT_SUPPORTED（该 region profile 未开通），
                // 置位供刷新路径（D）只对**确认坏的号** reprobe 重选可用 region，健康号不额外探测（省成本）。
                if e.to_string().contains("FEATURE_NOT_SUPPORTED") {
                    let entries = self.entries.lock();
                    if let Some(entry) = entries.iter().find(|e| e.id == id) {
                        entry
                            .last_usage_403_feature_not_supported
                            .store(true, Ordering::Relaxed);
                    }
                }
                return Err(e);
            }
        };

        // 成功查询 → 清除 FEATURE_NOT_SUPPORTED 标记（该号当前 region profile 已可用）。
        {
            let entries = self.entries.lock();
            if let Some(entry) = entries.iter().find(|e| e.id == id) {
                entry
                    .last_usage_403_feature_not_supported
                    .store(false, Ordering::Relaxed);
            }
        }

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
        // 自定义 API 透传号:不能走 ensure_valid_token(会用 api_key 当 Kiro token 打
        // runtime.kiro.dev/generateAssistantResponse 必 401/403,把活号误判死号)。改走
        // 透传专属探测:打它自己的 base_url,只看 header status(隔离铁律,不进 Kiro 池、不解析流)。
        let custom_cred = {
            let entries = self.entries.lock();
            entries
                .iter()
                .find(|e| e.id == id)
                .filter(|e| e.credentials.is_custom_api_credential())
                .map(|e| e.credentials.clone())
        };
        if let Some(cred) = custom_cred {
            return self.deep_verify_custom_api(&cred).await;
        }

        // 双检刷新收敛到 ensure_valid_token：credentials 为刷新后最新快照
        // （含可能动态解析到的 profileArn），供后续 region / machine_id / 请求头使用。
        let (credentials, token) = self.ensure_valid_token(id).await?;

        let cfg = self.config.load();
        // URL / 请求头 / body 加工全部交给**该凭据实际会用的端点实现**，与对话热路径
        // (`KiroProvider::endpoint_for`) 同一口径。
        //
        // 为何不再手搓 `runtime.{region}.kiro.dev`：CLI(ksk_)号必须走
        // `q.{region}.amazonaws.com` 服务根 + X-Amz-Target + 不带 profileArn，打 IDE 端点
        // 稳定 403。而本函数把 403 当「权限被拒/疑似封号」上报，classify_balance_error 据此
        // **自动禁用凭据** → 一个完全健康的 ksk_ 号会被验活自己弄死。交给端点抽象后，
        // 将来新增端点也不必再改这里（历史上这里就漏迁过一次 host）。
        let endpoint = crate::kiro::endpoint::for_credentials(&credentials, &cfg.default_endpoint);
        let machine_id = machine_id::generate_from_credentials(&credentials, &cfg);
        let rctx = crate::kiro::endpoint::RequestContext {
            credentials: &credentials,
            token: &token,
            machine_id: &machine_id,
            config: &cfg,
            // 验活不涉及 1M 变体（探测体只有一句 "hi"），固定 false。
            is_1m: false,
        };
        let url = endpoint.api_url(&rctx);

        // 构建最小请求体（故意不完整——缺 modelId 等必填字段，只为触发认证/suspend 检查）。
        // profileArn 由端点的 transform_api_body 按各自规则注入（IDE 注入、CLI 绝不注入）。
        let body = serde_json::json!({
            "conversationState": {
                "conversationId": uuid::Uuid::new_v4().to_string(),
                "currentMessage": {
                    "userInputMessage": {
                        "content": "hi"
                    }
                }
            }
        })
        .to_string();
        let body = endpoint.transform_api_body(&body, &rctx);

        let effective_proxy = credentials.effective_proxy(self.proxy.as_ref());
        let client = build_client(effective_proxy.as_ref(), 30, cfg.tls_backend)?;

        let request = endpoint.decorate_api(
            client
                .post(&url)
                .header("content-type", endpoint.content_type()),
            &rctx,
        );

        let response = request.body(body).send().await?;
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

    /// 自定义 API 透传号的测活探测:打它自己的 base_url(Anthropic messages 端点),
    /// 用它的 api_key,发一个极小请求看 header status 判活。
    ///
    /// **隔离铁律**:走 base_url 独立 client(非 Kiro 选号池)、非流式短超时、**只看 header status
    /// 绝不解析响应流**。判定按透传 failover 同口径:401=key 失效 / 402·403=额度耗尽 / 429=限流(视为活)
    /// / 200·400=可达有效 / 5xx·网络=上游不可用。bail 文案复用现有关键字,免改 classify_balance_error。
    async fn deep_verify_custom_api(&self, credentials: &KiroCredentials) -> anyhow::Result<()> {
        let base = match credentials.base_url.as_deref() {
            Some(b) if !b.trim().is_empty() => b.trim_end_matches('/').to_string(),
            _ => bail!("自定义 API 凭据缺少 base_url"),
        };
        let url = if base.ends_with("/v1") || base.contains("/v1/") {
            format!("{base}/messages")
        } else {
            format!("{base}/v1/messages")
        };
        // 非流式短超时(30s),勿用流式 720s。走该号 effective_proxy。
        // **禁重定向**(SSRF 纵深):防公网中转站 302→内网/元数据的盲 SSRF(端口探测)。
        let effective_proxy = credentials.effective_proxy(self.proxy.as_ref());
        let client = build_client_no_redirect(effective_proxy.as_ref(), 30, self.config.load().tls_backend)?;

        // 极小 Anthropic 探测体(max_tokens:1),只为触发认证/额度检查。
        let probe = serde_json::json!({
            "model": "claude-3-5-sonnet-20241022",
            "max_tokens": 1,
            "messages": [{"role": "user", "content": "ping"}]
        });
        let mut req = client
            .post(&url)
            .header("content-type", "application/json")
            .header("anthropic-version", "2023-06-01");
        if let Some(key) = credentials.api_key.as_deref().filter(|k| !k.is_empty()) {
            req = req
                .header("x-api-key", key)
                .header("Authorization", format!("Bearer {key}"));
        }
        let resp = match req.body(probe.to_string()).send().await {
            Ok(r) => r,
            Err(e) => bail!("上游不可达: {}", e),
        };
        let code = resp.status().as_u16();
        // 只看 status,不解析流(隔离铁律)。
        match code {
            401 => bail!("凭证已过期或无效 (401): 上游 API key 失效"),
            402 | 403 => bail!("额度已用尽或权限不足 ({}): 上游拒绝", code),
            429 => Ok(()),                          // 限流=号仍活,只是暂时被限
            c if (200..300).contains(&c) => Ok(()), // 可达有效
            // 其余 4xx(400 请求校验/404 模型名不认/422 等):上游可达且已通过认证(否则 401/403),
            // 号本身有效——不因探测体的 model 字段被某中转站拒就误判死号。只 401/402/403 + 5xx 判死。
            c if (400..500).contains(&c) => Ok(()),
            c if (500..600).contains(&c) => bail!("上游服务不可用 ({})", c),
            c => bail!("上游返回异常状态 ({})", c),
        }
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

        // 打标签持久化：把本轮探测结果写入该凭据的 tested_models（覆盖旧结果），
        // 下次进"测试可用模型"页无需重测即可看到该号测过什么、结果如何。
        {
            let now = chrono::Utc::now().to_rfc3339();
            let tested: Vec<crate::kiro::model::credentials::TestedModel> = detail
                .iter()
                .map(|(model, status, _credits)| crate::kiro::model::credentials::TestedModel {
                    model: model.clone(),
                    status: status.clone(),
                    tested_at: now.clone(),
                })
                .collect();
            let mut entries = self.entries.lock();
            if let Some(entry) = entries.iter_mut().find(|e| e.id == id) {
                entry.credentials.tested_models = Some(tested);
            }
        }
        if let Err(e) = self.persist_credentials() {
            tracing::warn!("持久化探测结果(tested_models)失败: {e}");
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
        // 与对话热路径同一端点抽象：CLI(ksk_)号走 q.{region}.amazonaws.com + X-Amz-Target，
        // 不带 profileArn。若继续硬编码 IDE host，CLI 号每个模型都会 401/403，而本函数把
        // 401/403 当"认证/账号级问题"直接 bail → 整轮探测中止并向面板报"账号有问题"。
        let endpoint = crate::kiro::endpoint::for_credentials(&credentials, &cfg.default_endpoint);
        let machine_id = machine_id::generate_from_credentials(&credentials, &cfg);
        let rctx = crate::kiro::endpoint::RequestContext {
            credentials: &credentials,
            token: &token,
            machine_id: &machine_id,
            config: &cfg,
            // 探测按原生 modelId 直发，不涉及 `[1m]` 变体。
            is_1m: false,
        };
        let url = endpoint.api_url(&rctx);

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
        let kiro_req = serde_json::to_value(&crate::kiro::model::requests::kiro::KiroRequest {
            conversation_state: conv,
            profile_arn: None,
        })?;
        // profileArn 注入与端点特有 body 加工统一交给端点实现（IDE 注入 arn，CLI 注入
        // agentTaskType/agentMode 且**绝不**注入 arn）。手写 arn 注入会让 CLI 号 403。
        let body = endpoint.transform_api_body(&kiro_req.to_string(), &rctx);

        let effective_proxy = credentials.effective_proxy(self.proxy.as_ref());
        // 探测要消费完整生成流,用 read_timeout(空闲间隔)而非总超时,否则慢模型生成中途被 30s
        // 总超时掐断→误判 unknown/失败(与 mid-response 同类)。空闲上限 60s:探测请求 content="hi"
        // 生成极短,只要上游在吐数据就不该超时;真卡死 60s 无数据才放弃,比对话路径更快止损。
        let client = build_streaming_client(effective_proxy.as_ref(), 60, cfg.tls_backend)?;
        let request = endpoint.decorate_api(
            client
                .post(&url)
                .header("content-type", endpoint.content_type()),
            &rctx,
        );

        // 单个模型探测的网络错误不应中止整轮：吞掉转成 None(unknown) 继续探下一个。
        let response = match request.body(body).send().await {
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
        self.add_credential_inner(new_cred, false).await
    }

    /// 添加凭据，**刻意允许与池中已有号重复**（同一账号多开）。
    ///
    /// # 用途
    ///
    /// 同一个账号导入多份、每份配不同 `machineId` 与不同代理，让上游把它们看成
    /// 「同一用户的多台设备」，以试探能否提高并发。三个前提都已现成：
    ///
    /// - **machineId 天然不同**：`generate_from_credentials` 对 api_key 号是
    ///   `sha256("KiroAPIKey/" + key)`（确定性），故 N 份派生出同一个指纹，随后
    ///   入池处的撞车检测（见本函数下方 `collides` 分支）把第 2..N 份轮换成独立随机值。
    /// - **不会被族级连坐**：`family_key()` 对 api_key/idc/social 返回 `cred:{id}`，
    ///   每份独立成族。只有 M365 external_idp 才共享 `m365:{tenant}`。
    /// - **每份可独立走代理**：`effective_proxy(global)` 每号可覆盖全局，且 provider
    ///   的 Client 缓存 key 就是 effective proxy，故每份各自建连接池、各自出口 IP。
    ///
    /// # ⚠️ 与去重保护的关系
    ///
    /// 去重（`kiroApiKey 重复` / `refreshToken 重复`）本是防**误操作**重复上号的护栏，
    /// 这里绕过它是**显式意图**，故只在调用方明确要求多份时使用，绝不设为默认 ——
    /// 否则误双击上号就会静默多出一条号，而多开的号共用同一份上游配额，
    /// 悄悄多出来的那条会稀释调度而不增加容量。
    ///
    /// # ⚠️ RPM 语义（会影响实验结论）
    ///
    /// `rpm_limit` 是**每凭据**的。导 N 份则网关侧放行量变为 N × 每份上限。
    /// 若上游实际按**账号**限流（而非按设备），多开只是把同一份配额切成 N 刀、
    /// 并更早撞上惩罚窗口。故每份的 `rpm_limit` 应在导入后按账号实测上限 ÷ N 调整
    /// （该字段在面板凭据卡片里可逐号设置，`0` 归一为 None＝继承全局）。
    pub async fn add_credential_allowing_duplicate(
        &self,
        new_cred: KiroCredentials,
    ) -> anyhow::Result<u64> {
        self.add_credential_inner(new_cred, true).await
    }

    async fn add_credential_inner(
        &self,
        new_cred: KiroCredentials,
        allow_duplicate: bool,
    ) -> anyhow::Result<u64> {
        // 1. 基本验证
        if new_cred.is_custom_api_credential() {
            // 自定义 API 代挂:只需 base_url(Anthropic 兼容上游),不需要 refreshToken/kiroApiKey。
            let base = new_cred
                .base_url
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("自定义 API 凭据缺少 base_url"))?;
            if base.trim().is_empty() {
                anyhow::bail!("自定义 API 的 base_url 为空");
            }
            // SSRF 写入校验(主防线):解析最终透传 URL 的目标 IP,禁内网/环回/链路本地/元数据。
            validate_custom_api_base_url(base).await?;
        } else if new_cred.is_api_key_credential() {
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
        //
        // `allow_duplicate` 为 true 时整段跳过 —— 那是「同一账号多开」的显式意图
        // （见 `add_credential_allowing_duplicate` 的文档）。写成 if 链的第一个分支
        // 而不是把下面三段包进 `if !allow_duplicate {}`：后者要对 68 行做整体重新缩进，
        // 而本仓明确禁止用脚本批量改代码（历史事故：正则改动造成 209 个编译错误）。
        if allow_duplicate {
            // 刻意不去重。machineId 的撞车轮换仍会执行（见下方 `collides` 分支），
            // 故 N 份副本各自拿到独立设备指纹。
        } else if new_cred.is_custom_api_credential() {
            // 自定义 API 去重键 = base_url + api_key(允许同一上游用不同 key,或不同上游)。
            let dup_key = format!(
                "{}|{}",
                new_cred.base_url.as_deref().unwrap_or(""),
                new_cred.api_key.as_deref().unwrap_or("")
            );
            let new_hash = sha256_hex(&dup_key);
            let duplicate_exists = {
                let entries = self.entries.lock();
                entries.iter().any(|entry| {
                    if !entry.credentials.is_custom_api_credential() {
                        return false;
                    }
                    let k = format!(
                        "{}|{}",
                        entry.credentials.base_url.as_deref().unwrap_or(""),
                        entry.credentials.api_key.as_deref().unwrap_or("")
                    );
                    sha256_hex(&k) == new_hash
                })
            };
            if duplicate_exists {
                anyhow::bail!("凭据已存在（相同 base_url + api_key）");
            }
        } else if new_cred.is_api_key_credential() {
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

        // 3. 验证凭据有效性（API Key / 自定义 API 无需 Kiro 网络刷新）
        let mut validated_cred = if new_cred.is_api_key_credential()
            || new_cred.is_custom_api_credential()
        {
            new_cred.clone()
        } else {
            let effective_proxy = new_cred.effective_proxy(self.proxy.as_ref());
            let cfg = self.config.load_full();
            refresh_token(&new_cred, &cfg, effective_proxy.as_ref()).await?
        };

        // 4. 分配新 ID：进程内单调计数器，fetch_add 原子取号，永不回退、永不复用（见 next_id 字段说明）。
        //    【为何不再扫 entries ∪ trash 取 max+1】旧算法在「删号 → purge 出回收站 → 再加号」时
        //    max+1 会回落复用刚被清除的 id，让新号继承死号残留的 cooldown/model_blocklist 内存态。
        //    计数器在启动时已初始化为 max(entries ∪ trash)+1 并随每次分配单调递增，天然 ≥ 任何现存
        //    /回收站 id，既杜绝复用又不会与 trash 恢复的号撞号。
        let new_id = self.next_id.fetch_add(1, Ordering::AcqRel);

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
        // 【统一上号治理·收口铁律】任何号进池那一刻,强制把 region/auth_region 同步成 profileArn
        // 内的 region——无论它来自哪条上号路径(external_idp 验活选/idc 探测/social token 解析)、
        // 无论建号前 region 填得对不对,进池即 region↔ARN 自洽,杜绝错配 → 400 Improperly formed。
        // 无 profileArn 的号(api_key/custom_api/待后台回补的 idc)是安全 no-op(返回 false)。
        if validated_cred.sync_region_from_arn() {
            tracing::info!(
                "上号收口:凭据 region 已随 profileArn 同步为 {}",
                validated_cred.region.as_deref().unwrap_or("?")
            );
        }
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

        // Track whether a machineId collision was detected and rotation occurred,
        // so the persist-failure warning can report the correct risk accurately.
        let mut mid_was_rotated = false;
        {
            let mut entries = self.entries.lock();
            // 指纹去重(防关联):新号指纹若与池中已有号撞车,轮换成独立随机指纹,避免上游
            // 按设备指纹把两个号关联封禁。与 reconcile 的 machine_id 碰撞轮换逻辑一致。
            if let Some(mid) = validated_cred.machine_id.clone() {
                let collides = entries
                    .iter()
                    .any(|e| e.credentials.machine_id.as_deref() == Some(mid.as_str()));
                if collides {
                    mid_was_rotated = true;
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
            // 尊重传入凭据自带的 disabled，而不是无条件置 false。
            //
            // 🔴 修复的实际事故：此处原为硬编码 `disabled: false`，于是「重新导入一个
            // 已知被上游封禁的号」会让它以**启用态**回池。而 persist_credentials 是从
            // 内存 entries 全量重写 credentials.json 的，所以一次导入还会把同批次其它
            // 号刚落盘的禁用状态一起刷掉——现场表现就是「第二次导入后全部凭据都启用了」。
            //
            // 危害不只是状态显示错：被封号回池后网关会拿它继续打上游，每次都换来一个
            // 403 TEMPORARILY_SUSPENDED，反而加深上游对该批号的风控判定。
            //
            // 语义：credentials.json 与 import 接口的 items[].disabled 都是既有字段，
            // 调用方契约不变；这里只是不再丢弃它。未提供时 serde default = false，
            // 与旧行为完全一致（新号仍默认启用），故无回归。
            let initial_disabled = validated_cred.disabled;
            entries.push(CredentialEntry {
                id: new_id,
                credentials: validated_cred,
                failure_count: 0,
                refresh_failure_count: 0,
                disabled_at: None,
                    consecutive_suspicious: 0,
                    consecutive_passthrough_failures: 0,
                    passthrough_overload_since: None,
                    last_passthrough_429_at: None,
                    last_selected_at: std::cell::Cell::new(Instant::now()),
                disabled: initial_disabled,
                // 复用既有的 Manual 语义：调用方显式要求禁用，非自动判定的死号，
                // 便于面板区分「人工/导入时置禁用」与「上游封禁/配额耗尽」。
                disabled_reason: initial_disabled.then_some(DisabledReason::Manual),
                success_count: 0,
                request_count: 0,
                total_credits_used: 0.0,
                last_used_at: None,
                inflight: Arc::new(AtomicU32::new(0)),
                last_usage_403_feature_not_supported: AtomicBool::new(false),
                last_full_reprobe_at: Mutex::new(None),
                reprobe_in_flight: AtomicBool::new(false),
                refresh_lock: Arc::new(TokioMutex::new(())),
            });
        }

        // 6. 持久化
        // Change 5: 持久化失败时发出结构化告警而不是硬错误返回，
        // 以免调用方因磁盘/权限问题无法上号。若 machineId 发生了轮换，
        // 仅内存中有新指纹；重启后 reconcile 会重新检测碰撞并再次轮换，
        // 但重启前的指纹将恢复旧值（漂移风险），故区分两种情况分别告警。
        if let Err(e) = self.persist_credentials() {
            if mid_was_rotated {
                tracing::warn!(
                    credential_id = new_id,
                    "machineId 轮转后持久化失败，重启后指纹将漂移。建议手动检查凭据文件权限。error = {}",
                    e
                );
            } else {
                tracing::warn!(
                    credential_id = new_id,
                    "add_credential 持久化失败，重启前新增的凭据将丢失。建议检查凭据文件权限。error = {}",
                    e
                );
            }
        }

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
        self.delete_credential_forced(id, false)
    }

    /// 删除凭据，`force=true` 时**跳过「必须先禁用」这道门**。
    ///
    /// # 为什么需要 force
    ///
    /// 用户原话要的是「多选菜单里加一个强制删除」。现状是删一个号要两次调用
    /// （先 `PATCH` 禁用、再 `DELETE`），批量删 N 个 = **2N 次往返**；而"号卡住了要拔掉"
    /// 正是强制删除的核心动机——要求先禁用等于让这个场景多绕一圈。
    ///
    /// # 只绕禁用门，**不**跳过回收站
    ///
    /// 删除仍是软删（进 `trash.json`，可 `restore_credential` 恢复）。理由：adminKey 明文存
    /// localStorage 且全仓无 CSP，一旦 XSS 就能整池清空，**回收站是被打穿后唯一的兜底**；
    /// 而 trash 受 `trashRetentionDays` 自动清理，留存成本近零。
    /// 真正的物理删除走既有的 `purge_credential`（回收站内二次确认），语义分层不变。
    ///
    /// # inflight > 0 的号也允许强删
    ///
    /// 已核实安全：`InflightGuard` 直接持 `Arc<AtomicU32>`，Drop 只对自己那个 Arc 做
    /// `saturating_sub`，与 entry 生命周期解耦；`report_failure` 在 entry 缺失时 early-return，
    /// 不 panic 不误伤其它号。在途请求会正常读完自己的流。
    pub fn delete_credential_forced(&self, id: u64, force: bool) -> anyhow::Result<()> {
        let was_current = {
            let mut entries = self.entries.lock();

            // 查找凭据位置
            let idx = entries
                .iter()
                .position(|e| e.id == id)
                .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?;

            // 检查是否已禁用（force 时跳过这道门，见 delete_credential_forced 的文档）
            if !entries[idx].disabled && !force {
                anyhow::bail!("只能删除已禁用的凭据（请先禁用凭据 #{}）", id);
            }
            if force && !entries[idx].disabled {
                // 强删一个**仍在服务**的号值得留痕：便于事后对照「用户投诉的中断」与这条记录。
                // inflight 一并记下——它是"这次强删影响了多少在途请求"的唯一证据。
                tracing::warn!(
                    credential_id = id,
                    inflight = entries[idx].inflight.load(Ordering::Acquire),
                    "强制删除**未禁用**的凭据（绕过先禁用门）；在途请求会正常读完自己的流"
                );
            }

            // 记录是否是当前凭据
            let current_id = *self.current_id.lock();
            let was_current = current_id == id;

            // 物理移出 entries，包成 TrashEntry 推入回收站
            let removed = entries.remove(idx);
            let mut cred = removed.credentials;
            cred.id = Some(removed.id); // 确保 id 落在凭据内，便于恢复
            // ⭐ 同步禁用状态三元组（与 persist_credentials 同一口径，见其内的同步块）。
            //
            // 🔴 修复的缺陷：`disabled` / `disabled_reason` / `disabled_at` 的**权威副本在
            // `CredentialEntry` 上**，`KiroCredentials` 里那份只在 `persist_credentials()` 落盘时
            // 被同步。而本路径直接把 `removed.credentials` 塞进 `TrashEntry`，**绕过了那次同步**，
            // 于是回收站里的凭据恒为 `disabled=false / reason=None / at=None`。
            //
            // 实测证据：线上 07-30 之后删除的 31 个号（此时 reason 持久化已上线），
            // trash.json 里三字段全是 `(False, None, None)`；175 条回收站记录**无一条**带禁用原因。
            //
            // 后果有三层：
            // ① 用户明确要求「认定封号必须标明原因」，而号被判死→删除后原因即丢失——恰恰是最需要它
            //    的时刻（判断该换号还是该申诉）。
            // ② `restore_credential` 恢复时因读不到真实原因，只能一律落 `Manual`，
            //    即批 2 修掉的「自动禁用原因变手动」在回收站路径上的翻版（同型漏修）。
            // ③ 以 reason 为判据的自愈/诊断逻辑对恢复出来的号全部失效。
            cred.disabled = removed.disabled;
            cred.disabled_reason = removed.disabled_reason;
            cred.disabled_at = removed.disabled_at.clone();
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

        // 清除被删凭据的一切 per-id 调度内存态（cooldown / rpm / model_blocklist / rate_limiter）。
        // 这些结构以 credential_id 为键但不随删号自动收缩，若不清：
        //   ①从回收站 restore(按原 id 恢复)的号会背着删除前的长冷却/退避/日计数/黑名单被静默跳过；
        //   ②即便有单调 id 计数器兜底新号不复用 id，删→恢复同一 id 的路径仍需要它。
        // 与上面 affinity 清理同属「删号清干净它的调度态」契约。current_id 切换靠下方
        // select_highest_priority；health 是族级(family_key)非 per-id，与单号删除无关，不动。
        self.cooldown.clear_cooldown(id);
        self.rpm.remove(id);
        // model_blocklist 键是复合 (credential_id, model)，按 id 剔除该号的所有模型级黑名单条目。
        self.model_blocklist.lock().retain(|(cred_id, _), _| *cred_id != id);
        // rate_limiter per-id 状态(backoff_until 退避≤1h / daily_count / consecutive_failures):
        // 不清则 restore 同 id 的 Kiro 号会继承残留退避被静默跳过直到自愈,与 cooldown 同源同类。
        self.rate_limiter.reset(id);

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
                    disabled_at: None,
                    consecutive_suspicious: 0,
                    consecutive_passthrough_failures: 0,
                    passthrough_overload_since: None,
                    last_passthrough_429_at: None,
                    last_selected_at: std::cell::Cell::new(Instant::now()),
                    disabled: true,
                    disabled_reason: Some(DisabledReason::Manual),
                    success_count: 0,
                    request_count: 0,
                    total_credits_used: 0.0,
                    last_used_at: None,
                    inflight: Arc::new(AtomicU32::new(0)),
                    last_usage_403_feature_not_supported: AtomicBool::new(false),
                    last_full_reprobe_at: Mutex::new(None),
                    reprobe_in_flight: AtomicBool::new(false),
                    refresh_lock: Arc::new(TokioMutex::new(())),
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
                    // 删除前的禁用原因/时刻。回收站此前不带这两项（见 delete_credential 的同步块），
                    // 面板即便想显示也拿不到；老数据为 None，前端按缺省处理。
                    disabled_reason: c.disabled_reason,
                    disabled_at: c.disabled_at.clone(),
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
            // 保留删除前的真实禁用原因/时刻（回收站现在会带上它们，见 delete_credential 的同步块）。
            // 恢复态仍是 disabled=true（不自动回池，交由 Admin 手动启用），但**原因不再被抹成 Manual**：
            // 运维需要知道这号当初是「额度耗尽」还是「被封」才能决定启不启用。
            // 老回收站数据无该字段 → None，此时才回落 Manual（与加载路径同一兼容策略）。
            let restored_reason = cred.disabled_reason.or(Some(DisabledReason::Manual));
            // 时刻同样保留；**但缺失时必须补当前时间**而不是留 None。
            // 缺失的真实路径（smoke 实测到的）：号在**启用**态被强制删除 → 它从来没有 disabled_at，
            // 恢复时却被置成 disabled=true。若不补时间戳，面板会显示一个"已禁用但不知何时禁用"的号，
            // 而 disabled_at 的整个用途就是让运维判断"这号坏了多久"。此处恢复即是它被禁用的时刻。
            let restored_at = cred
                .disabled_at
                .clone()
                .or_else(|| Some(Utc::now().to_rfc3339()));
            cred.disabled_reason = restored_reason;
            cred.disabled_at = restored_at.clone();
            entries.push(CredentialEntry {
                id,
                credentials: cred,
                failure_count: 0,
                refresh_failure_count: 0,
                disabled_at: restored_at,
                    consecutive_suspicious: 0,
                    consecutive_passthrough_failures: 0,
                    passthrough_overload_since: None,
                    last_passthrough_429_at: None,
                    last_selected_at: std::cell::Cell::new(Instant::now()),
                disabled: true,
                disabled_reason: restored_reason,
                success_count: restored_entry.success_count,
                request_count: 0,
                total_credits_used: restored_entry.total_credits_used,
                last_used_at: restored_entry.last_used_at,
                inflight: Arc::new(AtomicU32::new(0)),
                last_usage_403_feature_not_supported: AtomicBool::new(false),
                last_full_reprobe_at: Mutex::new(None),
                reprobe_in_flight: AtomicBool::new(false),
                refresh_lock: Arc::new(TokioMutex::new(())),
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

    /// 清空整个回收站（管理员在面板点「清理」时的显式全清）。
    ///
    /// 与 [`Self::purge_expired_trash`] 的区别是**没有时间维度**：它不看 `deleted_at`，
    /// 一次清空全部条目。这道独立入口是必要的，因为 `retention_days` 的 `0` 已经被
    /// 后台任务占用为「永久保留、永不自动清」的语义，导致按天数的接口**无法表达
    /// 「现在就全清」**——传 0 会被解释成永久保留而直接返回 0，传 N 则清不掉 N 天内
    /// 新删除的条目。历史缺陷正是这个：面板点清理，67 条刚删的凭据一条都清不掉，
    /// 用户看到「清理完成，共移除 0 项」以为按钮坏了。
    ///
    /// 返回被清空的条目数量。不可逆，调用方（面板）须自行做二次确认。
    pub fn purge_all_trash(&self) -> usize {
        let removed = {
            let mut trash = self.trash.lock();
            let n = trash.len();
            trash.clear();
            n
        };
        if removed > 0 {
            if let Err(e) = self.persist_trash() {
                tracing::warn!("清空回收站后回写失败: {}", e);
            }
            tracing::info!("回收站已全部清空：彻底删除 {} 条凭据", removed);
        }
        removed
    }

    /// 清理回收站中超过保留期的条目（由后台定时任务周期调用）
    ///
    /// `retention_days == 0` 表示永久保留，直接返回 0 —— 这是**后台任务**的语义。
    /// 管理员要立即全清请走 [`Self::purge_all_trash`]，不要给本函数传 0。
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

    /// 新号入池后一次性自动初始化(异步 fire-and-forget):刷 token + 动态解析 profileArn。
    ///
    /// 根治「刚上号查余额报 403 Invalid token / 400 profileArn is required」的时序坑(#89):
    /// 新号 add 后不再被动等后台刷新循环(它过滤临期 token,新号不命中)才解析 arn,而是入池即触发一次
    /// [`force_refresh_token_for`](Self::force_refresh_token_for)(内含「刷 token + 缺则 ListAvailableProfiles
    /// 解析 arn + persist」)。
    ///
    /// **门控集中在此**(4 条上号路径调用点无需各自判类型):custom_api(透传,无 refresh_token/arn)、
    /// api_key(直接用 kiro_api_key,无需刷新)一律跳过——否则 custom_api 会误入 force_refresh 的
    /// refresh_token 分支 bail。不阻塞上号响应(spawn 后台跑),失败仅 warn。
    pub fn spawn_initial_refresh(self: &Arc<Self>, id: u64) {
        let eligible = {
            let entries = self.entries.lock();
            match entries.iter().find(|e| e.id == id) {
                Some(e) => {
                    !e.credentials.is_custom_api_credential()
                        && !e.credentials.is_api_key_credential()
                }
                None => false,
            }
        };
        if !eligible {
            return;
        }
        let tm = Arc::clone(self);
        tokio::spawn(async move {
            tracing::info!("凭据 #{} 新号自动初始化开始(刷新 Token + 解析 profileArn)", id);
            match tm.force_refresh_token_for(id).await {
                Ok(_) => tracing::info!("凭据 #{} 新号自动初始化完成", id),
                Err(e) => tracing::warn!(
                    "凭据 #{} 新号初始化失败(不影响入池,后台刷新循环会重试): {}",
                    id,
                    e
                ),
            }
        });
    }

    /// 强制刷新指定凭据的 Token（admin 手动强刷）。
    ///
    /// 无条件刷新；错误直接返回给调用方（admin 侧）展示，不在此累计失败/禁用。
    pub async fn force_refresh_token_for(&self, id: u64) -> anyhow::Result<()> {
        self.refresh_token_locked(id, None).await.map(|_| ())
    }

    /// 【F】切换指定 external_idp 号到目标 region 的 profile。
    ///
    /// 流程：取有效 token → [`probe_profile_usable`] 验活目标 arn → **仅 Usable 才**写回
    /// `profile_arn` + `sync_region_from_arn()` + 持久化，并返回订阅标题；
    /// FeatureNotSupported/Unauthorized/其它一律 `bail!`，**校验不可用绝不写入**（防呆铁律）。
    pub async fn switch_profile_region_for(
        &self,
        id: u64,
        target_arn: &str,
    ) -> anyhow::Result<Option<String>> {
        let target_arn = target_arn.trim().to_string();
        if target_arn.is_empty() {
            bail!("目标 profileArn 为空");
        }
        // 取有效 token（过期会先刷新）。credentials 为最新快照。
        let (credentials, token) = self.ensure_valid_token(id).await?;
        // External IdP + IdC 支持切换 region profile(底层探测对 IdC 用纯 Bearer,已在刷新路径验证)。
        // 排除 social(通常只占位 ARN)/api_key/custom_api(无 profile 概念)。
        if !credentials.is_external_idp_credential() && !credentials.is_idc_credential() {
            bail!("仅 External IdP / IdC 凭据支持切换 region profile");
        }
        let cfg = self.config.load_full();
        let proxy = credentials.effective_proxy(self.proxy.as_ref());
        match probe_profile_usable(&credentials, &cfg, &token, proxy.as_ref(), &target_arn).await {
            ProfileProbeOutcome::Usable { subscription_title } => {
                {
                    let mut entries = self.entries.lock();
                    let entry = entries
                        .iter_mut()
                        .find(|e| e.id == id)
                        .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?;
                    entry.credentials.profile_arn = Some(target_arn.clone());
                    entry.credentials.sync_region_from_arn();
                    if let Some(t) = &subscription_title {
                        entry.credentials.subscription_title = Some(t.clone());
                    }
                    // 切到已验活可用的 region → 清除坏标记。
                    entry
                        .last_usage_403_feature_not_supported
                        .store(false, Ordering::Relaxed);
                }
                if let Err(e) = self.persist_credentials() {
                    tracing::warn!("切换 region 后持久化失败（不影响本次切换）: {}", e);
                }
                tracing::info!("凭据 #{} 已切换到 region profile {}", id, target_arn);
                Ok(subscription_title)
            }
            ProfileProbeOutcome::FeatureNotSupported => {
                bail!("目标 region profile 不可用（FEATURE_NOT_SUPPORTED，该 region 未开通），未切换")
            }
            ProfileProbeOutcome::Unauthorized => {
                bail!("目标 region profile 验活失败（401 认证无效），未切换")
            }
            ProfileProbeOutcome::OtherError(e) => {
                bail!("目标 region profile 验活失败（{}），未切换", e)
            }
        }
    }

    /// 【F】列出指定 external_idp 号在候选 region 的全部 profile 及其验活结果（供前端选 region）。
    pub async fn probe_regions_for(&self, id: u64) -> anyhow::Result<Vec<ProfileCandidate>> {
        let (credentials, token) = self.ensure_valid_token(id).await?;
        // External IdP + IdC 支持列出 region profile(排除 social/api_key/custom_api)。
        if !credentials.is_external_idp_credential() && !credentials.is_idc_credential() {
            bail!("仅 External IdP / IdC 凭据支持列出 region profile");
        }
        let cfg = self.config.load_full();
        let proxy = credentials.effective_proxy(self.proxy.as_ref());
        Ok(probe_all_usable_profiles(&credentials, &cfg, &token, proxy.as_ref()).await)
    }

    /// 验活重选并写回可用 region profile(刷新路径 + 对话路径异步任务共用的单一真相源)。
    ///
    /// 枚举全部候选 → 真验活(probe_all_usable_profiles,一整轮 getUsageLimits) → 选 usable 的 arn
    /// 写回 + `sync_region_from_arn` + 清 403 坏标记;全坏则记 6h 冷却时间戳。
    /// 返回 `true` = 找到并应用了可用 region(含"原 arn 复验仍可用");`false` = 全坏未纠正。
    /// **不持锁跑网络**:探测在锁外,只在写回时短临界区持 entries 锁。
    async fn reprobe_and_correct_region_with(
        &self,
        id: u64,
        creds: &KiroCredentials,
        token: &str,
    ) -> bool {
        let cfg = self.config.load_full();
        let proxy = creds.effective_proxy(self.proxy.as_ref());
        let candidates = probe_all_usable_profiles(creds, &cfg, token, proxy.as_ref()).await;
        if let Some(best) = candidates.iter().find(|c| c.usable) {
            let mut entries = self.entries.lock();
            if let Some(entry) = entries.iter_mut().find(|e| e.id == id) {
                let old = entry.credentials.profile_arn.clone();
                if old.as_deref() != Some(best.arn.as_str()) {
                    entry.credentials.profile_arn = Some(best.arn.clone());
                    entry.credentials.sync_region_from_arn();
                    if let Some(t) = &best.subscription_title {
                        entry.credentials.subscription_title = Some(t.clone());
                    }
                    tracing::info!(
                        "凭据 #{} 验活重选：{:?} → {}（region={}, {}）",
                        id, old, best.arn, best.region,
                        best.subscription_title.as_deref().unwrap_or("?")
                    );
                }
                // 无论 arn 是否变，清除坏标记 + 清空全坏冷却时间戳(恢复灵敏)。
                entry.last_usage_403_feature_not_supported.store(false, Ordering::Relaxed);
                *entry.last_full_reprobe_at.lock() = None;
            }
            crate::common::recovery_metrics::bump_region_reprobe_ok();
            true
        } else {
            // 全 region 都探测不到可用 profile：记时间戳进入 6h 冷却，避免反复白跑一整轮探测。
            {
                let entries = self.entries.lock();
                if let Some(entry) = entries.iter().find(|e| e.id == id) {
                    *entry.last_full_reprobe_at.lock() = Some(Instant::now());
                }
            }
            crate::common::recovery_metrics::bump_region_reprobe_fail();
            tracing::warn!(
                "凭据 #{} 验活重选未找到可用 region profile（保持原 arn，{}h 内不再重复全 region 探测）",
                id, REPROBE_ALL_BAD_COOLDOWN.as_secs() / 3600
            );
            false
        }
    }

    /// 标记某号上次对话/查询撞了 403 FEATURE_NOT_SUPPORTED(供后台刷新循环 needs_reprobe 门兜底纠正)。
    pub fn mark_usage_403_feature_not_supported(&self, id: u64) {
        let entries = self.entries.lock();
        if let Some(entry) = entries.iter().find(|e| e.id == id) {
            entry.last_usage_403_feature_not_supported.store(true, Ordering::Relaxed);
        }
    }

    /// 廉价本地纠正:只把 region/auth_region 同步成 profileArn 内的 region(纯字符串,无网络)。
    /// 返回 true = region 字段确实被改动(正交隐患"region 与 ARN 漂移"的即时修正)。
    /// 对真正的 FEATURE_NOT_SUPPORTED(ARN region 本身就是未开通那个)通常是 no-op → false。
    pub fn sync_region_from_arn_for(&self, id: u64) -> bool {
        let mut entries = self.entries.lock();
        if let Some(entry) = entries.iter_mut().find(|e| e.id == id) {
            entry.credentials.sync_region_from_arn()
        } else {
            false
        }
    }

    /// 触发某 external_idp 号的**后台异步** region 重探(对话路径撞 403 时调用,不阻塞当前请求)。
    ///
    /// per-id 守卫:`reprobe_in_flight` compare_exchange 抢占,抢不到直接返回(N 并发只 1 个真探测)。
    /// 6h 冷却双检:全坏号冷却期内不重探。抢到则 detached spawn,任务内取 token → 校验 external_idp
    /// → `reprobe_and_correct_region_with` → 成功则持久化;无论成败由 guard Drop 清回 in_flight。
    pub fn trigger_background_reprobe(self: &Arc<Self>, id: u64) {
        // 抢占 in_flight;抢不到 = 已有任务在跑,直接返回。
        {
            let entries = self.entries.lock();
            let Some(entry) = entries.iter().find(|e| e.id == id) else { return };
            if entry
                .reprobe_in_flight
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
                .is_err()
            {
                return; // 已有重探在飞
            }
            // 6h 冷却双检:全坏号冷却期内不重探(省成本)。抢到了锁但在冷却→立即清回并返回。
            let in_cooldown = entry
                .last_full_reprobe_at
                .lock()
                .map(|t| t.elapsed() < REPROBE_ALL_BAD_COOLDOWN)
                .unwrap_or(false);
            if in_cooldown {
                entry.reprobe_in_flight.store(false, Ordering::Release);
                return;
            }
        }
        // detached 任务:克隆 Arc 进 spawn,当前对话请求不等待。
        let this = Arc::clone(self);
        tokio::spawn(async move {
            // guard:任务无论走哪条路径退出(含 panic 后的栈展开),Drop 都清回 in_flight。
            struct InFlightGuard {
                tm: Arc<MultiTokenManager>,
                id: u64,
            }
            impl Drop for InFlightGuard {
                fn drop(&mut self) {
                    let entries = self.tm.entries.lock();
                    if let Some(e) = entries.iter().find(|e| e.id == self.id) {
                        e.reprobe_in_flight.store(false, Ordering::Release);
                    }
                }
            }
            let _guard = InFlightGuard { tm: Arc::clone(&this), id };

            // 取有效 token(过期先刷)。失败则放弃本次重探(guard 会清标记)。
            let (creds, token) = match this.ensure_valid_token(id).await {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!("凭据 #{} 后台重探取 token 失败,跳过: {}", id, e);
                    return;
                }
            };
            if !creds.is_external_idp_credential() {
                return; // 只有 external_idp 号有多 region profile 概念
            }
            if this.reprobe_and_correct_region_with(id, &creds, &token).await {
                if let Err(e) = this.persist_credentials() {
                    tracing::warn!("凭据 #{} 后台重探纠正 region 后持久化失败(不影响本次纠正): {}", id, e);
                }
            }
        });
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
        // 快速存在性检查（无锁），同时取出该凭据的 per-credential 刷新锁。
        // Arc clone 出来后无需持有 entries 锁，await 期间不阻塞其他凭据的 entries 读写。
        let cred_refresh_lock = {
            let entries = self.entries.lock();
            entries
                .iter()
                .find(|e| e.id == id)
                .map(|e| Arc::clone(&e.refresh_lock))
                .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?
        };

        // 获取该凭据专属的刷新锁——仅串行化同一凭据的并发刷新，不影响其他凭据。
        let _guard = cred_refresh_lock.lock().await;

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

        // 条件刷新：拿锁后二次确认 token 仍将在 lead 内过期才刷,否则跳过(避免惊群重刷)。
        // unwrap_or(**true**):expires_at 缺失/不可解析时视为"需刷新"不跳过,与热路径进入判定
        // is_token_expired(unwrap_or=true) 同口径——否则 A3/C2 修复会把 expiry 未知的凭据误跳过
        // 导致该刷不刷、返回陈旧 token。(后台预刷新在 :4900 已用 unwrap_or(false) 预筛,expiry
        // 未知的凭据根本不会进预刷新,故此处改 true 不会引发后台重刷。)
        if let Some(lead) = conditional_lead {
            if !is_token_expiring_within(&credentials, lead).unwrap_or(true) {
                return Ok(RefreshOutcome::Skipped);
            }
        }

        let effective_proxy = credentials.effective_proxy(self.proxy.as_ref());
        let cfg = self.config.load_full();

        // Change 2: 瞬态错误重试退避（3 次，1s/2s/4s）。
        // 仅对 5xx 状态码和网络/连接错误重试；400/401/403/429/invalid_grant/DiagnosedError
        // 属永久性或策略性失败，直接透传给上层（保留 RefreshTokenInvalidError 语义不变）。
        let new_creds = {
            const MAX_ATTEMPTS: u32 = 3;
            let mut last_err: anyhow::Error = anyhow::anyhow!("unreachable");
            let mut succeeded = false;
            let mut result_creds = None;
            for attempt in 0..MAX_ATTEMPTS {
                match refresh_token(&credentials, &cfg, effective_proxy.as_ref()).await {
                    Ok(c) => {
                        result_creds = Some(c);
                        succeeded = true;
                        break;
                    }
                    Err(e) => {
                        // 永久/策略失败：不重试，立即透传
                        if e.downcast_ref::<RefreshTokenInvalidError>().is_some()
                            || e.downcast_ref::<DiagnosedError>().is_some()
                        {
                            return Err(e);
                        }
                        // 检查是否为可重试的瞬态错误（5xx 响应体中会含状态码描述，
                        // 或为网络层 IO 错误）。reqwest 的连接/超时错误均无状态码，
                        // 凡不属上述永久错误的 anyhow::Error 视为可重试瞬态错误。
                        let is_5xx = e.to_string().contains("500")
                            || e.to_string().contains("502")
                            || e.to_string().contains("503")
                            || e.to_string().contains("504")
                            || e.to_string().contains("505")
                            || e.to_string().contains("服务器错误")
                            || e.to_string().contains("暂时不可用");
                        // Exclude all permanent HTTP error codes so they are not retried.
                        // Any error string not matching these exclusions is treated as a
                        // transient network/IO error and is eligible for retry.
                        let is_network = !e.to_string().contains("400")
                            && !e.to_string().contains("401")
                            && !e.to_string().contains("403")
                            && !e.to_string().contains("404")
                            && !e.to_string().contains("410")
                            && !e.to_string().contains("422")
                            && !e.to_string().contains("429")
                            && !e.to_string().contains("invalid_grant")
                            // ⭐ 结构性不可刷新：api_key 号没有 refreshToken，`refresh_token()`
                            // 对它是契约级 bail。该串**不含任何 HTTP 状态码**，故上面那串
                            // 排除条件一个都命中不了 → 本黑名单式判据会把它当"网络瞬态错误"
                            // 重试满 3 次（1s + 2s 白等）。重试一个结构上永不可能成功的操作
                            // 是纯损耗，且每轮都计一次失败 → 加速把号判成死号。
                            //
                            // 根因已在 provider.rs 的两处 force-refresh 调用点堵住
                            // （api_key 号不再进入刷新）；这里是**纵深防御**：任何将来新增的
                            // 调用方即使漏判，也只会失败一次而不是被退避重试三次。
                            && !e.to_string().contains("API Key 凭据不支持刷新");
                        if (is_5xx || is_network) && attempt + 1 < MAX_ATTEMPTS {
                            let backoff_secs = 1u64 << attempt; // 1, 2
                            tracing::warn!(
                                "凭据 #{} 刷新瞬态错误（第 {}/{}），{}s 后重试: {}",
                                id, attempt + 1, MAX_ATTEMPTS, backoff_secs, e
                            );
                            tokio::time::sleep(std::time::Duration::from_secs(backoff_secs)).await;
                            last_err = e;
                        } else {
                            last_err = e;
                            break;
                        }
                    }
                }
            }
            if succeeded {
                result_creds.unwrap()
            } else {
                return Err(last_err);
            }
        };

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

        // 队头阻塞根治（Medium 1）：token 已刷新并写回，refresh_lock 的职责（串行化 token 轮换，
        // 防并发用同一 refresh_token 重复换取）到此结束。下面的 profileArn 动态解析 / 验活重选
        // 是**独立的纯网络探测**（只改 profile_arn，与 refresh_token 轮换正交），若继续持锁，一个
        // 全坏 external_idp 号 reprobe 一整轮 getUsageLimits 会把所有号的刷新全堵在锁后（队头阻塞）。
        // 故在此显式释放 refresh_lock，让 arn/reprobe 在锁外并发进行。写回 profile_arn 时另用
        // entries 短临界区 + 值比对，无需 refresh_lock 保护。
        drop(_guard);

        // 动态解析 profileArn:idc/Enterprise 号常无 profileArn(oidc 刷新不回传),而对话/余额
        // 端点要求真实 profileArn(占位 ARN 对 Enterprise 号会被判 Invalid token/403)。刷新成功后
        // 若该号仍缺 profileArn 且非 external_idp(它不带),运行时调 management ListAvailableProfiles
        // 拿真实 arn 写回,一次解析后持久化缓存,后续对话/余额直接用真实值。失败仅告警不阻断。
        let (needs_arn, needs_reprobe, arn_creds, arn_token) = {
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
                    // 验活重选（D）：external_idp 号当前 arn 上次 getUsageLimits 返回过
                    // 403 FEATURE_NOT_SUPPORTED（该 region profile 未开通）→ 需要 reprobe 换可用 region。
                    // 只对**确认坏的号**触发（健康号 flag=false 不动，省成本）。missing 优先走解析路径。
                    //
                    // 成本护栏（Medium 2）：全 region 都坏的号，上次全坏探测若在 REPROBE_ALL_BAD_COOLDOWN
                    // 冷却期内则跳过——否则余额环每 ~30min 重置 403 flag，每 token TTL 都白跑一整轮探测。
                    let in_reprobe_cooldown = e
                        .last_full_reprobe_at
                        .lock()
                        .map(|t| t.elapsed() < REPROBE_ALL_BAD_COOLDOWN)
                        .unwrap_or(false);
                    let needs_reprobe = !missing
                        && c.is_external_idp_credential()
                        && e.last_usage_403_feature_not_supported.load(Ordering::Relaxed)
                        && !in_reprobe_cooldown;
                    (
                        eligible,
                        needs_reprobe,
                        c.clone(),
                        c.access_token.clone().unwrap_or_default(),
                    )
                }
                None => (false, false, KiroCredentials::default(), String::new()),
            }
        };
        // 验活重选（D）：确认坏的 external_idp 号——枚举全部候选、真验活、选 usable 的 arn 写回。
        // 用 probe_all_usable_profiles（而非 resolve_profile_arn_multi_region 的「取第一个」），
        // 否则可能再次选中同一个 FEATURE_NOT_SUPPORTED 的坏 arn。
        if needs_reprobe && !arn_token.is_empty() {
            // 抽成 helper 供刷新路径 + 对话路径异步任务共用(逻辑单一真相源)。
            self.reprobe_and_correct_region_with(id, &arn_creds, &arn_token).await;
        }
        if needs_arn && !arn_token.is_empty() {
            let cfg2 = self.config.load_full();
            let proxy2 = arn_creds.effective_proxy(self.proxy.as_ref());
            // 优先探测号自己的 region（拿到 region 与 ARN 自洽的 profile），无则候选兜底。
            let preferred_region = arn_creds.effective_upstream_region(&cfg2).to_string();
            match resolve_profile_arn_multi_region(
                &arn_creds,
                &cfg2,
                &arn_token,
                proxy2.as_ref(),
                &preferred_region,
            )
            .await
            {
                Ok(Some(arn)) => {
                    let mut entries = self.entries.lock();
                    if let Some(entry) = entries.iter_mut().find(|e| e.id == id) {
                        entry.credentials.profile_arn = Some(arn.clone());
                        // 防呆铁律:profile_arn 一变,region/auth_region 立即同步成 ARN 内 region,
                        // 杜绝「解析到 X region 的 ARN 却留着 Y region」错配 → 400 Improperly formed。
                        if entry.credentials.sync_region_from_arn() {
                            tracing::info!(
                                "凭据 #{} region 已随 profileArn 同步为 {}",
                                id,
                                entry.credentials.region.as_deref().unwrap_or("?")
                            );
                        }
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
mod endpoint_bypass_guard_tests {
    //! 源码级守卫：**不经 provider** 的上游调用路径（深度验活 / 模型探测）必须走端点抽象，
    //! 不得手搓 IDE 的 host 与 profileArn 注入。
    //!
    //! 为何用源码断言：这两个函数都需要真实上游 + 号池才能跑通，纯单测覆盖不到；而它们正是
    //! CLI(ksk_)号的**自伤点** —— 硬编码 `runtime.{region}.kiro.dev` 会让 ksk_ 号稳定 403，
    //! 两者又都把 403/401 当"认证/账号级问题"上报：验活侧经 classify_balance_error
    //! **自动禁用凭据**，探测侧整轮中止并向面板报账号有问题。历史上这里已漏迁过一次 host
    //! （q.* → runtime.*），故把"必须走抽象"这条钉死。

    /// 取源码中某函数体的近似切片：从函数签名到下一个同缩进 `    }` 之前。
    fn fn_body(src: &str, signature: &str) -> String {
        let after = src
            .split(signature)
            .nth(1)
            .unwrap_or_else(|| panic!("函数 {signature} 不应被改名/删除"));
        // 4 空格缩进的收尾花括号即函数结束（本文件所有方法都在 impl 内，缩进固定）。
        after
            .split("\n    }")
            .next()
            .unwrap_or(after)
            .to_string()
    }

    #[test]
    fn should_use_endpoint_abstraction_in_deep_verify() {
        let body = fn_body(
            include_str!("token_manager.rs"),
            "pub async fn deep_verify_credential(&self, id: u64)",
        );
        assert!(
            body.contains("endpoint::for_credentials"),
            "深度验活必须按凭据解析端点，否则 CLI 号 403 → 被自动禁用"
        );
        assert!(
            body.contains("endpoint.api_url(&rctx)"),
            "URL 必须来自端点实现"
        );
        assert!(
            !body.contains("runtime.{}.kiro.dev"),
            "不得硬编码 IDE host（CLI 号必须打 q.{{region}}.amazonaws.com）"
        );
        assert!(
            !body.contains("effective_profile_arn"),
            "profileArn 注入必须交给端点的 transform_api_body（CLI 带 ARN 会 403）"
        );
    }

    #[test]
    fn should_use_endpoint_abstraction_in_model_probe() {
        let body = fn_body(
            include_str!("token_manager.rs"),
            "async fn probe_single_model(",
        );
        assert!(
            body.contains("endpoint::for_credentials"),
            "模型探测必须按凭据解析端点，否则 CLI 号每个模型都 401/403 → 整轮中止"
        );
        assert!(body.contains("endpoint.api_url(&rctx)"), "URL 必须来自端点实现");
        assert!(
            !body.contains("runtime.{}.kiro.dev"),
            "不得硬编码 IDE host"
        );
        assert!(
            !body.contains("effective_profile_arn"),
            "profileArn 注入必须交给端点的 transform_api_body"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ===== External IdP 验活层：纯逻辑单测 =====

    #[test]
    fn test_classify_probe_200_usable() {
        assert_eq!(
            classify_profile_probe(200, r#"{"subscriptionInfo":{}}"#),
            ProfileProbeOutcome::Usable { subscription_title: None }
        );
        assert_eq!(
            classify_profile_probe(204, ""),
            ProfileProbeOutcome::Usable { subscription_title: None }
        );
    }

    #[test]
    fn test_classify_probe_403_feature_not_supported() {
        // 实测 us-east-1 未开通号的真实症状
        let body = r#"{"__type":"AccessDeniedException","message":"FEATURE_NOT_SUPPORTED"}"#;
        assert_eq!(
            classify_profile_probe(403, body),
            ProfileProbeOutcome::FeatureNotSupported
        );
    }

    #[test]
    fn test_classify_probe_403_other_is_not_feature() {
        // 403 但不含 FEATURE_NOT_SUPPORTED → OtherError（不判死 region）
        match classify_profile_probe(403, "some other 403 reason") {
            ProfileProbeOutcome::OtherError(_) => {}
            other => panic!("期望 OtherError，得到 {:?}", other),
        }
    }

    #[test]
    fn test_classify_probe_401_unauthorized() {
        assert_eq!(
            classify_profile_probe(401, "invalid token"),
            ProfileProbeOutcome::Unauthorized
        );
    }

    #[test]
    fn test_classify_probe_429_is_other_error_not_dead() {
        // 铁律：429 归 OtherError（暂时不可用），绝不因限流判死一个 region
        match classify_profile_probe(429, "Too Many Requests") {
            ProfileProbeOutcome::OtherError(_) => {}
            other => panic!("429 必须是 OtherError，得到 {:?}", other),
        }
    }

    #[test]
    fn test_classify_probe_5xx_is_other_error() {
        match classify_profile_probe(502, "bad gateway") {
            ProfileProbeOutcome::OtherError(_) => {}
            other => panic!("期望 OtherError，得到 {:?}", other),
        }
    }

    fn mk_candidate(usable: bool, title: Option<&str>) -> ProfileCandidate {
        ProfileCandidate {
            arn: "arn:aws:codewhisperer:eu-central-1:1:profile/x".to_string(),
            region: "eu-central-1".to_string(),
            account: "1".to_string(),
            usable,
            subscription_title: title.map(|s| s.to_string()),
            reason: if usable { "usable" } else { "feature_not_supported" },
            current: false,
        }
    }

    #[test]
    fn test_candidate_rank_usable_before_unusable() {
        let usable = mk_candidate(true, None);
        let unusable = mk_candidate(false, None);
        assert!(candidate_rank(&usable) < candidate_rank(&unusable));
    }

    #[test]
    fn test_candidate_rank_paid_before_free() {
        let paid = mk_candidate(true, Some("KIRO POWER"));
        let free = mk_candidate(true, Some("KIRO FREE"));
        let none = mk_candidate(true, None);
        assert!(candidate_rank(&paid) < candidate_rank(&free));
        assert!(candidate_rank(&paid) < candidate_rank(&none));
    }

    #[test]
    fn test_candidate_sort_orders_usable_paid_first() {
        let mut v = vec![
            mk_candidate(false, None),
            mk_candidate(true, Some("KIRO FREE")),
            mk_candidate(true, Some("KIRO POWER")),
        ];
        v.sort_by_key(candidate_rank);
        assert_eq!(v[0].subscription_title.as_deref(), Some("KIRO POWER"));
        assert_eq!(v[1].subscription_title.as_deref(), Some("KIRO FREE"));
        assert!(!v[2].usable);
    }

    #[test]
    fn test_account_from_arn() {
        assert_eq!(
            account_from_arn("arn:aws:codewhisperer:eu-central-1:155119901513:profile/abc"),
            "155119901513"
        );
        assert_eq!(account_from_arn("garbage"), "");
    }

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

    /// ⭐ 回归：api_key 号的"不支持刷新"必须**快速失败**，不得被当瞬态错误退避重试。
    ///
    /// 缺陷（本轮多开时线上暴露）：刷新层的瞬态判据是**黑名单式**——
    /// 错误串不含 400/401/403/404/410/422/429/invalid_grant 就当"网络瞬态错误"。
    /// 而 "API Key 凭据不支持刷新 Token" 一个都不含 → 被判瞬态 →
    /// 退避重试满 3 次（白等 1s + 2s），且每轮计一次失败。
    ///
    /// 线上后果：api_key 号遇 403 后每轮白等约 3 秒、连计 3 次失败即判死号自动禁用，
    /// 相当于**把它的死亡速度放大三倍**（实测 #421-424 就是这样被反复禁用的）。
    ///
    /// 用耗时做判据而非日志：重试是可观测的时间代价（1s + 2s ≥ 3s），
    /// 而快速失败 < 1s。回退分类器里那条排除条件即 FAIL。
    #[tokio::test]
    async fn api_key_refresh_rejection_must_fail_fast_not_retry() {
        let mut cred = KiroCredentials::default();
        cred.id = Some(1);
        cred.kiro_api_key = Some("ksk_fail_fast_probe".to_string());
        cred.auth_method = Some("api_key".to_string());
        let mgr = MultiTokenManager::new(Config::default(), vec![cred], None, None, false).unwrap();

        let started = std::time::Instant::now();
        let err = mgr
            .refresh_token_locked(1, None)
            .await
            .expect_err("api_key 号刷新必须失败");
        let elapsed = started.elapsed();

        assert!(
            err.to_string().contains("API Key 凭据不支持刷新"),
            "错误应是契约级拒绝，实际: {err}"
        );
        assert!(
            elapsed < std::time::Duration::from_millis(900),
            "必须快速失败：结构上不可能成功的刷新被退避重试了（耗时 {elapsed:?}，\
             重试 3 次会花 1s+2s）。这会让 api_key 号每轮白等 3 秒并加速判死"
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
    async fn test_refresh_conditional_skips_when_peer_already_refreshed() {
        // A3/C2 回归:模拟惊群第二个 waiter——出队拿锁时 token 已被前一个 waiter 刷新成新鲜。
        // ensure_valid_token 现在传 Some(10),故拿锁后的条件重检应 Skipped、**不再重打上游刷新**。
        // (旧代码 ensure_valid 传 None → 无条件刷新 → 该测试若把 lead 换 None 会走网络失败。)
        let mut cred = KiroCredentials::default();
        cred.refresh_token = Some("r".repeat(120));
        cred.access_token = Some("already_fresh".to_string());
        // 关键:token 已新鲜(1 小时后过期),等价于"前一个 waiter 刚刷好"。
        cred.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());

        let manager = MultiTokenManager::new(Config::default(), vec![cred], None, None, true)
            .expect("构造 manager");

        // 与 ensure_valid_token 同阈值 Some(10):新鲜 → Skipped,无网络。
        let outcome = manager
            .refresh_token_locked(1, Some(10))
            .await
            .expect("新鲜 token 条件刷新应跳过,不报错");
        assert_eq!(
            outcome,
            RefreshOutcome::Skipped,
            "惊群出队者遇已刷新的新鲜 token 应跳过(消除重复刷新)"
        );
    }

    #[tokio::test]
    async fn test_refresh_conditional_does_not_skip_on_unknown_expiry() {
        // A3/C2 边界:expires_at 不可解析时,条件重检必须**不跳过**(unwrap_or(true)),
        // 与热路径 is_token_expired(unwrap_or=true) 同口径——否则 expiry 未知的凭据会被
        // 误跳过、该刷不刷、返回陈旧 token。旧实现 unwrap_or(false) 会错误 Skipped,此测试会失败。
        // 用短 refresh_token(<100)让底层 validate 在任何网络前 bail,从而无网络地证明"进入了刷新"。
        let mut cred = KiroCredentials::default();
        cred.refresh_token = Some("short".to_string()); // <100 → validate 阶段即 bail
        cred.access_token = Some("stale".to_string());
        cred.expires_at = Some("not-a-date".to_string()); // 不可解析 → expiring_within=None

        let manager = MultiTokenManager::new(Config::default(), vec![cred], None, None, true)
            .expect("构造 manager");

        let err = manager
            .refresh_token_locked(1, Some(10))
            .await
            .expect_err("expiry 未知应进入刷新(而非 Skipped),因 refresh_token 截断而失败");
        assert!(
            err.to_string().contains("refreshToken 已被截断"),
            "应进入真实刷新路径的 validate 报错(证明未被条件重检误跳过),实际: {}",
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

    /// 多开：同一账号可导入多份，且**每份 machineId 必须不同**。
    ///
    /// 这是「一个号导入多次、每次机器码不同、再各配代理试探并发」这个需求的承重测试。
    /// 三条断言分别锁住它成立的三个前提：
    ///
    /// 1. 默认路径**仍然去重**（多开不能把误双击上号的护栏一起拆掉）；
    /// 2. 显式路径能绕过去重；
    /// 3. 两份的 machineId **不相同** —— 若相同，上游会按设备指纹把它们关联封禁，
    ///    多开反而变成"把两份一起烧掉"。
    ///
    /// 第 3 条的机制：`generate_from_credentials` 对 api_key 号是
    /// `sha256("KiroAPIKey/" + key)`（**确定性**），故两份派生出同一个指纹，
    /// 随后入池处的撞车检测把第 2 份轮换成独立随机值。测试直接验最终状态而不验机制，
    /// 这样将来换实现（比如改成入池即随机）也不会误报失败。
    #[tokio::test]
    async fn test_multi_open_allows_duplicate_with_distinct_machine_ids() {
        const KEY: &str = "ksk_multi_open_probe_key";
        let manager =
            MultiTokenManager::new(Config::default(), vec![], None, None, false).unwrap();

        let mk = || {
            let mut c = KiroCredentials::default();
            c.auth_method = Some("api_key".to_string());
            c.kiro_api_key = Some(KEY.to_string());
            c
        };

        let id1 = manager.add_credential(mk()).await.expect("第 1 份应成功");

        // ① 默认路径仍去重：多开不得削弱误操作护栏。
        let dup = manager.add_credential(mk()).await;
        assert!(
            dup.is_err() && dup.as_ref().err().unwrap().to_string().contains("凭据已存在"),
            "默认路径必须仍然拒绝重复 kiroApiKey（否则误双击上号会静默多出一条号）"
        );

        // ② 显式多开路径放行。
        let id2 = manager
            .add_credential_allowing_duplicate(mk())
            .await
            .expect("显式多开必须允许同 key 再入池");
        assert_ne!(id1, id2, "两份必须是独立凭据（各自独立的 id）");

        // ③ machineId 必须不同 —— 这是多开有意义的前提。
        let m1 = manager
            .export_credential(id1)
            .and_then(|c| c.machine_id)
            .expect("第 1 份应已冻结 machineId");
        let m2 = manager
            .export_credential(id2)
            .and_then(|c| c.machine_id)
            .expect("第 2 份应已冻结 machineId");
        assert_ne!(
            m1, m2,
            "两份 machineId 相同 → 上游按设备指纹关联封禁，多开等于把两份一起烧掉"
        );
        assert_eq!(m2.len(), 64, "轮换后的指纹应是 64 位 hex");
    }

    /// 多开的第二个前提：每份**各自独立成族**，不会被族级连坐一锅端。
    ///
    /// `family_key` 是限流/健康的分组单位。若多开的 N 份共享族键，则一份被 403 风控
    /// 会让整族退避 —— 多开就完全失去意义（等于 N 份同生共死）。
    /// api_key/idc/social 号返回 `cred:{id}` 各自独立；只有 M365 external_idp 才共享
    /// `m365:{tenant}`。本测试锁住 api_key 这条路径。
    #[tokio::test]
    async fn test_multi_open_copies_are_in_separate_families() {
        const KEY: &str = "ksk_family_isolation_probe";
        let manager =
            MultiTokenManager::new(Config::default(), vec![], None, None, false).unwrap();

        let mk = || {
            let mut c = KiroCredentials::default();
            c.auth_method = Some("api_key".to_string());
            c.kiro_api_key = Some(KEY.to_string());
            c
        };

        let id1 = manager.add_credential(mk()).await.expect("第 1 份");
        let id2 = manager
            .add_credential_allowing_duplicate(mk())
            .await
            .expect("第 2 份");

        let f1 = manager.family_key_of(id1);
        let f2 = manager.family_key_of(id2);
        assert_ne!(
            f1, f2,
            "多开的两份共享了族键 → 一份被 403 风控会让整族退避，多开失去意义"
        );
        assert_eq!(f1, format!("cred:{id1}"), "api_key 号应各自独立成族");
        assert_eq!(f2, format!("cred:{id2}"));
    }

    /// 回归：导入一个标了 disabled 的凭据，必须以**禁用态**入池。
    ///
    /// 事故现场：此前 add_credential 硬编码 `disabled: false`，于是重新导入已知被上游
    /// 封禁的号会让它以启用态回池；而 persist_credentials 从内存全量重写
    /// credentials.json，一次导入还会把同批次其它号刚落盘的禁用状态一起刷掉，
    /// 表现为「第二次导入后全部凭据都启用了」。
    #[tokio::test]
    async fn test_add_credential_preserves_disabled_flag() {
        let manager =
            MultiTokenManager::new(Config::default(), vec![], None, None, false).unwrap();

        let mut cred = KiroCredentials::default();
        cred.kiro_api_key = Some("ksk_banned_account_key".to_string());
        cred.auth_method = Some("api_key".to_string());
        cred.disabled = true;

        let id = manager.add_credential(cred).await.unwrap();

        let entries = manager.entries.lock();
        let entry = entries.iter().find(|e| e.id == id).expect("凭据应已入池");
        assert!(entry.disabled, "标了 disabled 的凭据必须以禁用态入池");
        assert_eq!(
            entry.disabled_reason,
            Some(DisabledReason::Manual),
            "调用方显式要求的禁用应记为 Manual，与上游封禁等自动判定区分"
        );
    }

    /// 对照组：未指定 disabled 时仍默认启用（serde default = false），确保无回归。
    #[tokio::test]
    async fn test_add_credential_defaults_to_enabled() {
        let manager =
            MultiTokenManager::new(Config::default(), vec![], None, None, false).unwrap();

        let mut cred = KiroCredentials::default();
        cred.kiro_api_key = Some("ksk_normal_account_key".to_string());
        cred.auth_method = Some("api_key".to_string());
        // 不设 disabled，走 Default

        let id = manager.add_credential(cred).await.unwrap();

        let entries = manager.entries.lock();
        let entry = entries.iter().find(|e| e.id == id).expect("凭据应已入池");
        assert!(!entry.disabled, "未指定 disabled 的新号应默认启用");
        assert_eq!(entry.disabled_reason, None);
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

    #[tokio::test]
    async fn test_credential_id_never_reused_after_purge() {
        // 回归:删号→从回收站彻底清除(purge)→再加号,新号绝不复用被清除的 id。
        // 旧算法 max(entries∪trash)+1 会在 purge 后回落复用 id,使新号继承死号残留的
        // cooldown/model_blocklist 内存态。单调 id 计数器根治之。custom_api 号免网络校验。
        let config = Config::default();
        let mk = |url: &str| {
            let mut c = KiroCredentials::default();
            c.auth_method = Some("custom_api".to_string());
            c.base_url = Some(url.to_string());
            c
        };
        let mgr = MultiTokenManager::new(config, vec![], None, None, false).unwrap();
        let id1 = mgr.add_credential(mk("https://a.example.invalid")).await.unwrap();
        let id2 = mgr.add_credential(mk("https://b.example.invalid")).await.unwrap();
        assert!(id2 > id1, "id 应单调递增: #{id1} → #{id2}");

        // 删除最高 id 的号并从回收站彻底清除。
        mgr.set_disabled(id2, true).unwrap();
        mgr.delete_credential(id2).unwrap();
        mgr.purge_credential(id2).unwrap();

        // 此刻 entries∪trash 的 max 已回落到 id1;旧算法会把 id2 分配给新号(复用),
        // 计数器则继续给 id2 之后的值。
        let id3 = mgr.add_credential(mk("https://c.example.invalid")).await.unwrap();
        assert!(
            id3 > id2,
            "purge 后新号 id 必须 > 已清除的 id,不得复用(新号 #{id3},已清除 #{id2})"
        );
    }

    #[tokio::test]
    async fn test_delete_clears_per_id_cooldown_and_restore_is_clean() {
        // 回归:删号应清掉其 per-id 调度内存态(cooldown 等);从回收站按原 id 恢复的号
        // 不得继承删除前的长冷却而被静默跳过。
        use crate::kiro::cooldown::CooldownReason;
        let config = Config::default();
        let mut c = KiroCredentials::default();
        c.auth_method = Some("custom_api".to_string());
        c.base_url = Some("https://relay.example.invalid".to_string());
        let mgr = MultiTokenManager::new(config, vec![], None, None, false).unwrap();
        let id = mgr.add_credential(c).await.unwrap();

        // 打一个长冷却(账户暂停=24h,测试期内不会自然到期)。
        mgr.cooldown.set_cooldown(id, CooldownReason::AccountSuspended);
        assert!(
            mgr.cooldown_snapshot().iter().any(|i| i.credential_id == id),
            "冷却应已设置"
        );

        // 禁用 + 删除:delete_credential 应清掉该号的 per-id 冷却态。
        mgr.set_disabled(id, true).unwrap();
        mgr.delete_credential(id).unwrap();

        // 从回收站恢复(id 不变):不应再背着删除前的长冷却。
        mgr.restore_credential(id).unwrap();
        assert!(
            !mgr.cooldown_snapshot().iter().any(|i| i.credential_id == id),
            "restore 后不应继承删除前的冷却(#{id} 仍在冷却快照 = 泄漏)"
        );
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
    fn test_select_custom_api_priority_and_failover_exclude() {
        // custom_api 池内调度:①优先级小先选 ②exclude 排除已试号 failover 到下一个
        // ③全部 exclude → None(供上层落 Kiro 主力)。
        use std::collections::HashSet;
        let config = Config::default();
        let mk = |id: u64, prio: u32| {
            let mut c = KiroCredentials::default();
            c.id = Some(id);
            c.auth_method = Some("custom_api".to_string());
            c.base_url = Some(format!("https://relay{id}.example.invalid"));
            c.api_key = Some(format!("sk-{id}"));
            c.priority = prio;
            c
        };
        // #1 prio0, #2 prio0, #3 prio1
        let mgr = MultiTokenManager::new(config, vec![mk(1, 0), mk(2, 0), mk(3, 1)], None, None, false).unwrap();

        let empty = HashSet::new();
        // 初选:priority 最小(0)的 #1/#2 之一(同级按 RPM 均衡,初始 RPM 全 0 → 取 id 最小 #1)。
        let first = mgr.select_custom_api(&empty).expect("应选到 custom_api 号");
        assert!(first.0 == 1 || first.0 == 2, "应先选 priority=0 的号,得到 #{}", first.0);

        // failover:排除 #1、#2 后应落到 priority=1 的 #3(仍在 custom_api 池内,不跳类型)。
        let mut ex: HashSet<u64> = HashSet::new();
        ex.insert(1);
        ex.insert(2);
        let third = mgr.select_custom_api(&ex).expect("排除两个 prio0 后应选 #3");
        assert_eq!(third.0, 3, "failover 应落到 priority=1 的 #3");

        // 全部排除 → None(上层据此落 Kiro 主力路径)。
        ex.insert(3);
        assert!(mgr.select_custom_api(&ex).is_none(), "全部 custom_api 排除后应返回 None");
    }

    /// 造一个代挂号管理器（单号，供透传惩罚策略测试用）。
    fn mk_passthrough_mgr() -> MultiTokenManager {
        let mut c = KiroCredentials::default();
        c.id = Some(1);
        c.auth_method = Some("custom_api".to_string());
        c.base_url = Some("https://relay.example.invalid".to_string());
        c.api_key = Some("sk-relay".to_string());
        MultiTokenManager::new(Config::default(), vec![c], None, None, false).unwrap()
    }

    /// 回归（dwgx 明确要求）：代挂号**偶尔 429 绝不受任何惩罚**。
    ///
    /// 语义依据：代挂号是用户自购的付费第三方中转站，**没有"被风控"这个状态**，
    /// 429 只代表"它现在忙"。把它按下去既不能让它变快，又白白缩小可用池
    /// （两个号轮流 429 就会两个都被冷却 → 整池不可用 → 回落 Kiro，而 Kiro 侧此刻
    /// 可能正被风控烧号）。
    ///
    /// **旧代码为何 FAIL**：`provider.rs` 的透传 failover 对 429 给 30s 冷却
    /// （`429 => 30`），于是一次 429 就让该号在 `select_custom_api` 里被跳过 30 秒。
    #[test]
    fn test_passthrough_occasional_429_incurs_no_cooldown_and_no_penalty() {
        use std::collections::HashSet;
        let mgr = mk_passthrough_mgr();

        // 连打若干次 429（远超任何失败阈值），中间不成功。
        for _ in 0..10 {
            mgr.record_passthrough_result(1, crate::usage::RequestOutcome::RateLimited);
        }

        assert_eq!(
            mgr.available_count(),
            1,
            "偶尔/连续 429 不得禁用代挂号（未到持续过载窗口）"
        );
        assert!(
            mgr.select_custom_api(&HashSet::new()).is_some(),
            "429 不得进入惩罚系统：号必须仍可被选中（旧代码 429=>30s 惩罚性冷却，此处会是 None）"
        );
        // 边界说明：provider 的透传 failover 另有一个 **5s 调度级跳过**（不进 health、不计失败、
        // 不影响自动禁用判据），用于避免 100% 429 的站被每个新请求重新撞一次。
        // 那不是惩罚，且不经过本函数 —— 故本测试只锁「惩罚系统不得介入」这条。
    }

    /// 回归：代挂号连续「非瞬态」失败达阈值即自动禁用，且原因可辨。
    ///
    /// 非瞬态 = key 失效(401/403) / 额度耗尽(402) / 配置错(4xx)——**再试一万次也一样**，
    /// 等更多次纯属浪费用户请求。
    ///
    /// **旧代码为何 FAIL**：`record_passthrough_result` 对这些 outcome 走
    /// `_ => {}` 空分支，**永不自动禁用**（注释写的是"透传给客户端由其处理"）。
    /// 于是一个 key 已失效的中转站会永久留在池里，每条请求都先白撞它一次。
    #[test]
    fn test_passthrough_consecutive_nontransient_failures_auto_disable() {
        let mgr = mk_passthrough_mgr();

        for _ in 0..MAX_PASSTHROUGH_FAILURES {
            mgr.record_passthrough_result(1, crate::usage::RequestOutcome::AuthFailed);
        }

        assert_eq!(
            mgr.available_count(),
            0,
            "连续 {} 次非瞬态失败应自动禁用（旧代码永不禁用）",
            MAX_PASSTHROUGH_FAILURES
        );
        let snap = mgr.snapshot();
        let e = snap.entries.iter().find(|e| e.id == 1).unwrap();
        assert_eq!(
            e.disabled_reason.as_deref(),
            Some("PassthroughFailed"),
            "原因须可区分「中转站凭据问题」与 Kiro 号的 TooManyFailures（排查方向完全不同）"
        );
    }

    /// 回归：任一次成功即清零连续失败计数 —— 健康号永不被偶发 4xx 误禁。
    ///
    /// 这是「连续 + 成功归零」判据的核心保证。反例是旧的
    /// `cooldown.trigger_count >= 10` 累计式判据：它在数学上不可达
    /// （`report_success` 删掉整个冷却条目使计数归零），导致自动禁用从未生效。
    #[test]
    fn test_passthrough_success_resets_consecutive_failures() {
        let mgr = mk_passthrough_mgr();

        // 差一次到阈值
        for _ in 0..(MAX_PASSTHROUGH_FAILURES - 1) {
            mgr.record_passthrough_result(1, crate::usage::RequestOutcome::AuthFailed);
        }
        assert_eq!(mgr.available_count(), 1, "未达阈值不应禁用");

        // 成功一次 → 计数归零
        mgr.record_passthrough_result(1, crate::usage::RequestOutcome::Success);

        // 再来 阈值-1 次失败：若计数没归零，这里就会命中阈值被禁用
        for _ in 0..(MAX_PASSTHROUGH_FAILURES - 1) {
            mgr.record_passthrough_result(1, crate::usage::RequestOutcome::AuthFailed);
        }
        assert_eq!(
            mgr.available_count(),
            1,
            "成功后计数未归零 → 健康号被偶发 4xx 误禁（间歇性失败的站会被冤杀）"
        );
    }

    /// 回归（实测驱动）：`bad_request` 绝不计入号的健康 —— 那是坏请求，不是坏号。
    ///
    /// **依据是线上真实数据**，不是推理：代挂号 #216 成功率 **80.3%**（2910/3622）是个健康号，
    /// 却有 712 次 `bad_request`，其中 **119 次达到 ≥3 连**、最长 6 连。若把它计入连续失败
    /// 计数（阈值 3），这个健康号会被**误禁 119 次**。
    ///
    /// 更关键：代挂号历史 429 次数为 **0**，失败形态**全是** bad_request —— 所以"把 400 当号坏了"
    /// 恰好命中唯一真实存在的失败形态，是最容易踩的那个坑。
    ///
    /// 本测试锁住这条边界，防止将来有人"顺手"把 4xx 一起算进健康判据。
    #[test]
    fn test_passthrough_bad_request_never_counts_against_credential_health() {
        let mgr = mk_passthrough_mgr();

        // 远超阈值的连续 400：一次都不该惩罚（换号也一样错）。
        for _ in 0..(MAX_PASSTHROUGH_FAILURES * 4) {
            mgr.record_passthrough_result(1, crate::usage::RequestOutcome::BadRequest);
        }
        assert_eq!(
            mgr.available_count(),
            1,
            "客户端请求错误(400/404/422)不得禁用代挂号 —— 实测健康号有 119 次 ≥3 连 bad_request"
        );

        // 且不得污染连续计数：紧接着来 阈值-1 次真·非瞬态失败，仍不该被禁。
        for _ in 0..(MAX_PASSTHROUGH_FAILURES - 1) {
            mgr.record_passthrough_result(1, crate::usage::RequestOutcome::AuthFailed);
        }
        assert_eq!(
            mgr.available_count(),
            1,
            "bad_request 不得把连续计数垫高，否则等效于降低了真实阈值"
        );
    }

    /// 回归：瞬态失败（5xx / 网络错误）永不进连续计数、永不自动禁用。
    ///
    /// 中转站抖一下不代表它坏了，failover 换号即可。只有"再试无用"的信号才该累加。
    #[test]
    fn test_passthrough_transient_failures_never_auto_disable() {
        let mgr = mk_passthrough_mgr();

        for _ in 0..(MAX_PASSTHROUGH_FAILURES * 5) {
            mgr.record_passthrough_result(1, crate::usage::RequestOutcome::ServerError);
            mgr.record_passthrough_result(1, crate::usage::RequestOutcome::NetworkError);
        }

        assert_eq!(
            mgr.available_count(),
            1,
            "5xx/网络错误是瞬态信号，不得触发自动禁用（否则上游抖动会清空代挂池）"
        );
    }

    #[test]
    fn test_select_custom_api_skips_cooldown() {
        // failover 给失败号设了冷却后,select_custom_api 应跳过冷却中的号。
        use std::collections::HashSet;
        let config = Config::default();
        let mut c1 = KiroCredentials::default();
        c1.id = Some(1);
        c1.auth_method = Some("custom_api".to_string());
        c1.base_url = Some("https://relay1.example.invalid".to_string());
        c1.priority = 0;
        let mut c2 = KiroCredentials::default();
        c2.id = Some(2);
        c2.auth_method = Some("custom_api".to_string());
        c2.base_url = Some("https://relay2.example.invalid".to_string());
        c2.priority = 0;
        let mgr = MultiTokenManager::new(config, vec![c1, c2], None, None, false).unwrap();

        // 给 #1 设冷却(模拟它 403 额度满被 failover 冷却)→ 选号应跳过 #1 选 #2。
        mgr.cooldown_custom_api(1, 180);
        let empty = HashSet::new();
        let sel = mgr.select_custom_api(&empty).expect("应选到未冷却的 #2");
        assert_eq!(sel.0, 2, "#1 冷却中,应选 #2");
    }

    /// 造一个 custom_api 代挂号（priority 可指定，可选凭据级 custom_api_first 覆盖）。
    fn mk_custom(id: u64, priority: u32, first: Option<bool>) -> KiroCredentials {
        let mut c = KiroCredentials::default();
        c.id = Some(id);
        c.auth_method = Some("custom_api".to_string());
        c.base_url = Some(format!("https://relay{id}.example.invalid"));
        c.api_key = Some(format!("sk-relay-{id}"));
        c.priority = priority;
        c.custom_api_first = first;
        c
    }

    /// 造一个 Kiro（api_key 型）号，priority 可指定。
    fn mk_kiro(id: u64, priority: u32) -> KiroCredentials {
        let mut c = KiroCredentials::default();
        c.id = Some(id);
        c.auth_method = Some("api_key".to_string());
        c.kiro_api_key = Some(format!("sk-kiro-{id}"));
        c.priority = priority;
        c
    }

    #[test]
    fn test_cross_pool_priority_kiro_wins_when_lower() {
        // ⭐用户实测反馈的核心场景:"优先级设置了 kiro 的 apikey 更小,还是会优先调度上游的 apikey"。
        // 历史行为把「custom_api 优先」写死在分派顺序里(handlers 一进来就先试透传),而
        // select_custom_api 只在代挂号**子集内**比 priority → 跨池优先级从未被比较过。
        // 修复后:默认(custom_api_first=false)按 priority 全局公平比较,Kiro 更优则不先走透传。
        let config = Config::default();
        assert!(
            !config.custom_api_first,
            "全局默认必须是 false(priority 全局统一比较),否则又回到代挂号绝对优先"
        );
        // kiro priority=0 优于 relay priority=5
        let mgr = MultiTokenManager::new(
            config,
            vec![mk_custom(1, 5, None), mk_kiro(2, 0)],
            None,
            None,
            false,
        )
        .unwrap();
        assert!(
            !mgr.should_try_custom_api_first(),
            "Kiro 号 priority(0) 更小时应先走 Kiro,不该先试代挂透传"
        );
    }

    #[test]
    fn test_cross_pool_priority_custom_wins_when_lower_or_equal() {
        let config = Config::default();
        // 代挂 priority=0 优于 kiro priority=5 → 先走透传
        let mgr = MultiTokenManager::new(
            config.clone(),
            vec![mk_custom(1, 0, None), mk_kiro(2, 5)],
            None,
            None,
            false,
        )
        .unwrap();
        assert!(mgr.should_try_custom_api_first(), "代挂 priority 更小时应先走透传");

        // priority 相同 → 维持"代挂在前"的既有习惯(用 <= 而非 <),避免纯升级场景行为突变
        let mgr2 = MultiTokenManager::new(
            config,
            vec![mk_custom(1, 3, None), mk_kiro(2, 3)],
            None,
            None,
            false,
        )
        .unwrap();
        assert!(
            mgr2.should_try_custom_api_first(),
            "priority 相同时保持代挂在前(兼容既有部署)"
        );
    }

    #[test]
    fn test_cross_pool_per_credential_override_wins() {
        // 凭据级 custom_api_first=Some(true) 必须覆盖全局 false:
        // 即便该代挂号 priority 明显更差,它也要求无条件优先。
        // 这是用户要的"每个上游账号 apikey 都可以自定义"。
        let config = Config::default();
        let mgr = MultiTokenManager::new(
            config,
            vec![mk_custom(1, 99, Some(true)), mk_kiro(2, 0)],
            None,
            None,
            false,
        )
        .unwrap();
        assert!(
            mgr.should_try_custom_api_first(),
            "凭据级 custom_api_first=true 必须覆盖全局,无条件优先"
        );
    }

    #[test]
    fn test_cross_pool_global_switch_restores_legacy_behavior() {
        // 全局 custom_api_first=true 恢复历史行为:代挂号无条件优先(供"就是要中转兜底在前"的部署)。
        let mut config = Config::default();
        config.custom_api_first = true;
        let mgr = MultiTokenManager::new(
            config,
            vec![mk_custom(1, 99, None), mk_kiro(2, 0)],
            None,
            None,
            false,
        )
        .unwrap();
        assert!(
            mgr.should_try_custom_api_first(),
            "全局开关为 true 时应恢复代挂号绝对优先的历史行为"
        );
    }

    #[test]
    fn test_cross_pool_per_credential_false_overrides_global_true() {
        // 反向覆盖也必须生效:全局 true,但该号显式 false → 参与公平比较。
        let mut config = Config::default();
        config.custom_api_first = true;
        let mgr = MultiTokenManager::new(
            config,
            vec![mk_custom(1, 99, Some(false)), mk_kiro(2, 0)],
            None,
            None,
            false,
        )
        .unwrap();
        assert!(
            !mgr.should_try_custom_api_first(),
            "凭据级显式 false 必须覆盖全局 true,回到 priority 公平比较"
        );
    }

    #[test]
    fn test_cross_pool_edge_cases() {
        let config = Config::default();

        // 无代挂号 → 不必尝试透传（也省掉一次无谓的 select_custom_api）
        let only_kiro =
            MultiTokenManager::new(config.clone(), vec![mk_kiro(1, 0)], None, None, false).unwrap();
        assert!(!only_kiro.should_try_custom_api_first(), "池中无代挂号时不该尝试透传");

        // 只有代挂号 → 只能走透传（不管 priority 多大）
        let only_custom =
            MultiTokenManager::new(config.clone(), vec![mk_custom(1, 999, None)], None, None, false)
                .unwrap();
        assert!(only_custom.should_try_custom_api_first(), "只有代挂号时必须走透传");

        // 空池 → 不尝试
        let empty = MultiTokenManager::new(config.clone(), vec![], None, None, false).unwrap();
        assert!(!empty.should_try_custom_api_first(), "空池不该尝试透传");

        // 被禁用的号不参与仲裁：唯一的代挂号被禁用 → 视为无代挂号
        let mgr = MultiTokenManager::new(
            config,
            vec![mk_custom(1, 0, None), mk_kiro(2, 5)],
            None,
            None,
            false,
        )
        .unwrap();
        assert!(mgr.should_try_custom_api_first(), "禁用前:代挂 priority 更小 → 走透传");
        mgr.set_disabled(1, true).unwrap();
        assert!(
            !mgr.should_try_custom_api_first(),
            "代挂号被禁用后应视为无代挂号,不再尝试透传"
        );
    }

    #[test]
    fn test_cross_pool_cooldown_excluded_from_arbitration() {
        // 冷却中的代挂号此刻选不出来,不该影响路径决策:
        // 唯一的(且 priority 更优的)代挂号在冷却中 → 应先走 Kiro,而不是先试一次必然失败的透传。
        let config = Config::default();
        let mgr = MultiTokenManager::new(
            config,
            vec![mk_custom(1, 0, None), mk_kiro(2, 5)],
            None,
            None,
            false,
        )
        .unwrap();
        assert!(mgr.should_try_custom_api_first(), "冷却前应走透传");
        mgr.cooldown_custom_api(1, 300);
        assert!(
            !mgr.should_try_custom_api_first(),
            "唯一代挂号在冷却中时应先走 Kiro(避免白试一次注定失败的透传)"
        );
    }

    #[tokio::test]
    async fn test_acquire_context_no_busy_loop_with_only_custom_api() {
        // ⭐⭐ CPU 死循环回归(旧代码会挂死+烧满一核,本测试靠超时兜底):
        //
        // is_entry_selectable 会过滤 custom_api 号(两池隔离铁律:Kiro 路径永不碰代挂号),
        // 但 transient_wait_outcome 旧代码**漏了这道过滤**。于是当池中只有 custom_api 号时:
        //   - select_next_credential → None(全被 is_entry_selectable 过滤)
        //   - transient_wait_outcome → Available(它看不出这些号不可选,判为"立即可用")
        //   - acquire_context 的 `WaitOutcome::Available => continue` 既不 sleep 也不递增
        //     attempt_count(该分支语义是"竞态,立刻重选")
        //   → 循环顶部的 attempt_count >= max_attempts 永远不成立 → **无退出条件的忙等热循环**,
        //     请求永不返回且烧满一个 CPU 核。
        //
        // 真实触发路径:try_custom_api_passthrough 在 custom_api 全部冷却后返回 None,
        // 随即回落 Kiro 主路径调 acquire_context;以及 MCP/WebSearch 等不走透传的调用。
        //
        // 修复后应**快速返回 Err**(NoCandidate → "所有凭据均已禁用"类错误),而不是挂死。
        let config = Config::default();
        let mut c = KiroCredentials::default();
        c.id = Some(1);
        c.auth_method = Some("custom_api".to_string());
        c.base_url = Some("https://relay.example.invalid".to_string());
        let mgr = MultiTokenManager::new(config, vec![c], None, None, false).unwrap();

        let r = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            mgr.acquire_context(Some("claude-sonnet-4.5"), None),
        )
        .await;

        match r {
            Err(_) => panic!(
                "⭐acquire_context 在【池中只有 custom_api 号】时挂死(忙等热循环)。\
                 说明 transient_wait_outcome 与 is_entry_selectable 的硬门条件不对齐。"
            ),
            Ok(Ok(_)) => panic!("custom_api 号绝不该被 Kiro 主路径选中(两池隔离铁律被破坏)"),
            Ok(Err(e)) => {
                // 预期:快速失败。错误应指向无可用凭据,而非竞态收敛失败。
                let msg = e.to_string();
                assert!(
                    !msg.contains("竞态无法收敛"),
                    "不该退化到竞态兜底上限才结束,说明 transient_wait_outcome 仍未正确过滤: {msg}"
                );
            }
        }
    }

    #[tokio::test]
    async fn test_acquire_context_no_busy_loop_when_model_blocked_for_all() {
        // ⭐ 同一死循环的第二条触发路径:某模型被池中**所有**号加进 model_blocklist
        //（每个号都曾对该模型返回 INVALID_MODEL_ID,TTL 1800s）之后,再来一个同模型请求。
        // is_entry_selectable 会因 is_model_blocked 过滤掉全部号 → select 返 None;
        // 而 transient_wait_outcome 旧代码不检查 model_blocklist → 判 Available → 忙等挂死。
        let config = Config::default();
        let mut c = KiroCredentials::default();
        c.id = Some(1);
        c.auth_method = Some("api_key".to_string());
        c.kiro_api_key = Some("sk-test-key".to_string());
        let mgr = MultiTokenManager::new(config, vec![c], None, None, false).unwrap();

        const MODEL: &str = "claude-opus-4.5";
        // 把该模型对唯一的号拉黑(模拟上游回过 INVALID_MODEL_ID)。
        mgr.report_model_invalid(1, Some(MODEL));

        let r = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            mgr.acquire_context(Some(MODEL), None),
        )
        .await;

        match r {
            Err(_) => panic!(
                "⭐acquire_context 在【该模型已被全池拉黑】时挂死(忙等热循环)。\
                 说明 transient_wait_outcome 缺少 is_model_blocked 过滤。"
            ),
            Ok(Ok(_)) => panic!("该模型已被唯一的号拉黑,不该还能选出号"),
            Ok(Err(e)) => assert!(
                !e.to_string().contains("竞态无法收敛"),
                "不该退化到竞态兜底上限才结束,说明过滤仍不对齐: {e}"
            ),
        }
    }

    /// 回归（审查发现）：**永久性**模型硬门（订阅档位/成本白名单）不得报成可重试的 429。
    ///
    /// **首版修复为何不够**：我原先对拿不到 blocklist TTL 的情形一律 `unwrap_or(MODEL_BLOCK_TTL)`
    /// 兜底 → 带 `retry_after_secs`。但 `allowed_models` 白名单不含该模型、或 FREE 档请求 opus，
    /// 是**永久**状态（等多久都不会变）。带退避秒数会让客户端每 5 分钟重试一次直到永远
    /// （下游还会把秒数 clamp 到 300）—— 只是把 502 死循环换成了 429 死循环。
    ///
    /// 本测试用 `allowed_models` 白名单造永久门，断言错误里**没有** retry_after 标记、
    /// 且带 `model_unsupported_by_pool=1`（供 map_provider_error 映射成不可重试的 404）。
    #[tokio::test]
    async fn test_permanent_model_gate_is_not_reported_as_retryable() {
        let config = Config::default();
        let mut c = KiroCredentials::default();
        c.id = Some(1);
        c.auth_method = Some("api_key".to_string());
        c.kiro_api_key = Some("sk-test-key".to_string());
        // 成本白名单只允许一个便宜模型 → 请求其它模型时是**永久**不可用，不是限时
        c.allowed_models = Some(vec!["qwen3-coder-next".to_string()]);
        let mgr = MultiTokenManager::new(config, vec![c], None, None, false).unwrap();

        let msg = match tokio::time::timeout(
            std::time::Duration::from_secs(3),
            mgr.acquire_context(Some("claude-opus-5"), None),
        )
        .await
        {
            Err(_) => panic!("不应挂死"),
            Ok(Ok(_)) => panic!("白名单不含该模型，不该选出号"),
            Ok(Err(e)) => e.to_string(),
        };

        assert!(
            !msg.contains("retry_after_secs="),
            "永久性硬门绝不能带退避秒数（会让客户端无限重试一个永不成功的请求）: {msg}"
        );
        assert!(
            msg.contains("model_unsupported_by_pool=1"),
            "须带显式标记供 map_provider_error 映射成不可重试的 404（不靠中文文案匹配）: {msg}"
        );
    }

    /// 回归：模型级硬门挡掉全部号时，错误必须是**可重试的模型态**，而不是"号池已禁用"。
    ///
    /// **旧代码为何 FAIL**：`WaitOutcome::NoCandidate` 分支对两种成因报同一句
    /// `所有凭据均已禁用（{available}/{total}）`。号没被禁用、只是被 `model_blocklist` 挡掉时，
    /// 它会产出自相矛盾的 `所有凭据均已禁用（1/1）`，且该串**匹配不上 `map_provider_error`
    /// 的任何分支**（无 429/QUOTA 类关键词、无 `retry_after_secs` 标记）→ 落末尾兜底
    /// **502 BAD_GATEWAY 且无 Retry-After**。
    ///
    /// 而 `model_blocklist` 是**限时态**（`MODEL_BLOCK_TTL` = 1800s，到期自动放行重试探），
    /// 报成 502 等于把可重试的临时态表达成服务端故障：客户端（Claude Code）既不退避也不换模型，
    /// 原样重发 → 再 502。线上 24h 实测 577 次此类假报（订阅不含的 gpt-5.6-* / deepseek / glm 各 ~85，
    /// claude-opus-5 88），同时污染"号池耗尽"统计口径，使真实耗尽（3221 次 available=0）无法评估。
    ///
    /// 本测试的两条断言分别锁住"不再谎报禁用"与"必须带可退避标记"，去掉修复中任一半都会 FAIL。
    #[tokio::test]
    async fn test_model_gated_pool_reports_retryable_model_error_not_pool_exhausted() {
        let config = Config::default();
        let mut c = KiroCredentials::default();
        c.id = Some(1);
        c.auth_method = Some("api_key".to_string());
        c.kiro_api_key = Some("sk-test-key".to_string());
        let mgr = MultiTokenManager::new(config, vec![c], None, None, false).unwrap();

        const MODEL: &str = "gpt-5.6-sol";
        // 唯一的号对该模型返 INVALID_MODEL_ID → 进模型级黑名单。号本身**未被禁用**。
        mgr.report_model_invalid(1, Some(MODEL));
        assert_eq!(mgr.available_count(), 1, "前提：号未被禁用，只是被模型硬门挡住");

        // 用 match 而非 expect_err：CallContext 不实现 Debug，且不该为测试便利给生产类型加 derive。
        let msg = match tokio::time::timeout(
            std::time::Duration::from_secs(3),
            mgr.acquire_context(Some(MODEL), None),
        )
        .await
        {
            Err(_) => panic!("acquire_context 不应挂死"),
            Ok(Ok(_)) => panic!("该模型已被唯一的号拉黑，不该还能选出号"),
            Ok(Err(e)) => e.to_string(),
        };

        assert!(
            !msg.contains("所有凭据均已禁用"),
            "号未被禁用却谎报'所有凭据均已禁用'（旧代码即如此，且该串会落 502 兜底）: {msg}"
        );
        assert!(
            msg.contains("retry_after_secs="),
            "模型级黑名单是限时态，必须带 retry_after_secs 才能被 map_provider_error 映射成 \
             可重试的 429（旧代码无此标记 → 502 无 Retry-After）: {msg}"
        );
    }

    /// ⭐ 致命缺陷回归（去掉 bail 串里的 `retry_after_secs=` 即 FAIL）：
    /// **号池真耗尽**（`available == 0`）必须带可退避标记。
    ///
    /// 与上面 `test_model_gated_pool_reports_retryable_model_error_not_pool_exhausted` 是
    /// 同一类缺陷的两个实例。0.7.45 修了情形②（模型硬门），情形①「真耗尽」当时漏了 ——
    /// 而线上量最大的是①：2026-08-03 01:55–02:10 号池被上游风控烧空的 15 分钟窗口里，
    /// `所有凭据均已禁用（0/0）` 产生 **2082 次**，单个 5 分钟桶峰值 937 次，
    /// 且该窗口内未识别兜底 502 **全部**是这一种。
    ///
    /// 不带标记时该串逐条穿过 `map_provider_error` 的所有分支 → 落末尾兜底 →
    /// 502 无 Retry-After → 客户端（Claude Code）把它当服务端故障、退避逻辑不启动、
    /// 原样重发 → 又 502（放大了耗尽窗口内的请求量）。
    ///
    /// 用 `AccountSuspended` 禁用而非 `TooManyFailures`：后者在
    /// `is_self_healable_reason` 覆盖范围内，会触发全池自愈把号重新启用 →
    /// 走不到 bail 分支。`AccountSuspended` 被刻意排除在自愈之外（见该函数文档），
    /// 故能稳定复现"池子确实空且不会自愈"这个终态。
    #[tokio::test]
    async fn test_truly_exhausted_pool_bail_carries_retry_after_marker() {
        let config = Config::default();
        let mut c = KiroCredentials::default();
        c.id = Some(1);
        c.auth_method = Some("api_key".to_string());
        c.kiro_api_key = Some("sk-test-key".to_string());
        let mgr = MultiTokenManager::new(config, vec![c], None, None, false).unwrap();

        // 唯一的号被真封禁用（不可自愈）→ available 归零。
        mgr.report_account_suspended(1);
        assert_eq!(mgr.available_count(), 0, "前提：池子必须真的空");

        let msg = match tokio::time::timeout(
            std::time::Duration::from_secs(3),
            mgr.acquire_context(None, None),
        )
        .await
        {
            Err(_) => panic!("acquire_context 不应挂死"),
            Ok(Ok(_)) => panic!("池子已空，不该还能选出号"),
            Ok(Err(e)) => e.to_string(),
        };

        assert!(
            msg.contains("所有凭据均已禁用"),
            "真耗尽应如实报耗尽（这条是与模型硬门情形的分界，不该混淆）: {msg}"
        );
        assert!(
            msg.contains("retry_after_secs="),
            "真耗尽是可自愈的临时态，必须带 retry_after_secs 才能被 map_provider_error \
             映射成 429 + Retry-After（旧代码无此标记 → 502 无 Retry-After → 客户端不退避 \
             → 线上 15 分钟内 2082 次）: {msg}"
        );
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

    // ============ 余额加权分流(0.7.24,旧代码上会失败:旧代码无 balance_factor/set_balance_snapshots)============

    fn mk_bal_snap(remaining: f64, effective_limit: f64, used_at_cache: f64) -> BalanceSnapshot {
        BalanceSnapshot { remaining_at_cache: remaining, effective_limit, credits_used_at_cache: used_at_cache }
    }

    /// 余额加权因子数学:满额→1.0、半额→floor+0.5×(1-floor)、耗尽→floor;缺快照/关闭→中性 1.0。
    #[test]
    fn test_balance_factor_math_and_neutral() {
        let m = make_balanced_manager(1);
        // 默认 floor=50 → 0.5。满额(remaining=eff)→ factor=1.0。
        m.set_balance_snapshots(HashMap::from([(1, mk_bal_snap(100.0, 100.0, 0.0))]));
        assert!((m.balance_factor(1, 0.0) - 1.0).abs() < 1e-9, "满额 → 1.0");
        // 半额(remaining=50/100)→ 0.5 + 0.5×0.5 = 0.75。
        m.set_balance_snapshots(HashMap::from([(1, mk_bal_snap(50.0, 100.0, 0.0))]));
        assert!((m.balance_factor(1, 0.0) - 0.75).abs() < 1e-9, "半额 → 0.75");
        // 耗尽(remaining=0)→ floor=0.5。
        m.set_balance_snapshots(HashMap::from([(1, mk_bal_snap(0.0, 100.0, 0.0))]));
        assert!((m.balance_factor(1, 0.0) - 0.5).abs() < 1e-9, "耗尽 → floor 0.5");
        // 缺快照 → 中性 1.0(新号不被惩罚)。
        m.set_balance_snapshots(HashMap::new());
        assert!((m.balance_factor(1, 0.0) - 1.0).abs() < 1e-9, "缺快照 → 中性 1.0");
        // 加权关 → 恒中性 1.0(退回纯 0.7.23)。
        m.set_balance_snapshots(HashMap::from([(1, mk_bal_snap(0.0, 100.0, 0.0))]));
        m.balance_weight_enabled.store(false, Ordering::Relaxed);
        assert!((m.balance_factor(1, 0.0) - 1.0).abs() < 1e-9, "开关关 → 中性 1.0");
    }

    /// 本地累加修正:快照后本地花费增量拉低估算剩余 → 因子下降(比纯 30 分钟旧快照更准)。
    #[test]
    fn test_balance_factor_local_correction() {
        let m = make_balanced_manager(1);
        // 快照:满额 100,基线花费 200(生命周期累计)。
        m.set_balance_snapshots(HashMap::from([(1, mk_bal_snap(100.0, 100.0, 200.0))]));
        // 当前累计花费仍 200(无新增)→ 满额 → 1.0。
        assert!((m.balance_factor(1, 200.0) - 1.0).abs() < 1e-9, "无新增 → 满额");
        // 当前累计花费 250(快照后新花 50)→ est_remaining=100-50=50 → 半额 → 0.75。
        assert!((m.balance_factor(1, 250.0) - 0.75).abs() < 1e-9, "快照后花 50 → 估半额 0.75");
        // 花费退回(重置/负增量)钳到 0 → 满额,不因时钟/重置乱跳。
        assert!((m.balance_factor(1, 150.0) - 1.0).abs() < 1e-9, "负增量钳 0 → 满额");
    }

    /// 性质守卫：**全新号池（所有排序键平局）必须把流量铺开**，不得钉在下标最小的号上。
    ///
    /// 平局条件（缺一个就会被别的键打破、测不出 jitter 的作用）：
    /// - `credential_rpm_limit` 设极大 → `rpm_usage_permille = rpm*1000/cap` 恒为 0
    ///   （否则 `commit_selection` 的 `rpm.record` 会立刻让它分叉）；
    /// - 每次取号后立即 drop → `inflight` 归零；
    /// - 不调 `report_success` → `success_count` 恒 0；
    /// - 同 priority、同健康（全新号 EWMA 均为乐观初值）。
    ///
    /// 这是"刚补一批新号、池子空闲"时的真实形态（新号 success_count 全为 0）。
    ///
    /// ⚠️ 本测试**不断言实现方式**，只断言可观测性质。历史注记：曾为此加过一个随机
    /// 打散键 `tie_break_jitter`，但两次尝试都无法构造出"移除它即失败"的测试
    /// （本测试在有无该键时都通过），说明平局在实际调用序列里已被其它键打破。
    /// 按"无证据支撑的改动不保留"的原则该键已删除，本测试作为性质回归留下。
    #[tokio::test]
    async fn test_jitter_breaks_full_tie_among_fresh_credentials() {
        let mut config = Config::default();
        config.load_balancing_mode = "balanced".to_string();
        config.affinity_enabled = false;
        // 关键：容量极大，让 rpm_usage_permille 恒为 0，不去打破平局
        config.credential_rpm_limit = 1_000_000;
        let creds: Vec<KiroCredentials> = (0..4)
            .map(|i| {
                let mut c = KiroCredentials::default();
                c.priority = 0;
                c.access_token = Some(format!("tok-{i}"));
                c.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());
                c
            })
            .collect();
        let manager = MultiTokenManager::new(config, creds, None, None, false).unwrap();

        let mut hits: std::collections::HashMap<u64, usize> = std::collections::HashMap::new();
        for _ in 0..80 {
            let g = manager.acquire_context(None, None).await.unwrap();
            *hits.entry(g.id).or_default() += 1;
            drop(g); // inflight 归零，下一轮重新全平局
        }
        assert!(
            hits.len() >= 3,
            "全新号池应把流量铺开到多个号，实际只用了 {} 个：{hits:?}",
            hits.len()
        );
    }

    /// 回归（S2 封号原因持久化）：自动禁用原因**不得**在重启后退化成「手动禁用」。
    ///
    /// **旧代码为何失败**：`KiroCredentials` 只有 `disabled: bool`、没有原因字段；
    /// `persist_credentials` 只写 `cred.disabled`；而加载路径对**所有** disabled 号
    /// 一律回填 `Some(DisabledReason::Manual)`。于是 `QuotaExceeded` / `AccountSuspended` /
    /// `SuspiciousActivityAuto` / `TooManyFailures` 等自动禁用原因重启即全部丢失
    /// —— 用户明确要求的「认定封号必须标明原因」当前是重启即失效。
    /// 并且以 reason 为判据的自愈逻辑会把自动禁用误判成人工禁用。
    ///
    /// 本测试走**真实的序列化往返**（serde_json），断言原因与时刻都能穿过重启。
    /// 旧代码下 `KiroCredentials` 连这两个字段都没有，反序列化后 `disabled_reason` 为 None
    /// → 加载路径回填 Manual → 断言失败。
    #[test]
    fn test_disabled_reason_survives_persist_roundtrip() {
        let mut cred = KiroCredentials::default();
        cred.id = Some(7);
        cred.disabled = true;
        cred.disabled_reason = Some(DisabledReason::AccountSuspended);
        cred.disabled_at = Some("2026-07-29T10:00:00+00:00".to_string());

        // 真实往返：写盘格式 → 读回
        let json = serde_json::to_string(&cred).expect("序列化");
        let back: KiroCredentials = serde_json::from_str(&json).expect("反序列化");

        assert_eq!(
            back.disabled_reason,
            Some(DisabledReason::AccountSuspended),
            "自动禁用原因必须穿过持久化往返（旧代码无此字段 → 恒 None → 加载时被回填成 Manual）"
        );
        assert_eq!(
            back.disabled_at.as_deref(),
            Some("2026-07-29T10:00:00+00:00"),
            "禁用时刻必须持久化，运维需要它判断『这号坏了多久』"
        );
        assert!(back.disabled, "disabled 本身当然也要保留");

        // 线格式必须是稳定的 camelCase（外部脚本/前端按它读）
        assert!(
            json.contains("\"disabledReason\":\"accountSuspended\""),
            "线格式应为 camelCase 稳定命名，实际 json={json}"
        );
    }

    /// 回归（S2 向后兼容）：旧凭据文件（无 `disabledReason` 字段）必须仍能加载。
    ///
    /// 新增字段用 `#[serde(default)]`，所以旧文件读回来是 `None`，
    /// 加载路径再回落 `Manual` —— 这是**刻意保留**的降级行为，不是缺陷。
    #[test]
    fn test_legacy_credentials_without_reason_still_load() {
        let legacy = r#"{"id":9,"accessToken":"t","refreshToken":"r","disabled":true}"#;
        let cred: KiroCredentials = serde_json::from_str(legacy).expect("旧格式必须仍可解析");
        assert!(cred.disabled);
        assert_eq!(cred.disabled_reason, None, "旧文件无该字段 → None（加载路径回落 Manual）");
        assert_eq!(cred.disabled_at, None);
    }

    /// 回归（G2 反饥饿强制探测 · 结构性兜底）：健康分最差的可选号也不得被永久排除。
    ///
    /// **旧代码为何失败**：`health_tier` 在排序键的高位，最差档号只在高档全部不可用时才被选到。
    /// 而健康分的上升路径依赖 `on_success` —— 拿不到请求就没有成功 → 永久留在最差档。
    /// 线上实测：6 号池 4 个号进 T2 且 `rpm=0 inflight=0` 空转，有效容量 6→3，全程零 429。
    ///
    /// 本测试用**两个号 + 极少轮次**保证对照干净：
    /// - `credential_rpm_limit` 设很大，避免多轮选号把 RPM 打饱和从而污染排序（那会让
    ///   两个号都变 `unusable`、测试失去区分度 —— 这正是本测试第一版失败的原因）；
    /// - 只跑 6 轮，远小于任何饱和阈值；
    /// - #1 打成"健康分最差 + 已饥饿"，#2 保持健康。
    ///
    /// 有反饥饿键时 #1 至少被探测到一次；把 `starved` 常量化（模拟旧代码）后 #1 恒 0 次。
    #[tokio::test]
    async fn test_starved_credential_is_force_probed() {
        let mut config = Config::default();
        config.load_balancing_mode = "balanced".to_string();
        config.affinity_enabled = false;
        // 关键：抬高 RPM 容量，确保 6 轮选号绝不触发饱和（饱和会让两个号都 unusable）
        config.credential_rpm_limit = 100_000;
        let creds: Vec<KiroCredentials> = (0..2)
            .map(|i| {
                let mut c = KiroCredentials::default();
                c.priority = 0;
                c.access_token = Some(format!("tok-{i}"));
                c.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());
                c
            })
            .collect();
        let manager = MultiTokenManager::new(config, creds, None, None, false).unwrap();

        // #1 健康分打到最差档（连续 429），但**不让它熔断沉底**：
        // 熔断 Open 会让 p_avail=0 → unusable=1 → 本就该沉底，那不是饥饿场景。
        // 故只打到 TRIP_THRESHOLD-1 次，保持 Closed 但健康分很低。
        let fam1 = manager.family_key_of(1);
        for _ in 0..(crate::kiro::health::TRIP_THRESHOLD - 1) {
            manager.health.on_429(&fam1);
        }
        {
            let snap = manager.health.snapshot(&fam1).unwrap();
            assert!(!snap.circuit_open, "前置：#1 不应熔断（否则测的不是饥饿）");
            assert!(
                snap.health < crate::kiro::health::HEALTH_TIER_HEALTHY_MIN,
                "前置：#1 应已跌出健康档，实际 {}",
                snap.health
            );
        }
        // 让 #1 处于"已饥饿"（上次被选中远在探测窗口外）
        {
            let entries = manager.entries.lock();
            let e1 = entries.iter().find(|e| e.id == 1).unwrap();
            e1.last_selected_at.set(
                Instant::now() - StdDuration::from_secs(STARVATION_PROBE_SECS + 60),
            );
        }

        let mut hit_1 = 0usize;
        for _ in 0..6 {
            let g = manager.acquire_context(None, None).await.unwrap();
            if g.id == 1 {
                hit_1 += 1;
            }
            drop(g);
        }
        assert!(
            hit_1 > 0,
            "健康分最差但仍可选的饥饿号必须被强制探测到（旧代码 0 次），实际 {hit_1} 次"
        );
    }

    /// 回归（低负载分流偏斜 · 本轮修复）：请求归零后再选，不得恒偏向同一个号。
    ///
    /// **旧代码为何失败**：低负载下排序键的前几位会**全部平局** —— 全池健康（①②③ 恒等）、
    /// 每个请求处理完 inflight 即归零（④⑤ 恒 0）、`rpm_usage_permille` 用 60s 滑窗且
    /// 过期即归零（⑥ 追不上）。而 `min_by_key` 全平局时**恒返回第一个元素**，于是流量
    /// 持续偏向 `entries` 里下标靠前的号。
    ///
    /// 线上 6 号池实测（52 次请求全部成功、无坏号）：gini **0.378**、最热/最冷 **6.67x**，
    /// idx0 的 #208 拿 20 次而 idx5 的 #213 只拿 3 次。
    ///
    /// 本测试每次取号后**立即 drop guard**（inflight 归零），复现那个"每次都从全平局
    /// 开始"的低负载场景。旧代码下会 100% 选中 id=1，`distinct == 1` → 断言失败。
    #[tokio::test]
    async fn test_low_load_selection_does_not_always_pick_first_entry() {
        let manager = make_balanced_manager(6);
        // 同优先级才是真并列（make_balanced_manager 给的是 0..n）
        {
            let mut entries = manager.entries.lock();
            for e in entries.iter_mut() {
                e.credentials.priority = 0;
            }
        }
        let mut hits: std::collections::HashMap<u64, usize> = std::collections::HashMap::new();
        for _ in 0..120 {
            let g = manager.acquire_context(None, None).await.unwrap();
            *hits.entry(g.id).or_default() += 1;
            drop(g); // 立即归零 inflight —— 关键：让下一轮又从"全平局"开始
        }
        let distinct = hits.len();
        assert!(
            distinct >= 5,
            "6 号池 120 次低负载选号应铺开到几乎所有号，实际只用了 {distinct} 个：{hits:?}\
             （旧代码恒选下标最小的号，distinct==1）"
        );
        // 最热号不应垄断：完美均分是 20 次/号，给足容差但要挡住 6.67x 那种偏斜。
        let max = *hits.values().max().unwrap();
        assert!(
            max <= 60,
            "最热号拿了 {max}/120 次，偏斜过大：{hits:?}"
        );
    }

    /// 余额加权作为末位 tie-break 生效:两号同优先级/同健康/同在途 0/同 RPM 时,余额多的先选。
    /// 旧代码无余额概念,两号完全并列靠 id 兜底(选 #1);新代码 #2 余额满、#1 半额 → 首取应是 #2。
    #[tokio::test]
    async fn test_balance_weight_breaks_tie_toward_richer() {
        let manager = make_balanced_manager(2);
        // 同优先级(make_balanced 给的是 0/1,改成都 0 才是真并列)——直接改 entries 优先级。
        {
            let mut entries = manager.entries.lock();
            for e in entries.iter_mut() {
                e.credentials.priority = 0;
            }
        }
        // #1 半额(factor 0.75),#2 满额(factor 1.0)。其余全并列(在途 0、RPM 0、健康满)。
        manager.set_balance_snapshots(HashMap::from([
            (1, mk_bal_snap(50.0, 100.0, 0.0)),
            (2, mk_bal_snap(100.0, 100.0, 0.0)),
        ]));
        // 首取(此刻两号在途都 0)→ 余额是唯一区分键 → 选余额多的 #2。
        let g = manager.acquire_context(None, None).await.unwrap();
        assert_eq!(g.id, 2, "全并列时余额多的 #2 应先选(余额末位 tie-break),实际 #{}", g.id);
    }

    /// 余额加权是软偏置微调,**不掀翻在途分流**:#1 余额耗尽但在途少、#2 满额但在途多,
    /// 仍先选在途少的 #1(在途是第 4 位主键,余额只在第 6 位末位)。证明不打架 0.7.23。
    #[tokio::test]
    async fn test_balance_weight_does_not_override_inflight() {
        let manager = make_balanced_manager(2);
        {
            let mut entries = manager.entries.lock();
            for e in entries.iter_mut() {
                e.credentials.priority = 0;
            }
        }
        // #1 余额耗尽(factor 0.5),#2 满额(factor 1.0)。
        manager.set_balance_snapshots(HashMap::from([
            (1, mk_bal_snap(0.0, 100.0, 0.0)),
            (2, mk_bal_snap(100.0, 100.0, 0.0)),
        ]));
        // 给 #2 压 3 个在途(直接改 inflight 原子,不走选号避免污染)。
        {
            let entries = manager.entries.lock();
            let e2 = entries.iter().find(|e| e.id == 2).unwrap();
            for _ in 0..3 {
                e2.inflight.fetch_add(1, Ordering::Release);
            }
        }
        // 选号:#1 在途 0、#2 在途 3。在途(第4位主键)先决 → 选 #1,尽管 #1 余额因子更低。
        let g = manager.acquire_context(None, None).await.unwrap();
        assert_eq!(g.id, 1, "在途少的 #1 应先选(在途主键压过余额末位键),实际 #{}", g.id);
    }

    /// T1(L2 回归,旧代码必挂):健康不对称下同优先级仍按负载分流,不被最健康的号吸走整轮。
    /// 旧代码 neg_p_bucket 首排:健康分高的号哪怕在途多也压过空闲的稍弱号 → 突发全扑一个号。
    /// L2 后:健康降 3 档粗门,同档内在途最少优先 → 3 个并发在途分摊到不同号。
    #[tokio::test]
    async fn test_l2_health_asymmetric_still_spreads() {
        let manager = make_balanced_manager(3);
        // 造同档内 p_avail 不对称:#1 满血(p=1.0),#2/#3 各记 3 次近窗 RPM 制造轻微 rpm_pressure
        // (cap 默认 25,pressure=3/25=0.12 → p≈0.88),三者都在 healthy 档(≥0.75,tier 同为 0)。
        // 旧代码:neg_p_bucket 首要连续键,#1 的 p 最高;每 +1 在途只压 ~6 分,压不过 #1 对 #2/#3 的
        // 健康优势 → 3 个并发全吸到 #1(惊群)。L2:同档内在途最少优先 → 分摊到 3 个号。
        manager.rpm.record(2);
        manager.rpm.record(2);
        manager.rpm.record(2);
        manager.rpm.record(3);
        manager.rpm.record(3);
        manager.rpm.record(3);
        // 连取 3 个 context 不释放(guard 持有 = 模拟 3 个并发在途)。
        let a = manager.acquire_context(None, None).await.unwrap();
        let b = manager.acquire_context(None, None).await.unwrap();
        let c = manager.acquire_context(None, None).await.unwrap();
        let mut ids = [a.id, b.id, c.id];
        ids.sort();
        assert_eq!(
            ids, [1, 2, 3],
            "3 个并发在途应分摊到 3 个不同号(同档内在途最少优先,不吸附到最健康的 #1),实际 {:?}",
            ids
        );
    }

    /// T2(L4 硬门回归,旧代码必挂):常载下 RPM 不越限——两趟选号只在非饱和候选里选。
    /// 旧代码饱和只是软降权,整池未满时也可能把 burst 拍到排序靠前的号使其越限;L4 硬门过滤饱和号。
    #[tokio::test]
    async fn test_l4_hard_gate_no_overshoot() {
        let mut config = Config::default();
        config.load_balancing_mode = "balanced".to_string();
        config.affinity_enabled = false;
        config.credential_rpm_limit = 3;
        config.rpm_headroom_factor = 100; // 隔离 headroom,只验硬门:阈值恰为 3
        let creds: Vec<KiroCredentials> = (1..=2)
            .map(|i| {
                let mut c = KiroCredentials::default();
                c.priority = 0;
                c.access_token = Some(format!("tok-{}", i));
                c.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());
                c
            })
            .collect();
        let manager = MultiTokenManager::new(config, creds, None, None, false).unwrap();

        // 取 6 次(=2 号 × 3),每次释放 guard(只累积 rpm.record)。硬门保证不越各号 limit=3。
        for _ in 0..6 {
            let ctx = manager.acquire_context(None, None).await.unwrap();
            drop(ctx);
        }
        let snap = manager.snapshot();
        for e in &snap.entries {
            assert!(
                manager.rpm.count(e.id) <= 3,
                "L4 硬门:#{} 近窗 RPM {} 不应越过 limit=3",
                e.id,
                manager.rpm.count(e.id)
            );
        }
        // 总量恰好 6(2×3),证明两号都被用满而非一个越限、一个闲置。
        let total: u32 = snap.entries.iter().map(|e| manager.rpm.count(e.id)).sum();
        assert_eq!(total, 6, "两号各 3 次共 6,负载均摊到容量");
    }

    /// T2b(L4 背压路径):背压开 + 整池 RPM 饱和时,transient_wait_outcome 返回
    /// Wait(≤60s, RpmRecovery),使 acquire_context 等待 RPM 恢复而非误判"所有凭据均已禁用"bail。
    /// 背压关时饱和号算立即可用候选 → Available(不等待,保持默认行为)。
    #[tokio::test]
    async fn test_l4_backpressure_waits_on_rpm_saturation() {
        let mut config = Config::default();
        config.load_balancing_mode = "balanced".to_string();
        config.affinity_enabled = false;
        config.credential_rpm_limit = 2;
        config.rpm_headroom_factor = 100;
        config.rpm_hard_gate_overload_wait = true; // 开背压
        let mut c = KiroCredentials::default();
        c.access_token = Some("tok-1".to_string());
        c.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());
        let manager = MultiTokenManager::new(config, vec![c], None, None, false).unwrap();

        // 打到饱和(limit=2)。
        manager.rpm.record(1);
        manager.rpm.record(1);
        assert!(manager.is_rpm_saturated(1), "应已饱和");
        // 背压开:该号是"将恢复的等待候选",返回 Wait(≤60s, RpmRecovery)——原因是 RPM 恢复而非冷却,
        // 调用方据此绝不 cooling-fast-fail、绝不报"已禁用"(D1 修复核心)。
        match manager.transient_wait_outcome(None) {
            WaitOutcome::Wait(d, reason) => {
                assert_eq!(reason, WaitReason::RpmRecovery, "饱和的等待原因应是 RpmRecovery 而非 Cooling");
                assert!(d <= StdDuration::from_secs(60), "恢复窗口不超过 60s 窗口长度");
            }
            other => panic!("背压开 + 饱和应返回 Wait(RpmRecovery),实际 {:?}", other),
        }

        // 对照:背压关时,饱和号仍算立即可用候选 → Available(不等待,保持默认行为)。
        manager.rpm_hard_gate_overload_wait.store(false, Ordering::Relaxed);
        assert_eq!(
            manager.transient_wait_outcome(None),
            WaitOutcome::Available,
            "背压关:饱和号是立即可选候选,不等待(保持默认行为)"
        );
    }

    /// D1 回归(旧代码上会失败):RPM 饱和的等待原因是 RpmRecovery(非 Cooling),确保调用方不会把
    /// "整池 RPM 饱和"误当"全在冷却"而 cooling-fast-fail、也不会因去饱和竞态误报"已禁用"。
    /// 旧实现 transient_wait_duration 返回无类型 Option<Duration>,冷却/RPM 饱和不可区分——本测坐实已分型。
    #[tokio::test]
    async fn test_l4_backpressure_wait_reason_is_rpm_not_cooling() {
        let mut config = Config::default();
        config.load_balancing_mode = "balanced".to_string();
        config.affinity_enabled = false;
        config.credential_rpm_limit = 2;
        config.rpm_headroom_factor = 100;
        config.rpm_hard_gate_overload_wait = true;
        config.cooldown_enabled = true; // 冷却开着,但该号没被冷却——只是 RPM 饱和
        let mut c = KiroCredentials::default();
        c.access_token = Some("tok-1".to_string());
        c.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());
        let manager = MultiTokenManager::new(config, vec![c], None, None, false).unwrap();
        manager.rpm.record(1);
        manager.rpm.record(1);
        // 无冷却记录 → 若原因被误判为 Cooling,长窗口下会 cooling-fast-fail;分型后必为 RpmRecovery。
        assert!(
            matches!(
                manager.transient_wait_outcome(None),
                WaitOutcome::Wait(_, WaitReason::RpmRecovery)
            ),
            "未冷却、仅 RPM 饱和 → 等待原因必须是 RpmRecovery(不得误当 Cooling)"
        );
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
        // L3 headroom 默认 factor=85 → 兜底 30 打折为 floor(30×0.85)=25,饱和阈值提前到 25(留 15% 缓冲)。
        let config = Config::default(); // credential_rpm_limit=0, rpm_headroom_factor=85
        let mut c1 = KiroCredentials::default();
        c1.access_token = Some("tok1".to_string());
        c1.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());
        let manager = MultiTokenManager::new(config, vec![c1], None, None, false).unwrap();
        // 默认兜底 30 × 0.85 = 25:打 24 次不饱和,第 25 次达 headroom 后阈值饱和。
        for _ in 0..24 {
            manager.rpm.record(1);
        }
        assert!(!manager.is_rpm_saturated(1), "默认兜底 30×0.85=25,打 24 次不应饱和");
        manager.rpm.record(1);
        assert!(manager.is_rpm_saturated(1), "打到 25(headroom 后阈值)应触发饱和");
    }

    // ============ rpm_saturation_gate_active(虚假饱和告警修复,旧代码没有此函数,
    // 下面的"旧行为"用等价断言复现) ============

    /// ⭐⭐ 调度归一化回归（**旧代码必失败**）：priority 模式下 RPM 饱和硬门**必须真正生效**，
    /// 饱和的高优先级号必须优雅溢出到下一优先级层。
    ///
    /// 本测试的前身断言的是相反的事（"priority 模式下饱和不影响选号，仍选 #1"）——那记录的是
    /// 归一化前的**缺陷行为**：priority 分支只有 `min_by_key(|e| e.credentials.priority)` 一行，
    /// 不看饱和/熔断/inflight，于是 #1 被打爆后流量仍全压在它身上，旁边空闲的 #2 一个都接不到。
    /// 归一化（priority ≡ balanced + priority_in_balanced=true）后，priority 语义保留为
    /// "按优先级**分层**"，但排序键第①位 `unusable`（含饱和判定）会让打爆的整层沉底 → 溢出到 #2。
    #[tokio::test]
    async fn test_priority_mode_saturated_credential_spills_over() {
        let mut config = Config::default();
        config.load_balancing_mode = "priority".to_string(); // 出厂默认值，写明意图
        config.affinity_enabled = false;
        config.credential_rpm_limit = 2;
        config.rpm_headroom_factor = 100; // 隔离 headroom，让阈值恰为 2

        let mut c1 = KiroCredentials::default();
        c1.priority = 0; // 高优先级
        c1.access_token = Some("tok1".to_string());
        c1.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());
        let mut c2 = KiroCredentials::default();
        c2.priority = 1; // 低优先级
        c2.access_token = Some("tok2".to_string());
        c2.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());

        let manager = MultiTokenManager::new(config, vec![c1, c2], None, None, false).unwrap();

        // 把 #1 打到远超阈值 → 饱和
        for _ in 0..6 {
            manager.rpm.record(1);
        }
        assert!(manager.is_rpm_saturated(1), "前置条件:#1 已超过软上限");

        // 归一化后硬门在两种模式下都生效（面板据此报告 rpmSaturated 才与调度一致）
        assert!(
            manager.rpm_saturation_gate_active(),
            "归一化后 priority 模式同样走排序键，饱和硬门必须生效"
        );

        // ⭐核心断言：应溢出到未饱和的 #2，而不是死磕已打爆的高优先级 #1。
        let ctx = manager.acquire_context(None, None).await.unwrap();
        assert_eq!(
            ctx.id, 2,
            "高优先级号已饱和时必须优雅溢出到 #2（旧代码恒返 1：裸 min_by_key(priority) 不看饱和）"
        );
    }

    /// ⭐ 零回归对照：priority 语义**必须保留** —— 高优先级号健康时绝不提前用低优先级号。
    /// 与上一个测试构成一对，证明归一化不是"把 priority 变成纯均衡"，而是"分层 + 层内均衡"。
    #[tokio::test]
    async fn test_priority_mode_healthy_high_priority_not_bypassed() {
        let mut config = Config::default();
        config.load_balancing_mode = "priority".to_string();
        config.affinity_enabled = false;
        config.credential_rpm_limit = 100; // 阈值放大，保证不饱和

        let mut c1 = KiroCredentials::default();
        c1.priority = 0;
        c1.access_token = Some("tok1".to_string());
        c1.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());
        let mut c2 = KiroCredentials::default();
        c2.priority = 1;
        c2.access_token = Some("tok2".to_string());
        c2.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());

        let manager = MultiTokenManager::new(config, vec![c1, c2], None, None, false).unwrap();

        let ctx = manager.acquire_context(None, None).await.unwrap();
        assert_eq!(ctx.id, 1, "高优先级号健康时必须优先使用，priority 语义不能丢");
    }

    /// ⭐ 归一化回归（**旧代码必失败**）：同优先级多号必须**分流**，不能恒选下标最小那个。
    ///
    /// 旧代码 `min_by_key(|e| e.credentials.priority)` 遇平局取第一个遇到的元素，而 `available`
    /// 的顺序 = 凭据入池顺序，于是同 priority 的号里**恒定选中最早创建的那个**，与运行时负载无关。
    /// 实测后果：5 号池 priority 全为 0 时，某号 rpm=23 而另一号 rpm=1（负载差 23 倍）。
    #[tokio::test]
    async fn test_priority_mode_same_priority_spreads_load() {
        let mut config = Config::default();
        config.load_balancing_mode = "priority".to_string();
        config.affinity_enabled = false;
        config.credential_rpm_limit = 1000; // 不触发饱和，纯看负载分流

        // 三个号 priority 全为 0（平局）——正是实机现场的配置
        let creds: Vec<KiroCredentials> = (0..3)
            .map(|i| {
                let mut c = KiroCredentials::default();
                c.priority = 0;
                c.access_token = Some(format!("tok{i}"));
                c.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());
                c
            })
            .collect();

        let manager = MultiTokenManager::new(config, creds, None, None, false).unwrap();

        // 连续取 6 次上下文，**持有不释放**（inflight 累积），观察是否分摊。
        let mut ctxs = Vec::new();
        for _ in 0..6 {
            ctxs.push(manager.acquire_context(None, None).await.unwrap());
        }
        let mut hits = std::collections::HashMap::new();
        for c in &ctxs {
            *hits.entry(c.id).or_insert(0u32) += 1;
        }
        assert!(
            hits.len() >= 2,
            "同优先级多号必须分流到至少 2 个号，实际只用了 {:?}（旧代码恒选下标最小的那一个）",
            hits
        );
        // 排序键第④位是 inflight 最少优先 → 3 个号 6 次应各 2 次，最热号不该独吞过半。
        let max_hit = hits.values().copied().max().unwrap();
        assert!(
            max_hit <= 3,
            "最热号占了 {max_hit}/6 次，分流失效（in-flight 维度未生效）：{hits:?}"
        );
    }

    /// ⭐回归:balanced 模式 + 池号数 >1 时硬门才真正生效(与 test_rpm_saturation_deprioritizes_credential
    /// 描述的调度行为一致)。
    #[test]
    fn test_rpm_saturation_gate_active_in_balanced_multi_credential() {
        let mut config = Config::default();
        config.load_balancing_mode = "balanced".to_string();

        let mut c1 = KiroCredentials::default();
        c1.access_token = Some("tok1".to_string());
        c1.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());
        let mut c2 = KiroCredentials::default();
        c2.access_token = Some("tok2".to_string());
        c2.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());

        let manager = MultiTokenManager::new(config, vec![c1, c2], None, None, false).unwrap();
        assert!(
            manager.rpm_saturation_gate_active(),
            "balanced 模式 + 池号数>1 应报硬门生效"
        );
    }

    /// ⭐回归:即便是 balanced 模式，只要池里只有 1 个号，"分流"这个概念本身就不适用
    /// (无处可分)，硬门也不该报生效——否则 UI 会显示"建议分流"却根本没有第二个号。
    #[test]
    fn test_rpm_saturation_gate_inactive_with_single_credential() {
        let mut config = Config::default();
        config.load_balancing_mode = "balanced".to_string(); // 即便开了 balanced

        let mut c1 = KiroCredentials::default();
        c1.access_token = Some("tok1".to_string());
        c1.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());

        let manager = MultiTokenManager::new(config, vec![c1], None, None, false).unwrap();
        assert!(
            !manager.rpm_saturation_gate_active(),
            "单号池下无分流对象，硬门不应报生效，即便是 balanced 模式"
        );
    }

    /// ⭐ 归一化回归（**旧代码必失败**）：priority 模式下硬门同样生效，与 balanced 无差别。
    ///
    /// 本测试的前身断言相反（"priority 模式下硬门不生效"）——那记录的是归一化前的事实：
    /// 裸 priority 分支不读饱和，所以那个阈值确实从未拦过请求（面板据此报"已达软上限"即虚假告警）。
    /// 归一化后两种模式共用同一套排序键，饱和硬门真实生效，面板报告与调度重新对齐。
    #[test]
    fn test_rpm_saturation_gate_active_in_priority_mode_after_normalization() {
        let mut config = Config::default();
        config.load_balancing_mode = "priority".to_string();

        let mut c1 = KiroCredentials::default();
        c1.access_token = Some("tok1".to_string());
        c1.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());
        let mut c2 = KiroCredentials::default();
        c2.access_token = Some("tok2".to_string());
        c2.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());

        let manager = MultiTokenManager::new(config, vec![c1, c2], None, None, false).unwrap();
        assert!(
            manager.rpm_saturation_gate_active(),
            "归一化后 priority 模式也走排序键，饱和硬门必须生效（旧代码此处为 false）"
        );
    }

    /// ⭐ 零回归：单号池下无论何种模式，硬门都不该报生效（无分流对象，"饱和"概念不适用）。
    /// 这条不受归一化影响，用于确认归一化没把单号池的特例判断一起吃掉。
    #[test]
    fn test_rpm_saturation_gate_still_inactive_for_single_credential_in_priority() {
        let mut config = Config::default();
        config.load_balancing_mode = "priority".to_string();

        let mut c1 = KiroCredentials::default();
        c1.access_token = Some("tok1".to_string());
        c1.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());

        let manager = MultiTokenManager::new(config, vec![c1], None, None, false).unwrap();
        assert!(
            !manager.rpm_saturation_gate_active(),
            "单号池无分流对象，硬门恒不生效（归一化不改变这条特例）"
        );
    }

    // ============ L3 headroom 折扣(旧代码上会失败:旧代码无折扣恒等于 base)============

    #[test]
    fn test_rpm_headroom_discount() {
        let config = Config::default();
        let manager = MultiTokenManager::new(config, vec![], None, None, false).unwrap();
        // 默认 factor=85:base 30 → 25;base 100 → 85。
        assert_eq!(manager.effective_saturation_limit(Some(30)), 25, "30×0.85=25");
        assert_eq!(manager.effective_saturation_limit(Some(100)), 85, "100×0.85=85");
        // factor=100(不打折):= base。
        manager.rpm_headroom_factor.store(100, Ordering::Relaxed);
        assert_eq!(manager.effective_saturation_limit(Some(30)), 30, "factor=100 不打折");
        // factor=0 视为不打折(防误配把号打成恒饱和)。
        manager.rpm_headroom_factor.store(0, Ordering::Relaxed);
        assert_eq!(manager.effective_saturation_limit(Some(30)), 30, "factor=0 视为不打折");
        // reserve_slots 叠加:base 30 × 0.85=25,再减 3 = 22。
        manager.rpm_headroom_factor.store(85, Ordering::Relaxed);
        manager.rpm_reserve_slots.store(3, Ordering::Relaxed);
        assert_eq!(manager.effective_saturation_limit(Some(30)), 22, "25-3=22");
        // 边界:base 1 × 0.85 = floor 0 → max(1)=1(绝不 0,否则恒饱和)。
        manager.rpm_reserve_slots.store(0, Ordering::Relaxed);
        assert_eq!(manager.effective_saturation_limit(Some(1)), 1, "base 1 折后下限 1,不得 0");
    }

    #[test]
    fn test_rpm_headroom_preserves_percred_priority() {
        // per-cred rpm_limit(>0) 优先级不被 headroom 破坏:#1 设 100、#2 未设(用全局/兜底)。
        let mut config = Config::default();
        config.credential_rpm_limit = 0; // 全局未设 → #2 走兜底 30
        config.rpm_headroom_factor = 100; // 隔离 headroom 变量,只验优先级选取
        let manager = MultiTokenManager::new(config, vec![], None, None, false).unwrap();
        assert_eq!(manager.effective_saturation_limit(Some(100)), 100, "per-cred 100 优先");
        assert_eq!(manager.effective_saturation_limit(None), 30, "未设 → 兜底 30");
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

    /// 回归（死号自动禁用 · 本轮核心）：连续账户级风控且期间零成功 → 自动禁用。
    ///
    /// **旧代码为何失败**：判据是 `cooldown.trigger_count(id) >= 10`，而
    /// `cooldown_enabled=false` 时 `set_cooldown` 根本不被调用 → `trigger_count`
    /// 恒 0 → 阈值永不可达。且整个自动禁用块被 `if cooldown_enabled` 包住，
    /// 该开关关闭时连计数都不发生。线上实测正是这个组合：8 个成功率恒 0% 的死号
    /// 跑几小时仍全部 `disabled=false`，每个请求都在它们身上白撞。
    #[test]
    fn test_suspicious_auto_disable_works_with_cooldown_disabled() {
        let mut config = Config::default();
        // 关键：冷却关闭。自动禁用**不得**依赖这个不相关的开关。
        config.cooldown_enabled = false;
        let manager = MultiTokenManager::new(
            config,
            vec![KiroCredentials::default(), KiroCredentials::default()],
            None,
            None,
            false,
        )
        .unwrap();

        // 阈值前一次：仍在池内（临时风控不能一见 403 就禁，会误伤健康号）
        for _ in 0..(MAX_CONSECUTIVE_SUSPICIOUS_BEFORE_DISABLE - 1) {
            manager.report_suspicious_activity(1);
        }
        assert_eq!(
            manager.available_count(),
            2,
            "未达阈值前不得禁用（403 是临时态，历史上误判成永久封禁造成过生产事故）"
        );

        // 达阈值：自动禁用，移出调度
        manager.report_suspicious_activity(1);
        assert_eq!(
            manager.available_count(),
            1,
            "连续 {} 次风控且零成功应自动禁用（旧代码 trigger_count 恒 0，永不触发）",
            MAX_CONSECUTIVE_SUSPICIOUS_BEFORE_DISABLE
        );
    }

    /// 回归（健康号永不误禁）：一次成功即清零连续风控计数。
    ///
    /// **旧代码为何失败**：计数依赖 `cooldown.trigger_count`，而 `report_success`
    /// 调 `clear_cooldown` → `entries.remove()` 删掉整个冷却条目 → 计数归零。
    /// 表面上"成功会清零"是对的，但副作用是**半死号也永远回不到阈值**，
    /// 于是自动禁用整体失效。新实现把计数挂在凭据条目上，与冷却条目解耦：
    /// 成功仍然清零（本测试），但失败能持续累加（上一个测试）。
    #[test]
    fn test_success_resets_suspicious_counter_so_healthy_cred_never_disabled() {
        let config = Config::default();
        let manager =
            MultiTokenManager::new(config, vec![KiroCredentials::default()], None, None, false)
                .unwrap();

        // 模拟实测中的健康号：偶发 403，但成功率 90~100%。
        // 循环远超阈值；若成功没能清零，早就被误禁了。
        for _ in 0..(MAX_CONSECUTIVE_SUSPICIOUS_BEFORE_DISABLE * 5) {
            for _ in 0..(MAX_CONSECUTIVE_SUSPICIOUS_BEFORE_DISABLE - 1) {
                manager.report_suspicious_activity(1);
            }
            manager.report_success(1); // 一次成功即清零
        }
        assert_eq!(
            manager.available_count(),
            1,
            "健康号（偶发风控但持续有成功）绝不能被自动禁用——这正是 403 不可按\
             『见过即封』处理的原因"
        );
    }

    /// 回归（**回滚安全**，发布前 review 抓到的 BLOCKER）：
    /// 未知的 `disabledReason` 必须退化成 `Unknown`，**绝不能让整个凭据文件解析失败**。
    ///
    /// **为什么这是 BLOCKER**：`DisabledReason` 每次新增变体（本轮加了
    /// `PassthroughFailed` / `PassthroughOverloaded`），新版本就会把它写进
    /// `credentials.json`。而**旧版本**读到未知变体时 serde 报
    /// `unknown variant 'passthroughFailed', expected one of ...` →
    /// `CredentialsConfig::load` 返 Err → `main.rs` 直接 `std::process::exit(1)`
    /// （刻意的 fail-safe：宁可拒绝启动也不用空池覆盖真实凭据）。
    ///
    /// 于是 **回滚到旧二进制 = 服务起不来**，且必须在生产压力下手工编辑 JSON 才能恢复。
    /// 这条实测验证过：去掉 `#[serde(other)]` 后本测试 FAIL 并给出上述 serde 报错。
    ///
    /// ⚠️ 本测试同时守着「未知变体不得让**其它字段**丢失」——
    /// 退化成 Unknown 后 disabled 等字段必须照常读出来。
    #[test]
    fn test_unknown_disabled_reason_degrades_instead_of_failing_load() {
        // 模拟"未来版本"写出的凭据文件（含本版本不认识的 reason）
        let json = r#"[{
            "authMethod": "api_key",
            "kiroApiKey": "ksk_future",
            "disabled": true,
            "disabledReason": "someFutureReasonFromNewerVersion",
            "disabledAt": "2026-07-31T00:00:00+00:00"
        }]"#;
        let creds: Vec<KiroCredentials> =
            serde_json::from_str(json).expect("未知 reason 绝不能让整个凭据文件解析失败（回滚会因此起不来）");
        assert_eq!(creds.len(), 1);
        assert_eq!(
            creds[0].disabled_reason,
            Some(DisabledReason::Unknown),
            "未知变体应退化成 Unknown"
        );
        // 关键：退化不得连带丢掉其它字段
        assert!(creds[0].disabled, "disabled 必须照常读出（退化不应影响其它字段）");
        assert!(creds[0].disabled_at.is_some(), "disabled_at 必须照常读出");
        assert_eq!(creds[0].kiro_api_key.as_deref(), Some("ksk_future"));
    }

    /// 回归（回滚安全 · 对照组）：**已知**变体必须精确解析，不得被 `Unknown` 吞掉。
    ///
    /// 防止"修过头"——`#[serde(other)]` 若误配成捕获全部，所有原因都会变成 Unknown，
    /// 那等于把「标明封号原因」这个需求整体废掉。
    #[test]
    fn test_known_disabled_reasons_still_parse_exactly() {
        for (wire, expect) in [
            ("manual", DisabledReason::Manual),
            ("quotaExceeded", DisabledReason::QuotaExceeded),
            ("passthroughFailed", DisabledReason::PassthroughFailed),
            ("passthroughOverloaded", DisabledReason::PassthroughOverloaded),
        ] {
            let got: DisabledReason =
                serde_json::from_str(&format!("\"{wire}\"")).unwrap_or_else(|e| panic!("{wire} 应可解析: {e}"));
            assert_eq!(got, expect, "{wire} 被解析错了（serde(other) 不应吞掉已知变体）");
        }
    }

    /// 回归（E3 · 复活必须清全部惩罚计数）：`reset_and_enable` 后再来一次风控不得秒禁。
    ///
    /// **旧代码为何失败**：`reset_and_enable` 只清 `failure_count` /
    /// `refresh_failure_count` / `request_count`，**漏了 `consecutive_suspicious`**
    /// （该字段唯一清零点是 `report_success`）。于是被 `SuspiciousActivityAuto` 禁用的号
    /// （计数已达阈值）人工「重置并启用」后计数仍在阈值上，**下一次风控即再次禁用**，
    /// 「重置」形同虚设。且自动禁用落盘后该秒禁重启也回不来。
    #[test]
    fn test_reset_and_enable_clears_suspicious_counter_so_revived_cred_survives_one_hit() {
        let config = Config::default();
        let manager = MultiTokenManager::new(
            config,
            vec![KiroCredentials::default(), KiroCredentials::default()],
            None,
            None,
            false,
        )
        .unwrap();

        // 打到阈值 → 自动禁用
        for _ in 0..MAX_CONSECUTIVE_SUSPICIOUS_BEFORE_DISABLE {
            manager.report_suspicious_activity(1);
        }
        assert_eq!(manager.available_count(), 1, "达阈值应已自动禁用");

        // 人工复活
        manager.reset_and_enable(1).unwrap();
        assert_eq!(manager.available_count(), 2, "重置并启用后应回池");

        // 复活后再来一次风控：绝不能立即再禁（计数应已归零，距阈值还差 N-1 次）
        manager.report_suspicious_activity(1);
        assert_eq!(
            manager.available_count(),
            2,
            "复活后一次风控就秒禁 = consecutive_suspicious 没被清零（旧代码即如此）"
        );
    }

    /// 回归（E3 · 同上，覆盖 `set_disabled(false)` 这条复活路径）。
    ///
    /// 三条复活路径此前各自手写清零列表且都漏同一个字段，故三条都要有守卫。
    #[test]
    fn test_set_disabled_false_clears_suspicious_counter() {
        let config = Config::default();
        let manager = MultiTokenManager::new(
            config,
            vec![KiroCredentials::default(), KiroCredentials::default()],
            None,
            None,
            false,
        )
        .unwrap();

        for _ in 0..MAX_CONSECUTIVE_SUSPICIOUS_BEFORE_DISABLE {
            manager.report_suspicious_activity(1);
        }
        assert_eq!(manager.available_count(), 1, "达阈值应已自动禁用");

        // 走 Admin 的启用开关复活
        manager.set_disabled(1, false).unwrap();
        assert_eq!(manager.available_count(), 2, "启用后应回池");

        manager.report_suspicious_activity(1);
        assert_eq!(
            manager.available_count(),
            2,
            "set_disabled(false) 同样必须清 consecutive_suspicious"
        );
    }

    /// 回归（🔴 会让整池永久死锁）：全池被 403 风控打成 `SuspiciousActivityAuto` 后必须能自愈。
    ///
    /// **旧代码为何 FAIL**：自愈的判定条件只匹配 `TooManyFailures`，
    /// 而 403 风控走的是 `SuspiciousActivityAuto`。于是一次 IP 级风控把全池打死后
    /// **没有任何自动恢复路径** —— 只能人工介入或重启。
    ///
    /// 而 403 `TEMPORARILY_SUSPENDED` 是**临时态**（代码注释与历史事故都明确记录：
    /// 曾被当永久封禁处理 → 12h 内 88 次误禁 + 36 次全池活锁 → 逐小时拒绝率升到 100%）。
    ///
    /// 线上实测（48h）：`判定为死号并自动禁用` **46 次**，而 `执行自愈` **0 次**
    /// —— 坐实自愈对这个原因从未生效过。
    #[tokio::test]
    async fn test_pool_wide_suspicious_auto_disable_can_self_heal() {
        let config = Config::default();
        let manager = MultiTokenManager::new(
            config,
            vec![KiroCredentials::default()],
            None,
            None,
            false,
        )
        .unwrap();

        // 打到阈值 → 以 SuspiciousActivityAuto 自动禁用（全池只有这一个号）
        for _ in 0..MAX_CONSECUTIVE_SUSPICIOUS_BEFORE_DISABLE {
            manager.report_suspicious_activity(1);
        }
        assert_eq!(manager.available_count(), 0, "前提：全池应已被自动禁用");

        // 自愈：acquire_context 应把它复活并重试。
        //
        // ⚠️ 断言口径很关键：**不能**在调用后查 `available_count()`。
        // 测试凭据是假的（没有真 token），复活后必然刷新失败并被重新禁用为
        // `TooManyRefreshFailures` —— 那是**预期行为**，不是自愈没生效。
        // 所以判据取"最终禁用原因已不再是 SuspiciousActivityAuto"：
        // 说明它确实被复活过并走到了刷新阶段（旧代码下会原地卡在 SuspiciousActivityAuto）。
        let _ = manager.acquire_context(None, None).await;
        let snap = manager.snapshot();
        let entry = snap.entries.iter().find(|e| e.id == 1).expect("号应仍在池中");
        assert_ne!(
            entry.disabled_reason.as_deref(),
            Some("SuspiciousActivityAuto"),
            "全池被 403 风控打死后必须能自愈并重新参与调度（旧代码只认 TooManyFailures → \
             原地死锁在 SuspiciousActivityAuto，永无恢复路径）。实际原因: {:?}",
            entry.disabled_reason
        );
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

    /// 回归：删号进回收站必须保留禁用状态三元组，恢复时不得把真实原因抹成 `Manual`。
    ///
    /// **旧代码为何 FAIL**：`disabled` / `disabled_reason` / `disabled_at` 的权威副本在
    /// `CredentialEntry` 上，`KiroCredentials` 里那份只在 `persist_credentials()` 落盘时同步。
    /// 而 `delete_credential` 直接把 `removed.credentials` 塞进 `TrashEntry`，**绕过那次同步**
    /// → 回收站条目恒为 `(false, None, None)`，第一个断言即 FAIL。
    ///
    /// 实测：线上 07-30 之后删的 31 个号（reason 持久化已上线）在 trash.json 里三字段全空，
    /// 175 条回收站记录无一条带原因。用户明确要求过「认定封号必须标明原因」，而这恰在最需要
    /// 该信息的时刻（判断换号还是申诉）丢失；且 `restore_credential` 因读不到原因只能一律落
    /// `Manual`，是批 2 修掉的「自动原因变手动」在回收站路径上的同型漏修。
    #[test]
    fn test_trash_preserves_disable_reason_and_restore_does_not_downgrade_to_manual() {
        let config = Config::default();
        let mut c1 = KiroCredentials::default();
        c1.refresh_token = Some("refresh-trash-reason".to_string());
        let manager = MultiTokenManager::new(config, vec![c1], None, None, false).unwrap();

        // 自动禁用（额度耗尽），而非手动禁用 —— 这是原因会不会被抹掉的关键区别。
        manager.report_quota_exhausted(1);
        manager.delete_credential(1).unwrap();

        // ① 回收站快照必须带上真实原因与时刻
        let trash = manager.list_trash();
        assert_eq!(trash.len(), 1);
        assert_eq!(
            trash[0].disabled_reason,
            Some(DisabledReason::QuotaExceeded),
            "回收站丢了禁用原因（旧代码即如此：绕过 persist_credentials 的同步块）"
        );
        assert!(
            trash[0].disabled_at.is_some(),
            "回收站丢了禁用时刻，无法区分「刚坏就删」与「坏了很久」"
        );

        // ② 恢复后原因不得被降级成 Manual（仍保持 disabled，交由 Admin 手动启用）
        manager.restore_credential(1).unwrap();
        let snap = manager.snapshot();
        let e = snap.entries.iter().find(|e| e.id == 1).expect("应已回池");
        assert!(e.disabled, "恢复后应仍为禁用态，不自动回池");
        // 注意：面板快照里 disabled_reason 是字符串（前端 i18n 用），非枚举。
        assert_eq!(
            e.disabled_reason.as_deref(),
            Some("QuotaExceeded"),
            "恢复把自动禁用原因抹成了 Manual —— 运维据此无法判断该不该启用"
        );
    }

    /// 回归（用户要求的「强制删除」）：`force=true` 可直接删掉**未禁用**的号，且仍进回收站。
    ///
    /// **旧代码为何 FAIL**：`delete_credential` 无条件 bail
    /// `只能删除已禁用的凭据（请先禁用凭据 #N）`，删一个号必须两次调用（禁用 + 删除），
    /// 批量删 N 个 = **2N 次往返**；而"号卡住了要拔掉"正是强制删除的核心动机。
    ///
    /// 同时锁住**不能修过头**：force 只绕"必须先禁用"这道门，**不跳过回收站** ——
    /// adminKey 明文存 localStorage 且全仓无 CSP，一旦 XSS 就能整池清空，
    /// 回收站是被打穿后唯一的兜底。物理删除仍走 `purge_credential`。
    #[test]
    fn test_force_delete_bypasses_disable_gate_but_still_goes_to_trash() {
        let config = Config::default();
        let mut c1 = KiroCredentials::default();
        c1.refresh_token = Some("refresh-force".to_string());
        let manager = MultiTokenManager::new(config, vec![c1], None, None, false).unwrap();

        // 前提：号是**启用**状态 —— 非 force 路径会被拒绝。
        assert_eq!(manager.available_count(), 1);
        let err = manager.delete_credential(1).unwrap_err().to_string();
        assert!(
            err.contains("只能删除已禁用的凭据"),
            "非 force 必须保持原有保守语义（旧客户端不发 force 时行为不变）: {err}"
        );

        // force：直接删掉，无需先禁用。
        manager
            .delete_credential_forced(1, true)
            .expect("force 应能删除未禁用的号");
        assert_eq!(manager.total_count(), 0, "号应已从调度池移出");

        // 仍在回收站里 → 可恢复（force 不等于 purge）。
        let trash = manager.list_trash();
        assert_eq!(trash.len(), 1, "force 删除仍须进回收站（XSS 兜底不能被绕过）");
        assert_eq!(trash[0].id, 1);
        manager.restore_credential(1).expect("回收站里的号应可恢复");
        assert_eq!(manager.total_count(), 1, "恢复后应回到凭据池");
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

    /// 上游 5xx 必须给该号设冷却，否则失败的号下一轮立刻又被选中。
    ///
    /// 回归背景：`CooldownReason::ServerError`（30s，自动恢复）早已定义，但在**生产路径上
    /// 从未被设置过** —— 唯一调用方是 admin 的手工冷却接口。于是 500 风暴时（实测一小时
    /// 408 次 500）请求只 sleep 200ms~2s 就换号，在同一批坏号之间来回打。
    #[test]
    fn should_set_cooldown_on_upstream_server_error() {
        let config = Config::default();
        let mut c1 = KiroCredentials::default();
        c1.refresh_token = Some("refresh-1".to_string());
        let manager = MultiTokenManager::new(config, vec![c1], None, None, false).unwrap();

        assert!(
            !manager
                .cooldown_snapshot()
                .iter()
                .any(|i| i.credential_id == 1),
            "初始不应有冷却"
        );

        manager.report_server_error(1);

        let info = manager
            .cooldown_snapshot()
            .into_iter()
            .find(|i| i.credential_id == 1)
            .expect("5xx 后该号必须处于冷却中（此前完全不设冷却）");
        assert_eq!(
            info.reason,
            crate::kiro::cooldown::CooldownReason::ServerError,
            "应使用早已定义却从未在生产路径被用过的 ServerError 原因"
        );
        assert!(
            info.reason.is_auto_recoverable(),
            "5xx 是上游整体故障，必须能自动恢复，不可要求人工介入"
        );
    }

    /// `cooldownEnabled=false` 时 5xx 不设冷却（尊重全局开关，不绕过门禁）。
    #[test]
    fn should_skip_server_error_cooldown_when_cooldown_disabled() {
        let mut config = Config::default();
        config.cooldown_enabled = false;
        let mut c1 = KiroCredentials::default();
        c1.refresh_token = Some("refresh-1".to_string());
        let manager = MultiTokenManager::new(config, vec![c1], None, None, false).unwrap();

        manager.report_server_error(1);
        assert!(
            !manager
                .cooldown_snapshot()
                .iter()
                .any(|i| i.credential_id == 1),
            "冷却总开关关闭时不应设冷却"
        );
    }

    /// 重试预算必须只数 Kiro 路径**真正可选**的号：排除 disabled 与 custom_api。
    ///
    /// 回归背景：预算此前按 `total_count()`（= entries.len()）× 3 计算，把永不可选的
    /// custom_api 与已禁用号也算进去，凭空抬高预算 —— 生产日志 `尝试 8/36` 即由此而来。
    #[test]
    fn should_count_only_kiro_selectable_credentials_for_retry_budget() {
        let config = Config::default();
        let mut kiro1 = KiroCredentials::default();
        kiro1.refresh_token = Some("refresh-1".to_string());
        let mut kiro2 = KiroCredentials::default();
        kiro2.refresh_token = Some("refresh-2".to_string());
        // custom_api 代挂号：is_entry_selectable 永远拒绝它走 Kiro 路径
        let mut custom = KiroCredentials::default();
        custom.auth_method = Some("custom_api".to_string());
        custom.base_url = Some("https://relay.invalid".to_string());
        custom.api_key = Some("sk-x".to_string());

        let manager =
            MultiTokenManager::new(config, vec![kiro1, kiro2, custom], None, None, false).unwrap();

        assert_eq!(manager.total_count(), 3, "entries 总数含 custom_api");
        assert_eq!(
            manager.kiro_selectable_count(),
            2,
            "Kiro 可选数必须排除 custom_api"
        );

        // 禁用其中一个 Kiro 号 → 可选数再降
        manager.set_disabled(1, true).unwrap();
        assert_eq!(
            manager.kiro_selectable_count(),
            1,
            "disabled 号不可选，必须从预算基数里剔除"
        );
        assert_eq!(manager.total_count(), 3, "total_count 仍含全部条目（对照）");
    }

    /// 面板「全部清空」必须能清掉**刚删除**的条目。
    ///
    /// 这是历史缺陷的回归守卫：按天数的接口无法表达「立即全清」——传 0 被解释成
    /// 永久保留（返回 0），传 N 又清不掉 N 天内新删的条目。于是面板点清理时 67 条
    /// 刚删的凭据一条都清不掉，只提示「共移除 0 项」。
    #[test]
    fn should_purge_all_trash_including_freshly_deleted_entries() {
        let config = Config::default();
        let mut c1 = KiroCredentials::default();
        c1.refresh_token = Some("refresh-1".to_string());
        let mut c2 = KiroCredentials::default();
        c2.refresh_token = Some("refresh-2".to_string());

        let manager = MultiTokenManager::new(config, vec![c1, c2], None, None, false).unwrap();
        for id in [1, 2] {
            manager.set_disabled(id, true).unwrap();
            manager.delete_credential(id).unwrap();
        }
        assert_eq!(manager.list_trash().len(), 2, "两条都应在回收站");

        // 按天数的两条路径都清不掉刚删的条目（这正是缺陷本身）
        assert_eq!(
            manager.purge_expired_trash(0),
            0,
            "0 被解释为永久保留，清不掉"
        );
        assert_eq!(
            manager.purge_expired_trash(30),
            0,
            "刚删除的条目未超过 30 天，清不掉"
        );
        assert_eq!(manager.list_trash().len(), 2, "此时回收站仍有 2 条");

        // 全清入口必须真的清空
        assert_eq!(manager.purge_all_trash(), 2, "全清应返回被清条目数");
        assert_eq!(manager.list_trash().len(), 0, "回收站应为空");
    }

    /// 空回收站上全清应返回 0 且不报错（幂等，前端可重复点）。
    #[test]
    fn should_return_zero_when_purging_empty_trash() {
        let config = Config::default();
        let mut c1 = KiroCredentials::default();
        c1.refresh_token = Some("refresh-1".to_string());
        let manager = MultiTokenManager::new(config, vec![c1], None, None, false).unwrap();
        assert_eq!(manager.purge_all_trash(), 0);
        assert_eq!(manager.purge_all_trash(), 0, "重复调用仍为 0，不 panic");
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

    #[test]
    fn test_custom_api_request_count_and_disabled_persist_across_restart() {
        // 回归(对抗 review #3):custom_api 的 request_count 是「终身预算」计数,达上限后
        // request_count + disabled 都必须跨重启持久化——否则重启即额度归零、被禁号重新可用,防超预算失效。
        use crate::usage::RequestOutcome;

        let dir = std::env::temp_dir().join(format!("kiro-reqcount-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let cred_path = dir.join("credentials.json");
        std::fs::write(
            &cred_path,
            r#"[{"id":1,"authMethod":"custom_api","baseUrl":"https://up.example.invalid","requestLimit":2}]"#,
        )
        .unwrap();

        let cred = {
            let mut c = KiroCredentials::default();
            c.id = Some(1);
            c.auth_method = Some("custom_api".to_string());
            c.base_url = Some("https://up.example.invalid".to_string());
            c.request_limit = Some(2);
            c
        };

        let manager = MultiTokenManager::new(
            Config::default(),
            vec![cred],
            None,
            Some(cred_path.clone()),
            true,
        )
        .unwrap();

        // 两次成功透传 → 命中 request_limit=2 → 自动禁用 + 立即落盘。
        manager.record_passthrough_result(1, RequestOutcome::Success);
        manager.record_passthrough_result(1, RequestOutcome::Success);

        // stats 文件应已写入 request_count(命中上限走立即 save_stats,非 debounced)。
        let stats_file = dir.join("kiro_stats.json");
        assert!(stats_file.exists(), "kiro_stats.json 应已落盘");
        let stats_json = std::fs::read_to_string(&stats_file).unwrap();
        assert!(
            stats_json.contains("\"request_count\""),
            "stats 文件应含 request_count 字段"
        );

        // 用同一目录重建 manager,模拟进程重启:reload_creds 从 credentials.json 读回(含 disabled),
        // load_stats 从 kiro_stats.json 读回 request_count。
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

        let snap = manager2
            .snapshot()
            .entries
            .into_iter()
            .find(|c| c.id == 1)
            .expect("id=1 应仍在池中");
        assert_eq!(snap.request_count, 2, "request_count 应跨重启保留为 2,不回退归零");
        assert!(snap.disabled, "达上限的禁用状态应跨重启保留");

        std::fs::remove_dir_all(&dir).ok();
    }

    // ============ 服务端防呆:setter 越界自动修补(不信任前端校验)============

    /// 优先级超上界 → clamp 到 MAX_PRIORITY(直打 admin API 的极值不污染排序)。
    #[test]
    fn test_set_priority_clamps_to_max() {
        let config = Config::default();
        let mut c = KiroCredentials::default();
        c.refresh_token = Some("r-clamp-prio".to_string());
        let mgr = MultiTokenManager::new(config, vec![c], None, None, false).unwrap();

        mgr.set_priority(1, u32::MAX).unwrap();
        let snap = mgr.snapshot().entries.into_iter().find(|e| e.id == 1).unwrap();
        assert_eq!(snap.priority, MAX_PRIORITY, "越界优先级应 clamp 到上界");

        // 界内值不动。
        mgr.set_priority(1, 5).unwrap();
        let snap2 = mgr.snapshot().entries.into_iter().find(|e| e.id == 1).unwrap();
        assert_eq!(snap2.priority, 5);
    }

    /// RPM 上限:0→None(继承全局),极值→clamp 到 MAX_RPM_LIMIT。
    #[test]
    fn test_set_rpm_limit_normalizes_and_clamps() {
        let config = Config::default();
        let mut c = KiroCredentials::default();
        c.refresh_token = Some("r-clamp-rpm".to_string());
        let mgr = MultiTokenManager::new(config, vec![c], None, None, false).unwrap();

        // 0 → None
        mgr.set_rpm_limit(1, Some(0)).unwrap();
        let snap = mgr.snapshot().entries.into_iter().find(|e| e.id == 1).unwrap();
        assert_eq!(snap.rpm_limit, None, "0 应归一为 None(继承全局)");

        // 极值 → clamp 到上界
        mgr.set_rpm_limit(1, Some(u32::MAX)).unwrap();
        let snap2 = mgr.snapshot().entries.into_iter().find(|e| e.id == 1).unwrap();
        assert_eq!(snap2.rpm_limit, Some(MAX_RPM_LIMIT), "越界 RPM 应 clamp 到上界");

        // 界内值不动
        mgr.set_rpm_limit(1, Some(60)).unwrap();
        let snap3 = mgr.snapshot().entries.into_iter().find(|e| e.id == 1).unwrap();
        assert_eq!(snap3.rpm_limit, Some(60));
    }

    /// 别名超长 → 按字符截断到 MAX_NAME_CHARS(多字节安全,不切坏 UTF-8);空白清除。
    #[test]
    fn test_set_credential_name_truncates_and_trims() {
        let config = Config::default();
        let mut c = KiroCredentials::default();
        c.refresh_token = Some("r-clamp-name".to_string());
        let mgr = MultiTokenManager::new(config, vec![c], None, None, false).unwrap();

        // 超长中文(每字符多字节)→ 截断到 MAX_NAME_CHARS 个 char,不 panic 不切坏。
        let long = "中".repeat(100);
        mgr.set_credential_name(1, Some(long)).unwrap();
        let snap = mgr.snapshot().entries.into_iter().find(|e| e.id == 1).unwrap();
        let name = snap.name.expect("应有别名");
        assert_eq!(name.chars().count(), MAX_NAME_CHARS, "超长别名应截断到上界字符数");

        // 纯空白 → 清除。
        mgr.set_credential_name(1, Some("   ".to_string())).unwrap();
        let snap2 = mgr.snapshot().entries.into_iter().find(|e| e.id == 1).unwrap();
        assert_eq!(snap2.name, None, "纯空白别名应清除");
    }

    // ============ 熔断/健康快照暴露(Phase 2:此前后端算好但无出口)============

    /// 从未被选过的号无健康记录 → 不在 health_snapshots 表中(调用方按缺省满血处理)。
    /// 连续 429 跳闸后 → 表中出现该号且 circuit_open=true、健康分被拉低。
    #[test]
    fn test_health_snapshots_reflects_circuit_state() {
        let mut config = Config::default();
        config.cooldown_enabled = true;
        let mut c = KiroCredentials::default();
        c.refresh_token = Some("r-health-snap".to_string());
        let mgr = MultiTokenManager::new(config, vec![c], None, None, false).unwrap();

        // 初始:无健康记录(从未 429/成功过)→ 不在表中。
        assert!(mgr.health_snapshots().get(&1).is_none(), "初始无健康记录");

        // 连续裸 429 跳闸(TRIP_THRESHOLD 次以上),触发熔断 Open。
        for _ in 0..5 {
            mgr.report_rate_limited_with_retry_after(1, None);
        }
        let snaps = mgr.health_snapshots();
        let h = snaps.get(&1).expect("跳闸后应有健康记录");
        assert!(h.circuit_open, "连续 429 应跳闸 circuit Open");
        assert!(h.health < 1.0, "健康分应被 429 拉低");
        assert!(h.consecutive_429 >= 5, "连续 429 计数应累加");
    }

    // ============ at-rest 加密:落盘密文 + 重载解密全链路 ============

    /// 开启加密后:credentials.json 落盘是密文(带 magic、不含明文 token);重载能透明解密还原。
    #[test]
    fn test_at_rest_encryption_roundtrip_on_disk() {
        let dir = std::env::temp_dir().join(format!("kiro-enc-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let cred_path = dir.join("credentials.json");

        let mut config = Config::default();
        config.encrypt_credentials_at_rest = true;

        let mut c = KiroCredentials::default();
        c.refresh_token = Some("super-secret-refresh-token-xyz".to_string());
        let mgr = MultiTokenManager::new(config, vec![c], None, Some(cred_path.clone()), true).unwrap();

        // 触发一次 persist(改个字段即回写)。
        mgr.set_priority(1, 3).unwrap();

        // 磁盘上应是密文:带 magic、绝不含明文 refresh_token。
        let raw = std::fs::read(&cred_path).unwrap();
        assert!(crate::common::secret_store::is_encrypted(&raw), "开启加密后落盘应为密文");
        let raw_str = String::from_utf8_lossy(&raw);
        assert!(!raw_str.contains("super-secret-refresh-token-xyz"), "密文不应含明文 token");

        // 重载能解密还原(透明迁移的反向:密文→明文→解析)。
        let reloaded = crate::kiro::model::credentials::CredentialsConfig::load(&cred_path)
            .unwrap()
            .into_sorted_credentials();
        assert_eq!(reloaded.len(), 1);
        assert_eq!(reloaded[0].refresh_token.as_deref(), Some("super-secret-refresh-token-xyz"));
        assert_eq!(reloaded[0].priority, 3);

        std::fs::remove_dir_all(&dir).ok();
    }

    /// 迁移兼容:关闭加密(默认)时落盘仍是明文;已有明文文件能被读(直通,不因加密崩)。
    #[test]
    fn test_at_rest_disabled_stays_plaintext_and_reads_legacy() {
        let dir = std::env::temp_dir().join(format!("kiro-enc-off-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let cred_path = dir.join("credentials.json");
        // 预置一个老的明文 credentials.json(模拟升级前用户)。
        std::fs::write(&cred_path, r#"[{"id":1,"refreshToken":"legacy-plain-token"}]"#).unwrap();

        // 明文照旧能读(直通)。
        let loaded = crate::kiro::model::credentials::CredentialsConfig::load(&cred_path)
            .unwrap()
            .into_sorted_credentials();
        assert_eq!(loaded[0].refresh_token.as_deref(), Some("legacy-plain-token"));

        // 加密关(默认)→ persist 后仍是明文(不惊扰现有用户)。
        let mgr = MultiTokenManager::new(Config::default(), loaded, None, Some(cred_path.clone()), true).unwrap();
        mgr.set_priority(1, 7).unwrap();
        let raw = std::fs::read(&cred_path).unwrap();
        assert!(!crate::common::secret_store::is_encrypted(&raw), "加密关时应保持明文");
        assert!(String::from_utf8_lossy(&raw).contains("legacy-plain-token"));

        std::fs::remove_dir_all(&dir).ok();
    }

    /// TEMP ADVERSARIAL REPRO (F1): 池中只有 custom_api 号时 acquire_context 是否忙等不返回。
    #[test]
    fn temp_repro_f1_custom_api_only_busy_loop() {
        let (tx, rx) = std::sync::mpsc::channel::<&'static str>();
        std::thread::spawn(move || {
            let mut config = Config::default();
            config.load_balancing_mode = "balanced".to_string();
            let mut c = KiroCredentials::default();
            c.id = Some(1);
            c.auth_method = Some("custom_api".to_string());
            c.base_url = Some("https://relay.example.invalid".to_string());
            c.api_key = Some("sk-1".to_string());
            let mgr = MultiTokenManager::new(config, vec![c], None, None, false).unwrap();
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let r = rt.block_on(async { mgr.acquire_context(None, None).await });
            let _ = tx.send(if r.is_ok() { "ok" } else { "err" });
        });
        match rx.recv_timeout(std::time::Duration::from_secs(5)) {
            Ok(v) => println!("F1-REPRO: acquire_context 返回了 {v}（未忙等）"),
            Err(_) => println!("F1-REPRO: acquire_context 5s 内未返回（忙等热循环成立）"),
        }
    }

    /// TEMP ADVERSARIAL REPRO (F1b): 所有号对该模型进 model_blocklist 时是否忙等。
    #[test]
    fn temp_repro_f1b_model_blocked_busy_loop() {
        let (tx, rx) = std::sync::mpsc::channel::<&'static str>();
        std::thread::spawn(move || {
            let mut config = Config::default();
            config.load_balancing_mode = "balanced".to_string();
            let mut c = KiroCredentials::default();
            c.id = Some(1);
            c.access_token = Some("tok-1".to_string());
            c.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());
            let mgr = MultiTokenManager::new(config, vec![c], None, None, false).unwrap();
            mgr.report_model_invalid(1, Some("claude-opus-4.8"));
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let r = rt.block_on(async {
                mgr.acquire_context(Some("claude-opus-4.8"), None).await
            });
            let _ = tx.send(if r.is_ok() { "ok" } else { "err" });
        });
        match rx.recv_timeout(std::time::Duration::from_secs(5)) {
            Ok(v) => println!("F1b-REPRO: acquire_context 返回了 {v}（未忙等）"),
            Err(_) => println!("F1b-REPRO: acquire_context 5s 内未返回（忙等热循环成立）"),
        }
    }

}
