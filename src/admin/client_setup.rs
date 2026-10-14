//! 客户端一键导入（cc-switch 深链接）——只读探测 → 计划 → 交付，服务端零写入。
//!
//! # 这是什么
//!
//! 产出 `ccswitch://v1/import?...` 深链接：用户在面板点一下，CC Switch 桌面端弹确认框，
//! 把本网关当成一个「供应商」导入（Claude Code 与 Codex 各一条），不必手抄
//! `ANTHROPIC_BASE_URL` / key / 模型名。协议与参数以 CC Switch 官方文档为准
//! （`docs/user-manual/*/5-faq/5.3-deeplink.md` 参数表 +
//! `docs/guides/deeplink-site-integration-guide-*.md` 站点接入指南，后者明确要求
//! 「链接由**站点后端**按用户自己的 key 生成，别放公开页」——本端点正是那个后端）。
//! 参数拼法参考生态实现 TsinHzl/kiro2cc-proxy `web-ui/*/src/lib/ccswitch.ts`（MIT；
//! 只借「扁平 URL 参数、不用 `config=` 内嵌 TOML」这一条结论，代码为 Rust 重写）。
//!
//! # 事务规格的来源与本仓映射（kiro-lb `client_setup.rs`，AGPL——只抄设计不抄代码）
//!
//! 原规格：**只读探测 → 计划 → 条件提交 → journal → 幂等 restore**；非 Linux fail-closed。
//! 本仓逐条对应，以及**为什么后两段没有移植**：
//!
//! - **只读探测**：[`probe`] 由**一次**号池快照 + 模型目录算出就绪度；不发起任何上游调用
//!   （不产生计费推理请求），任一前提不满足就不产出任何东西（对齐原规格「写前只做只读探测，
//!   任一失败则一个字节都不写」）。
//! - **计划**：[`plan_imports`] 是纯函数（输入 → 链接，不碰 IO、不碰状态），且**自身复检
//!   就绪度**——未就绪的探测永远产不出计划。这正是原规格「条件提交」的语义：
//!   不满足条件就不提交。
//! - **条件提交**：探测与计划取自**同一次**快照，两者之间不存在可被并发修改的窗口
//!   （原规格用 `renameat2(RENAME_EXCHANGE)` 事后发现「计划后被改动」；本仓把窗口消掉）。
//! - **journal / 幂等 restore / RENAME_EXCHANGE**：**未移植**。它们服务的是「写**用户客户端**的
//!   配置文件」——而本仓的交付物是深链接：文件写入发生在用户机器上、由 CC Switch 自己完成
//!   （它自带导入确认对话框、导入后可再切换/删除）。服务端一个字节都不写 ⇒ 无 journal 可记、
//!   无 restore 可回滚。真正需要这套机制的是「网关本机直接改写 `~/.claude/settings.json`」那类
//!   功能，本仓当前不提供：网关常在容器/远端（写的是容器内 home，不是用户的），
//!   且 Windows 没有 `RENAME_EXCHANGE`，原规格在该平台 fail-closed ⇒ 功能直接不可用。
//!
//! # 安全边界（必读）
//!
//! 响应里的 `link` **含网关对话密钥明文**——CC Switch 对无 key 的链接直接拒绝导入
//! （官方文档 Rejected links 表首行 `API key is required`），这是协议硬要求。故：
//!
//! - 端点只挂在 admin 鉴权树内（`admin_auth_middleware` 统一鉴权）；
//! - 响应带 `Cache-Control: no-store`（同 `export_kam_credentials` 的 MINOR-4：
//!   共享代理/浏览器不留副本；**含 400/409 早退**——缓存策略不随状态码分叉，
//!   否则每条早退分支都要重新判断「这个体里有没有密钥」，那是会漂移的隐性契约）；
//! - 本模块**不打印链接原文**（访问本身由 admin 审计中间件按方法/路径/IP 记录，
//!   不需要在日志里再留一份含密钥的链接）。
//!
//! 面板「接入信息」页对**复制粘贴片段**仍用 `<API_KEY>` 占位符（那类片段会被贴进
//! shell 历史 / `~/.zshrc` / issue，泄露面完全不同）；深链接是交给操作系统的协议跳转，
//! 两者不冲突。

use std::collections::HashMap;

use axum::{
    extract::{Query, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Json, Response},
};

use super::middleware::AdminState;
use super::types::AdminErrorResponse;

// ============================================================================
// 协议常量
// ============================================================================

/// cc-switch 深链接协议头。`v1` 是协议版本、`/import` 是固定路径，
/// 两者任一写错整条链接都会被拒（官方文档「Link structure」）。
const CCSWITCH_IMPORT_PREFIX: &str = "ccswitch://v1/import";

/// 导入 Codex 时写进 `config.toml` 的默认模型。
///
/// 取 GPT 三并列变体里 1.0x 基准倍率的 `terra`（`sol` 2.4x / `luna` 0.1x 是并列变体、
/// 无语义默认值，倍率见 `model_catalog.rs` 的 GPT 段注释）；生态参考实现同取 terra。
/// 不填的后果是 CC Switch 按自家默认值填 `gpt-5-codex`——那个名字不在本仓模型目录里，
/// 导入完第一个请求就 `INVALID_MODEL_ID`。守卫测试钉住它必须仍在目录里**被广告**，
/// 目录改名而这里不改会立刻红。
const CODEX_DEFAULT_MODEL: &str = "gpt-5.6-terra";

/// 供应商卡片上的备注（cc-switch 的 `notes` 参数，显示在卡片上）。
const PROVIDER_NOTES: &str = "KiroStudio 网关（模型由号池提供）";

// ============================================================================
// 探测与计划（纯函数，无 IO）
// ============================================================================

/// 一键导入的目标 app。每个目标一条链接——cc-switch 一次只处理一条链接
/// （官方文档：连点两条只有最后一条生效），故由前端/运维逐条点击。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ClientTarget {
    /// Claude Code（`app=claude`）：anthropic 格式，endpoint **不带** `/v1`——
    /// 该 app 自己会拼 `/v1/messages`（官方文档「Write the endpoint following the
    /// convention of the chosen format」）。
    CcSwitchClaude,
    /// Codex（`app=codex`）：openai_responses 格式，endpoint **带** `/v1`
    /// （Codex 的 base_url 约定，它自己拼 `/responses`）。
    CcSwitchCodex,
}

