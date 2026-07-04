//! Kiro Web Portal API 客户端（app.kiro.dev）
//!
//! 移植自 Foxfishc kiro.rs（MIT），适配 KiroStudio 的 KiroCredentials / build_client 结构。
//!
//! 协议要点：
//! - POST https://app.kiro.dev/service/KiroWebPortalService/operation/{Operation}
//! - 协议：rpc-v2-cbor（请求/响应体均为 CBOR，不是 JSON）
//! - Content-Type/Accept: application/cbor
//! - Authorization: Bearer <accessToken>
//! - Cookie: Idp=<idp>; AccessToken=<accessToken>（写操作再附加 UserId）
//!
//! 本模块只承载「超额（overage）真开关」所需的最小接口：
//! - `get_user_usage_and_limits`：读取当前 overage 配置（只读）
//! - `update_billing_preferences`：开启/关闭 overage（写操作，需 CSRF）
//! - `fetch_csrf_session`：写操作前拿 CSRF token + UserId

#![allow(dead_code)]

use std::time::Duration;

use anyhow::Context;
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, COOKIE, HeaderMap, HeaderValue};

use crate::http_client::{ProxyConfig, build_client};
use crate::model::config::TlsBackend;

const KIRO_API_BASE: &str = "https://app.kiro.dev/service/KiroWebPortalService/operation";
const KIRO_HOME_URL: &str = "https://app.kiro.dev/";
const SMITHY_PROTOCOL: &str = "rpc-v2-cbor";
const AMZ_SDK_REQUEST: &str = "attempt=1; max=1";
const X_AMZ_USER_AGENT: &str = "aws-sdk-js/1.0.0 kiro-rs/1.0.0";

