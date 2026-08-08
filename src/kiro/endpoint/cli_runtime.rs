//! Kiro CLI 端点（`runtime.{region}.kiro.dev` 变体 / API Key 认证）
//!
//! 与 [`super::cli`]（`q.{region}.amazonaws.com`）**协议完全同构**：服务根 `/` +
//! `X-Amz-Target` + `tokentype: API_KEY` + 绝不注入 profileArn，**仅 host 域不同**。
//!
//! # 为什么需要第二个 CLI 端点
//!
//! 上游对 `q.{region}.amazonaws.com` 与 `runtime.{region}.kiro.dev` 按 host 划分**独立限流桶**
//! （参考 kiro2cc `endpoint.rs`：4 端点 = 4 桶）。实测 `q.*` 300 并发 0 个 429、`runtime.*` 31%
//! （`docs/batch2-region-endpoint-matrix.md`），故默认 **`q.*` 优先**；当 `q.*` 桶被 429 封禁时，
//! 同一把 `ksk_` key 自动换到本端点（`runtime.*` 桶）继续，绕过该桶限流。
//!
//! host 域不同，但请求形状与 `cli` 完全一致（Kiro-RS-Tool 2026.1.8 的 CLI 端点同样走
//! `runtime.{region}.kiro.dev` 服务根，是参考实现）。本端点的 `transform_api_body` / UA /
//! 全部 head 复用 [`super::cli`]，避免两份协议逻辑漂移。

use reqwest::RequestBuilder;

use super::cli::{
    cli_user_agent, decorate_cli_mcp, decorate_cli_protocol, inject_cli_agent_fields,
    set_origin_kiro_cli,
};
use super::{KiroEndpoint, RequestContext};

/// Kiro CLI 端点名称（对应 credentials.endpoint / config.defaultEndpoint 的 `"cli-runtime"` 取值）。
pub const CLI_RUNTIME_ENDPOINT_NAME: &str = "cli-runtime";

/// Amazon Q CLI 的目标操作头值（与 `cli` 端点同一协议）。
const CLI_AMZ_TARGET: &str = "AmazonCodeWhispererStreamingService.GenerateAssistantResponse";

/// Kiro CLI 端点（runtime.* host）
pub struct CliRuntimeEndpoint;

impl CliRuntimeEndpoint {
    pub fn new() -> Self {
        Self
    }

    fn api_region<'a>(&self, ctx: &'a RequestContext<'_>) -> &'a str {
        // 与 cli 端点同口径：profileArn 第 4 段 > 凭据 region > config。
        ctx.credentials.effective_upstream_region(ctx.config)
    }

    fn host(&self, ctx: &RequestContext<'_>) -> String {
        format!("runtime.{}.kiro.dev", self.api_region(ctx))
    }

    /// aws-sdk-rust 版 UA，带 `app/AmazonQ-For-CLI` 标识（与 `cli` 端点同形状）。
    fn user_agent(&self, ctx: &RequestContext<'_>) -> String {
        cli_user_agent(ctx.machine_id)
    }
}

impl Default for CliRuntimeEndpoint {
    fn default() -> Self {
        Self::new()
    }
}

impl KiroEndpoint for CliRuntimeEndpoint {
    fn name(&self) -> &'static str {
        CLI_RUNTIME_ENDPOINT_NAME
    }

    fn content_type(&self) -> &'static str {
        // CLI 协议走 X-Amz-Target 路由，content-type 必须是 x-amz-json-1.0，否则上游返回
        // UnknownOperationException 的 JSON（非 event-stream），解码器会读到非法帧长而中断。
        "application/x-amz-json-1.0"
    }

    fn amz_target(&self) -> Option<&'static str> {
        Some(CLI_AMZ_TARGET)
    }

    fn api_url(&self, ctx: &RequestContext<'_>) -> String {
        // 服务根路径（末尾 `/`），操作由 X-Amz-Target 头路由。
        format!("https://runtime.{}.kiro.dev/", self.api_region(ctx))
    }

    fn mcp_url(&self, ctx: &RequestContext<'_>) -> String {
        // CLI 协议同样走服务根 + X-Amz-Target；CLI 号目前不走 MCP，保留同 host 兜底。
        format!("https://runtime.{}.kiro.dev/", self.api_region(ctx))
    }

    fn decorate_api(&self, req: RequestBuilder, ctx: &RequestContext<'_>) -> RequestBuilder {
        decorate_cli_protocol(req, ctx, self.host(ctx), CLI_AMZ_TARGET, self.user_agent(ctx))
    }

    fn decorate_mcp(&self, req: RequestBuilder, ctx: &RequestContext<'_>) -> RequestBuilder {
        decorate_cli_mcp(req, ctx, self.host(ctx), self.user_agent(ctx))
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

    /// 回归：cli-runtime 的 URL 必须是 `runtime.{region}.kiro.dev` 服务根（末尾 `/`、
    /// 无 `/generateAssistantResponse` 路径）。若打成 IDE 的路径寻址，CLI 协议会拿到
    /// UnknownOperationException / 403。
    #[test]
    fn should_target_runtime_service_root_url() {
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

        let url = CliRuntimeEndpoint::new().api_url(&ctx);
        assert_eq!(url, "https://runtime.us-east-1.kiro.dev/");
        assert!(
            !url.contains("generateAssistantResponse"),
            "CLI 协议不用路径寻址操作: {url}"
        );
    }

    /// content-type 必须是 x-amz-json-1.0（X-Amz-Target 路由必需）。
    #[test]
    fn should_use_amz_json_content_type() {
        assert_eq!(
            CliRuntimeEndpoint::new().content_type(),
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

        let out = CliRuntimeEndpoint::new().transform_api_body(
            r#"{"conversationState":{"conversationId":"c1"}}"#,
            &ctx,
        );
        assert!(
            !out.contains("profileArn"),
            "cli-runtime 绝不能注入 profileArn: {out}"
        );
        assert!(out.contains("vibe"), "应注入 agentMode=vibe: {out}");
    }

    /// ⭐ 顺序守卫（deepseek review 补齐）：本文件复制了 cli.rs 的 `inject → origin` 序列，
    /// 但此前没有守卫，cli.rs:673 那份管不到这里。`set_origin_kiro_cli` 第一步是字符串字面量
    /// 替换 `"origin":"AI_EDITOR"`，只对 serde 紧凑序列化成立；inject 必须在它之前，保证它拿到的
    /// 一定是 serde 刚吐出的紧凑串。删掉/颠倒这里的顺序 → 本测试必须 FAILED。
    #[test]
    fn inject_must_run_before_origin_rewrite() {
        let src = include_str!("cli_runtime.rs");
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
