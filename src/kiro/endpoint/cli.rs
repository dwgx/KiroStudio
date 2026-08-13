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
    /// 形状受 `config.cli_ua_align_real_client` 控制（默认沿用本仓历史形状，零回归）。
    fn user_agent(&self, ctx: &RequestContext<'_>) -> String {
        cli_user_agent_with(ctx.machine_id, ctx.config.cli_ua_align_real_client)
    }
}

/// 真实 `kiro-cli` 抓包里的 codewhispererstreaming API 版本号。
///
/// 取值来源（五方实读）：GreyGunG `0.1.16551`（最新）、ZyphrZero / Foxfishc / M-JYuan
/// `0.1.14474`（Foxfishc 标注抓包日期 2026-05-12 / `kiro-cli 2.3.0`）。取最新那个。
/// 注意真实客户端用 `/` 分隔，而本仓历史形状用 `#1.28.3` —— 见 `cli_user_agent`。
const REAL_CLI_STREAMING_API_VER: &str = "0.1.16551";

/// CLI 协议 UA（`app/AmazonQ-For-CLI` 标识），`cli` / `cli-runtime` / `codewhisperer` /
/// `amazonq` 四个端点共用同一形状（区别于 IDE 的 aws-sdk-js/KiroIDE）。
///
/// `align_real_client` = `config.cli_ua_align_real_client`：
/// - `false`（默认）→ 本仓历史形状，逐字节不变（零回归）。
/// - `true` → 对齐真实客户端抓包形状（`/` 分隔版本号 + `m/F`），且与
///   [`cli_x_amz_user_agent`] 配对使用（两个头发**不同**的串，见该函数说明）。
pub(crate) fn cli_user_agent_with(machine_id: &str, align_real_client: bool) -> String {
    if align_real_client {
        // 真实客户端形状：`api/codewhispererstreaming/{ver}`（斜杠）+ `m/F` +
        // `md/appVersion-...`。machine_id 仍嵌在 appVersion 尾部（本仓既有做法，
        // 四家参考仓在这一段各有出入，且它是我们自己的设备标识载体，保留）。
        format!(
            "aws-sdk-rust/1.0.0 ua/2.1 os/other lang/rust api/codewhispererstreaming/{REAL_CLI_STREAMING_API_VER} m/F app/AmazonQ-For-CLI md/appVersion-1.28.3-{machine_id}"
        )
    } else {
        format!(
            "aws-sdk-rust/1.0.0 ua/2.1 os/other lang/rust api/codewhispererstreaming#1.28.3 m/E app/AmazonQ-For-CLI md/appVersion-1.28.3-{machine_id}"
        )
    }
}

/// 兼容旧调用点：等价于 `cli_user_agent_with(machine_id, false)`（历史形状）。
pub(crate) fn cli_user_agent(machine_id: &str) -> String {
    cli_user_agent_with(machine_id, false)
}

/// `x-amz-user-agent` 头的值。
///
/// 🔴 四家参考实现**都把这个头与 `user-agent` 拆成两个不同的串**：
/// `x-amz-user-agent` 带 `m/F` 但**不带** `md/appVersion-...`，而 `user-agent` 反之。
/// 本仓历史上两个头喂的是同一个串 —— 这本身就是一处指纹差异（比版本号更容易被识别，
/// 因为它是**结构性**的：真实 AWS SDK 生成这两个头的代码路径不同，串必然不同）。
///
/// `align_real_client = false` 时返回 `None`，调用方沿用 `user_agent` 那一串（历史行为，
/// 零回归）；`true` 时返回拆分后的真实形状。
pub(crate) fn cli_x_amz_user_agent(align_real_client: bool) -> Option<String> {
    if align_real_client {
        Some(format!(
            "aws-sdk-rust/1.0.0 ua/2.1 os/other lang/rust api/codewhispererstreaming/{REAL_CLI_STREAMING_API_VER} m/F app/AmazonQ-For-CLI"
        ))
    } else {
        None
    }
}

