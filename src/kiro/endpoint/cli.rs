//! Kiro CLI 端点（Amazon Q Developer CLI 协议 / API Key 认证）
//!
//! 对应「Kiro API Key」(`ksk_` 前缀) 号，它们本质是 AWS IAM Identity Center(IdC) 账号
//! 的 CLI 访问密钥。这类号**绝不能**走 IDE 端点（`runtime.{region}.kiro.dev/generateAssistantResponse`）——
//! 实测会被上游 403（`User is not authorized` / 缺自己租户真实 profileArn 时 400/403）。
//!
//! CLI 协议与 IDE 端点的关键差异（全部旁挂实测四个 ksk_ 号 HTTP 200 验证）：
//! - **URL 是服务根 `/`**：`https://q.{region}.amazonaws.com/`（注意 host 是 `q.` 不是
//!   `runtime.`，路径为空），操作由 `X-Amz-Target` 头路由，而非 URL 路径。
//! - **`X-Amz-Target: AmazonCodeWhispererStreamingService.GenerateAssistantResponse`**。
//! - **`tokentype: API_KEY`** 必带。
//! - **绝不注入 profileArn**：API_KEY 认证既不使用也不支持 profileArn；带上反而 403。
//!   （这是它与 IDE 端点 `transform_api_body` 注入 ARN 的**根本区别**。）
//! - User-Agent 用 **aws-sdk-rust ... app/AmazonQ-For-CLI** 标识（区别于 IDE 的 aws-sdk-js/KiroIDE）。
//! - 请求体加 `conversationState.agentTaskType="vibe"` + 顶层 `agentMode="vibe"`（与官方 CLI 一致；
//!   实测缺省也可，保留以贴合上游）。
//!
//! 响应体同为 AWS event-stream（`application/vnd.amazon.eventstream`），复用现有 `parser/` 解码。
//!
//! 参考实现（协议来源）：satiyap/pi-kiro-api（API-key 适配）、hueyexe/open-kiro、jwadow/kiro-gateway。

use reqwest::RequestBuilder;
use uuid::Uuid;

use super::{KiroEndpoint, RequestContext};

/// Kiro CLI 端点名称（对应 credentials.endpoint / config.defaultEndpoint 的 `"cli"` 取值）。
pub const CLI_ENDPOINT_NAME: &str = "cli";

/// Amazon Q CLI 的目标操作头值（对话生成）。
const CLI_AMZ_TARGET: &str = "AmazonCodeWhispererStreamingService.GenerateAssistantResponse";

/// Kiro CLI 端点
pub struct CliEndpoint;

impl CliEndpoint {
    pub fn new() -> Self {
        Self
    }

    fn api_region<'a>(&self, ctx: &'a RequestContext<'_>) -> &'a str {
        // 与 IDE 端点同口径：profileArn 第 4 段 > 凭据 region > config；ksk_ 号通常无
        // profileArn/region → 回退 config region（默认 us-east-1，实测 q.us-east-1 可用）。
        ctx.credentials.effective_upstream_region(ctx.config)
    }

    fn host(&self, ctx: &RequestContext<'_>) -> String {
        format!("q.{}.amazonaws.com", self.api_region(ctx))
    }

    /// aws-sdk-rust 版 UA，带 `app/AmazonQ-For-CLI` 标识（区别于 IDE 的 aws-sdk-js/KiroIDE）。
    fn user_agent(&self, ctx: &RequestContext<'_>) -> String {
        format!(
            "aws-sdk-rust/1.0.0 ua/2.1 os/other lang/rust api/codewhispererstreaming#1.28.3 m/E app/AmazonQ-For-CLI md/appVersion-1.28.3-{}",
            ctx.machine_id
        )
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

    fn content_type(&self) -> &'static str {
        // CLI 协议走 X-Amz-Target 路由，content-type 必须是 x-amz-json-1.0，否则上游返回
        // UnknownOperationException 的 JSON（非 event-stream），解码器会读到非法帧长而中断。
        "application/x-amz-json-1.0"
    }

    fn api_url(&self, ctx: &RequestContext<'_>) -> String {
        // 服务根路径（末尾 `/`），操作由 X-Amz-Target 头路由。
        format!("https://q.{}.amazonaws.com/", self.api_region(ctx))
    }

    fn mcp_url(&self, ctx: &RequestContext<'_>) -> String {
        // CLI 协议同样走服务根 + X-Amz-Target；MCP 目前未在 CLI 号上使用，保留同 host 兜底。
        format!("https://q.{}.amazonaws.com/", self.api_region(ctx))
    }

    fn decorate_api(&self, req: RequestBuilder, ctx: &RequestContext<'_>) -> RequestBuilder {
        let ua = self.user_agent(ctx);
        req.header("X-Amz-Target", CLI_AMZ_TARGET)
            .header("tokentype", "API_KEY")
            .header("x-amzn-codewhisperer-optout", "true")
            .header("x-amzn-kiro-agent-mode", "vibe")
            .header("x-amz-user-agent", &ua)
            .header("user-agent", &ua)
            .header("host", self.host(ctx))
            .header("amz-sdk-invocation-id", Uuid::new_v4().to_string())
            .header("amz-sdk-request", "attempt=1; max=1")
            .header("Authorization", format!("Bearer {}", ctx.token))
        // 刻意不注入 profileArn / anthropic-beta：API_KEY 认证不使用 profileArn；
        // CLI 端点的 1M 窗口由上游按 modelId 决定，不依赖 anthropic-beta 头。
    }

