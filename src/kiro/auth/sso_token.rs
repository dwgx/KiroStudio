//! SSO Token 导入 —— AWS portal 已登录用户的 Bearer Token 静默换号
//!
//! 场景：用户已在 AWS portal（`portal.sso.us-east-1.amazonaws.com`）登录，把
//! portal 会话的 Bearer Token（`x-amz-sso_authn`）粘贴给本工具。服务端走纯
//! HTTP 流程模拟「用户已在浏览器完成设备授权」，直接换取**标准 IdC 凭据**
//! （accessToken + refreshToken + clientId + clientSecret）入池 —— 免去
//! IdC 上号里「浏览器开授权页 + 输验证码」的人工步骤。
//!
//! 流程（移植自 Kiro-Go `auth/sso_token.go` 的 `ImportFromSsoToken`，7 步）：
//! 1. register client（带 `grantTypes` 含 refresh_token + `issuerUrl`）
//! 2. start device authorization（`startUrl=https://view.awsapps.com/start`）
//! 3. verify bearer token（portal `/token/whoAmI`）—— token 有效性验证
//! 4. get device session token（portal `/session/device`）
//! 5. accept user code（oidc `/device_authorization/accept_user_code`）
//! 6. approve auth（oidc `/device_authorization/associate_token`）
//! 7. poll token（oidc `/token`，device_code grant）→ accessToken/refreshToken
//!
//! # 生命周期评估（本模块的边界决策）
//!
//! 导入产物是**标准 IdC 凭据**（`auth_method=idc`）：refreshToken 存在、
//! clientId/clientSecret 全套，刷新走既有 [`crate::kiro::token_manager`] 的
//! `refresh_idc_token` 路径（`oidc.{region}.amazonaws.com/token`）——**不是**
//! 「只导入不刷新」。用户粘贴的 portal Bearer Token 是**单次用途**：只在本流程
//! 里用于 whoAmI 验证 + 换取 device session，**绝不落盘、不写入凭据**。
//! 过期续期走标准 IdC 刷新；刷新永久失败（invalid_grant）则重新粘贴新 portal
//! token 导入（现有重新上号语义）。
//!
//! # 安全
//!
//! - token 不进日志：本模块所有日志/错误消息都不含 token 原文（只打 email/region）。
//! - region 直接拼进出站 host（`oidc.{region}.amazonaws.com`）：调用方必须先过
//!   [`KiroCredentials::is_supported_region`] 白名单（service 层拦截），否则污染
//!   值可把请求引到攻击者控制的 host、明文携带 device session / clientSecret。
//! - portal 端点固定 `us-east-1`（AWS portal 单一实例），不可配置。

use std::time::Duration;

use anyhow::{Context, anyhow, bail};

use crate::http_client::{ProxyConfig, build_client};
use crate::kiro::auth::idc::{OidcClient, build_user_agent};
use crate::kiro::model::credentials::KiroCredentials;
use crate::model::config::Config;

/// AWS portal 固定实例（Bearer Token 属于它，不可配置）。
const PORTAL_BASE: &str = "https://portal.sso.us-east-1.amazonaws.com";
/// SSO 起始 URL：Kiro-Go 与 IdC 上号都用 view.awsapps.com/start。
const SSO_START_URL: &str = "https://view.awsapps.com/start";
/// 轮询 CreateToken 的总超时（秒），对齐 Kiro-Go 的 2 分钟。
const POLL_TIMEOUT_SECS: u64 = 120;
/// 粘贴 token 的长度上限（防整页误粘；SSO token 实际远小于此）。
const SSO_TOKEN_MAX_LEN: usize = 4096;

/// SSO Token 导入流程的最终产物（标准 IdC 凭据的全部身份字段 + 展示 email）。
pub struct SsoTokenExchangeResult {
    pub access_token: String,
    pub refresh_token: String,
    pub client_id: String,
    pub client_secret: String,
    pub expires_in: i64,
    /// 解析到的账号 email（best-effort：获取失败为 None，不阻断导入）。
    pub email: Option<String>,
}

