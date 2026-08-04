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
        // ⚠️ 绝不在此设 content-type：Provider 已按 [`KiroEndpoint::content_type`] 设过一次，
        // 而 reqwest 的 `.header()` 语义是 **append 而非 insert**，在此再设会让请求带**两个**
        // content-type 头。生产事故实证（2026-08-04，真实上游抓包）：
        //   发 ["application/json", "application/x-amz-json-1.0"] → 服务端取第一个值 →
        //   HTTP **200** + `{"Output":{"__type":"...#UnknownOperationException"},"Version":"1.0"}`
        // 200 让网关记成功、健康分只升不降，JSON 又被喂进 event-stream 解码器读出
        // `total_length = 2065846133`（即 ASCII `{"Ou`）——与生产日志数字逐位相同。
        // 只发单个正确值时上游正常返回 400 ValidationException。
        req.header(
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

    /// CLI 端点走 AWS JSON 1.0 协议。
    ///
    /// 由 Provider 唯一设置（见 [`KiroEndpoint::content_type`]）：本实现的 `decorate`
    /// 绝不再自己设 content-type，否则请求会带两个值、服务端取第一个（`application/json`）
    /// 而回 200 + Coral `UnknownOperationException` 信封——生产事故根因，已实证。
    fn content_type(&self) -> &'static str {
        "application/x-amz-json-1.0"
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

    /// 回归（生产事故 2026-08-04，根因已实证）：**每个**端点发出的请求都必须恰好
    /// 携带一个 content-type 头。
    ///
    /// 旧实现里 provider 先设 `application/json`，`CliEndpoint::decorate` 又设
    /// `application/x-amz-json-1.0`，而 reqwest 的 `.header()` 是 `append` 而非
    /// `insert` —— 于是请求真的带了两个值。实测上游取**第一个**（`application/json`），
    /// Coral 框架不认这个操作，返回 `UnknownOperationException`，且以 **HTTP 200**
    /// 包在 `{"Output":..,"Version":"1.0"}` 信封里下发：
    ///
    /// - 两个头   → 200 + `{"Ou..` → 记成功、喂进二进制解码器 → “19 亿字节” → 502
    /// - 单个正确头 → 400 + 正常 `ValidationException`
    ///
    /// 现在 content-type 由 [`KiroEndpoint::content_type`] 单一声明、provider 单点设置，
    /// 端点不再自行追加，重复在结构上不可能发生。这个测试遍历所有端点守住该不变量。
    #[test]
    fn every_endpoint_sends_exactly_one_content_type() {
        use crate::kiro::endpoint::{
            AmazonQEndpoint, CodeWhispererEndpoint, IdeEndpoint, KiroEndpoint,
        };

        let credentials = KiroCredentials::default();
        let config = Config::default();
        let endpoints: Vec<Box<dyn KiroEndpoint>> = vec![
            Box::new(CliEndpoint::new()),
            Box::new(IdeEndpoint::new()),
            Box::new(CodeWhispererEndpoint),
            Box::new(AmazonQEndpoint),
        ];
        let client = reqwest::Client::new();

        for endpoint in &endpoints {
            let ctx = RequestContext {
                credentials: &credentials,
                token: "tok",
                machine_id: "machine",
                config: &config,
                is_1m: false,
            };
            // 复刻 provider 的构造顺序：单点设置 endpoint 声明的 content-type。
            let base = client
                .post("https://example.invalid/")
                .body("{}")
                .header("content-type", endpoint.content_type());
            let req = endpoint.decorate_api(base, &ctx).build().unwrap();

            let values: Vec<&str> = req
                .headers()
                .get_all("content-type")
                .iter()
                .map(|v| v.to_str().unwrap())
                .collect();
            assert_eq!(
                values.len(),
                1,
                "端点 {} 发出了 {} 个 content-type ({:?})；重复头会让上游取错值、\
                 以 HTTP 200 返回 Coral 错误信封，进而被记成功并喂进 event-stream 解码器",
                endpoint.name(),
                values.len(),
                values
            );
            assert_eq!(
                values[0],
                endpoint.content_type(),
                "端点 {} 实际发出的 content-type 与其声明不一致",
                endpoint.name()
            );
        }
    }

    /// cli 端点必须声明 AWS JSON 1.0：它 POST 到 `runtime.*.kiro.dev` 的根路径，
    /// 走的是 `x-amz-target` 寻址的 AWS JSON 协议，而非 ide 的 REST 风格路径。
    #[test]
    fn cli_declares_amz_json_content_type() {
        assert_eq!(
            CliEndpoint::new().content_type(),
            "application/x-amz-json-1.0"
        );
    }
}
