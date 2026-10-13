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

/// 构造与 Kiro IDE 一致的 SSO-OIDC 请求 UA（idc 上号与 sso_token 导入共用）。
pub(crate) fn build_user_agent(config: &Config) -> (String, String) {
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

/// 解析 Device Authorization 的 200 响应体（纯函数，供确定性测试）。
///
/// 三个承重字段（`deviceCode` / `userCode` / `verificationUri`）缺失或空白 = 上游契约
/// 违约：宁可当场报错，也不把**空设备码**交给用户 —— 旧实现 `unwrap_or_default()` 会
/// 静默产出空码，前端显示空白、用户无从授权，直到 `expires_in` 到期才报「已过期」
/// （根因被吞掉，排障只能看到表象）。`expiresIn`/`interval` 保留保守默认值。
fn parse_device_auth(data: &serde_json::Value) -> anyhow::Result<DeviceAuth> {
    let non_empty = |name: &str| -> Option<String> {
        data.get(name)
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
    };
    let device_code = non_empty("deviceCode").ok_or_else(|| {
        anyhow::anyhow!("Device authorization 响应缺少 deviceCode（上游契约违约）")
    })?;
    let user_code = non_empty("userCode").ok_or_else(|| {
        anyhow::anyhow!("Device authorization 响应缺少 userCode（上游契约违约）")
    })?;
    let verification_uri = non_empty("verificationUri").ok_or_else(|| {
        anyhow::anyhow!("Device authorization 响应缺少 verificationUri（上游契约违约）")
    })?;
    Ok(DeviceAuth {
        device_code,
        user_code,
        verification_uri,
        verification_uri_complete: non_empty("verificationUriComplete"),
        expires_in: data.get("expiresIn").and_then(|v| v.as_u64()).unwrap_or(600),
        interval: data.get("interval").and_then(|v| v.as_u64()).unwrap_or(5),
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
    parse_device_auth(&data)
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
                tracing::debug!(
                    "IdC region={} register_client 失败(试下一个): {}",
                    region,
                    e
                );
                last_err = Some(format!("register_client@{}: {}", region, e));
                continue;
            }
        };
        match start_device_authorization(region, &oidc_client, start_url, config, proxy).await {
            Ok(device_auth) => {
                tracing::info!(
                    "IdC 探测到实例 region={}（用户填 {}）",
                    region,
                    if user_region.trim().is_empty() {
                        "<空>"
                    } else {
                        user_region.trim()
                    }
                );
                return Ok((region.clone(), oidc_client, device_auth));
            }
            Err(e) => {
                tracing::debug!(
                    "IdC region={} device_authorization 未命中(试下一个): {}",
                    region,
                    e
                );
                last_err = Some(format!("device_authorization@{}: {}", region, e));
                continue;
            }
        }
    }
    // 全 region 都不成 → 结构化诊断 REGION_MISMATCH（归因 UserInput + 引导查 IAM Identity Center），
    // 取代裸 bail 字符串,前端渲染成引导卡片。
    let diagnosis =
        crate::kiro::diagnosis::diagnose_device_auth_all_failed(order.len(), last_err.as_deref());
    tracing::warn!("IdC device flow 全 region 失败：{}", diagnosis.log_line());
    Err(crate::kiro::token_manager::DiagnosedError { diagnosis }.into())
}