    fn decorate_mcp(&self, req: RequestBuilder, ctx: &RequestContext<'_>) -> RequestBuilder {
        let ua = self.user_agent(ctx);
        req.header("tokentype", "API_KEY")
            .header("x-amz-user-agent", &ua)
            .header("user-agent", &ua)
            .header("host", self.host(ctx))
            .header("amz-sdk-invocation-id", Uuid::new_v4().to_string())
            .header("amz-sdk-request", "attempt=1; max=1")
            .header("Authorization", format!("Bearer {}", ctx.token))
    }

    fn transform_api_body(&self, body: &str, _ctx: &RequestContext<'_>) -> String {
        // CLI 协议：注入 agentTaskType/agentMode="vibe"，**绝不**注入 profileArn。
        inject_cli_agent_fields(body)
    }
}

/// 给请求体注入 CLI 协议字段：`conversationState.agentTaskType="vibe"` + 顶层 `agentMode="vibe"`。
/// 解析失败时原样返回（与上游宽松，实测缺这两字段也 200，故不因注入失败而破坏请求）。
fn inject_cli_agent_fields(request_body: &str) -> String {
    let Ok(mut json) = serde_json::from_str::<serde_json::Value>(request_body) else {
        return request_body.to_string();
    };
    if let Some(cs) = json
        .get_mut("conversationState")
        .and_then(|v| v.as_object_mut())
    {
        cs.insert(
            "agentTaskType".to_string(),
            serde_json::Value::String("vibe".to_string()),
        );
    }
    if let Some(obj) = json.as_object_mut() {
        obj.insert(
            "agentMode".to_string(),
            serde_json::Value::String("vibe".to_string()),
        );
    }
    serde_json::to_string(&json).unwrap_or_else(|_| request_body.to_string())
}

#[cfg(test)]
mod tests {
    use super::{CliEndpoint, inject_cli_agent_fields};
    use serde_json::Value;

    #[test]
    fn test_inject_cli_agent_fields_adds_vibe() {
        let body = r#"{"conversationState":{"conversationId":"c1","chatTriggerType":"MANUAL"}}"#;
        let out = inject_cli_agent_fields(body);
        let json: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(json["agentMode"], "vibe");
        assert_eq!(json["conversationState"]["agentTaskType"], "vibe");
        // 原有字段不动
        assert_eq!(json["conversationState"]["conversationId"], "c1");
        assert_eq!(json["conversationState"]["chatTriggerType"], "MANUAL");
    }

    #[test]
    fn test_inject_cli_agent_fields_never_adds_profile_arn() {
        // CLI 端点铁律：绝不注入 profileArn（API_KEY 带上会 403）。
        let body = r#"{"conversationState":{"conversationId":"c1"}}"#;
        let out = inject_cli_agent_fields(body);
        let json: Value = serde_json::from_str(&out).unwrap();
        assert!(
            json.get("profileArn").is_none(),
            "CLI 端点绝不能注入 profileArn"
        );
    }

    #[test]
    fn test_inject_cli_agent_fields_invalid_json_passthrough() {
        let body = "not-valid-json";
        assert_eq!(inject_cli_agent_fields(body), "not-valid-json");
    }

    /// CLI 的 URL 必须是 **`q.` 服务根**（末尾 `/`、无 `/generateAssistantResponse` 路径）——
    /// 操作靠 `X-Amz-Target` 头路由。打成 IDE 的 `runtime.*.kiro.dev/generateAssistantResponse`
    /// 会让 ksk_ 号 403。
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

        let url = CliEndpoint::new().api_url(&ctx);
        assert_eq!(url, "https://q.us-east-1.amazonaws.com/");
        assert!(
            !url.contains("generateAssistantResponse"),
            "CLI 协议不用路径寻址操作: {url}"
        );
        assert!(
            !url.contains("kiro.dev"),
            "CLI 不打 IDE 的 kiro.dev host: {url}"
        );
    }

    /// 端到端 body 契约：transform_api_body 注入 vibe 字段但**绝不**注入 profileArn，
    /// 即使凭据自带一个（API_KEY 认证带 ARN 会 403）。
    #[test]
    fn should_never_inject_profile_arn_even_when_credential_has_one() {
        use super::super::{KiroEndpoint, RequestContext};
        use crate::kiro::model::credentials::KiroCredentials;
        use crate::model::config::Config;

        let mut cred = KiroCredentials::default();
        cred.kiro_api_key = Some("ksk_test".to_string());
        cred.profile_arn = Some("arn:aws:codewhisperer:us-east-1:999:profile/OWN".to_string());
        let config = Config::default();
        let ctx = RequestContext {
            credentials: &cred,
            token: "ksk_test",
            machine_id: "mid",
            config: &config,
            is_1m: false,
        };

        let out = CliEndpoint::new()
            .transform_api_body(r#"{"conversationState":{"conversationId":"c1"}}"#, &ctx);
        let json: Value = serde_json::from_str(&out).unwrap();
        assert!(
            json.get("profileArn").is_none(),
            "CLI 端点绝不能注入 profileArn（含凭据自带的），实际: {out}"
        );
        assert_eq!(json["agentMode"], "vibe");
    }
}
