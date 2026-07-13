//! Kiro IDE 端点
//!
//! 对应 Kiro IDE 客户端目前使用的端点（已随 Kiro 迁移到 kiro.dev；旧的
//! `q.{region}.amazonaws.com` 已停用）：
//! - API: `https://runtime.{region}.kiro.dev/generateAssistantResponse`
//! - MCP: `https://runtime.{region}.kiro.dev/mcp`
//!
//! region 优先从凭据 `profileArn` 的第 4 段提取（与 Kiro IDE 一致），回退到凭据/config region。
//! 请求头使用 aws-sdk-js User-Agent 标识。请求体按凭据类型条件注入 `profileArn`
//! （Enterprise/external_idp 不注入，见 `should_send_profile_arn`）。

use reqwest::RequestBuilder;
use uuid::Uuid;

use super::{KiroEndpoint, RequestContext};

/// Kiro IDE 端点名称
pub const IDE_ENDPOINT_NAME: &str = "ide";

/// Anthropic 1M 上下文窗口的 beta 特性标识(官方 `context-1m-2025-08-07`)。
const BETA_1M: &str = "context-1m-2025-08-07";

/// 纯函数:据 is_1m 决定要不要注入 1M beta 头。抽出便于单测(decorate_api 返回 RequestBuilder
/// 不便直接断言 header)。is_1m=true → Some(beta 值);否则 None(不注入)。
fn beta_header_for_1m(is_1m: bool) -> Option<&'static str> {
    if is_1m { Some(BETA_1M) } else { None }
}

/// Kiro IDE 端点
pub struct IdeEndpoint;

impl IdeEndpoint {
    pub fn new() -> Self {
        Self
    }

    fn api_region<'a>(&self, ctx: &'a RequestContext<'_>) -> &'a str {
        // Region 解析(稳健版):profileArn 第 4 段(严格校验 arn 前缀 + region 白名单)
        // > 凭据 region/auth_region > config。严格校验防污染 ARN 拼出坏 host(DNS/502)。
        ctx.credentials.effective_upstream_region(ctx.config)
    }

    fn host(&self, ctx: &RequestContext<'_>) -> String {
        format!("runtime.{}.kiro.dev", self.api_region(ctx))
    }

    fn x_amz_user_agent(&self, ctx: &RequestContext<'_>) -> String {
        format!(
            "aws-sdk-js/1.0.34 KiroIDE-{}-{}",
            ctx.config.kiro_version, ctx.machine_id
        )
    }

    fn user_agent(&self, ctx: &RequestContext<'_>) -> String {
        format!(
            "aws-sdk-js/1.0.34 ua/2.1 os/{} lang/js md/nodejs#{} api/codewhispererstreaming#1.0.34 m/E KiroIDE-{}-{}",
            ctx.config.system_version,
            ctx.config.node_version,
            ctx.config.kiro_version,
            ctx.machine_id
        )
    }
}

impl Default for IdeEndpoint {
    fn default() -> Self {
        Self::new()
    }
}