/// 调用写操作（如 UpdateBillingPreferences）必需的 CSRF 会话上下文
///
/// 通过带 Cookie 访问 `https://app.kiro.dev/` 拿到 HTML，从两个 meta 标签提取：
/// - `<meta name="csrf-token" content="...">` —— 写操作必须放进 `X-CSRF-Token` header；
/// - `<meta name="user-id" content="...">` —— 后续写操作的 Cookie 必须把它带回去
///   （CSRF 校验依赖这个 cookie）。
#[derive(Debug, Clone)]
struct CsrfSession {
    csrf_token: String,
    user_id: String,
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GetUserUsageAndLimitsRequest {
    pub is_email_required: bool,
    pub origin: String,
}

#[derive(Debug, serde::Serialize, serde::Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct UsageUserInfo {
    pub email: Option<String>,
    pub user_id: Option<String>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct SubscriptionInfo {
    pub r#type: Option<String>,
    pub subscription_title: Option<String>,
    pub overage_capability: Option<String>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct OverageConfiguration {
    pub overage_enabled: Option<bool>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct UsageAndLimitsResponse {
    pub user_info: Option<UsageUserInfo>,
    pub subscription_info: Option<SubscriptionInfo>,
    pub next_date_reset: Option<f64>,
    pub overage_configuration: Option<OverageConfiguration>,
}

impl UsageAndLimitsResponse {
    /// 上游明确上报的 overage 开关状态（None 表示未包含该字段）
    pub fn overage_enabled(&self) -> Option<bool> {
        self.overage_configuration
            .as_ref()
            .and_then(|c| c.overage_enabled)
    }
}

#[derive(Debug, serde::Deserialize)]
struct CborErrorResponse {
    #[serde(rename = "__type")]
    pub type_name: Option<String>,
    pub message: Option<String>,
}

fn header_value(s: &str, name: &'static str) -> anyhow::Result<HeaderValue> {
    HeaderValue::from_str(s).with_context(|| format!("{} header 无效", name))
}

fn build_headers(
    access_token: &str,
    idp: &str,
    csrf: Option<&CsrfSession>,
) -> anyhow::Result<HeaderMap> {
    let mut headers = HeaderMap::new();

    headers.insert(ACCEPT, HeaderValue::from_static("application/cbor"));
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/cbor"));
    headers.insert("smithy-protocol", HeaderValue::from_static(SMITHY_PROTOCOL));
    headers.insert(
        "amz-sdk-invocation-id",
        header_value(&uuid::Uuid::new_v4().to_string(), "amz-sdk-invocation-id")?,
    );
    headers.insert("amz-sdk-request", HeaderValue::from_static(AMZ_SDK_REQUEST));
    headers.insert(
        "x-amz-user-agent",
        header_value(X_AMZ_USER_AGENT, "x-amz-user-agent")?,
    );

    headers.insert(
        AUTHORIZATION,
        header_value(&format!("Bearer {}", access_token), "authorization")?,
    );

    // Cookie 顺序：Idp / AccessToken 是基础鉴权；UserId 仅在写操作（带 CSRF）时附加，
    // 上游 CSRF 校验需要它和 X-CSRF-Token 一起回传。
    let cookie = match csrf {
        Some(s) => format!(
            "Idp={}; AccessToken={}; UserId={}",
            idp, access_token, s.user_id
        ),
        None => format!("Idp={}; AccessToken={}", idp, access_token),
    };
    headers.insert(COOKIE, header_value(&cookie, "cookie")?);

    if let Some(s) = csrf {
        headers.insert("x-csrf-token", header_value(&s.csrf_token, "x-csrf-token")?);
    }

    Ok(headers)
}

/// 从 HTML 中提取 `<meta name="<name>" content="...">` 的 content。
///
/// 简单匹配，仅供 CSRF/User-ID 这两个固定 meta 使用。
fn extract_meta(html: &str, name: &str) -> Option<String> {
    let needle = format!("name=\"{}\"", name);
    let idx = html.find(&needle)?;
    let tail = &html[idx..];
    let content_idx = tail.find("content=\"")?;
    let after = &tail[content_idx + "content=\"".len()..];
    let end = after.find('"')?;
    Some(after[..end].to_string())
}

/// 获取 CSRF 会话上下文。
///
/// 实际请求 `https://app.kiro.dev/`（带 Idp / AccessToken Cookie），从 HTML 的
/// `<meta name="csrf-token">` 和 `<meta name="user-id">` 抽取两个值。
///
/// 上游对未登录用户返回的 HTML 里这两个 meta 都是占位注释，
/// 所以 token 失效或 idp 错误会立刻在这里失败，便于早判错。
async fn fetch_csrf_session(
    access_token: &str,
    idp: &str,
    proxy: Option<&ProxyConfig>,
    tls_backend: TlsBackend,
) -> anyhow::Result<CsrfSession> {
    let client = build_client(proxy, 30, tls_backend)?;
    let resp = client
        .get(KIRO_HOME_URL)
        .header(
            COOKIE,
            header_value(
                &format!("Idp={}; AccessToken={}", idp, access_token),
                "cookie",
            )?,
        )
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .context("获取 CSRF 会话页失败")?;

    let status = resp.status();
    let html = resp.text().await.context("读取 CSRF 会话页响应失败")?;
    if !status.is_success() {
        anyhow::bail!("获取 CSRF 会话页失败：HTTP {}", status);
    }

    let csrf_token = extract_meta(&html, "csrf-token")
        .ok_or_else(|| anyhow::anyhow!("HTML 未包含 csrf-token meta（access_token 可能已失效）"))?;
    let user_id = extract_meta(&html, "user-id")
        .ok_or_else(|| anyhow::anyhow!("HTML 未包含 user-id meta（access_token 可能已失效）"))?;

    Ok(CsrfSession {
        csrf_token,
        user_id,
    })
}

async fn request_cbor<TResp, TReq>(
    operation: &str,
    req: &TReq,
    access_token: &str,
    idp: &str,
    proxy: Option<&ProxyConfig>,
    tls_backend: TlsBackend,
    csrf: Option<&CsrfSession>,
) -> anyhow::Result<TResp>
where
    TResp: for<'de> serde::Deserialize<'de>,
    TReq: serde::Serialize,
{
    let url = format!("{}/{}", KIRO_API_BASE, operation);

    let body = serde_cbor::to_vec(req).context("CBOR 编码失败")?;

    let client = build_client(proxy, 60, tls_backend)?;

    let resp = client
        .post(&url)
        .headers(build_headers(access_token, idp, csrf)?)
        .timeout(Duration::from_secs(60))
        .body(body)
        .send()
        .await
        .context("请求 Kiro Web Portal API 失败")?;

    let status = resp.status();
    let bytes = resp.bytes().await.context("读取响应失败")?;

    if !status.is_success() {
        // 尽力解析 CBOR 错误体
        if let Ok(err) = serde_cbor::from_slice::<CborErrorResponse>(&bytes) {
            let type_name = err
                .type_name
                .as_deref()
                .and_then(|s| s.split('#').next_back())
                .unwrap_or("HTTPError");
            let msg = err.message.unwrap_or_else(|| format!("HTTP {}", status));
            anyhow::bail!("{}: {}", type_name, msg);
        }

        let raw = String::from_utf8_lossy(&bytes);
        anyhow::bail!("HTTP {}: {}", status, raw);
    }

    let out = serde_cbor::from_slice::<TResp>(&bytes).context("CBOR 解码失败")?;
    Ok(out)
}

/// 读取当前用户的用量与限额（含 overage 配置）。只读操作，无需 CSRF。
pub async fn get_user_usage_and_limits(
    access_token: &str,
    idp: &str,
    proxy: Option<&ProxyConfig>,
    tls_backend: TlsBackend,
) -> anyhow::Result<UsageAndLimitsResponse> {
    request_cbor(
        "GetUserUsageAndLimits",
        &GetUserUsageAndLimitsRequest {
            is_email_required: true,
            origin: "KIRO_IDE".to_string(),
        },
        access_token,
        idp,
        proxy,
        tls_backend,
        None,
    )
    .await
}

// ============================================================================
// UpdateBillingPreferences —— 用于开启/关闭超额（overage）
// ============================================================================

/// UpdateBillingPreferences 请求体里的超额配置子结构
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct UpdateOverageConfiguration {
    pub overage_enabled: bool,
}

/// UpdateBillingPreferences 请求体
///
/// CBOR 体形如 `{ overageConfiguration: { overageEnabled: true }, profileArn: "arn:..." }`
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct UpdateBillingPreferencesRequest {
    pub overage_configuration: UpdateOverageConfiguration,
    pub profile_arn: String,
}

/// UpdateBillingPreferences 响应（上游返回空对象，用 IgnoredAny 接收）
#[derive(Debug, serde::Deserialize)]
struct UpdateBillingPreferencesResponse {
    #[serde(flatten, default)]
    _ignored: serde::de::IgnoredAny,
}

/// 调用 UpdateBillingPreferences 开启或关闭超额。
///
/// 必须传入 profileArn，否则上游会返回 400/ValidationException。
///
/// 写操作流程：
/// 1. 先 GET `https://app.kiro.dev/` 拿 CSRF token + UserId（[`fetch_csrf_session`]）；
/// 2. 然后 POST 操作时附加 `X-CSRF-Token` header 与 `UserId` cookie，
///    上游缺一不可（错误信息分别是 "CSRF token is required" / "Invalid CSRF token"）。
///
/// ⚠️ 计费红线：开启（overage_enabled=true）意味着超出 base 额度后按量真实付费。
/// 本函数只在被显式调用时才提交，不做任何自动/批量触发。
pub async fn update_billing_preferences(
    access_token: &str,
    idp: &str,
    profile_arn: &str,
    overage_enabled: bool,
    proxy: Option<&ProxyConfig>,
    tls_backend: TlsBackend,
) -> anyhow::Result<()> {
    if profile_arn.trim().is_empty() {
        anyhow::bail!("UpdateBillingPreferences 需要 profileArn，但凭据未提供");
    }
    let csrf = fetch_csrf_session(access_token, idp, proxy, tls_backend).await?;
    let _resp: UpdateBillingPreferencesResponse = request_cbor(
        "UpdateBillingPreferences",
        &UpdateBillingPreferencesRequest {
            overage_configuration: UpdateOverageConfiguration { overage_enabled },
            profile_arn: profile_arn.to_string(),
        },
        access_token,
        idp,
        proxy,
        tls_backend,
        Some(&csrf),
    )
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_meta_basic() {
        let html = r#"<meta name="csrf-token" content="abc123"><meta name="user-id" content="u-42">"#;
        assert_eq!(extract_meta(html, "csrf-token"), Some("abc123".to_string()));
        assert_eq!(extract_meta(html, "user-id"), Some("u-42".to_string()));
    }

    #[test]
    fn test_extract_meta_missing() {
        let html = r#"<meta name="other" content="x">"#;
        assert_eq!(extract_meta(html, "csrf-token"), None);
    }

    #[test]
    fn test_overage_enabled_accessor() {
        let resp = UsageAndLimitsResponse {
            user_info: None,
            subscription_info: None,
            next_date_reset: None,
            overage_configuration: Some(OverageConfiguration {
                overage_enabled: Some(true),
            }),
        };
        assert_eq!(resp.overage_enabled(), Some(true));

        let resp_none = UsageAndLimitsResponse {
            user_info: None,
            subscription_info: None,
            next_date_reset: None,
            overage_configuration: None,
        };
        assert_eq!(resp_none.overage_enabled(), None);
    }

    #[test]
    fn test_update_billing_preferences_rejects_empty_profile_arn() {
        // 空 profileArn 必须在发请求前就失败（幂等安全边界）
        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(update_billing_preferences(
            "token",
            "Google",
            "   ",
            true,
            None,
            TlsBackend::Rustls,
        ));
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("profileArn")
        );
    }
}