/// 就绪度探测结果。
///
/// 判据只有三条，全部来自本进程内存（零上游调用）：
/// ① 号池有**可用**凭据；② 模型目录有**被广告**的模型；③ 网关对话密钥已配置。
/// 任一不满足 ⇒ [`Self::ready`] 为 false，且 [`Self::blockers`] 逐条点名原因——
/// 面板要能直接告诉运维「差哪一步」，而不是一个笼统的 409。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SetupProbe {
    /// 号池凭据总数（含禁用）。
    pub pool_total: usize,
    /// 号池可用（未禁用）凭据数。与 `/healthz` 的 `pool_count` 同源同义（后者取 total）。
    pub pool_available: usize,
    /// 模型目录里被 `/v1/models` 广告的模型数（与 `/v1/models` 同一判据）。
    pub usable_models: usize,
    /// 网关对话密钥（`config.json` 的 `apiKey`）是否已配置且非空白。
    pub api_key_present: bool,
    /// 三条判据是否全过。
    pub ready: bool,
    /// 未通过的原因（人类可读，逐条对应上面三条判据）。ready=true 时为空。
    pub blockers: Vec<String>,
}

/// 单个目标的一条导入计划。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientImportPlan {
    /// 目标（枚举形态，前端可穷举判读）。
    pub target: ClientTarget,
    /// cc-switch 的 `app` 参数原文（`claude` / `codex`），回显便于排障与手抄。
    pub app: &'static str,
    /// 供应商显示名。**带 host**：cc-switch 不按名字去重（同名重复导入会多出卡片），
    /// 多网关共存时带 host 才分得清哪张卡指向哪台机器。
    pub name: String,
    /// 写进客户端的 API 地址。
    pub endpoint: String,
    /// 协议格式（cc-switch 的 `apiFormat`）。
    pub api_format: &'static str,
    /// 默认模型（仅 Codex 有；Claude Code 侧留空让它用自己的内置模型表）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// 密钥脱敏展示（前 4 + `...` + 后 4）。用于面板回显「导入的是哪把 key」，
    /// 真值只出现在 `link` 里。
    pub masked_key: String,
    /// `ccswitch://v1/import?...` 深链接（**含密钥明文**，见模块头安全边界）。
    pub link: String,
}

/// 端点响应体。就绪与未就绪**共用同一形状**，前端按 [`Self::ready`] 分支渲染；
/// 未就绪时 HTTP 状态码为 409 且 [`Self::plans`] 恒空（见 [`client_setup`]）。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientSetupResponse {
    /// 是否可以产出可用的导入链接。
    pub ready: bool,
    /// 探测明细（含 blockers）。
    pub probe: SetupProbe,
    /// 实际使用的网关对外地址（无尾斜杠）。
    pub base_url: String,
    /// `baseUrl` 的来源，排障用：`explicit`（显式传参）/ `requestHost`（请求 Host）。
    /// 生成错了（如反代改写 Host）一眼能看出是猜的还是显式给的。
    pub base_url_source: &'static str,
    /// 导入计划。未就绪时为空——**这就是「不满足条件不提交」的可见形态**。
    pub plans: Vec<ClientImportPlan>,
}

/// `/v1/models` 广告的模型数。判据与 `anthropic::handlers::get_models` 完全一致
/// （都是 `CATALOG` 里 `advertised` 的那批），有源码级守卫钉住两处不漂移。
pub fn usable_model_count() -> usize {
    crate::anthropic::model_catalog::CATALOG
        .iter()
        .filter(|s| s.advertised)
        .count()
}

/// 组装探测结果（纯函数：调用方传入从**一次**快照读到的数字）。
///
/// 收成纯函数是为了让「就绪判据」可单测——真实 handler 里这些数字来自
/// `AdminService::client_setup_inputs`，测试里直接构造。
///
/// 模型数这条判据照原规格（kiro-lb 要求 `/v1/models` 至少回一个非空模型 id）保留：
/// 本仓目录当前是**编译期常量**（恒非空，这条实际打不到），但「广告清单」将来若变成
/// 配置驱动（自定义模型清单方向），它就是唯一能挡住「导入完客户端无模型可选」的判据。
pub fn probe(
    pool_total: usize,
    pool_available: usize,
    usable_models: usize,
    api_key_present: bool,
) -> SetupProbe {
    let mut blockers = Vec::new();
    if pool_available == 0 {
        blockers.push(format!(
            "号池没有可用凭据（可用 {pool_available}/{pool_total}）：先上号，否则导入的供应商每次请求都会失败"
        ));
    }
    if usable_models == 0 {
        blockers.push("模型目录为空：/v1/models 无任何模型，客户端会无从选模型".to_string());
    }
    if !api_key_present {
        blockers.push(
            "网关对话密钥未配置（config.json 的 apiKey 为空）：没有 key 的导入链接会被 cc-switch 直接拒绝"
                .to_string(),
        );
    }
    SetupProbe {
        pool_total,
        pool_available,
        usable_models,
        api_key_present,
        ready: blockers.is_empty(),
        blockers,
    }
}

/// 生成导入计划。**未就绪 ⇒ 空**（这是本模块的「零产出」契约：不满足条件不产出任何链接）。
///
/// 纯函数：不读时钟、不碰 IO、不碰状态——同样的输入恒得同样的链接（可复现、可断言）。
pub fn plan_imports(probe: &SetupProbe, base_url: &str, api_key: &str) -> Vec<ClientImportPlan> {
    if !probe.ready || api_key.trim().is_empty() {
        return Vec::new();
    }

    let host = authority_of(base_url);
    let name = format!("KiroStudio · {host}");
    let masked_key = mask_key(api_key);

    let mut plans = Vec::with_capacity(2);
    for target in [ClientTarget::CcSwitchClaude, ClientTarget::CcSwitchCodex] {
        // Claude Code：endpoint 不带 /v1（客户端自己拼 /v1/messages），模型留空
        // （本网关提供的是真 Claude 模型，让客户端用自己的内置模型表）。
        // Codex：endpoint 带 /v1，模型必须显式给（见 CODEX_DEFAULT_MODEL 注释）。
        let (app, endpoint, api_format, model) = match target {
            ClientTarget::CcSwitchClaude => ("claude", base_url.to_string(), "anthropic", None),
            ClientTarget::CcSwitchCodex => (
                "codex",
                format!("{base_url}/v1"),
                "openai_responses",
                Some(CODEX_DEFAULT_MODEL.to_string()),
            ),
        };

        let link = build_link(&CcSwitchLinkParams {
            app,
            name: &name,
            endpoint: &endpoint,
            api_key,
            api_format,
            model: model.as_deref(),
            homepage: base_url,
        });

        plans.push(ClientImportPlan {
            target,
            app,
            name: name.clone(),
            endpoint,
            api_format,
            model,
            masked_key: masked_key.clone(),
            link,
        });
    }
    plans
}

/// 链接参数（收成结构体而不是 6 个位置参数：参数顺序错了不会编译报错，只会静默错位）。
struct CcSwitchLinkParams<'a> {
    app: &'a str,
    name: &'a str,
    endpoint: &'a str,
    api_key: &'a str,
    api_format: &'a str,
    model: Option<&'a str>,
    homepage: &'a str,
}