/// CLI 协议 API 请求装饰（`cli` / `cli-runtime` / `codewhisperer` / `amazonq` 共用）。
///
/// host 与 `X-Amz-Target` 是端点差异，其余头（tokentype / UA / 遥测退出 / agent-mode / 授权）
/// 全部同构。抽成公共函数避免每加一个 host 就复制一份 headers 导致漂移。
pub(crate) fn decorate_cli_protocol(
    req: RequestBuilder,
    ctx: &RequestContext<'_>,
    host: String,
    amz_target: &'static str,
    ua: String,
) -> RequestBuilder {
    // 遥测退出头：默认 "true"（隐私优先，本仓历史行为）。开
    // `cli_codewhisperer_optout_false` 后发 "false" —— 那是四家参考实现一致的真实客户端
    // 值（Foxfishc/M-JYuan 有抓包出处），但**语义是同意上游用会话做训练**，
    // 所以只能由用户显式选择，不由代码替他决定。详见该配置字段的文档注释。
    let optout = if ctx.config.cli_codewhisperer_optout_false {
        "false"
    } else {
        "true"
    };
    // `x-amz-user-agent` 是否与 `user-agent` 拆成不同串（见 cli_x_amz_user_agent）。
    let align_ua = ctx.config.cli_ua_align_real_client;
    let x_amz_ua = cli_x_amz_user_agent(align_ua);

    req.header("X-Amz-Target", amz_target)
        .header("tokentype", "API_KEY")
        .header("x-amzn-codewhisperer-optout", optout)
        .header("x-amzn-kiro-agent-mode", "vibe")
        .header(
            "x-amz-user-agent",
            x_amz_ua.as_deref().unwrap_or(ua.as_str()),
        )
        .header("user-agent", &ua)
        .header("host", host)
        .header("amz-sdk-invocation-id", Uuid::new_v4().to_string())
        // ⚠️ TODO(指纹未完全对齐)：真实 AWS SDK 这个头的 attempt 会**随重试递增**
        // （kiro2cc 实现为 `attempt={n+1}; max=3`），而这里 attempt 恒为 1。
        // ⇒ 即便开了 `cli_ua_align_real_client`，「attempt 永远是 1」本身仍是一个
        // 可被上游识别的指纹（真实客户端重试时该值会变）。
        //
        // 没有现在就改的原因：要拿到当前重试轮次，必须把它透传进 `RequestContext`
        // 或 `decorate_api` 签名 —— 那会动 `KiroEndpoint` trait 的公共签名与全部 5 个
        // 端点实现，属独立改动。四家参考仓也都写死 attempt=1（只有 kiro2cc 递增），
        // 所以对齐它们不需要这一步；要对齐**真实 SDK** 才需要。
        //
        // max 的取值同理待定：本仓 `max=1`，四家参考仓一致 `max=3`。这个头是**告知
        // 上游** SDK 层的重试上限声明，不改变网关自身行为（网关实际上限是
        // `provider.rs::ABSOLUTE_MAX_TOTAL_RETRIES = 4`），所以它纯粹是指纹项。
        // 未随 `cli_ua_align_real_client` 一起改，是因为该开关的语义限定在 UA 形状；
        // 把不相关的头塞进同一个开关会让 A/B 结果无法归因。
        .header("amz-sdk-request", "attempt=1; max=1")
        .header("Authorization", format!("Bearer {}", ctx.token))
    // 刻意不注入 profileArn / anthropic-beta：API_KEY 认证不使用 profileArn；
    // CLI 端点的 1M 窗口由上游按 modelId 决定，不依赖 anthropic-beta 头。
}