/// 粘贴的 SSO Token 清洗（纯函数，可单测）。
///
/// 规则（fail-closed）：
/// - trim 首尾空白（用户从浏览器/控制台复制常带换行）。
/// - 空 / 纯空白 → 拒绝。
/// - 含换行 → 拒绝：SSO token 是单行值，含换行说明粘了多个 token 或多余内容，
///   静默只取第一行会在用户不知情时导入**错误的账号**。
/// - 超过长度上限 → 拒绝（疑似粘了整页内容，截断会让 whoAmI 返回无法归因的 401）。
fn sanitize_sso_token(token: &str) -> anyhow::Result<String> {
    if token.contains('\n') {
        bail!("SSO Token 含换行——疑似粘贴了多个 Token 或多余内容，请只粘贴一个 Token");
    }
    let t = token.trim();
    if t.is_empty() {
        bail!("SSO Token 不能为空");
    }
    if t.len() > SSO_TOKEN_MAX_LEN {
        bail!("SSO Token 过长（疑似粘贴了整页内容），请只粘贴 Token 本身");
    }
    Ok(t.to_string())
}

/// Step 1：注册 OIDC 客户端（SSO 导入专用变体）。
///
/// 与 [`crate::kiro::auth::idc::register_client`] 的差异：**带 `grantTypes`
/// （含 refresh_token）与 `issuerUrl`**。Kiro-Go 的 `ImportFromSsoToken` 如此注册，
/// 保证后续 poll 拿到的 refreshToken 被上游认可（idc 上号的注册没有这两个字段，
/// 那是为纯 device flow 设计的，这里不复用以防上游按注册形态校验 grant）。
async fn register_oidc_client(
    region: &str,
    config: &Config,
    proxy: Option<&ProxyConfig>,
) -> anyhow::Result<OidcClient> {
    let url = format!("https://oidc.{}.amazonaws.com/client/register", region);
    let client = build_client(proxy, 30, config.tls_backend)?;
    let (x_amz, ua) = build_user_agent(config);

    let body = serde_json::json!({
        "clientName": "Kiro API Proxy",
        "clientType": "public",
        "scopes": [
            "codewhisperer:completions",
            "codewhisperer:analysis",
            "codewhisperer:conversations",
            "codewhisperer:transformations",
            "codewhisperer:taskassist",
        ],
        "grantTypes": ["urn:ietf:params:oauth:grant-type:device_code", "refresh_token"],
        "issuerUrl": SSO_START_URL,
    });

    let resp = client
        .post(&url)
        .header("Content-Type", "application/json")
        .header("x-amz-user-agent", &x_amz)
        .header("user-agent", &ua)
        .header("host", format!("oidc.{}.amazonaws.com", region))
        .header("amz-sdk-invocation-id", uuid::Uuid::new_v4().to_string())
        .header("amz-sdk-request", "attempt=1; max=3")
        .json(&body)
        .send()
        .await
        .context("SSO OIDC client 注册请求失败")?;

    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        bail!("SSO OIDC client 注册失败 {}: {}", status, text);
    }

    let data: serde_json::Value = resp.json().await?;
    let client_id = data["clientId"]
        .as_str()
        .ok_or_else(|| anyhow!("SSO OIDC 注册响应缺少 clientId"))?
        .to_string();
    let client_secret = data["clientSecret"]
        .as_str()
        .ok_or_else(|| anyhow!("SSO OIDC 注册响应缺少 clientSecret"))?
        .to_string();

    Ok(OidcClient {
        client_id,
        client_secret,
    })
}