/// 拼 `ccswitch://v1/import?...`。
///
/// 两个刻意的省略（不是漏了）：
/// - **不传 `enabled`**：官方文档说明它是「导入后是否立刻切换」开关的初值，
///   切换会改写用户的客户端配置（Codex 侧还会删 `~/.codex/auth.json`），
///   该由用户在确认框里自己决定，不由我们预设。
/// - **不传 `usageScript`**：余额查询脚本要求「HTTPS + 与 endpoint 同源」的端点，
///   而本仓没有按**客户端密钥**维度的用量/额度接口（客户端 Key 体系仍冻结），
///   硬拼一个查不出东西的脚本只会让卡片显示误导性的余额。
fn build_link(p: &CcSwitchLinkParams<'_>) -> String {
    // 每个值都过一遍百分号编码：官方文档「Encoding rules」第 2 步要求
    // 全部参数值按 `application/x-www-form-urlencoded` 编码（`+` 必须变 `%2B`，
    // 否则解码成空格）。key 是用户自定义字符串，可能含 `+` `&` `=` `#` 空格甚至中文，
    // 不编码会让链接被截断或参数错位。
    let mut params: Vec<(&str, String)> = vec![
        ("resource", "provider".to_string()),
        ("app", p.app.to_string()),
        ("name", p.name.to_string()),
        ("endpoint", p.endpoint.to_string()),
        ("apiKey", p.api_key.to_string()),
        ("apiFormat", p.api_format.to_string()),
    ];
    if let Some(model) = p.model {
        params.push(("model", model.to_string()));
    }
    params.push(("homepage", p.homepage.to_string()));
    params.push(("notes", PROVIDER_NOTES.to_string()));

    let query = params
        .iter()
        .map(|(k, v)| format!("{k}={}", urlencoding::encode(v)))
        .collect::<Vec<_>>()
        .join("&");
    format!("{CCSWITCH_IMPORT_PREFIX}?{query}")
}

/// 取 `scheme://authority` 里的 authority（用于供应商显示名与 `host` 回显）。
///
/// 入参必须是 [`normalize_base_url`] 的产物（已保证 scheme 存在、无 userinfo、无空白）。
fn authority_of(base_url: &str) -> &str {
    let rest = base_url
        .split_once("://")
        .map(|(_, r)| r)
        .unwrap_or(base_url);
    rest.split('/').next().unwrap_or(rest)
}

/// 密钥脱敏展示（前 4 + `...` + 后 4；非 ASCII 或长度 ≤16 时退化为 `***`）。
///
/// 与 `kiro::token_manager::mask_api_key` 同语义：两份实现分属不同模块的既有 API
/// （一份服务深链接回显、一份服务凭据列表），各自保留，不做二合一（把其中一份改成
/// 引用另一份会牵动各自的调用面）。等价性由本文件测试
/// `mask_key_matches_token_manager_mask_api_key` 逐项对拍钉住——两份若漂移，
/// 面板上同一个 key 会在两处显示成不同掩码。
pub fn mask_key(key: &str) -> String {
    if key.is_ascii() && key.len() > 16 {
        format!("{}...{}", &key[..4], &key[key.len() - 4..])
    } else {
        "***".to_string()
    }
}

// ============================================================================
// base_url 解析
// ============================================================================

/// 解析网关对外地址。优先级：显式 `baseUrl` 参数 → 请求 `Host` 头（配 `X-Forwarded-Proto`）。
///
/// 为什么不从配置里猜：网关自身不知道「用户是从哪个地址访问到我的」——面板可能经
/// 反代、隧道或局域网 IP 打开，只有请求本身带着这个信息（`Host`），或者调用方显式给。
///
/// 返回值第二项是来源标签，随响应回显，便于排障（见 [`ClientSetupResponse::base_url_source`]）。
pub fn resolve_base_url(
    explicit: Option<&str>,
    host: Option<&str>,
    forwarded_proto: Option<&str>,
) -> Result<(String, &'static str), String> {
    if let Some(raw) = explicit {
        let normalized = normalize_base_url(raw).map_err(|e| format!("baseUrl 参数不合法：{e}"))?;
        return Ok((normalized, "explicit"));
    }

    let host = host
        .map(str::trim)
        .filter(|h| !h.is_empty())
        .ok_or_else(|| {
            "无法确定网关对外地址：请求没有 Host 头，请显式传 baseUrl=http(s)://主机[:端口]"
                .to_string()
        })?;

    // scheme 取 `X-Forwarded-Proto` 的首段（反代场景：本进程只听 http，对外是 https）。
    // 这里**刻意不复用** XFF 的信任开关：那个开关守的是「客户端 IP」这类**安全判定**，
    // 而本值只影响**给管理员自己看/自己点的链接**——伪造它最多让链接写错 scheme，
    // 管理员在响应里一眼可见（baseUrl 原样回显）并能用 baseUrl 参数覆盖。
    let scheme = forwarded_proto
        .and_then(|v| v.split(',').next())
        .map(str::trim)
        .map(str::to_ascii_lowercase)
        .filter(|s| s == "http" || s == "https")
        .unwrap_or_else(|| "http".to_string());

    let candidate = format!("{scheme}://{host}");
    let normalized = normalize_base_url(&candidate)
        .map_err(|e| format!("请求 Host 头不合法（{candidate}）：{e}；请显式传 baseUrl=..."))?;
    Ok((normalized, "requestHost"))
}

/// 校验并规范化 `http(s)://主机[:端口][/路径]`，去掉尾部斜杠。
///
/// **白名单式**校验而不是「解析再放行」：本函数不解释 URL（不做百分号解码、不解析
/// userinfo），只按字符集收下三段，再**用收下的三段重新拼**返回值——任何没被显式允许的
/// 字符都不可能溜进产物。产物会写进用户客户端配置，宽松解析在这里没有收益。
fn normalize_base_url(raw: &str) -> Result<String, String> {
    let s = raw.trim();
    if s.is_empty() {
        return Err("空字符串".to_string());
    }

    let lower = s.to_ascii_lowercase();
    let (scheme, rest) = if lower.starts_with("http://") {
        ("http", &s["http://".len()..])
    } else if lower.starts_with("https://") {
        ("https", &s["https://".len()..])
    } else {
        return Err("只接受 http:// 或 https:// 开头的地址".to_string());
    };

    // 路径与 authority 的分界：第一个 `/`。
    let (authority, path) = match rest.split_once('/') {
        Some((a, p)) => (a, Some(p)),
        None => (rest, None),
    };
    if authority.is_empty() {
        return Err("缺少主机名".to_string());
    }
    if !authority
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_' | b':' | b'[' | b']'))
    {
        // 顺带挡掉 userinfo（`user:pass@host` 里的 `@`）、空白、以及任何非 ASCII。
        return Err(format!("主机名含非法字符：{authority}"));
    }

    let mut out = format!("{scheme}://{authority}");
    if let Some(p) = path {
        let p = p.trim_end_matches('/');
        if !p.is_empty() {
            if !p.bytes().all(|b| {
                b.is_ascii_alphanumeric() || matches!(b, b'/' | b'.' | b'-' | b'_' | b'~' | b'%')
            }) {
                return Err(format!("路径含非法字符：/{p}"));
            }
            out.push('/');
            out.push_str(p);
        }
    }
    Ok(out)
}