/// 解析 CreateToken 的 200 响应体（纯函数，供确定性测试）。
///
/// `accessToken` 缺失/空白 = 上游契约违约：**绝不返回 `Done`** —— 调用方会拿它建号
/// 入池并持久化进 `credentials.json`，空 token 的号每次对话必然 401/403，只能人工删。
/// 旧实现 `unwrap_or_default()` 正好会产出这种号。`refreshToken` 空白同样归 None
/// （宁可不带，也不存一个假 refresh token）；`expiresIn` 缺失用 8h 保守默认。
fn parse_create_token_success(text: &str) -> PollTokenResult {
    let data: serde_json::Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(e) => return PollTokenResult::Error(format!("解析 JSON 失败: {}", e)),
    };
    let non_empty = |name: &str| -> Option<String> {
        data.get(name)
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
    };
    let Some(access_token) = non_empty("accessToken") else {
        return PollTokenResult::Error(
            "CreateToken 200 响应缺少 accessToken（上游契约违约，拒绝以空 token 建号）".to_string(),
        );
    };
    PollTokenResult::Done(IdcTokenResult {
        access_token,
        refresh_token: non_empty("refreshToken"),
        expires_in: data.get("expiresIn").and_then(|v| v.as_u64()).unwrap_or(28800),
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
        return parse_create_token_success(&text);
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
        assert!(
            !IDC_OIDC_PROBE_REGIONS.contains(&novel),
            "测试前提:该 region 不在 OIDC 候选表"
        );
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

    /// 设备授权响应：三个承重字段缺失/空白必须报错（旧实现 `unwrap_or_default()`
    /// 静默产出空设备码 —— 前端显示空白、轮询到超时才报「已过期」，根因被吞）。
    #[test]
    fn parse_device_auth_rejects_missing_core_fields() {
        for bad in [
            r#"{}"#,
            r#"{"deviceCode":"d"}"#,
            r#"{"deviceCode":"d","userCode":"u"}"#,
            r#"{"deviceCode":" ","userCode":"u","verificationUri":"https://x"}"#,
            r#"{"deviceCode":"d","userCode":"","verificationUri":"https://x"}"#,
            r#"{"deviceCode":"d","userCode":"u","verificationUri":"  "}"#,
        ] {
            let v: serde_json::Value = serde_json::from_str(bad).unwrap();
            assert!(
                parse_device_auth(&v).is_err(),
                "缺承重字段必须报错而不是产出空设备码: {bad}"
            );
        }
        // 正常形态：字段带 trim、可选字段缺省为 None、数值字段保留保守默认。
        let v: serde_json::Value = serde_json::from_str(
            r#"{"deviceCode":" dc-1 ","userCode":" UC-1 ","verificationUri":"https://x/y"}"#,
        )
        .unwrap();
        let d = parse_device_auth(&v).expect("完整响应必须解析成功");
        assert_eq!(d.device_code, "dc-1");
        assert_eq!(d.user_code, "UC-1");
        assert_eq!(d.verification_uri, "https://x/y");
        assert_eq!(d.verification_uri_complete, None);
        assert_eq!(d.expires_in, 600, "expiresIn 缺省 600s");
        assert_eq!(d.interval, 5, "interval 缺省 5s");
    }

    /// CreateToken 200：`accessToken` 缺失/空白必须报错（旧实现会返回 `Done`，
    /// 调用方拿空 token 建号入池并落盘 —— 那个号之后每次对话必然 401/403）。
    #[test]
    fn parse_create_token_success_rejects_missing_access_token() {
        for bad in [
            r#"{}"#,
            r#"{"accessToken":""}"#,
            r#"{"accessToken":"   "}"#,
            r#"{"accessToken":null}"#,
            r#"{"accessToken":123}"#,
        ] {
            assert!(
                matches!(parse_create_token_success(bad), PollTokenResult::Error(_)),
                "缺 accessToken 必须报错（不得 Done 让空号入池）: {bad}"
            );
        }
        // 正常形态：token 两侧空白被 trim；refreshToken 空白归 None；expiresIn 默认 28800。
        match parse_create_token_success(r#"{"accessToken":" at-1 ","refreshToken":"  "}"#) {
            PollTokenResult::Done(t) => {
                assert_eq!(t.access_token, "at-1");
                assert_eq!(t.refresh_token, None, "空白 refreshToken 不得存成假值");
                assert_eq!(t.expires_in, 28800);
            }
            _ => panic!("完整响应必须 Done"),
        }
        match parse_create_token_success(r#"{"accessToken":"at-2","refreshToken":"rt-2"}"#) {
            PollTokenResult::Done(t) => {
                assert_eq!(t.access_token, "at-2");
                assert_eq!(t.refresh_token.as_deref(), Some("rt-2"));
            }
            _ => panic!("完整响应必须 Done"),
        }
        // 非法 JSON：必须 Error 而非 panic。
        assert!(matches!(
            parse_create_token_success("not-json"),
            PollTokenResult::Error(_)
        ));
    }
}
