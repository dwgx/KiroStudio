//! 备用 Kiro 端点（CodeWhisperer / AmazonQ）
//!
//! 上游 429 / 5xx 往往是**端点级**容量问题，而非凭据额度问题：同一个凭据换到
//! 另一个上游端点常常立刻成功。kiro-go 的 `endpointFallback` 正是靠这三个端点
//! 轮转把 429 吃掉的（见 `proxy/kiro.go` 的 `kiroEndpoints`），而本项目此前只有
//! `ide` 一个端点，单凭据时 `compute_max_retries` 退化为 1 → 一次 429 直接透传，
//! 客户端遂报 `exceeded retry limit`。
//!
//! 三个端点的差异只在 host 与 `x-amz-target`，其余请求头/请求体与 [`super::ide`]
//! 完全一致，故此处复用 ide 的 `inject_profile_arn` 与 1M beta 常量：
//!
//! | 端点           | host                                  | x-amz-target                                              |
//! |----------------|---------------------------------------|-----------------------------------------------------------|
//! | ide            | `q.{region}.amazonaws.com`            | （不设）                                                  |
//! | codewhisperer  | `codewhisperer.{region}.amazonaws.com`| `AmazonCodeWhispererStreamingService.GenerateAssistantResponse` |
//! | amazonq        | `q.{region}.amazonaws.com`            | `AmazonQDeveloperStreamingService.SendMessage`             |
//!
//! 三者均已用同一 idc 凭据实测返回 200（codewhisperer 已确认返回真实对话内容）。
//! 注意：`amazonq` 与 `ide` 同 host，仅 target 不同，是否真提供独立容量未经限流态
//! 验证；`codewhisperer` 是独立 host，回退价值更明确。

use reqwest::RequestBuilder;
use uuid::Uuid;

use super::ide::{beta_header_for_1m, inject_profile_arn};
use super::{KiroEndpoint, RequestContext};

/// CodeWhisperer 端点名称
pub const CODEWHISPERER_ENDPOINT_NAME: &str = "codewhisperer";

/// AmazonQ 端点名称
pub const AMAZONQ_ENDPOINT_NAME: &str = "amazonq";

/// 端点回退链的固定顺序（`ide` 优先，其后为备用端点）。
///
/// `endpoint_chain_for` 以凭据/配置指定的端点为链首，其余按此顺序补齐。
pub const ENDPOINT_FALLBACK_ORDER: &[&str] = &[
    super::ide::IDE_ENDPOINT_NAME,
    CODEWHISPERER_ENDPOINT_NAME,
    AMAZONQ_ENDPOINT_NAME,
];

/// 构造与 ide 端点同款的 `x-amz-user-agent`
fn x_amz_user_agent(ctx: &RequestContext<'_>) -> String {
    format!(
        "aws-sdk-js/1.0.34 KiroIDE-{}-{}",
        ctx.config.kiro_version, ctx.machine_id
    )
}

/// 构造与 ide 端点同款的 `user-agent`
fn user_agent(ctx: &RequestContext<'_>) -> String {
    format!(
        "aws-sdk-js/1.0.34 ua/2.1 os/{} lang/js md/nodejs#{} api/codewhispererstreaming#1.0.34 m/E KiroIDE-{}-{}",
        ctx.config.system_version, ctx.config.node_version, ctx.config.kiro_version, ctx.machine_id
    )
}

/// 备用端点共用的请求头装饰（与 ide 的 `decorate_api` 等价，额外带 `x-amz-target`）
fn decorate(
    req: RequestBuilder,
    ctx: &RequestContext<'_>,
    host: &str,
    amz_target: &str,
) -> RequestBuilder {
    let mut req = req
        .header("x-amzn-codewhisperer-optout", "true")
        .header("x-amzn-kiro-agent-mode", "vibe")
        .header("x-amz-target", amz_target)
        .header("x-amz-user-agent", x_amz_user_agent(ctx))
        .header("user-agent", user_agent(ctx))
        .header("host", host)
        .header("amz-sdk-invocation-id", Uuid::new_v4().to_string())
        .header("amz-sdk-request", "attempt=1; max=3")
        .header("Authorization", format!("Bearer {}", ctx.token));

    if ctx.credentials.is_api_key_credential() {
        req = req.header("tokentype", "API_KEY");
    } else if ctx.credentials.is_external_idp_credential() {
        req = req.header("tokentype", "EXTERNAL_IDP");
    }
    if let Some(beta) = beta_header_for_1m(ctx.is_1m) {
        req = req.header("anthropic-beta", beta);
    }
    req
}