/// Step 3：验证粘贴的 Bearer Token（`GET /token/whoAmI`）。
///
/// whoAmI 返回 200 = token 有效；4xx/网络错误 = 无效或过期，直接拒绝。
async fn verify_bearer_token(
    bearer_token: &str,
    config: &Config,
    proxy: Option<&ProxyConfig>,
) -> anyhow::Result<()> {
    let client = build_client(proxy, 30, config.tls_backend)?;
    let resp = client
        .get(format!("{}/token/whoAmI", PORTAL_BASE))
        .header("Authorization", format!("Bearer {}", bearer_token))
        .header("Accept", "application/json")
        .send()
        .await
        .context("SSO Token 验证请求失败")?;

    let status = resp.status();
    if status.is_success() {
        return Ok(());
    }
    // 错误体不回显给用户（可能含 token 关联信息）；只报状态码语义。
    let msg = match status.as_u16() {
        401 => "SSO Token 无效或已过期（whoAmI 401），请重新从 portal 复制",
        403 => "SSO Token 无权限访问（whoAmI 403）",
        _ => return Err(anyhow!("SSO Token 验证失败（whoAmI {}）", status)),
    };
    bail!("{}", msg)
}

/// Step 4：用 Bearer Token 换取 device session token（`POST /session/device`）。
async fn get_device_session_token(
    bearer_token: &str,
    config: &Config,
    proxy: Option<&ProxyConfig>,
) -> anyhow::Result<String> {
    let client = build_client(proxy, 30, config.tls_backend)?;
    let resp = client
        .post(format!("{}/session/device", PORTAL_BASE))
        .header("Authorization", format!("Bearer {}", bearer_token))
        .header("Content-Type", "application/json")
        .body("{}")
        .send()
        .await
        .context("SSO 设备会话请求失败")?;

    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!("获取 SSO 设备会话失败（{}）", status);
    }
    let data: serde_json::Value = serde_json::from_str(&text)
        .with_context(|| format!("SSO 设备会话响应解析失败（{}）", status))?;
    data["token"]
        .as_str()
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow!("SSO 设备会话响应缺少 token"))
}

/// Step 5：接受用户代码（`POST /device_authorization/accept_user_code`）。
///
/// 返回 deviceContext（有值时 Step 6 需要 approve；上游也可能直接放行返回空）。
async fn accept_user_code(
    region: &str,
    user_code: &str,
    device_session_token: &str,
    config: &Config,
    proxy: Option<&ProxyConfig>,
) -> anyhow::Result<Option<DeviceContext>> {
    let client = build_client(proxy, 30, config.tls_backend)?;
    let (x_amz, ua) = build_user_agent(config);

    let body = serde_json::json!({
        "userCode": user_code,
        "userSessionId": device_session_token,
    });

    let resp = client
        .post(format!(
            "https://oidc.{}.amazonaws.com/device_authorization/accept_user_code",
            region
        ))
        .header("Content-Type", "application/json")
        .header("Referer", "https://view.awsapps.com/")
        .header("x-amz-user-agent", &x_amz)
        .header("user-agent", &ua)
        .header("host", format!("oidc.{}.amazonaws.com", region))
        .header("amz-sdk-invocation-id", uuid::Uuid::new_v4().to_string())
        .header("amz-sdk-request", "attempt=1; max=3")
        .json(&body)
        .send()
        .await
        .context("接受用户代码请求失败")?;

    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!("接受用户代码失败（{}）", status);
    }
    let data: serde_json::Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(_) => return Ok(None),
    };
    let ctx = data.get("deviceContext").and_then(|c| {
        Some(DeviceContext {
            device_context_id: c.get("deviceContextId")?.as_str()?.to_string(),
            client_id: c.get("clientId")?.as_str()?.to_string(),
            client_type: c.get("clientType")?.as_str()?.to_string(),
        })
    });
    Ok(ctx)
}

/// device_authorization 的 accept_user_code 响应里的 device context（Step 6 输入）。
struct DeviceContext {
    device_context_id: String,
    client_id: String,
    client_type: String,
}

