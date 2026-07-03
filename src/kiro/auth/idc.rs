//! IDC (AWS IAM Identity Center) Device Authorization Grant 登录流程
//!
//! 对接 AWS SSO-OIDC 的 device code flow：
//! 1. Register Client → clientId + clientSecret
//! 2. Start Device Authorization → deviceCode + userCode + verificationUri
//! 3. 用户在浏览器打开 verificationUri 完成授权
//! 4. Poll CreateToken → accessToken + refreshToken
//!
//! 参考 Kiro IDE 的 aws-sdk-js SSO-OIDC 交互协议。

use crate::http_client::{ProxyConfig, build_client};
use crate::model::config::Config;

/// Kiro IDE 使用的 OIDC scopes
const OIDC_SCOPES: &[&str] = &[
    "codewhisperer:completions",
    "codewhisperer:analysis",
    "codewhisperer:conversations",
    "codewhisperer:transformations",
    "codewhisperer:taskassist",
];

/// OIDC 客户端注册结果
#[derive(Debug, Clone)]
pub struct OidcClient {
    pub client_id: String,
    pub client_secret: String,
}

/// Device Authorization 结果
#[derive(Debug, Clone)]
pub struct DeviceAuth {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub verification_uri_complete: Option<String>,
    pub expires_in: u64,
    pub interval: u64,
}

/// CreateToken 结果
#[derive(Debug, Clone)]
pub struct IdcTokenResult {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_in: u64,
}

/// CreateToken 轮询状态
pub enum PollTokenResult {
    /// 用户尚未完成授权，继续轮询
    Pending,
    /// 授权完成
    Done(IdcTokenResult),
    /// 已过期（用户未在规定时间内完成）
    Expired,
    /// 其它错误
    Error(String),
}

fn oidc_base_url(region: &str) -> String {
    format!("https://oidc.{}.amazonaws.com", region)
}

fn build_user_agent(config: &Config) -> (String, String) {
    let os_name = &config.system_version;
    let node_version = &config.node_version;
    let x_amz = "aws-sdk-js/3.980.0 KiroIDE".to_string();
    let ua = format!(
        "aws-sdk-js/3.980.0 ua/2.1 os/{} lang/js md/nodejs#{} api/sso-oidc#3.980.0 m/E KiroIDE",
        os_name, node_version
    );
    (x_amz, ua)
}

/// Step 1: 向 AWS SSO-OIDC 注册客户端（public client）
pub async fn register_client(
    region: &str,
    config: &Config,
    proxy: Option<&ProxyConfig>,
) -> anyhow::Result<OidcClient> {
    let url = format!("{}/client/register", oidc_base_url(region));
    let client = build_client(proxy, 30, config.tls_backend)?;
    let (x_amz, ua) = build_user_agent(config);

    let body = serde_json::json!({
        "clientName": "Amazon Q Developer for command line",
        "clientType": "public",
        "scopes": OIDC_SCOPES,
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
        .await?;

    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        anyhow::bail!("OIDC client 注册失败 {}: {}", status, text);
    }

    let data: serde_json::Value = resp.json().await?;
    let client_id = data["clientId"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("响应缺少 clientId"))?
        .to_string();
    let client_secret = data["clientSecret"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("响应缺少 clientSecret"))?
        .to_string();

    Ok(OidcClient {
        client_id,
        client_secret,
    })
}

/// Step 2: 发起 device authorization
pub async fn start_device_authorization(
    region: &str,
    oidc_client: &OidcClient,
    start_url: &str,
    config: &Config,
    proxy: Option<&ProxyConfig>,
) -> anyhow::Result<DeviceAuth> {
    let url = format!("{}/device_authorization", oidc_base_url(region));
    let client = build_client(proxy, 30, config.tls_backend)?;
    let (x_amz, ua) = build_user_agent(config);

    let body = serde_json::json!({
        "clientId": oidc_client.client_id,
        "clientSecret": oidc_client.client_secret,
        "startUrl": start_url,
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
        .await?;

    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        anyhow::bail!("Device authorization 失败 {}: {}", status, text);
    }

    let data: serde_json::Value = resp.json().await?;
    Ok(DeviceAuth {
        device_code: data["deviceCode"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
        user_code: data["userCode"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
        verification_uri: data["verificationUri"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
        verification_uri_complete: data["verificationUriComplete"]
            .as_str()
            .map(|s| s.to_string()),
        expires_in: data["expiresIn"].as_u64().unwrap_or(600),
        interval: data["interval"].as_u64().unwrap_or(5),
    })
}

/// Step 4: 轮询 CreateToken（一次调用，由上层循环驱动）
pub async fn poll_create_token(
    region: &str,
    oidc_client: &OidcClient,
    device_code: &str,
    config: &Config,
    proxy: Option<&ProxyConfig>,
) -> PollTokenResult {
    let url = format!("{}/token", oidc_base_url(region));
    let client = match build_client(proxy, 30, config.tls_backend) {
        Ok(c) => c,
        Err(e) => return PollTokenResult::Error(format!("构建 HTTP 客户端失败: {}", e)),
    };
    let (x_amz, ua) = build_user_agent(config);

    let body = serde_json::json!({
        "clientId": oidc_client.client_id,
        "clientSecret": oidc_client.client_secret,
        "deviceCode": device_code,
        "grantType": "urn:ietf:params:oauth:grant-type:device_code",
    });

    let resp = match client
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
    {
        Ok(r) => r,
        Err(e) => return PollTokenResult::Error(format!("网络错误: {}", e)),
    };

    let status = resp.status();
    let text = match resp.text().await {
        Ok(t) => t,
        Err(e) => return PollTokenResult::Error(format!("读取响应失败: {}", e)),
    };

    if status.is_success() {
        let data: serde_json::Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(e) => return PollTokenResult::Error(format!("解析 JSON 失败: {}", e)),
        };
        return PollTokenResult::Done(IdcTokenResult {
            access_token: data["accessToken"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
            refresh_token: data["refreshToken"].as_str().map(|s| s.to_string()),
            expires_in: data["expiresIn"].as_u64().unwrap_or(28800),
        });
    }

    // 400 系列：检查具体错误码
    if text.contains("authorization_pending") || text.contains("AuthorizationPendingException") {
        return PollTokenResult::Pending;
    }
    if text.contains("slow_down") || text.contains("SlowDownException") {
        return PollTokenResult::Pending;
    }
    if text.contains("expired_token") || text.contains("ExpiredTokenException") {
        return PollTokenResult::Expired;
    }
    if text.contains("access_denied") || text.contains("AccessDeniedException") {
        return PollTokenResult::Error("用户拒绝了授权".to_string());
    }

    PollTokenResult::Error(format!("CreateToken 失败 {}: {}", status, text))
}
