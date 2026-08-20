//! Token 管理模块
//!
//! 负责 Token 过期检测和刷新，支持 Social 和 IdC 认证方式
//! 支持多凭据 (MultiTokenManager) 管理

use anyhow::bail;
use chrono::{DateTime, Datelike, Duration, Timelike, Utc};
use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex as TokioMutex;
use tokio::task::JoinHandle;
use tokio::time::sleep;

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::atomic::{AtomicU32, AtomicU64};
use std::time::{Duration as StdDuration, Instant};

use arc_swap::ArcSwap;

use crate::http_client::{
    ProxyConfig, build_client, build_client_no_redirect, build_streaming_client,
};
use crate::kiro::affinity::UserAffinityManager;
use crate::kiro::cooldown::{CooldownManager, CooldownReason};
use crate::kiro::health::HealthTracker;
use crate::kiro::machine_id;
use crate::kiro::model::credentials::{KiroCredentials, TrashEntry};
use crate::kiro::model::token_refresh::{
    ExternalIdpRefreshResponse, IdcRefreshRequest, IdcRefreshResponse, RefreshRequest,
    RefreshResponse,
};
use crate::kiro::model::usage_limits::UsageLimitsResponse;
use crate::kiro::rate_limiter::{FailureKind, RateLimitConfig, RateLimiter};
use crate::kiro::scheduling::{InflightGuard, RpmTracker, RAMP_RECENT_SECS};
use crate::model::config::Config;

#[path = "credential_persist.rs"]
mod credential_persist;
#[path = "token_refresh_http.rs"]
mod token_refresh_http;

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

/// `pub(crate)`：`admin::service` 的余额缓存键复用它算账号指纹
/// （见 `AdminService::balance_cache_key`）。与 `api_key_hash` 字段同一个算法 ——
/// 两处若各写一份 hash，同一个 key 会算出两个键，余额同步会静默失效。
pub(crate) fn sha256_hex(input: &str) -> String {
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
        .ok_or_else(|| RefreshValidationError::new("缺少 refreshToken"))?;

    if refresh_token.is_empty() {
        return Err(RefreshValidationError::new("refreshToken 为空").into());
    }

    if refresh_token.len() < 100 || refresh_token.ends_with("...") || refresh_token.contains("...")
    {
        return Err(RefreshValidationError::new(format!(
            "refreshToken 已被截断（长度: {} 字符）。\n\
             这通常是 Kiro IDE 为了防止凭证被第三方工具使用而故意截断的。",
            refresh_token.len()
        ))
        .into());
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

/// 刷新链路**带 HTTP 状态码**的错误（2026-08-15 新增）。
///
/// 可重试性判定用结构化 `status` 字段，不再靠裸子串匹配状态码数字
/// （`contains("500")` 会把错误串里的 URL 端口 / 错误体数字 / 毫秒时间误判成 5xx，
/// 也会把真 5xx 误判成网络错误）。Display 保留原文（`{status} {message}` 形态），
/// 字符串消费点不受影响。
#[derive(Debug)]
pub(crate) struct RefreshHttpError {
    pub status: u16,
    pub message: String,
}

impl fmt::Display for RefreshHttpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for RefreshHttpError {}

/// 结构性不可刷新（api_key 号没有 refreshToken）：任何重试都不可能成功。
///
/// Display 保留原串「API Key 凭据不支持刷新 Token」——`is_refresh_error_credential_level`
/// 与 admin/service.rs 仍按字符串分类，不受类型化影响。调度层（refresh_token_locked）
/// 按类型识别并**不重试**（此前靠黑名单字符串排除，见该处历史注释）。
#[derive(Debug)]
pub(crate) struct RefreshNotSupportedError;

impl fmt::Display for RefreshNotSupportedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "API Key 凭据不支持刷新 Token")
    }
}

impl std::error::Error for RefreshNotSupportedError {}

/// 本地校验/配置类错误（refreshToken 缺失/截断、构建刷新客户端失败等）：
/// 与凭据内容或本机配置绑定，重试必败 —— 不参与瞬态退避（白等 1s+2s，
/// 且每轮多计一次失败，加速把号判成死号）。
///
/// Display 保留原文案（`is_refresh_error_credential_level` 等字符串消费点不受影响）。
#[derive(Debug)]
pub(crate) struct RefreshValidationError {
    message: String,
}

impl RefreshValidationError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for RefreshValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for RefreshValidationError {}

/// 刷新错误的可重试性判定（2026-08-15 起**结构化**，不靠字符串猜）。
///
/// 调用方须**先行排除**永久类型（`RefreshTokenInvalidError` / `DiagnosedError` /
/// `RefreshNotSupportedError`），本函数只回答「余下错误是否值得退避重试」：
/// - 带 [`RefreshHttpError`] 状态码的：仅 5xx 可重试（403/429 等策略性失败不重试）；
/// - 本地校验/配置类（[`RefreshValidationError`]）：重试必败，不重试；
/// - 其余无状态码的（reqwest 连接/超时/JSON 解析）：网络层瞬态，可重试。
///
/// 与调用方注释承诺对齐：仅对 5xx 和网络/连接错误重试。
fn refresh_error_retryable(e: &anyhow::Error) -> bool {
    // 本地校验/配置类错误与凭据内容绑定，重试结果不变 —— 直接不重试。
    if e.downcast_ref::<RefreshValidationError>().is_some() {
        return false;
    }
    match e.downcast_ref::<RefreshHttpError>() {
        Some(http) => (500..600).contains(&http.status),
        None => true,
    }
}