/// Step 6：批准授权（`POST /device_authorization/associate_token`）。
async fn approve_auth(
    region: &str,
    device_context: &DeviceContext,
    device_session_token: &str,
    config: &Config,
    proxy: Option<&ProxyConfig>,
) -> anyhow::Result<()> {
    let client = build_client(proxy, 30, config.tls_backend)?;
    let (x_amz, ua) = build_user_agent(config);

    let body = serde_json::json!({
        "deviceContext": {
            "deviceContextId": device_context.device_context_id,
            "clientId": device_context.client_id,
            "clientType": device_context.client_type,
        },
        "userSessionId": device_session_token,
    });

    let resp = client
        .post(format!(
            "https://oidc.{}.amazonaws.com/device_authorization/associate_token",
            region
        ))
        .header("Content-Type", "application/json")
        .header("Referer", "https://view.awsapps.com/")
        .header("x-amz-user-agent", &x_amz)
        .header("user-agent", &ua)
        .header("host", format!("oidc.{}.amazonaws.com", region))
        .header("amz-sdk-invocation-id", uuid::Uuid::new_v4().to_string())
        .header("amz-sdk-request", "attempt=1; max=3")
        .json(&body)
        .send()
        .await
        .context("批准授权请求失败")?;

    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        bail!("批准授权失败（{}）: {}", status, text);
    }
    Ok(())
}

/// Step 7：轮询 CreateToken（`POST /token`，device_code grant）。
///
/// 与 IdC 上号不同：授权已被服务端自动批准（Step 5/6），用户无感，正常首轮即
/// 200；循环只是兜底 AWS 侧延迟（authorization_pending / slow_down 语义照常）。
/// 总超时 [`POLL_TIMEOUT_SECS`]（对齐 Kiro-Go 的 2 分钟）。
async fn poll_for_token(
    region: &str,
    oidc_client: &OidcClient,
    device_code: &str,
    interval_secs: u64,
    config: &Config,
    proxy: Option<&ProxyConfig>,
) -> anyhow::Result<(String, String, i64)> {
    let client = build_client(proxy, 30, config.tls_backend)?;
    let (x_amz, ua) = build_user_agent(config);

    let body = serde_json::json!({
        "clientId": oidc_client.client_id,
        "clientSecret": oidc_client.client_secret,
        "grantType": "urn:ietf:params:oauth:grant-type:device_code",
        "deviceCode": device_code,
    });

    let interval = interval_secs.max(1);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(POLL_TIMEOUT_SECS);
    loop {
        if tokio::time::Instant::now() >= deadline {
            bail!("SSO Token 导入授权超时（{}s）", POLL_TIMEOUT_SECS);
        }

        let resp = client
            .post(format!("https://oidc.{}.amazonaws.com/token", region))
            .header("Content-Type", "application/json")
            .header("x-amz-user-agent", &x_amz)
            .header("user-agent", &ua)
            .header("host", format!("oidc.{}.amazonaws.com", region))
            .header("amz-sdk-invocation-id", uuid::Uuid::new_v4().to_string())
            .header("amz-sdk-request", "attempt=1; max=3")
            .json(&body)
            .send()
            .await;

        let (status, text) = match resp {
            Ok(r) => {
                let s = r.status();
                let t = r.text().await.unwrap_or_default();
                (s, t)
            }
            Err(e) => {
                // 网络瞬时错误：等一个间隔重试（与 Kiro-Go 的 continue 同语义）。
                tracing::debug!("SSO poll token 网络错误(重试): {}", e);
                tokio::time::sleep(Duration::from_secs(interval)).await;
                continue;
            }
        };

        if status.is_success() {
            let data: serde_json::Value = serde_json::from_str(&text)
                .context("SSO token 轮询响应解析失败")?;
            let access = data["accessToken"]
                .as_str()
                .ok_or_else(|| anyhow!("SSO token 轮询响应缺少 accessToken"))?
                .to_string();
            let refresh = data["refreshToken"].as_str().unwrap_or("").to_string();
            let expires_in = data["expiresIn"].as_i64().unwrap_or(0);
            return Ok((access, refresh, expires_in));
        }

        if status.as_u16() == 400 {
            if text.contains("authorization_pending") || text.contains("AuthorizationPendingException")
            {
                tokio::time::sleep(Duration::from_secs(interval)).await;
                continue;
            }
            if text.contains("slow_down") || text.contains("SlowDownException") {
                tokio::time::sleep(Duration::from_secs(interval + 5)).await;
                continue;
            }
            bail!("SSO 授权错误: {}", text);
        }
        bail!("SSO token 轮询失败（{}）", status);
    }
}

