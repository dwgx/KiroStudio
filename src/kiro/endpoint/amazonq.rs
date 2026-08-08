//! Kiro CLI 端点（`q.{region}.amazonaws.com` + `AmazonQDeveloperStreamingService.SendMessage`
//! 变体 / API Key 认证）
//!
//! 与 [`super::cli`]（`q.{region}.amazonaws.com` + `GenerateAssistantResponse`）**协议同构**：
//! 服务根 `/` + `X-Amz-Target` + `tokentype: API_KEY` + 绝不注入 profileArn，仅
//! `X-Amz-Target` 的**操作**不同（`SendMessage` 而非 `GenerateAssistantResponse`）。
//!
//! # 为什么需要第四个 CLI 端点
//!
//! 上游对不同操作/host 的组合划分**独立限流桶**（参考 kiro2cc `endpoint.rs`：4 端点 = 4 桶）。
//! 本端点的 host 与 `cli` 相同（`q.*`），但 `X-Amz-Target` 是 Amazon Q Developer 的
//! `SendMessage` 操作。当 `q.*` 的 GenerateAssistantResponse 桶被 429 封禁时可换到本桶
//! 继续。
//!
//! ⚠️ **协议可靠性风险**：`SendMessage` 操作对 `ksk_` API Key 号**未实测**。kiro2cc 把它
//! 作为第 4 个桶使用，但本仓没有线上证据确认上游接受该 target。故在
//! [`KiroCredentials::effective_endpoint_order`] 里放**最后**（兜底），需线上验证。若不可用，
//! 最坏情况是该桶请求 4xx → 换号，不会比现状更差（现有 2 桶本来就会在 q.* 封禁时 429 透传）。
//!
//! `transform_api_body` / UA / 全部 head 复用 [`super::cli`]，避免三份协议逻辑漂移。

use reqwest::RequestBuilder;

use super::cli::{
    cli_user_agent, decorate_cli_mcp, decorate_cli_protocol, inject_cli_agent_fields,
    set_origin_kiro_cli,
};
use super::{KiroEndpoint, RequestContext};

/// Kiro CLI 端点名称（对应 credentials.endpoint / config.defaultEndpoint 的 `"amazonq"` 取值）。
pub const AMAZONQ_ENDPOINT_NAME: &str = "amazonq";

/// Amazon Q Developer Streaming 的目标操作头值（`SendMessage`，区别于 cli 的
/// `GenerateAssistantResponse`）。
const AMZ_TARGET: &str = "AmazonQDeveloperStreamingService.SendMessage";

/// Kiro CLI 端点（amazonq 变体，host = q.*）
pub struct AmazonqEndpoint;

impl AmazonqEndpoint {
    pub fn new() -> Self {
        Self
    }

    fn api_region<'a>(&self, ctx: &'a RequestContext<'_>) -> &'a str {
        // 与 cli 端点同口径：profileArn 第 4 段 > 凭据 region > config。
        ctx.credentials.effective_upstream_region(ctx.config)
    }

    fn host(&self, ctx: &RequestContext<'_>) -> String {
        format!("q.{}.amazonaws.com", self.api_region(ctx))
    }
}

impl Default for AmazonqEndpoint {
    fn default() -> Self {
        Self::new()
    }
}

impl KiroEndpoint for AmazonqEndpoint {
    fn name(&self) -> &'static str {
        AMAZONQ_ENDPOINT_NAME
    }

    fn content_type(&self) -> &'static str {
        // CLI 协议走 X-Amz-Target 路由，content-type 必须是 x-amz-json-1.0。
        "application/x-amz-json-1.0"
    }

    fn api_url(&self, ctx: &RequestContext<'_>) -> String {
        // 服务根路径（末尾 `/`），操作由 X-Amz-Target 头路由。
        format!("https://{}/", self.host(ctx))
    }

    fn mcp_url(&self, ctx: &RequestContext<'_>) -> String {
        // CLI 协议同样走服务根 + X-Amz-Target；CLI 号目前不走 MCP，保留同 host 兜底。
        format!("https://{}/", self.host(ctx))
    }

    fn decorate_api(&self, req: RequestBuilder, ctx: &RequestContext<'_>) -> RequestBuilder {
        decorate_cli_protocol(
            req,
            ctx,
            self.host(ctx),
            AMZ_TARGET,
            cli_user_agent(ctx.machine_id),
        )
    }

    fn decorate_mcp(&self, req: RequestBuilder, ctx: &RequestContext<'_>) -> RequestBuilder {
        decorate_cli_mcp(req, ctx, self.host(ctx), cli_user_agent(ctx.machine_id))
    }

    fn transform_api_body(&self, body: &str, ctx: &RequestContext<'_>) -> String {
        // 与 cli 端点逐字节同构：注入 vibe 字段，**绝不**注入 profileArn。
        let body = inject_cli_agent_fields(body);
        if ctx
            .credentials
            .effective_cli_origin_kiro_cli(ctx.config.cli_origin_kiro_cli)
        {
            set_origin_kiro_cli(&body)
        } else {
            body
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 回归：amazonq 的 URL 必须是 `q.{region}.amazonaws.com` 服务根（末尾 `/`、
    /// 无 `/generateAssistantResponse` 路径）。
    #[test]
    fn should_target_q_service_root_url() {
        use super::super::{KiroEndpoint, RequestContext};
        use crate::kiro::model::credentials::KiroCredentials;
        use crate::model::config::Config;

        let mut cred = KiroCredentials::default();
        cred.kiro_api_key = Some("ksk_test".to_string());
        cred.region = Some("us-east-1".to_string());
        let config = Config::default();
        let ctx = RequestContext {
            credentials: &cred,
            token: "ksk_test",
            machine_id: "mid",
            config: &config,
            is_1m: false,
        };

        let url = AmazonqEndpoint::new().api_url(&ctx);
        assert_eq!(url, "https://q.us-east-1.amazonaws.com/");
        assert!(
            !url.contains("generateAssistantResponse"),
            "CLI 协议不用路径寻址操作: {url}"
        );
    }

    /// content-type 必须是 x-amz-json-1.0（X-Amz-Target 路由必需）。
    #[test]
    fn should_use_amz_json_content_type() {
        assert_eq!(
            AmazonqEndpoint::new().content_type(),
            "application/x-amz-json-1.0"
        );
    }

    /// 端到端 body 契约：注入 vibe 字段但**绝不**注入 profileArn（API_KEY 认证带 ARN 会 403）。
    #[test]
    fn should_never_inject_profile_arn_even_when_credential_has_one() {
        use super::super::{KiroEndpoint, RequestContext};
        use crate::kiro::model::credentials::KiroCredentials;
        use crate::model::config::Config;

        let mut cred = KiroCredentials::default();
        cred.kiro_api_key = Some("ksk_test".to_string());
        cred.profile_arn = Some("arn:aws:codewhisperer:us-east-1:1:profile/OWN".to_string());
        let config = Config::default();
        let ctx = RequestContext {
            credentials: &cred,
            token: "ksk_test",
            machine_id: "mid",
            config: &config,
            is_1m: false,
        };

        let out = AmazonqEndpoint::new().transform_api_body(
            r#"{"conversationState":{"conversationId":"c1"}}"#,
            &ctx,
        );
        assert!(
            !out.contains("profileArn"),
            "amazonq 绝不能注入 profileArn: {out}"
        );
        assert!(out.contains("vibe"), "应注入 agentMode=vibe: {out}");
    }
}