/// CLI 协议 MCP 请求装饰（与 [`decorate_cli_protocol`] 同款复用；MCP 不走 X-Amz-Target 路由）。
pub(crate) fn decorate_cli_mcp(
    req: RequestBuilder,
    ctx: &RequestContext<'_>,
    host: String,
    ua: String,
) -> RequestBuilder {
    req.header("tokentype", "API_KEY")
        .header("x-amz-user-agent", &ua)
        .header("user-agent", &ua)
        .header("host", host)
        .header("amz-sdk-invocation-id", Uuid::new_v4().to_string())
        .header("amz-sdk-request", "attempt=1; max=1")
        .header("Authorization", format!("Bearer {}", ctx.token))
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

    fn amz_target(&self) -> Option<&'static str> {
        Some(CLI_AMZ_TARGET)
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
        decorate_cli_protocol(req, ctx, self.host(ctx), CLI_AMZ_TARGET, self.user_agent(ctx))
    }

    fn decorate_mcp(&self, req: RequestBuilder, ctx: &RequestContext<'_>) -> RequestBuilder {
        decorate_cli_mcp(req, ctx, self.host(ctx), self.user_agent(ctx))
    }

    fn transform_api_body(&self, body: &str, ctx: &RequestContext<'_>) -> String {
        // CLI 协议：注入 agentTaskType/agentMode="vibe"，**绝不**注入 profileArn。
        let body = inject_cli_agent_fields(body);

        // 🔴 顺序承重：vibe 注入必须在 set_origin_kiro_cli **之前**。
        //
        // 反过来的话，注入那步会把已改好的 body 重新 `to_string` 一遍 —— 那本身无害，
        // 但 `set_origin_kiro_cli` 的第一步是**字符串**替换 `"origin":"AI_EDITOR"`，
        // 它只对 serde 的紧凑输出成立。让它拿到"刚被 serde 序列化过"的 body 是这个
        // 前提最结实的形态；若排在注入之前，它拿到的是上游调用方给的原始串，
        // 一旦哪天那串带了空格/缩进（如有人加了 to_string_pretty 的调试分支），
        // 字面量就静默不命中而**其余两步照做** = 半套变更上线且没人看得见。
        //
        // 开关关闭时这里逐字节等价旧行为（只有 inject 一步）。
        //
        // 门禁走 `effective_cli_origin_kiro_cli`：凭据级 `cli_origin_kiro_cli` 优先，
        // 未设时回落全局 `ctx.config.cli_origin_kiro_cli`。这是唯一改动点 —— 全局开关
        // 一开就是全池切换，做不到「单号开、对比 429 率」的 A/B；字段范式照抄
        // `custom_api_first`（凭据级 `Option<bool>` 覆盖 + 全局兜底）。
        if ctx
            .credentials
            .effective_cli_origin_kiro_cli(ctx.config.cli_origin_kiro_cli)
        {
            return set_origin_kiro_cli(&body);
        }
        body
    }
}