// ============================================================================
// HTTP 端点
// ============================================================================

/// GET /api/admin/client-setup/cc-switch
///
/// 一键导入 cc-switch：返回就绪度探测 + 每个目标（Claude Code / Codex）的导入深链接。
///
/// 只读：不写文件、不改配置、不发上游请求。可选 query 参数 `baseUrl` 覆盖网关对外地址
/// （默认取请求 Host + `X-Forwarded-Proto`，见 [`resolve_base_url`]）。
///
/// 状态码语义：就绪 → `200`；**未就绪 → `409`** 且 `plans` 为空（响应体形状不变，
/// blockers 逐条说明差什么）。刻意用 409 而不是 200+ready:false：这是「拒绝提交」，
/// 用成功码会让把状态码当唯一判据的调用方以为拿到了可用链接。
///
/// 所有响应（含 400/409 早退）统一带 `Cache-Control: no-store`（体含密钥明文，见模块头）。
pub async fn client_setup(
    State(state): State<AdminState>,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    let (pool_total, pool_available, api_key) = state.service.client_setup_inputs();
    let api_key = api_key.filter(|k| !k.trim().is_empty());

    let probe = probe(
        pool_total,
        pool_available,
        usable_model_count(),
        api_key.is_some(),
    );

    let host = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let forwarded_proto = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);

    let (base_url, base_url_source) = match resolve_base_url(
        params.get("baseUrl").map(String::as_str),
        host.as_deref(),
        forwarded_proto.as_deref(),
    ) {
        Ok(v) => v,
        Err(msg) => {
            // 连对外地址都定不下来 ⇒ 一条链接也产不出来，直接 400 让调用方补参数。
            // 400 的体里没有密钥，但照样补 `no-store`：本端点的缓存策略**不随状态码
            // 分叉**——分叉就得在每条早退分支上重新判断「这个体里有没有密钥」，
            // 那是会漂移的隐性契约（模块头「响应带 no-store」的表述也因此改准）。
            let mut response = Json(AdminErrorResponse::invalid_request(msg)).into_response();
            *response.status_mut() = StatusCode::BAD_REQUEST;
            response.headers_mut().insert(
                header::CACHE_CONTROL,
                axum::http::HeaderValue::from_static("no-store"),
            );
            return response;
        }
    };

    // 计划在**同一份探测**上生成（见模块头「条件提交」）。未就绪时计划恒空。
    let plans = match api_key.as_deref() {
        Some(key) => plan_imports(&probe, &base_url, key),
        None => Vec::new(),
    };

    let status = if probe.ready {
        StatusCode::OK
    } else {
        StatusCode::CONFLICT
    };
    let body = ClientSetupResponse {
        ready: probe.ready,
        probe,
        base_url,
        base_url_source,
        plans,
    };

    // 不用元组形式拼状态码 + 头 + Json：显式构造避免依赖元组 impl 的组合支持。
    let mut response = Json(body).into_response();
    *response.status_mut() = status;
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("no-store"),
    );
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "sk-kiro-0123456789abcdef0123456789abcdef";

    fn ready() -> SetupProbe {
        probe(3, 3, usable_model_count(), true)
    }

    // ------------------------------------------------------------------
    // 探测：三条判据
    // ------------------------------------------------------------------

    /// 三条判据全过才 ready；全不过时 blockers 必须逐条点名（不是一句笼统的话）。
    #[test]
    fn probe_requires_pool_models_and_key() {
        assert!(ready().ready);

        let no_pool = probe(0, 0, 30, true);
        assert!(!no_pool.ready);
        assert_eq!(no_pool.blockers.len(), 1);
        assert!(
            no_pool.blockers[0].contains("号池"),
            "{:?}",
            no_pool.blockers
        );

        let all_missing = probe(0, 0, 0, false);
        assert!(!all_missing.ready);
        assert_eq!(
            all_missing.blockers.len(),
            3,
            "三条判据各缺一条 ⇒ 三条 blocker：{:?}",
            all_missing.blockers
        );
        assert!(all_missing.blockers.iter().any(|b| b.contains("模型目录")));
        assert!(all_missing.blockers.iter().any(|b| b.contains("apiKey")));
    }

    /// 「有号但全禁用」不是就绪：可用数才是判据（与面板/`/healthz` 同源的 `available`）。
    /// 回退即 FAIL：把判据从 `pool_available` 改成 `pool_total`，本用例红。
    #[test]
    fn probe_counts_only_enabled_credentials() {
        let disabled_only = probe(2, 0, 30, true);
        assert!(!disabled_only.ready);
        assert!(
            disabled_only.blockers[0].contains("可用 0/2"),
            "文案要点明可用/总数，否则运维分不清「没号」和「号全被禁了」：{:?}",
            disabled_only.blockers
        );
    }

    // ------------------------------------------------------------------
    // 零产出契约（验收：无号池时 setup 必须零写入）
    // ------------------------------------------------------------------

    /// ★ 验收用例：号池为空 ⇒ 一条计划都产不出、一个字节的链接都不出。
    /// 回退即 FAIL：让 `plan_imports` 跳过就绪复检（直接拼链接），本用例红。
    #[test]
    fn empty_pool_yields_no_plan_and_no_link() {
        let p = probe(0, 0, usable_model_count(), true);
        let plans = plan_imports(&p, "http://127.0.0.1:8990", KEY);
        assert!(
            plans.is_empty(),
            "无号池时不得产出任何导入计划（kiro-lb「任一失败则一个字节都不写」的同形验收）"
        );
    }

    /// 就绪但 key 缺失/空白 ⇒ 同样零产出（无 key 的链接会被 cc-switch 拒，产出等于误导）。
    #[test]
    fn blank_key_yields_no_plan() {
        assert!(plan_imports(&ready(), "http://127.0.0.1:8990", "   ").is_empty());
    }

    /// 未就绪的整包响应里**不得出现协议头或密钥明文**——这是「零产出」对调用方的可见形态。
    /// 回退即 FAIL：未就绪时仍拼链接（如只在 handler 里判 ready 而 plan_imports 不判），本用例红。
    #[test]
    fn blocked_response_never_contains_link_or_key() {
        let p = probe(0, 0, usable_model_count(), true);
        let body = ClientSetupResponse {
            ready: p.ready,
            plans: plan_imports(&p, "http://127.0.0.1:8990", KEY),
            probe: p,
            base_url: "http://127.0.0.1:8990".to_string(),
            base_url_source: "explicit",
        };
        let json = serde_json::to_string(&body).expect("响应必须可序列化");
        assert!(
            !json.contains("ccswitch://"),
            "未就绪响应不得含深链接：{json}"
        );
        assert!(!json.contains(KEY), "未就绪响应不得含密钥原文");
        assert!(json.contains("\"ready\":false"));
    }

    // ------------------------------------------------------------------
    // 计划形状
    // ------------------------------------------------------------------

    /// Claude Code：endpoint 就是网关根（不带 /v1），模型留空让客户端用自己的内置表。
    #[test]
    fn claude_plan_uses_bare_origin_without_v1() {
        let plans = plan_imports(&ready(), "http://192.168.11.4:18001", KEY);
        let claude = plans
            .iter()
            .find(|p| p.target == ClientTarget::CcSwitchClaude)
            .expect("必须产出 Claude Code 计划");
        assert_eq!(claude.app, "claude");
        assert_eq!(claude.endpoint, "http://192.168.11.4:18001");
        assert_eq!(claude.api_format, "anthropic");
        assert!(claude.model.is_none(), "真 Claude 模型不该钉死模型名");
        assert!(claude.link.contains("app=claude"));
        // 注意：不能断言「整条链接不含 /v1」——深链接前缀 `ccswitch://v1/import` 自带 /v1。
        // 参数值里的 `/` 会被百分号编码成 `%2F`，故查编码形态才真正指向 endpoint 参数。
        assert!(
            !claude.link.contains("%2Fv1"),
            "Claude Code 侧 endpoint 不带 /v1：{}",
            claude.link
        );
    }

    /// Codex：endpoint 带 /v1（它自己拼 /responses），模型取目录里被广告的 GPT 变体。
    #[test]
    fn codex_plan_uses_v1_endpoint_and_catalog_model() {
        let plans = plan_imports(&ready(), "http://192.168.11.4:18001", KEY);
        let codex = plans
            .iter()
            .find(|p| p.target == ClientTarget::CcSwitchCodex)
            .expect("必须产出 Codex 计划");
        assert_eq!(codex.app, "codex");
        assert_eq!(codex.endpoint, "http://192.168.11.4:18001/v1");
        assert_eq!(codex.api_format, "openai_responses");
        assert_eq!(codex.model.as_deref(), Some(CODEX_DEFAULT_MODEL));
        assert!(codex.link.contains("model=gpt-5.6-terra"));
    }

    /// 供应商名带 host：cc-switch 不按名字去重，多网关共存时要能区分。
    #[test]
    fn plan_name_carries_host() {
        let plans = plan_imports(&ready(), "https://kiro.example.com", KEY);
        for p in &plans {
            assert_eq!(p.name, "KiroStudio · kiro.example.com");
        }
    }

    /// 面板回显的掩码：与 token_manager 的 mask_api_key 同语义（前 4 + ... + 后 4）。
    #[test]
    fn plan_masks_key_for_display() {
        let plans = plan_imports(&ready(), "http://127.0.0.1:8990", KEY);
        assert_eq!(plans[0].masked_key, "sk-k...cdef");
        assert_eq!(mask_key("short"), "***");
        assert_eq!(mask_key("密钥密钥密钥密钥密钥密钥密钥密钥"), "***");
    }

    /// 跨模块对照：`mask_key` 与 `kiro::token_manager::mask_api_key` 必须逐字节同语义。
    ///
    /// 两份实现服务两个展示面（深链接回显 / 凭据列表），此前全仓没有一处同时调用
    /// 两者 ⇒ 任一份漂移都不会红。本用例把同一批输入喂给两边逐项比对：正常 key、
    /// 非 ASCII、长度 16/17 边界、空串、混合字符。边界再钉两处绝对值——只对拍
    /// 「两边相等」的话，两边被改成同一种错误实现仍会绿。
    /// 回退即 FAIL：任一份的阈值（>16）、ASCII 判据或首尾切片（4/4）被改动，红。
    #[test]
    fn mask_key_matches_token_manager_mask_api_key() {
        use crate::kiro::token_manager::mask_api_key;

        let cases: [(&str, &str); 7] = [
            ("正常 key", "sk-kiro-0123456789abcdef0123456789abcdef"),
            ("长度恰 16（不掩码）", "0123456789abcdef"),
            ("长度 17（掩码下界）", "0123456789abcdefg"),
            ("非 ASCII（整体回退）", "密钥密钥密钥密钥密钥密钥密钥密钥"),
            ("ASCII 混非 ASCII", "sk-密钥-0123456789abcdef"),
            ("空串", ""),
            ("短串", "short"),
        ];
        for (label, key) in cases {
            assert_eq!(
                mask_key(key),
                mask_api_key(key),
                "两份掩码实现漂移（{label}）：mask_key={:?}，mask_api_key={:?}",
                mask_key(key),
                mask_api_key(key)
            );
        }

        // 边界绝对值：16 及以下不掩码；17 起前 4 + ... + 后 4。
        assert_eq!(mask_key("0123456789abcdef"), "***");
        assert_eq!(mask_key("0123456789abcdefg"), "0123...defg");
    }

    /// 深链接的每个参数值都必须过百分号编码——用**反解**证明往返一致。
    /// 回退即 FAIL：去掉 `urlencoding::encode`（或只编码一部分），反解结果与原文不符，本用例红。
    #[test]
    fn link_percent_encodes_special_characters() {
        // 含 `+` `&` `=` `#` 空格与中文的 key（用户自定义密钥完全可能长这样）。
        let tricky = "sk+a&b=c#d 密钥";
        let plans = plan_imports(&ready(), "http://127.0.0.1:8990", tricky);
        let link = &plans[0].link;

        assert!(!link.contains(' '), "链接里不得有裸空格：{link}");
        assert!(
            link.contains("apiKey=sk%2Ba%26b%3Dc%23d%20"),
            "key 必须整体百分号编码（`+`→%2B、`&`→%26、空格→%20）：{link}"
        );

        let raw_key = link
            .split("apiKey=")
            .nth(1)
            .and_then(|rest| rest.split('&').next())
            .expect("链接必须带 apiKey 参数");
        let decoded = urlencoding::decode(raw_key).expect("参数值必须是合法百分号编码");
        assert_eq!(
            decoded, tricky,
            "编码后必须能原样解回，否则导入的 key 是错的"
        );
    }

    // ------------------------------------------------------------------
    // base_url 解析
    // ------------------------------------------------------------------

    /// 显式参数优先于 Host，且尾部斜杠被规范化掉（否则 endpoint 会变成 `host//v1`）。
    #[test]
    fn base_url_prefers_explicit_and_strips_trailing_slash() {
        let (url, src) = resolve_base_url(
            Some("https://kiro.example.com/"),
            Some("127.0.0.1:8990"),
            None,
        )
        .expect("显式 baseUrl 应通过");
        assert_eq!(url, "https://kiro.example.com");
        assert_eq!(src, "explicit");

        let (url, _) = resolve_base_url(Some("http://host:1234/kiro/"), None, None)
            .expect("带路径前缀也应通过");
        assert_eq!(url, "http://host:1234/kiro");
    }

    /// 反代场景：scheme 取 `X-Forwarded-Proto` 首段；非 http/https 的值一律忽略回落 http。
    #[test]
    fn base_url_uses_forwarded_proto_first_token() {
        let (url, src) = resolve_base_url(None, Some("kiro.example.com"), Some("https")).unwrap();
        assert_eq!(url, "https://kiro.example.com");
        assert_eq!(src, "requestHost");

        let (url, _) =
            resolve_base_url(None, Some("kiro.example.com"), Some("https, http")).unwrap();
        assert_eq!(url, "https://kiro.example.com", "多段取第一段");

        let (url, _) = resolve_base_url(None, Some("kiro.example.com"), Some("gopher")).unwrap();
        assert_eq!(url, "http://kiro.example.com", "未知 scheme 一律回落 http");
    }

    /// 白名单校验：非 http(s)、带 userinfo、带 query/fragment、含空白与非法字符一律拒绝。
    /// 这些字符串会写进用户客户端配置，宁可报错也不能放行。
    #[test]
    fn base_url_rejects_dangerous_forms() {
        for bad in [
            "file:///etc/passwd",
            "ftp://host",
            "javascript:alert(1)",
            "https://user:pass@host",
            "http://host?x=1",
            "http://host#frag",
            "http://",
            "http://ho st",
            "http://host/路径",
            "  ",
        ] {
            assert!(
                resolve_base_url(Some(bad), None, None).is_err(),
                "必须拒绝：{bad}"
            );
        }
        // 没有 Host 也没有 baseUrl ⇒ 明确报错（而不是产出一条 host 为空的链接）。
        assert!(resolve_base_url(None, None, None).is_err());
        assert!(resolve_base_url(None, Some("   "), None).is_err());
    }

    /// 伪造 `Host` / `X-Forwarded-Proto` 的形态表：Host 是**有意信任**的来源
    /// （反代/隧道场景只有请求自己知道用户从哪个地址进来），本用例不改信任模型，
    /// 只钉住白名单行为的两条不变量：
    /// ① 产物 scheme 只可能是 http/https（伪造 proto 为 javascript/gopher 等一律回落 http）；
    /// ② 产物里不可能出现 `@`（userinfo）或空白——`normalize_base_url` 的字符集
    ///    只收 ASCII 字母数字与 `. - _ : [ ]`，`@`/空白/非 ASCII 在 authority 段直接拒绝。
    /// 回退即 FAIL：放宽 normalize 的字符集（放行 `@` 或空白）⇒ 下面 Err 断言红；
    /// 把 scheme 回落改成直接采用 forwarded_proto 原值 ⇒ `javascript` 用例红。
    #[test]
    fn forged_host_and_forwarded_proto_cannot_smuggle_scheme_or_userinfo() {
        // 可解析的伪造形态：产物必须是指向该 host 的干净 http(s) origin。
        for (host, proto, expected) in [
            ("evil.com", None, "http://evil.com"),
            ("evil.com", Some("https"), "https://evil.com"),
            ("evil.com:8080", Some("http"), "http://evil.com:8080"),
            ("evil.com", Some("javascript"), "http://evil.com"),
            ("evil.com", Some("http://"), "http://evil.com"),
            ("evil.com", Some("  HTTPS  "), "https://evil.com"),
        ] {
            let (url, src) = resolve_base_url(None, Some(host), proto)
                .unwrap_or_else(|e| panic!("Host={host:?} proto={proto:?} 应可解析：{e}"));
            assert_eq!(src, "requestHost");
            assert_eq!(url, expected, "Host={host:?} proto={proto:?}");
            assert!(
                url.starts_with("http://") || url.starts_with("https://"),
                "scheme 只可能是 http(s)：{url}"
            );
            assert!(!url.contains('@'), "endpoint 不得含 userinfo 的 `@`：{url}");
            assert!(
                !url.contains(char::is_whitespace),
                "endpoint 不得含空白：{url}"
            );
        }

        // 必须整体拒绝的伪造形态：任何一条被「解析收下」都会让 `@`/空白/非 ASCII
        // 进入产物（产物会写进用户客户端配置），所以这里断言的是拒绝而不是清洗。
        for (host, proto) in [
            ("user@evil.com", None),          // userinfo 形态（Host 头里带 @）
            ("user:pass@evil.com", Some("https")), // 完整 userinfo
            ("evil .com", None),              // 内部空白
            ("evil.com\r\nX-Forwarded-Host: evil2.com", None), // CRLF 注入形态
            ("evil.com\nevil2.com", None),    // 内部换行
            ("例子.com", None),               // 非 ASCII
            ("evil.com\u{200b}", None),       // 零宽空格（trim 不吞、非 ASCII 字节）
        ] {
            assert!(
                resolve_base_url(None, Some(host), proto).is_err(),
                "伪造形态必须被拒绝（host={host:?}，proto={proto:?}）"
            );
        }
    }

    // ------------------------------------------------------------------
    // 常量与接线守卫
    // ------------------------------------------------------------------

    /// Codex 默认模型必须仍在模型目录里**被广告**且属 GPT 族。
    /// 回退即 FAIL：目录里把 gpt-5.6-terra 改名/下架而这里不改，导入的 Codex 供应商
    /// 第一个请求就 INVALID_MODEL_ID，而本用例会先红。
    #[test]
    fn codex_default_model_is_advertised_in_catalog() {
        use crate::anthropic::model_catalog::{CATALOG, Family};
        assert!(
            CATALOG.iter().any(|s| {
                s.kiro_id == CODEX_DEFAULT_MODEL && s.advertised && s.family == Family::Gpt
            }),
            "{CODEX_DEFAULT_MODEL} 不在模型目录（或未广告 / 非 GPT 族）——目录改名的同时必须改这里"
        );
    }

    /// `usable_model_count` 与 `/v1/models` 必须同判据（都是 `advertised` 那批）。
    /// 守卫用源码级断言：两处各写一份判据时，改一处漏一处会让面板说「有模型」
    /// 而客户端 `/v1/models` 拿到空表。
    #[test]
    fn usable_model_count_uses_same_filter_as_models_endpoint() {
        let handlers = include_str!("../anthropic/handlers.rs");
        let compact: String = handlers.chars().filter(|c| !c.is_whitespace()).collect();
        assert!(
            compact.contains(".filter(|s|s.advertised)"),
            "anthropic/handlers.rs 的 /v1/models 不再按 advertised 过滤——\
             usable_model_count 的判据必须与它同步，否则本模块的「可用模型数」是假的"
        );
    }

    /// 路由接线守卫：端点必须挂在 authed 树内（不是又挂出一条匿名入口）。
    ///
    /// 只断言「源码里出现过这段接线」不够：把接线**复制**一份进 public 树、
    /// 或整个**挪**进 public 树，`contains` 仍然绿（验证者实测）。本守卫改成三条：
    /// ① 接线在 `let authed = Router::new()` 之后、authed 树的 `admin_auth_middleware`
    ///    层之前（位置断言——挪进 public 树 ⇒ 越过鉴权层 ⇒ 红）；
    /// ② 生产代码里**只允许出现一次**（复制一份进 public 树 ⇒ 计数 2 ⇒ 红）；
    /// ③ public 树段（`let public =` 起、到下一个 `pub fn` 前）不含该路径名
    ///    （换写法绕过 needle 比对也要命中，例如限定路径 `axum::routing::get`）。
    ///
    /// 锚点字符串取自 router.rs 现文（去空白比对）：接线写法变了守卫会红并提示同步。
    #[test]
    fn client_setup_route_is_wired_in_authed_tree() {
        let router = include_str!("router.rs");
        // 只比生产代码：router.rs 自己的测试模块里若出现同名字面量不该算进接线计数
        // （本仓既有教训——守卫读到的源码含测试文本会多算/假绿；router.rs 既有守卫同款裁切）。
        let cut = router.find("#[cfg(test)]").unwrap_or(router.len());
        let compact: String = router[..cut].chars().filter(|c| !c.is_whitespace()).collect();

        let needle = "\"/client-setup/cc-switch\",get(client_setup)";
        let needle_pos = compact.find(needle).expect(
            "router.rs 里缺少 /client-setup/cc-switch 的接线（或写法变了，请同步本守卫）",
        );

        // ② 只允许一处接线：多出来的一份无论挂到哪都是绕过鉴权层的第二入口。
        assert_eq!(
            compact.matches(needle).count(),
            1,
            "该接线必须全文件唯一——复制一份到 public 树 / merge 之后再加 .route，都会在这里红"
        );

        // ① 位置必须落在 authed 树内：起点之后、其鉴权层之前（取起点之后第一次出现
        // `admin_auth_middleware`，即 authed 树自己挂的那层；顶部 import 在起点之前，不计）。
        let authed_start = compact.find("letauthed=Router::new()").expect(
            "router.rs 的 authed 树起点（`let authed = Router::new()`）不见了，请同步本守卫",
        );
        let auth_layer = authed_start
            + compact[authed_start..]
                .find("admin_auth_middleware")
                .expect("authed 树的 `admin_auth_middleware` 层不见了，请同步本守卫");
        assert!(
            needle_pos > authed_start,
            "接线出现在 authed 树起点之前——它不在鉴权树内"
        );
        assert!(
            needle_pos < auth_layer,
            "接线出现在 authed 树的鉴权层之后（例如被挪进 public 树）——它不在鉴权树内"
        );

        // ③ public 树段不得出现该路径（扫路径名而不是扫 needle：换写法也要命中）。
        let public_start = compact.find("letpublic=Router::new()").expect(
            "router.rs 的 public 树起点（`let public = Router::new()`）不见了，请同步本守卫",
        );
        let public_end = public_start
            + compact[public_start..]
                .find("pubfn")
                .expect("public 树之后找不到函数边界（`pub fn`），请同步本守卫");
        assert!(
            !compact[public_start..public_end].contains("/client-setup"),
            "public 树段里出现了 /client-setup——本端点会带密钥明文，绝不允许匿名入口"
        );
    }

    /// 零写入承诺的机械守卫：本模块不得出现任何文件写入/删除 API。
    ///
    /// 判据用运行时拼接（注释里出现这些字面量会让守卫自己变红——本仓有同款教训）。
    /// 真需要写文件时（如将来做本机客户端配置写入）必须**同时**改这条守卫，
    /// 也就是逼一次「为什么现在可以写了」的显式决策。
    #[test]
    fn module_never_touches_the_filesystem() {
        let src = include_str!("client_setup.rs");
        for needle in [
            concat!("fs::", "write"),
            concat!("File::", "create"),
            concat!("Open", "Options"),
            concat!("write_", "all"),
            concat!("remove_", "file"),
            concat!("fs::", "rename"),
            concat!("create_", "dir"),
            concat!("std::", "process"),
        ] {
            assert!(
                !src.contains(needle),
                "本模块的契约是「服务端零写入」，不得出现 {needle}；\
                 确有必要时请连同本守卫一起改，并在模块头写明为什么"
            );
        }
    }
}

