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

/// 自动探测 IdC 实例 region 的候选表。单一真相源见
/// [`crate::kiro::regions::OIDC_PROBE_REGIONS`]（此处 re-export，调用点不变）。
use crate::kiro::regions::OIDC_PROBE_REGIONS as IDC_OIDC_PROBE_REGIONS;

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

/// 构造探测顺序:用户填的 region 打头,再补候选表(去重)。抽成纯函数便于单测。
fn probe_region_order(user_region: &str) -> Vec<String> {
    let mut order: Vec<String> = Vec::with_capacity(IDC_OIDC_PROBE_REGIONS.len() + 1);
    let ur = user_region.trim();
    if !ur.is_empty() {
        order.push(ur.to_string());
    }
    for r in IDC_OIDC_PROBE_REGIONS {
        if !order.iter().any(|x| x == r) {
            order.push((*r).to_string());
        }
    }
    order
}

/// Step 1+2 合并·自动探测 region（防呆）：IdC start URL 不含 region,用户填错会 400。
/// 按「用户填的 region 打头 + 候选表」顺次试 register_client + start_device_authorization,
/// **第一个 device_authorization 成功的 region 即实例所在 region**,返回 `(region, client, device_auth)`。
///
/// - clientId/secret 是 per-region 的,故每个 region 各注册一次,返回命中 region 的那套。
/// - device_authorization 报 400/invalid_request = 该 region 无此实例 → 试下一个（这是探测主判据）。
/// - 其它错误（网络/5xx）记 last_err 继续试别的 region,不因单点故障中断整轮。
/// - 全部 region 都不成 → 返回可读中文错误 + 最后一个上游错误（不再裸抛 400 让用户懵）。
/// - 成本:走 AWS 公开 OIDC 端点,与 Kiro 号池无关、不烧号；用户填对时首个即命中零额外开销。
pub async fn register_and_authorize_probing(
    user_region: &str,
    start_url: &str,
    config: &Config,
    proxy: Option<&ProxyConfig>,
) -> anyhow::Result<(String, OidcClient, DeviceAuth)> {
    let order = probe_region_order(user_region);
    let mut last_err: Option<String> = None;
    for region in &order {
        let oidc_client = match register_client(region, config, proxy).await {
            Ok(c) => c,
            Err(e) => {
                tracing::debug!("IdC region={} register_client 失败(试下一个): {}", region, e);
                last_err = Some(format!("register_client@{}: {}", region, e));
                continue;
            }
        };
        match start_device_authorization(region, &oidc_client, start_url, config, proxy).await {
            Ok(device_auth) => {
                tracing::info!(
                    "IdC 探测到实例 region={}（用户填 {}）",
                    region,
                    if user_region.trim().is_empty() { "<空>" } else { user_region.trim() }
                );
                return Ok((region.clone(), oidc_client, device_auth));
            }
            Err(e) => {
                tracing::debug!(
                    "IdC region={} device_authorization 未命中(试下一个): {}",
                    region, e
                );
                last_err = Some(format!("device_authorization@{}: {}", region, e));
                continue;
            }
        }
    }
    anyhow::bail!(
        "IdC 登录失败:start URL 在所试的 {} 个 region 均无对应实例。请确认 start URL 正确,\
         或到 AWS IAM Identity Center 设置查看实例所在 region 后重填。（最后错误：{}）",
        order.len(),
        last_err.as_deref().unwrap_or("无")
    )
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_probe_region_order_user_first_dedup() {
        // 用户填的 region 打头,候选表补齐,去重(用户填的若在候选表里不重复出现)。
        let order = probe_region_order("eu-central-1");
        assert_eq!(order.first().map(|s| s.as_str()), Some("eu-central-1"));
        // eu-central-1 只出现一次(去重)
        assert_eq!(order.iter().filter(|r| *r == "eu-central-1").count(), 1);
        // 候选表其余 region 仍在
        assert!(order.iter().any(|r| r == "us-east-1"));
        assert_eq!(order.len(), IDC_OIDC_PROBE_REGIONS.len());
    }

    #[test]
    fn test_probe_region_order_novel_user_region_prepended() {
        // 用户填了候选表没有的 region → 打头,总数 = 候选表 + 1。
        // 用一个确定不在 OIDC_PROBE_REGIONS 里的 region(us-west-1 只在对话白名单,不在 OIDC 探测表)。
        let novel = "us-west-1";
        assert!(!IDC_OIDC_PROBE_REGIONS.contains(&novel), "测试前提:该 region 不在 OIDC 候选表");
        let order = probe_region_order(novel);
        assert_eq!(order.first().map(|s| s.as_str()), Some(novel));
        assert_eq!(order.len(), IDC_OIDC_PROBE_REGIONS.len() + 1);
    }

    #[test]
    fn test_probe_region_order_empty_user_region() {
        // 用户没填 region → 只用候选表,顺序不变。
        let order = probe_region_order("   ");
        assert_eq!(order.len(), IDC_OIDC_PROBE_REGIONS.len());
        assert_eq!(order.first().map(|s| s.as_str()), Some("us-east-1"));
    }
}