/// 解析账号 email（best-effort，对齐 Kiro-Go 的 GetUserInfo）。
///
/// 用换到的 access_token 打一次 `getUsageLimits?isEmailRequired=true`，从
/// 响应 `userInfo.email` 取邮箱。请求与业务路径**同构**（复用
/// `build_usage_limits_request` 的 UA / host / profileArn 规则），失败不阻断导入
/// ——email 只是展示与幂等判重用，换号本身不依赖它。
async fn fetch_account_email(
    access_token: &str,
    region: &str,
    config: &Config,
    proxy: Option<&ProxyConfig>,
) -> Option<String> {
    let base = KiroCredentials {
        access_token: Some(access_token.to_string()),
        auth_method: Some("idc".to_string()),
        region: Some(region.to_string()),
        auth_region: Some(region.to_string()),
        ..Default::default()
    };
    let client = match build_client(proxy, 30, config.tls_backend) {
        Ok(c) => c,
        Err(_) => return None,
    };
    let req = crate::kiro::token_manager::build_usage_limits_request(
        &client, &base, config, access_token, region,
    );
    match req.send().await {
        Ok(resp) if resp.status().is_success() => {
            let v: serde_json::Value = match resp.json().await {
                Ok(v) => v,
                Err(_) => return None,
            };
            v["userInfo"]["email"]
                .as_str()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        }
        _ => None,
    }
}

/// 完整的 SSO Token 导入流程（7 步）。
///
/// `region` 必须已过 [`KiroCredentials::is_supported_region`] 白名单（调用方负责，
/// 它直接拼进 `oidc.{region}.amazonaws.com` 出站 host）。
pub async fn exchange_sso_token(
    bearer_token: &str,
    region: &str,
    config: &Config,
    proxy: Option<&ProxyConfig>,
) -> anyhow::Result<SsoTokenExchangeResult> {
    let token = sanitize_sso_token(bearer_token)?;

    // Step 1-2：注册客户端 + 发起设备授权（startUrl 固定 view.awsapps.com/start）。
    let oidc_client = register_oidc_client(region, config, proxy).await?;
    let device_auth = crate::kiro::auth::idc::start_device_authorization(
        region, &oidc_client, SSO_START_URL, config, proxy,
    )
    .await?;

    // Step 3-4：验证粘贴 token + 换 device session。
    verify_bearer_token(&token, config, proxy).await?;
    let device_session_token = get_device_session_token(&token, config, proxy).await?;

    // Step 5-6：接受用户代码 + 批准授权（模拟浏览器已完成授权）。
    if let Some(ctx) = accept_user_code(
        region,
        &device_auth.user_code,
        &device_session_token,
        config,
        proxy,
    )
    .await?
    {
        approve_auth(region, &ctx, &device_session_token, config, proxy).await?;
    }

    // Step 7：轮询换取正式 IdC token。
    let (access_token, refresh_token, expires_in) = poll_for_token(
        region,
        &oidc_client,
        &device_auth.device_code,
        device_auth.interval,
        config,
        proxy,
    )
    .await?;

    // best-effort 解析 email（展示 + 幂等判重用，失败不阻断）。
    let email = fetch_account_email(&access_token, region, config, proxy).await;

    tracing::info!(
        "SSO Token 导入换号成功（region={}）",
        region
    );
    Ok(SsoTokenExchangeResult {
        access_token,
        refresh_token,
        client_id: oidc_client.client_id,
        client_secret: oidc_client.client_secret,
        expires_in,
        email,
    })
}