/// 端点级测试：真调 [`client_setup`]，验证状态码 / 缓存头 / 响应体三件事。
///
/// 夹具照 `service_tests` 的既有做法（`MultiTokenManager::new(..., None, None, false)`
/// = 纯内存、无落盘、无网络），不引新的测试基座。
#[cfg(test)]
mod handler_tests {
    use super::*;
    use crate::kiro::model::credentials::KiroCredentials;
    use crate::kiro::token_manager::MultiTokenManager;
    use crate::model::config::Config;
    use std::sync::Arc;

    const KEY: &str = "sk-kiro-0123456789abcdef0123456789abcdef";

    fn state_with(creds: Vec<KiroCredentials>, api_key: Option<&str>) -> AdminState {
        let mut config = Config::default();
        config.api_key = api_key.map(str::to_string);
        let tm = Arc::new(
            MultiTokenManager::new(config, creds, None, None, false).expect("构造 token manager"),
        );
        AdminState::new(super::super::service::AdminService::new(
            tm,
            Vec::<String>::new(),
        ))
    }

    fn one_credential() -> KiroCredentials {
        let mut c = KiroCredentials::default();
        c.id = Some(1);
        c.auth_method = Some("api_key".to_string());
        c.kiro_api_key = Some("ksk_test".to_string());
        c
    }