/// 给请求体注入 CLI 协议字段：`conversationState.agentTaskType="vibe"` + 顶层 `agentMode="vibe"`。
/// 解析失败时原样返回（与上游宽松，实测缺这两字段也 200，故不因注入失败而破坏请求）。
///
/// `pub(crate)`：`cli-runtime` 端点（`runtime.{region}.kiro.dev` 的 CLI 协议）复用同一套 body 加工。
pub(crate) fn inject_cli_agent_fields(request_body: &str) -> String {
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

/// 把请求体改成**真实 Kiro CLI 客户端**的形状（对齐 kiro-rs 的 `set_origin_kiro_cli`）。
/// 仅在 `config.cliOriginKiroCli = true` 时调用，默认关。
///
/// 三步（与 kiro-rs 逐条对应）：
/// 1. 所有 `"origin":"AI_EDITOR"` → `"origin":"KIRO_CLI"`；
/// 2. 删 `conversationState.agentContinuationId`（CLI 客户端不发这个字段）；
/// 3. 删 history 里每条 `userInputMessage.modelId`（CLI 只在 currentMessage 带 modelId）。
///
/// 【为什么三步捆在一个开关里】它们是同一件事的三个面 —— "这条请求看起来像谁发的"。
/// kiro-rs 是三步一起发且实测无 429，拆成三个开关会让第一轮 A/B 变成 8 种组合。
/// 若开着仍 429，**下一步才是**逐条二分（先只留第 1 步）。
///
/// 【第 1 步为何用字符串替换而非遍历 JSON】刻意与 kiro-rs 一致：`origin` 在 body 里出现
/// 在 `currentMessage.userInputMessage` 与 history 每条消息下，共 N 处；字符串替换一次覆盖
/// 全部，遍历要写两处递归且随 converter 的结构演进而漂。代价是依赖紧凑序列化，
/// 已由调用点的顺序保证（见 `transform_api_body` 的顺序说明）。
///
/// 解析失败时返回**已做过字符串替换**的 body（而非原样）：第 1 步不需要合法 JSON，
/// 已经生效的部分没有理由回退。
///
/// `pub(crate)`：`cli-runtime` 端点复用同一套 origin 改写。
pub(crate) fn set_origin_kiro_cli(request_body: &str) -> String {
    let replaced = request_body.replace(r#""origin":"AI_EDITOR""#, r#""origin":"KIRO_CLI""#);

    let Ok(mut json) = serde_json::from_str::<serde_json::Value>(&replaced) else {
        return replaced;
    };

    if let Some(state) = json
        .get_mut("conversationState")
        .and_then(|v| v.as_object_mut())
    {
        state.remove("agentContinuationId");

        if let Some(history) = state.get_mut("history").and_then(|v| v.as_array_mut()) {
            for msg in history.iter_mut() {
                if let Some(user_input) = msg
                    .get_mut("userInputMessage")
                    .and_then(|v| v.as_object_mut())
                {
                    user_input.remove("modelId");
                }
            }
        }
    }

    serde_json::to_string(&json).unwrap_or(replaced)
}

// ─────────────────────────────────────────────────────────────────────────────
// 与 kiro-rs CLI 端点其余六处差异（本轮**刻意不动**）
//
// 本轮只动 `origin` 一项（最可疑那条）。一次改七项，失败了不知道是哪项 —— 而每次
// A/B 都要用线上流量换数据，没法重来。下面逐条记差异与"为什么现在不动"，
// 供 `origin` 那轮出结论后按顺序二分：
//
// 1. `x-amzn-codewhisperer-optout`：本仓 `true`，kiro-rs `false`。
//    这是**遥测退出**标志，语义上与配额无关；且改成 false 等于把用户会话交给上游做训练，
//    属隐私默认值变更，不该跟一次限流实验一起悄悄改。
// 2. `x-amzn-kiro-agent-mode: vibe`：本仓发，kiro-rs 只在 IDE 端点发。
//    多发一个上游可能不认的头，最坏是被忽略；它与 body 里的 `agentMode` 重复，
//    真要清理应连 body 一起，属独立一轮。
// 3. `amz-sdk-request`：本仓 `attempt=1; max=1`，kiro-rs `attempt=1; max=3`。
//    这是 AWS SDK 自报的**客户端重试预算**，纯声明性（我们自己不按它重试）。
//    若上游据此做退避判断，改它会与网关内部的 failover/吸收层记账重叠 —— 要改得先
//    想清楚两套重试语义谁说了算，不是一行头的事。
// 4. UA 形状：本仓把 `machine_id` 嵌进 `md/appVersion-...`，kiro-rs 用
//    `config.system_version` + `config.kiro_version` 且不含 machineId。
//    machineId 进 UA 是本仓**刻意**的多号隔离手段（同一 UA 跨号会给上游关联线索），
//    与 kiro-rs 单号场景不同，不能照抄。
// 5. `tokentype`：本仓无条件发 `API_KEY`，kiro-rs 走 `credentials.token_type_header()`。
//    本仓 CLI 端点只服务 `ksk_` 号，两者结果等价，改动为零收益。
// 6. `mcp_url`：本仓服务根 `/`，kiro-rs `/mcp`。CLI 号目前不走 MCP（WebSearch 只在 IDE
//    路径），改它无法被任何现有流量验证 —— 没有观测就不改。
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::{
        CliEndpoint, REAL_CLI_STREAMING_API_VER, cli_user_agent, cli_user_agent_with,
        cli_x_amz_user_agent, inject_cli_agent_fields, set_origin_kiro_cli,
    };
    use serde_json::Value;

    /// UA 开关关闭时**逐字节**等于历史形状（零回归）。
    ///
    /// 这条是承重的：默认关意味着线上行为一个比特都不能变，否则「加开关」本身就成了
    /// 一次未经 A/B 的全池切换 —— 那正是这个开关要避免的事。
    #[test]
    fn ua_switch_off_is_byte_identical_to_legacy() {
        let mid = "test-machine-id";
        assert_eq!(cli_user_agent_with(mid, false), cli_user_agent(mid));
        let legacy = cli_user_agent(mid);
        assert!(legacy.contains("api/codewhispererstreaming#1.28.3"), "{legacy}");
        assert!(legacy.contains(" m/E "), "历史形状用 m/E：{legacy}");
        // 关闭时 x-amz-user-agent 不拆串（返回 None ⇒ 调用方沿用 user-agent 那一串）。
        assert_eq!(cli_x_amz_user_agent(false), None);
    }

    /// UA 开关打开时对齐真实客户端抓包形状：`/` 分隔版本号 + `m/F` + 两个头拆开。
    #[test]
    fn ua_switch_on_matches_real_client_shape() {
        let mid = "test-machine-id";
        let aligned = cli_user_agent_with(mid, true);
        assert!(
            aligned.contains(&format!(
                "api/codewhispererstreaming/{REAL_CLI_STREAMING_API_VER}"
            )),
            "真实客户端用 / 分隔版本号：{aligned}"
        );
        assert!(!aligned.contains('#'), "不得再出现 # 分隔：{aligned}");
        assert!(aligned.contains(" m/F "), "真实客户端用 m/F：{aligned}");
        assert!(
            aligned.contains(mid),
            "machineId 仍须嵌在 appVersion 尾部：{aligned}"
        );

        // 🔴 关键差异：两个头必须是**不同**的串。
        // `x-amz-user-agent` 带 m/F 但不带 appVersion；`user-agent` 反之带 appVersion。
        let x = cli_x_amz_user_agent(true).expect("开启时必须给出拆分后的串");
        assert_ne!(x, aligned, "四家参考实现都把这两个头拆成不同的串");
        assert!(x.contains(" m/F"), "{x}");
        assert!(
            !x.contains("md/appVersion"),
            "x-amz-user-agent 不带 appVersion：{x}"
        );
        assert!(
            aligned.contains("md/appVersion"),
            "user-agent 带 appVersion：{aligned}"
        );
    }

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

    // ═══════════════════════════════════════════════════════════════════════
    // `cliOriginKiroCli` 开关（对齐 kiro-rs 的 body 形状）
    //
    // ⚠️ 这组测试**必须**走 `transform_api_body`，不能只测 `set_origin_kiro_cli` 本身：
    // 后者是纯函数，把 `transform_api_body` 里那句 `if ctx.config.cli_origin_kiro_cli`
    // 整段删掉，纯函数测试照样全绿而线上一个字节都不会变。本仓「纸面测试」第 8 种形态
    // （测了分支内部、没测分支是否被走到）就是这么发生的。
    // ═══════════════════════════════════════════════════════════════════════

    /// 造一份带 history 的真实形状 body（converter 的实际输出形状：
    /// currentMessage 与 history 每条都带 `origin`/`modelId`，conversationState 带
    /// `agentContinuationId`）。
    fn sample_body() -> &'static str {
        r#"{"conversationState":{"conversationId":"c1","agentContinuationId":"a1","chatTriggerType":"MANUAL","history":[{"userInputMessage":{"content":"q1","modelId":"claude-sonnet-4","origin":"AI_EDITOR"}},{"assistantResponseMessage":{"content":"a1"}},{"userInputMessage":{"content":"q2","modelId":"claude-sonnet-4","origin":"AI_EDITOR"}}],"currentMessage":{"userInputMessage":{"content":"q3","modelId":"claude-sonnet-4","origin":"AI_EDITOR"}}}}"#
    }

    fn transform_with_switch(body: &str, on: bool) -> Value {
        use super::super::{KiroEndpoint, RequestContext};
        use crate::kiro::model::credentials::KiroCredentials;
        use crate::model::config::Config;

        let mut cred = KiroCredentials::default();
        cred.kiro_api_key = Some("ksk_test".to_string());
        let mut config = Config::default();
        config.cli_origin_kiro_cli = on;
        let ctx = RequestContext {
            credentials: &cred,
            token: "ksk_test",
            machine_id: "mid",
            config: &config,
            is_1m: false,
        };
        let out = CliEndpoint::new().transform_api_body(body, &ctx);
        serde_json::from_str(&out).unwrap_or_else(|e| panic!("输出应是合法 JSON: {e}\n{out}"))
    }

    /// ① 开关**关**（默认）时，body 必须与开关引入之前逐字节等价：
    /// `origin` 仍 `AI_EDITOR`、`agentContinuationId` 仍在、history 的 `modelId` 仍在、
    /// vibe 两字段仍注入。
    ///
    /// 这条是「默认关」这个硬约束的守卫：线上 17 个号正在服务，升级不得改变任何在途行为。
    #[test]
    fn switch_off_keeps_body_byte_identical_to_legacy() {
        let json = transform_with_switch(sample_body(), false);

        assert_eq!(
            json["conversationState"]["currentMessage"]["userInputMessage"]["origin"], "AI_EDITOR",
            "开关关闭时 origin 必须保持 AI_EDITOR"
        );
        assert_eq!(
            json["conversationState"]["agentContinuationId"], "a1",
            "开关关闭时 agentContinuationId 必须保留（它带着上游 prefix 缓存折扣）"
        );
        assert_eq!(
            json["conversationState"]["history"][0]["userInputMessage"]["modelId"],
            "claude-sonnet-4",
            "开关关闭时 history 的 modelId 必须保留"
        );
        // vibe 两字段不受开关影响（它们是差异表第 4 项，本轮不动）。
        assert_eq!(json["agentMode"], "vibe");
        assert_eq!(json["conversationState"]["agentTaskType"], "vibe");

        // 逐字节等价的正面证明：与只跑 inject 的旧路径输出完全相同。
        let legacy = inject_cli_agent_fields(sample_body());
        let legacy_json: Value = serde_json::from_str(&legacy).unwrap();
        assert_eq!(json, legacy_json, "开关关闭时必须与旧路径输出完全一致");
    }

    /// ② 开关**开**时三步必须都生效：`origin` → `KIRO_CLI`、`agentContinuationId` 删除、
    /// history 里每条 `userInputMessage.modelId` 删除。
    ///
    /// 【回退即 FAIL】把 `transform_api_body` 里那段
    /// `if ctx.config.cli_origin_kiro_cli { return set_origin_kiro_cli(&body); }` 删掉
    /// （或把条件写死 false）→ 本测试的第一条断言就 FAILED。已实测验证。
    #[test]
    fn switch_on_sends_kiro_cli_shaped_body() {
        let json = transform_with_switch(sample_body(), true);
        let cs = &json["conversationState"];

        // 步骤 1：所有 origin 都得改，包括 history 里的（字符串替换是全局的）。
        assert_eq!(
            cs["currentMessage"]["userInputMessage"]["origin"], "KIRO_CLI",
            "currentMessage 的 origin 必须是 KIRO_CLI"
        );
        for i in [0usize, 2] {
            assert_eq!(
                cs["history"][i]["userInputMessage"]["origin"], "KIRO_CLI",
                "history[{i}] 的 origin 也必须改（AI_EDITOR 残留一处就等于自报是 IDE）"
            );
        }
        assert!(
            !serde_json::to_string(&json).unwrap().contains("AI_EDITOR"),
            "整个 body 里不得残留 AI_EDITOR"
        );

        // 步骤 2：agentContinuationId 删除。
        assert!(
            cs.get("agentContinuationId").is_none(),
            "agentContinuationId 必须删除（真实 CLI 客户端不发这个字段）"
        );

        // 步骤 3：history 的 modelId 删除，但 currentMessage 的**保留**
        //（真实 CLI 只在当前消息带 modelId；把它一起删会让上游不知道要哪个模型）。
        for i in [0usize, 2] {
            assert!(
                cs["history"][i]["userInputMessage"]
                    .get("modelId")
                    .is_none(),
                "history[{i}] 的 modelId 必须删除"
            );
        }
        assert_eq!(
            cs["currentMessage"]["userInputMessage"]["modelId"], "claude-sonnet-4",
            "currentMessage 的 modelId 必须保留 —— 删了上游不知道用哪个模型"
        );

        // 无关字段不得被顺带改动。
        assert_eq!(cs["conversationId"], "c1");
        assert_eq!(cs["chatTriggerType"], "MANUAL");
        assert_eq!(
            cs["history"][1]["assistantResponseMessage"]["content"],
            "a1"
        );
    }

    /// ③ 开关开时**仍绝不注入 profileArn**（CLI 铁律：API_KEY 带 ARN 会 403）。
    /// 上面那条 `should_never_inject_profile_arn_even_when_credential_has_one` 只覆盖
    /// 开关关的路径，新增分支必须同样受这条铁律约束。
    #[test]
    fn switch_on_still_never_injects_profile_arn() {
        use super::super::{KiroEndpoint, RequestContext};
        use crate::kiro::model::credentials::KiroCredentials;
        use crate::model::config::Config;

        let mut cred = KiroCredentials::default();
        cred.kiro_api_key = Some("ksk_test".to_string());
        cred.profile_arn = Some("arn:aws:codewhisperer:us-east-1:999:profile/OWN".to_string());
        let mut config = Config::default();
        config.cli_origin_kiro_cli = true;
        let ctx = RequestContext {
            credentials: &cred,
            token: "ksk_test",
            machine_id: "mid",
            config: &config,
            is_1m: false,
        };

        let out = CliEndpoint::new().transform_api_body(sample_body(), &ctx);
        assert!(
            !out.contains("profileArn"),
            "开关开启时同样绝不能注入 profileArn，实际: {out}"
        );
        assert!(
            !out.contains("arn:aws:codewhisperer"),
            "凭据自带的 ARN 也不得漏进 body: {out}"
        );
        // 开关确实生效（否则这条测试会在"什么都没做"的情况下也通过）。
        assert!(out.contains("KIRO_CLI"), "开关开启的证据");
    }

    /// 开关默认必须是**关**。这是本轮最硬的约束：默认开等于升级即把 17 个正在服务的号
    /// 全池切到一个未验证的上游协议形状。
    ///
    /// 回退即 FAIL：把字段默认改成 true，或把 `#[serde(default)]` 换成
    /// `#[serde(default = "…true…")]`。
    #[test]
    fn switch_defaults_off_including_absent_field() {
        use crate::model::config::Config;
        assert!(
            !Config::default().cli_origin_kiro_cli,
            "默认必须关：线上号池正在服务，不做未验证的全池协议切换"
        );
        let absent: Config = serde_json::from_str("{}").expect("缺字段必须能反序列化");
        assert!(
            !absent.cli_origin_kiro_cli,
            "旧 config.json 缺该字段时必须落在关的一侧"
        );
        // 显式开启必须被尊重，否则这个开关等于不存在（无启用途径）。
        let on: Config = serde_json::from_str(r#"{"cliOriginKiroCli":true}"#)
            .expect("camelCase 键必须能反序列化");
        assert!(on.cli_origin_kiro_cli, "面板/配置显式开启必须生效");
    }

    // ═══════════════════════════════════════════════════════════════════════
    // 凭据级 `cliOriginKiroCli` 覆盖（全局一开就是全池切换，做不到「单号开、
    // 对比 429 率」的 A/B；字段范式照抄 `custom_api_first`：凭据级 `Option<bool>`
    // 优先，`None` 时回落全局 `config.cliOriginKiroCli`）。
    //
    // 四种组合：凭据级 true / 凭据级 false / 未设回落全局 true / 未设回落全局 false。
    // 同上面那组一样，**必须**走 `transform_api_body`（而非只测
    // `effective_cli_origin_kiro_cli` 本身），否则测的是分支内部、不是分支是否被走到。
    // ═══════════════════════════════════════════════════════════════════════

    /// 与 `transform_with_switch` 同构，但额外接受凭据级覆盖值（`None`=未设）。
    fn transform_with_cred_override(
        body: &str,
        global_on: bool,
        cred_override: Option<bool>,
    ) -> Value {
        use super::super::{KiroEndpoint, RequestContext};
        use crate::kiro::model::credentials::KiroCredentials;
        use crate::model::config::Config;

        let mut cred = KiroCredentials::default();
        cred.kiro_api_key = Some("ksk_test".to_string());
        cred.cli_origin_kiro_cli = cred_override;
        let mut config = Config::default();
        config.cli_origin_kiro_cli = global_on;
        let ctx = RequestContext {
            credentials: &cred,
            token: "ksk_test",
            machine_id: "mid",
            config: &config,
            is_1m: false,
        };
        let out = CliEndpoint::new().transform_api_body(body, &ctx);
        serde_json::from_str(&out).unwrap_or_else(|e| panic!("输出应是合法 JSON: {e}\n{out}"))
    }

    /// ① 凭据级 `Some(true)`：即使全局关，该号也必须按 KIRO_CLI 形状发送。
    /// 这是单号 A/B 能力本身——不开全局开关也能只让一个号试。
    #[test]
    fn credential_override_true_wins_even_when_global_off() {
        let json = transform_with_cred_override(sample_body(), false, Some(true));
        assert_eq!(
            json["conversationState"]["currentMessage"]["userInputMessage"]["origin"], "KIRO_CLI",
            "凭据级 true 必须覆盖全局 false，origin 改写生效"
        );
        assert!(
            json["conversationState"]
                .get("agentContinuationId")
                .is_none(),
            "凭据级开启时 agentContinuationId 必须删除"
        );
        assert!(
            !serde_json::to_string(&json).unwrap().contains("AI_EDITOR"),
            "整个 body 不得残留 AI_EDITOR"
        );
    }

    /// ② 凭据级 `Some(false)`：即使全局开，该号也必须保持旧形状——这是「留一个号不动
    /// 当对照组」的能力，做 A/B 时缺了这条就没有基线可比。
    #[test]
    fn credential_override_false_wins_even_when_global_on() {
        let json = transform_with_cred_override(sample_body(), true, Some(false));
        assert_eq!(
            json["conversationState"]["currentMessage"]["userInputMessage"]["origin"], "AI_EDITOR",
            "凭据级 false 必须覆盖全局 true，origin 保持不变"
        );
        assert_eq!(
            json["conversationState"]["agentContinuationId"], "a1",
            "凭据级关闭时 agentContinuationId 必须保留"
        );
        assert_eq!(
            json["conversationState"]["history"][0]["userInputMessage"]["modelId"],
            "claude-sonnet-4",
            "凭据级关闭时 history 的 modelId 必须保留"
        );
    }

    /// ③ 凭据级未设（`None`）+ 全局 true：必须回落全局，按 KIRO_CLI 形状发送。
    #[test]
    fn credential_unset_falls_back_to_global_on() {
        let json = transform_with_cred_override(sample_body(), true, None);
        assert_eq!(
            json["conversationState"]["currentMessage"]["userInputMessage"]["origin"], "KIRO_CLI",
            "凭据级未设时必须回落全局 true"
        );
        assert!(
            json["conversationState"]
                .get("agentContinuationId")
                .is_none(),
            "回落全局开启时 agentContinuationId 必须删除"
        );
    }

    /// ④ 凭据级未设（`None`）+ 全局 false：必须回落全局，保持旧形状——
    /// 这条钉住「默认关」在有了凭据级覆盖之后依然是默认行为，不会被新字段意外改变。
    #[test]
    fn credential_unset_falls_back_to_global_off() {
        let json = transform_with_cred_override(sample_body(), false, None);
        assert_eq!(
            json["conversationState"]["currentMessage"]["userInputMessage"]["origin"], "AI_EDITOR",
            "凭据级未设时必须回落全局 false"
        );
        assert_eq!(
            json["conversationState"]["agentContinuationId"], "a1",
            "回落全局关闭时 agentContinuationId 必须保留"
        );
    }

    /// ⭐ 顺序守卫：`inject_cli_agent_fields` 必须排在 `set_origin_kiro_cli` **之前**。
    ///
    /// 为什么单独立一条：这两步的**结果**在当前 body 形状下与顺序无关（vibe 字段里不含
    /// `origin`），所以上面那些行为断言对顺序**完全不可见** —— 正是本仓踩过的
    /// 「测了分支内部、没测分支顺序」那个坑。而顺序在这里是承重的：
    /// `set_origin_kiro_cli` 第一步是字符串字面量替换 `"origin":"AI_EDITOR"`，只对紧凑
    /// 序列化成立；排在 inject 之后才能保证它拿到的一定是 serde 刚吐出的紧凑串。
    /// 反过来则依赖调用方给的原始串永远紧凑 —— 一旦不是，替换静默失效而其余两步照做，
    /// 上线的是**半套**变更且面板上看不见。
    #[test]
    fn inject_must_run_before_origin_rewrite() {
        let src = include_str!("cli.rs");
        let prod = src
            .split("#[cfg(test)]")
            .next()
            .expect("生产段应存在")
            .lines()
            .filter(|l| !l.trim_start().starts_with("//") && !l.trim_start().starts_with("///"))
            .collect::<Vec<_>>()
            .join("\n");
        // needle 运行时拼接，避免 include_str! 自匹配本行。
        // 只匹配到左括号、**不含实参**：实参写法（`body` / `&body` / `&pre`）是重构就会变的
        // 噪声，钉住它会让守卫在真正的顺序倒置之外先因改名而 FAIL —— 那种 FAIL 的报错信息
        // 指不到根因（实测：倒序验证时它报的是"必须仍调 inject"而非"顺序错"）。
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
        // 开关必须是 origin 改写的**前置条件**（而非改完再判），否则默认关的语义不成立。
        let gate = ["ctx.config.cli_origin", "_kiro_cli"].concat();
        let g = prod
            .find(gate.as_str())
            .expect("origin 改写必须由 config 开关把门");
        assert!(g < o, "开关判定必须排在 set_origin_kiro_cli 调用之前");
    }

    /// 纯函数层面的边界：非法 JSON 时第 1 步（字符串替换）已生效的部分不回退，
    /// 后两步跳过。上游对 body 宽松，不该因为解析失败而丢掉已做对的改写。
    #[test]
    fn origin_rewrite_survives_unparseable_body() {
        let broken = r#"{"conversationState":{"origin":"AI_EDITOR","truncated"#;
        let out = set_origin_kiro_cli(broken);
        assert!(out.contains(r#""origin":"KIRO_CLI""#), "字符串替换应已生效");
        assert!(!out.contains("AI_EDITOR"));
        // 完全无 origin 的 body 原样返回（不凭空造字段）。
        assert_eq!(
            set_origin_kiro_cli(r#"{"conversationState":{}}"#),
            r#"{"conversationState":{}}"#
        );
    }
}