/// 池中是否已存在「同一邮箱的 idc 号」（SSO 导入幂等判重）。
///
/// 语义：SSO Token 导入是「把一个 portal 已登录账号搬进池」，同一邮箱再次导入
/// 是重复操作（误双击/重复粘贴）。与 api_key/refreshToken 的哈希判重互补：
/// SSO 每次导入都重新授权、换出**不同的** refreshToken，哈希判重抓不住，
/// email 是账号级稳定指纹。
///
/// `pool` 为 `(auth_method, email)` 对（调用方从 snapshot 取；无 email 的条目
/// 传入 `None`）。email 判重大小写不敏感。email 为空时不做判重（解析失败时
/// 只能靠 refreshToken 哈希兜底，与存量行为一致）。
pub(crate) fn find_duplicate_idc_email(
    pool: &[(Option<String>, Option<String>)],
    email: &str,
) -> bool {
    let email = email.trim().to_lowercase();
    if email.is_empty() {
        return false;
    }
    pool.iter().any(|(auth_method, other_email)| {
        let is_idc = auth_method
            .as_ref()
            .map(|m| {
                m.eq_ignore_ascii_case("idc")
                    || m.eq_ignore_ascii_case("builder-id")
                    || m.eq_ignore_ascii_case("iam")
            })
            .unwrap_or(false);
        is_idc
            && other_email
                .as_ref()
                .map(|e| e.trim().to_lowercase())
                .as_deref()
                == Some(email.as_str())
    })
}