    async fn call(
        state: AdminState,
        base_url: Option<&str>,
    ) -> (StatusCode, HeaderMap, serde_json::Value) {
        let mut params = HashMap::new();
        if let Some(b) = base_url {
            params.insert("baseUrl".to_string(), b.to_string());
        }
        let mut headers = HeaderMap::new();
        if base_url.is_none() {
            headers.insert(header::HOST, "lan.example:8990".parse().unwrap());
        }
        let resp = client_setup(State(state), Query(params), headers).await;
        let status = resp.status();
        let headers = resp.headers().clone();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("读响应体");
        let json: serde_json::Value = serde_json::from_slice(&bytes).expect("响应必须是 JSON");
        (status, headers, json)
    }

    /// ★ 验收（kiro-lb 用例形态）：**没有可用号池时，端点拒绝提交且一个字节的产物都不出**。
    ///
    /// 三条断言分别对应「零写入」的三个可见面：状态码是拒绝（409 而非 200）、
    /// 计划为空、响应体里既没有深链接也没有密钥原文。
    /// 回退即 FAIL：让 handler 无视 `probe.ready` 直接拼计划，本用例红。
    #[tokio::test]
    async fn handler_refuses_and_emits_nothing_without_a_pool() {
        let (status, headers, body) = call(state_with(Vec::new(), Some(KEY)), None).await;

        assert_eq!(status, StatusCode::CONFLICT, "未就绪必须拒绝提交");
        assert_eq!(
            headers
                .get(header::CACHE_CONTROL)
                .and_then(|v| v.to_str().ok()),
            Some("no-store"),
            "体含密钥明文，必须禁缓存（同 export_kam_credentials 的 MINOR-4）"
        );
        assert_eq!(body["ready"], serde_json::json!(false));
        assert_eq!(body["plans"], serde_json::json!([]));
        assert_eq!(body["probe"]["poolAvailable"], serde_json::json!(0));
        assert!(
            body["probe"]["blockers"]
                .as_array()
                .is_some_and(|b| !b.is_empty()),
            "必须逐条说明差什么：{body}"
        );

        let raw = body.to_string();
        assert!(!raw.contains("ccswitch://"), "未就绪不得吐深链接");
        assert!(!raw.contains(KEY), "未就绪不得吐密钥原文");
    }

