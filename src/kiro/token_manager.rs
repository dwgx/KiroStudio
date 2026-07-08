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
use tokio::time::sleep;

use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::sync::atomic::AtomicU32;
use std::time::{Duration as StdDuration, Instant};

use crate::http_client::{ProxyConfig, build_client};
use crate::kiro::affinity::UserAffinityManager;
use crate::kiro::cooldown::{CooldownManager, CooldownReason};
use crate::kiro::machine_id;
use crate::kiro::model::credentials::{KiroCredentials, TrashEntry};
use crate::kiro::model::token_refresh::{
    ExternalIdpRefreshResponse, IdcRefreshRequest, IdcRefreshResponse, RefreshRequest,
    RefreshResponse,
};
use crate::kiro::model::usage_limits::UsageLimitsResponse;
use crate::kiro::rate_limiter::{RateLimitConfig, RateLimiter};
use crate::kiro::scheduling::{InflightGuard, RpmTracker};
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

/// 获取使用额度信息
pub(crate) async fn get_usage_limits(
    credentials: &KiroCredentials,
    config: &Config,
    token: &str,
    proxy: Option<&ProxyConfig>,
) -> anyhow::Result<UsageLimitsResponse> {
    tracing::debug!("正在获取使用额度信息...");

    // 优先级：凭据.api_region > config.api_region > config.region
    let region = credentials.effective_api_region(config);
    let host = format!("q.{}.amazonaws.com", region);
    let machine_id = machine_id::generate_from_credentials(credentials, config);
    let kiro_version = &config.kiro_version;
    let os_name = &config.system_version;
    let node_version = &config.node_version;

    // 构建 URL
    let mut url = format!(
        "https://{}/getUsageLimits?origin=AI_EDITOR&resourceType=AGENTIC_REQUEST",
        host
    );

    // profileArn 是可选的
    if let Some(profile_arn) = &credentials.profile_arn {
        url.push_str(&format!("&profileArn={}", urlencoding::encode(profile_arn)));
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
    config: Config,
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
    /// 是否启用冷却
    cooldown_enabled: bool,
    /// 拟人速率限制器（防关联：每日上限 + 请求间隔）
    rate_limiter: RateLimiter,
    /// 是否启用速率限制
    rate_limit_enabled: bool,
    /// 会话亲和性管理器（防关联：同一会话粘同一凭据）
    affinity: UserAffinityManager,
    /// 是否启用会话亲和性
    affinity_enabled: bool,
    /// RPM 滚动窗口追踪器（balanced 选号时对接近 RPM 上限的号降权）
    rpm: RpmTracker,
    /// 每凭据 RPM 软上限（0 = 不限制）
    rpm_limit: u32,
    /// 全池冷却时是否快速失败（立即返回 429+Retry-After 让客户端退避，而非网关内硬扛）。
    all_cooling_fast_fail: bool,
    /// 是否在凭据持续可疑活动风控(trigger_count 达阈值)时自动禁用它。
    auto_disable_suspicious: bool,
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
                    last_used_at: None,
                    inflight: Arc::new(AtomicU32::new(0)),
                }
            })
            .collect();

        // 校验 API Key 凭据配置完整性：authMethod=api_key 时必须提供 kiroApiKey
        let mut entries = entries;
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
        let rate_limit_config = RateLimitConfig {
            daily_max_requests: config.rate_limit_daily_max,
            min_interval_ms: config.rate_limit_min_interval_ms,
            ..RateLimitConfig::default()
        };
        let manager = Self {
            config,
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
            cooldown_enabled,
            rate_limiter: RateLimiter::new(rate_limit_config),
            rate_limit_enabled,
            affinity: UserAffinityManager::new(),
            affinity_enabled,
            rpm: RpmTracker::new(),
            rpm_limit,
            all_cooling_fast_fail,
            auto_disable_suspicious,
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

    /// 获取配置的引用
    pub fn config(&self) -> &Config {
        &self.config
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
        if !self.cooldown_enabled {
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

        // 过滤可用凭据：可选性判定统一收敛到 is_entry_selectable
        // （disabled / opus 订阅 / 冷却 / 限流）。历史上此处曾在其后再挂一个
        // 逐字段重复的 filter（inflight 改动残留），锁临界区内重复判定 + config 克隆
        // 翻倍；已合并为单次 filter。
        let available: Vec<&CredentialEntry> = entries
            .iter()
            .filter(|e| self.is_entry_selectable(e, is_opus))
            .collect();

        if available.is_empty() {
            return None;
        }

        // 会话亲和性：若该会话已绑定某凭据且当前可用，优先复用，让同一对话粘同一账号
        if self.affinity_enabled {
            if let Some(uid) = user_id {
                if let Some(bound_id) = self.affinity.get(uid) {
                    if let Some(entry) = available.iter().find(|e| e.id == bound_id) {
                        tracing::debug!(user_id = %uid, credential_id = %bound_id, "亲和性复用凭据");
                        // 续期，使持续活跃的会话不因 TTL 到期而解绑
                        self.affinity.touch(uid);
                        return Some(self.commit_selection(entry));
                    }
                    // 绑定的凭据已不可用（禁用/冷却/限流），解绑后按常规策略重选
                    tracing::debug!(
                        user_id = %uid,
                        credential_id = %bound_id,
                        "亲和性绑定的凭据当前不可用，重新选择"
                    );
                }
            }
        }

        let mode = self.load_balancing_mode.lock().clone();
        let mode = mode.as_str();

        let selected = match mode {
            "balanced" => {
                // 按 (rpm 饱和, 在途数, 近 60s RPM, 终身成功数, 优先级) 升序：
                // ① 先避开 RPM 已饱和的号（软降权，非硬跳过）；
                // ② 再挑在飞请求最少的（分摊并发）；
                // ③ ⭐再挑**近 60s 实际负载（RPM）最少**的——真正的"分流"关键：
                //    让流量均匀铺到所有号，每号 RPM 降到 ~1/N，直接降低账户级可疑活动的触发频率（防封）。
                // ④ 终身成功数（Least-Used）与优先级仅作平局兜底。
                //
                // 为何不是终身 success_count 主导：那会让新号（终身计数低）被持续灌到追平老号为止，
                // 把负载**集中**在少数号上（老号闲置、新号被打爆）——既让部分号"不动"，又抬高单号 RPM
                // 触发风控，与防封目标背道而驰。改用滚动窗口 RPM 主导即真正的即时均衡。
                available
                    .iter()
                    .min_by_key(|e| {
                        (
                            self.is_rpm_saturated(e.id),
                            e.inflight.load(Ordering::Acquire),
                            self.rpm.count(e.id),
                            e.success_count,
                            e.credentials.priority,
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
        if self.affinity_enabled {
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
    fn is_entry_selectable(&self, entry: &CredentialEntry, is_opus: bool) -> bool {
        if entry.disabled {
            return false;
        }
        if is_opus && !entry.credentials.supports_opus() {
            return false;
        }
        if self.cooldown_enabled && !self.cooldown.is_available(entry.id) {
            return false;
        }
        if self.rate_limit_enabled && self.rate_limiter.check_rate_limit(entry.id).is_err() {
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

            if self.cooldown_enabled {
                if let Some((_reason, remaining)) = self.cooldown.check_cooldown(entry.id) {
                    waits.push(remaining);
                    continue;
                }
            }

            if self.rate_limit_enabled {
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
    fn is_rpm_saturated(&self, id: u64) -> bool {
        self.rpm_limit > 0 && self.rpm.count(id) >= self.rpm_limit
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

            let (id, credentials, inflight) = {
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
                        .find(|e| e.id == current_id && self.is_entry_selectable(e, is_opus))
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
                            if self.all_cooling_fast_fail && wait > FAST_FAIL_THRESHOLD {
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
            match self.try_ensure_token(id, &credentials, inflight).await {
                Ok(ctx) => {
                    // 记录一次速率获取（递增每日计数 + 标记本次请求时间，驱动最小间隔）
                    if self.rate_limit_enabled {
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

    /// 尝试使用指定凭据获取有效 Token
    ///
    /// 使用双重检查锁定模式，确保同一时间只有一个刷新操作
    ///
    /// # Arguments
    /// * `id` - 凭据 ID，用于更新正确的条目
    /// * `credentials` - 凭据信息
    /// * `inflight` - 选号时占用的在途守卫；成功则移入 `CallContext` 随请求存活，
    ///   失败则随本函数返回而 Drop（该次尝试不再在途，inflight -1）。
    async fn try_ensure_token(
        &self,
        id: u64,
        credentials: &KiroCredentials,
        inflight: InflightGuard,
    ) -> anyhow::Result<CallContext> {
        // API Key 凭据直接使用 kiro_api_key 作为 Bearer Token，无需刷新
        if credentials.is_api_key_credential() {
            let token = credentials
                .kiro_api_key
                .clone()
                .ok_or_else(|| anyhow::anyhow!("API Key 凭据缺少 kiroApiKey"))?;
            return Ok(CallContext {
                id,
                credentials: credentials.clone(),
                token,
                inflight,
            });
        }

        // 第一次检查（无锁）：快速判断是否需要刷新
        let needs_refresh = is_token_expired(credentials) || is_token_expiring_soon(credentials);

        let creds = if needs_refresh {
            // 获取刷新锁，确保同一时间只有一个刷新操作
            let _guard = self.refresh_lock.lock().await;

            // 第二次检查：获取锁后重新读取凭据，因为其他请求可能已经完成刷新
            let current_creds = {
                let entries = self.entries.lock();
                entries
                    .iter()
                    .find(|e| e.id == id)
                    .map(|e| e.credentials.clone())
                    .ok_or_else(|| anyhow::anyhow!("凭据 #{} 不存在", id))?
            };

            if is_token_expired(&current_creds) || is_token_expiring_soon(&current_creds) {
                // 确实需要刷新
                let effective_proxy = current_creds.effective_proxy(self.proxy.as_ref());
                let new_creds =
                    refresh_token(&current_creds, &self.config, effective_proxy.as_ref()).await?;

                if is_token_expired(&new_creds) {
                    anyhow::bail!("刷新后的 Token 仍然无效或已过期");
                }

                // 更新凭据
                {
                    let mut entries = self.entries.lock();
                    if let Some(entry) = entries.iter_mut().find(|e| e.id == id) {
                        entry.credentials = new_creds.clone();
                    }
                }

                // 回写凭据到文件（仅多凭据格式），失败只记录警告
                if let Err(e) = self.persist_credentials() {
                    tracing::warn!("Token 刷新后持久化失败（不影响本次请求）: {}", e);
                }

                new_creds
            } else {
                // 其他请求已经完成刷新，直接使用新凭据
                tracing::debug!("Token 已被其他请求刷新，跳过刷新");
                current_creds
            }
        } else {
            credentials.clone()
        };

        let token = creds
            .access_token
            .clone()
            .ok_or_else(|| anyhow::anyhow!("没有可用的 accessToken"))?;

        {
            let mut entries = self.entries.lock();
            if let Some(entry) = entries.iter_mut().find(|e| e.id == id) {
                entry.refresh_failure_count = 0;
            }
        }

        Ok(CallContext {
            id,
            credentials: creds,
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
        if self.cooldown_enabled {
            self.cooldown.clear_cooldown(id);
        }
        if self.rate_limit_enabled {
            self.rate_limiter.record_success(id);
        }
        self.save_stats_debounced();
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
        // 记录速率失败（驱动指数退避）
        if self.rate_limit_enabled {
            self.rate_limiter.record_failure(id, None);
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
        if self.cooldown_enabled {
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
        if self.rate_limit_enabled {
            self.rate_limiter.record_failure(id, Some("rate limited"));
        }
    }

    /// 报告凭据触发**账户级可疑活动风控**（`suspicious activity`+`temporary limits`）。
    ///
    /// 与普通 429 的关键区别：走 [`CooldownReason::SuspiciousActivity`] 的**分钟级冷却**
    /// （基线 3min，递增至上限 30min），而非普通限速的 15s 瞬时冷却。
    /// 原因：可疑活动是账户级风控、持续数分钟且 Kiro 正在"调查"——15s 后重新入池会被
    /// 立刻再限，反复砸只会加重可疑度、把账户推向真封禁。此处让该号**真正退避**，
    /// 冷却期内不参与选号，等调查窗口过去自愈。不禁用、不计永久失败、不改发往上游字节。
    pub fn report_suspicious_activity(&self, id: u64) {
        if self.cooldown_enabled {
            let dur = self
                .cooldown
                .set_cooldown(id, CooldownReason::SuspiciousActivity);
            tracing::warn!(
                "凭据 #{} 触发账户级可疑活动风控，冷却 {:?}（分钟级退避，避免反复砸加重风控/触发封禁）",
                id,
                dur
            );

            // 自动禁用(dwgx:账户不行了就自动禁用)：连续可疑活动触发达阈值,说明该号已被 Kiro 盯死、
            // 冷却也顶格(30min)仍反复被限——继续放它参与调度只会不停砸、加重风控甚至触发真封禁。
            // 达阈值即自动禁用并标注 SuspiciousActivityAuto(可人工/自愈重新启用),把它移出轮转。
            const AUTO_DISABLE_TRIGGER: u32 = 10;
            if self.auto_disable_suspicious && self.cooldown.trigger_count(id) >= AUTO_DISABLE_TRIGGER {
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
        if self.rate_limit_enabled {
            self.rate_limiter.record_failure(id, Some("suspicious activity"));
        }
    }

    /// 报告凭据认证失败，设置较长冷却（配合 force-refresh 失败后调用）
    pub fn report_auth_cooldown(&self, id: u64) {
        if self.cooldown_enabled {
            let dur = self
                .cooldown
                .set_cooldown(id, CooldownReason::AuthenticationFailed);
            tracing::warn!("凭据 #{} 认证失败，冷却 {:?}", id, dur);
        }
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
        if self.cooldown_enabled {
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
        let credentials = {
            let entries = self.entries.lock();
            entries
                .iter()
                .find(|e| e.id == id)
                .map(|e| e.credentials.clone())
                .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?
        };

        // API Key 凭据直接使用 kiro_api_key，无需刷新
        let token = if credentials.is_api_key_credential() {
            credentials
                .kiro_api_key
                .clone()
                .ok_or_else(|| anyhow::anyhow!("API Key 凭据缺少 kiroApiKey"))?
        } else {
            // 检查是否需要刷新 token
            let needs_refresh =
                is_token_expired(&credentials) || is_token_expiring_soon(&credentials);

            if needs_refresh {
                let _guard = self.refresh_lock.lock().await;
                let current_creds = {
                    let entries = self.entries.lock();
                    entries
                        .iter()
                        .find(|e| e.id == id)
                        .map(|e| e.credentials.clone())
                        .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?
                };

                if is_token_expired(&current_creds) || is_token_expiring_soon(&current_creds) {
                    let effective_proxy = current_creds.effective_proxy(self.proxy.as_ref());
                    let new_creds =
                        refresh_token(&current_creds, &self.config, effective_proxy.as_ref())
                            .await?;
                    {
                        let mut entries = self.entries.lock();
                        if let Some(entry) = entries.iter_mut().find(|e| e.id == id) {
                            entry.credentials = new_creds.clone();
                        }
                    }
                    // 持久化失败只记录警告，不影响本次请求
                    if let Err(e) = self.persist_credentials() {
                        tracing::warn!("Token 刷新后持久化失败（不影响本次请求）: {}", e);
                    }
                    new_creds
                        .access_token
                        .ok_or_else(|| anyhow::anyhow!("刷新后无 access_token"))?
                } else {
                    current_creds
                        .access_token
                        .ok_or_else(|| anyhow::anyhow!("凭据无 access_token"))?
                }
            } else {
                credentials
                    .access_token
                    .ok_or_else(|| anyhow::anyhow!("凭据无 access_token"))?
            }
        };

        let credentials = {
            let entries = self.entries.lock();
            entries
                .iter()
                .find(|e| e.id == id)
                .map(|e| e.credentials.clone())
                .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?
        };

        let effective_proxy = credentials.effective_proxy(self.proxy.as_ref());
        let usage_limits = get_usage_limits(&credentials, &self.config, &token, effective_proxy.as_ref()).await?;

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
        let credentials = {
            let entries = self.entries.lock();
            entries
                .iter()
                .find(|e| e.id == id)
                .map(|e| e.credentials.clone())
                .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?
        };

        if credentials.is_api_key_credential() {
            anyhow::bail!("API Key 凭据不支持 Web Portal 接口（overage 开关仅限 social 凭据）");
        }

        // 需要有效 token：过期或即将过期则先刷新（复用 get_usage_limits_for 的双检锁流程）
        let needs_refresh = is_token_expired(&credentials) || is_token_expiring_soon(&credentials);
        let final_creds = if needs_refresh {
            let _guard = self.refresh_lock.lock().await;
            let current = {
                let entries = self.entries.lock();
                entries
                    .iter()
                    .find(|e| e.id == id)
                    .map(|e| e.credentials.clone())
                    .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?
            };
            if is_token_expired(&current) || is_token_expiring_soon(&current) {
                let effective_proxy = current.effective_proxy(self.proxy.as_ref());
                let new_creds =
                    refresh_token(&current, &self.config, effective_proxy.as_ref()).await?;
                {
                    let mut entries = self.entries.lock();
                    if let Some(entry) = entries.iter_mut().find(|e| e.id == id) {
                        entry.credentials = new_creds.clone();
                    }
                }
                if let Err(e) = self.persist_credentials() {
                    tracing::warn!("Token 刷新后持久化失败（不影响本次请求）: {}", e);
                }
                new_creds
            } else {
                current
            }
        } else {
            credentials
        };

        let token = final_creds
            .access_token
            .clone()
            .ok_or_else(|| anyhow::anyhow!("凭据无 access_token"))?;
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
            tls_backend: self.config.tls_backend,
        })
    }

    /// 深度验活：发送最小 generateAssistantResponse 请求检测账号 suspend 状态
    ///
    /// getUsageLimits 不检查 suspend，只有真实对话请求才能检测。
    /// 发送一个会被服务端拒绝（空 conversationState）的请求，
    /// 只要返回 400（格式错误）而非 403（suspend）即表示凭据存活。
    pub async fn deep_verify_credential(&self, id: u64) -> anyhow::Result<()> {
        let credentials = {
            let entries = self.entries.lock();
            entries
                .iter()
                .find(|e| e.id == id)
                .map(|e| e.credentials.clone())
                .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?
        };

        let token = if credentials.is_api_key_credential() {
            credentials
                .kiro_api_key
                .clone()
                .ok_or_else(|| anyhow::anyhow!("API Key 凭据缺少 kiroApiKey"))?
        } else {
            let needs_refresh =
                is_token_expired(&credentials) || is_token_expiring_soon(&credentials);
            if needs_refresh {
                let _guard = self.refresh_lock.lock().await;
                let current_creds = {
                    let entries = self.entries.lock();
                    entries
                        .iter()
                        .find(|e| e.id == id)
                        .map(|e| e.credentials.clone())
                        .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?
                };
                if is_token_expired(&current_creds) || is_token_expiring_soon(&current_creds) {
                    let effective_proxy = current_creds.effective_proxy(self.proxy.as_ref());
                    let new_creds =
                        refresh_token(&current_creds, &self.config, effective_proxy.as_ref())
                            .await?;
                    {
                        let mut entries = self.entries.lock();
                        if let Some(entry) = entries.iter_mut().find(|e| e.id == id) {
                            entry.credentials = new_creds.clone();
                        }
                    }
                    if let Err(e) = self.persist_credentials() {
                        tracing::warn!("深度验活刷新后持久化失败: {}", e);
                    }
                    new_creds
                        .access_token
                        .ok_or_else(|| anyhow::anyhow!("刷新后无 access_token"))?
                } else {
                    current_creds
                        .access_token
                        .clone()
                        .ok_or_else(|| anyhow::anyhow!("凭据无 access_token"))?
                }
            } else {
                credentials
                    .access_token
                    .clone()
                    .ok_or_else(|| anyhow::anyhow!("凭据无 access_token"))?
            }
        };

        let region = credentials.effective_api_region(&self.config);
        let host = format!("q.{}.amazonaws.com", region);
        let url = format!("https://{}/generateAssistantResponse", host);
        let machine_id = machine_id::generate_from_credentials(&credentials, &self.config);
        let kiro_version = &self.config.kiro_version;
        let os_name = &self.config.system_version;
        let node_version = &self.config.node_version;

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
        if credentials.should_send_profile_arn() {
            let arn = credentials.profile_arn.as_ref().unwrap();
            body["profileArn"] = serde_json::Value::String(arn.clone());
        }

        let effective_proxy = credentials.effective_proxy(self.proxy.as_ref());
        let client = build_client(effective_proxy.as_ref(), 30, self.config.tls_backend)?;

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

        // 400 = 请求格式错误（预期的，说明凭据有效，只是请求体不完整）
        // 200/其他 = 凭据有效
        Ok(())
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
            refresh_token(&new_cred, &self.config, effective_proxy.as_ref()).await?
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

        {
            let mut entries = self.entries.lock();
            entries.push(CredentialEntry {
                id: new_id,
                credentials: validated_cred,
                failure_count: 0,
                refresh_failure_count: 0,
                disabled: false,
                disabled_reason: None,
                success_count: 0,
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

        // 条件刷新（后台预刷新）：token 已不再将过期 → 跳过，避免重复刷新
        if let Some(lead) = conditional_lead {
            if !is_token_expiring_within(&credentials, lead).unwrap_or(false) {
                return Ok(RefreshOutcome::Skipped);
            }
        }

        let effective_proxy = credentials.effective_proxy(self.proxy.as_ref());
        let new_creds =
            refresh_token(&credentials, &self.config, effective_proxy.as_ref()).await?;

        // 更新 entries 中对应凭据
        {
            let mut entries = self.entries.lock();
            if let Some(entry) = entries.iter_mut().find(|e| e.id == id) {
                entry.credentials = new_creds;
                entry.refresh_failure_count = 0;
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

        let config_path = match self.config.config_path() {
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
            "https://192.168.11.4/token",
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
        // 空字符串 auth_region 被视为已设置（虽然不推荐，但行为应一致）
        let mut config = Config::default();
        config.region = "us-west-2".to_string();

        let mut credentials = KiroCredentials::default();
        credentials.auth_region = Some("".to_string());

        let region = credentials.effective_auth_region(&config);
        // 空字符串被视为已设置，不会回退到 config
        assert_eq!(region, "");
    }

    #[test]
    fn test_auth_and_api_region_independent() {
        // auth_region 和 api_region 互不影响
        let mut config = Config::default();
        config.region = "default".to_string();

        let mut credentials = KiroCredentials::default();
        credentials.auth_region = Some("auth-only".to_string());
        credentials.api_region = Some("api-only".to_string());

        assert_eq!(credentials.effective_auth_region(&config), "auth-only");
        assert_eq!(credentials.effective_api_region(&config), "api-only");
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