/// CodeWhisperer 端点（独立 host，回退首选）
pub struct CodeWhispererEndpoint;

impl CodeWhispererEndpoint {
    pub fn new() -> Self {
        Self
    }

    fn host(&self, ctx: &RequestContext<'_>) -> String {
        format!(
            "codewhisperer.{}.amazonaws.com",
            ctx.credentials.effective_upstream_region(ctx.config)
        )
    }
}

impl Default for CodeWhispererEndpoint {
    fn default() -> Self {
        Self::new()
    }
}

impl KiroEndpoint for CodeWhispererEndpoint {
    fn name(&self) -> &'static str {
        CODEWHISPERER_ENDPOINT_NAME
    }

    fn api_url(&self, ctx: &RequestContext<'_>) -> String {
        format!("https://{}/generateAssistantResponse", self.host(ctx))
    }

    fn mcp_url(&self, ctx: &RequestContext<'_>) -> String {
        format!("https://{}/mcp", self.host(ctx))
    }

    fn decorate_api(&self, req: RequestBuilder, ctx: &RequestContext<'_>) -> RequestBuilder {
        decorate(
            req,
            ctx,
            &self.host(ctx),
            "AmazonCodeWhispererStreamingService.GenerateAssistantResponse",
        )
    }

    fn decorate_mcp(&self, req: RequestBuilder, ctx: &RequestContext<'_>) -> RequestBuilder {
        self.decorate_api(req, ctx)
    }

    fn transform_api_body(&self, body: &str, ctx: &RequestContext<'_>) -> String {
        inject_profile_arn(body, &ctx.credentials.effective_profile_arn())
    }
}

/// AmazonQ 端点（与 ide 同 host，仅 `x-amz-target` 不同）
pub struct AmazonQEndpoint;

impl AmazonQEndpoint {
    pub fn new() -> Self {
        Self
    }

    fn host(&self, ctx: &RequestContext<'_>) -> String {
        format!(
            "q.{}.amazonaws.com",
            ctx.credentials.effective_upstream_region(ctx.config)
        )
    }
}

impl Default for AmazonQEndpoint {
    fn default() -> Self {
        Self::new()
    }
}

impl KiroEndpoint for AmazonQEndpoint {
    fn name(&self) -> &'static str {
        AMAZONQ_ENDPOINT_NAME
    }

    fn api_url(&self, ctx: &RequestContext<'_>) -> String {
        format!("https://{}/generateAssistantResponse", self.host(ctx))
    }

    fn mcp_url(&self, ctx: &RequestContext<'_>) -> String {
        format!("https://{}/mcp", self.host(ctx))
    }

    fn decorate_api(&self, req: RequestBuilder, ctx: &RequestContext<'_>) -> RequestBuilder {
        decorate(
            req,
            ctx,
            &self.host(ctx),
            "AmazonQDeveloperStreamingService.SendMessage",
        )
    }

    fn decorate_mcp(&self, req: RequestBuilder, ctx: &RequestContext<'_>) -> RequestBuilder {
        self.decorate_api(req, ctx)
    }

    fn transform_api_body(&self, body: &str, ctx: &RequestContext<'_>) -> String {
        inject_profile_arn(body, &ctx.credentials.effective_profile_arn())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_endpoint_names() {
        assert_eq!(CodeWhispererEndpoint::new().name(), "codewhisperer");
        assert_eq!(AmazonQEndpoint::new().name(), "amazonq");
    }

    #[test]
    fn test_fallback_order_starts_with_ide_and_has_no_dupes() {
        assert_eq!(
            ENDPOINT_FALLBACK_ORDER[0],
            super::super::ide::IDE_ENDPOINT_NAME
        );
        let mut seen = ENDPOINT_FALLBACK_ORDER.to_vec();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), ENDPOINT_FALLBACK_ORDER.len());
    }
}