    /// 就绪时：两条链接 + `Host` 头推出的地址 + 密钥进了链接（且只以掩码回显）。
    #[tokio::test]
    async fn handler_emits_two_links_when_ready() {
        let (status, headers, body) =
            call(state_with(vec![one_credential()], Some(KEY)), None).await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            headers
                .get(header::CACHE_CONTROL)
                .and_then(|v| v.to_str().ok()),
            Some("no-store")
        );
        assert_eq!(body["ready"], serde_json::json!(true));
        assert_eq!(
            body["baseUrl"],
            serde_json::json!("http://lan.example:8990")
        );
        assert_eq!(body["baseUrlSource"], serde_json::json!("requestHost"));

        let plans = body["plans"].as_array().expect("plans 必须是数组");
        assert_eq!(plans.len(), 2, "Claude Code 与 Codex 各一条");
        for plan in plans {
            let link = plan["link"].as_str().expect("链接必须是字符串");
            assert!(
                link.starts_with("ccswitch://v1/import?"),
                "协议头必须正确（写错整条链接会被 cc-switch 拒）：{link}"
            );
            assert!(
                link.contains(KEY),
                "链接必须带密钥（cc-switch 无 key 直接拒）"
            );
            assert!(
                !plan["maskedKey"].as_str().unwrap_or_default().contains(KEY),
                "回显的必须是掩码，不能是原文"
            );
        }
    }

    /// 显式 `baseUrl` 覆盖 Host（面板经隧道/反代访问时的逃生口），且响应回显来源标签。
    #[tokio::test]
    async fn handler_honours_explicit_base_url() {
        let (status, _, body) = call(
            state_with(vec![one_credential()], Some(KEY)),
            Some("https://kiro.example.com/"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body["baseUrl"],
            serde_json::json!("https://kiro.example.com")
        );
        assert_eq!(body["baseUrlSource"], serde_json::json!("explicit"));
    }

    /// 没有 Host 也没有 baseUrl ⇒ 400（绝不产出一条 host 为空的链接）。
    #[tokio::test]
    async fn handler_rejects_when_no_address_is_known() {
        let state = state_with(vec![one_credential()], Some(KEY));
        let resp = client_setup(State(state), Query(HashMap::new()), HeaderMap::new()).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    /// 伪造 Host（userinfo 形态）在 HTTP 层即被拒：400、响应体不含密钥，
    /// 且 400 早退同样带 `no-store`（缓存策略不随状态码分叉，见模块头）。
    #[tokio::test]
    async fn handler_rejects_forged_host_with_userinfo() {
        let state = state_with(vec![one_credential()], Some(KEY));
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, "user@evil.com".parse().expect("合法头值"));
        let resp = client_setup(State(state), Query(HashMap::new()), headers).await;

        assert_eq!(
            resp.status(),
            StatusCode::BAD_REQUEST,
            "userinfo 形态的 Host 必须拒绝"
        );
        assert_eq!(
            resp.headers()
                .get(header::CACHE_CONTROL)
                .and_then(|v| v.to_str().ok()),
            Some("no-store"),
            "400 早退也必须 no-store（不随状态码分叉）"
        );
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("读响应体");
        let raw = String::from_utf8_lossy(&bytes);
        assert!(!raw.contains(KEY), "400 体里不得出现密钥：{raw}");
        assert!(!raw.contains("ccswitch://"), "400 体里不得出现深链接：{raw}");
    }

    /// 伪造 `X-Forwarded-Proto`（非 http(s)）：scheme 只可能回落 http，
    /// 产出的 baseUrl / endpoint 绝不含 `@` 或非 http(s) scheme。
    #[tokio::test]
    async fn handler_ignores_forged_forwarded_proto() {
        let state = state_with(vec![one_credential()], Some(KEY));
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, "lan.example:8990".parse().unwrap());
        headers.insert("x-forwarded-proto", "javascript".parse().unwrap());
        let resp = client_setup(State(state), Query(HashMap::new()), headers).await;
        assert_eq!(resp.status(), StatusCode::OK);

        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("读响应体");
        let body: serde_json::Value = serde_json::from_slice(&bytes).expect("响应必须是 JSON");
        assert_eq!(
            body["baseUrl"],
            serde_json::json!("http://lan.example:8990"),
            "伪造的 proto 不得进产物（回落 http）"
        );
        for plan in body["plans"].as_array().expect("plans 必须是数组") {
            let endpoint = plan["endpoint"].as_str().expect("endpoint 必须是字符串");
            assert!(
                endpoint.starts_with("http://") || endpoint.starts_with("https://"),
                "endpoint scheme 只可能是 http(s)：{endpoint}"
            );
            assert!(!endpoint.contains('@'), "endpoint 不得含 `@`：{endpoint}");
        }
    }
}
