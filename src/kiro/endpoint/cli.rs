//! Kiro CLI runtime endpoint used by `ksk_` API-key credentials.

use reqwest::RequestBuilder;
use uuid::Uuid;

use super::{KiroEndpoint, RequestContext};

pub const CLI_ENDPOINT_NAME: &str = "cli";

pub struct CliEndpoint;

impl CliEndpoint {
    pub fn new() -> Self {
        Self
    }

    fn host(&self, ctx: &RequestContext<'_>) -> String {
        format!(
            "runtime.{}.kiro.dev",
            ctx.credentials.effective_upstream_region(ctx.config)
        )
    }

    fn user_agent(&self, ctx: &RequestContext<'_>) -> String {
        format!(
            "aws-sdk-js/1.0.0 ua/2.1 os/{} lang/js md/nodejs#{} api/codewhispererruntime#1.0.0 m/N,E KiroIDE-{}-{}",
            ctx.config.system_version,
            ctx.config.node_version,
            ctx.config.kiro_version,
            ctx.machine_id
        )
    }

    fn decorate(&self, req: RequestBuilder, ctx: &RequestContext<'_>) -> RequestBuilder {
        req.header("content-type", "application/x-amz-json-1.0")
            .header(
                "x-amz-target",
                "AmazonCodeWhispererStreamingService.GenerateAssistantResponse",
            )
            .header("x-amzn-codewhisperer-optout", "true")
            .header(
                "x-amz-user-agent",
                format!("aws-sdk-js/1.0.0 KiroIDE-{}", ctx.config.kiro_version),
            )
            .header("user-agent", self.user_agent(ctx))
            .header("host", self.host(ctx))
            .header("amz-sdk-invocation-id", Uuid::new_v4().to_string())
            .header("amz-sdk-request", "attempt=1; max=3")
            .header("Authorization", format!("Bearer {}", ctx.token))
            .header("tokentype", "API_KEY")
    }
}

impl Default for CliEndpoint {
    fn default() -> Self {
        Self::new()
    }
}

impl KiroEndpoint for CliEndpoint {
    fn name(&self) -> &'static str {
        CLI_ENDPOINT_NAME
    }

    fn api_url(&self, ctx: &RequestContext<'_>) -> String {
        format!("https://{}/", self.host(ctx))
    }

    fn mcp_url(&self, ctx: &RequestContext<'_>) -> String {
        format!("https://{}/mcp", self.host(ctx))
    }

    fn decorate_api(&self, req: RequestBuilder, ctx: &RequestContext<'_>) -> RequestBuilder {
        self.decorate(req, ctx)
    }

    fn decorate_mcp(&self, req: RequestBuilder, ctx: &RequestContext<'_>) -> RequestBuilder {
        self.decorate(req, ctx)
    }

    fn transform_api_body(&self, body: &str, _ctx: &RequestContext<'_>) -> String {
        let Ok(mut value) = serde_json::from_str::<serde_json::Value>(body) else {
            return body.to_string();
        };
        if let Some(message) =
            value.pointer_mut("/conversationState/currentMessage/userInputMessage")
        {
            message["origin"] = serde_json::Value::String("KIRO_CLI".to_string());
        }
        value.as_object_mut().map(|o| o.remove("profileArn"));
        serde_json::to_string(&value).unwrap_or_else(|_| body.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kiro::model::credentials::KiroCredentials;
    use crate::model::config::Config;

    #[test]
    fn cli_body_sets_origin_and_removes_profile_arn() {
        let endpoint = CliEndpoint::new();
        let credentials = KiroCredentials::default();
        let config = Config::default();
        let ctx = RequestContext {
            credentials: &credentials,
            token: "ksk_test",
            machine_id: "machine",
            config: &config,
            is_1m: false,
        };
        let body = r#"{"profileArn":"stale","conversationState":{"currentMessage":{"userInputMessage":{"origin":"AI_EDITOR"}}}}"#;
        let value: serde_json::Value =
            serde_json::from_str(&endpoint.transform_api_body(body, &ctx)).unwrap();
        assert!(value.get("profileArn").is_none());
        assert_eq!(
            value.pointer("/conversationState/currentMessage/userInputMessage/origin"),
            Some(&serde_json::Value::String("KIRO_CLI".to_string()))
        );
    }
}