/// 用换号结果构造标准 IdC 凭据（对齐 idc_login 上号的字段形态）。
///
/// `custom_proxy` 为导入时**显式填的**代理（仅此项持久化到新凭据；global 回落不持久化，
/// 与 external_idp/idc 上号同口径）。
pub(crate) fn build_idc_credential_from_sso(
    exchange: &SsoTokenExchangeResult,
    region: &str,
    priority: u32,
    custom_proxy: Option<&ProxyConfig>,
) -> KiroCredentials {
    let expires_at = (exchange.expires_in > 0).then(|| {
        chrono::Utc::now()
            .checked_add_signed(chrono::Duration::seconds(exchange.expires_in))
            .unwrap_or_else(chrono::Utc::now)
            .to_rfc3339()
    });

    KiroCredentials {
        access_token: Some(exchange.access_token.clone()),
        refresh_token: (!exchange.refresh_token.is_empty()).then(|| exchange.refresh_token.clone()),
        profile_arn: None,
        expires_at,
        auth_method: Some("idc".to_string()),
        client_id: Some(exchange.client_id.clone()),
        client_secret: Some(exchange.client_secret.clone()),
        token_endpoint: None,
        issuer_url: None,
        scopes: None,
        priority,
        region: Some(region.to_string()),
        auth_region: Some(region.to_string()),
        api_region: None,
        machine_id: None,
        email: exchange.email.clone(),
        name: None,
        clone_group: None,
        clone_seq: None,
        tag: None,
        subscription_title: None,
        proxy_url: custom_proxy.map(|p| p.url.clone()),
        proxy_username: custom_proxy.and_then(|p| p.username.clone()),
        proxy_password: custom_proxy.and_then(|p| p.password.clone()),
        disabled: false,
        disabled_reason: None,
        disabled_at: None,
        quota_exhausted_at: None,
        kiro_api_key: None,
        endpoint: None,
        cli_origin_kiro_cli: None,
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_accepts_single_line_token() {
        let t = sanitize_sso_token("  eyJhbGciOiJSUzI1NiIsInR5cCI6IkpXVCJ9.abc  ").unwrap();
        assert_eq!(t, "eyJhbGciOiJSUzI1NiIsInR5cCI6IkpXVCJ9.abc");
    }

    #[test]
    fn sanitize_rejects_empty_or_blank() {
        assert!(sanitize_sso_token("").is_err());
        assert!(sanitize_sso_token("   ").is_err());
    }

    #[test]
    fn sanitize_rejects_multiline_token() {
        // 粘贴多个 token / 带说明文字：必须整体拒绝，绝不静默只取第一行
        // （否则会导入用户没看到的那个账号）。
        assert!(sanitize_sso_token("tok1\ntok2").is_err());
        assert!(sanitize_sso_token("tok1\n").is_err());
        assert!(sanitize_sso_token("description\n tok1").is_err());
    }

    #[test]
    fn sanitize_rejects_oversized_token() {
        let long = "x".repeat(SSO_TOKEN_MAX_LEN + 1);
        assert!(sanitize_sso_token(&long).is_err());
        let ok = "x".repeat(SSO_TOKEN_MAX_LEN);
        assert!(sanitize_sso_token(&ok).is_ok());
    }

    #[test]
    fn duplicate_detection_matches_same_email_idc_cred() {
        let pool = vec![
            (Some("idc".to_string()), Some("a@example.com".to_string())),
            (Some("social".to_string()), Some("a@example.com".to_string())),
        ];
        // 同 email 的 idc 号 → 判重复。
        assert!(find_duplicate_idc_email(&pool, "A@Example.com"));
        // social 号（非 idc）不参与判重：SSO 导入产物是 idc 形态，只与 idc 号比。
        let social_only = vec![(Some("social".to_string()), Some("a@example.com".to_string()))];
        assert!(!find_duplicate_idc_email(&social_only, "a@example.com"));
    }

    #[test]
    fn duplicate_detection_ignores_other_accounts_and_empty() {
        let pool = vec![(Some("idc".to_string()), Some("b@example.com".to_string()))];
        assert!(!find_duplicate_idc_email(&pool, "a@example.com"));
        // email 解析失败（空）→ 不做判重（靠 refreshToken 哈希兜底）。
        assert!(!find_duplicate_idc_email(&pool, "  "));
        // 无 email 的存量条目（旧号 email=None）不参与判重。
        let no_email = vec![(Some("idc".to_string()), None)];
        assert!(!find_duplicate_idc_email(&no_email, "a@example.com"));
    }

    #[test]
    fn built_credential_is_standard_idc_shape() {
        let exchange = SsoTokenExchangeResult {
            access_token: "at".to_string(),
            refresh_token: "rt".to_string(),
            client_id: "cid".to_string(),
            client_secret: "cs".to_string(),
            expires_in: 3600,
            email: Some("u@example.com".to_string()),
        };
        let cred = build_idc_credential_from_sso(&exchange, "eu-central-1", 3, None);

        assert_eq!(cred.auth_method.as_deref(), Some("idc"));
        assert_eq!(cred.region.as_deref(), Some("eu-central-1"));
        assert_eq!(cred.auth_region.as_deref(), Some("eu-central-1"));
        assert_eq!(cred.client_id.as_deref(), Some("cid"));
        assert_eq!(cred.client_secret.as_deref(), Some("cs"));
        assert_eq!(cred.refresh_token.as_deref(), Some("rt"));
        assert_eq!(cred.email.as_deref(), Some("u@example.com"));
        assert_eq!(cred.priority, 3);
        assert!(cred.expires_at.is_some());
        // 标准 idc 号：profileArn 由刷新后动态解析，导入时不预填。
        assert!(cred.profile_arn.is_none());
        // 显式代理持久化。
        let p = ProxyConfig::new("socks5://127.0.0.1:1080");
        let cred2 = build_idc_credential_from_sso(&exchange, "us-east-1", 0, Some(&p));
        assert_eq!(cred2.proxy_url.as_deref(), Some("socks5://127.0.0.1:1080"));
    }

    #[test]
    fn built_credential_without_refresh_token_or_expiry() {
        // 上游未返回 refreshToken（异常形态）→ 字段为 None 而非空串；expires_in=0 → 无过期时间。
        let exchange = SsoTokenExchangeResult {
            access_token: "at".to_string(),
            refresh_token: String::new(),
            client_id: "cid".to_string(),
            client_secret: "cs".to_string(),
            expires_in: 0,
            email: None,
        };
        let cred = build_idc_credential_from_sso(&exchange, "us-east-1", 0, None);
        assert!(cred.refresh_token.is_none());
        assert!(cred.expires_at.is_none());
    }
}
