//! Kiro CLI 端点（`codewhisperer.{region}.amazonaws.com` 变体 / API Key 认证）
//!
//! 与 [`super::cli`]（`q.{region}.amazonaws.com`）**协议完全同构**：服务根 `/` +
//! `X-Amz-Target` + `tokentype: API_KEY` + 绝不注入 profileArn，仅 host 域不同。
//!
//! # 为什么需要第三个 CLI 端点
//!
//! 上游对不同的 host 划分**独立限流桶**（参考 kiro2cc `endpoint.rs`：4 端点 = 4 桶）。
//! 本端点在 **us-east-1** 走独占主机 `codewhisperer.{region}.amazonaws.com`（与 `q.*` /
//! `runtime.*` 都独立的桶）；其它区域回退 `q.{region}.amazonaws.com`（与 cli 同一 host，
//! 但按 kiro2cc 仍算独立桶）。当 `q.*` / `runtime.*` 桶被 429 封禁时可换到本桶继续，
//! 绕过该桶限流。
//!
//! `transform_api_body` / UA / 全部 head 复用 [`super::cli`]，避免三份协议逻辑漂移。

use reqwest::RequestBuilder;

use super::cli::{
    cli_user_agent, decorate_cli_mcp, decorate_cli_protocol, inject_cli_agent_fields,
    set_origin_kiro_cli,
};
use super::{KiroEndpoint, RequestContext};

/// Kiro CLI 端点名称（对应 credentials.endpoint / config.defaultEndpoint 的 `"codewhisperer"` 取值）。
pub const CODEWHISPERER_ENDPOINT_NAME: &str = "codewhisperer";

/// Amazon CodeWhisperer Streaming 的目标操作头值（与 `cli` 端点同一协议）。
const AMZ_TARGET: &str = "AmazonCodeWhispererStreamingService.GenerateAssistantResponse";

/// Kiro CLI 端点（codewhisperer.* host）
pub struct CodewhispererEndpoint;

impl CodewhispererEndpoint {
    pub fn new() -> Self {
        Self
    }

    fn api_region<'a>(&self, ctx: &'a RequestContext<'_>) -> &'a str {
        // 与 cli 端点同口径：profileArn 第 4 段 > 凭据 region > config。
        ctx.credentials.effective_upstream_region(ctx.config)
    }

    /// us-east-1 走独占主机 `codewhisperer.{region}.amazonaws.com`，其它区域回退 `q.*`
    ///（与 kiro2cc `endpoint.rs` 的 `codewhisperer_host_lowered` 一致）。
    fn host(&self, ctx: &RequestContext<'_>) -> String {
        let region = self.api_region(ctx);
        if region == "us-east-1" {
            format!("codewhisperer.{region}.amazonaws.com")
        } else {
            format!("q.{region}.amazonaws.com")
        }
    }
}

impl Default for CodewhispererEndpoint {
    fn default() -> Self {
        Self::new()
    }
}

impl KiroEndpoint for CodewhispererEndpoint {
    fn name(&self) -> &'static str {
        CODEWHISPERER_ENDPOINT_NAME
    }

    fn content_type(&self) -> &'static str {
        // CLI 协议走 X-Amz-Target 路由，content-type 必须是 x-amz-json-1.0。
        "application/x-amz-json-1.0"
    }

    fn amz_target(&self) -> Option<&'static str> {
        Some(AMZ_TARGET)
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

    /// 回归：codewhisperer 的 URL 在 us-east-1 必须是独占 host `codewhisperer.{region}.amazonaws.com`
    /// 服务根（末尾 `/`、无 `/generateAssistantResponse` 路径）。
    #[test]
    fn should_target_codewhisperer_host_for_us_east_1() {
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

        let url = CodewhispererEndpoint::new().api_url(&ctx);
        assert_eq!(url, "https://codewhisperer.us-east-1.amazonaws.com/");
        assert!(
            !url.contains("generateAssistantResponse"),
            "CLI 协议不用路径寻址操作: {url}"
        );
    }

    /// 非 us-east-1 区域回退 `q.{region}.amazonaws.com`（与 kiro2cc 一致）。
    #[test]
    fn should_fall_back_to_q_host_for_non_us_east_1() {
        use super::super::{KiroEndpoint, RequestContext};
        use crate::kiro::model::credentials::KiroCredentials;
        use crate::model::config::Config;

        let mut cred = KiroCredentials::default();
        cred.kiro_api_key = Some("ksk_test".to_string());
        cred.region = Some("eu-central-1".to_string());
        let config = Config::default();
        let ctx = RequestContext {
            credentials: &cred,
            token: "ksk_test",
            machine_id: "mid",
            config: &config,
            is_1m: false,
        };

        let url = CodewhispererEndpoint::new().api_url(&ctx);
        assert_eq!(url, "https://q.eu-central-1.amazonaws.com/");
    }

    /// content-type 必须是 x-amz-json-1.0（X-Amz-Target 路由必需）。
    #[test]
    fn should_use_amz_json_content_type() {
        assert_eq!(
            CodewhispererEndpoint::new().content_type(),
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

        let out = CodewhispererEndpoint::new().transform_api_body(
            r#"{"conversationState":{"conversationId":"c1"}}"#,
            &ctx,
        );
        assert!(
            !out.contains("profileArn"),
            "codewhisperer 绝不能注入 profileArn: {out}"
        );
        assert!(out.contains("vibe"), "应注入 agentMode=vibe: {out}");
    }

    /// ⭐ 顺序守卫（deepseek review 补齐）：本文件复制了 cli.rs 的 `inject → origin` 序列，
    /// 但此前没有守卫，cli.rs:673 那份管不到这里。`set_origin_kiro_cli` 第一步是字符串字面量
    /// 替换 `"origin":"AI_EDITOR"`，只对 serde 紧凑序列化成立；inject 必须在它之前，保证它拿到的
    /// 一定是 serde 刚吐出的紧凑串。删掉/颠倒这里的顺序 → 本测试必须 FAILED。
    #[test]
    fn inject_must_run_before_origin_rewrite() {
        let src = include_str!("codewhisperer.rs");
        let prod = src
            .split("#[cfg(test)]")
            .next()
            .expect("生产段应存在")
            .lines()
            .filter(|l| !l.trim_start().starts_with("//") && !l.trim_start().starts_with("///"))
            .collect::<Vec<_>>()
            .join("\n");
        // needle 运行时拼接，避免 include_str! 自匹配；只匹配到左括号、不含实参。
        let inject_call = ["inject_cli_agent", "_fields("].concat();
        let origin_call = ["set_origin_kiro", "_cli("].concat();
        let i = prod
            .find(inject_call.as_str())
            .expect("transform_api_body 必须仍调 inject_cli_agent_fields");
        let o = prod
            .find(origin_call.as_str())
            .expect("transform_api_body 必须按开关调 set_origin_kiro_cli");
        assert!(
            i < o,
            "vibe 注入必须在 origin 改写之前求值 —— 否则字面量替换的紧凑序列化前提失去保证"
        );
        let gate = ["ctx.config.cli_origin", "_kiro_cli"].concat();
        let g = prod
            .find(gate.as_str())
            .expect("origin 改写必须由 config 开关把门");
        assert!(g < o, "开关判定必须排在 set_origin_kiro_cli 调用之前");
    }
}