impl KiroEndpoint for IdeEndpoint {
    fn name(&self) -> &'static str {
        IDE_ENDPOINT_NAME
    }

    fn api_url(&self, ctx: &RequestContext<'_>) -> String {
        format!(
            "https://runtime.{}.kiro.dev/generateAssistantResponse",
            self.api_region(ctx)
        )
    }

    fn mcp_url(&self, ctx: &RequestContext<'_>) -> String {
        format!("https://runtime.{}.kiro.dev/mcp", self.api_region(ctx))
    }

    fn decorate_api(&self, req: RequestBuilder, ctx: &RequestContext<'_>) -> RequestBuilder {
        let mut req = req
            .header("x-amzn-codewhisperer-optout", "true")
            .header("x-amzn-kiro-agent-mode", "vibe")
            .header("x-amz-user-agent", self.x_amz_user_agent(ctx))
            .header("user-agent", self.user_agent(ctx))
            .header("host", self.host(ctx))
            .header("amz-sdk-invocation-id", Uuid::new_v4().to_string())
            .header("amz-sdk-request", "attempt=1; max=3")
            .header("Authorization", format!("Bearer {}", ctx.token));

        if ctx.credentials.is_api_key_credential() {
            req = req.header("tokentype", "API_KEY");
        } else if ctx.credentials.is_external_idp_credential() {
            req = req.header("tokentype", "EXTERNAL_IDP");
        }
        // 1M 上下文变体:注入 anthropic-beta 头,上游(若为 Anthropic 直连/透传)才会放开 1M 窗口。
        // Kiro 路径从零构造请求(不转发客户端原始 header),故此处不会与已有 anthropic-beta 重复。
        // 诚实边界见 model_catalog::ModelSpec::supports_1m 注释:上游是否真识别待旁挂验证。
        if let Some(beta) = beta_header_for_1m(ctx.is_1m) {
            req = req.header("anthropic-beta", beta);
        }
        req
    }

    fn decorate_mcp(&self, req: RequestBuilder, ctx: &RequestContext<'_>) -> RequestBuilder {
        let mut req = req
            .header("x-amz-user-agent", self.x_amz_user_agent(ctx))
            .header("user-agent", self.user_agent(ctx))
            .header("host", self.host(ctx))
            .header("amz-sdk-invocation-id", Uuid::new_v4().to_string())
            .header("amz-sdk-request", "attempt=1; max=3")
            .header("Authorization", format!("Bearer {}", ctx.token));

        if let Some(arn) = ctx.credentials.effective_profile_arn() {
            req = req.header("x-amzn-kiro-profile-arn", arn);
        }
        if ctx.credentials.is_api_key_credential() {
            req = req.header("tokentype", "API_KEY");
        } else if ctx.credentials.is_external_idp_credential() {
            req = req.header("tokentype", "EXTERNAL_IDP");
        }
        req
    }

    fn transform_api_body(&self, body: &str, ctx: &RequestContext<'_>) -> String {
        // 用 effective_profile_arn:idc/social 缺 profileArn 时回退默认 BuilderId ARN,
        // external_idp 用动态解析到的真实租户 ARN(kiro.dev 迁移后 external_idp 也必须带,
        // 缺了 400 profileArn is required);仅在 arn 为 None 时不注入。
        inject_profile_arn(body, &ctx.credentials.effective_profile_arn())
    }
}

/// 将 profile_arn 注入到请求体 JSON 根对象
fn inject_profile_arn(request_body: &str, profile_arn: &Option<String>) -> String {
    if let Some(arn) = profile_arn {
        if let Ok(mut json) = serde_json::from_str::<serde_json::Value>(request_body) {
            json["profileArn"] = serde_json::Value::String(arn.clone());
            if let Ok(body) = serde_json::to_string(&json) {
                return body;
            }
        }
    }
    request_body.to_string()
}

#[cfg(test)]
mod tests {
    use super::{beta_header_for_1m, inject_profile_arn, BETA_1M};
    use serde_json::Value;

    #[test]
    fn test_beta_header_for_1m() {
        assert_eq!(beta_header_for_1m(true), Some(BETA_1M));
        assert_eq!(beta_header_for_1m(true), Some("context-1m-2025-08-07"));
        assert_eq!(beta_header_for_1m(false), None);
    }

    #[test]
    fn test_inject_profile_arn_with_some() {
        let body = r#"{"conversationState":{"conversationId":"c1"}}"#;
        let arn = Some("arn:aws:codewhisperer:us-east-1:123:profile/ABC".to_string());
        let result = inject_profile_arn(body, &arn);
        let json: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(
            json["profileArn"],
            "arn:aws:codewhisperer:us-east-1:123:profile/ABC"
        );
        assert_eq!(json["conversationState"]["conversationId"], "c1");
    }

    #[test]
    fn test_inject_profile_arn_with_none() {
        let body = r#"{"conversationState":{"conversationId":"c1"}}"#;
        let result = inject_profile_arn(body, &None);
        let json: Value = serde_json::from_str(&result).unwrap();
        assert!(json.get("profileArn").is_none());
        assert_eq!(json["conversationState"]["conversationId"], "c1");
    }

    #[test]
    fn test_inject_profile_arn_overwrites_existing() {
        let body = r#"{"conversationState":{},"profileArn":"old-arn"}"#;
        let arn = Some("new-arn".to_string());
        let result = inject_profile_arn(body, &arn);
        let json: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(json["profileArn"], "new-arn");
    }

    #[test]
    fn test_inject_profile_arn_invalid_json() {
        let body = "not-valid-json";
        let arn = Some("arn:test".to_string());
        let result = inject_profile_arn(body, &arn);
        assert_eq!(result, "not-valid-json");
    }
}
