//! Kiro IDE 端点
//!
//! 对应 Kiro IDE 客户端目前使用的端点（已随 Kiro 迁移到 kiro.dev）：
//! - API: `https://runtime.{region}.kiro.dev/generateAssistantResponse`
//! - MCP: `https://runtime.{region}.kiro.dev/mcp`
//!
//! # 为什么 IDE 端点用 `runtime.*` 而不是 `q.*`（2026-08-06 更正，别再改回原说法）
//!
//! 本注释此前写「旧的 `q.{region}.amazonaws.com` **已停用**」——**那句无依据且有反证**：
//! 同目录 `cli.rs` 的头注释记载 2026-08 实测四个 `ksk_` 号在 `q.*` 拿到 HTTP 200。
//! `q.*` **没有停用**，它是 **CLI 协议**（Amazon Q Developer CLI）的 host。
//! 那句错断言曾直接导致一次错误的架构决策（改坏 region 探测 → US 号恒 403，上线后回滚）。
//!
//! 仍然用 `runtime.*` 的真实理由是**证据不对称**，不是「对面已停用」：
//! - `runtime.*` 有 **4360 请求 / 99.9% 成功**的 **IDE 协议**实测。
//! - 本仓所有 `q.*` 成功记录**全是 CLI 协议**（服务根 `/` + `X-Amz-Target` 头 +
//!   `tokentype: API_KEY` + 绝不注入 profileArn）。**没有一条**是 IDE 协议路径在 `q.*`
//!   上的实测 —— 两个协议的请求形状根本不同，CLI 的 200 不能外推成 IDE 的 200。
//! - DNS 实测（`dig @1.1.1.1`，绕开本机 fake-IP 代理）：两个域名族**都只在 `us-east-1`
//!   与 `eu-central-1` 解析**，其余 7 区两者都只返 SOA。⇒ 换 host 不解决区域覆盖。
//!
//! ⇒ **要把 IDE 端点迁到 `q.*`，前置条件是先补 IDE 协议在 `q.*` 上的实测**（同号同区，
//! 确认 200 而非 403/400）。在那之前迁移是拿 4360 请求的已知good换一个未验证路径。
//!
//! ## ⚠️ 未解决的矛盾（两条观测冲突，谁都还没被证否）
//!
//! 1. 本仓实测：`runtime.*` **4360 请求 / 99.9% 成功**（IDE 协议，生产流量）。
//! 2. 另一个 kiro 实现的注释（用户多次引用）：
//!    > `runtime.<region>.kiro.dev` serves the same data plane but throttles far harder:
//!    > under load it returns 25-40% 429 where `q.<region>` returns none.
//!    同向的还有本仓 `docs/batch2-region-endpoint-matrix.md` 记的「300 并发 `q.*` 0 个 429
//!    vs `runtime.*` 31%」。
//!
//! **不要替这两条下结论。** 它们可能都对（99.9% 那批是常规负载、429 那批是高并发压测），
//! 也可能有一条口径错（例如把 403 记进了别的桶）。**判定它需要的实验**：
//! **同一个号、同一个 region、同一并发梯度，对打两个 host 比 429 率** —— 且 `q.*` 侧必须
//! 用 CLI 协议形状、`runtime.*` 侧用 IDE 协议形状（否则测的是协议差异不是 host 差异）。
//! 在这个对打实验做出来之前，「哪个 host 更抗限流」是**未知**，不是已知。
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
///
/// # 验证结论(0713 旁挂 8995 黑盒实测,重要)
/// **Kiro 上游(CodeWhisperer 协议)大概率不依赖这个 HTTP 头。** 实测 `claude-opus-4-6`
/// **不带** `[1m]`、**不带**任何 beta 头,64 万 token 输入直接返回 200(input_tokens=640571)——
/// 说明上游本就给足远超 opus「官方 200K」的窗口,不靠此头开 1M。故本头注入是**保留但无害**:
/// 上游认则加成、不认则忽略,绝不破坏正常请求。真正让大窗口生效的是上游本身(按 modelId 给窗口),
/// 不是这个头。`[1m]` 后缀的实际价值 = 给只能传纯模型名的客户端一个显式的 1M 变体名(已验证可用)。
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
            // 用 as_object_mut 而非 `json["profileArn"] = …`：后者是 serde_json 的 IndexMut，
            // 对非对象 JSON（数组/标量）会 panic（"cannot access key ... in JSON"）。
            // 与 cli.rs 的 inject_cli_agent_fields 同款安全模式：非对象原样透传。
            if let Some(obj) = json.as_object_mut() {
                obj.insert("profileArn".to_string(), serde_json::Value::String(arn.clone()));
            }
            if let Ok(body) = serde_json::to_string(&json) {
                return body;
            }
        }
    }
    request_body.to_string()
}

#[cfg(test)]
mod tests {
    use super::{BETA_1M, beta_header_for_1m, inject_profile_arn};
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

    /// 回归：非对象 JSON（数组/标量/字符串）不得 panic，必须原样透传。
    ///
    /// `json["profileArn"] = …` 走 serde_json 的 IndexMut，对非容器值会 panic
    /// （"cannot access key ... in JSON"）。与 cli.rs 的 inject_cli_agent_fields
    /// 同款安全模式：as_object_mut 在非对象上返回 None，不注入即透传。
    #[test]
    fn test_inject_profile_arn_non_object_passthrough() {
        let arn = Some("arn:aws:codewhisperer:us-east-1:1:profile/X".to_string());
        for body in [r#"[{"a":1}]"#, "42", r#""hi""#] {
            assert_eq!(
                inject_profile_arn(body, &arn),
                body,
                "非对象 JSON 必须原样透传且不 panic: {body}"
            );
        }
    }
}