/// 刷新 Token
pub(crate) async fn refresh_token(
    credentials: &KiroCredentials,
    config: &Config,
    proxy: Option<&ProxyConfig>,
) -> anyhow::Result<KiroCredentials> {
    // API Key 凭据不支持 Token 刷新：底层契约级拦截
    // 其他调用点（try_ensure_token / 活跃路径 / add_credential）在调用前已显式分流 API Key；
    // 仅 force_refresh_token_for 未分流，此处返回类型化错误（Display 保留原串）让错误
    // 自然传播为 400 BAD_REQUEST，且 refresh_token_locked 按类型识别、绝不退避重试。
    if credentials.is_api_key_credential() {
        return Err(RefreshNotSupportedError.into());
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

    let refresh_token = credentials
        .refresh_token
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("Social 刷新需要 refreshToken"))?;
    // 优先级：凭据.auth_region > 凭据.region > config.auth_region > config.region
    let region = credentials.effective_auth_region(config);

    let refresh_url = format!("https://prod.{}.auth.desktop.kiro.dev/refreshToken", region);
    let refresh_domain = format!("prod.{}.auth.desktop.kiro.dev", region);
    let machine_id = machine_id::generate_from_credentials(credentials, config);
    // ⚠️ 2026-08-14 对抗审查 M1：版本段**固定**用 config.kiro_version，不走
    // version_mask::effective —— 刷新请求不带 profileArn（参考仓实测新版 IDE
    // 会在用量类接口按版本/UA 强制 profileArn 导致 400；刷新接口属于同一批
    // 未验证形态，若上游对刷新端点同源判定，换最新版 UA = social 号全池无法
    // 续期）。号池存亡路径不换未验证指纹：要么先在 staging 实测「最新版 UA +
    // 无 profileArn」刷新返回 200，再接线。
    let kiro_version = config.kiro_version.clone();

    let client = build_client(proxy, 60, config.tls_backend)
        .map_err(|e| RefreshValidationError::new(format!("构建刷新客户端失败: {}", e)))?;
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
        let body_text = {
                // ⚠️ 2026-08-11 横切审查：错误体原文可能含账号/凭据类敏感串（上游
                // auth 端点无法离线验证回显内容），进日志/错误消息前截断，与对话路径
                // 「原文只进日志不回客户端」的纪律对齐（此处连日志也截断，双保险）。
                let raw = response.text().await.unwrap_or_default();
                raw.chars().take(200).collect::<String>()
            };

        // 400 + invalid_grant → refreshToken 永久失效
        if status.as_u16() == 400 && body_text.contains("invalid_grant") {
            return Err(RefreshTokenInvalidError {
                message: format!("Social refreshToken 已失效 (invalid_grant): {}", body_text),
            }
            .into());
        }

        // 401 = Cognito token 被吊销或已永久过期 → 立即禁用凭据。
        if status.as_u16() == 401 {
            return Err(RefreshTokenInvalidError {
                message: format!(
                    "Social refreshToken 已失效 (401 Unauthorized): {}",
                    body_text
                ),
            }
            .into());
        }

        let error_msg = match status.as_u16() {
            403 => "权限不足，无法刷新 Token",
            429 => "请求过于频繁，已被限流",
            500..=599 => "服务器错误，AWS OAuth 服务暂时不可用",
            _ => "Token 刷新失败",
        };
        return Err(RefreshHttpError {
            status: status.as_u16(),
            message: format!("{}: {} {}", error_msg, status, body_text),
        }
        .into());
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
    let host = authority
        .split(':')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
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

    let refresh_token = credentials
        .refresh_token
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("External IdP 刷新需要 refreshToken"))?;
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

    let client = build_client(proxy, 60, config.tls_backend)
        .map_err(|e| RefreshValidationError::new(format!("构建刷新客户端失败: {}", e)))?;
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
                message: format!(
                    "External IdP refreshToken 已失效 (invalid_grant): {}",
                    body_text
                ),
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
        return Err(RefreshHttpError {
            status: status.as_u16(),
            message: format!("{}: {} {}", error_msg, status, body_text),
        }
        .into());
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

    let refresh_token = credentials
        .refresh_token
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("IdC 刷新需要 refreshToken"))?;
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

    let client = build_client(proxy, 60, config.tls_backend)
        .map_err(|e| RefreshValidationError::new(format!("构建刷新客户端失败: {}", e)))?;
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
        if status.as_u16() == 400 && body_text.contains("invalid_grant") {
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

/// 把刷新产物（`new_creds`）里刷新链路**真正拥有**的字段，逐个搬到活的 `entry_credentials`
/// 上，其余字段原地不动。**白名单**，不是排除法：
///
/// `refresh_social_token` / `refresh_idc_token` / `refresh_external_idp_token`（见上方三个
/// 函数体）都是 `let mut new_credentials = credentials.clone();` 之后只对下列 4 个字段按响应体
/// 条件赋值 `Some(..)`，从未触碰其它字段：
/// - `access_token`  — 三者都无条件写
/// - `refresh_token` — 响应带 `refresh_token` 时轮换
/// - `expires_at`    — 响应带 `expires_in` 时重算
/// - `profile_arn`   — 仅 social/idc 响应可能带，external_idp 从不写（该函数体内无此赋值）
///
/// 为什么用白名单而非黑名单排除已知的非 token 字段：`KiroCredentials` 将来加新字段时，
/// 黑名单会漏掉新字段——新字段若不慎被写在 `new_creds` 上就会被这条路径静默搬运/覆盖；
/// 白名单只搬运这里显式列出的 4 个字段，新增字段天然不在名单里，永远原样保留 `entry_credentials`
/// 上的值，不可能被这条路径意外覆盖。
///
/// # 为什么不能 `entry.credentials = new_creds` 整体替换（🔴 磁盘级数据丢失）
///
/// `new_creds` 源自刷新发起前的快照 `credentials.clone()`（见 `refresh_token_locked` 顶部）。
/// 刷新是跨 `.await` 的网络往返（含重试退避最坏约 183s），期间其它路径可能已经改动了
/// 快照之外的字段并写回 `entries`——典型例子是每 30 分钟一次的余额刷新环（见 `:subscription_title`
/// 的写入点），它会更新 `subscription_title` 等字段。若整体替换，这些改动会被本次刷新的
/// 陈旧快照**回退**，且随后 `persist_credentials()` 会把回退结果**写进磁盘**：面板显示的
/// 额度信息永久卡在刷新发起前的旧值，直到下一次余额环再覆盖一次（而下一次刷新又会再撞一次）。
///
/// # 与陈旧刷新守卫的关系
///
/// 写回前仍保留 `entry.credentials.refresh_token != refresh_token_snapshot` 的陈旧守卫——
/// 它防的是另一件事："用本次换到的新 token 覆盖已经被别的并发刷新路径抢先轮换过的
/// `refresh_token`"，这与"逐字段合并 vs 整体替换"是两个独立问题，缺一不可。逐字段合并后，
/// 守卫的语义反而更准确：它只需要担保这 4 个 token 字段的新鲜度，不再被迫替其它无关字段
/// （如 `subscription_title`）的新鲜度背书。
fn apply_refresh_result_fields(entry_credentials: &mut KiroCredentials, new_creds: &KiroCredentials) {
    entry_credentials.access_token = new_creds.access_token.clone();
    entry_credentials.refresh_token = new_creds.refresh_token.clone();
    entry_credentials.expires_at = new_creds.expires_at.clone();
    entry_credentials.profile_arn = new_creds.profile_arn.clone();
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
    // ⭐ 候选端点(主 + 403 回退)。见 rest_api_region_candidates 的完整说明。
    let candidates = rest_api_region_candidates(region);

    // 请求构造（UA / URL / 头 / 状态码分类）全部收口在 fetch_usage_limits_once：
    // 本函数只负责「按候选顺序试、403 才换区」这一层策略。
    let client = build_client(proxy, 60, config.tls_backend)
        .map_err(|e| RefreshValidationError::new(format!("构建刷新客户端失败: {}", e)))?;

    let mut last_error: Option<String> = None;
    for (idx, cand_region) in candidates.iter().enumerate() {
        match fetch_usage_limits_once(&client, credentials, config, token, cand_region).await {
            Ok(data) => {
                if idx > 0 {
                    tracing::info!(
                        "getUsageLimits 在主端点失败后由备用端点 {} 成功（该号 SSO region 与 REST 端点不同区）",
                        cand_region
                    );
                }
                return Ok(data);
            }
            Err((status, msg)) => {
                // ⭐ 403 且还有备用端点 → 试另一个区。
                //
                // 这是 REST 端点只在 us-east-1 / eu-central-1 存在导致的：SSO region 是
                // 别的区（Enterprise/IdC 常见）时，按 SSO region 拼出的 host 根本不是这两个
                // 之一，或者是这两个里"错的那个"，上游一律回 403 `Invalid token`。
                // 只对 403 回退：401 是 token 真废、429 是限流，换端点都没有意义。
                if status == Some(403) && idx + 1 < candidates.len() {
                    tracing::debug!(
                        "getUsageLimits 在 {} 返回 403，尝试备用端点 {}",
                        cand_region,
                        candidates[idx + 1]
                    );
                    last_error = Some(msg);
                    continue;
                }
                bail!("{}", msg);
            }
        }
    }

    // 所有候选端点均 403（循环内只有 403 会走到这里）。
    bail!(
        "权限不足，无法获取使用额度（已试全部 {} 个 REST 端点）: {}",
        candidates.len(),
        last_error.unwrap_or_else(|| "无可用端点".to_string())
    );
}

/// 只打**一个** region 的 `getUsageLimits`，**绝不**换区回退。
///
/// # 为什么必须存在（region 自动探测的正确性依赖它）
///
/// [`get_usage_limits`] 自带 403 换区回退（见其循环）：探 `eu-central-1` 时若上游 403，
/// 它会**静默改打** `us-east-1`，成功后 `return Ok(_)` —— 调用方拿到的 `Ok` 里
/// **不含"实际生效的是哪个区"**。而 `region_probe` 正是拿这个 `Ok` 当"候选区可用"的判据，
/// 于是：一个真实授权在 `us-east-1` 的 `ksk_` key，探 `eu-central-1` → 内部回退到
/// `us-east-1` 成功 → 探测把 **`eu-central-1`** 写死进 `api_region` → 该号此后
/// 恒打 `q.eu-central-1.amazonaws.com` → **恒 403**。分身还会继承这个错值。
///
/// 这就是"US 的 key 添加后显示成 EU"的成因。探测必须用本函数：一次一个区、
/// 结论就是该区自己的结论。
///
/// 错误里带回 HTTP 状态码（`None` = 网络/解析层失败），让调用方能区分
/// 「403 该换区」与「401 token 已废」——两者的处置动作完全不同。
async fn fetch_usage_limits_once(
    client: &reqwest::Client,
    credentials: &KiroCredentials,
    config: &Config,
    token: &str,
    region: &str,
) -> Result<UsageLimitsResponse, (Option<u16>, String)> {
    {
        let response = build_usage_limits_request(client, credentials, config, token, region)
            .send()
            .await
            .map_err(|e| (None, format!("获取使用额度失败: {}", e)))?;
        let status = response.status();
        if status.is_success() {
            return response
                .json::<UsageLimitsResponse>()
                .await
                .map_err(|e| (None, format!("获取使用额度响应解析失败: {}", e)));
        }

        let body_text = response.text().await.unwrap_or_default();
        let code = status.as_u16();
        let error_msg = match code {
            401 => "认证失败，Token 无效或已过期",
            403 => "权限不足，无法获取使用额度",
            429 => "请求过于频繁，已被限流",
            500..=599 => "服务器错误，AWS 服务暂时不可用",
            _ => "获取使用额度失败",
        };
        Err((
            Some(code),
            format!("{}: {} {}", error_msg, status, body_text),
        ))
    }
}

/// `getUsageLimits` 的 `(host, url)`。**单一真相**：请求路径与 region 探测路径共用它。
///
/// 抽出来的理由是本仓踩过的那类事故：同一个 URL 各写一份，改了一处漏另一处，
/// 于是「探测打的」与「业务打的」静默分叉。探测的结论只有在两者同形时才有意义。
fn usage_limits_endpoint(credentials: &KiroCredentials, region: &str) -> (String, String) {
    // Kiro management API（已迁移，旧 q.{region}.amazonaws.com 不再提供本 REST 接口）
    let host = format!("management.{}.kiro.dev", region);
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
    (host, url)
}

/// region 探测要拿的那个 URL（只为日志与断言可见，不发请求）。
///
/// 存在的理由：`region_probe` 不许手搓 host（那正是它历史上分叉过的地方），
/// 而它的日志/测试需要知道「这一次到底打了哪个 URL」。
pub(crate) fn usage_limits_probe_url(credentials: &KiroCredentials, region: &str) -> String {
    usage_limits_endpoint(credentials, region).1
}

/// 装配一条 `getUsageLimits` 请求（不发送）。
///
/// `fetch_usage_limits_once`（业务取额度）与 `region_probe`（探区）**必须**共用它：
/// 判据的有效性完全建立在「探测请求与业务请求同形」之上 —— UA / `tokentype` /
/// `profileArn` 任一项不同都可能换来另一个状态码，那样探出来的结论就不适用于业务路径。
pub(crate) fn build_usage_limits_request(
    client: &reqwest::Client,
    credentials: &KiroCredentials,
    config: &Config,
    token: &str,
    region: &str,
) -> reqwest::RequestBuilder {
    let machine_id = machine_id::generate_from_credentials(credentials, config);
    let user_agent = format!(
        "aws-sdk-js/1.0.0 ua/2.1 os/{} lang/js md/nodejs#{} api/codewhispererruntime#1.0.0 m/N,E KiroIDE-{}-{}",
        config.system_version, config.node_version, config.kiro_version, machine_id
    );
    let amz_user_agent = format!(
        "aws-sdk-js/1.0.0 KiroIDE-{}-{}",
        config.kiro_version, machine_id
    );
    let (host, url) = usage_limits_endpoint(credentials, region);

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
    request
}

/// 官方 Kiro REST 接口（`getUsageLimits` / `ListAvailableModels`）**只在
/// `us-east-1` 与 `eu-central-1` 两个端点提供服务**。
///
/// # 为什么需要它（实测）
///
/// `management.{region}.kiro.dev` 与 `runtime.{region}.kiro.dev` 只有这两个区
/// 解析得到，其余 13 个区 DNS 直接不通。所以任何按凭据 SSO region 拼 host 的做法
/// 在 SSO region 不是这两个之一时（Enterprise / IAM Identity Center 常见）必然失败，
/// 上游回 `403 {"message":"Invalid token"}` —— 那个文案会让人误判成 token 坏了。
///
/// # 判据（对齐 ZyphrZero kiro.rs v0.7.1 的 `rest_api_region_candidates`）
///
/// 按 SSO region 前缀选主端点，另一个作 403 回退候选：
/// - `eu-central-1` 或任何 `eu-*` → 主 `eu-central-1`，回退 `us-east-1`
/// - 其余（含 `us-*` / `ap-*` / 空） → 主 `us-east-1`，回退 `eu-central-1`
///
/// 用前缀而非精确匹配：`eu-west-1` / `eu-north-1` 的账号虽然 REST 端点在
/// `eu-central-1`，但它们**是欧洲账号**，先试欧洲命中率更高。
///
/// ⚠️ 刻意**只有两个候选**。此前仓里三张 region 表（34 / 24 / 6 项）绝大多数是死项，
/// 每个多余候选都是一次白打的上游往返。
fn rest_api_region_candidates(sso_region: &str) -> [&'static str; 2] {
    let primary_eu = sso_region == "eu-central-1" || sso_region.starts_with("eu-");
    if primary_eu {
        ["eu-central-1", "us-east-1"]
    } else {
        ["us-east-1", "eu-central-1"]
    }
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
pub(crate) async fn validate_custom_api_base_url(base_raw: &str) -> anyhow::Result<()> {
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
        .header(
            "x-amz-target",
            "KiroControlPlaneBearerService.ListAvailableProfiles",
        )
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
        .and_then(|arr| {
            arr.iter()
                .find_map(|p| p.get("arn").and_then(|a| a.as_str()))
        })
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
/// per-credential 刷新锁的**等待上限**（秒）。见 `refresh_token_locked` 锁获取处的说明：
/// 超时按瞬态错误返回（请求换号/重试），不无限排队。取值远小于单次刷新的正常耗时
/// （网络往返 + 退避最坏 180s+），只在刷新异常卡死时兜底触发。
const REFRESH_LOCK_TIMEOUT_SECS: u64 = 60;

/// 全池自愈的**基础退避**（值已配置化到 [`Config::self_heal_base_backoff_secs`]，默认 60s）。
/// 第 n 次连续自愈需等 `BASE × 2^(n-1)`，上限 `self_heal_max_backoff_secs`（默认 900s）。
///
/// 🔴 修复的缺陷（线上实测 + 用户直接反馈「已经 403 封号了，不知道为什么一直被自动开启」）：
/// 自愈此前**没有任何退避** —— 只要选不出号且存在可自愈的禁用号就立刻复活全池。
/// 实测 41 分钟内触发 **36 次**（约每 68 秒一次）。
///
/// 危害不是"多试几次"，而是**加深封禁**：403 `temporarily is suspended` 是上游刚刚
/// 对该账号下的惩罚，每次复活都立刻再打一轮请求，等于持续撞同一面墙。
/// 线上观测到的 403 突发窗口约 10 分钟（当天两次，928 / 516 条），
/// 而 68 秒一次的复活相当于在一个窗口内重试约 9 遍。
///
/// 默认值取 60s 起、翻倍、上限 900s（15 分钟）的依据：让探测频率与真实窗口同量级 ——
/// 一个窗口内最多探两三次，而不是九次。任一号成功即清零 streak（见 `report_success`），
/// 所以真的恢复了会立刻回到灵敏状态，不会因为退避过而错过恢复。
///
/// 消费点每次在进 entries 锁**之前**读 config（热更下一个自愈周期即生效），
/// 详见 `acquire_context` 的自愈分支。

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
    url.push_str(&format!(
        "&profileArn={}",
        urlencoding::encode(candidate_arn)
    ));

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
        .header(
            "x-amz-target",
            "KiroControlPlaneBearerService.ListAvailableProfiles",
        )
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
    /// **代挂号专用**：历史遗留的上游失败计数槽。
    ///
    /// custom_api 是用户配置的第三方上游，不属于 Kiro 凭据健康体系。任何上游结果
    /// （429、5xx、认证失败、额度错误或账户状态）都不得据此自动禁用；字段只为保持
    /// 内存结构/旧测试构造兼容，不再参与健康判定。
    consecutive_passthrough_failures: u32,
    /// 上次被选号**真正选中**的时刻（`commit_selection` 里更新）。
    ///
    /// 用于 [`STARVATION_PROBE_SECS`] 的反饥饿强制探测。**不能用 `RpmTracker` 代替**：
    /// 它是 60s 滑窗，超窗即无数据，无法区分"61 秒没选中"与"20 分钟没选中"，
    /// 而饥饿自锁的典型时长是分钟级（实测 192 秒）。
    ///
    /// 非持久化：进程内状态。重启后视为"刚被选中"（乐观），至多延迟一个探测窗口。
    last_selected_at: std::cell::Cell<Instant>,
    /// 透传池专用：最近一次上游失败（5xx/429/401/403，[`Self::mark_passthrough_failure`]
    /// 写入）的时刻。供排序键「失败余温」降权位使用。
    ///
    /// 独立于冷却体系：`cooldown_custom_api` 被 `cooldown_enabled` 门控
    /// （线上 cooldownEnabled=false 时冷却完全不生效），死号（恒 502）靠本字段在
    /// 失败后 [`PASSTHROUGH_FAILURE_DECAY_SECS`] 内被降权，健康号优先被选，
    /// 根治「每请求先打死号白付一跳 + 延迟」；窗口过期后恢复平权（瞬态抖动
    /// 不误杀，真死号再次失败再次降权，每窗口至多白打一跳）。
    ///
    /// 非持久化：进程内状态，重启后清空（与 `last_selected_at` 同款乐观语义——
    /// 重启即视为无失败，至多多白打一跳后重新降权）。
    last_failure_at: std::cell::Cell<Option<Instant>>,
    /// 是否已禁用（**真源**：运行期一切禁用/恢复判定读本字段）。
    ///
    /// #10 双份契约：`credentials.disabled` 是持久化镜像——只在 `persist_credentials`
    /// 写盘时由本字段覆盖（加载时反向回填），运行期**不作为事实读**。三处同步：
    /// load 回填（`MultiTokenManager::new`）/ persist 全量写盘 / set_disabled 收口。
    disabled: bool,
    /// 禁用原因（用于区分手动禁用 vs 自动禁用，便于自愈）。**真源**，持久化镜像为
    /// `credentials.disabled_reason`（#10 双份契约同上，见 `disabled` 字段注释）。
    disabled_reason: Option<DisabledReason>,
    /// 被禁用的时刻（RFC3339）。**真源**，持久化镜像为 `credentials.disabled_at`
    /// （#10 双份契约同上），供运维判断"这号坏了多久"。
    disabled_at: Option<String>,
    /// 额度耗尽被禁用的时刻（RFC3339）。**真源**，持久化镜像为
    /// `credentials.quota_exhausted_at`（#10 双份契约同上）。
    ///
    /// 与 `disabled_at` 解耦：本字段记「额度耗尽判定」的时刻，跨月懒恢复
    /// （`recover_expired_quota_disables`）以它判月份——按 **UTC 自然月 + 12h 缓冲**
    /// 判定：当前月份 != 判定月份 **且** 当前时刻距当月 1 日 ≥ 12h 才恢复（上游
    /// 非 UTC 时区重置时至多延迟 12h 恢复，不会整月失效）。`None` = 旧版本数据
    /// （未持久化该字段），跨月恢复时视为可恢复，避免永久钉死。
    quota_exhausted_at: Option<String>,
    /// API 调用成功次数
    success_count: u64,
    /// 累计请求数（custom_api 的观测计数）。
    /// **持久化**进 kiro_stats.json（随 success_count 一起）；`request_limit` 仍可用于
    /// 面板展示/告警，但不能据此自动关闭代挂站。
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
    /// **族键缓存**（`family_key(id)` 的预计算结果）。
    ///
    /// # 为什么缓存（2026-08-14，选号临界区优化）
    ///
    /// `report_success` 的族级清零要对**全池**逐条算 `family_key`（每次成功一次
    /// 全池扫描，每次都是 String 分配 + issuer_url 解析 + clone_group 判定）——
    /// 共享族（`clone:` / `m365:` / `aws:`）下这是每成功一次 O(n) 分配扫描。
    /// 缓存后读侧变纯字段访问，分配归零。
    ///
    /// # 失效（惰性重算）
    ///
    /// `None` = 需要按当前 `credentials` 重算。族键只依赖 `auth_method` /
    /// `issuer_url` / `profile_arn` / `clone_group` 四个输入，其中后两个有运行期
    /// 写入点（`set_clone_identity` / region 纠正 / 刷新写回），那些变更点必须把
    /// 本字段置 None，否则缓存与凭据漂移（详见各变更点的注释）。
    family_key: Option<String>,
}

impl CredentialEntry {
    /// 读族键缓存；`None`（凭据变更点已置空）时按当前凭据惰性重算并回填。
    ///
    /// 需 `&mut self`：重算要写缓存字段。调用点都在 entries 锁内（iter_mut），
    /// 满足要求；选号排序键等只读点不调用本方法（那里按既有约定现算）。
    fn family_key_cached(&mut self) -> &str {
        if self.family_key.is_none() {
            self.family_key = Some(self.credentials.family_key(self.id));
        }
        self.family_key
            .as_deref()
            .expect("上面刚回填，必为 Some")
    }

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
    /// 与历史 `RequestLimitReached` 的 `request_count` 清零是同型 bug，
    /// 差别只是当时只想到了一个字段。收口后新增惩罚计数只需改这一个地方。
    ///
    /// # 不在此清零的东西
    ///
    /// `cooldown` / `rate_limiter` 是**独立锁下的旁挂结构**，不属于本 entry，
    /// 由调用方在 entries 锁外单独清（见 `reset_and_enable`）。
    /// `request_count` 只在恢复历史 `RequestLimitReached` 数据时才该清，由调用方判定。
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
    /// 历史版本曾在 custom_api 达到 `request_limit` 后写入此原因。
    /// 仅保留用于读取旧数据/回滚兼容；新版本绝不再据此自动禁用代挂站。
    RequestLimitReached,
    /// 历史版本曾因代挂站连续上游错误写入此原因。
    /// 仅保留用于读取旧数据/清理保护；新版本绝不再写入。
    PassthroughFailed,
    /// 历史版本曾因代挂站持续 429 写入此原因。
    /// 仅保留用于读取旧数据/清理保护；新版本绝不再写入。
    PassthroughOverloaded,
    /// 上号时 region 自动探测**未探到任何可用 region**（候选全部 403 / 无结论）。
    ///
    /// # 为什么必须是独立原因而不是复用 `TooManyFailures`
    ///
    /// 两条理由，缺一不可：
    ///
    /// 1. **不可自愈**。`is_self_healable_reason` 是白名单，本变体天然被排除 ⇒
    ///    自愈不会把它捞回池子。若复用 `TooManyFailures`，自愈每轮都会「重置失败计数
    ///    并重新启用」，把探不出 region 的号原样放回去重演一遍 —— 线上实测 24h 内
    ///    全池自愈 44 次、退避已升到第 5 级，正是这个形状。
    /// 2. **处置动作不同**。`TooManyFailures` 指「号可能被上游风控」（要查风控），
    ///    这条指「token 的 region 授权范围与我们探的候选不交叉」（要查号的来源区），
    ///    运维看原因就知道该查哪儿。
    ///
    /// 属"自动禁用"，人工确认 region 后可手动启用（手动启用不受白名单限制）。
    RegionProbeFailed,
    /// 上号时 region 探测发现 **token 本身已失效**（上游 401）。
    ///
    /// 与 [`Self::RegionProbeFailed`] 分开：那条是 region 不对（换区可能有救），
    /// 这条是凭据本身废了（换区无用，要重新取 token）。同样不在自愈白名单里。
    RegionProbeTokenDead,
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
            Self::RegionProbeFailed => "RegionProbeFailed",
            Self::RegionProbeTokenDead => "RegionProbeTokenDead",
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
/// - `QuotaExceeded`：额度耗尽，等自然月重置——但由 `recover_expired_quota_disables`
///   专门处理（跨自然月后自动恢复，不经过本白名单；当月复活只会白撞 402）
/// - `InvalidRefreshToken` / `InvalidConfig`：凭据本身坏了
/// - `RequestLimitReached` / `PassthroughFailed` / `PassthroughOverloaded`：仅为旧版代挂
///   禁用数据的兼容变体；新版不再写入，但也不应由 Kiro 自愈改写其历史状态
/// - `Manual`：人工禁用，绝不能被自动复活
/// - `Unknown`：读不懂的原因，保守不动
///
/// # `TooManyRefreshFailures` 为什么在列（2026-08-04 加入）
///
/// 它与 `InvalidRefreshToken` 是**两个不同的信号**，此前被一起排除是错的：
/// - `InvalidRefreshToken` = 上游明确回 `invalid_grant`，refreshToken 作废，复活只会白撞 → 排除。
/// - `TooManyRefreshFailures` = 连续 3 次刷新**没成功**，而 `refresh_token_locked` 内部
///   （`:6922` 一带）已经对 5xx/网络错误退避重试过 3 次才上报一次 → 走到阈值的典型成因是
///   **token 端点抖了几十秒**，凭据本身完好。
///
/// 旧行为的后果：一次 30s 的上游 token 端点抖动 → 3 次计数 → 禁用 + `persist_disabled_state`
/// 落盘 → 且因不在本函数覆盖范围内，**全池自愈也救不回来**，必须人工去面板点启用。
/// 该函数原注释（`:3683-3692`）已经写明了这个缺陷，只是当时判定"改它要单独一批"。
fn is_self_healable_reason(reason: Option<DisabledReason>) -> bool {
    matches!(
        reason,
        Some(DisabledReason::TooManyFailures)
            | Some(DisabledReason::SuspiciousActivityAuto)
            | Some(DisabledReason::TooManyRefreshFailures)
    )
}

/// 刷新失败是**瞬态**（不该计入永久失败计数）还是**凭据自身问题**（该计数）。
///
/// # 为什么需要这个判据
///
/// `report_refresh_failure` 的两个调用点（`:3674` 请求热路径、`:6863` 后台预刷新）此前
/// 对**一切**非 `invalid_grant` 的刷新错误都计数，3 次即 `TooManyRefreshFailures` 禁用。
/// 而 `refresh_token_locked` 内部已经对 5xx / 网络错误做过 3 次退避重试（1s/2s/4s），
/// 所以能走到上报的错误里，网络与上游 5xx 占绝大多数 —— 那是**上游/链路**的问题，
/// 号完全是好的。线上实测：健康号被这条路径反复打死。
///
/// # 判据方向：白名单式（默认瞬态）
///
/// 刻意与 `refresh_token_locked` 里那个**默认可重试**的判据（2026-08-15 起结构化：
/// `refresh_error_retryable`，仅按状态码排除永久类）反向：
/// 那里默认可重试、逐个排除永久码；这里默认瞬态、只认**明确的凭据级**信号才计数。
/// 两边都用「宁可少判一次永久」的方向，因为误判成永久 = 烧号（不可逆），
/// 误判成瞬态 = 多试几次（可逆，且真废的号会在别的路径拿到判决：
/// `invalid_grant` 走 `report_refresh_token_invalid`、上游 401/403 走对话路径）。
fn is_refresh_error_credential_level(err: &anyhow::Error) -> bool {
    let s = err.to_string();
    // 凭据级：token 端点明确拒绝了这个凭据（4xx）。注意 429 **不算** —— 那是限流，
    // 号是好的，计数它等于把上游拥堵记成号坏。
    s.contains("400")
        || s.contains("401")
        || s.contains("403")
        || s.contains("404")
        || s.contains("410")
        || s.contains("422")
        || s.contains("invalid_grant")
        || s.contains("invalid_client")
        || s.contains("unauthorized_client")
        // 结构性不可刷新（api_key 号没有 refreshToken）：重试永不可能成功，
        // 但也不该算"号坏"→ 由调用点单独处理，见 report_refresh_failure_classified。
        || s.contains("API Key 凭据不支持刷新")
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
    /// 是否豁免全局模型映射(None=false，即应用映射)
    pub model_mapping_exempt: Option<bool>,
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
    /// 分身组标识（同一次多开的全部份共享；单开为 None）
    pub clone_group: Option<String>,
    /// 组内序号（1-based，1 = 主份）
    pub clone_seq: Option<u32>,
    /// 分身标签（这一份的用途标记，与 name 是账号别名不同）
    pub tag: Option<String>,
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
    /// **实际生效**的上游 region（真正拼进 host 的那个值，恒有值）。
    ///
    /// 与 `endpoint`/`effective_endpoint` 完全同款语义：取
    /// `effective_upstream_region`，而非裸的 `api_region` 字段。`ksk_` 是按区授权的
    /// token，打错区恒 403，所以「这号在打哪个区」必须可见。
    pub effective_region: String,
    /// 该 region 是否被显式写死（凭据里有 `api_region`/`region`/`auth_region` 任一）。
    ///
    /// `false` = 现值来自 `config` 全局默认回退，即没人真的为这个号定过区。
    pub region_pinned: bool,
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

/// 同一把 `kiroApiKey`（⇒ 同一个上游账号）下的一个**其它**凭据。
///
/// 为什么判据必须是 key 而不是 `clone_group`：一个账号的成员里，**最先入池的那一份
/// 往往没有 `clone_group`**（它是普通上号进来的，组标识是后来加分身时才产生的）。
/// 按组去找同账号成员，恰好会漏掉这一份 —— 而它正是最需要被看见的那一份
/// （实测线上 `#776` 无组无代理，`#778–787` 同 key 同组各有独立 SOCKS）。
#[derive(Debug, Clone)]
pub struct SameKeyPeer {
    pub id: u64,
    /// 它现在的分身组（`None` = 早于本字段入池，等着被回填）。
    pub clone_group: Option<String>,
    /// 它现在的组内序号。回填组标识时必须原样带回来，否则会把序号抹成 `None`。
    pub clone_seq: Option<u32>,
    /// 它有没有**自己的**出口 IP。
    ///
    /// `None` / 空串（回退全局代理）与 `"direct"`（显式不走代理）都算**没有** ——
    /// 这里问的是「实际从哪个 IP 出去」，两者都是服务器自身那个出口，
    /// 与同 key 其它份共用账号时的风控暴露面完全相同。意图上的差别由调用方
    /// 在文案里说明（本仓不因此擅自改配置）。
    pub has_own_exit: bool,
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
/// 号池全灭告警去抖门（B8）：窗口内连续 [`POOL_EXHAUST_ALERT_THRESHOLD`] 次
/// 「无候选」才触发 bump("pool_exhausted")。
///
/// # 为什么需要这个门
///
/// alerting 的冷却只防「同 key 重复发送」，不防「不值得告警的偶发失败也被
/// 累计进 value」。单请求抖动（一次换号失败）在 failover 循环里会每跳命中
/// 无候选出口——直接 bump 的话一次小抖动就把计数打高、噪音淹没真告警。
/// 窗口语义：30s 内连续 3 次无候选才算「池子真的空了」，单次偶发不告警。
#[derive(Debug)]
struct PoolExhaustionGate {
    count: u32,
    window_start: Instant,
}

/// 窗口时长（秒）：窗口过期则计数重置。
const POOL_EXHAUST_ALERT_WINDOW_SECS: u64 = 30;
/// 窗口内触发阈值：连续 3 次无候选才告警。
const POOL_EXHAUST_ALERT_THRESHOLD: u32 = 3;

impl Default for PoolExhaustionGate {
    fn default() -> Self {
        Self {
            count: 0,
            window_start: Instant::now(),
        }
    }
}

impl PoolExhaustionGate {
    /// 记录一次「无候选」。窗口过期则重置计数并重开窗口。返回是否达到阈值
    /// （达到即该告警；告警方负责调用 [`Self::reset`]）。
    fn record(&mut self, now: Instant) -> bool {
        if now.duration_since(self.window_start)
            > StdDuration::from_secs(POOL_EXHAUST_ALERT_WINDOW_SECS)
        {
            self.count = 0;
            self.window_start = now;
        }
        self.count += 1;
        self.count >= POOL_EXHAUST_ALERT_THRESHOLD
    }

    /// 告警已发：清零，避免同一窗口内反复触发（alerting 冷却兜底第二道）。
    fn reset(&mut self) {
        self.count = 0;
    }
}

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
    ///
    /// 2026-08-13 起 persist 恒写数组格式，本字段不再参与持久化判定
    /// （见 persist_credentials 注释「参数保留仅为向后兼容」）——保留字段
    /// 仅维持 new() 签名与调用点不变，release 下是死字段。
    #[allow(dead_code)]
    is_multiple_format: bool,
    /// 负载均衡模式（运行时可修改）
    load_balancing_mode: Mutex<String>,
    /// 最近一次统计持久化时间（用于 debounce）
    last_stats_save_at: Mutex<Option<Instant>>,
    /// 最近一次**全池自愈**的时刻（用于退避，见 `Config::self_heal_base_backoff_secs`）
    last_self_heal_at: Mutex<Option<Instant>>,
    /// 连续自愈次数（未被成功打断）。驱动指数退避，见 [`Config::self_heal_base_backoff_secs`]。
    ///
    /// 清零判据是「**最近一次自愈复活的号**成功了」，不是「任意号成功了」——
    /// 见 `self_heal_revived` 与 `report_success` 的说明。
    self_heal_streak: AtomicU32,
    /// 最近一次全池自愈**复活了哪些号**（每次自愈覆盖，只保留最近一批）。
    ///
    /// # 为什么需要它（实测：指数退避从未生效过）
    ///
    /// `self_heal_streak` 的语义是「连续自愈未被成功打断」，而清零判据原先是
    /// **任意凭据成功**。这在设计时假设了「全池被自动禁用」与「有成功」互斥，
    /// 但实际不互斥：部分号被禁 → 自愈复活 → 少量成功 → 再被禁，两者持续交织。
    ///
    /// 线上池子成功率 99.7%，于是 streak 每次自增后立刻被清回 0 ⇒
    /// `wait` 恒为 `BASE × 2^0` = **60s** ⇒ 死号每 60 秒被复活一次，
    /// 而退避本该涨到 120/240/480/900s。实测日志分布坐实：`执行自愈` 间隔全部聚集在
    /// 恰好 60.0s，`连续第 N 次` 有 70 次落在 N=1、仅 1 次到 N=5。
    ///
    /// # 为什么不能改成「从不清零」
    ///
    /// 那是这条判据原本要修的缺陷：streak 只增不减会让退避爬到 900s 上限并**永远停在
    /// 那里**，即使号池早已恢复。`report_success` 的注释点名这是本仓反复出现的
    /// 「单向棘轮」形态（见 `health.rs` 的 decay_penalties 那段历史）。两个方向都是真缺陷，
    /// 修法必须在它们之间穿过。
    ///
    /// # 收窄后的判据
    ///
    /// 只有**被这次自愈复活的号**成功，才证明这次复活真起了作用、才该把 streak 打断。
    /// 一个从未被禁用的健康号成功，说不出「复活有效」——而正是它在持续清零。
    self_heal_revived: Mutex<std::collections::HashSet<u64>>,
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
    /// #9 合并后为**两池共用一张表**：Kiro 主路径（`report_model_invalid`，
    /// 上游返 `INVALID_MODEL_ID`）与 custom_api 透传路径（`mark_model_unsupported`，
    /// 上游返 model_not_found / no available channel）写同一张表、查同一张表
    /// （`is_model_blocked` / `is_model_blacklisted` 同实现）。语义相同——「该号
    /// 确定性不支持该模型，TTL 内跳过该号×该模型组合」；一个号只属于一个池，
    /// key 空间不冲突。TTL 统一 `MODEL_BLOCK_TTL`（1800s）。
    ///
    /// 上游对某号返回 `INVALID_MODEL_ID` 时，只记"这个号 + 这个模型"不可用（短 TTL），
    /// 选号时**仅对该模型**跳过它，该号对其它模型照常参与调度。这修正了 v0.6.0 的致命
    /// 设计缺陷：此前把 INVALID_MODEL_ID 当"整个号坏了"冷却/自动禁用，导致一个客户端请求
    /// 一个订阅不含的模型就能把能正常服务其它模型的号（乃至整池）全部打下线。
    model_blocklist: Mutex<HashMap<(u64, String), Instant>>,
    /// 模型目录缓存（模型感知正向路由，S1）：key = credential id。
    ///
    /// 只写 custom_api 号（巡检只探这些号）；ksk 号无目录概念恒 Unknown
    /// （`select_custom_api_inner` 只处理 custom_api）。写入**唯一来源**是巡检成功
    /// （设计文档 §2：负向证据走 `model_blocklist` 黑名单，双通道互不重复）。
    /// TTL 见 [`MODEL_CATALOG_TTL`]；空列表不写（保持 Unknown）。过期的查询时
    /// 惰性判 Unknown，由巡检任务下轮覆盖写。
    model_catalog_cache: Mutex<HashMap<u64, ModelCatalogEntry>>,
    /// 巡检单飞锁：per-id TokioMutex（同一凭据不并发 fetch；换上游/删号时移除）。
    model_catalog_locks: Mutex<HashMap<u64, Arc<TokioMutex<()>>>>,
    /// 巡检失败退避：key = credential id，值见 [`CatalogBackoff`]。
    model_catalog_backoff: Mutex<HashMap<u64, CatalogBackoff>>,
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
    /// 🔴 **连续「全池不可用」次数**（进程级，成功即清零）。2026-08-10 对抗评审后新增。
    ///
    /// # 它解决的问题
    /// 纯 custom_api 代挂池下，`available` 恒 0（选号已排除代挂号）而 `any_healable` 恒 true
    /// （只要有未禁用的代挂号就算"等一会儿会好"）⇒ 那条 `retry_after_secs=` 的 429
    /// **永远可重试** ⇒ 上游**真·永久坏**时（余额耗尽返 402/403），客户端无限收
    /// 429+Retry-After:10 却永远拿不到终态。
    ///
    /// # 为什么用计数器而不是把号标 `disabled`
    /// 「透传失败绝不 auto-disable 号」是**有实测依据的刻意设计**
    /// （见 `record_passthrough_result`：健康号 #216 曾被误禁 119 次）。
    /// 所以这里只在**进程内**数「连续多少轮全池都不可用」，不碰任何持久化状态：
    /// 超过阈值就给错误串补上 `pool_permanently_exhausted=1`，让吸收层停止吸收、
    /// 客户端拿到终态；任何一次成功选号立刻清零，中转站恢复后自动回到可重试语义。
    consecutive_pool_unavailable: std::sync::atomic::AtomicU32,
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
    /// 每个分身组**已发放**的最大 `clone_seq` 高水位（key = clone_group UUID）。
    ///
    /// # 为什么不能只靠扫 entries 现算 max
    ///
    /// 发号与入池之间隔着 `.await`（`add_credential_inner` 要走网络/写盘，copies 循环
    /// 里每份还要再 await 一次）。两个并发的「给同一个 key 加分身」请求会在这些 await
    /// 上交错：A 扫到 max=0、还没写进 entries，B 也扫到 max=0 → 两边都从 1 开始编号
    /// → 同一组里出现两个 `分身 #2`，管理页无法区分、删除时无法指名。
    ///
    /// 高水位在**发号那一刻**就前进（`reserve_clone_seqs`，见其文档），故发出去的号段
    /// 天然互不重叠，与入池是否已完成无关。与 `socks_next_id` 同款思路，只是这里要按组
    /// 分别记账（序号是组内序号，不是全局序号）。
    ///
    /// 重启后该表为空，`reserve_clone_seqs` 会用 entries 里的既有 seq 补齐地板，
    /// 所以不需要持久化（entries 本身已经持久化了）。
    clone_seq_hwm: Mutex<HashMap<String, u32>>,
    /// 全池冷却兜底放行（[`Self::select_ignoring_cooldown`]）的**轮转游标**：上次放行的 id。
    ///
    /// # 为什么需要它（实测）
    ///
    /// 兜底原先只按「冷却剩余最短」排序，**完全确定性**：冷却到期时刻一旦排定就不再变，
    /// 于是同一个号被反复选中 —— 实测 #578 近 3 小时拿到 128 次兜底放行、单分钟峰值 63。
    /// 兜底的用意是「拿真实上游 429 好过网关自造 429」，把它全压在一个号上等于
    /// 专挑一个号去打爆，而池里其它同样在冷却的号一次都不试。
    ///
    /// # 为什么是游标而不是随机
    ///
    /// 本仓曾有个随机打散键 `tie_break_jitter`，**已被删除**，理由是不可复现
    /// （见排序键里 `starved` 上方那段历史注记）。轮转游标同样能打散、且完全确定：
    /// 给定 (候选集, 游标) 结果唯一 ⇒ 可写测试、线上可复盘。
    ///
    /// 用 `AtomicU64` 而非放进 `entries` 锁内：它只是个提示值，读到旧值最坏是多轮一次。
    /// 初值 0 = 「还没放行过」，而 id 从 1 起分配（`next_id`），故 0 不与任何号撞。
    fallback_cursor: AtomicU64,
    /// 号池全灭告警去抖门（B8）：窗口内连续 N 次「无候选」才 bump pool_exhausted。
    pool_exhaust_gate: Mutex<PoolExhaustionGate>,
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

/// 爬坡压力判定常量（`RAMP_RECENT_SECS` / `RAMP_MIN_SAMPLES`）与分档函数
/// `ramp_tier_of` 已收敛到 `crate::kiro::scheduling`（单一真相源，主路径与
/// 透传池共用，改档位/窗口折算只动那里）。

/// 透传池「失败余温」窗口（秒）：近此窗口内上游失败（5xx/429/401/403，
/// `mark_passthrough_failure` 写入）过的号在排序键被降权，健康号优先被选；
/// 窗口过期后恢复平权（瞬态抖动只降权一轮，不误杀；真死号再次失败再次降权，
/// 每窗口至多白打一跳）。
///
/// ## 为什么独立于冷却体系（N1 根治的关键）
///
/// `cooldown_custom_api` 被 `cooldown_enabled` 门控——线上 cooldownEnabled=false
/// （config.rs 默认 true，但线上配置显式关掉）时整条冷却体系不生效，死号
/// （如恒 502 的 #3 cursorapi）永远留在候选里且每请求都被重新选中（低负载时
/// RPM 滑窗归零，排序键前 5 键全平局，min_by_key 恒选 Vec 头部 = 死号），
/// 白打一跳才 failover。本窗口**只进排序键**，不受任何开关门控，是
/// `cooldownEnabled=false` 下唯一的跨请求失败记忆。
///
/// 60s 与 RPM 滑窗同量级：与「请求间隔 >60s 时 RPM 归零」的线上低负载形态一致，
/// 窗口过期即恢复探测能力——死号复活不被埋没。
const PASSTHROUGH_FAILURE_DECAY_SECS: u64 = 60;

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
/// 上游 `Retry-After` / `resets_in_seconds` 能把一个号压进多长的短冷却（秒，钳制上限）。
///
/// 超过这个数的「上游指定重置」不该塞进短冷却（那类是月度配额，应走配额耗尽禁用），
/// 所以它同时也是**短冷却类冷却剩余的事实上界**。
/// 两个用处：[`MultiTokenManager::report_rate_limited_with_retry_after`] 用它钳制，
/// [`MultiTokenManager::select_ignoring_cooldown`] 用它划「浅冷却 / 深冷却」那一刀。
const MAX_RETRY_AFTER_COOLDOWN_SECS: u64 = 600;

/// 全池冷却兜底放行时的**冷却深度档**（越小越优先），由 `check_cooldown` 的
/// `(reason, remaining)` 判定。见 [`MultiTokenManager::select_ignoring_cooldown`]。
///
/// ## 为什么是「语义分类」而不是「剩余秒数 / 档宽」（2026-08-06 改）
///
/// 上一版按 `remaining / 60` 分档，理由是「所有会自愈的原因基线都 ≤ 60s」。
/// 那个前提**在真实链路上不成立**，于是聚集形态只被部分消除：
///
/// - 429 带 `Retry-After` 时冷却时长**由上游给**（`provider.rs:1770` 取头/body，
///   本文件钳到 600s 上界），一个号拿 90s、另一个裸 429 拿 15s 是常态；
/// - `SuspiciousActivity` 是 `20 × 1.6^(n-1)` 且不受 90s 短冷却上限钳制
///   （`cooldown.rs` 的 `SUSPICIOUS_MAX_SECS`），第 4 次触发就到 ~81s。
///
/// 两种情况都让「会自愈的号」跨到不同档，而档位是排序键第一维 ⇒ 只剩一个号在第 0 档时
/// 它被**重新钉住**，且它每被放行一次就重新拿一段短冷却、继续留在第 0 档（自我维持），
/// 而其余号只有在剩余衰减到同档时才偶尔轮到一次 —— 与旧实现同症状
/// （实测 #578 近 3h 拿 128 次、单分钟峰值 63）。
///
/// 现在只分三档，档内一律靠 id 轮转打散：
/// 1. `Ready` —— 无冷却记录或剩余已归零（竞态：刚到期还没被清理）。最可能真的能用。
/// 2. `Shallow` —— 原因可自动恢复**且**剩余 ≤ [`MAX_RETRY_AFTER_COOLDOWN_SECS`]。
///    这一档里谁先恢复不重要（兜底本就预期吃一个真实 429），**摊开打才重要**，
///    所以不再按剩余细分 —— 那正是钉住单号的那一维。
/// 3. `Deep` —— 不可自动恢复（`AuthenticationFailed`/`AccountSuspended`/`QuotaExhausted`，
///    实际 86400s），或上游明确要求等超过上界。必须沉底：把请求送给这类号是白扔一次往返。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum FallbackCooldownTier {
    Ready = 0,
    Shallow = 1,
    Deep = 2,
}

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

/// 每凭据最大并发（in-flight 上限）硬门（迁移差距 P1，对齐 kiro-rs-admin v0.9.55
/// 的每账号 max_concurrency）。
///
/// 与 [`InflightGuard`] 的关系：guard 是「持有期标记」（选中 +1、流真正结束 -1，
/// 见 `scheduling.rs`），本常量是「硬门」——`inflight >= 上限` 的号在选号时被跳过
/// （不可选）。二者互补：guard 保证计数准确，硬门保证单号不被灌爆（上游风控）。
/// 与 RPM 饱和也互补：RPM 限「速率」，本门限「瞬时并发」。
///
/// 取 16 的理由：线上每号常态在途约 8.6（6000 RPM / 200 号的实测参考值），16 是
/// 常态的两倍、正常并发形态远够不到；只有单号被异常灌爆（慢流堆积/亲和钉死）时才
/// 触发。历史教训（`is_entry_selectable_inner` 的旧注释）：硬门设 1 会把「每号同时
/// 只 1 个请求」变成假性限流；16 是真正的饱和级护栏，不是常态阻塞。
///
/// ⚠️ 每凭据覆盖暂未落地（`KiroCredentials` 在 `credentials.rs`，不在本次改动范围）：
/// 先以全局默认值起步；需要 per-cred 覆盖时给凭据加字段并在选号判据
/// （`at_max_concurrency`）里改为优先读该字段（0 = 不限，镜像参考仓语义）。
pub const CREDENTIAL_MAX_CONCURRENCY: u32 = 16;

/// 该凭据是否已达并发上限（硬门判据，镜像 kiro-rs-admin 的 `is_concurrency_exceeded`）。
///
/// 调用约定：**必须在持 `entries` 锁时调用**——选号路径的 check 与 acquire 在同一
/// 临界区内完成（`select_custom_api_inner` / `commit_selection`），check-then-acquire
/// 才原子；锁外调用会有 check 通过后 inflight 已被他人 +1 越过上限的竞态。
fn at_max_concurrency(entry: &CredentialEntry) -> bool {
    entry.inflight.load(Ordering::Acquire) >= CREDENTIAL_MAX_CONCURRENCY
}

/// 全池无立即可用候选时,一个候选为何在等待——决定调用方终态处理与文案类别。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WaitReason {
    /// 冷却/风控/速率限制:典型秒~分钟级,长时应 fast-fail 让客户端退避。
    Cooling,
    /// RPM 饱和,滑窗过期即恢复(L4 背压):属"限流/繁忙"可重试类别,绝不报"已禁用"。
    RpmRecovery,
    /// 并发上限硬门（[`CREDENTIAL_MAX_CONCURRENCY`]）：在飞请求连续释放（流结束即 -1），
    /// 短固定等待后重选即大概率命中；属"繁忙"可重试类别，与 RpmRecovery 同族。
    ConcurrencyFull,
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

/// 模型级不支持黑名单的 TTL（#9 合并后唯一常量）：某号对某模型返回 INVALID_MODEL_ID
/// （Kiro 路径）/ 上游明确说「不支持该模型」（custom_api 路径 model_not_found / no
/// available channel）后，这段时间内选号跳过「该号+该模型」组合。
///
/// 取中长窗 30min（2026-08-14 初版 60s；2026-08-15 对齐 sub2api 的 30min 严格语义）：
/// 「上游确定性说该模型不支持」是**稳定属性**（不是抖动），60s 冷却只够挡 failover 链，
/// 下一个请求 60s 后照样撞。30min 内调度器跳过该 (号, 模型) 对；上游模型分组调整后
/// 30min 自动解禁，不需要人工干预。
const MODEL_BLOCK_TTL: StdDuration = StdDuration::from_secs(1800);

/// 模型支持三态（模型感知正向路由，S1，zyphr `cached_model_support` 同款）：
/// - `Confirmed`：巡检目录含目标模型（改写后名，大小写不敏感）；
/// - `Unknown`：无目录 / 目录过期 / 不巡检的号 —— 中性，排序与无缓存时完全一致；
/// - `Unsupported`：目录明确不含 —— 软降权（仅排序压后，**绝不出局**）。
///
/// 与 `model_blocklist`（黑名单）的分工：黑名单是运行时负向证据（filter 硬门出局），
/// 本三态是离线正向证据（排序软偏好），判定先后天然隔离。设计见
/// docs/model-forward-routing-design.md §2-§3。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelSupport {
    Confirmed,
    Unknown,
    Unsupported,
}

/// 单凭据的模型目录缓存条目（per-id 目录形态，设计文档 §2）：上游 `/models` 拉回的
/// 模型名列表（已排序去重）+ 刷新时刻。目录形态优于 per-(id, model) 状态表：
/// 条目数 = 号数而非号数 × 模型数，TTL 语义自然落在目录上，判定是线性扫描。
struct ModelCatalogEntry {
    models: Vec<String>,
    refreshed_at: Instant,
}

/// 巡检失败退避状态（指数，见 `MODEL_CATALOG_BACKOFF_*`）。
struct CatalogBackoff {
    /// 连续失败次数（成功即重置）
    failures: u32,
    /// 退避到期时刻（在此之前跳过巡检；该号维持 Unknown = 排序中性）
    until: Instant,
}

/// 模型目录缓存 TTL（30min，与 [`MODEL_BLOCK_TTL`] 同量级——负向证据与正向证据
/// 同一刷新尺度，注释互相引用）。过期条目在查询时惰性判 Unknown，由巡检任务
/// 下轮覆盖写（设计文档 §2）。
const MODEL_CATALOG_TTL_SECS: u64 = 30 * 60;
const MODEL_CATALOG_TTL: StdDuration = StdDuration::from_secs(MODEL_CATALOG_TTL_SECS);
/// 巡检失败退避：第 n 次连续失败等 `BASE × 2^(n-1)`，上限 [`MODEL_CATALOG_BACKOFF_MAX_SECS`]。
/// 退避只影响巡检节奏（该号维持 Unknown），不进排序判定（设计文档 §5）。
const MODEL_CATALOG_BACKOFF_BASE_SECS: u64 = 60;
const MODEL_CATALOG_BACKOFF_MAX_SECS: u64 = 30 * 60;
/// 退避指数封顶：`2^6 × 60 = 3840s` 已超上限，更大指数无意义（防移位溢出）。
const MODEL_CATALOG_BACKOFF_MAX_SHIFT: u32 = 6;
/// 巡检任务启动延迟（region 回填同款：避开启动期 token 预刷新抢上游往返）。
const MODEL_CATALOG_PROBE_START_DELAY_SECS: u64 = 10;

/// 纯判定（无状态，可单测）：目录含目标（大小写不敏感）→ `Confirmed`，否则
/// `Unsupported`。空目录不会到这一步——查询无条目时在 `model_support` 短路为
/// `Unknown`（空列表不写缓存，设计文档 §2 第 4 点）。
fn support_for(target: &str, models: &[String]) -> ModelSupport {
    if models.iter().any(|m| m.eq_ignore_ascii_case(target)) {
        ModelSupport::Confirmed
    } else {
        ModelSupport::Unsupported
    }
}

/// 测试用压力次数。历史版本曾把此值用作 custom_api 自动禁用阈值；生产逻辑已移除。
// 用 `cfg(any(test))` 是因为本文件的源码守卫以测试模块属性作为生产代码截止点。
#[cfg(any(test))]
const MAX_PASSTHROUGH_FAILURES: u32 = 3;

/// region 探测只属于 Kiro API Key 凭据。custom_api 即使旧数据里同时带了
/// `kiro_api_key`，也不得进入 Kiro 的 token / region / 自动禁用链。
fn needs_api_region_probe(credentials: &KiroCredentials) -> bool {
    !credentials.is_custom_api_credential()
        && credentials.is_api_key_credential()
        && credentials.region.is_none()
        && credentials.auth_region.is_none()
        && credentials.api_region.is_none()
}

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
        // 冷却状态持久化目录（凭据文件所在目录）——必须先算，credentials_path 稍后
        // 会被 move 进 Self。⚠️ 无路径时**保持纯内存**（new()）：测试环境全部
        // credentials_path=None，若统一落 "." 会让并行测试共享同一冷却文件互相污染
        // （全量跑 17 个选号测试随机红，2026-08-13 实测）。
        let cooldown_dir = credentials_path.as_deref().and_then(|p| p.parent()).map(|d| d.to_path_buf());
        // 计算当前最大 ID，为没有 ID 的凭据分配新 ID
        let max_existing_id = credentials.iter().filter_map(|c| c.id).max().unwrap_or(0);
        let mut next_id = max_existing_id + 1;
        let mut has_new_ids = false;
        let mut has_new_machine_ids = false;
        let mut has_reenabled_custom_api = false;
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
                    last_selected_at: std::cell::Cell::new(Instant::now()),
                last_failure_at: std::cell::Cell::new(None),
                    // #10 三处同步契约之「load 回填」：持久化四件套（disabled /
                    // disabled_reason / disabled_at / quota_exhausted_at）从 credentials
                    // 镜像回填进 entry 真源；另两处同步是 persist 全量写盘与 set_disabled 收口。
                    disabled: cred.disabled, // 从配置文件读取 disabled 状态
                    // ⭐ 优先用持久化的真实原因；只有旧文件（无该字段）才回落 Manual。
                    // 旧代码无条件回落 Manual，导致自动禁用原因重启即丢失（见 DisabledReason 说明）。
                    disabled_reason: if cred.disabled {
                        Some(cred.disabled_reason.unwrap_or(DisabledReason::Manual))
                    } else {
                        None
                    },
                    disabled_at: cred.disabled_at.clone(),
                    quota_exhausted_at: cred.quota_exhausted_at.clone(),
                    success_count: 0,
                    request_count: 0,
                    total_credits_used: 0.0,
                    last_used_at: None,
                    inflight: Arc::new(AtomicU32::new(0)),
                    last_usage_403_feature_not_supported: AtomicBool::new(false),
                    last_full_reprobe_at: Mutex::new(None),
                    reprobe_in_flight: AtomicBool::new(false),
                    refresh_lock: Arc::new(TokioMutex::new(())),
                    // 族键缓存：构造时即按当前凭据预计算（auth_method 已 canonicalize）。
                    family_key: Some(cred.family_key(id)),
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

        // 一次性迁移旧版留下的 custom_api 自动禁用状态。
        //
        // 新契约是「代挂站只接受管理员手动开关，任何上游/Kiro 判据都不能自动关闭」。只删除
        // 写入逻辑仍不够：旧二进制已经落盘的 PassthroughFailed / RequestLimitReached，甚至误入
        // Kiro 路径后写下的 TooManyFailures 等，会在升级后永久保持 disabled，表现得像修复没生效。
        //
        // `Manual` 必须原样保留；旧文件缺原因时加载阶段也已回落成 Manual。`Unknown` 保守不动，
        // 避免擅自改写来自更新版本、当前无法理解的管理员状态。其余当前可识别原因都是自动写入。
        for entry in &mut entries {
            let legacy_auto_disabled = entry.credentials.is_custom_api_credential()
                && entry.disabled
                && !matches!(
                    entry.disabled_reason,
                    None | Some(DisabledReason::Manual | DisabledReason::Unknown)
                );
            if legacy_auto_disabled {
                let old_reason = entry.disabled_reason;
                entry.disabled = false;
                entry.disabled_reason = None;
                entry.disabled_at = None;
                // persist_credentials 从 credentials 副本序列化，故两份权威状态必须同步改。
                entry.credentials.disabled = false;
                entry.credentials.disabled_reason = None;
                entry.credentials.disabled_at = None;
                has_reenabled_custom_api = true;
                tracing::warn!(
                    credential_id = entry.id,
                    ?old_reason,
                    "已解除旧版本对 custom_api 的自动禁用；管理员手动禁用不受影响"
                );
            }
        }

        // 校验 Kiro API Key 凭据配置完整性：authMethod=api_key 时必须提供 kiroApiKey。
        // 旧版代挂数据可能是 `authMethod=api_key + baseUrl`，它仍由
        // `is_custom_api_credential` 判为代挂站，绝不得在启动时被 InvalidConfig 自动关闭。
        for entry in &mut entries {
            if !entry.credentials.is_custom_api_credential()
                && entry.credentials.kiro_api_key.is_none()
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
            // 首次自愈不需要等待（None 即"从未自愈过"）；streak 从 0 起。
            last_self_heal_at: Mutex::new(None),
            self_heal_revived: Mutex::new(std::collections::HashSet::new()),
            self_heal_streak: AtomicU32::new(0),
            stats_dirty: AtomicBool::new(false),
            // 2026-08-13：冷却状态持久化（风控退避档位重启清零 = 烧号反向放大器）。
            // 数据目录 = 凭据文件所在目录（cache_dir 同源）；无路径时纯内存（与旧行为一致）。
            cooldown: match &cooldown_dir {
                Some(dir) => CooldownManager::with_data_dir(dir),
                None => CooldownManager::new(),
            },
            cooldown_enabled: AtomicBool::new(cooldown_enabled),
            throttle,
            rate_limiter: RateLimiter::new(rate_limit_config),
            rate_limit_enabled: AtomicBool::new(rate_limit_enabled),
            affinity: UserAffinityManager::new(),
            model_blocklist: Mutex::new(HashMap::new()),
            model_catalog_cache: Mutex::new(HashMap::new()),
            model_catalog_locks: Mutex::new(HashMap::new()),
            model_catalog_backoff: Mutex::new(HashMap::new()),
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
            // 连续全池不可用计数（进程级，不落盘；成功即清零）
            consecutive_pool_unavailable: std::sync::atomic::AtomicU32::new(0),
            auto_disable_suspicious: AtomicBool::new(auto_disable_suspicious),
            priority_in_balanced: AtomicBool::new(priority_in_balanced),
            balance_weight_enabled: AtomicBool::new(balance_weight_enabled),
            balance_weight_floor: AtomicU32::new(balance_weight_floor),
            balance_snapshots: RwLock::new(HashMap::new()),
            refresh_task: Mutex::new(None),
            // 空表即"本进程还没发过号"；地板由 reserve_clone_seqs 从 entries 现补。
            clone_seq_hwm: Mutex::new(HashMap::new()),
            // 0 = 还没兜底放行过；id 从 1 起，故 0 不与任何号撞。
            fallback_cursor: AtomicU64::new(0),
            pool_exhaust_gate: Mutex::new(PoolExhaustionGate::default()),
        };
        // 播种冷却时长缩放(启动即用 config 值)。
        manager.cooldown.set_cooldown_scale_pct(cooldown_scale_pct);

        // 补全字段或迁移旧版 custom_api 自动禁用后，立即写回配置文件。
        if has_new_ids || has_new_machine_ids || has_reenabled_custom_api {
            if let Err(e) = manager.persist_credentials() {
                tracing::warn!("补全/迁移凭据状态后持久化失败: {}", e);
            } else {
                tracing::info!("已补全/迁移凭据状态并写回配置文件");
            }
        }

        // 加载持久化的统计数据（success_count, last_used_at）
        manager.load_stats();

        // 懒恢复（触发点一：启动）——跨自然月后自动恢复因额度耗尽被禁用的凭据
        // （对齐 k2cc 启动路径的做法；quota_exhausted_at 由持久化字段读回）。
        manager.recover_expired_quota_disables(None);

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
            // ⭐ `default_endpoint` 与上面十项同款：`admin/service.rs` 已把它 push 进
            // `restart_fields`（改它要求重启），但它此前漏出这张 restore 表 ⇒ reload 后
            // ArcSwap 拿到**新**值，而 `KiroProvider` 仍持构造时（`main.rs`）传入的拷贝
            // （`provider.rs` 的 `default_endpoint` 字段 + `fn endpoint_for`）。
            // 于是只要同批改了任何热字段触发 reload：**对话路径走旧端点，而
            // `region_probe.rs` 的探测、`token_manager.rs` 的余额/验活路径（活读
            // `config().default_endpoint`）走新端点** = 与 proxy 完全同型的 split-brain，
            // 持续到重启。两个端点按凭据类型绑定、不可互换（打错恒 403）。
            new.default_endpoint = old.default_endpoint.clone();
            // ⭐ 三个版本串与 default_endpoint 同型，但影响面更大：`endpoint/ide.rs` 的
            // `ctx.config` 是每请求从 ArcSwap 取的 `Arc<Config>` ⇒ 它们是**活读**的。
            // 它们在 `restart_fields` 里（面板说「改了要重启」）却此前漏出本表 ⇒ 同批改任何
            // 热字段触发 reload 后，**对话路径立刻发新版本串**，与面板承诺矛盾。
            //
            // 为什么这条比"不一致"更严重：这三个串是**上游请求指纹**的组成部分，且与
            // `machineId` 配对下发。指纹在飞行中途变化正是风控关注的形态 —— 而每个 IDE
            // 协议请求都带它们。
            new.kiro_version = old.kiro_version.clone();
            new.system_version = old.system_version.clone();
            new.node_version = old.node_version.clone();
            // ⭐ 反代安全五件套与上面同款：`admin/service.rs` 把它们 push 进 `restart_fields`
            // （面板说「改了要重启」），消费点全部**构造时固化**（CORS layer / DefaultBodyLimit
            // 在 router 构造、IP 白名单/入口限流在 security 中间件构造、trust_forwarded_header
            // 在 main.rs 启动装配镜像），ArcSwap 里它们只供面板快照展示。此前漏出本表 ⇒ 同批
            // 改任何热字段触发 reload 后，ArcSwap 拿到磁盘新值而运行态仍是启动旧值 = 快照说谎
            // （面板显示「已改」实际未生效，与 proxy split-brain 同型，2026-08-15 清单 #3）。
            new.cors_allowed_origins = old.cors_allowed_origins.clone();
            new.ip_allowlist = old.ip_allowlist.clone();
            new.trust_forwarded_header = old.trust_forwarded_header;
            new.ingress_rate_limit_per_min = old.ingress_rate_limit_per_min;
            new.max_body_bytes = old.max_body_bytes;
            // ⭐ `ota_auto_check` 与上面同款：`admin/service.rs` 已把它 push 进 `restart_fields`
            // （面板说「改了要重启」），消费点是 main.rs 启动期一次性 spawn 检查任务，无热更
            // 机制。漏出本表 ⇒ 同批改任何热字段触发 reload 后，ArcSwap 拿到磁盘新值而检查
            // 任务仍是启动时的旧开关 = 快照说谎（与反代安全五件套同型）。
            new.ota_auto_check = old.ota_auto_check;
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
        // 错误码/提示词覆盖表（TIER1 表 + handlers 进程镜像）：reload 后用新表改写
        // 镜像，错误翻译处下个请求即读到（main 启动播种 + 此处热更 = 两个入口都齐）。
        //
        // ⚠️ 调用环标注（结构绊脚石 #16）：本行是 kiro 层 → anthropic 层的**反向引用**。
        // 环的另一半（anthropic → kiro）大量存在（handlers → provider 主方向 + provider
        // 对 AbsorbClass 的类型引用），故本环无法归零；依赖方向纪律 = 本仓 kiro 层对
        // anthropic 层的镜像类反向引用**仅限这 2 处**（此处 + set_compression 下方），
        // 新增 kiro → anthropic 引用必须在 review 时显式说明理由。正解是把镜像存储搬到
        // 中性模块（如 common/runtime_state.rs，set/current 签名不动 + anthropic 层
        // re-export），届时本行与 set_compression 两处改指新模块即可（消费点零改动）。
        crate::anthropic::handlers::set_error_messages(new.error_messages.clone());
        // 输入压缩配置（handlers 进程镜像，同 error_messages 同款）：消费点全部读
        // `current_compression()` 镜像而非 config（handlers.rs 压缩热路径 5 处），
        // reload 后用新配置改写镜像，下个请求即读到（main/router 启动播种 + 此处
        // 热更 = 两个入口都齐；此前只有启动播种 ⇒ 手改 config.json 压缩配置 +
        // 面板保存触发 reload 后热路径仍读启动旧镜像，2026-08-15 清单 #1）。
        // 调用环标注同上方 set_error_messages（#16 仅此 2 处反向引用之一）。
        crate::anthropic::handlers::set_compression(new.compression.clone());
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
            tracing::info!(
                "主动 token 预刷新未启用（proactive_token_refresh=false），后台任务不运行"
            );
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
    /// 按 `kiroApiKey` 找池中已有的同号（用于「多开」时继承父号字段）。
    ///
    /// 存在的理由：多开请求通常只带 `authMethod` + `kiroApiKey` + `copies`，
    /// 而 `api_region` 等**路由相关字段**若缺失会让分身打到错误的 region host
    /// （`q.{region}.amazonaws.com`），而 ksk_ token 按 region 授权 → 上游 403
    /// `bearer token invalid`。实测：不继承时分身 0% 成功，继承后 83~100%。
    ///
    /// 返回第一个匹配（同 key 的多份分身彼此等价，取哪个都一样）。
    /// 用 SHA256 比对而非明文相等：与 `add_credential` 的去重判据同口径，
    /// 避免明文 key 在比较过程中被复制到更多临时变量里。
    pub fn find_credential_by_api_key(&self, api_key: &str) -> Option<KiroCredentials> {
        let want = sha256_hex(api_key);
        self.entries
            .lock()
            .iter()
            .find(|e| {
                e.credentials
                    .kiro_api_key
                    .as_deref()
                    .map(sha256_hex)
                    .as_deref()
                    == Some(want.as_str())
            })
            .map(|e| e.credentials.clone())
    }

    /// 同一把 key 下的**其它**凭据（`exclude` 里的 id 全部剔除）。
    ///
    /// 与 [`Self::find_credential_by_api_key`]（只取第一个、用来继承字段）互补：
    /// 那个回答「这个 key 长什么样」，本方法回答「这个账号下还有谁、各自有没有出口」。
    ///
    /// 用途是两件必须共用同一份名单的事（见 `add_credential_with_intent` 调用处）：
    /// ① 告警「同账号里有份走服务器裸 IP」；② 给缺 `clone_group` 的老成员回填组标识。
    /// 共用一份名单是刻意的 —— 两处各查一次就会再次分叉（本仓已有多起同因缺陷）。
    ///
    /// `exclude` 存在的理由：调用方刚建出来的那几份也同 key，但它们的出口状况已由
    /// 「本次分了几个节点/几份直连」那段文案如实报过，再报一遍是重复且会误导
    /// （看起来像池里另有问题号）。
    pub fn peers_sharing_api_key(&self, api_key: &str, exclude: &[u64]) -> Vec<SameKeyPeer> {
        let want = sha256_hex(api_key);
        self.entries
            .lock()
            .iter()
            .filter(|e| !exclude.contains(&e.id))
            .filter(|e| {
                e.credentials
                    .kiro_api_key
                    .as_deref()
                    .map(sha256_hex)
                    .as_deref()
                    == Some(want.as_str())
            })
            .map(|e| SameKeyPeer {
                id: e.id,
                clone_group: e.credentials.clone_group.clone(),
                clone_seq: e.credentials.clone_seq,
                has_own_exit: e
                    .credentials
                    .proxy_url
                    .as_deref()
                    .map(str::trim)
                    .is_some_and(|u| {
                        !u.is_empty() && !u.eq_ignore_ascii_case(KiroCredentials::PROXY_DIRECT)
                    }),
            })
            .collect()
    }

    /// 某分身组内已用的最大 `clone_seq`（组不存在或成员都没有 seq 时返回 0）。
    ///
    /// 供「给已有号再加 N 个分身」时接着编号，而不是从 1 重来 —— 后者会让同一组里
    /// 出现两个 `#1`，管理页上无法区分，删除时也无法指名。
    /// 该凭据**曾否成功过**（`success_count > 0`）—— 这是**终身**口径，不是本进程口径。
    ///
    /// ⚠️ 本注释原先写的是「这次进程生命周期内」，那是**错的**，而且这个区别很要紧：
    /// `success_count` 在启动时由 `load_stats` 从 `kiro_stats.json` 恢复
    /// （见 `:4065` 一带），所以一个上次运行成功过的号**重启后仍然是 true**。
    ///
    /// 为什么这个区别会误导人：若按「本进程内」理解，会以为「重启后必然 false ⇒
    /// 重启后第一个 bearer-invalid 403 必然被计失败」，从而去改本函数或它的调用点。
    /// 真实情况是——只有当 stats **没落盘**时才会退化成 false，而那条链已经单独修过：
    /// 停机路径的 `flush_stats_now()` + 每号**首次成功绕过 debounce 立刻落盘**
    /// （见 `report_success` 末尾）。剩余暴露面只有真 SIGKILL / panic / OOM。
    ///
    /// 用途：区分 `bearer token invalid` 403 的两种成因。同一句上游文案，含义相反：
    /// - 从未成功过 → 大概率是 **region 错配**（`ksk_` token 按 region 授权，
    ///   打错区就恒 403），该计失败、该被禁用。
    /// - 已经成功过 → token 对这个端点**证明有效**，403 只能是瞬态抖动，
    ///   计失败会把一个健康号打死。
    ///
    /// 为什么不用 `failure_count` 的「连续」语义兜住：`report_success` 确实会把
    /// `failure_count` 归零，但那只在**成功先落地**时有效。高并发下（实测单号 60+ RPM、
    /// 同一秒内成功与失败交错）三个并发请求可以各自 +1 到阈值，中间没有任何成功插进来。
    /// 实测 #481：2412 次成功、93.9% 成功率，仍被 3 次瞬态 403 在 1 秒内推到
    /// `TooManyFailures`（当天全池 116 次禁用 / 42 次自愈，号池一直在抖）。
    pub fn has_ever_succeeded(&self, id: u64) -> bool {
        self.entries
            .lock()
            .iter()
            .find(|e| e.id == id)
            .is_some_and(|e| e.success_count > 0)
    }

    pub fn max_clone_seq_in_group(&self, group: &str) -> u32 {
        self.entries
            .lock()
            .iter()
            .filter(|e| e.credentials.clone_group.as_deref() == Some(group))
            .filter_map(|e| e.credentials.clone_seq)
            .max()
            .unwrap_or(0)
    }

    /// **原子**预留某分身组内连续 `count` 个序号，返回起始序号（含）。
    ///
    /// 即返回 `s` 时，本次调用独占 `s..s+count`（`count=0` 时返回下一个可用号但不占用）。
    ///
    /// # 为什么必须是"预留"而不是"读 max 再各自 +1"
    ///
    /// 🔴 修复的并发缺陷：`add_credential` 原先是「读 `max_clone_seq_in_group` → 写第 1 份
    /// → 再读一次当基准 → 循环里逐份 `.await` 入池」。发号与入池之间横跨多个 await，
    /// 两个并发请求（两个面板标签页、脚本重试）就会各自读到同一个 max：
    ///
    /// ```text
    /// A: max=0 →                                    A 写 #1  A 写 #2 #3
    /// B:        max=0（A 还没落进 entries）→ B 写 #1  B 写 #2 #3
    /// ```
    ///
    /// 结果同一组里两个 `#1`、两个 `#2`、两个 `#3` —— 管理页显示两个「分身 #2」，
    /// 运维既分不清哪个是哪个，也没法指名删掉其中一个。
    ///
    /// 本方法把「决定用哪些号」压进**一个临界区**：高水位在这里就前进，故任何后来者
    /// 拿到的号段都在它之后，与前一批是否已经入池无关。
    ///
    /// # 锁序（唯一同时持两把锁的地方）
    ///
    /// 先 `clone_seq_hwm` 再 `entries`，且**不跨 await**（两把都是
    /// `parking_lot::Mutex`，跨 await 持有会把整个池的读写卡在这个任务上）。
    /// 全仓仅此一处按该顺序取两把锁，故不存在与之相反的路径构成死锁。
    ///
    /// entries 那一遍扫是**重启后的地板**：内存高水位表不持久化（没必要，seq 本身
    /// 随凭据落盘），所以进程重启后第一次发号必须从既有成员的 max 接着走，
    /// 否则会把已经用过的号再发一次。
    pub fn reserve_clone_seqs(&self, group: &str, count: u32) -> u32 {
        let mut hwm = self.clone_seq_hwm.lock();
        let floor = self.max_clone_seq_in_group(group);
        let cur = hwm.get(group).copied().unwrap_or(0).max(floor);
        let start = cur + 1;
        if count > 0 {
            hwm.insert(group.to_string(), cur.saturating_add(count));
        }
        start
    }

    pub fn export_credential(&self, id: u64) -> Option<KiroCredentials> {
        self.entries
            .lock()
            .iter()
            .find(|e| e.id == id)
            .map(|e| e.credentials.clone())
    }

    /// 「每个出口 URL 上挂了几个凭据」的计数表（键是 `proxy_url` 原文）。
    ///
    /// # 这是**启发式**，不是正式关联
    ///
    /// 凭据与节点之间没有 id 级绑定（`KiroCredentials` 里只有 `proxy_url` 字符串），
    /// 所以调用方只能按「凭据的 proxy_url == 节点的 url」推断。两种已知漏算：
    /// - 手工在卡片上填的代理不走 `parse_proxy_link` 归一（`set_credential_proxy` 只拆账密、
    ///   不动 scheme），于是 `socks://h:1080` 与节点表里的 `socks5://h:1080` 字符串不等；
    /// - 用户直接改过节点的 url 之后，老凭据仍指着旧地址。
    ///
    /// 漏算的方向是**安全的**：一个实际已被占用的节点会显示成空闲，最坏结果是两份共用
    /// 一个出口 —— 而那正是节点不足时的既有行为。反过来（虚高）才会让可用节点被跳过。
    ///
    /// `"direct"`（强制不走代理）与 `None`（回退全局代理）都不计入任何节点。
    pub fn proxy_url_usage(&self) -> std::collections::HashMap<String, usize> {
        let mut map: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        for e in self.entries.lock().iter() {
            if let Some(u) = e.credentials.proxy_url.as_deref() {
                if u.is_empty() || u.eq_ignore_ascii_case("direct") {
                    continue;
                }
                *map.entry(u.to_string()).or_insert(0) += 1;
            }
        }
        map
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

    /// custom_api 自动状态变更的统一保险。
    ///
    /// 返回 `Some(池中是否仍有启用项)` 表示目标是代挂站，调用方必须立即返回，保持该项
    /// 当前的 `disabled` 状态不变；`None` 表示普通 Kiro 凭据，可继续原有自动禁用逻辑。
    /// 手动开关不走本保险，因此管理员仍可明确禁用/启用代挂站。
    /// ⚠️ **返回的 bool 是「全池是否仍有启用项」，不是「Kiro 池是否仍有可选号」**
    /// —— 2026-08-10 审计提出这是同 `:4455` 一类的口径缺陷，复核结论是**当前架构下不可达，
    /// 故刻意不改**（改一个不可达分支只增加风险）。留此说明避免重复上报：
    ///
    /// 该 `Some(..)` 分支只在「调用方把 **custom_api 的 id** 喂进 `report_*` 系列」时命中，
    /// 而透传路径走的是 `record_passthrough_result`（它有独立逻辑，**不调** `report_*`），
    /// Kiro 路径又因两池隔离拿不到 custom_api 的 id ⇒ 本分支是**纯防御性**代码（见上方
    /// 「统一保险」的措辞）。若将来真出现「Kiro 路径持有代挂号 id」的路径，
    /// 那本身就是隔离铁律被破坏，届时要修的是那条路径，而不是这里的计数口径。
    fn preserve_custom_api_state(&self, id: u64) -> Option<bool> {
        let entries = self.entries.lock();
        let is_custom_api = entries
            .iter()
            .any(|e| e.id == id && e.credentials.is_custom_api_credential());
        is_custom_api.then(|| entries.iter().any(|e| !e.disabled))
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
    /// 与 Kiro 的 is_entry_selectable 彻底分离——Kiro 选号已排除 custom_api,此处只管 custom_api。
    ///
    /// # 返回值（2026-08-10 起多带一个 guard）
    /// 命中返回 `(id, credentials, InflightGuard)`；无可用返回 `None` → 调用方 failover
    /// 完毕落 Kiro 主力路径。
    ///
    /// 🔴 **第三项 `InflightGuard` 是承重的**：它必须由调用方**持有到本次上游调用结束**
    /// （Drop 即 inflight-1）。改前本函数只返回 `(id, credentials)`、完全不占位，
    /// 导致排序键里的 inflight 维度对代挂号恒为 0（结构性失效）+ 选号到记账之间有一整个
    /// 上游 RTT 的惊群窗口。详见 `.map(...)` 处的完整说明。
    /// 把 guard 提前 drop（如 `let (id, cred, _) = ...`）等于把这个修复废掉。
    fn select_custom_api_inner(
        &self,
        exclude: &std::collections::HashSet<u64>,
        model: Option<&str>,
        // true = 只探测有无候选，不做 inflight/rpm 占位（见下方 `peek_only` 处的理由）
        peek_only: bool,
    ) -> Option<(u64, KiroCredentials, Option<InflightGuard>)> {
        let entries = self.entries.lock();
        let cooldown_on = self.cooldown_enabled.load(Ordering::Relaxed);
        // 模型白名单硬门（与 Kiro 路径同款）：设了 allowed_models 且当前模型不在其中
        // → 过滤掉；model 为空（无模型语义的调用）放行。
        //
        // 🔴 2026-08-16 行为变化：白名单判定**改回原始模型名**。此前（deepseek 归一化
        // 时代）选号层按改写后名（fallback 预判）判白名单（`claude-*`/`gpt-*` →
        // fallback_model）；归一化已完全移除，请求体模型名不再改写（仅 model_mapping
        // 表映射保留，发生在选号**之后**的 forward 内）—— 白名单必须对客户端原始名
        // 判定，否则 `["claude-*"]` 白名单会因预判成 `deepseek-v4-flash` 永不命中。
        // 模型映射规则快照（support_rank 的映射后名判定用，一次取——排序闭包内每
        // 候选一次读缓存，规则表本身只取一次）。
        let mapping_rules = self.config().model_mapping.clone();
        // ⭐ RPM 计数**一次加锁批量取回**（与 Kiro 主路径 `select_next_credential` 同款模式）。
        //
        // 此前排序键对每个候选各调一次 `self.rpm.count(e.id)`，每次独立加 RpmTracker
        // 的锁 —— 43 号池一次选号最多 43 次加锁，而这整段都在 entries 锁临界区内
        // （100 并发压测最先暴露的选号瓶颈）。先把过滤后的候选收集成 Vec，再一次
        // `counts_for` 取回全部候选的计数：锁获取 O(n) → O(1)，闭包内退化为纯
        // HashMap 查表。收集成 Vec 同时让排序键变成「快照后稳定全序」（与 Kiro
        // 主路径 memoize 约定一致），min_by_key 的比较器不再依赖链式重算。
        // 候选判定拆成两个闭包（M1.3 逃生舱要复用**同一套**过滤链，杜绝两处漂移）：
        // `is_candidate` = 除失败余温外的一切硬门（启用/池型/exclude/冷却/模型黑名单/
        // 白名单）；`is_warm` = 失败余温（近 PASSTHROUGH_FAILURE_DECAY_SECS 失败）。
        let is_candidate = |e: &CredentialEntry| {
            !e.disabled
                && e.credentials.is_custom_api_credential()
                && !exclude.contains(&e.id)
                && (!cooldown_on || self.cooldown.is_available(e.id))
                // 🔴 并发上限硬门（迁移差距 P1）：inflight >= CREDENTIAL_MAX_CONCURRENCY
                // 的号**不可选**——上游按账号限瞬时并发，单号被灌爆会触发风控。与
                // `InflightGuard`（持有期标记）与每凭据闸（等响应头的并发）都不同：
                // 本门是「同时在飞流数」的硬上限，慢流堆积（响应头已回、流仍在传）时
                // 只有它能兜住。全部达限 → 候选为空：混池立刻 None 分流 Kiro；
                // 纯代挂池由 `select_custom_api_or_wait` 短等 ConcurrencyFull 后重选
                // （与 Kiro `acquire_context` 同款 250ms / MAX_TRANSIENT_WAIT）。
                && !at_max_concurrency(e)
                // 🔴 模型黑名单（2026-08-14 根治）：上游明确说过「该模型不支持」的
                // 号×模型组合直接跳过——请求 opus-5 不再白付一跳撞 pigcode 类中转站。
                && !model.is_some_and(|m| self.is_model_blacklisted(e.id, m))
                && model.is_none_or(|m| {
                    // 🔴 2026-08-16：白名单判定改回**原始模型名**（deepseek 归一化移除）。
                    // 此前选号层按改写后名（fallback 预判）判白名单（选中即意味着
                    // 改写后的模型该号真的能服务）；现在请求不再改写，客户端原始名就是
                    // 实际发给上游的名（映射发生在 forward 内、选号之后），白名单对原始
                    // 名直接判定即可 —— 判定键 = 发送键，语义自洽。
                    e.credentials.allows_model(m)
                })
        };
        // 🔴 失败余温硬排除（2026-08-16 复测根治）：近 60s 失败的号**不参与选号**
        // （过滤级，非排序位）——复测证明排序位无效：高频形态下 RPM 维度恒判
        // 死号优先（死号每轮只被打一次 RPM 恒少），余温位永不参与比较，死号
        // 仍每请求白打一跳。过滤排除后：死号 60s 内零命中；60s 后恢复探测
        // （瞬态抖动不误杀）。
        let is_warm = |e: &CredentialEntry| {
            e.last_failure_at.get().is_some_and(|t| {
                t.elapsed() <= StdDuration::from_secs(PASSTHROUGH_FAILURE_DECAY_SECS)
            })
        };
        let mut candidates: Vec<&CredentialEntry> = entries
            .iter()
            .filter(|e| is_candidate(e) && !is_warm(e))
            .collect();
        // 🔴 全池余温逃生舱（2026-08-16 对抗审查 M1.3）：所有候选都带余温（系统性抖动，
        // 如上游整体压限/集体短暂故障）时不再直接「无候选 → 503」，按**最老余温**号
        // （失败最早 = 最接近恢复）硬试一次——对照 Kiro 主路径 `select_ignoring_cooldown`
        // 兜底先例（拿真实上游错误好过网关自造 503）。硬试仍失败则该号余温刷新 + 被
        // exclude，本请求链继续换下一个最老余温号（每号至多一次，hop 上限兜底）；
        // 真正死透的号（恒 502）失败时刻被不断刷新，自然排在逃生链末端，健康号恢复
        // 后可立即重进冷候选，逃生舱不再触发。
        if candidates.is_empty() {
            if let Some(oldest) = entries
                .iter()
                .filter(|e| is_candidate(e) && is_warm(e))
                .min_by_key(|e| e.last_failure_at.get())
            {
                tracing::warn!(
                    credential_id = oldest.id,
                    "全 custom_api 候选均带失败余温：逃生舱硬试最老余温号 #{}（系统性抖动时不再纯 503）",
                    oldest.id
                );
                candidates = vec![oldest];
            }
        }
        let cand_ids: Vec<u64> = candidates.iter().map(|e| e.id).collect();
        let rpm_counts = self.rpm.counts_for(&cand_ids);
        let rpm_of = |id: u64| rpm_counts.get(&id).copied().unwrap_or(0);
        // ⭐ 爬坡计数同样**一次加锁批量取**（与 counts_for 同理由：排序键对每个候选都要
        // 读，逐个加锁会在选号临界区内放大锁竞争）。与 Kiro 主路径选号同款预取。
        let ramp_counts = self
            .rpm
            .ramp_counts_for(&cand_ids, StdDuration::from_secs(RAMP_RECENT_SECS as u64));
        let ramp_of = |id: u64| ramp_counts.get(&id).copied().unwrap_or((0, 0));
        // 模型级计数同样**一次加锁批量取**（与 counts_for 同理由）；模型为空（无模型
        // 语义的调用）时跳过，该维度不参与也不记录，与记录侧对称。
        let model_calls = model
            .filter(|m| !m.is_empty())
            .map(|m| self.rpm.model_counts_for(&cand_ids, m))
            .unwrap_or_default();
        let model_calls_of = |id: u64| model_calls.get(&id).copied().unwrap_or(0);
        candidates
            .into_iter()
            // 均衡分流键(升序):⭐support_rank(正向路由) → 优先级 → 爬坡压力档 →
            // 近 60s RPM → 模型级近期调用 → 在途 → ⭐失败余温(近 60s 上游失败过的号
            // 降权) → ⭐显式 tie-break(id=创建序=下标序)。
            // 8 位结构：support_rank 只进透传池，Kiro 主路径 12 位键（health_tier 起）
            // 不碰——两池隔离铁律（设计文档 §6，守卫对齐）。
            // rpm/爬坡/模型级均来自上方批量预取。
            .min_by_key(|e| {
                // ⭐ 模型支持档（2026-08-16 模型感知正向路由 S2，设计文档 §3）：
                // Confirmed=0 / Unknown=1 / Unsupported=2，升序小整数档位（与主路径
                // health_tier 同风格，可测、可断言、无魔法系数）。判定键 = **改写后名**
                // （map_target 结果；exempt 号回落原始名）——请求实际打到上游的名字
                // 才与目录可比（映射过的请求 claude-opus-5 → gpt-5.6-sol 在 pigcode
                // 类目录里用原始名永远查不中）。无模型语义的调用（model=None）
                // 同 Unknown（不参与正向判定，与 model_calls 维度对称）。
                //
                // 放**首位**（priority 之前）是有意行为变化：目录确认的号优先于配置
                // 优先级（正向证据比静态配置更接近「该号能服务该模型」的事实）。
                // 若线上观察到 priority 语义被过度稀释，可一行降位（设计文档 §3 兜底）。
                let support_rank = match model {
                    None => 1u8,
                    Some(m) => {
                        let target = if e.credentials.model_mapping_exempt == Some(true) {
                            m.to_string()
                        } else {
                            crate::kiro::model_mapping::map_target(m, &mapping_rules)
                                .unwrap_or_else(|| m.to_string())
                        };
                        match self.model_support(e.id, &target) {
                            ModelSupport::Confirmed => 0,
                            ModelSupport::Unknown => 1,
                            ModelSupport::Unsupported => 2,
                        }
                    }
                };
                // ⭐ 爬坡压力档（slew-rate 分档）：与 Kiro 主路径共用
                // `scheduling::ramp_tier_of` —— 5x/2x 阈值、RAMP_MIN_SAMPLES、窗口
                // 折算全在那一个函数里（2026-08-16 收敛，防两池分叉）。纯 RPM 派生，
                // 不碰 health/family —— 守两池隔离铁律。透传池此前只有
                // (priority, rpm, model_calls, inflight) 四个键：rpm 是**绝对速率**，
                // 而上游惩罚的是**速率的跃升**（slew-rate，实测 ≥5x 跃升 48.3% 429
                // vs 平稳 0.7%），正在被猛灌的中转站绝对速率可能并不高，会被当
                // "空闲"继续选中。排在 rpm 之前：与主路径「ramp 先于
                // rpm_usage/inflight」同序（见主路径排序键闭包注释）。
                let ramp_tier = {
                    let (recent, total) = ramp_of(e.id);
                    crate::kiro::scheduling::ramp_tier_of(recent, total)
                };
                (
                    support_rank,                    // ① NEW 模型支持档（正向路由）
                    e.credentials.priority,          // ② 优先级（池内首选）
                    ramp_tier,                       // ③ 爬坡压力档
                    rpm_of(e.id),                    // ④ 近 60s RPM
                    model_calls_of(e.id),            // ⑤ 该模型近期调用数
                    e.inflight.load(Ordering::Acquire), // ⑥ 在途
                    // ⭐ 失败余温降权位（2026-08-16 N1 根治）：近 PASSTHROUGH_FAILURE_DECAY_SECS
                    // 内上游失败（5xx/429/401/403）的号 → 1 排后，健康号优先。
                    //
                    // 为什么放倒数第二（仅 e.id tie-break 在它之后）：它是**软降权**——
                    // 只在其余均衡维度（支持档/优先级/RPM/爬坡/在途）全平局时生效
                    // （线上低负载形态正是如此：请求间隔 >60s，RPM 滑窗归零，
                    // support_rank 同档时前 6 键全平）。RPM 有数据时仍以
                    // 真实负载分流，避免用失败记忆压住正在真实工作的号。
                    //
                    // 为什么独立于冷却体系：`cooldown_custom_api` 被 `cooldown_enabled`
                    // 门控（线上 cooldownEnabled=false 时冷却完全不生效），本键只进
                    // 排序键、不受任何开关门控——死号恒 502 时不再每请求白打一跳；
                    // 60s 后余温过期恢复平权（瞬态抖动不误杀，死号有机会被探测复活）。
                    u8::from(
                        e.last_failure_at.get().is_some_and(|t| {
                            t.elapsed() <= StdDuration::from_secs(PASSTHROUGH_FAILURE_DECAY_SECS)
                        }),
                    ),
                    // ⭐ 显式 tie-break（2026-08-16 契约化）：id 单调递增 = 创建序 = entries
                    // 下标序，平局时恒选最早创建的号 —— 与 min_by_key 的隐式行为逐位等价，
                    // 但把「平局胜者 = Vec 下标」的隐式依赖变成排序键的显式契约
                    // （删除/恢复/重排不再静默改变平局胜者）。
                    e.id,
                )
            })
            // 🔴 `peek_only` = 只探测「有没有候选」，**不做任何占位**。
            //
            // 为什么需要它：provider 的透传闸门要判断「排除当前号后还有别的号可换吗」，
            // 若直接调本函数探测，会白白 `InflightGuard::acquire` + `rpm.record` 一次
            // ⇒ 污染 inflight 与 RPM 计数（探测比真实请求频繁得多，污染量级不小）。
            // 而复制一份过滤谓词去别处写探测又必然与这里漂移（这段谓词含 deepseek
            // 白名单感知，正是 2026-08-09 修过一次的分叉点）⇒ 用同一函数 + 开关最稳。
            .map(|e| {
                if peek_only {
                    // 占位交给真正选号的那次调用；这里只回报"有候选"。
                    return (e.id, e.credentials.clone(), None);
                }
                (
                    e.id,
                    e.credentials.clone(),
                    Some((e.inflight.clone(), e.id)),
                )
            })
            // 真正的占位在锁**仍持有**时完成（与 commit_selection 同一临界区语义）。
            .map(|(id, cred, commit)| {
                let guard = commit.map(|(inflight, cid)| {
                    let g = InflightGuard::acquire(inflight);
                    self.rpm.record(cid);
                    // 模型级分流计数与每凭据计数同点记录（与 Kiro 主路径 commit_selection
                    // 对称，全仓 record_model 只在这两处）。模型为空（无模型语义）不记。
                    if let Some(m) = model.filter(|m| !m.is_empty()) {
                        self.rpm.record_model(cid, m);
                    }
                    // 成功选到号 ⇒ 清零「连续全池不可用」计数（配对逻辑见该字段的文档）。
                    // ⚠️ 只在**非 peek** 分支清零：peek 只是探测，不代表真有请求被服务。
                    self.consecutive_pool_unavailable
                        .store(0, Ordering::Relaxed);
                    g
                });
                (id, cred, guard)
            })
    }

    /// 只探测「透传池里还有没有可选号」，**不占位、不记账**。
    ///
    /// 供 provider 的并发闸判断「排除当前号后是否还有别的号可换」——
    /// 有则换号，没有则必须**等许可**而不能排除唯一的号
    /// （上一版无条件排除，在单号池上直接制造 429 且 trace 的 credential_id 为空）。
    pub fn has_other_custom_api_candidate(
        &self,
        exclude: &std::collections::HashSet<u64>,
        model: Option<&str>,
    ) -> bool {
        self.select_custom_api_inner(exclude, model, true).is_some()
    }

    /// 选号并占位（对外的正式入口，见下方 `select_custom_api_inner` 的完整说明）。
    pub fn select_custom_api(
        &self,
        exclude: &std::collections::HashSet<u64>,
        model: Option<&str>,
    ) -> Option<(u64, KiroCredentials, InflightGuard)> {
        self.select_custom_api_inner(exclude, model, false)
            .map(|(id, cred, guard)| {
                // peek_only=false 时 guard 必定为 Some（见 inner 的 map）。
                (id, cred, guard.expect("非 peek 模式必有 InflightGuard"))
            })
    }

    /// Kiro 路径此刻是否有**立即可选**号（`is_entry_selectable`，不含 custom_api）。
    ///
    /// 透传满并发时的分流开关：有则立刻 `None` 让 Kiro 吃溢出；无则才短等
    /// `WaitReason::ConcurrencyFull`（纯代挂大中转站：inflight 毫秒级释放，立刻
    /// None 会硬失败）。只 peek，不占位、不 `report_failure`。
    pub fn has_kiro_selectable(&self, model: Option<&str>) -> bool {
        let is_opus = model
            .map(|m| m.to_lowercase().contains("opus"))
            .unwrap_or(false);
        let model_key = model.unwrap_or("");
        self.entries
            .lock()
            .iter()
            .any(|e| self.is_entry_selectable(e, is_opus, model_key))
    }

    /// 启用中的 Kiro 凭据快照（不含 custom_api、不占位）。供桶封禁 last-hop 判定。
    pub fn peek_enabled_kiro(&self) -> Vec<(u64, KiroCredentials)> {
        self.entries
            .lock()
            .iter()
            .filter(|e| !e.disabled && !e.credentials.is_custom_api_credential())
            .map(|e| (e.id, e.credentials.clone()))
            .collect()
    }

    /// 透传池等待判定（**只看 custom_api**）。与 `transient_wait_outcome` 镜像但方向相反：
    /// 那边 continue 掉代挂号（两池隔离），这边 continue 掉 Kiro。
    ///
    /// `exclude` 是本请求已试过的号：它们不会被再选，不算「将要恢复的候选」
    /// （与 Kiro 侧「不吃排除集」不同——透传 exclude 是硬门，不是偏好）。
    fn custom_api_wait_outcome(
        &self,
        exclude: &HashSet<u64>,
        model: Option<&str>,
    ) -> WaitOutcome {
        let cooldown_on = self.cooldown_enabled.load(Ordering::Relaxed);
        let entries = self.entries.lock();
        let mut has_candidate = false;
        let mut immediate_available = false;
        let mut waits: Vec<(StdDuration, WaitReason)> = Vec::new();

        for entry in entries.iter() {
            if entry.disabled || !entry.credentials.is_custom_api_credential() {
                continue;
            }
            if exclude.contains(&entry.id) {
                continue;
            }
            if model.is_some_and(|m| self.is_model_blacklisted(entry.id, m)) {
                continue;
            }
            if let Some(m) = model {
                if !m.is_empty() && !entry.credentials.allows_model(m) {
                    continue;
                }
            }

            has_candidate = true;

            if cooldown_on {
                if let Some((_reason, remaining)) = self.cooldown.check_cooldown(entry.id) {
                    waits.push((remaining, WaitReason::Cooling));
                    continue;
                }
            }

            if at_max_concurrency(entry) {
                waits.push((
                    StdDuration::from_millis(250),
                    WaitReason::ConcurrencyFull,
                ));
                continue;
            }

            immediate_available = true;
        }

        if !has_candidate {
            return WaitOutcome::NoCandidate;
        }
        if immediate_available {
            return WaitOutcome::Available;
        }
        match waits.into_iter().min_by_key(|(d, _)| *d) {
            Some((d, reason)) => WaitOutcome::Wait(d, reason),
            None => WaitOutcome::Available,
        }
    }

    /// 透传选号：满并发且**无** Kiro 可分流时，复用 `WaitReason::ConcurrencyFull`
    /// 短等（250ms，封顶 [`MAX_TRANSIENT_WAIT_SECS`]）后重选。混池立刻 None。
    ///
    /// 不另造信号量：占位仍是 `select_custom_api` 的 `InflightGuard`。
    pub async fn select_custom_api_or_wait(
        &self,
        exclude: &HashSet<u64>,
        model: Option<&str>,
    ) -> Option<(u64, KiroCredentials, InflightGuard)> {
        if let Some(x) = self.select_custom_api(exclude, model) {
            return Some(x);
        }
        // 混池：Kiro 吃溢出。立刻 None，不睡。
        if self.has_kiro_selectable(model) {
            return None;
        }
        let wait_started = Instant::now();
        let mut race_reselect = 0usize;
        const MAX_RACE_RESELECT: usize = 64;
        loop {
            match self.custom_api_wait_outcome(exclude, model) {
                WaitOutcome::Wait(wait, WaitReason::ConcurrencyFull)
                    if wait_started.elapsed()
                        < StdDuration::from_secs(MAX_TRANSIENT_WAIT_SECS) =>
                {
                    let remaining = StdDuration::from_secs(MAX_TRANSIENT_WAIT_SECS)
                        .saturating_sub(wait_started.elapsed());
                    let w = wait
                        .max(StdDuration::from_millis(250))
                        .min(remaining.max(StdDuration::from_millis(250)));
                    tracing::warn!(
                        "custom_api 池在飞均达并发上限且无 Kiro 可分流，等待释放 {:?} 后重试",
                        w
                    );
                    sleep(w).await;
                }
                WaitOutcome::Available if race_reselect < MAX_RACE_RESELECT => {
                    race_reselect += 1;
                }
                _ => return None,
            }
            if let Some(x) = self.select_custom_api(exclude, model) {
                return Some(x);
            }
            if self.has_kiro_selectable(model) {
                return None;
            }
        }
    }

    /// 给 custom_api 透传号设一段冷却(**仅操作 cooldown,不碰 health/family/report_success/failure**,
    /// 守两池隔离铁律)。供透传 failover:某号 403 额度满 / 401 key 失效 / 429 / 5xx 时暂时跳过它,
    /// 让 select_custom_api 下次(及本请求循环 exclude)避开,换下一个号。
    /// 上游明确返回「不支持该模型」（model_not_found / no available channel）时调用：
    /// 记 (id, model) 短期黑名单（TTL 见 MODEL_BLOCK_TTL，#9 合并后与 Kiro 主路径
    /// `report_model_invalid` 写**同一张表** model_blocklist），后续选号跳过该号×该模型组合——
    /// 根治「请求 opus-5 白付一跳打到只有 gpt 的中转站」（2026-08-14）。
    /// 粒度是号×模型：该号对别的模型仍可服务，不做号级冷却（白丢池容量）。
    pub fn mark_model_unsupported(&self, id: u64, model: &str) {
        if model.is_empty() {
            return;
        }
        self.model_blocklist
            .lock()
            .insert((id, model.to_string()), Instant::now());
    }

    /// 选号过滤用：该号×该模型是否在模型级黑名单内（顺带惰性清理过期项）。
    ///
    /// 与 Kiro 主路径 [`is_model_blocked`](Self::is_model_blocked) 查**同一张表**
    /// （#9 合并），逻辑复用其实现——两池共用一份「号×模型不支持」黑名单，防再次分叉。
    pub fn is_model_blacklisted(&self, id: u64, model: &str) -> bool {
        self.is_model_blocked(id, model)
    }

    /// 模型支持三态查询（模型感知正向路由，S1，签名同 zyphr `cached_model_support`）：
    /// - 无缓存条目 / 条目过期 → `Unknown`（中性）；
    /// - 目录含目标（大小写不敏感）→ `Confirmed`；
    /// - 目录明确不含 → `Unsupported`（软降权，绝不出局）。
    ///
    /// 纯内存读（parking_lot 短临界，与 `model_blocklist` 同款），**绝不触发网络**——
    /// 选号热路径不做巡检（设计文档 §5 评审补强：未知即未知，下轮巡检覆盖）。
    pub fn model_support(&self, id: u64, target: &str) -> ModelSupport {
        let cache = self.model_catalog_cache.lock();
        match cache.get(&id) {
            Some(e) if e.refreshed_at.elapsed() < MODEL_CATALOG_TTL => {
                support_for(target, &e.models)
            }
            _ => ModelSupport::Unknown,
        }
    }

    /// 清凭据的模型目录缓存（含退避与单飞锁），统一收口（设计文档 §2 失效挂点）。
    ///
    /// 调用点：`set_custom_api_config`（base_url/api_key 变更）、
    /// `delete_credential_forced`
    /// （删号防内存残留）、`set_disabled` **启用**路径（Review3 m5：禁用期残留的
    /// Confirmed 不会自愈，重启用后白打一跳）。凭据禁用**不**清（缓存无害，
    /// 巡检循环跳过禁用号即可）。
    fn invalidate_model_catalog(&self, id: u64) {
        self.model_catalog_cache.lock().remove(&id);
        self.model_catalog_backoff.lock().remove(&id);
        self.model_catalog_locks.lock().remove(&id);
    }

    /// 写模型目录缓存（唯一写入源：巡检成功且列表非空，设计文档 §2）。与旧目录
    /// 对比记 diff 日志（设计文档 §5）；成功即重置退避。空列表**不**走本函数。
    fn store_model_catalog(&self, id: u64, models: Vec<String>) {
        let old = {
            let mut cache = self.model_catalog_cache.lock();
            let old = cache.get(&id).map(|e| e.models.clone());
            cache.insert(
                id,
                ModelCatalogEntry {
                    models: models.clone(),
                    refreshed_at: Instant::now(),
                },
            );
            old
        };
        self.model_catalog_backoff.lock().remove(&id);
        match old {
            Some(old) => {
                let added: Vec<&String> = models
                    .iter()
                    .filter(|m| !old.iter().any(|o| o == *m))
                    .collect();
                let removed: Vec<&String> = old
                    .iter()
                    .filter(|o| !models.iter().any(|m| m == *o))
                    .collect();
                if !added.is_empty() {
                    tracing::info!(
                        credential_id = id,
                        "模型目录新增 {} 个模型: {:?}",
                        added.len(),
                        added
                    );
                }
                if !removed.is_empty() {
                    tracing::info!(
                        credential_id = id,
                        "模型目录移除 {} 个模型: {:?}",
                        removed.len(),
                        removed
                    );
                }
            }
            None => {
                tracing::info!(credential_id = id, "模型目录首次建立（{} 个模型）", models.len());
            }
        }
    }

    /// 给 custom_api 透传号设一段冷却(**仅操作 cooldown,不碰 health/family/report_success/failure**,
    /// 守两池隔离铁律)。供透传 failover:某号 403 额度满 / 401 key 失效 / 429 / 5xx 时暂时跳过它,
    /// 让 select_custom_api 下次(及本请求循环 exclude)避开,换下一个号。
    ///
    /// 返回**是否真的设置了冷却**：本函数被 `cooldown_enabled` 门控，线上
    /// cooldownEnabled=false 时什么都不做返回 false —— 调用方（provider 的日志）
    /// 必须按真实行为表述，不得打印「该号冷却 Ns」（那是撒谎，见 provider.rs
    /// 该调用点 2026-08-16 N2 修正）。冷却体系被门控时的跨请求失败记忆由
    /// [`Self::mark_passthrough_failure`]（排序键失败余温位）承担。
    ///
    /// `reason`（2026-08-16 S4 独立标签）：只决定面板 `cooldownReason`/`cooldownCode`
    /// 的展示（admin 侧经 `cooldown_snapshot` → `CooldownInfo.reason` 下发），
    /// **不改变时长**——时长由 provider 显式给定（见 provider.rs `passthrough_cooldown_for`：
    /// 401/403 用 `AuthTransient` 仍是 180s，不走 `CooldownReason::default_duration()` 的 20s）。
    pub fn cooldown_custom_api(&self, id: u64, secs: u64, reason: CooldownReason) -> bool {
        if self.cooldown_enabled.load(Ordering::Relaxed) {
            self.cooldown.set_cooldown_with_duration(
                id,
                reason,
                Some(std::time::Duration::from_secs(secs)),
            );
            true
        } else {
            false
        }
    }

    /// 记录一次透传失败时刻（供排序键「失败余温」降权位使用）。
    ///
    /// 与 [`Self::cooldown_custom_api`] 的分工：冷却被 `cooldown_enabled` 门控
    /// （线上 cooldownEnabled=false 时冷却完全不生效，failover 只能靠本请求链内
    /// 的 exclude，跨请求的死号仍会被每个新请求重新选中），而本字段**只进排序键**、
    /// 不受任何开关门控——死号恒 502 时失败后 [`PASSTHROUGH_FAILURE_DECAY_SECS`]
    /// 内被降权，健康号优先，根治「每请求先打死号白付一跳 + 延迟」。
    ///
    /// 调用方：provider 透传 failover 判定「值得换号」的失败（5xx/429/401/402/403；
    /// 🔴 2026-08-16 对抗审查 M1.2：**400/404 除外**——坏请求（无效 tool schema / 该站
    /// 不认模型）是全池同质的客户端错误，一次 failover 把所有号打上余温会让 60s 内
    /// 任何请求零尝试直返 503；其模型语义已由 `mark_model_unsupported` 黑名单通道覆盖）。
    /// 注意 5xx 走「瞬态不冷却」分支（`_ => 0`），但**同样要记失败时刻**，否则死号
    /// 恒 502 场景无任何跨请求记忆。成功由 [`Self::record_passthrough_result`] 清热
    /// （M1.1）：号证明活了立刻回来，不等 60s 窗口自然过期。
    pub fn mark_passthrough_failure(&self, id: u64) {
        if let Some(e) = self.entries.lock().iter_mut().find(|e| e.id == id) {
            e.last_failure_at.set(Some(Instant::now()));
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
    /// - `Success`(2xx):`success_count += 1` + `request_count += 1`（只计成功口径）。
    /// - `ServerError`/`NetworkError`(5xx/连接错误):`failure_count += 1`(供展示号"不健康"),
    ///   **不**计 request_count、**不**禁用(透传失败多为上游临时问题,客户端自行重试/退避)。
    /// - 其余(429 RateLimited / 401·403 AuthFailed / 4xx BadRequest 等):**既不计成功也不计失败**
    ///   ——透传给客户端由其处理,不误判号健康(dwgx:4xx/429 不计号失败)。
    ///
    /// 三态都会更新 `last_used_at`,让号池状态条/发光网格能"流动"(反映真实活跃)。
    ///
    /// ⚠️ **本函数不再 `rpm.record`**（2026-08-10 移到 `select_custom_api` 的选号占位处，
    /// 理由见函数体内注释：记在上游返回后太晚，会留出一整个 RTT 的惊群窗口）。
    /// 原注释写「三态都会 `rpm.record`」已是**假断言**，2026-08-10 对抗评审抓出并改正
    /// —— 全仓 `self.rpm.record` 只有两处：`select_custom_api`（透传）与
    /// `commit_selection`（Kiro 主路径）。**别照旧注释去"修"回来，那会变成双记。**
    /// 模型级 `rpm.record_model` 沿用同一纪律：只在上述同两个选号占位点记录
    /// （2026-08-14，见两处调用点的注释），此处同样不得补记，否则模型计数双记。
    pub fn record_passthrough_result(&self, id: u64, outcome: crate::usage::RequestOutcome) {
        use crate::usage::RequestOutcome as RO;
        // ⚠️ **这里刻意不再 `rpm.record(id)`**（2026-08-10 移走）。
        //
        // 它已被移到 `select_custom_api` 的选号占位里（与 Kiro 路径的 `commit_selection`
        // 同款：选中 + inflight+1 + rpm.record 在同一临界区内完成）。原因是记在**上游返回后**
        // 太晚 —— 从选号到记账之间隔着一整个上游 RTT，期间并发请求读到的 RPM 都是旧值，
        // 排序键的「按 RPM 均衡分流」在这个窗口内完全失效 ⇒ 惊群全压同一号。
        //
        // 🔴 若在此处恢复调用，同一请求会被记**两次** ⇒ 代挂号 RPM 翻倍虚高 ⇒
        // 排序键与饱和判定同时被污染（面板也会显示假的 2 倍速率）。两处只能有一处。
        // ⚠️ entries 锁必须在内层块内释放，再调 save_stats_debounced ——
        //    后者会二次加 entries 锁（parking_lot 非可重入）。
        {
            let mut entries = self.entries.lock();
            if let Some(e) = entries.iter_mut().find(|e| e.id == id) {
                e.last_used_at = Some(Utc::now().to_rfc3339());
                match outcome {
                    RO::Success => {
                        // 成功即清空「持续坏」判据：健康号永不误禁（与 report_success 同款保证）。
                        e.consecutive_passthrough_failures = 0;
                        // 🔴 成功清热（2026-08-16 对抗审查 M1.1）：上游恢复的号**立即**回到
                        // 候选池，不再被余温排除到 60s 窗口尾（此前上游 30s 恢复后该号仍被
                        // 排除整 60s，白白缩水可用池）。只有真成功才清：5xx/429 等失败路径
                        // 不得触碰余温，否则「死号每成功前必先失败一次」的探测就永不清热。
                        e.last_failure_at.set(None);
                        e.success_count = e.success_count.saturating_add(1);
                        e.request_count = e.request_count.saturating_add(1);
                        // request_limit 只保留为观测/告警配置。custom_api 不是 Kiro 凭据，
                        // 达到上限也不能改变管理员明确设置的 enabled 状态。
                    }
                    // 🟢 **瞬态**失败:5xx / 连接错误 / 超时。仅计数供展示,**绝不**进
                    // consecutive_passthrough_failures、绝不 auto-disable。
                    // 中转站抖一下不代表它坏了,failover 到下一个号即可(见 provider 的透传循环)。
                    // 为什么连「看起来像号坏了」的 5xx 也不自动禁用:代挂号是用户自购的
                    // 第三方上游,「自动禁用」的保护对象(被风控/坏掉的 Kiro 号)对它不成立;
                    // 误杀一个只是暂时抖动的代挂号,会把流量引回 Kiro 池——而 Kiro 号
                    // 恰恰可能正被风控,整池不可用正是最需要代挂分流的时候(provider.rs
                    // 透传循环的 429 语义注释即由此定)。真实的号级坏(401/额度耗尽)由
                    // 管理员在面板自行处置,网关绝不代行。
                    RO::ServerError | RO::NetworkError => {
                        e.failure_count = e.failure_count.saturating_add(1);
                    }
                    // 429 无论偶发还是持续都只属于上游结果，不改变 custom_api 的启用状态。
                    RO::RateLimited => {
                        e.consecutive_passthrough_failures = 0;
                    }
                    // 🟢 **客户端请求错误**（400/404/422）：**绝不计入号的健康**。
                    //
                    // 这是坏的请求，不是坏的号 —— 换任何号都一样错。
                    // 实测依据（线上 traces.db）：代挂号 #216 成功率 80.3%（2910/3622）是个**健康号**，
                    // 但它历史上有 712 次 bad_request、其中 **119 次是 ≥3 连**、最长 6 连。
                    // 若把 BadRequest 计入连续失败计数（阈值 3），这个健康号会被误禁 119 次。
                    // 而且代挂号历史 429 次数为 **0**、失败形态**全是** bad_request ——
                    // 也就是说"把 bad_request 当号坏了"恰好会命中唯一真实存在的失败形态。
                    RO::BadRequest => {}
                    // 认证、额度、账户和模型错误也只是代挂上游结果。可以记失败次数供展示，
                    // 但绝不能写 disabled / disabled_reason / disabled_at。
                    RO::AuthFailed
                    | RO::QuotaExhausted
                    | RO::OtherError
                    | RO::AccountSuspended
                    | RO::ModelUnavailable => {
                        e.failure_count = e.failure_count.saturating_add(1);
                        e.consecutive_passthrough_failures = 0;
                    }
                }
            }
        }
        self.save_stats_debounced();
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
    /// - `excluded`: 本次客户端请求**已经试过**的凭据 id。见
    ///   [`Self::acquire_context_excluding`] 的长注释了解为什么需要它。
    ///
    /// # 返回
    /// 命中则返回 `(id, credentials, 在途守卫)`，守卫 Drop 时把该号 inflight -1。
    fn select_next_credential(
        &self,
        model: Option<&str>,
        user_id: Option<&str>,
        excluded: &HashSet<u64>,
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
        let selectable: Vec<&CredentialEntry> = entries
            .iter()
            .filter(|e| self.is_entry_selectable(e, is_opus, model_key))
            .collect();

        // ⭐ 排除集：优先只在「本请求还没试过的号」里选。
        //
        // **全部被排除时必须退化成允许重选，绝不返回 None** —— 否则单号池
        // （或所有号都已试过一轮）会直接变成「无可用凭据」，把一个只是需要重试的
        // 请求报成池子耗尽。排除集是**偏好**而非硬门，这一点是承重的。
        //
        // ⚠️ `selectable` 刻意**不被 move**：它是「排除**前**的全集」，而下方那道 RPM 饱和
        // 硬门是唯一排在排除集**之后**的硬门 —— 它在「fresh 全饱和、但全集里还有未饱和号」
        // 时必须能退化回全集，否则 `select_next_credential` 返 None，而
        // `transient_wait_outcome`（按设计**不吃**排除集，见 `acquire_context_excluding`
        // 的不变量 2）看全池会返 `Available` ⇒ `acquire_context` 的 `Available` 分支零
        // sleep、零 attempt 递增 ⇒ 确定性忙等直到撞 `MAX_RACE_RESELECT` 才 bail。
        // 详见下方硬门处的长注释。
        let fresh: Vec<&CredentialEntry> = if excluded.is_empty() {
            Vec::new()
        } else {
            selectable
                .iter()
                .copied()
                .filter(|e| !excluded.contains(&e.id))
                .collect()
        };
        // 排除集是否**真的**收窄了候选集。为 false 时下方硬门无需退化：
        // fresh 为空时 `available` 已等于全集；excluded 不命中任何可选号时两者也相同。
        let exclusion_narrowed = !fresh.is_empty() && fresh.len() < selectable.len();
        let available: &[&CredentialEntry] = if fresh.is_empty() {
            &selectable
        } else {
            &fresh
        };

        if available.is_empty() {
            return None;
        }

        // 会话亲和性：若该会话已绑定某凭据且当前可用，优先复用，让同一对话粘同一账号
        if self.affinity_enabled.load(Ordering::Relaxed) {
            if let Some(uid) = user_id {
                // ⚠️ 亲和命中是一条 `return` 旁路（跳过下方全部排序键），所以排除集
                // **必须在这里也生效**，否则 failover 会立刻把刚失败的绑定号再选回来 ——
                // 那正是排除集要消除的行为，只加在 filter 处等于没加（本请求 100% 有
                // session_id，亲和默认开）。
                //
                // 排除即视为「本请求内临时解绑」：不动 affinity 表本身（会话结束后仍粘
                // 同一个号，保住 prompt cache），只是这一跳换人。
                if let Some(bound_id) = self.affinity.get(uid).filter(|id| !excluded.contains(id)) {
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
                            return Some(self.commit_selection(entry, model_key));
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
                // 自适应分流排序键（升序 min_by_key）——**共 13 位**，完整定义见本闭包末尾的元组，
                // 那里是唯一权威；本概览只说明各位的作用与次序，改元组时必须同步这里。
                //
                // ① unusable                真不可用(p_avail=0 或 RPM 饱和)沉底 —— 优雅溢出
                // ② starved                 ⭐反饥饿强制探测(0=已饥饿排最前)；排在 ① 之后故不绕硬门
                // ③ prio_key                开关开则按 priority 分层；关时恒 0
                // ④ health_tier             健康 3 档粗门。p_avail = 熔断门×健康×(1-RPM压力)×(1-负载)。
                //                           熔断 Open 的号/族 p_avail=0 自然沉底、半开期按 admit_prob
                //                           软降权。族键连坐：M365 同租户共享一个 health(整族一起沉)，
                //                           IdC/social/api_key 各自 cred:{id} 独立(坚强兜底不受连坐)。
                // ⑤ ramp_tier               ⭐爬坡压力档(治 slew-rate 429，见排序键闭包内的实测依据)
                // ⑥ whitelist_hit           该模型在该号白名单里显式列出 → 0(首选)；通吃号 → 1。
                //                           显式路由软因子(newapi「首选凭据组」的软版)：同健康同爬坡
                //                           档内白名单命中的号优先，白名单号整档饱和时优雅溢出到通吃号。
                //                           全池无白名单时恒 1(均匀) → 零回归。
                // ⑦ inflight_now            ⭐同档内在途最少优先 —— 治惊群核心
                // ⑧ model_calls_now         ⭐该号近窗内被**当前模型**调用的次数(低先)。把爆款模型
                //                           摊到整池，防单模型把部分号顶到饱和而其它号平局分不到。
                //                           排在 inflight 之后：总在途仍是抗惊群主键，模型维度只细分。
                // ⑨ slot_pressure_permille  ⭐在途/自身容量千分比(大池高基数区分)
                // ⑩ rpm_usage_permille      RPM 已用率低的先选(按容量比例分流)
                // ⑪ neg_p_fine              p_avail 精细兜底(含余额加权)
                // ⑫ e.success_count        终身成功数(兜底倒数第二,低负载全平局时生效)
                // ⑬ e.id                    ⭐显式 tie-break(id=创建序=下标序,契约化)
                //
                // ⚠️ 健康分档**不是**首要键（历史注释曾如此描述，已不成立）：①② 先于它。
                // 这是刻意的 —— 真不可用的号必须沉底，饥饿号必须能拿到探测机会，
                // 二者都优先于"谁更健康"。
                // p_avail 已内含 rpm 压力/在途，⑦⑧⑨⑩ 作同档兜底仍保留
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
                // ⚠️ 取自 `selectable`（排除**前**全集）而非 `available`：下方硬门的退化趟会
                // 对被排除的号求 `rpm_of`，而 `rpm_of` 对缺键返回 **0** ⇒ 若这里只装
                // `available` 的 id，一个已饱和的被排除号会被读成 rpm=0＝未饱和而被选出，
                // 把「整池真饱和应背压」错成「放行」。多出的键只是几次 HashMap 插入。
                let cand_ids: Vec<u64> = selectable.iter().map(|e| e.id).collect();
                let rpm_counts = self.rpm.counts_for(&cand_ids);
                let rpm_of = |id: u64| rpm_counts.get(&id).copied().unwrap_or(0);
                // 爬坡计数同样**一次加锁批量取**（与 counts_for 同理由：排序键对每个候选都要
                // 读，逐个加锁会在 entries 临界区内放大锁竞争）。
                let ramp_counts = self
                    .rpm
                    .ramp_counts_for(&cand_ids, StdDuration::from_secs(RAMP_RECENT_SECS as u64));
                let ramp_of = |id: u64| ramp_counts.get(&id).copied().unwrap_or((0, 0));
                // ⭐ 模型级计数同样**一次加锁批量取**（与 counts_for 同理由）。模型为空串
                // （无模型语义的调用）时跳过整段：该维度不参与也不记录，与记录侧对称。
                let model_calls = if model_key.is_empty() {
                    std::collections::HashMap::new()
                } else {
                    self.rpm.model_counts_for(&cand_ids, model_key)
                };
                let model_calls_of = |id: u64| model_calls.get(&id).copied().unwrap_or(0);
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
                    // 喂给 p_avail 的那次决定第④位 health_tier 与第⑪位 neg_p_fine，
                    // 另一次决定第⑦位 inflight_now 与第⑨位 slot_pressure_permille。
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
                    // 只作用在 neg_p_fine（第 ⑪ 位精细兜底键），前面在途/已用率相等才轮到 → 不掀翻 0.7.23 分流。
                    let p_weighted = p * self.balance_factor(e.id, e.total_credits_used);
                    // p_avail 精细值(含余额加权)降到第 ⑪ 位兜底,保留确定性 + 避免同档抖动。
                    let neg_p_fine = -((p_weighted * 1000.0) as i64);
                    // 优先级键仅在开关开启时参与首排;关闭时置 0(不影响原有均衡)。
                    let prio_key = if prio_first {
                        e.credentials.priority
                    } else {
                        0
                    };
                    // ⭐ 高基数负载维度（企业级分流精度）：在途占**该号自身容量**的千分比。
                    //
                    // 为什么需要它：第⑦位是**裸 inflight**（绝对值）。400 号池中同 health_tier
                    // 的号 inflight 普遍落在 5~7，只有 3 档区分度 → 大量候选在 ⑦ 上完全平局；
                    // 若各号 rpm_limit 相同，第⑩位 rpm_usage_permille 也高度重合 →
                    // 一路平局到第⑪位 neg_p_fine。实测（5 号小池即已如此）：
                    // gini(inflight) median 0.524、最热/最冷 2.4x。
                    //
                    // 为什么不替换⑦：⑦的语义是"绝对在途最少优先"，是治惊群的核心
                    // （突发涌入时必须优先给完全空闲的号，不管它容量多大）。本维度是
                    // **容量归一**后的相对压力，回答的是"按各自体量谁更该接"——两者互补：
                    //   同 inflight 但容量不同的号，本维度把大容量号排前（它更能扛）；
                    //   同容量不同 inflight 的号，⑦已先分开，本维度不改变其相对次序。
                    //
                    // 与改动④（自适应 LOAD_REF）不重复惩罚：那一项作用在 p_avail 内部、
                    // 归一分母是**池平均在途**（跨号横向比较）；本项归一分母是**该号自身容量**
                    // （纵向比较自身余量），信息源不同、且分别落在排序键的第⑨位与第⑩位，
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
                    // 根因是**低负载下前面大多数键会全部平局**：
                    //   - 全池健康 → ①②③④ 恒等；低负载下爬坡样本不足 → ⑤ 恒 0；
                    //     全池无白名单 → ⑥ 恒 1；
                    //   - 每秒几个请求、响应即归零 → `inflight_now` ⑦ 大部分时刻恒 0，
                    //     连带 model_calls_now ⑧ / slot_pressure_permille ⑨ /
                    //     rpm_usage_permille ⑩ 恒 0；
                    //   - ⑪ 的 neg_p_fine 在全健康时高度重合（余额加权接近 1.0）。
                    // 而 `min_by_key` 在全平局时**恒返回第一个元素**，于是流量持续偏向
                    // `entries` 里下标靠前的号 —— 与观测到的 idx0 最热完全一致。
                    //
                    // ⑫ `success_count`（终身成功数）本该纠正这个，但它排在末位：
                    // 刚服务过的号一旦滑窗过期，⑦⑨⑩ 就回到 0 并再次胜出，⑫ 根本轮不到。
                    //
                    // ## 为什么不把 ⑫ success_count 提前来纠正
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
                    // ⭐ 爬坡压力档（slew-rate）：上游惩罚**速率的跃升**，不是绝对吞吐。
                    // 判据与分档共用 `scheduling::ramp_tier_of`（与透传池同一函数，
                    // 2026-08-16 收敛防分叉）——近 `RAMP_RECENT_SECS` 折算分钟值 vs
                    // 整 `RPM_WINDOW_SECS` 窗口均值比。比值越大 = 正在被猛灌 →
                    // 档位越高 → 同健康档内让路给「已经平稳在跑」的号。
                    //
                    // 为什么是**排序键而不是硬门**：硬门在单号池（或全池都在爬坡）时会让
                    // 请求无号可选 → 网关自造 503，而放它过去最坏只是拿一个真实 429。
                    // 排序键让「有平稳号可用时优先用平稳号」，没有时照常放行。
                    //
                    // 为什么排在 inflight **之前**：429 在 ~1s 返回、成功要 3s+，所以正在被
                    // 打爆的号 inflight 反而恒低（实测 507 inflight=1、健康的 508 inflight=13）
                    // → 若让 inflight 先排，失败越快的号越显得空闲、越被优先选中（正反馈）。
                    let ramp_tier = {
                        let (recent, total) = ramp_of(e.id);
                        crate::kiro::scheduling::ramp_tier_of(recent, total)
                    };
                    // ⑥ 模型→渠道路由偏好（newapi「首选凭据组」的软版）：该模型在该号
                    // 白名单里**显式列出**时置 0（首选），未设白名单的「通吃号」置 1。
                    // 排在健康/爬坡之后、负载之前：健康与 429 风险仍是主键，白名单只在
                    // 同健康同爬坡档内做路由偏好，坏号不因白名单命中而插队；白名单号整档
                    // RPM 饱和时第一趟硬门自然把它们剔除，流量优雅溢出到通吃号。
                    // 全池无白名单时恒 1（均匀）→ 零回归；模型为空串时恒 1 不参与。
                    // ⚠️ 已知边界（2026-08-14 审查 m1 文档化）：白名单号未饱和时通吃号
                    // 零流量——其健康观测完全冻结（无样本）。N=1 且白名单号容量配置
                    // 错误时流量会一直压在该号上；通吃号要接流量只能等白名单号饱和。
                    let whitelist_hit = if model_key.is_empty() {
                        1u8
                    } else {
                        let explicit = e
                            .credentials
                            .allowed_models
                            .as_deref()
                            .is_some_and(|l| !l.is_empty());
                        u8::from(!(explicit && e.credentials.allows_model(model_key)))
                    };
                    // ⑧ 模型级近期调用数（2026-08-14 新增）：该号近窗内被**当前模型**调用的次数，
                    // 低者优先。与 inflight 的差别：inflight 是全模型混在一起的总在途，
                    // 本键回答「这个号最近是不是正在被这个爆款模型猛灌」——同一模型跨多号
                    // 同时段热时，优先选该模型计数最少的号，把热点模型摊到整池，防单个
                    // 爆款模型把部分号顶到饱和而其它号因平局分不到。排在 inflight 之后：
                    // 总在途仍是抗惊群主键，模型维度只做同档细分。模型为空时恒 0（与
                    // 记录侧对称，无模型语义的调用不参与）。阈值不新增：饱和判定仍复用
                    // 每凭据 rpm_limit，模型级只是分流计数。
                    let model_calls_now = if model_key.is_empty() {
                        0u32
                    } else {
                        model_calls_of(e.id)
                    };
                    (
                        unusable,               // ① 真不可用沉底(优雅溢出)
                        starved,                // ② ⭐饥饿号强制探测(0=饥饿排前)
                        prio_key,               // ③ 开关开:按优先级分层;关:恒 0
                        health_tier,            // ④ 健康 3 档粗门(坏号沉档)
                        ramp_tier,              // ⑤ ⭐爬坡压力档(治 slew-rate 429,见上)
                        whitelist_hit,          // ⑥ 白名单命中(该模型显式路由的号优先)
                        inflight_now,           // ⑦ ⭐同档内在途最少优先(治惠群核心)
                        model_calls_now,        // ⑧ ⭐该模型近期调用数(爆款模型摊整池)
                        slot_pressure_permille, // ⑨ ⭐在途/自身容量千分比(大池高基数区分)
                        rpm_usage_permille,     // ⑩ RPM 已用率低的先选(按容量比例分流)
                        neg_p_fine,             // ⑪ p_avail 精细兜底(含余额加权)
                        // ⑫ 兜底二合一：(终身成功数, 显式 tie-break id)。
                        // ⚠️ Rust 元组 Ord 只实现到 12 元素——13 位编译不过（E0277 实测），
                        // 且 ⑬ 单列会破坏 12 位结构；把 success_count 与 e.id 合并为
                        // 一个二元组位（id=创建序=下标序，契约化，平局恒选最早创建）。
                        (e.success_count, e.id),
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
                // 未饱和判据抽成**一处**：下方降级趟必须与第一趟同口径。
                // 同一判据各写一份正是本函数历史缺陷的成因（判据漂移 → 两趟结论不一致）。
                let is_not_saturated = |e: &&CredentialEntry| {
                    rpm_of(e.id) < self.effective_saturation_limit(e.credentials.rpm_limit)
                };
                // 背压开关**只读一次**：下方 `saturation_pool` 与 `else if` 两处都要用它，
                // 分两次读原子镜像时 TIER3 热重载可能在中间翻转 ⇒ 同一次选号内自相矛盾
                // （与排序键里 inflight 单读收口同一理由）。
                let hard_gate = self.rpm_hard_gate_overload_wait.load(Ordering::Relaxed);
                // ⭐ 排除集降级趟：RPM 饱和是本函数**唯一**排在排除集之后的硬门，
                // 所以排除集的降级必须在这里再做一次。
                //
                // 上方 `available` 定义处的 `fresh.is_empty()` 那道降级只覆盖
                // 「排除集吃掉了全部可选号」，覆盖不到「fresh 非空、但 fresh 里的号全部
                // RPM 饱和，而被排除的号里还有未饱和的」—— 那时本硬门返回 None，而
                // `transient_wait_outcome` 按设计**不吃排除集**（见
                // `acquire_context_excluding` 不变量 2），它遍历全池看见那个未饱和的
                // 被排除号 ⇒ 返回 `Available` ⇒ `acquire_context` 的 `Available` 分支
                // 零 sleep、零 attempt 递增 ⇒ 忙等重选。
                //
                // 且这不是概率性抖动而是**确定性**死循环：忙等期间 `commit_selection`
                // 从未被调用 ⇒ RPM 计数不变 ⇒ 每轮判定完全相同 ⇒ 必然打满
                // `MAX_RACE_RESELECT` 才 bail。线上 2h 内 351 次；bail 文案不含
                // `retry_after` ⇒ 面板只记成 OtherError + credential_id IS NULL，查不到根因。
                //
                // 两种情形必须区分，**只有 (b) 降级**：
                //   (a) 全池**真**饱和 ⇒ 保持返 None，由背压等 RPM 恢复（现行为正确）。
                //   (b) 仅因排除集收窄才「看起来」全饱和 ⇒ 降级回 `selectable` 重选。
                // 排除集是**偏好而非硬门**（不变量 1）：重选一个本请求已试过的号，
                // 严格优于空转 64 轮后失败。
                //
                // 只在 `hard_gate` 开时降级：背压关时下方软门回退恒返回 Some，
                // 本缺陷不存在，故不碰那条路径（零回归）。
                let saturation_pool: &[&CredentialEntry] = if hard_gate
                    && exclusion_narrowed
                    && !available.iter().any(is_not_saturated)
                    && selectable.iter().any(is_not_saturated)
                {
                    tracing::debug!(
                        excluded_len = excluded.len(),
                        fresh_len = available.len(),
                        selectable_len = selectable.len(),
                        "本请求已试过的号里还有未饱和的，而未试过的号全已 RPM 饱和：\
                         降级回全体可选号重选（排除集是偏好而非硬门），避免选号忙等"
                    );
                    &selectable
                } else {
                    available
                };
                let non_saturated: Vec<_> = saturation_pool
                    .iter()
                    .copied()
                    .filter(|e| is_not_saturated(e))
                    .map(|e| (sort_key(e), e))
                    .collect();
                if !non_saturated.is_empty() {
                    non_saturated
                        .into_iter()
                        .min_by_key(|(k, _)| *k)
                        .map(|(_, e)| e)
                } else if hard_gate {
                    // 整池饱和 + 背压开:返回 None,上游 acquire_context 等待恢复(受 MAX_TRANSIENT_WAIT 限)。
                    // 走到这里说明是**真**饱和：降级趟已确认全体可选号里也没有未饱和的。
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

        Some(self.commit_selection(selected, model_key))
    }

    /// 提交一次选号：在持有 `entries` 锁的前提下原子占用在途名额并记录 RPM。
    ///
    /// 必须在 `select_next_credential` 的 `entries.lock()` 临界区内调用，
    /// 以保证 `inflight += 1` 相对其它并发选号是原子可见的。
    /// 全池冷却时的**兜底选号**：只放宽「冷却」这一道门，其余硬门逐条保留。
    ///
    /// # 为什么需要它（实测代价）
    ///
    /// 所有号都在冷却时，`acquire_context` 原先直接 `bail` ⇒ 客户端拿到的是
    /// **网关自造的 429，上游根本没被请求过**。逐小时实测 `credential_id IS NULL`
    /// 占比：20 点 8.2% / 21 点 9.5% / **22 点 15.3%** / 23 点 1.1%。
    /// （00 点 0% 是池子恰好健康，不是已修好。）
    ///
    /// 判断依据来自对照实现（kiro-rs 的 VPS 本地补丁）的注释：
    /// **最坏也只是再拿一个上游 429（真实响应），好过网关自造的 503/429。**
    /// 真实 429 还带 `Retry-After`，客户端能据此退避；自造的那个不带任何上游信息。
    ///
    /// # 三条不变量（改这里前必读）
    ///
    /// 1. **绝不放行 `disabled` 的号。** 那会绕过配额耗尽 / 账号封禁 /
    ///    refreshToken 失效这些**终态**判定，变成反复打已死的号。
    ///    本函数通过复用 `is_entry_selectable` 保证这一点 —— 那里第一道就是 `disabled`。
    /// 2. **只放宽冷却，其余硬门全保留**（custom_api 隔离 / opus 订阅 /
    ///    `allows_model` 成本白名单 / `model_blocklist`）。故实现方式是给
    ///    `is_entry_selectable` 加 `ignore_cooldown` 参数，而**不是**在这里重写一遍过滤 ——
    ///    重写就会漂移，而漂移过的历史后果是 `acquire_context` 忙等死循环（见
    ///    `transient_wait_outcome` 的长注释）。
    /// 3. **排除集仍然生效**：优先选本请求没试过的；全被排除时退化成允许重选
    ///    （与 `acquire_context_excluding` 的不变量 1 同款）。
    /// 4. **一次只放行一个号。** 返回类型（单个 guard）已由构造保证这一点；改这里时
    ///    别改成"把全池都放出去"—— 那是惊群，会让一批请求同时打进同一批冷却号。
    ///
    /// # 选谁：冷却深度档 + id 轮转（2026-08-06 改，治「兜底聚集单号」）
    ///
    /// 最早的实现是 `min_by_key(冷却剩余)`，**完全确定性且无任何轮转**：冷却到期时刻一旦
    /// 排定就不再变，于是同一个号被反复选中 —— 实测 #578 近 3 小时拿 128 次、单分钟峰值 63。
    ///
    /// 排序键现在是 `(FallbackCooldownTier, 轮转序, id)`：
    ///
    /// - **第一维（深度档）只区分「值得试 / 铁定白扔」**，见 [`FallbackCooldownTier`]。
    ///   不能整个丢掉它：`AuthenticationFailed`/`QuotaExhausted` 的冷却是 86400s，
    ///   纯轮转会把请求送给一个铁定失败的号，而同池可能有个几秒后就恢复的
    ///   `RateLimitExceeded` 号。
    /// - ⚠️ 但这一维**不能按剩余秒数细分**（上一版按 `剩余/60` 分档，就是这里出的缺口）：
    ///   429 的冷却时长由上游 `Retry-After` 给、`SuspiciousActivity` 会指数升到 80s+，
    ///   于是「都会自愈」的号仍跨档 ⇒ 只剩一个号在最前档时它被重新钉住，
    ///   且每次放行都给它续一段新冷却、让它继续留在最前档（自我维持）。
    ///   同一档内谁先恢复几十秒，对兜底来说不重要（本就预期吃一个真实 429），
    ///   **摊开打才重要**。
    /// - **第二维（轮转）打散同档内的聚集**：按 id 升序取「游标之后的第一个」，
    ///   到尾则回绕。见 `fallback_cursor` 的文档（为什么是游标而不是随机）。
    ///
    /// 轮转是**确定性**的：给定 (候选集, 游标) 结果唯一，可测可复盘。
    fn select_ignoring_cooldown(
        &self,
        model: Option<&str>,
        excluded: &HashSet<u64>,
    ) -> Option<(u64, KiroCredentials, InflightGuard)> {
        let entries = self.entries.lock();
        let is_opus = model
            .map(|m| m.to_lowercase().contains("opus"))
            .unwrap_or(false);
        let model_key = model.unwrap_or("");

        // 复用同一个判据函数（只放宽冷却），杜绝两处过滤条件漂移。
        let candidates: Vec<&CredentialEntry> = entries
            .iter()
            .filter(|e| self.is_entry_selectable_inner(e, is_opus, model_key, true))
            .collect();
        if candidates.is_empty() {
            return None;
        }
        // 排除集：偏好没试过的，全被排除时退化（同 select_next_credential）。
        let pool: Vec<&CredentialEntry> = if excluded.is_empty() {
            candidates.clone()
        } else {
            let fresh: Vec<&CredentialEntry> = candidates
                .iter()
                .copied()
                .filter(|e| !excluded.contains(&e.id))
                .collect();
            if fresh.is_empty() { candidates } else { fresh }
        };

        // 排序键：(冷却深度档, 轮转序, id)。理由见函数文档。
        let cursor = self.fallback_cursor.load(Ordering::Relaxed);
        let best = pool.into_iter().min_by_key(|e| {
            let tier = self.fallback_cooldown_tier(e.id);
            // 轮转序：id > cursor 的排在前（0），id <= cursor 的排在后（1）——
            // 即「从游标之后继续走，走到尾回绕」。命名按「已被游标走过」取，
            // 因为该位为 1 的含义是"这一格本轮已经轮过了"，不是"在游标之后"。
            // 同档内再按 id 升序，使整条键确定（给定候选集与游标，结果唯一）。
            let already_passed = u8::from(e.id <= cursor);
            (tier, already_passed, e.id)
        })?;
        // 游标只在真的放行时前进：探测失败/候选为空都不该让轮转空转一格。
        self.fallback_cursor.store(best.id, Ordering::Relaxed);
        tracing::warn!(
            "全池均在冷却，兜底放行凭据 #{}（{:?} 档内按 id 轮转，游标前值 #{}；拿真实上游 429 好过网关自造 429）",
            best.id,
            self.fallback_cooldown_tier(best.id),
            cursor
        );
        Some(self.commit_selection(best, model_key))
    }

    /// 该号当前冷却的**深度档**，供 [`Self::select_ignoring_cooldown`] 的排序键第一维。
    ///
    /// 判据取 `reason`（能不能自愈）而非剩余秒数，理由见 [`FallbackCooldownTier`]：
    /// 按秒数细分会让「都会自愈」的号跨档，从而把最前档里唯一的号重新钉死。
    /// 剩余秒数只用来兜住「上游明确要求等超过短冷却上界」这一种反常形态。
    fn fallback_cooldown_tier(&self, id: u64) -> FallbackCooldownTier {
        match self.cooldown.check_cooldown(id) {
            // 无记录：兜底路径理论上不该走到这里，但选号与 check 之间冷却可能恰好到期。
            None => FallbackCooldownTier::Ready,
            Some((_, remaining)) if remaining.is_zero() => FallbackCooldownTier::Ready,
            Some((reason, remaining)) => {
                if reason.is_auto_recoverable()
                    && remaining.as_secs() <= MAX_RETRY_AFTER_COOLDOWN_SECS
                {
                    FallbackCooldownTier::Shallow
                } else {
                    FallbackCooldownTier::Deep
                }
            }
        }
    }

    fn is_entry_selectable(&self, entry: &CredentialEntry, is_opus: bool, model: &str) -> bool {
        self.is_entry_selectable_inner(entry, is_opus, model, false)
    }

    /// `ignore_cooldown = true` 时**只**跳过冷却那一道，其余硬门逐条照旧。
    /// 供 [`Self::select_ignoring_cooldown`] 使用，见那里的不变量 2。
    fn is_entry_selectable_inner(
        &self,
        entry: &CredentialEntry,
        is_opus: bool,
        model: &str,
        ignore_cooldown: bool,
    ) -> bool {
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
        if !ignore_cooldown
            && self.cooldown_enabled.load(Ordering::Relaxed)
            && !self.cooldown.is_available(entry.id)
        {
            return false;
        }
        if self.rate_limit_enabled.load(Ordering::Relaxed)
            && self.rate_limiter.check_rate_limit(entry.id).is_err()
        {
            return false;
        }
        // ⚠️ inflight 只在**饱和级**才作硬门槛（2026-08-16 迁移差距 P1 起）。
        // 历史教训：曾被硬编码 inflight < 1 阻塞成"每号同时只 1 个请求"，多客户端下
        // 多余请求全排队 = 假性限流、体感极慢。故正常并发形态下 inflight 绝不阻塞：
        // 在途只进 select_next_credential 的排序键（⑦ 在途最少优先），把并发自然分摊；
        // 号不够时并发落到同一号由 RPM 软降权调节，而不是把请求卡在网关里排队干等。
        // 唯一例外是 [`CREDENTIAL_MAX_CONCURRENCY`]（默认 16）这个饱和级硬门——正常
        // 并发远够不到（常态每号在途 ~8.6），只有单号被灌爆时才触发，防止上游风控。
        // 它与 RPM 饱和硬门（L4 背压）同构：同为「保护上游」的饱和护栏，达限即跳过，
        // 全部达限由 transient_wait_outcome 的镜像（ConcurrencyFull）短等重试。
        if at_max_concurrency(entry) {
            return false;
        }
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
            // 恢复窗口 = 该号第 `fresh - limit + 1` 老命中再过 (60s - age) 就过期、窗口内
            // 回落到限值内。limit 被热调低（fresh > limit）时比"等最老一条过期"更精确；
            // fresh == limit 时 k=1，与旧行为（等最老一条）完全一致。背压关时不计(RPM 饱和号
            // 在软门下仍是立即可选候选,不等待——保持默认行为)。
            if self.rpm_hard_gate_overload_wait.load(Ordering::Relaxed)
                && self.is_rpm_saturated_with_limit(entry.id, entry.credentials.rpm_limit)
            {
                // 与 is_rpm_saturated_with_limit 同源同口径的限值（headroom 折扣后），
                // 保证「饱和判定」与「恢复目标」对同一阈值说话。
                let limit = self.effective_saturation_limit(entry.credentials.rpm_limit);
                let fresh = self
                    .rpm
                    .counts_for(&[entry.id])
                    .get(&entry.id)
                    .copied()
                    .unwrap_or(0);
                let release_index = fresh.saturating_sub(limit) + 1;
                let recover = self
                    .rpm
                    .kth_oldest_age(entry.id, release_index)
                    .map(|age| self.rpm.window().saturating_sub(age))
                    .unwrap_or_else(|| StdDuration::from_secs(1));
                // 至少等 250ms,避免 0 等待空转;上限由外层 MAX_TRANSIENT_WAIT 兜底。
                waits.push((
                    recover.max(StdDuration::from_millis(250)),
                    WaitReason::RpmRecovery,
                ));
                continue;
            }

            // 并发上限硬门镜像（2026-08-16，迁移差距 P1）——**必须与 is_entry_selectable_inner
            // 逐条对齐**（见本函数开头那组硬门注释的忙等陷阱）：达限号在 select 里被过滤，
            // 这里若仍判「立即可用」⇒ select 返 None 而本函数返 Available ⇒ acquire_context
            // 的 Available 分支零 sleep 零递增 ⇒ 确定性忙等热循环。in-flight 连续释放
            // （流结束即 -1，通常毫秒级），给一个短固定等待即可，与 RPM 背压同族。
            if at_max_concurrency(entry) {
                waits.push((
                    StdDuration::from_millis(250),
                    WaitReason::ConcurrencyFull,
                ));
                continue;
            }

            // 走到这里说明该号既未冷却/限流、也未被背压计为饱和、也未达并发上限 → 立即可用候选。
            // inflight 在饱和级以下绝不作为阻塞门槛(在途只进排序键,并发直接落它)。
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

    fn commit_selection(
        &self,
        entry: &CredentialEntry,
        model: &str,
    ) -> (u64, KiroCredentials, InflightGuard) {
        let guard = InflightGuard::acquire(entry.inflight.clone());
        self.rpm.record(entry.id);
        // 模型级 RPM 分流计数与每凭据计数**同点记录**（同一临界区、同一口径，见
        // `record_passthrough_result` 处「全仓只有两处 rpm.record」的说明）。
        // `model` 是选号时的原始模型名（与白名单/模型黑名单同源）；模型为空串
        // （无模型语义的调用，如 MCP）时不记，避免空键条目堆积。
        if !model.is_empty() {
            self.rpm.record_model(entry.id, model);
        }
        // 成功选到号 ⇒ 清零「连续全池不可用」计数（与透传路径 select_custom_api 对称，
        // 见该字段的文档）。任一路径成功都代表池子可用，计数恢复后立刻回到可重试语义。
        // 2026-08-14 补齐：此前只有透传选号清零，纯 Kiro 池短暂抖动会把计数一路累到
        // 终态升级阈值，误判「永久故障」。
        self.consecutive_pool_unavailable
            .store(0, Ordering::Relaxed);
        // 反饥饿探测的时间基准（见 STARVATION_PROBE_SECS）。用 Cell 而非 &mut：
        // 本函数按既有约定收 &CredentialEntry（选号闭包里持的是不可变引用）。
        entry.last_selected_at.set(Instant::now());
        (entry.id, entry.credentials.clone(), guard)
    }

    /// 该号近 60s RPM 是否达到容量上限（按 id 判饱和）。
    ///
    /// ⚠️ 本函数**绝不能在已持 entries 锁时调用**（parking_lot 非重入会死锁）；
    /// 选号热路径已持锁，必须直接传入该号的凭据级 rpm_limit 走本函数，
    /// 避免二次锁 entries。测试场景同理：测试通常在未持锁上下文，直接传
    /// 该号容量的 `Some(per_cred_limit)`（无 per-cred 时传 `None` 走全局/兜底）。
    ///
    /// 容量优先级:凭据级 `rpm_limit`(体质好的号可设高) > 全局 `credential_rpm_limit`(>0)
    /// > **默认高水位兜底**。默认兜底(SATURATION_FALLBACK_RPM=30)是"默认配置也最优"的
    /// 关键:两者都没设时,不再"恒不饱和→affinity 死粘单号打爆"(retry 慢根因),而是在
    /// ~30rpm/号(正好在上游 USER_REQUEST_RATE_EXCEEDED 硬限之前)判饱和,让 affinity
    /// 解绑 + balanced 分流到空闲号。体质好的号设 per-cred rpm_limit=100 即用 100,
    /// 弱号/默认用 30 兜底。
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
        // 并发上限硬门（迁移差距 P1）：亲和命中是同一条 `return` 旁路（跳过全部排序键），
        // 排序键里那道 inflight 均衡完全够不着它 —— 单会话并行工具调用会把绑定号灌爆
        // （会话钉死一个号、在飞只增不减），必须在这里同样封堵（镜像 kiro-rs-admin
        // 亲和路径的 `is_concurrency_exceeded` 检查）。达限不复用 → 落 balanced 分流
        // 到未达限的号；全部达限时由排序键/背压按最不坏处理。
        if at_max_concurrency(entry) {
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
            priority_layered: is_priority_mode || self.priority_in_balanced.load(Ordering::Relaxed),
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
    ///
    /// ⚠️ 这是**目标**，不是实测。要实测入站速率用 [`Self::observed_inbound_rpm`]。
    /// 面板的 `inboundCurrentRpm` 曾错用本函数，导致「当前 RPM」恒等于「目标 RPM」，
    /// 实测差一个数量级（面板 500 / 客户端实际 50~70）。
    pub fn inbound_target_rpm(&self) -> u32 {
        self.throttle.current_target_rpm()
    }

    /// 最近 60 秒**实测**入站 RPM（客户端请求数，不含 failover 重试）。
    ///
    /// 与「逐号 RPM 之和」量纲不同：后者统计上游尝试数，两者比值即重试放大倍数
    /// （2026-08-06 实测 4.59×）。并排展示时必须标注清楚，否则读者会以为整形没生效。
    pub fn observed_inbound_rpm(&self) -> u32 {
        self.throttle.observed_inbound_rpm()
    }

    /// 累计放行的客户端请求数（用于对账滑窗是否在正常滚动）。
    pub fn inbound_admitted_total(&self) -> u64 {
        self.throttle
            .admitted_total
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// AIMD 可观测三元组：`(累计排队次数, 累计降档次数, 累计升档次数)`。
    ///
    /// 见 [`crate::kiro::throttle::InboundThrottle::aimd_counters`] 的完整理由：
    /// 这三个数此前是只写不读的死代码，而它们是判断「整形是否在起作用、是否卡在下限」
    /// 的唯一依据（先修度量，再谈调参）。
    pub fn inbound_aimd_counters(&self) -> (u64, u64, u64) {
        self.throttle.aimd_counters()
    }

    /// 逐号 RPM 之和 = **上游实际承受**的尝试速率（含 failover 重试）。
    ///
    /// 与 [`Self::observed_inbound_rpm`] 的比值就是重试放大倍数。两者必须并排展示：
    /// 只看前者会以为整形没生效（面板 500 而客户端 50），只看后者会以为上游很闲
    /// （客户端 50 而上游其实在承受 600+）。2026-08-06 实测放大 4.59×。
    ///
    /// 用 `counts_for` 一次加锁批量读，而不是逐号 `count()`：后者在 N 号池上是 N 次加锁，
    /// 而这个函数会被面板轮询调用。
    pub fn observed_upstream_rpm(&self) -> u32 {
        let ids: Vec<u64> = {
            let entries = self.entries.lock();
            entries.iter().filter(|e| !e.disabled).map(|e| e.id).collect()
        };
        if ids.is_empty() {
            return 0;
        }
        self.rpm.counts_for(&ids).values().copied().sum()
    }

    /// MCP「无号直连」用：从凭据池里找第一个「带 Kiro Bearer token」的凭据。
    ///
    /// # 与 [`Self::acquire_context`] 的差异（承重）
    ///
    /// `acquire_context` 的选号要过 `is_entry_selectable` 全部门槛（禁用 / 冷却 /
    /// custom_api 结构性排除等）——纯 custom_api 透传池或全池禁用时它**选不到号**，
    /// WebSearch 快路径的 MCP 调用因此失败（502）。而 MCP（web_search）调用本质只
    /// 依赖一个有效的 Kiro Bearer token（kiro-gateway 的 mcp_tools.py 证明：只带
    /// `Authorization: Bearer` + `x-amzn-codewhisperer-optout` + `Content-Type` 即可
    /// 调通 `runtime.{region}.kiro.dev/mcp`，**不依赖 profileArn**），不该被对话
    /// 路径的选号门槛绑架。
    ///
    /// 本方法刻意**绕过选号门槛**：只要凭据带 Kiro token 就直接可用——
    /// - `access_token` 非空（OAuth 号优先：直连 URL 是 `runtime.*.kiro.dev/mcp`，
    ///   IDE 协议；冷却中的号 token 仍有效，可直连）
    /// - `kiro_api_key` 非空（ksk_ 号，永不过期，但属于 q.* CLI 协议，排最后）
    ///
    /// 唯二不绕过的：
    /// - **disabled 跳过**（M3）：禁用是网关自己的惩罚决策（风控/额度/连败），
    ///   直连绕过它自相矛盾——全池禁用时返回 `None`，不拿被惩罚的 token 满速打上游。
    ///   冷却**不**检查：冷却不是惩罚，token 本身有效，直连照常可用。
    /// - custom_api 代挂号的 `api_key` 是**中转站密钥**，不是 Kiro token，不算
    ///   （纯 custom_api 池下本方法返回 `None`，由调用方降级现状错误）。
    ///
    /// 轮转（M3）：多 OAuth 候选时优先「曾成功过」的号（`success_count > 0` =
    /// [`Self::has_ever_succeeded`]，token 更可能仍有效），全部未成功过才回退第一个
    /// OAuth，再回退 ksk_ ——避免确定性首匹配把并发全压到第一个号，也避免把
    /// ksk_ 送到 IDE MCP 主机上抢在可用 OAuth 前面。
    ///
    /// 返回 `(凭据 id, 凭据, token)`；`None` = 池里没有任何带 Kiro token 的凭据。
    pub fn acquire_mcp_direct_token(&self) -> Option<(u64, KiroCredentials, String)> {
        static EMPTY: std::sync::LazyLock<HashSet<u64>> =
            std::sync::LazyLock::new(HashSet::new);
        self.acquire_mcp_direct_token_excluding(&EMPTY)
    }

    /// 同 [`acquire_mcp_direct_token`]，但跳过 `exclude` 里的 id（同请求 401 后换号）。
    pub fn acquire_mcp_direct_token_excluding(
        &self,
        exclude: &HashSet<u64>,
    ) -> Option<(u64, KiroCredentials, String)> {
        let entries = self.entries.lock();
        // 单遍遍历：OAuth 候选按 has_ever_succeeded 优先；ksk_ 只记作回退，
        // 不再命中即返回（直连 URL 是 IDE 主机，ksk_ 属于 CLI）。OAuth token
        // 可能已过期，失败由调用方降级现状，不强求刷新。
        let mut first_successful_oauth: Option<(u64, KiroCredentials, String)> = None;
        let mut first_oauth: Option<(u64, KiroCredentials, String)> = None;
        let mut first_ksk: Option<(u64, KiroCredentials, String)> = None;
        for e in entries.iter() {
            if e.disabled || exclude.contains(&e.id) {
                continue;
            }
            if let Some(key) = e.credentials.kiro_api_key.as_deref() {
                if !key.trim().is_empty() && first_ksk.is_none() {
                    first_ksk = Some((e.id, e.credentials.clone(), key.to_string()));
                }
            }
            if let Some(tok) = e.credentials.access_token.as_deref() {
                if !tok.trim().is_empty() {
                    let cand = (e.id, e.credentials.clone(), tok.to_string());
                    if e.success_count > 0 {
                        if first_successful_oauth.is_none() {
                            first_successful_oauth = Some(cand);
                        }
                    } else if first_oauth.is_none() {
                        first_oauth = Some(cand);
                    }
                }
            }
        }
        first_successful_oauth.or(first_oauth).or(first_ksk)
    }

    pub async fn acquire_context(
        &self,
        model: Option<&str>,
        user_id: Option<&str>,
    ) -> anyhow::Result<CallContext> {
        static EMPTY: std::sync::LazyLock<HashSet<u64>> = std::sync::LazyLock::new(HashSet::new);
        self.acquire_context_excluding(model, user_id, &EMPTY).await
    }

    /// 选号并排除「本次客户端请求已经试过的号」。
    ///
    /// # 为什么必须有排除集（2026-08-04，线上实测的核心缺陷）
    ///
    /// `acquire_context` 原先没有任何「别再选它」的入参，于是 provider 的 failover
    /// 循环**下一跳可能立刻重选刚刚失败的同一个号**。唯一阻止这件事的机制是
    /// `is_entry_selectable`（`:3033`）里那道冷却硬门：
    ///
    /// ```text
    /// if self.cooldown_enabled.load(..) && !self.cooldown.is_available(entry.id) { return false }
    /// ```
    ///
    /// 也就是说：**`cooldownEnabled=false` 时 failover 事实上不存在**（换号纯靠排序键
    /// 的软降权，而 3 号池里降权后它照样是候选之一）。线上正是 `cooldownEnabled=false`，
    /// 一个真实 429 因此被放大成连环 429 —— 用户朋友对照 kiro-rs 时抓到的就是这条。
    ///
    /// 排除集把「跳过刚失败的号」从**配置依赖**变成**结构保证**：
    /// 冷却开或关、缩放多少，failover 都真的换人。
    ///
    /// # 三条不变量（改这里前必读）
    ///
    /// 1. **排除是偏好、不是硬门**：全部候选都被排除时退化成允许重选
    ///    （见 `select_next_credential` 里的 `if fresh.is_empty()`）。否则单号池
    ///    或「一轮试完」会把可重试请求报成池子耗尽。
    /// 2. **`transient_wait_outcome` 不吃排除集**：它回答的是「池子里还有没有号将要恢复」，
    ///    与「本请求还想不想再试它」是两个问题。给它加排除集会让「一轮试完后」误判
    ///    `NoCandidate` → bail「所有凭据均已禁用」，而池子明明健康。
    /// 3. **亲和旁路也要排除**（见 `select_next_credential` 内的 `filter`）：那是一条
    ///    `return`，只在 filter 处排除等于没排除。
    pub async fn acquire_context_excluding(
        &self,
        model: Option<&str>,
        user_id: Option<&str>,
        excluded: &HashSet<u64>,
    ) -> anyhow::Result<CallContext> {
        // 注意:入站整形闸门**不在这里**。acquire_context 会被 failover 循环每跳调用,
        // 若在此扣令牌 → 一个客户端请求 failover N 次就扣 N 个令牌 + fast-fail 空转白扣(review Finding 1)。
        // 整形应针对"客户端请求"一次,故闸门上移到 provider 调用入口(acquire_admission,循环外只过一次)。
        let total = self.total_count();
        // 内层尝试预算需与 provider 层的外层重试预算同量级放开：
        // 以可用凭据数为下限，保证内层不会在外层遍历完所有可用号之前就先耗尽。
        // （历史上仅 total*MAX_FAILURES，当可用数因禁用波动大时可能过紧）
        //
        // 🔴 下限必须用 `kiro_selectable_count()` 而非 `available_count()`（2026-08-10 修）：
        // 本函数是 **Kiro 路径**的选号入口，而 `available_count()` 含 custom_api 代挂号
        // （它们被 `is_entry_selectable_inner` 一律拒绝，Kiro 路径永远选不到）。
        // 注释自己写的「以**可用凭据数**为下限」，要的正是「Kiro 可用数」——用错了函数。
        //
        // 用错的后果：纯代挂池（线上现状）时 total=N、available=N ⇒ 预算 3N，而一个号都选不到
        // ⇒ 全部空转在 `NoCandidate` 分支上，**预算越大失败越慢**。
        // 而 provider 层的外层预算（`provider.rs` 的 `compute_max_retries` 两个调用点）
        // **早已改用** `kiro_selectable_count()` ⇒ 内层漏改就是**两层口径分叉**。
        let max_attempts = (total * MAX_FAILURES_PER_CREDENTIAL as usize)
            .max(self.kiro_selectable_count())
            .max(1);
        let mut attempt_count = 0;
        // 懒恢复标志：跨月配额恢复（recover_expired_quota_disables）要扫全表，
        // 本请求内只尝试一次——首次「无候选」时执行，后续无候选直接跳过
        // （启动期与下一个请求自会再试，重复扫描纯属浪费）。
        let mut recovery_attempted = false;
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
                // B8：号池无候选告警（30s 窗口连续 3 次才发）。
                self.note_pool_exhausted("all_unavailable");
                anyhow::bail!(
                    "所有凭据均无法获取有效 Token（可用: {}/{}）retry_after_secs={}",
                    self.available_count(),
                    total,
                    POOL_EXHAUSTED_RETRY_AFTER_SECS
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
                    let mut best = self.select_next_credential(model, user_id, excluded);

                    // 没有可用凭据：如果是"自动禁用导致全灭"，做一次类似重启的自愈
                    if best.is_none() {
                        // 先做跨月配额恢复（放在自愈检查之前）：
                        // `QuotaExceeded` 刻意不在 is_self_healable_reason 白名单里（当月复活
                        // 只会白撞 402），但跨自然月后 MONTHLY_REQUEST_COUNT 已重置，应自动
                        // 复活——否则这些号永远躺死等人工。恢复成功后重选一次，避免刚复活的
                        // 号立刻被下面的 transient_wait_outcome 判成"全在等待"。
                        if !recovery_attempted {
                            recovery_attempted = true;
                            if self.recover_expired_quota_disables(None) > 0 {
                                best = self.select_next_credential(model, user_id, excluded);
                            }
                        }
                        if best.is_none() {
                        // 退避参数来自 config（ArcSwap 热更）。**必须在进 entries 锁之前**
                        // 读取并存局部变量，绝不在锁内 load —— 保持「锁外读配置」纪律，
                        // 与 reload 路径的锁序互不纠缠（load 只 +1 引用计数，无锁）。
                        // reload 换入新值后**下一个自愈周期**即按新参数计算退避。
                        let cfg = self.config.load();
                        let heal_base = StdDuration::from_secs(cfg.self_heal_base_backoff_secs);
                        let heal_max = StdDuration::from_secs(cfg.self_heal_max_backoff_secs);
                        // shift 上限来自 config（运行期值）：钳到 31 防配置 ≥32 时
                        // `1u32 << shift` 移位溢出 panic（原常量 4 是编译期定死的）。
                        let heal_max_shift = cfg.self_heal_max_shift.min(31);
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
                        // ⭐ 退避闸门：自愈**必须**限频，否则会加深上游封禁。
                        //
                        // 此前无任何限频，实测 41 分钟触发 36 次（约每 68 秒一次）。
                        // 而 403 `temporarily is suspended` 是上游刚下的惩罚，每次复活都立刻
                        // 再打一轮 → 持续撞同一面墙 → 窗口被拉长。用户直接反馈过这个现象
                        // （「已经 403 封号了，不知道为什么一直被自动开启」）。
                        //
                        // 退避按**连续自愈次数**指数增长；任一号成功即清零（见 report_success），
                        // 故真恢复了会立刻回到灵敏状态，不会因退避而错过。
                        let heal_allowed = {
                            let streak = self.self_heal_streak.load(Ordering::Relaxed);
                            let shift = streak.min(heal_max_shift);
                            let wait = heal_base
                                .saturating_mul(1u32 << shift)
                                .min(heal_max);
                            let last = *self.last_self_heal_at.lock();
                            match last {
                                // 首次自愈不等待：可能真的只是一次抖动。
                                None => true,
                                Some(t) => t.elapsed() >= wait,
                            }
                        };
                        if entries
                            .iter()
                            .any(|e| e.disabled && is_self_healable_reason(e.disabled_reason))
                            && !heal_allowed
                        {
                            let streak = self.self_heal_streak.load(Ordering::Relaxed);
                            tracing::debug!(
                                "全池自愈处于退避期（连续第 {} 次未被成功打断），本轮跳过复活以免加深上游封禁",
                                streak
                            );
                        } else if entries
                            .iter()
                            .any(|e| e.disabled && is_self_healable_reason(e.disabled_reason))
                        {
                            *self.last_self_heal_at.lock() = Some(Instant::now());
                            let streak = self.self_heal_streak.fetch_add(1, Ordering::Relaxed) + 1;
                            tracing::warn!(
                                "所有凭据均已被自动禁用，执行自愈：重置失败计数并重新启用（连续第 {} 次，下次需退避）",
                                streak
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

                            // 记下这批复活的号：只有它们之中有号成功，才算「这次自愈起了作用」
                            // 并把 streak 打断。见 `self_heal_revived` 的完整理由 ——
                            // 原先任意号成功即清零，而池子 99.7% 成功 ⇒ 指数退避从未生效。
                            //
                            // 每次自愈**覆盖**而非累积：streak 问的是「最近这次复活有没有用」，
                            // 累积会让上一轮复活的号在本轮成功时也清零，等于把判据放回原样。
                            {
                                let mut revived = self.self_heal_revived.lock();
                                revived.clear();
                                revived.extend(healed_ids.iter().copied());
                            }

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

                            best = self.select_next_credential(model, user_id, excluded);
                        }
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
                                // ⭐ 兜底放行优先于任何 bail：与其回一个网关自造的 429
                                // （上游压根没被请求），不如放出「冷却最快到期」的号去打真实上游。
                                // 见 select_ignoring_cooldown 的实测代价（22 点 15.3% 自造失败）。
                                if let Some((id, _creds, inflight)) =
                                    self.select_ignoring_cooldown(model, excluded)
                                {
                                    match self.try_ensure_token(id, inflight).await {
                                        Ok(ctx) => return Ok(ctx),
                                        Err(e) => {
                                            // 取 token 都失败 ⇒ 兜底也救不了，落回原路径。
                                            tracing::warn!(
                                                "兜底放行凭据 #{} 但取 token 失败，落回冷却等待/透传: {}",
                                                id,
                                                e
                                            );
                                        }
                                    }
                                }
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
                                if wait_started.elapsed()
                                    < StdDuration::from_secs(MAX_TRANSIENT_WAIT_SECS)
                                {
                                    let w = wait
                                        .max(StdDuration::from_millis(250))
                                        .min(StdDuration::from_secs(2));
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
                                if wait_started.elapsed()
                                    < StdDuration::from_secs(MAX_TRANSIENT_WAIT_SECS)
                                {
                                    // RPM 恢复窗口可长达 ~60s,等待封顶到剩余总预算内,不空转也不超墙钟。
                                    let remaining = StdDuration::from_secs(MAX_TRANSIENT_WAIT_SECS)
                                        .saturating_sub(wait_started.elapsed());
                                    let w = wait
                                        .max(StdDuration::from_millis(250))
                                        .min(remaining.max(StdDuration::from_millis(250)));
                                    tracing::warn!(
                                        "整池 RPM 饱和(背压),等待恢复窗口 {:?} 后重试",
                                        w
                                    );
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
                            // 并发上限硬门（迁移差距 P1）：在飞请求连续释放（流结束即 -1，
                            // 通常毫秒级），短固定等待后重选即大概率命中。与 RpmRecovery 同族
                            // （"繁忙"类别，绝不报"已禁用"）；等待封顶到剩余总预算内。
                            WaitOutcome::Wait(wait, WaitReason::ConcurrencyFull) => {
                                if wait_started.elapsed()
                                    < StdDuration::from_secs(MAX_TRANSIENT_WAIT_SECS)
                                {
                                    let remaining = StdDuration::from_secs(MAX_TRANSIENT_WAIT_SECS)
                                        .saturating_sub(wait_started.elapsed());
                                    let w = wait
                                        .max(StdDuration::from_millis(250))
                                        .min(remaining.max(StdDuration::from_millis(250)));
                                    tracing::warn!(
                                        "整池在飞请求均达并发上限，等待释放 {:?} 后重试",
                                        w
                                    );
                                    sleep(w).await;
                                    continue;
                                }
                                let retry_after = wait.as_secs().max(1);
                                anyhow::bail!(
                                    "整池在飞请求均达并发上限，等待释放超时（{}/{}）retry_after_secs={}",
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
                                // 两个量必须在**同一个**临界区里算完再放锁：available_count()
                                // 会再锁 entries 致死锁，而分两次锁会读到不一致的池快照。
                                //
                                // 🔴 **`available` 必须排除 custom_api**（2026-08-10 修，致命）。
                                //
                                // 这里的 `available` 是用来给下面那个二分（① available==0 = 真耗尽
                                // → 429；② available>0 = 被**模型级硬门**挡掉 → 按有无 TTL 分 429/404）
                                // 做分诊的，所以它的语义必须是「**Kiro 路径**可选号数」。
                                // 而 `is_entry_selectable_inner`(:3716) 对 custom_api **一律 return
                                // false**（注释原文「Kiro 路径永不碰 custom_api」），
                                // `transient_wait_outcome`(:3783) 同样 continue ⇒ 代挂号从来就不是
                                // 「Kiro 可用号」，算进来就是喂给二分一个错的口径。
                                //
                                // 后果链（线上现状 = 号池全是 custom_api 代挂号、无 ksk_ Kiro 号）：
                                //   代挂号未 disabled ⇒ available=N>0 ⇒ **跳过** ① 那两条正确的
                                //   429 出口 ⇒ 落 ②；而透传路径从不调 `report_model_invalid`
                                //   （它只被 provider 的 Kiro 路径调用）⇒ 代挂号永远不在 blocklist
                                //   ⇒ `model_block_min_remaining` 返 None ⇒ 必然走「无 TTL = 永久」
                                //   分支报 `model_unsupported_by_pool=1` ⇒ handlers.rs:1482 映射成
                                //   **404 且刻意无 Retry-After** ⇒ 客户端（Claude Code/Cursor）
                                //   **当场断会话**。
                                // 而那条 404 的文案「N/M 个号均因订阅档位或成本白名单不含该模型而被
                                // 过滤」对代挂号是**错误归因**——真实原因是「池里一个 Kiro 号都没有」。
                                //
                                // ⚠️ 不能改成调 `kiro_selectable_count()`：它自己会再锁 entries，
                                // 在本临界区内调用即死锁（见本注释开头那条）。故**就地内联**它的
                                // 谓词，与 `:3078` 逐字一致——两处若再分叉，这个 bug 会以另一种形式回来。
                                let (available, any_healable) = {
                                    let entries = self.entries.lock();
                                    (
                                        entries
                                            .iter()
                                            .filter(|e| {
                                                !e.disabled
                                                    && !e.credentials.is_custom_api_credential()
                                            })
                                            .count(),
                                        // 🔴 `any_healable` = 「这个池等一会儿有希望好吗」。
                                        // 两种情形都算有希望（2026-08-10 补第二种，实测缺陷）：
                                        entries.iter().any(|e| {
                                            // ① 有被禁用但**可自愈**的号（TooManyFailures /
                                            //    SuspiciousActivityAuto / TooManyRefreshFailures）
                                            //    —— 全池自愈会把它们放回来。
                                            (e.disabled
                                                && is_self_healable_reason(e.disabled_reason))
                                            // ② 有**未禁用的代挂号**（2026-08-10 补）。
                                            //
                                            // 为什么必须算：走到这里说明 Kiro 侧选不到号，而池里
                                            // 全是代挂号时 available 恒为 0 ⇒ 旧判据里
                                            // any_healable=false（没有"被禁用且可自愈"的号）
                                            // ⇒ 打 `pool_permanently_exhausted=1` ⇒
                                            // `absorb_class_of` 显式**拒绝吸收**它（那条注释说
                                            // "池里全是需人工处置的终态，等多久都不会变"）
                                            // ⇒ 429 直接透给客户端 ⇒ 会话断。
                                            //
                                            // 但那个前提在这里不成立：代挂号**没被禁用**，
                                            // 走到 Kiro 路径只是因为它们的上游此刻全故障了
                                            // （实测 2026-08-10：k2cc 30s 无响应 + denzao 502
                                            // `Upstream service temporarily unavailable`，
                                            // 30 分钟内 128 次这条 429）。上游故障是**瞬态**，
                                            // 等一会儿真的会好 ⇒ 该让吸收层去吸收，而不是当永久态。
                                            //
                                            // ⚠️ 只看「未禁用」不看「是否冷却中」：冷却本身就是
                                            // 限时的（透传失败给 5s/180s），冷却期满自然恢复，
                                            // 同样属于"等一会儿会好"。
                                                || (!e.disabled
                                                    && e.credentials.is_custom_api_credential())
                                        }),
                                    )
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
                                    //
                                    // ⭐ 但对**内置吸收层**必须再分一次：池里一个可自愈的号都没有时
                                    // （全是 QuotaExhausted / RefreshTokenInvalid / AccountSuspended
                                    // 这类需人工处置的终态），等多久都不会变。吸收层若把它当
                                    // PoolCooldown 就会拿满 45s 预算对一个**永不恢复**的池空转，
                                    // 客户端从 <2s 拿到 429 变成 45s 才拿到，且这 45s 内它占着连接。
                                    // 标记语义与 `inbound_admission_timeout=1` 同款：**对客户端仍是
                                    // 429 + Retry-After**（人工补号后确实会好，客户端该退避），
                                    // 只是对「单请求内重试」这件事显式说不。
                                    // 🔴 文案必须区分两种「Kiro 侧 0 可选」（2026-08-10 修）：
                                    // 旧文案一律写「所有凭据均已禁用（0/N）」，而纯代挂池时
                                    // N 个号**一个都没被禁用** ⇒ 线上实测出现自相矛盾的
                                    // 「所有凭据均已禁用（0/2）」，把排障直接带偏
                                    // （去查为什么号被禁用，而真因是两个代挂上游都挂了）。
                                    let has_enabled_custom_api = {
                                        let entries = self.entries.lock();
                                        entries.iter().any(|e| {
                                            !e.disabled && e.credentials.is_custom_api_credential()
                                        })
                                    };
                                    if any_healable {
                                        if has_enabled_custom_api {
                                            // 🔴 **连续全池不可用达阈值 ⇒ 升级为终态**
                                            // （2026-08-10 对抗评审抓出的缺陷修复）。
                                            //
                                            // 不这么做的后果：纯代挂池下 `available` 恒 0 而
                                            // `any_healable` 恒 true（只要有未禁用代挂号），
                                            // 而「透传失败绝不 auto-disable 号」是有实测依据的
                                            // 刻意设计（健康号 #216 曾被误禁 119 次）⇒ 上游
                                            // **真·永久坏**（余额耗尽返 402/403）时这条 429
                                            // 永远带 `retry_after_secs=` ⇒ 客户端**无限重试**、
                                            // 永远拿不到终态。
                                            //
                                            // 阈值取 20：按 `POOL_EXHAUSTED_RETRY_AFTER_SECS`=10
                                            // 的客户端退避节奏，约 200s 持续全败才判永久
                                            // —— 足以跨过中转站的常见瞬时抖动（实测代挂上游
                                            // 502 通常几秒自愈），又不会让真故障拖到分钟级以上。
                                            const POOL_PERMANENT_THRESHOLD: u32 = 20;
                                            let n = self
                                                .consecutive_pool_unavailable
                                                .fetch_add(1, Ordering::Relaxed)
                                                + 1;
                                            if n >= POOL_PERMANENT_THRESHOLD {
                                                // B8：代挂全挂持续故障（终态）。
                                                self.note_pool_exhausted("all_custom_api_down");
                                                anyhow::bail!(
                                                    "Kiro 路径无可用凭据（池中 {} 个号均为 custom_api 代挂号，\
                                                     其上游已连续 {} 轮全部失败，判定为持续故障；\
                                                     代挂号本身未被禁用——请检查中转站余额/可用性）\
                                                     pool_permanently_exhausted=1 retry_after_secs={}",
                                                    total,
                                                    n,
                                                    POOL_EXHAUSTED_RETRY_AFTER_SECS
                                                );
                                            }
                                            // B8：代挂全挂（瞬态，仍值得告警）。
                                            self.note_pool_exhausted("all_custom_api_down");
                                            anyhow::bail!(
                                                "Kiro 路径无可用凭据（池中 {} 个号均为 custom_api 代挂号，\
                                                 其上游此刻全部失败；代挂号本身未被禁用）\
                                                 consecutive_pool_unavailable={} retry_after_secs={}",
                                                total,
                                                n,
                                                POOL_EXHAUSTED_RETRY_AFTER_SECS
                                            );
                                        }
                                        // B8：池中无自愈号（全禁用，瞬态等待）。
                                        self.note_pool_exhausted("all_disabled");
                                        anyhow::bail!(
                                            "所有凭据均已禁用（0/{}）retry_after_secs={}",
                                            total,
                                            POOL_EXHAUSTED_RETRY_AFTER_SECS
                                        );
                                    }
                                    // B8：池中无自愈号（全禁用，永久态）。
                                    self.note_pool_exhausted("all_disabled");
                                    anyhow::bail!(
                                        "所有凭据均已禁用（0/{}）pool_permanently_exhausted=1 retry_after_secs={}",
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
                    let has_available = if e.downcast_ref::<RefreshTokenInvalidError>().is_some() {
                        tracing::warn!("凭据 #{} refreshToken 永久失效: {}", id, e);
                        self.report_refresh_token_invalid(id)
                    } else {
                        tracing::warn!("凭据 #{} Token 刷新失败: {}", id, e);
                        // 按错误类型分流：上游 5xx / 网络抖动只设可自愈冷却，不计永久失败。
                        // 旧行为无条件计数 → 一次 token 端点抖动 3 次即烧号（见 classified 的说明）。
                        self.report_refresh_failure_classified(id, &e)
                    };
                    attempt_count += 1;
                    if !has_available {
                        // 与上面 NoCandidate 那处同理：不带标记就落 502 无 Retry-After。
                        // 这条路径是"刷新失败把最后一个号也禁用了"。
                        //
                        // ⭐ 判据与 NoCandidate 那处共用 `is_self_healable_reason`。
                        //
                        // ⚠️ 走到这里的**两种**禁用原因当前都不可自愈：
                        // - `report_refresh_failure` → `TooManyRefreshFailures`
                        // - `report_refresh_token_invalid` → `InvalidRefreshToken`
                        // 而 `is_self_healable_reason` 只认 `TooManyFailures` /
                        // `SuspiciousActivityAuto`。所以本站点实际上**恒**走标记分支。
                        // 这不是 bug（两者确实都没有自动复活路径：全池自愈只复活可自愈
                        // 原因，另两处 `disabled = false` 都是面板手动操作），但意味着
                        // 一次 30s 的 token 端点抖动也会被报成「池永久耗尽」，需要人工
                        // 介入才恢复 —— 那是 `is_self_healable_reason` 覆盖面的问题，
                        // 不是本标记的问题，改它要单独一批（会改变自愈行为本身）。
                        let (any_healable, has_enabled_custom_api) = {
                            let entries = self.entries.lock();
                            (
                                // 与 NoCandidate 那处**同款两情形判据**（2026-08-10 补第二种）：
                                // ① 有被禁用但可自愈的号；② 有未禁用的代挂号（其上游故障是瞬态）。
                                // 线上实测漏网点：本站点是第二处产出 `pool_permanently_exhausted=1`
                                // 的地方，只修 NoCandidate 那处时这里仍会打出旧文案
                                // （实测 10 分钟内 7 次 `retries=0`，即吸收层拒收后直接透给客户端）。
                                entries.iter().any(|e| {
                                    (e.disabled && is_self_healable_reason(e.disabled_reason))
                                        || (!e.disabled
                                            && e.credentials.is_custom_api_credential())
                                }),
                                entries.iter().any(|e| {
                                    !e.disabled && e.credentials.is_custom_api_credential()
                                }),
                            )
                        };
                        if !any_healable {
                            anyhow::bail!(
                                "所有凭据均已禁用（0/{}）pool_permanently_exhausted=1 retry_after_secs={}",
                                total,
                                POOL_EXHAUSTED_RETRY_AFTER_SECS
                            );
                        }
                        // 纯代挂池：文案要说清"号没坏，是上游挂了"，否则排障方向被带偏。
                        if has_enabled_custom_api {
                            anyhow::bail!(
                                "Kiro 路径无可用凭据（池中 {} 个号均为 custom_api 代挂号，\
                                 其上游此刻全部失败；代挂号本身未被禁用）retry_after_secs={}",
                                total,
                                POOL_EXHAUSTED_RETRY_AFTER_SECS
                            );
                        }
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

    /// 无候选出口埋点（B8）：记录一次「号池无候选」，窗口内连续
    /// [`POOL_EXHAUST_ALERT_THRESHOLD`] 次才 bump "pool_exhausted"。
    ///
    /// `reason` 分类根因（全部禁用 / 代挂全挂 / 全不可用），随 webhook payload
    /// 投递（见 alerting::bump_with_reason）。只在 acquire_context_excluding 的
    /// 终态 bail 前调用，不占选号热路径（无候选本身已是失败路径）。
    fn note_pool_exhausted(&self, reason: &'static str) {
        let alert_now = {
            let mut gate = self.pool_exhaust_gate.lock();
            gate.record(Instant::now())
        };
        if alert_now {
            crate::common::alerting::bump_with_reason("pool_exhausted", Some(reason));
            self.pool_exhaust_gate.lock().reset();
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

    /// 从磁盘加载回收站（trash.json）
    ///
    /// 仅多凭据格式才有持久化文件；单凭据格式下回收站为纯内存态。
    /// 文件不存在或解析失败时静默回退为空。
    fn load_trash(&self) {
        // 2026-08-13：不再按格式跳过（旧版 Single 格式下回收站纯内存、重启即丢——
        // 与凭据持久化同源修复，统一为总是读写文件）。
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

    /// **无条件**落盘统计（绕过 debounce）。停机路径调用。
    ///
    /// # 为什么停机必须强制落一次（2026-08-04 线上实测的误禁链）
    ///
    /// `save_stats_debounced` 的 debounce 窗口是 [`STATS_SAVE_DEBOUNCE`]，所以最近一个
    /// 窗口内的 `success_count` 增量只在内存里。而线上今天 **41 次 SIGTERM 里有 39 次
    /// 走到 SIGKILL**（`TimeoutStopSec=10` < 无上限的 `serve().await`）⇒ 那些增量全丢。
    ///
    /// 丢掉之后的后果不是"统计难看"，而是**烧号**：
    /// `has_ever_succeeded()`（`:2276`）读的就是从 stats 恢复的 `success_count`，
    /// 它是 provider 里「bearer-invalid 403 判瞬态还是判真 region 错配」的唯一判据。
    /// 新号的成功记录没落盘 ⇒ 重启后它变成「从未成功过」⇒ 瞬态 403 被当成真错配 ⇒
    /// 3 次即 `TooManyFailures` 禁用。**实测 20:20:30 启动、20:20:32 就把 #483 打死。**
    ///
    /// 即 debounce 是个正确的写放大优化，但它与"进程随时可能被硬杀"叠起来会烧号。
    pub fn flush_stats_now(&self) {
        if self.stats_dirty.load(Ordering::Relaxed) {
            self.save_stats();
        }
        // 2026-08-13：停机时冷却状态一并强制落盘（即使 stats 不脏，冷却也可能脏——
        // debounce 30s 窗口内的冷却变更若被硬杀丢掉，重启后风控退避档位回基线，
        // 正是持久化要消除的烧号放大器）。
        self.cooldown.flush_now();
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
        // ⭐ 清零全池自愈的连续计数 → 退避回到灵敏状态。但**只在这个号是被最近一次自愈
        // 复活的**时才清，不是任意号成功都清。
        //
        // # 两个方向都是真缺陷，这条判据在它们之间穿过
        //
        // · **从不清零** → `self_heal_streak` 只增不减，退避爬到上限（15 分钟）并**永远停在
        //   那里**，即使号池早已恢复。这是本仓反复出现的"单向棘轮"形态
        //   （见 health.rs 的 decay_penalties / G1 元测试那段历史）。
        // · **任意成功即清零**（本行原本的行为）→ 线上池子成功率 99.7%，成功持续不断 ⇒
        //   streak 每次自增后立刻被清回 0 ⇒ `wait` 恒为 `BASE × 2^0` = **60s** ⇒
        //   死号每 60 秒被复活一次，而退避本该涨到 120/240/480/900s。
        //   实测日志坐实：`执行自愈` 间隔全部聚集在恰好 60.0s，`连续第 N 次` 70 次落 N=1、
        //   仅 1 次到 N=5。原判据假设「全池被禁用」与「有成功」互斥，实际两者持续交织
        //   （部分号被禁 → 自愈复活 → 少量成功 → 再被禁）。
        //
        // # 为什么"被复活的号成功"才是正确判据
        //
        // streak 的语义是「连续自愈**未被成功打断**」。能打断它的应当是「这次复活真起了
        // 作用」的证据 —— 而一个从未被禁用的健康号成功，对此不构成任何证据。
        // 同时它仍能解棘轮：号池真恢复时，被复活的号自然会成功，streak 照样归零。
        //
        // 命中后**移出集合**：同一批复活只需打断一次；留着会让后续每次成功都重复清零，
        // 等价于退回原判据。
        //
        // 放在函数最前、不在 entries 锁内：只碰 `self_heal_revived`（独立锁）与 relaxed 原子。
        {
            let was_revived = {
                let mut revived = self.self_heal_revived.lock();
                revived.remove(&id)
            };
            if was_revived {
                self.self_heal_streak.store(0, Ordering::Relaxed);
            }
        }
        let fam = self.family_key_of(id);
        // 该号本进程内是否**首次**成功（决定要不要绕过 debounce 立刻落盘，见函数末尾）。
        let mut first_success = false;
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
                // 该号在本进程内**第一次**成功 → 稍后强制落盘（见下方 first_success）。
                first_success = entry.success_count == 1;
                tracing::debug!(
                    "凭据 #{} API 调用成功（累计 {} 次）",
                    id,
                    entry.success_count
                );
            }
            // ⭐ 族级清零（承重，与 `report_suspicious_activity` 配对）
            //
            // 风控计数是按**族**累加的（同 clone_group 的分身共享一个上游账号，
            // 见 `report_suspicious_activity` 的实测依据）。若清零只清本号，则同族其它
            // 分身仍停在高位 ⇒ 下一次 403 会从那个高位 +1 直接把整族推过阈值，
            // 表现为「刚成功过的账号立刻被判死号并整族禁用」。累加与清零必须同口径。
            //
            // 只有共享族键（`clone:` / `m365:` / `aws:`）才需要扫全池；`cred:{id}` 只
            // 匹配自己，上面那行已经清完，故用前缀判断跳过，保持单号场景零额外开销。
            if !fam.starts_with("cred:") {
                for entry in entries.iter_mut() {
                    // 读预计算缓存而非逐条现算 `family_key`：否则每次成功都要对全池做
                    // 一次 String 分配 + issuer_url 解析扫描（共享族下每成功一次的
                    // O(n) 分配扫描，见 family_key 缓存字段的文档）。
                    if entry.family_key_cached() == fam {
                        entry.consecutive_suspicious = 0;
                    }
                }
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
        // ⭐ 首次成功**绕过 debounce 立刻落盘**（每号每进程仅一次，写放大可忽略）。
        //
        // 理由：`success_count > 0` 不只是统计，它是 `has_ever_succeeded()` 的判据，
        // 而那是 provider 区分「bearer-invalid 403 = 瞬态抖动」与「真 region 错配」的
        // 唯一依据。停机路径已加 `flush_stats_now()`，但那只覆盖收得到 SIGTERM 的情形；
        // SIGKILL 直杀 / panic / OOM 都绕过它 —— 而线上今天 41 次 SIGTERM 里 39 次
        // 最终走到 SIGKILL。写在"第一次成功"这个点上，则任何死法都不会让一个**已经
        // 证明能用**的号在重启后被当成从未成功过、进而被三次瞬态 403 打死。
        //
        // 后续成功仍走 debounce（那些只影响统计精度，丢了不烧号）。
        if first_success {
            self.save_stats();
        } else {
            self.save_stats_debounced();
        }
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
    pub fn health_snapshots(
        &self,
    ) -> std::collections::HashMap<u64, crate::kiro::health::HealthSnapshot> {
        // 先在 entries 锁内只收集 (id, family_key) 轻量对,立即释放锁;再逐个查 health(独立 Mutex)。
        // 避免持 entries 锁跨多次 health.snapshot() 调用形成锁嵌套(与既有"health 锁外调用"约定一致)。
        let pairs: Vec<(u64, String)> = {
            let entries = self.entries.lock();
            entries
                .iter()
                .map(|e| (e.id, e.credentials.family_key(e.id)))
                .collect()
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
        if let Some(has_enabled) = self.preserve_custom_api_state(id) {
            return has_enabled;
        }
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
            // 上界提到模块级常量，因为兜底选号的深浅分档也要用同一个数（见 FallbackCooldownTier）。
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
    ///
    /// ## 为什么计数单位是「族」而不是「号」（2026-08-07 线上实测）
    ///
    /// 上游 403 的 body 自带账户标识：
    /// `AccessDeniedException: Your User ID (NNN) temporarily is suspended`。
    /// 实测该 User ID 与 cred id 是 **N:1**（UID 079998937591 → cred 1294..1299）
    /// ⇒ 上游按**账号**记账，不按设备指纹。而多开分身（同 `clone_group`）定义上就是
    /// 同一把 key ⇒ 同一个账号。若按号计数，一次账户级 suspend 要**每份各自数满**
    /// 阈值才退出调度 ⇒ 白挨 6×N 次上游 403（线上 N=17 ⇒ 102 次/轮），且全池自愈
    /// 会把整族复活再来一轮（实测当天 `判定为死号并自动禁用` 231 次 / `执行自愈` 14 次），
    /// 持续撞同一面墙。按族计数后只需 6 次 403 即整族退出调度，上游少挨 6×(N−1) 次。
    ///
    /// ⚠️ 收族**只对同 `clone_group` 的 api_key 分身生效**；无 group 的号仍
    /// `cred:{id}` 各自独立（否则整池连坐）。边界由
    /// `test_suspicious_counting_stays_per_credential_without_clone_group` 等反向测试钉死。
    pub fn report_suspicious_activity(&self, id: u64) {
        if self.preserve_custom_api_state(id).is_some() {
            return;
        }
        // ── 1. 计数 + 自动禁用（恒执行，不受任何冷却/限速开关影响）
        //
        // 单位是族：同 clone_group 的分身共享一个上游账号（见 family_key 函数文档的
        // 实测依据），故整族共用一个计数、同时达阈值、同时禁用。
        let mut hit_count = 0u32;
        let mut disabled_ids: Vec<u64> = Vec::new();
        let mut family_size = 0usize;
        {
            let mut entries = self.entries.lock();
            // 目标号的族键。号已被删除 / 已禁用时不计数（与旧行为一致：只跳过本节，
            // 后面两节照常执行）。family_key 在锁内就地算，避免 family_key_of 二次取锁。
            let fam = entries
                .iter()
                .find(|e| e.id == id)
                .filter(|e| !e.disabled)
                .map(|e| e.credentials.family_key(e.id));
            if let Some(fam) = fam {
                // 族内**最高**计数 +1，而不是各自 +1：
                // 中途被自愈复活（clear_transient_counters 清零）或新导入的分身会从 0 起，
                // 若各自 +1 则整族计数长期参差 → 阈值被推迟到最慢那份数满，
                // 等价于退回按号计数。取 max 让整族步调一致。
                let next = entries
                    .iter()
                    .filter(|e| !e.disabled && e.credentials.family_key(e.id) == fam)
                    .map(|e| e.consecutive_suspicious)
                    .max()
                    .unwrap_or(0)
                    .saturating_add(1);
                hit_count = next;
                let should_disable = self.auto_disable_suspicious.load(Ordering::Relaxed)
                    && next >= MAX_CONSECUTIVE_SUSPICIOUS_BEFORE_DISABLE;
                // 时间戳算一次，整族共用（同一次风控判定应有同一个 disabled_at）。
                let now_rfc3339 = Utc::now().to_rfc3339();
                for entry in entries.iter_mut() {
                    if entry.disabled || entry.credentials.family_key(entry.id) != fam {
                        continue;
                    }
                    entry.consecutive_suspicious = next;
                    family_size += 1;
                    if should_disable {
                        entry.disabled = true;
                        entry.disabled_reason = Some(DisabledReason::SuspiciousActivityAuto);
                        entry.disabled_at = Some(now_rfc3339.clone());
                        disabled_ids.push(entry.id);
            crate::common::recovery_metrics::bump_dead_token_disabled();
            crate::common::alerting::bump("credential_disabled");
            // B8：配额耗尽独立告警 key（首次禁用即触发；重启后同号再撞 402 会
            // 再 bump，由 alerting 冷却去重——配额耗尽常是「整池轮转」形态，
            // 与 credential_disabled 的逐号语义分开统计）。
            crate::common::alerting::bump("quota_exhausted");
                    }
                }
            }
        }
        if !disabled_ids.is_empty() {
            tracing::error!(
                "凭据族（触发号 #{}，同族 {} 份）连续 {} 次账户级风控且期间零成功，\
                 判定为死号并自动禁用（SuspiciousActivityAuto）：{:?}；\
                 整族移出调度以免每个请求都在同一个上游账号上白撞",
                id,
                family_size,
                hit_count,
                disabled_ids
            );
            // 清亲和：否则绑定这些号的会话会反复重选到已禁用凭据。
            for cid in &disabled_ids {
                self.affinity.remove_by_credential(*cid);
            }
            // 必须落盘：save_stats_debounced 写的 StatsEntry 不含 disabled/disabled_reason，
            // 不落盘则重启后死号以 enabled 回池、重走一遍禁用流程（白耗上游请求）。
            // persist_credentials 从内存全量重写，故一次调用即覆盖整族。
            self.persist_disabled_state(id);
        } else if hit_count > 0 {
            tracing::debug!(
                "凭据 #{} 账户级风控，族内连续第 {}/{} 次（同族 {} 份共享该计数，一次成功即清零）",
                id,
                hit_count,
                MAX_CONSECUTIVE_SUSPICIOUS_BEFORE_DISABLE,
                family_size
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
            self.health
                .report_family_suspicious(&self.family_key_of(id), dur);
        }

        // ── 3. 限速器退避（受 rate_limit_enabled 门控）
        if self.rate_limit_enabled.load(Ordering::Relaxed) {
            // 可疑活动风控是瞬态（账户级软风控，会自愈）：限速器只需秒级退避即可，
            // 真正的分钟级退避由上面的 cooldown（SuspiciousActivity）承担；这里绝不长冻。
            self.rate_limiter.record_failure(id, FailureKind::Transient);
        }
    }

    /// 报告凭据认证失败，设置较长冷却（配合 force-refresh 失败后调用）
    ///
    /// ⚠️ 这条落的是 [`CooldownReason::AuthenticationFailed`]：`is_auto_recoverable=false`
    /// ⇒ `calculate_cooldown_duration` 走 `long_cooldown_secs`（**86400s**）且不禁用，
    /// 即面板上一个「冷却中」的僵尸号。只有**认证态真的坏了**（refreshToken 废了、
    /// 该号从未成功过）才该用它。
    ///
    /// 401/403 里那些**瞬态**的（token 对该端点已被证明有效 / region 未开通但后台重探
    /// 已启动）请用 [`Self::report_auth_transient_cooldown`]，别用本函数。
    pub fn report_auth_cooldown(&self, id: u64) {
        if self.cooldown_enabled.load(Ordering::Relaxed) {
            let dur = self
                .cooldown
                .set_cooldown(id, CooldownReason::AuthenticationFailed);
            tracing::warn!("凭据 #{} 认证失败，冷却 {:?}", id, dur);
        }
    }

    /// 报告凭据**瞬态**认证失败：短冷却（20s 基线）+ 可自愈，**不**计失败、**不**禁用。
    ///
    /// # 与 [`Self::report_auth_cooldown`] 的分界（承重）
    ///
    /// 两者在 wire 上逐字节相同（同一句 bearer-invalid + 403），语义相反 ——
    /// 判据只能来自**网关侧已知的历史**，不能来自上游文案：
    ///
    /// - 走本函数：token 对该端点**已被证明有效**（[`Self::has_ever_succeeded`]），
    ///   或 region 未开通而后台重探已启动 ⇒ 几十秒后大概率就能用。
    /// - 走 `report_auth_cooldown`：**从未成功过** ⇒ 大概率 region 错配或真封号，
    ///   套短冷却等于拿一个注定失败（或已被风控）的号每 20s 猛打上游一次，加重风控。
    ///
    /// 代价不对称：把瞬态当永久 = 一次抖动冻掉一个健康号 24h（不可逆，池子少一个号 →
    /// 剩下的吃更多流量 → 更容易撞惩罚窗口）；把永久当瞬态 = 每 20s 多打一次注定失败的
    /// 往返（可逆）。所以分界宁可偏向瞬态，但**必须**有 `has_ever_succeeded` 这道门。
    ///
    /// 反复触发由 `calculate_cooldown_duration` 的 1.3^n 递增兜住（上限 90s），
    /// 故「判错方向」的最坏态也是自限的。
    pub fn report_auth_transient_cooldown(&self, id: u64) {
        if self.cooldown_enabled.load(Ordering::Relaxed) {
            let dur = self
                .cooldown
                .set_cooldown(id, CooldownReason::AuthTransient);
            tracing::warn!(
                "凭据 #{} 认证瞬态失败（非永久失效），短冷却 {:?} 后自动回池，不计失败、不禁用",
                id,
                dur
            );
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
            id,
            model
        );
        self.count_selectable_for_model(&model) > 0
    }

    /// 判断"凭据 #id + 模型 model"当前是否在模型级黑名单内（未过 TTL）。惰性清理过期项。
    ///
    /// 两池共用（#9 合并）：Kiro 主路径选号与 custom_api 透传路径
    /// （`is_model_blacklisted` 复用本实现）查同一张 `model_blocklist` 表。
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
            // 🔴 必须排除 custom_api（2026-08-10 修，与 `:4455` / `kiro_selectable_count` 同一类修复）：
            // 本函数唯一的消费者是 `report_model_invalid`，它的返回值供 provider 判断
            // 「本模型**在 Kiro 路径上**还有号可试吗」。而 custom_api 号被
            // `is_entry_selectable_inner` 一律拒绝、Kiro 路径永远选不到，算进来会让纯代挂池
            // （线上现状）恒返回 `true` ⇒ provider 以为「还有号」而继续 failover 空转，
            // 直到烧完预算才失败。`INVALID_MODEL_ID` 本身也只在 Kiro 上游出现，
            // 代挂号的等价错误走透传路径的 400/404 分流，与本函数无关。
            .filter(|e| !e.disabled && !e.credentials.is_custom_api_credential())
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
        if let Some(has_enabled) = self.preserve_custom_api_state(id) {
            return has_enabled;
        }
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
            // 跨月自动恢复的判据时刻（与 disabled_at 解耦，见字段文档）。
            entry.quota_exhausted_at = Some(Utc::now().to_rfc3339());
            entry.last_used_at = Some(Utc::now().to_rfc3339());
            // 设为阈值，便于在管理面板中直观看到该凭据已不可用
            entry.failure_count = MAX_FAILURES_PER_CREDENTIAL;
            crate::common::recovery_metrics::bump_dead_token_disabled();
            crate::common::alerting::bump("credential_disabled");

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

    /// 懒恢复因额度耗尽（`QuotaExceeded`）被禁用的凭据：**仅启动期与「全池无候选」**
    /// （`acquire_context` 循环内首次无号可选）两个触发点，不设后台定时任务。
    ///
    /// Kiro 的 `MONTHLY_REQUEST_COUNT` 按自然月重置，因此只要当前月份不同于
    /// 判定耗尽时的月份（`quota_exhausted_at`），就应重新放回可用池；若下个月
    /// 仍无额度，上游会再次返回 402 并重新禁用，代价仅一次请求。
    ///
    /// ## 月初缓冲（时区安全网）
    ///
    /// 恢复判定按 **UTC 自然月 + 12h 缓冲**：`now` 距当月 1 日不足 12h 时整批跳过
    /// （不进 recovered 列表）。理由：上游（AWS/Kiro `MONTHLY_REQUEST_COUNT`）的
    /// 重置时区未验证，若按偏西时区（UTC-8~UTC-12）重置，UTC 月初 0 点恢复会在
    /// 上游还没重置时白撞一次 402 → 被重新禁用并盖上**当月** `quota_exhausted_at`
    /// → 整个月永不恢复。12h 覆盖最坏时区差；上游非 UTC 时区下恢复至多延迟 12h
    /// （代价可控，不会整月失效）。
    ///
    /// ## 幂等
    ///
    /// 只处理 `disabled && reason == QuotaExceeded` 的号，恢复后 `disabled_reason`
    /// 清空，重复调用天然跳过（`Manual` / `AccountSuspended` 等一律不碰）。
    ///
    /// ## 探针抑制取舍（评审要求，已评估后放弃）
    ///
    /// 评审提出：恢复前若余额缓存存在、新鲜且 remaining<=0，说明配额**还没**重置，
    /// 应跳过恢复以免白撞一次 402 探针。但余额缓存在 `AdminService.balance_cache`
    /// （`src/admin/service.rs`，私有字段）里，持有方向是 AdminService → token_manager，
    /// 反向无引用，结构上够不着；token_manager 自己的 `balance_snapshots` 只有
    /// `remaining_at_cache`（无缓存时刻），无法判「新鲜」。为它加回调/反向引用属于
    /// 跨层结构改动，超出本修复范围。退化为保守方案：**不查缓存直接恢复**，
    /// 靠幂等 + 下次 402 再禁用兜住（配额未重置时至多多撞一次请求，与 k2cc 参考
    /// 实现同语义）。
    ///
    /// `now`：判定的参考时刻，`None` = `Utc::now()`（测试传固定时刻避开月初缓冲窗口）。
    ///
    /// 返回被恢复的凭据数量。
    fn recover_expired_quota_disables(&self, now: Option<DateTime<Utc>>) -> usize {
        let now = now.unwrap_or_else(Utc::now);
        // 月初缓冲：距当月 1 日 ≥ 12h 才允许恢复。上游（AWS/Kiro MONTHLY_REQUEST_COUNT）
        // 重置时区未验证——若按偏西时区（最坏 UTC-12）重置，UTC 月初 0 点恢复会早于上游
        // 重置 → 402 → 重禁用盖当月时间戳 → 当月永不恢复。12h 缓冲覆盖最坏时区差，
        // 代价是恢复最多延迟 12h。
        // 用 day0()*24+hour() 手算（不依赖 chrono TimeZone trait / Duration 方法，版本兼容）。
        let hour_of_month = now.day0() as i64 * 24 + now.hour() as i64;
        if hour_of_month < 12 {
            return 0;
        }
        let now_year_month = (now.year(), now.month());
        let recovered: Vec<u64> = {
            let mut entries = self.entries.lock();
            let mut ids = Vec::new();
            for e in entries.iter_mut() {
                if !e.disabled || e.disabled_reason != Some(DisabledReason::QuotaExceeded) {
                    continue;
                }
                // 缺失时间戳（旧版本数据，未持久化 quota_exhausted_at）视为可恢复，
                // 避免旧数据被永久钉死（评审要求）。
                let same_month = e.quota_exhausted_at.as_deref().and_then(|t| {
                    DateTime::parse_from_rfc3339(t)
                        .ok()
                        .map(|t| (t.year(), t.month()) == now_year_month)
                });
                if same_month.unwrap_or(false) {
                    continue;
                }
                e.disabled = false;
                e.disabled_reason = None;
                e.quota_exhausted_at = None;
                // 禁用时刻一并清空（对齐 set_disabled 启用收口）：恢复即自动启用，
                // 残留的旧禁用时刻会随 persist 落盘，误导"这号坏了多久"的判断。
                e.disabled_at = None;
                // 走单一收口：failure_count 连同 refresh_failure_count /
                // consecutive_suspicious 一并清零（对齐 set_disabled 启用时的清法）。
                e.clear_transient_counters();
                ids.push(e.id);
            }
            ids
        };

        if !recovered.is_empty() {
            tracing::info!(
                "已跨自然月，自动恢复 {} 个额度耗尽的凭据: {:?}",
                recovered.len(),
                recovered
            );
            // 清旁挂结构（各自独立锁，必须在 entries 锁外）：残留冷却/退避会让刚复活的
            // 号立刻又被选号硬门跳过，恢复等于没做（对齐全池自愈复活路径 :4746-4749 的做法）。
            for id in &recovered {
                self.cooldown.clear_cooldown(*id);
                self.rate_limiter.reset(*id);
            }
            // 落盘：内存复活后磁盘仍是 disabled=true，重启会回死态。
            if let Err(e) = self.persist_credentials() {
                tracing::warn!("恢复额度耗尽凭据后持久化失败: {}", e);
            }
        }
        recovered.len()
    }

    /// 报告指定凭据被上游暂停/封禁。
    ///
    /// 与额度用尽类似立即禁用并切换，但原因标记为 `AccountSuspended`
    /// （不可自动恢复，等待人工处理），并设置长冷却。
    /// 返回是否还有可用凭据可继续重试。
    pub fn report_account_suspended(&self, id: u64) -> bool {
        if let Some(has_enabled) = self.preserve_custom_api_state(id) {
            return has_enabled;
        }
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
            crate::common::alerting::bump("credential_disabled");

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

    /// 按错误类型处置刷新失败：**只有凭据级错误才累加失败计数**，瞬态错误只设可自愈冷却。
    ///
    /// # 为什么必须分流（线上实测的误禁链）
    ///
    /// `refresh_token_locked` 内部已对 5xx / 网络错误退避重试 3 次（1s/2s/4s，`:6922` 一带），
    /// 所以能上报到这里的错误里，**上游 token 端点抖动与链路问题占绝大多数**。旧行为
    /// （无条件 `report_refresh_failure`）把它们等同于「凭据坏」：3 次即
    /// `TooManyRefreshFailures` 禁用 + `persist_disabled_state` 落盘。
    ///
    /// 于是一次几十秒的上游抖动能永久烧掉一个完全健康的号 —— 这正是用户报的
    /// 「号是正常的却被禁用了好几次」的成因之一。
    ///
    /// # 瞬态分支为什么用 `TokenRefreshFailed` 冷却
    ///
    /// `cooldown.rs:69` 给它 60s 基线且 `is_auto_recoverable()==true`，语义正好：
    /// 让调度**暂时**跳过这个号（token 还没换到，硬打也是 401），到期自动回池。
    /// 绝不能用 `AuthenticationFailed`（`is_auto_recoverable=false` → 实际走 86400s
    /// 长硬窗 = 面板显示「冷却中」的僵尸，比禁用更难发现）。
    ///
    /// 返回：池中是否还有未禁用凭据（与 [`Self::report_refresh_failure`] 同语义，
    /// 供调用方决定继续 failover 还是透传池空错误）。
    pub fn report_refresh_failure_classified(&self, id: u64, err: &anyhow::Error) -> bool {
        if let Some(has_enabled) = self.preserve_custom_api_state(id) {
            return has_enabled;
        }
        if is_refresh_error_credential_level(err) {
            return self.report_refresh_failure(id);
        }
        // 瞬态：不计数、不禁用，只让调度暂时跳过它。
        if self.cooldown_enabled.load(Ordering::Relaxed) {
            let dur = self
                .cooldown
                .set_cooldown(id, CooldownReason::TokenRefreshFailed);
            tracing::warn!(
                "凭据 #{} 刷新失败但判为瞬态（上游/链路问题，非凭据问题）：冷却 {:?} 后自动回池，\
                 不计入永久失败（旧行为会在 3 次抖动后把健康号禁用并落盘）: {}",
                id,
                dur,
                err
            );
        } else {
            // 冷却关闭时无法「暂时跳过」，但仍然**不**计永久失败 —— 宁可下个请求再撞一次
            // 瞬态错误，也不要把健康号烧掉（烧号不可逆，多撞一次可逆）。
            tracing::warn!(
                "凭据 #{} 刷新失败但判为瞬态；cooldownEnabled=false 故无法暂时跳过它，\
                 仍不计入永久失败: {}",
                id,
                err
            );
        }
        self.entries.lock().iter().any(|e| !e.disabled)
    }

    /// 报告指定凭据刷新 Token 失败（**无条件计数**，调用前请先分类）。
    ///
    /// ⚠️ 新调用点请用 [`Self::report_refresh_failure_classified`]：本函数不区分
    /// 「上游抖动」与「凭据坏」，直接计数 3 次即禁用 + 落盘。保留它是因为
    /// `classified` 的凭据级分支要复用它，以及既有测试按名字锁着它的行为。
    ///
    /// 连续刷新失败达到阈值后禁用凭据并切换，阈值内保持当前凭据不切换，
    /// 与 API 401/403 的累计失败策略保持一致。
    pub fn report_refresh_failure(&self, id: u64) -> bool {
        if let Some(has_enabled) = self.preserve_custom_api_state(id) {
            return has_enabled;
        }
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
        if let Some(has_enabled) = self.preserve_custom_api_state(id) {
            return has_enabled;
        }
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
            crate::common::alerting::bump("credential_disabled");

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
                            if m.eq_ignore_ascii_case("builder-id") || m.eq_ignore_ascii_case("iam")
                            {
                                "idc".to_string()
                            } else {
                                m.to_string()
                            }
                        })
                    },
                    base_url: e.credentials.base_url.clone(),
                    request_limit: e.credentials.request_limit,
                    request_count: e.request_count,
                    model_mapping_exempt: e.credentials.model_mapping_exempt,
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
                    clone_group: e.credentials.clone_group.clone(),
                    clone_seq: e.credentials.clone_seq,
                    tag: e.credentials.tag.clone(),
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
                    // 实际生效的 region（真正拼进 host 的值）。同款「实际值」语义：
                    // 面板此前完全拿不到 region（行视图恒显 `—`），于是探测探错了
                    // 也看不出来。`ksk_` 打错区恒 403，这个信息是排障必需的。
                    effective_region: e.credentials.effective_upstream_region(&cfg).to_string(),
                    // 是否有人真的为这个号定过区（否则现值只是 config 全局回退）。
                    region_pinned: e.credentials.api_region.is_some()
                        || e.credentials.region.is_some()
                        || e.credentials.auth_region.is_some(),
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
    /// 手动更新 refreshToken（OAuth 号 token 轮换后的运维入口，2026-08-11）。
    ///
    /// 语义：只改 refresh_token 一个字段（field-merge，参照 `refresh_token_locked`
    /// 的回写模式与 :9574 守卫——整个 struct 替换会把在途的其它字段变更冲掉），
    /// 并清空 access_token 与 expires_at：下一次调用**必然**走刷新链路用新
    /// refresh_token 换 token（对抗审查 MAJOR，2026-08-11——只清 access_token
    /// 不清 expires_at 时，陈旧 expires_at 仍在未来，热路径命中「未过期」分支、
    /// 报「凭据无 access_token」硬错误，刷新永不触发，最长 1 小时故障窗）。
    pub fn update_refresh_token(&self, id: u64, refresh_token: String) -> anyhow::Result<()> {
        {
            let mut entries = self.entries.lock();
            let entry = entries
                .iter_mut()
                .find(|e| e.id == id)
                .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?;
            entry.credentials.refresh_token = Some(refresh_token);
            entry.credentials.access_token = None;
            entry.credentials.expires_at = None;
            // 刷新失败计数一并清零：新 token 是新起点，旧计数会让下次失败过早触发
            // TooManyRefreshFailures 禁用。
            entry.refresh_failure_count = 0;
        }
        self.persist_credentials()?;
        Ok(())
    }

    /// 手动启用/禁用凭据（Admin API）。
    ///
    /// #10 三处同步契约之「set_disabled 收口」：本方法是双份字段
    /// （disabled / disabled_reason / disabled_at / quota_exhausted_at）的**唯一收口**——
    /// 改 entry 四件套 + `persist_credentials` 落盘一步到位；自动禁用路径走
    /// `persist_disabled_state`（= persist_credentials）同样落盘。另两处同步：
    /// load 回填（`MultiTokenManager::new`）与 persist 全量写盘。
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
                // 手动启用即回到干净起点：额度耗尽判定时刻一并清掉，避免残留
                // 旧月份时间戳干扰跨月自动恢复的判定。
                entry.quota_exhausted_at = None;
                // 禁用时刻一并清掉：残留会随 persist 落盘，重启后运维看到的是旧禁用
                // 时刻，判断"这号坏了多久"被误导（#10 四件套同步契约，与 reason 同清）。
                entry.disabled_at = None;
            } else {
                entry.disabled_reason = Some(DisabledReason::Manual);
                entry.disabled_at = Some(Utc::now().to_rfc3339());
            }
        }
        // 禁用凭据时清除其会话亲和性绑定，避免后续请求重选时反复尝试已禁用凭据
        if disabled {
            self.affinity.remove_by_credential(id);
        } else {
            // Review3 m5：重新启用后模型目录缓存里可能残留禁用前的 Confirmed
            // （TTL ≤30min 且巡检循环跳过禁用号 → 残留不会自己失效），重启用后
            // 死号仍被当「模型 Confirmed」白打一跳。启用即失效缓存，让巡检重抓。
            self.invalidate_model_catalog(id);
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
            entry.credentials.rpm_limit =
                rpm_limit.filter(|&v| v > 0).map(|v| v.min(MAX_RPM_LIMIT));
        }
        self.persist_credentials()?;
        Ok(())
    }

    /// 设置凭据级端点（走哪套 Kiro 协议）。`None`/空串 = 清除显式固定，回到**自动路由**
    /// （`ksk_` 号 → `cli`，其余 → `config.defaultEndpoint`）。即时生效于下次请求。
    ///
    /// 端点名必须已注册，否则拒绝——存了不存在的名字会让该号的每个请求都在热路径上
    /// 拿到「未知端点」错误（provider 的 `endpoint_for` 返 Err），等于静默废号。
    /// 设置单个凭据的 `api_region`（Admin API）。传 `None`/空串清除（回退全局 `config.region`）。
    ///
    /// # 为什么必须有这个端点
    ///
    /// `ksk_` token **按 region 授权**，打错区上游恒 403 `bearer token invalid`
    /// 且永不自愈。而在本方法加入之前，全仓**没有任何**修改 `api_region` 的入口：
    /// `/regions` 与 `/switch-region` 都是 ARN 门控的（只对有 `profileArn` 的
    /// external_idp 号有意义），`api_key` 号一旦 region 错了**只能删号重建**。
    ///
    /// 实测事故（2026-08-05 02:42）：4 个分身因 `api_region` 缺失被打成
    /// `TooManyFailures`，而运维手上没有任何"补上 region 再启用"的手段。
    ///
    /// 校验：必须过 `is_supported_region` 白名单 —— 污染值会拼出
    /// `q.{垃圾}.amazonaws.com` / `runtime.{垃圾}.kiro.dev`，DNS 失败或 502，
    /// 而那个失败长得像"号坏了"，会把排查带偏。
    pub fn set_credential_api_region(&self, id: u64, region: Option<String>) -> anyhow::Result<()> {
        let cleaned = region
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        if let Some(ref r) = cleaned {
            if !KiroCredentials::is_supported_region(r) {
                anyhow::bail!(
                    "不支持的 region: {}（必须在白名单内，否则会拼出无法解析的 host）",
                    r
                );
            }
        }
        {
            let mut entries = self.entries.lock();
            let entry = entries
                .iter_mut()
                .find(|e| e.id == id)
                .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?;
            entry.credentials.api_region = cleaned;
        }
        self.persist_credentials()?;
        Ok(())
    }

    /// 设置凭据的**模型映射豁免**开关（`model_mapping_exempt`）。
    ///
    /// 对 Kiro 号与 custom_api 代挂号都有效（映射对两条路径都生效，豁免也应都可用）。
    /// `Some(true)` = 该号发上游时保持客户端原始模型名，跳过全局 `config.model_mapping`。
    /// 写盘走 `persist_credentials`，立即生效无需重启。
    pub fn set_credential_model_mapping_exempt(
        &self,
        id: u64,
        exempt: Option<bool>,
    ) -> anyhow::Result<()> {
        {
            let mut entries = self.entries.lock();
            let entry = entries
                .iter_mut()
                .find(|e| e.id == id)
                .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?;
            entry.credentials.model_mapping_exempt = exempt;
        }
        self.persist_credentials()?;
        Ok(())
    }

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

    /// 设置分身标签（Admin API）。传空字符串清除。
    ///
    /// 与 `set_credential_name` 分开：`name` 是**账号**别名（多开时各份复制自同一个源），
    /// 而 tag 描述的是**这一份**的差异（如「日本出口」）。同款截断防呆。
    pub fn set_credential_tag(&self, id: u64, tag: Option<String>) -> anyhow::Result<()> {
        {
            let mut entries = self.entries.lock();
            let entry = entries
                .iter_mut()
                .find(|e| e.id == id)
                .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?;
            entry.credentials.tag = tag
                .map(|s| s.trim().chars().take(MAX_NAME_CHARS).collect::<String>())
                .filter(|s| !s.is_empty());
        }
        self.persist_credentials()?;
        Ok(())
    }

    /// 回填分身组身份（组 UUID + 组内序号）。仅供多开路径在入池后调用。
    ///
    /// 单独一个函数而不是在 `add_credential` 里做：序号要接着**组内既有最大值**编号，
    /// 而那个值只有等本份入池后才好一次算清（见 service.rs 的调用点注释）。
    /// 池内「需要 region 探测」的凭据 id 列表：非代挂的 api_key 号，
    /// 且**完全没有任何 region 字段**。
    ///
    /// 供启动时的存量回填使用。判据与 [`Self::probe_and_persist_api_region`] 的预判
    /// 逐条一致 —— 两处不一致时回填会喂进一批 no-op id（只是浪费，不影响正确性），
    /// 但仍应保持同步以免读代码时误解。
    pub fn ids_needing_region_probe(&self) -> Vec<u64> {
        self.entries
            .lock()
            .iter()
            .filter(|e| !e.disabled)
            .filter(|e| needs_api_region_probe(&e.credentials))
            .map(|e| e.id)
            .collect()
    }

    /// 需要模型目录巡检的凭据 id（模型感知正向路由，S3）：custom_api && 未禁用
    /// （仿 `ids_needing_region_probe` 先例）。
    ///
    /// 🔴 2026-08-16：deepseek 归一化已移除，不再有「跳过巡检的 deepseek 号」——
    /// 所有 custom_api 号都巡检。此前 deepseek 号跳过是因为请求被改写后的目标名
    /// （fallback_model，可配任意值）与 `/models` 目录的原生名对应关系不可预测；
    /// 请求不再改写（仅 model_mapping 表映射，正向路由判定键本来就是映射后名），
    /// 该排除条件消失。ksk 号天然排除（`is_custom_api_credential` 结构性排除，
    /// Kiro 池没有模型目录概念，设计文档 §6 铁律：正向路由只做透传池）。
    pub fn ids_needing_model_probe(&self) -> Vec<u64> {
        self.entries
            .lock()
            .iter()
            .filter(|e| !e.disabled)
            .filter(|e| e.credentials.is_custom_api_credential())
            .map(|e| e.id)
            .collect()
    }

    /// 巡检单飞锁：per-id TokioMutex（同一凭据不并发 fetch；换上游/删号时随
    /// `invalidate_model_catalog` 移除，换上游后旧锁无意义）。
    fn model_catalog_lock(&self, id: u64) -> Arc<TokioMutex<()>> {
        self.model_catalog_locks
            .lock()
            .entry(id)
            .or_insert_with(|| Arc::new(TokioMutex::new(())))
            .clone()
    }

    /// 该号是否在巡检失败退避中（退避期间本轮跳过，状态留到到期）。
    fn model_catalog_in_backoff(&self, id: u64) -> bool {
        self.model_catalog_backoff
            .lock()
            .get(&id)
            .is_some_and(|b| b.until > Instant::now())
    }

    /// 巡检失败 → 指数退避：第 n 次连续失败等 `BASE × 2^(n-1)`，上限 30min
    /// （设计文档 §5 表）。成功（非空 2xx）在 `store_model_catalog` 重置。
    fn bump_model_catalog_backoff(&self, id: u64) {
        let mut map = self.model_catalog_backoff.lock();
        let failures = map.get(&id).map(|b| b.failures).unwrap_or(0) + 1;
        let shift = failures.min(MODEL_CATALOG_BACKOFF_MAX_SHIFT);
        let wait = StdDuration::from_secs(
            MODEL_CATALOG_BACKOFF_BASE_SECS
                .saturating_mul(1u64 << shift.saturating_sub(1))
                .min(MODEL_CATALOG_BACKOFF_MAX_SECS),
        );
        map.insert(
            id,
            CatalogBackoff {
                failures,
                until: Instant::now() + wait,
            },
        );
    }

    /// 执行一轮模型目录巡检（S3，设计文档 §5）。返回成功写入缓存的号数（测试锚点）。
    ///
    /// `fetch` 注入以支持单测 mock（按值传凭据快照，规避「闭包返回借用参数的
    /// future」的 HRTB 难题）；生产路径在 [`Self::spawn_model_catalog_probe`] 里接
    /// [`crate::kiro::passthrough::fetch_upstream_models`]。
    ///
    /// 单飞 + double-check：先查退避（退避中跳过），再拿 per-id 锁；拿锁后**再查
    /// 一次缓存新鲜度**——并发多路触发（双任务 / 手动刷新）时第一路刚写完，第二路
    /// 看到目录仍新鲜（< TTL）即跳过，并发 N 次 probe 只打一次网络（验收 9）。
    /// 正常周期下每轮恰好 TTL 到期重探，double-check 不影响周期语义。
    pub async fn probe_model_catalog_round<F, Fut>(&self, fetch: F) -> usize
    where
        F: Fn(KiroCredentials) -> Fut,
        Fut: std::future::Future<Output = anyhow::Result<Vec<String>>>,
    {
        let mut written = 0;
        for id in self.ids_needing_model_probe() {
            if self.model_catalog_in_backoff(id) {
                continue;
            }
            let catalog_lock = self.model_catalog_lock(id);
            let _lock = catalog_lock.lock().await;
            let fresh = self
                .model_catalog_cache
                .lock()
                .get(&id)
                .is_some_and(|e| e.refreshed_at.elapsed() < MODEL_CATALOG_TTL);
            if fresh {
                continue;
            }
            let Some(cred) = self
                .entries
                .lock()
                .iter()
                .find(|e| e.id == id)
                .map(|e| e.credentials.clone())
            else {
                continue;
            };
            match fetch(cred).await {
                Ok(models) if !models.is_empty() => {
                    self.store_model_catalog(id, models);
                    written += 1;
                }
                // 空列表：不写缓存（保持 Unknown）、不算失败、不重置退避——空列表
                // 可能是上游暂时故障，固化成「无模型」会让目录失真永久化（守卫 #6）。
                Ok(_empty) => {}
                Err(e) => {
                    self.bump_model_catalog_backoff(id);
                    tracing::warn!(
                        credential_id = id,
                        "模型目录巡检失败（该号维持 Unknown）: {e:#}"
                    );
                }
            }
        }
        written
    }

    /// 启动模型目录巡检后台任务（30min 周期；首轮延迟 10s 避开启动期 token 预刷新
    /// 抢上游往返；`MissedTickBehavior::Skip` 防唤醒后连刷——main.rs affinity 清理
    /// 同款，设计文档 §5）。循环持 `Weak<Self>`：manager 被 drop 后下一轮 upgrade
    /// 失败即自我退出（`respawn_refresh_task` 同款，不构成 Arc 引用环）。
    /// 接线点：main.rs 启动路径（region 回填 :494 同款 spawn）。
    pub fn spawn_model_catalog_probe(self: &Arc<Self>) -> JoinHandle<()> {
        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            tokio::time::sleep(StdDuration::from_secs(MODEL_CATALOG_PROBE_START_DELAY_SECS))
                .await;
            let mut ticker = tokio::time::interval(StdDuration::from_secs(MODEL_CATALOG_TTL_SECS));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                let Some(mgr) = weak.upgrade() else {
                    return;
                };
                // 每轮取配置快照：proxy/tls 热更后下一轮生效（region 回填同款取法）。
                let cfg = mgr.config();
                let proxy = cfg.proxy_url.as_deref().map(ProxyConfig::new);
                let tls = cfg.tls_backend;
                let fetch = move |cred: KiroCredentials| {
                    let proxy = proxy.as_ref().cloned();
                    async move {
                        crate::kiro::passthrough::fetch_upstream_models(
                            &cred,
                            proxy.as_ref(),
                            tls,
                        )
                        .await
                    }
                };
                mgr.probe_model_catalog_round(fetch).await;
                ticker.tick().await;
            }
        })
    }

    /// 对指定凭据探测真正可用的 region，命中则**写死进凭据并落盘**。
    ///
    /// 上号路径（`add_credential`）调用。只对「api_key 号且完全没有任何 region 字段」
    /// 生效，判据在 [`crate::kiro::region_probe::probe_api_region`] 内；其余凭据是 no-op。
    ///
    /// # 为什么写死而不是靠全局默认值
    ///
    /// `ksk_` token 按 region 授权，打错区上游恒 403。不带 region 字段的凭据会回退到
    /// `config.region`，于是**它对不对纯靠运气**，而且改 `config.region` 会同时废掉
    /// 依赖旧值的那一半号（线上真实状态：3 个号里 2 个靠回退恰好对）。
    /// 探一次写死之后，该号从此不依赖任何全局默认值。
    ///
    /// # 失败即静默返回（绝不阻塞上号）
    ///
    /// 探不出结论时保持凭据原样（回退既有行为）。上号是用户交互路径，
    /// 探测最多 [`crate::kiro::region_probe::MAX_PROBE_ATTEMPTS`] 次上游往返，
    /// 且实测候选集只有 2 个真实存在的 region，所以最坏代价有界。
    pub async fn probe_and_persist_api_region(
        &self,
        id: u64,
    ) -> crate::kiro::region_probe::ProbeOutcome {
        use crate::kiro::region_probe::ProbeOutcome;
        // 先取一次凭据快照做**廉价预判**：不满足条件就完全不碰 token（避免为一个
        // no-op 去刷 token，那是一次真实上游往返）。
        let snapshot = {
            let entries = self.entries.lock();
            match entries.iter().find(|e| e.id == id) {
                Some(e) => e.credentials.clone(),
                None => return ProbeOutcome::Skipped,
            }
        };
        if !needs_api_region_probe(&snapshot) {
            return ProbeOutcome::Skipped;
        }

        let (credentials, token) = match self.ensure_valid_token(id).await {
            Ok(v) => v,
            Err(e) => {
                // ⚠️ 取 token 失败判 `Skipped` 而**不是** `NoUsableRegion`：
                // 前者让调用方照常启用，后者会禁用凭据。取不到 token 的成因绝大多数是
                // 上游/链路抖动（`refresh_token_locked` 内部已对 5xx/网络退避重试 3 次），
                // 号本身通常是好的 —— 据此禁用就是把链路抖动记成号坏，而那正是本仓
                // 反复修过的误禁形态（见 `is_refresh_error_credential_level` 的长注释）。
                tracing::warn!("凭据 #{} region 探测取 token 失败，跳过探测: {}", id, e);
                return ProbeOutcome::Skipped;
            }
        };
        let cfg = self.config.load_full();
        let outcome = crate::kiro::region_probe::probe_api_region(
            &credentials,
            &cfg,
            &token,
            crate::kiro::region_probe::PROBE_ORDER,
        )
        .await;

        let region = match outcome {
            ProbeOutcome::Usable(r) => r,
            // 探不出可用 region / token 已废：**原样返回判决**，由调用方决定禁用与否。
            // 这里刻意不自己禁用 —— 启动回填路径（`main.rs`）面对的是**已在服役**的存量号，
            // 禁用它们会把一个正在成功出活的号（靠 config.region 恰好对）打掉；
            // 而上号路径（`add_credential`）面对的是尚未接流量的新号，必须禁用。
            // 同一个判决在两条路径上的正确处置不同，故判决权归调用方。
            other => return other,
        };

        {
            let mut entries = self.entries.lock();
            match entries.iter_mut().find(|e| e.id == id) {
                Some(entry) => entry.credentials.api_region = Some(region.clone()),
                None => return ProbeOutcome::Skipped, // 探测期间被删了
            }
        }
        if let Err(e) = self.persist_credentials() {
            // 内存已生效，但重启后会丢 → 如实告警，让运维知道该号下次重启要重探。
            tracing::warn!(
                "凭据 #{} region 探测命中 {} 并已在内存生效，但落盘失败（重启后将回退 config.region）: {}",
                id,
                region,
                e
            );
        } else {
            tracing::info!(
                "凭据 #{} region 自动探测命中 {}，已写死进凭据（此后不再依赖 config.region）",
                id,
                region
            );
        }
        ProbeOutcome::Usable(region)
    }

    /// 探测判决为「不可用」时把凭据**保持/置为禁用**，并写一个可归因的原因。
    ///
    /// # 为什么需要它（这是 P0 的另一半）
    ///
    /// 只让 `add_credential` 以禁用态入池是不够的：探测失败后若什么都不做，号会**永久**
    /// 停在 `Manual` 原因上，运维在面板上看到的是「手动禁用」——而没人手动禁过它。
    /// 写明原因才能让人知道该去查 region 授权还是查 token。
    ///
    /// 两个原因都**不在** `is_self_healable_reason` 白名单里（那是白名单，新变体天然排除），
    /// 所以自愈不会把它们捞回池子重演。这一条是承重的：线上实测自愈 24h 内跑了 44 次，
    /// 若能捞回，禁用等于没做。
    pub fn mark_region_probe_failed(
        &self,
        id: u64,
        outcome: &crate::kiro::region_probe::ProbeOutcome,
    ) {
        use crate::kiro::region_probe::ProbeOutcome;
        if self.preserve_custom_api_state(id).is_some() {
            return;
        }
        let reason = match outcome {
            ProbeOutcome::NoUsableRegion => DisabledReason::RegionProbeFailed,
            ProbeOutcome::TokenDead => DisabledReason::RegionProbeTokenDead,
            // Usable / Skipped 不该走到这里；静默返回而不是 panic —— 调用方写错
            // 不应该打死正在服务的进程。
            _ => return,
        };
        {
            let mut entries = self.entries.lock();
            let Some(entry) = entries.iter_mut().find(|e| e.id == id) else {
                return;
            };
            entry.disabled = true;
            entry.disabled_reason = Some(reason);
            entry.disabled_at = Some(Utc::now().to_rfc3339());
        }
        tracing::warn!(
            credential_id = id,
            ?reason,
            "region 探测未得出可用 region，凭据保持禁用（启用会让它恒 403 并被自动禁用；\
             人工确认 region 后可在面板手动启用）"
        );
        // 复用既有的禁用态落盘收口：重启后不该以启用态复活。
        self.persist_disabled_state(id);
    }

    pub fn set_clone_identity(
        &self,
        id: u64,
        group: Option<String>,
        seq: Option<u32>,
    ) -> anyhow::Result<()> {
        {
            let mut entries = self.entries.lock();
            let entry = entries
                .iter_mut()
                .find(|e| e.id == id)
                .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?;
            entry.credentials.clone_group = group.filter(|s| !s.trim().is_empty());
            entry.credentials.clone_seq = seq;
            // 族键依赖 clone_group，变更后必须失效缓存（下次读取时惰性重算）。
            entry.family_key = None;
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
        // 换上游 / 换 key → 旧模型目录对新高地无意义（S4 失效挂点，设计文档 §2）；
        // 在参数被 move 进下方 if let 前取标记。
        let invalidate_catalog = base_url.is_some() || api_key.is_some();
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
            // ⚠️ N5（2026-08-16）：**必须与 success_count 成对清零** —— 旧实现只清
            // request_count，success_count 仍是终身 ⇒ 面板出现「成功数 > 请求数」的自相矛盾
            // （线上实测 #1 669/495、#2 477/396，两号都在 08-14 初被清过一次）。
            // 安全性：本函数只放行 custom_api 号（上方 gate），而 `has_ever_succeeded()`
            // 的调用点全在 Kiro 主路径（custom_api 被 `is_entry_selectable` 结构性排除），
            // 清零 success_count 不会破坏「bearer-invalid 403 = 瞬态抖动」的判据。
            // （`reset_and_enable` 的 RequestLimitReached 兼容分支刻意**不清** success_count：
            //  它可能命中 Kiro 号，那里 success_count>0 是承重的 has_ever_succeeded 判据。）
            if reset_count {
                entry.request_count = 0;
                entry.success_count = 0;
            }
        }
        self.persist_credentials()?;
        // entries 锁外失效（invalidate 有自己的锁，避免锁序嵌套）。
        if invalidate_catalog {
            self.invalidate_model_catalog(id);
        }
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
                anyhow::bail!("凭据 #{} 因配置无效被禁用，请修正配置后重启服务", id);
            }
            // 兼容历史数据：旧版曾在代挂站达到 request_limit 时写入
            // RequestLimitReached。管理员对这种老条目执行「重置并启用」时同时清计数。
            // 新版 request_limit 只观测、不会再产生该禁用原因。
            // ⚠️ 这里**只清 request_count 不清 success_count**（与 set_custom_api_config
            // 的 reset_count 不对称是刻意的）：本函数不限定凭据类型，可能命中 Kiro 号，
            // 而 Kiro 号 `success_count > 0` 是 `has_ever_succeeded()` 的承重判据
            // （provider 区分「bearer-invalid 403 = 瞬态抖动」与「真 region 错配」）。
            // N5 修复只落在 set_custom_api_config 的 reset_count（custom_api-only，安全）。
            if entry.disabled_reason == Some(DisabledReason::RequestLimitReached) {
                entry.request_count = 0;
            }
            // 全部进程内惩罚计数走单一收口（含此前漏清的 consecutive_suspicious）。
            entry.clear_transient_counters();
            entry.disabled = false;
            entry.disabled_reason = None;
            // 手动「重置并启用」同样回到干净起点（对齐 set_disabled 启用分支）。
            entry.quota_exhausted_at = None;
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
        let usage_limits =
            match get_usage_limits(&credentials, &cfg, &token, effective_proxy.as_ref()).await {
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
                        entry.credentials.subscription_title = Some(subscription_title.to_string());
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
                anyhow::bail!("API Key 凭据不支持 Web Portal 接口（overage 开关仅限 social 凭据）");
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
        let client =
            build_client_no_redirect(effective_proxy.as_ref(), 30, self.config.load().tls_backend)?;

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
                .map(
                    |(model, status, _credits)| crate::kiro::model::credentials::TestedModel {
                        model: model.clone(),
                        status: status.clone(),
                        tested_at: now.clone(),
                    },
                )
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
            additional_model_request_fields: None,
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
                        Event::Error {
                            error_code,
                            error_message,
                        } => {
                            if crate::kiro::endpoint::default_is_invalid_model_id(&error_code)
                                || crate::kiro::endpoint::default_is_invalid_model_id(
                                    &error_message,
                                )
                            {
                                invalid = true;
                            }
                            stop = true;
                        }
                        Event::Exception {
                            exception_type,
                            message,
                        } => {
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
        let mut validated_cred =
            if new_cred.is_api_key_credential() || new_cred.is_custom_api_credential() {
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

            // ══════════════════════════════════════════════════════════════════
            // 🔴 去重复检（TOCTOU 收口）
            //
            // 第 2 步的查重是在**另一把已经释放的锁**里做的：那段用
            // `let ... = { let entries = self.entries.lock(); ... }` 取值后立刻出作用域
            // 释放（`parking_lot::Mutex` 无 guard 逃逸），而真正的 `push` 在这里、
            // 隔着一次**重新取锁**。两段临界区之间没有任何互斥 ⇒
            // 「查重通过」与「插入」不是原子的。
            //
            // 为什么这不是理论问题（三个条件同时成立）：
            // 1. `import_keys` 用 `Semaphore(IMPORT_MAX_IN_FLIGHT)` **并发**派发每条；
            // 2. `#[tokio::main]` 是多线程运行时，两个 worker 真并行；
            // 3. **api_key 号在第 3 步走 `new_cred.clone()` 分支、不执行那次
            //    `refresh_token().await`** ⇒ 从查重到插入是一段**纯同步**代码，
            //    不需要任何 await 交错就能让两个线程都通过查重。
            //
            // 后果不是"多一条记录"：同一账号在池中裂成 N 条、共用一份上游配额，
            // 而上游按**账号**算风控。CLAUDE.md 记载的线上事故形态正是如此
            // （克隆 10 份，15 分钟后父号连同分身共 11 个全被 suspiciousActivityAuto
            // 禁用）—— 那次是用户显式多开，本路径则能在用户**没有多开意图**时复现。
            //
            // 【为什么不照抄参考实现】实测参考仓有**同一个**竞态：GreyGunG
            // `token_manager.rs:2759` 查重（锁随即释放）、`:2841` 重新取锁 push，
            // 中间同样没有复检（`awk` 扫 2807-2841 无任何 sha256/重复判据）。
            // 所以这条不是"移植他们的做法"能解决的，必须自己收口。
            //
            // 【收口方式】在**已经持有的这把锁**里复检一次同判据：不引入新锁、
            // 不引入新 await ⇒ 不改变锁顺序、不可能死锁、不增加热路径开销
            // （只在加号时跑一次，不在请求路径上）。
            //
            // 判据与第 2 步保持**同源**：三类凭据各自用自己的键（custom_api =
            // base_url+api_key、api_key = kiroApiKey、OAuth = refreshToken），
            // 否则复检会与初检不一致 —— 那比不复检更难排查。
            // `allow_duplicate`（多开）时整段跳过，与第 2 步的门控完全对称。
            // ══════════════════════════════════════════════════════════════════
            if !allow_duplicate {
                let dup_now = if validated_cred.is_custom_api_credential() {
                    // 与第 2 步**逐字节同键**：`base_url|api_key`（分隔符 `|`、字段
                    // 是 `api_key` 而非 kiro_api_key），且同样**跳过非 custom_api 条目**。
                    // 键的任何差异都会让复检与初检判定不一致，那比不复检更难排查。
                    let dup_key = format!(
                        "{}|{}",
                        validated_cred.base_url.as_deref().unwrap_or(""),
                        validated_cred.api_key.as_deref().unwrap_or("")
                    );
                    let want = sha256_hex(&dup_key);
                    entries.iter().any(|e| {
                        if !e.credentials.is_custom_api_credential() {
                            return false;
                        }
                        let k = format!(
                            "{}|{}",
                            e.credentials.base_url.as_deref().unwrap_or(""),
                            e.credentials.api_key.as_deref().unwrap_or("")
                        );
                        sha256_hex(&k) == want
                    })
                } else if let Some(api_key) = validated_cred.kiro_api_key.as_deref() {
                    let want = sha256_hex(api_key);
                    entries.iter().any(|e| {
                        e.credentials
                            .kiro_api_key
                            .as_deref()
                            .map(sha256_hex)
                            .as_deref()
                            == Some(want.as_str())
                    })
                } else if let Some(rt) = validated_cred.refresh_token.as_deref() {
                    // 与第 2 步同判据：比 **sha256 后**的值而非明文。两者结果等价，
                    // 但保持同源写法 —— 一旦将来有人给初检加盐/换算法，复检不会静默分叉。
                    let want = sha256_hex(rt);
                    entries.iter().any(|e| {
                        e.credentials
                            .refresh_token
                            .as_deref()
                            .map(sha256_hex)
                            .as_deref()
                            == Some(want.as_str())
                    })
                } else {
                    false
                };
                if dup_now {
                    // 与第 2 步同文案族：调用方（`import_keys` 的逐条 `results[].error`）
                    // 不需要区分"初检时已存在"与"并发插入抢先"，两者都是"已存在"。
                    // 但保留"并发"字样便于事后从日志判断竞态是否真的发生过。
                    anyhow::bail!("凭据已存在（并发插入被去重复检拦截）");
                }
            }

            // 族键缓存：构造前先算（validated_cred 随即被 move 进 entry）。
            let entry_family_key = validated_cred.family_key(new_id);
            entries.push(CredentialEntry {
                id: new_id,
                credentials: validated_cred,
                failure_count: 0,
                refresh_failure_count: 0,
                disabled_at: None,
                quota_exhausted_at: None,
                consecutive_suspicious: 0,
                consecutive_passthrough_failures: 0,
                last_selected_at: std::cell::Cell::new(Instant::now()),
                last_failure_at: std::cell::Cell::new(None),
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
                family_key: Some(entry_family_key),
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
            // 额度耗尽判定时刻同款同步进回收站（restore 时原样带回，跨月恢复判据才不丢）。
            cred.quota_exhausted_at = removed.quota_exhausted_at.clone();
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
        self.model_blocklist
            .lock()
            .retain(|(cred_id, _), _| *cred_id != id);
        // 模型目录缓存 / 退避 / 单飞锁同款清（删号防内存残留；restore 后重新巡检，
        // 不会背着旧目录——设计文档 §2 失效挂点）。
        self.invalidate_model_catalog(id);
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
                // 族键缓存：构造前先算（cred 随即被 move 进 entry）。
                let entry_family_key = cred.family_key(id);
                let mut entries = self.entries.lock();
                entries.push(CredentialEntry {
                    id,
                    credentials: cred,
                    failure_count: 0,
                    refresh_failure_count: 0,
                    disabled_at: None,
                    quota_exhausted_at: None,
                    consecutive_suspicious: 0,
                    consecutive_passthrough_failures: 0,
                    last_selected_at: std::cell::Cell::new(Instant::now()),
                last_failure_at: std::cell::Cell::new(None),
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
                    family_key: Some(entry_family_key),
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
    /// `force`：跳过 key 重复校验。**多开分身与主凭据必然同 key**，
    /// 不给这个出口的话删掉的分身永远恢复不了（用户直接反馈过这个现象：
    /// 面板反复弹「凭据已存在（kiroApiKey 重复），无法恢复」）。
    /// 默认 false 保留误操作护栏；恢复后仍是**禁用态**，故强制恢复不会立刻投入调度。
    pub fn restore_credential(&self, id: u64, force: bool) -> anyhow::Result<()> {
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

            // 去重校验：与现有 entries 比对 refreshToken / kiroApiKey 哈希。
            //
            // 🔴 修复的缺陷（用户直接反馈）：判据原先**只看 key**，而「多开」造出的分身
            // 与主凭据**必然同 key** —— 于是：
            // ① 删掉的分身永远恢复不了（池里还有主凭据）；
            // ② 主凭据也恢复不了（池里还有任一分身）。
            // 面板上表现为反复弹「凭据已存在（kiroApiKey 重复），无法恢复」。
            //
            // 修法：加 `force` 参数显式绕过。**刻意不改判据本身** ——
            // 我第一版试过把判据改成「key + machineId」，被既有测试
            // `test_restore_duplicate_refresh_token_rejected` 抓住并证否：
            // 入池时 machineId 撞车会**自动轮换**（同 key 派生出同一指纹 → 第二个换成随机值），
            // 所以同 key 的凭据在池里 machineId **永远不同** → 那个判据永不命中
            // → 等于把护栏整个拆掉。
            //
            // 用 force 而非放宽判据：默认仍拒（误操作护栏保留、既有语义不变），
            // 分身恢复由调用方显式声明意图 —— 与 `delete_credentials_batch` 的 force 同款设计。
            let cred = &trash[idx].credentials;
            if force {
                tracing::warn!(
                    "凭据 #{} 强制恢复：跳过 key 重复校验（多开分身与主凭据必然同 key）",
                    id
                );
            } else if cred.is_api_key_credential() {
                if let Some(new_hash) = cred.kiro_api_key.as_deref().map(sha256_hex) {
                    let dup = entries.iter().any(|e| {
                        e.credentials
                            .kiro_api_key
                            .as_deref()
                            .map(sha256_hex)
                            .as_deref()
                            == Some(new_hash.as_str())
                    });
                    if dup {
                        // 文案要能指导操作：多开分身与主凭据必然同 key，
                        // 用户看到这句才知道该走"强制恢复"而不是以为凭据坏了。
                        anyhow::bail!(
                            "凭据已存在（kiroApiKey 重复），无法恢复。若这是多开分身（与主凭据同 key 属正常），请用强制恢复"
                        );
                    }
                }
            } else if let Some(new_hash) = cred.refresh_token.as_deref().map(sha256_hex) {
                let dup = entries.iter().any(|e| {
                    e.credentials
                        .refresh_token
                        .as_deref()
                        .map(sha256_hex)
                        .as_deref()
                        == Some(new_hash.as_str())
                });
                if dup {
                    anyhow::bail!(
                        "凭据已存在（refreshToken 重复），无法恢复。若这是多开分身，请用强制恢复"
                    );
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
            // 额度耗尽判定时刻同样保留（与 disabled_reason/disabled_at 同款信息保留策略）：
            // 若这号当初因额度耗尽被禁、恢复时已跨自然月，跨月自动恢复会接住它。
            let restored_quota_at = cred.quota_exhausted_at.clone();
            // 族键缓存：构造前先算（cred 随即被 move 进 entry）。
            let entry_family_key = cred.family_key(id);
            entries.push(CredentialEntry {
                id,
                credentials: cred,
                failure_count: 0,
                refresh_failure_count: 0,
                disabled_at: restored_at,
                quota_exhausted_at: restored_quota_at,
                consecutive_suspicious: 0,
                consecutive_passthrough_failures: 0,
                last_selected_at: std::cell::Cell::new(Instant::now()),
                last_failure_at: std::cell::Cell::new(None),
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
                family_key: Some(entry_family_key),
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
            tracing::info!(
                "凭据 #{} 新号自动初始化开始(刷新 Token + 解析 profileArn)",
                id
            );
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
                    // 族键在 issuer_url 解析失败时退化为 profileArn 兜底，变更后须失效缓存。
                    entry.family_key = None;
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
                bail!(
                    "目标 region profile 不可用（FEATURE_NOT_SUPPORTED，该 region 未开通），未切换"
                )
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
                    // 族键在 issuer_url 解析失败时退化为 profileArn 兜底，变更后须失效缓存。
                    entry.family_key = None;
                    if let Some(t) = &best.subscription_title {
                        entry.credentials.subscription_title = Some(t.clone());
                    }
                    tracing::info!(
                        "凭据 #{} 验活重选：{:?} → {}（region={}, {}）",
                        id,
                        old,
                        best.arn,
                        best.region,
                        best.subscription_title.as_deref().unwrap_or("?")
                    );
                }
                // 无论 arn 是否变，清除坏标记 + 清空全坏冷却时间戳(恢复灵敏)。
                entry
                    .last_usage_403_feature_not_supported
                    .store(false, Ordering::Relaxed);
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
                id,
                REPROBE_ALL_BAD_COOLDOWN.as_secs() / 3600
            );
            false
        }
    }

    /// 标记某号上次对话/查询撞了 403 FEATURE_NOT_SUPPORTED(供后台刷新循环 needs_reprobe 门兜底纠正)。
    pub fn mark_usage_403_feature_not_supported(&self, id: u64) {
        let entries = self.entries.lock();
        if let Some(entry) = entries.iter().find(|e| e.id == id) {
            entry
                .last_usage_403_feature_not_supported
                .store(true, Ordering::Relaxed);
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
            let Some(entry) = entries.iter().find(|e| e.id == id) else {
                return;
            };
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
            let _guard = InFlightGuard {
                tm: Arc::clone(&this),
                id,
            };

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
            if this
                .reprobe_and_correct_region_with(id, &creds, &token)
                .await
            {
                if let Err(e) = this.persist_credentials() {
                    tracing::warn!(
                        "凭据 #{} 后台重探纠正 region 后持久化失败(不影响本次纠正): {}",
                        id,
                        e
                    );
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
                    // 同请求路径：只有凭据级错误才计数。后台预刷新每 5s 跑一次
                    // （`tokenRefreshIntervalSecs`），无条件计数时上游抖动几十秒就能把
                    // 全池刷成 TooManyRefreshFailures —— 这条路径比热路径更容易连打。
                    self.report_refresh_failure_classified(id, &e);
                }
            }
        }
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
#[path = "token_manager_endpoint_bypass_guard_tests.rs"]
mod endpoint_bypass_guard_tests;

#[cfg(test)]
#[path = "token_manager_tests.rs"]
mod tests;
