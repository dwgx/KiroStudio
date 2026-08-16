//! 自定义 API「代挂透传」——Anthropic 兼容上游中转站的反向代理。
//!
//! 语义(dwgx 定):自定义 API 凭据(auth_method=custom_api)是一个 **Anthropic 兼容上游**
//! (base_url + api_key)。当选号命中这类凭据时,把客户端的 `/v1/messages` 请求**原样透传**
//! 到 `base_url`、换用该凭据的 api_key,响应流**原样回**给客户端。入口=出口=Anthropic,
//! 零协议转换——效果等同用户直接拿那个 key 打上游。
//!
//! ⚠️ 与 Kiro 主路径完全隔离:透传响应**绝不进** Kiro 的 event-stream 解码器 / StreamContext,
//! 而是把上游的字节流原样 [`Body::from_stream`] 回去。Kiro 转发路径一行不改。

use axum::body::Body;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
// TryStreamExt 提供 map_err（错误传播）；StreamExt 的 map 不再需要。
use futures::TryStreamExt;

use crate::common::http_read::read_body_capped;
use crate::kiro::model::credentials::KiroCredentials;
use crate::kiro::passthrough_think_filter::{
    filter_json_bytes_with, filter_sse_stream_with, guard_empty_stream,
};
use crate::model::config::TlsBackend;

/// 非流式响应过滤前允许读取的最大字节数。非流式 JSON 响应通常远小于此；
/// 纯防御上限，防恶意上游吐超大 body 顶爆内存。
const PASSTHROUGH_JSON_CAP_BYTES: u64 = 32 * 1024 * 1024;

/// 上游模型列表的读取上限。模型列表是**外部可控**数据（上游被劫持/DNS 投毒时可无上限
/// 放大内存），而 `resp.json()` 会把整个 body 无上限读进内存 —— 本仓
/// `common/http_read.rs` 已把这条点名为 OOM 反模式并收口了 `read_json_capped`。
/// 4 MiB 对模型列表绰绰有余（几百个模型也只有几十 KB）。
const PASSTHROUGH_MODELS_CAP_BYTES: u64 = 4 * 1024 * 1024;

// 🔴 M8：透传出站 client 统一走 `http_client::pinned_streaming_client` ——
// 每次出站前**运行时 SSRF 复验 + DNS 固化**（见该函数文档）。
//
// 背景：`forward` / `fetch_upstream_models` 原先用 `build_streaming_client_no_redirect`
// （普通 client），base_url 只在**写入配置时**校验一次，之后域名由 reqwest 每请求
// 重新解析 —— DNS rebinding（写入时公网、运行时内网）可绕过写入时校验。pinned
// client 用与写入时**相同策略**（`AdminConfigured`：本机代挂 127.0.0.1 / fake-IP
// 段放行，私网/元数据拒绝）复验，并把解析结果 `resolve_to_addrs` 固化 —— 校验与
// 连接共用同一份解析结果，无 TOCTOU 窗口；合法显式内网配置不受影响。
//
// 连接池按 (host, proxy, tls) 缓存（缓存实现在 http_client.rs）：解析结果未变化时
// 复用 client，保留旧 `PASSTHROUGH_CLIENTS` 的「不每请求新建、避免 `error sending
// request`」语义；变化时才重建。

/// 🔴 P4：按白名单把上游响应头透传给客户端。
///
/// 改前三个响应构造点只设 `content-type`，上游 429 的 `Retry-After`、`x-ratelimit-*`、
/// `request-id` 全部丢弃。后果：客户端收到 429 却拿不到 `Retry-After`，只能用自己的固定
/// 退避，与上游的 429 节奏互相放大碰撞（配合 P1 的重试叠乘，是线上 429 风暴的一环）。
///
/// 白名单而非全量透传：`content-length` 必须排除（body 被过滤/改写或流式时它会错），
/// `transfer-encoding` 必须排除（帧结构由 axum 自己写）。这两个透传会让客户端读到
/// 矛盾的 body 边界 → 解析错乱。
fn apply_upstream_response_headers(
    mut builder: axum::http::response::Builder,
    upstream_headers: &axum::http::HeaderMap,
) -> axum::http::response::Builder {
    for (name, value) in upstream_headers.iter() {
        let n = name.as_str();
        let allow = n == "retry-after"
            || n == "request-id"
            // 🔴 `content-encoding` 必须透传（2026-08-09 `Failed to parse JSON` 的兜底半边）。
            //
            // body 是**原样字节流转发**的（`Body::from_stream(byte_stream)` 不解压），
            // 所以上游若回 gzip，客户端必须看到 `content-encoding: gzip` 才知道要解压。
            // 改前不透传它 ⇒ 客户端把 gzip 字节当明文 JSON 解析 → `Failed to parse JSON`。
            //
            // 与上面「不转发 accept-encoding」配对：那条让上游默认不压缩（治本），
            // 这条保证万一上游仍压缩（有些上游无视 accept-encoding）客户端也能正确解（兜底）。
            // ⚠️ 两条必须同时存在。只做一条都有漏：只治本 → 遇到强制压缩的上游仍炸；
            // 只兜底 → 依赖客户端一定支持该编码。
            || n == "content-encoding"
            || n.starts_with("x-ratelimit-")
            || n.starts_with("anthropic-ratelimit-")
            || n.starts_with("x-request-id");
        if allow {
            builder = builder.header(name.clone(), value.clone());
        }
    }
    builder
}

/// 被动配额观测（sub2api 借鉴；**纯观测，不参与选号/调度**）。
///
/// 现状核查（2026-08-14）：P4 白名单已把配额头**透传给客户端**（让客户端能主动
/// 限速），但网关自身从不读取它们 —— 上游的配额余量只进客户端、不落日志，运维侧
/// 看不到「哪个号快被限流了」。这里在成功路径上把配额头摘出来落一条 debug 日志。
///
/// 刻意不升级 info：成功路径每请求都走，info 会刷爆日志；失败路径已有上游错误
/// 原文日志（含 `Retry-After` 语义，见 `upstream_trace`）。
///
/// ⚠️ 范围声明：完整目标（写进凭据余额缓存、面板展示）需要 `admin::service` 的
/// 余额缓存写入口，超出本模块边界；此处只做日志观测，先把数据留下来。
/// 从上游响应头摘配额头（x-ratelimit-* 优先，anthropic-ratelimit-* 兜底）。
///
/// 2026-08-15 补 anthropic-ratelimit-* 三键：P4 白名单已透传这两族配额头给客户端，
/// 观测侧只读 x-ratelimit-* 会让 Anthropic 兼容上游（deepseek 等）的配额余量
/// 完全不落日志。Anthropic 的 requests 维度键名与 OpenAI 的 x-ratelimit-* 对等。
fn quota_headers(
    headers: &axum::http::HeaderMap,
) -> (Option<&str>, Option<&str>, Option<&str>) {
    let get = |n: &str| headers.get(n).and_then(|v| v.to_str().ok());
    let limit = get("x-ratelimit-limit")
        .or_else(|| get("anthropic-ratelimit-requests-limit"));
    let remaining = get("x-ratelimit-remaining")
        .or_else(|| get("anthropic-ratelimit-requests-remaining"));
    let reset = get("x-ratelimit-reset")
        .or_else(|| get("anthropic-ratelimit-requests-reset"));
    (limit, remaining, reset)
}

fn observe_upstream_quota_headers(upstream_headers: &axum::http::HeaderMap, base_url: &str) {
    let (limit, remaining, reset) = quota_headers(upstream_headers);
    if limit.is_none() && remaining.is_none() && reset.is_none() {
        return;
    }
    tracing::debug!(
        base_url = %base_url,
        limit = limit.unwrap_or("-"),
        remaining = remaining.unwrap_or("-"),
        reset = reset.unwrap_or("-"),
        "[透传] 上游配额头（被动观测，仅记录不参与调度）"
    );
}

/// 把一次 Anthropic 请求原样透传到自定义 API 上游,响应流式原样返回。
///
/// - `cred`:命中的自定义 API 凭据(提供 base_url / api_key / 代理)。
/// - `raw_body`:客户端原始 `/v1/messages` 请求体(**未经 Kiro 转换**)。
/// - `global_proxy` / `tls_backend`:复用全局代理与 TLS 后端配置。
///
/// 返回 `(Response, StatusCode)`:Response 原样透传上游 status/body(失败为 502 错误响应);
/// StatusCode 供调用侧(provider)据以推断 usage outcome 并做轻量结果计数。**只暴露 header 层
/// status,body 仍原样流式回传,绝不解析上游 SSE**(隔离铁律 3)。
// 🔴 **首字节（响应头）超时**（2026-08-10 补，实测缺口）。
//
// 为什么不能只靠 client 的 720s `read_timeout`：那个值是**流式空闲间隔**，
// 刻意放宽到 720s 以防长回复被中途掐断（见 `pinned_streaming_client` 的取值），
// 但它同时也成了"等响应头"的上限 —— 上游若接受连接却永不回响应头，单跳就能挂 720s。
//
// 实测（2026-08-10 真打线上上游）：`claude-nonexistent-zzz` 这类不存在的模型，
// k2cc 上游 **40s 不返回任何响应头**（TimeoutError），而 denzao 0.2s 就返 404。
// 后果：透传 failover 的 45s 墙钟只在**每轮进循环时**判，所以第一跳就能把整条
// 客户端请求拖到 720s，中间既不换号也不返回 —— 客户端与 trace 里都看不到任何记录
// （实测该请求在日志里只有一条 "Received"，之后彻底静默）。
//
// 🔴 **原取 30s 是错的，2026-08-10 同日改为 90s。**
//
// 原依据写「健康上游响应头延迟实测 0.2~6s」—— 那个数字来自**健康**上游，
// 而真实工况下代挂上游（kiro2cc）的响应头延迟 **p50=12.7s / p90=30.0s**。
// 把阈值设在 p90 上，等于**按设计砍掉一成正常请求**。
//
// 铁证（线上 traces，同日实测）：
// - 44 条请求在 **30.7s** 后才出响应头、且最终 **200 成功** —— 它们被 30s 白白掐死
// - 82 条卡在 30s 被掐断，两者合计占样本的 **12%**
// - 成功请求的最大延迟恰好是 **29.8s** —— 分布被阈值截断的典型指纹
//   （需要 >30s 的请求全被杀掉，所以"成功"样本里永远见不到 >30s）
// - 因果链数量级 1:1 对应（120min 窗）：30s 超时 64 → 换号 106 → 全池冷却 429 **71**
//
// 为什么池里只有 1 个可用号时后果被放大：超时 → 判该号不可用 → 换号 → 无号可换
// → 全池冷却 → **429 给客户端**。单号池下「换号」这个补救动作必然失败。
//
// 取 90s 的理由：
// - 覆盖 p90(30s) 与观测到的 p95(45s)，留足余量到长尾（实测 >25s 的成功请求仅占 0.7%
//   ⇒ 放宽阈值的代价极小，收益是救回那 12%）；
// - 仍**远小于** 720s read_timeout，保住「彻底卡死的连接不会拖满 12 分钟」这个初衷
//   —— 那才是本超时存在的真正理由（防第一跳静默拖死整条请求）；
// - ⚠️ 它现在**大于** 45s 墙钟，所以「单跳超时 → 换号」不再保证发生在墙钟内。
//   这是**刻意的取舍**：单号池下换号本来就救不了（无号可换），
//   与其在 30s 掐断一个本会成功的请求去换一个不存在的号，不如让它跑完。
//   多号池下墙钟仍会在下一轮循环顶部生效，不会无限拖。
pub const FIRST_BYTE_TIMEOUT_SECS: u64 = 90;

pub async fn forward(
    cred: &KiroCredentials,
    raw_body: Bytes,
    global_proxy: Option<&crate::http_client::ProxyConfig>,
    tls_backend: TlsBackend,
    global_deepseek_cfg: &crate::kiro::deepseek_normalize::DeepseekNormalizeConfig,
    // 全局模型映射规则（`config.model_mapping`，调用方每次请求快照一次）。
    // 在 `forward` **内部**应用：与 deepseek 归一化同处，可保证「先映射 → 再归一化」
    // 的顺序，且 `select_custom_api` 选号（映射前）与改写（映射后）自然分层。
    model_mapping: &std::collections::HashMap<String, String>,
    // 客户端原始请求头（P3：按白名单转发 `anthropic-beta` 等；`None` = 不转发任何客户端头）。
    client_headers: Option<&header::HeaderMap>,
) -> (Response, StatusCode, String) {
    let base = match cred.base_url.as_deref() {
        Some(b) if !b.trim().is_empty() => b.trim_end_matches('/').to_string(),
        _ => {
            return (
                err_response(StatusCode::BAD_GATEWAY, "自定义 API 凭据缺少 base_url"),
                StatusCode::BAD_GATEWAY,
                // 本地错误（还没打到上游）：无上游错误体，给空串。
                String::new(),
            );
        }
    };
    // Anthropic messages 端点:base 已含 /v1 则不重复拼;否则补 /v1/messages。
    // ⚠️ 只认 `ends_with("/v1")`（2026-08-15）：`contains("/v1/")` 过宽 —— base 中间
    // 含 `/v1/`（如 `https://x.com/v1/gateway`）时旧判定误认「已含挂载点」，拼出
    // `/v1/gateway/messages`，而正确形态是补 `/v1/messages`。
    let url = messages_endpoint(&base);

    // 透传用流式 client:read_timeout(空闲间隔)而非总超时,防长回复被中途掐断
    // (与 Kiro 对话路径同款,根因见 build_streaming_client 注释)。
    // **禁重定向 + 运行时 SSRF 复验 + DNS 固化**(M8):写入 base_url 时已校验目标非内网,
    // 但公网中转站若返回 302→内网/元数据仍能绕过,禁重定向堵死这条链;域名在运行时
    // 重新解析、DNS rebinding 可绕过写入时校验,`pinned_streaming_client` 每次出站前
    // 用与写入时相同策略复验并固化解析结果(本机代挂 127.0.0.1 等合法配置不受影响)。
    let proxy = cred.effective_proxy(global_proxy);
    let client = match crate::http_client::pinned_streaming_client(&base, proxy.as_ref(), tls_backend)
        .await
    {
        Ok(c) => c,
        Err(e) => {
            // 归入 `connect_error:` 前缀族:SSRF 复验拒绝是本地判定失败(非上游语义错误),
            // 调用侧据此不重试、直接换号(该号当前出站目标不可信,重试无意义)。
            return (
                err_response(
                    StatusCode::BAD_GATEWAY,
                    &format!("透传出站目标校验失败: {e}"),
                ),
                StatusCode::BAD_GATEWAY,
                format!("connect_error: 透传出站目标校验失败: {e}"),
            );
        }
    };

    // deepseek 归一化:opencodezen 代挂凭据(deepseekNormalize=true)时,转发前按 fuckopencode
    // 的 deepseek 协议修复改写请求体(模型名→deepseek-v4-flash、thinking adaptive→enabled、
    // reasoning_effort→output_config、多轮 tool_use 注入 thinking、剥 context_management 等),
    // 再原样转发;其余 custom_api 凭据保持零转换透传。
    // ⚠️ 响应侧 thinking 过滤(thinking disabled 时 deepseek 仍吐 thinking,客户端会报
    // "Tool result missing")在下方按 content-type 分流处理,仅 deepseek_normalize=true 启用。
    //
    // 配置提前到作用域（body 处理 + 响应过滤共用）：per-凭据覆盖全局标量，bool 取全局。
    let ds_cfg: Option<crate::kiro::deepseek_normalize::DeepseekNormalizeConfig> =
        if cred.deepseek_normalize == Some(true) {
            Some(
                cred
                    .deepseek_normalize_config
                    .as_ref()
                    .map(|c| c.merge_over(global_deepseek_cfg))
                    .unwrap_or_else(|| global_deepseek_cfg.clone()),
            )
        } else {
            None
        };
    // body 处理链：**先映射 → 再 deepseek 归一化**（顺序承重，反序会让 deepseek 先
    // 把名压成 fallback、映射规则再也匹配不到原始名）。模型映射只对 custom_api 号在
    // 非豁免时生效；`select_custom_api` 选号用**映射前**名（决定 3：白名单管原始名）。
    //
    // ⚠️ 一个**设计明确接受的不对称**（非 bug，见 `model_mapping` 模块文档）：选号侧
    // 预判的是 deepseek 改写后的名（`token_manager.rs` select_custom_api），映射不进
    // 预判 ⇒ 「映射后名该号上游不认」时仍可能选中该号 → 上游 400，由凭据豁免覆盖。
    let exempt = cred.model_mapping_exempt == Some(true);
    let body_bytes: Bytes = {
        let parsed = serde_json::from_slice::<serde_json::Value>(&raw_body);
        match parsed {
            Ok(mut v) => {
                // ① 全局模型映射（非豁免时）。透传请求体的模型名在顶层 `model` 字段
                // （Anthropic 格式），与 Kiro 主路径的
                // `/conversationState/currentMessage/userInputMessage/modelId` 不同。
                if !exempt {
                    if let Some(model) = v.get("model").and_then(|m| m.as_str()) {
                        if let Some(target) =
                            crate::kiro::model_mapping::map_target(model, model_mapping)
                        {
                            v["model"] = serde_json::json!(target);
                        }
                    }
                }
                // ② deepseek 归一化（仅该号开启时）。
                if let Some(cfg) = &ds_cfg {
                    crate::kiro::deepseek_normalize::normalize_request(
                        &mut v,
                        cfg,
                        cred.allowed_models.as_deref(),
                    );
                }
                serde_json::to_vec(&v).map(Bytes::from).unwrap_or_else(|_| raw_body.clone())
            }
            Err(_) => raw_body.clone(), // 非 JSON(理论不该出现),回落原样透传
        }
    };

    // 组装转发请求:换上该凭据的 api_key(Anthropic 双头兼容:x-api-key + Authorization),
    // 带上 anthropic-version(上游中转站通常要求),content-type json。发送处理后的 body。
    let mut req = client
        .post(&url)
        .header(header::CONTENT_TYPE, "application/json")
        .header("anthropic-version", "2023-06-01")
        .body(body_bytes);
    if let Some(key) = cred.api_key.as_deref().filter(|k| !k.is_empty()) {
        req = req
            .header("x-api-key", key)
            .header(header::AUTHORIZATION, format!("Bearer {key}"));
    }

    // 🔴 P3：**客户端请求头按白名单转发**。
    //
    // 改前只设上面四个头，客户端的 `anthropic-beta` 被整个丢掉 —— 而 1M 上下文变体
    // **依赖**这个头（主路径 `endpoint/ide.rs` 对 1M 变体显式注入
    // `anthropic-beta: context-1m-2025-08-07`，`model_catalog.rs` 说明 `[1m]` 变体依赖它）。
    // ⇒ 1M 变体走代挂路径时上游拿不到该头、1M 窗口不被放开。这是与主路径的**实际行为
    // 偏差**（线上 2h 内实测有 32 次 1M 请求），不是规范洁癖。
    //
    // 白名单而非黑名单：转发未知头有真实风险（`host`/`content-length` 会让上游收到
    // 矛盾的元信息，`authorization` 会把客户端 key 泄给中转站）。所以只放行确定安全的。
    if let Some(src) = client_headers {
        for (name, value) in src.iter() {
            let n = name.as_str();
            // 🔴 `accept-encoding` **刻意不转发**（2026-08-09 排查 `Failed to parse JSON`）。
            //
            // 因果链：reqwest 没开 gzip/brotli feature（Cargo.toml 的 reqwest features 无
            // gzip/deflate/brotli）→ 它不会自动发 `accept-encoding`，也不会自动解压。
            // 若我们把客户端的 `accept-encoding` 原样转发，上游会真的回 gzip 压缩体，
            // 而网关拿到的 body 是**压缩字节**、又不透传 `content-encoding` 标记
            // ⇒ 客户端按明文 JSON 解析 gzip → `Failed to parse JSON`。
            //
            // 修法：不透传 `accept-encoding`（让 reqwest 不发压缩请求），同时下面
            // `apply_upstream_response_headers` 透传 `content-encoding` 兜底（万一上游
            // 仍回压缩体，客户端能识别）。两条配合，任何上游都不会再触发这个错。
            let allow = n == "anthropic-beta"
                || n == "accept"
                // x-stainless-*：Anthropic SDK 的客户端标识；部分上游按它判断行为。
                || n.starts_with("x-stainless-");
            if !allow {
                continue;
            }
            // ⚠️ 刻意**不**转发这些（各有具体理由，别"顺手"加回来）：
            // - host / content-length / transfer-encoding / connection：本层重写，
            //   转发会让上游收到与实际 body 矛盾的元信息。
            // - authorization / x-api-key：已换成本凭据的 key，转发客户端的等于泄露。
            // - x-forwarded-*：`trustForwardedHeader` 保持 false 是刻意的
            //   （sub2api 不转发 XFF，开了也拿不到真实 IP，反而会让 IP 黑名单封掉反代自己）。
            req = req.header(name.clone(), value.clone());
        }
    }

    let send_fut = req.send();
    let sent = match tokio::time::timeout(
        std::time::Duration::from_secs(FIRST_BYTE_TIMEOUT_SECS),
        send_fut,
    )
    .await
    {
        Ok(r) => r,
        Err(_elapsed) => {
            tracing::warn!(
                base_url = %base,
                timeout_secs = FIRST_BYTE_TIMEOUT_SECS,
                "[透传] 上游 {}s 未返回响应头，判定该号本次不可用并换号（不动 720s read_timeout，\
                 长回复传输不受影响）",
                FIRST_BYTE_TIMEOUT_SECS
            );
            // 归 502：与「连接层失败」同类（都没拿到上游语义响应），调用侧的
            // `should_failover` 对 5xx 会换号，正是我们要的处置。
            return (
                err_response(
                    StatusCode::BAD_GATEWAY,
                    &format!("透传上游 {FIRST_BYTE_TIMEOUT_SECS}s 未返回响应头"),
                ),
                StatusCode::BAD_GATEWAY,
                // 用 `connect_error:` 前缀与连接层失败保持同一归类前缀，便于 trace 侧统计。
                format!("connect_error: first byte timeout after {FIRST_BYTE_TIMEOUT_SECS}s"),
            );
        }
    };
    let upstream = match sent {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("[透传] 上游请求失败({}): {e}", url);
            // 连接层错误:上游不可达/超时,归 502(调用侧据此计一次失败)。
            return (
                err_response(StatusCode::BAD_GATEWAY, &format!("透传上游请求失败: {e}")),
                StatusCode::BAD_GATEWAY,
                // 连接层失败：把 reqwest 错误文本作为"上游错误体"透出，供调用侧分类
                // （它能区分超时/DNS/TLS，与上游真返的 4xx/5xx 语义不同）。
                format!("connect_error: {e}"),
            );
        }
    };

    let status = upstream.status();
    // 保留上游 content-type(流式为 text/event-stream,非流式为 application/json)。
    let content_type = upstream
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/json")
        .to_string();

    // 🔴 P4 前置：**非 2xx 时把上游错误体读出来**（成功响应绝不碰，仍走原样流式）。
    //
    // 为什么必须读：改前上游的真实错误被完全丢弃，日志里只剩一个 `status=502` /
    // `status=400`。而线上实测这些码背后是**完全不同的故障**，处置方式也不同：
    //   - `{"type":"GoUsageLimitError","message":"Weekly usage limit reached. Resets in 19hr"}`
    //     → 额度用尽（该号今天别再选了）
    //   - `{"message":"Invalid tool use format.","reason":"REQUEST_BODY_INVALID"}`
    //     → 请求体问题，但**换个上游可能就认**（5 个代挂号指向 5 个不同上游）
    //   - `{"message":"Invalid model...","reason":"INVALID_MODEL_ID"}`
    //     → 该上游不认这个模型，换号可能成功
    // 不读 body 就无法区分，只能一律当"客户端错误"直返 —— 那正是下游吃到错误的原因。
    //
    // 上限 64 KiB：错误 JSON 都很小；给足余量又不至于被恶意上游用超大 body 顶爆内存。
    // 读失败/超限时退化成空串（fail-open：宁可少一条诊断信息，也不改变转发行为）。
    const UPSTREAM_ERR_PEEK_CAP: u64 = 64 * 1024;
    if !status.is_success() {
        // ⚠️ 必须在 read_body_capped **之前**克隆响应头 —— 它会消费 `upstream`。
        // P4 的关键场景就在这条路径上：429 的 `Retry-After` 只有这里能拿到。
        let upstream_headers = upstream.headers().clone();
        // 🔴 M7：错误体保留**原始字节**回给客户端。`read_body_capped` 不解压，
        // 而白名单已透传 `content-encoding`（见 apply_upstream_response_headers）——
        // 上游若回 gzip 错误体，客户端必须拿到**压缩字节**才能正确解压；改前
        // `String::from_utf8_lossy(&b)` 把压缩字节破坏成 UTF-8 替换字符，客户端按
        // content-encoding 解压必失败。诊断串另用 lossy 副本：日志与调用侧 failover
        // 分类（子串匹配）读的是文本语义，与透传字节无关。
        let err_body_raw = read_body_capped(upstream, "透传上游错误体", UPSTREAM_ERR_PEEK_CAP)
            .await
            .ok()
            .unwrap_or_default();
        let err_body = err_diag_string(&err_body_raw);
        // 日志按长度截断，避免超长 body 刷爆日志（诊断只需要开头那段 message/reason）。
        let peek: String = err_body.chars().take(400).collect();
        tracing::warn!(
            status = status.as_u16(),
            base_url = %base,
            upstream_error = %peek,
            "[透传] 上游返回非 2xx —— 上游错误原文（供分类：额度/模型/请求体）"
        );
        // body 已被消费，只能重新构造响应回给客户端（内容逐字节保持上游原文）。
        let resp = build_error_passthrough_response(status, &content_type, &upstream_headers, err_body_raw);
        return (resp, status, err_body);
    }

    // 原样把上游字节流转回客户端——不解析、不改写。上游怎么发,客户端怎么收。
    //
    // 🔴 修复的缺陷:此处原先是 `Err(e) => Ok(Bytes::new())`,即把上游中断**映射成一个正常的
    // 空 chunk**。空 chunk 在 HTTP 层完全不可见,于是 chunked body 会以**正常终止**收尾——
    // 客户端拿到 `200 OK` + 一个被截断的响应,判定成功、不重试、把半截内容当完整答案用。
    // 注释写的是"结束流",但 `Ok(_)` 表达的是"这一项没有数据",两者语义相反。
    // 根因是类型签名:`Result<Bytes, Infallible>` 里 `Infallible` **无法表达错误**,
    // 所以当时只剩 `Ok` 可用——是类型选错逼出的错误处理。
    //
    // 为什么严重:静默截断比报错危险得多。号池当前 33% 请求已在 429,截断并不罕见,
    // 而客户端对"成功但内容不全"没有任何恢复手段(它不知道出了问题)。
    //
    // 修法:用 `axum::Error` 让错误**真正传播**。`Body::from_stream` 见到 `Err` 会中止
    // body 并关闭连接,客户端侧得到一个"提前结束且非正常终止"的流 → 可据此判失败并重试。
    // 这正是原注释想表达的语义。`map_err` 只在出错时触发一次,不改变正常路径。
    //
    // ⚠️ 不在此处加重试:重试属 provider 层(见 try_custom_api_passthrough 的 failover)。
    // 在流层重试会绕过已建立的会话亲和绑定 → 破坏前缀缓存(历史教训:换号 = prompt cache
    // 全丢,单请求成本差 10 倍)。
    //
    // 注:这里**不会**因为返回 Err 而形成自旋——实测 reqwest 的 `bytes_stream` 出错后
    // 下一次 poll 返回 `None`,不重复吐同一个 Err;且 `map`/`map_err` 都不改变终止时机。
    //
    // ⚠️ 响应侧过滤分两层，门控不同（2026-08-10 拆开，理由见下方 `strip_dsml_only_ok`）：
    // - **thinking 块过滤**（滤 thinking/redacted_thinking 块 + index 重编号 + usage 扣减）：
    //   改协议结构，仅 `deepseek_normalize=true` 启用，其余号保零转换（隔离铁律 3）。
    // - **DSML 标记剥离**：所有 custom_api 号无条件启用 —— 上游就是 DeepSeek 系，
    //   标记泄漏与凭据配置无关。
    // 流式逐事件处理仍流式回传;非流式读完整 body 处理。解析失败 fail-open 原样透传。
    // 成功路径同样透传 P4 白名单头（x-ratelimit-* 让客户端能主动限速）。
    // 同样必须在消费 upstream 之前克隆。
    let upstream_headers = upstream.headers().clone();
    // 被动配额观测：成功响应里摘上游配额头落日志（纯观测，见该函数文档）。
    // 放在消费 `upstream` 之前（与下方 P4 白名单克隆同一时机）。
    observe_upstream_quota_headers(&upstream_headers, &base);
    // thinking 块过滤（含重编号、usage 扣减）仍只对 deepseek_normalize 凭据开启 ——
    // 它改协议结构，是「零转换透传」的刻意例外，不能推给所有 custom_api 号。
    let filter_thinking = ds_cfg.is_some();
    // 🔴 但**响应 filter 入口本身不能再由 `filter_thinking` 门控**。
    //
    // 改前入口条件是 `filter_thinking && ...`：没开 deepseek_normalize 的 custom_api 号
    // 走纯字节透传分支 ⇒ DSML 标记剥离**根本不执行**，`<｜DSML｜function_calls｜>` 原样
    // 泄漏给客户端（用户走代挂号拉 OpenZ 时看到的裸标记就是这条路径）。
    //
    // 而 DSML 泄漏与「客户端要不要 thinking」、「凭据有没有开归一化」都无关：
    // custom_api 的上游就是 DeepSeek 系中转站，会吐这个标记的是上游模型本身。
    // 所以 filter 一律接上，只把 thinking 块过滤按 `filter_thinking` 传下去 ——
    // 关闭时 filter 只剥 text 里的 DSML 标记，thinking 块与 index 一律不动。
    let strip_dsml_only_ok = content_type.contains("text/event-stream")
        || content_type.contains("application/json");
    // 内联 `<thinking>` 剥离开关取配置（流式 + 非流式共用，cfg 已提到作用域）。
    // ⚠️ 未开归一化的号取 `false`：内联 `<thinking>` 剥离属于 thinking 语义处理，
    // 不该跟着 DSML 一起对所有 custom_api 号生效（DSML 是标记泄漏，两码事）。
    let strip_inline = ds_cfg
        .as_ref()
        .map_or(false, |c| c.strip_inline_thinking);

    // 模拟缓存注入（mockCacheEnabled，仅透传路径）：上游 DeepSeek 的
    // cache_read_input_tokens 恒 0，下游（sub2api 等）看不到缓存分支 —— 开启后
    // filter 把 usage 注入 round(input × ratio) 的伪造 cache_read、creation 置 0。
    // 读取处从 handlers 的 TIER3 进程镜像取（main 启动接线 / admin 热更即时改写），
    // ratio 已在 setter 清洗到 [0,1]；关闭时 mock_cache=None，filter 零改动原样透传。
    // Kiro 池四层缓存链（handlers.rs resolve_cache_chain）不经过这里，不受影响。
    let (mock_cache_enabled, mock_ratio) = crate::anthropic::handlers::mock_cache_config();
    let mock_cache = mock_cache_enabled.then_some((true, mock_ratio));

    let resp = if strip_dsml_only_ok && content_type.contains("text/event-stream") {
        let byte_stream = upstream.bytes_stream().map_err(|e| {
            tracing::warn!("[透传] 上游流读取中断,以错误终止响应流(客户端可据此重试): {e}");
            axum::Error::new(e)
        });
        apply_upstream_response_headers(
            Response::builder()
                .status(status)
                .header(header::CONTENT_TYPE, content_type),
            &upstream_headers,
        )
            // ⚠️ 空流兜底：thinking 被滤光/上游真空响应时补发 error 事件，
            // 防客户端 "Stream ended without receiving any events" 卡死 agentic 循环。
            // message 保持静态：guard_empty_stream 签名要求 `&'static str`（结构持引用、
            // 流 `'static`），配置表返回 String 无法传（本文件边界内无解，见 err_response
            // 的接入说明——E7 是唯一未接表的 E 表本地构造点）。
            .body(Body::from_stream(guard_empty_stream(
                filter_sse_stream_with(byte_stream, strip_inline, filter_thinking, mock_cache),
                "上游返回空响应（未收到任何正文内容），请重试",
            )))
            .unwrap_or_else(|_| err_response(StatusCode::BAD_GATEWAY, "构建透传响应失败"))
    } else if strip_dsml_only_ok && content_type.contains("application/json") {
        // 非流式:缓冲完整 body 过滤（Content-Length 本就不透传,无需重算）。
        // 用 read_body_capped 给 body 加 32MiB 上限,防恶意上游吐超大 JSON 顶爆内存。
        match read_body_capped(upstream, "透传非流式响应", PASSTHROUGH_JSON_CAP_BYTES).await {
            Ok(body) => {
                // ⚠️ 非流式也要接 strip_inline_thinking 配置（与流式一致，否则配置 false 仍剥）。
                let filtered = filter_json_bytes_with(&body, strip_inline, filter_thinking, mock_cache);
                apply_upstream_response_headers(
                    Response::builder()
                        .status(status)
                        .header(header::CONTENT_TYPE, content_type),
                    &upstream_headers,
                )
                    .body(Body::from(filtered))
                    .unwrap_or_else(|_| err_response(StatusCode::BAD_GATEWAY, "构建透传响应失败"))
            }
            Err(e) => {
                tracing::warn!("[透传] 非流式响应读取失败: {e}");
                err_response(StatusCode::BAD_GATEWAY, "透传非流式响应读取失败")
            }
        }
    } else {
        // 其余 content-type（既非 SSE 也非 JSON）：纯字节透传。
        //
        // 🔴 P2 的空流守卫在这里**不需要**了：SSE 与 JSON 现在一律走上面两条 filter 分支
        // （`strip_dsml_only_ok` 覆盖两者），SSE 分支自带 `guard_empty_stream`。
        // 而非 SSE 的 200 空响应对客户端是合法的（如某些 HEAD 语义），补发 SSE error
        // 事件反而会破坏 content-type 契约 —— 故这条分支刻意不挂守卫。
        let byte_stream = upstream.bytes_stream().map_err(|e| {
            tracing::warn!("[透传] 上游流读取中断,以错误终止响应流(客户端可据此重试): {e}");
            axum::Error::new(e)
        });
        apply_upstream_response_headers(
            Response::builder()
                .status(status)
                .header(header::CONTENT_TYPE, content_type),
            &upstream_headers,
        )
        .body(Body::from_stream(byte_stream))
        .unwrap_or_else(|_| err_response(StatusCode::BAD_GATEWAY, "构建透传响应失败"))
    };
    // 返回上游真实 status 供调用侧推断 outcome(成功/限流/失败);body 已流式接管。
    // 第三项是上游错误体：**成功路径恒为空串**（2xx 的 body 是流式内容，绝不缓冲读取）。
    (resp, status, String::new())
}

/// 探测自定义 API 上游的可用模型列表（`GET {base}/v1/models`，OpenAI 兼容格式）。
///
/// 兼容三种响应形态：`{data:[{id}]}`（OpenAI 标准）、`{models:[...]}`（字符串或对象数组）、
/// 纯数组 `[string]`。排序去重后返回。
///
/// base_url 与 [`forward`] 同源（含 `/v1` 则不重复拼），SSRF 防护走
/// `pinned_streaming_client`（M8：运行时复验 + DNS 固化 + 禁重定向，与写入时
/// 校验同策略）。
pub async fn fetch_upstream_models(
    cred: &KiroCredentials,
    global_proxy: Option<&crate::http_client::ProxyConfig>,
    tls_backend: TlsBackend,
) -> anyhow::Result<Vec<String>> {
    let base = cred
        .base_url
        .as_deref()
        .filter(|b| !b.trim().is_empty())
        .ok_or_else(|| anyhow::anyhow!("自定义 API 凭据缺少 base_url"))?;
    let base = base.trim_end_matches('/');
    // ⚠️ 2026-08-13 修复（nbus 实测 404）：模型列表端点在**上游之间形态不一**——
    // OpenAI 兼容上游认 `{base}/models` 或 `{base}/v1/models`；DeepSeek 的 Anthropic
    // 兼容层（`{base}=.../anthropic`）**不提供** `/anthropic/v1/models`（Claude Code
    // 从不拉模型列表），但同域 OpenAI 端点（剥掉 `/anthropic` 后缀）有 `/models`。
    // 2026-08-13 二修：候选生成改为**智能剥离**——把 `/v1`、`/anthropic`、
    // `/anthropic/v1` 等路径片段逐一剥掉后对每个候选根都生成 `/models` 与
    // `/v1/models` 两种形态，按「最贴近原 base 优先」排序、去重后依次尝试，
    // 首个 2xx 即返回；全部失败时错误信息带上完整候选清单（不再靠猜路径）。
    let mut candidates: Vec<String> = Vec::new();
    {
        let mut roots: Vec<String> = vec![base.to_string()];
        // 剥 /anthropic（DeepSeek 类：anthropic 兼容层无模型列表，OpenAI 层有）
        if let Some(stripped) = base.strip_suffix("/anthropic") {
            roots.push(stripped.to_string());
        }
        // 剥 /v1（OpenAI 兼容层常见挂载点：/v1 下只有 chat/completions，models 在根）
        if base.ends_with("/v1") {
            roots.push(base.trim_end_matches("/v1").to_string());
        }
        if let Some(stripped) = base.strip_suffix("/anthropic/v1") {
            roots.push(stripped.to_string());
        }
        // 每个根生成两种形态；先根路径（多数 OpenAI 上游 /models 就在根），再 /v1/models。
        for root in roots {
            candidates.push(format!("{root}/models"));
            candidates.push(format!("{root}/v1/models"));
        }
        // 去重保序（linked-hash 语义：Vec + contains 检查）。
        let mut deduped: Vec<String> = Vec::with_capacity(candidates.len());
        for c in candidates {
            if !deduped.contains(&c) {
                deduped.push(c);
            }
        }
        candidates = deduped;
    }

    let proxy = cred.effective_proxy(global_proxy);
    // 同样走「运行时 SSRF 复验 + DNS 固化」的 pinned client（M8，与 `forward` 同源，
    // 打的是同一个上游 host，连接池按 (host, proxy, tls) 复用）。
    let client = crate::http_client::pinned_streaming_client(&base, proxy.as_ref(), tls_backend)
        .await?;
    let mut last_err: Option<anyhow::Error> = None;
    for url in &candidates {
        let mut req = client.get(url);
        if let Some(key) = cred.api_key.as_deref().filter(|k| !k.is_empty()) {
            req = req.header("Authorization", format!("Bearer {key}"));
        }
        let resp = match req.send().await {
            Ok(r) => r,
            Err(e) => {
                last_err = Some(anyhow::anyhow!("请求 {url} 失败: {e}"));
                continue;
            }
        };
        if !resp.status().is_success() {
            last_err = Some(anyhow::anyhow!(
                "上游返回 {} 获取模型列表失败（尝试过: {url}）",
                resp.status()
            ));
            continue;
        }
        let body: serde_json::Value = match crate::common::http_read::read_json_capped(
            resp,
            "上游模型列表",
            PASSTHROUGH_MODELS_CAP_BYTES,
        )
        .await
        {
            Ok(b) => b,
            Err(e) => {
                last_err = Some(anyhow::anyhow!("解析 {url} 模型列表失败: {e}"));
                continue;
            }
        };
        // 解析成功即返回（即使列表为空——上游确实没模型也如实返回）。
        let mut models: Vec<String> = Vec::new();
        if let Some(data) = body.get("data").and_then(|v| v.as_array()) {
            for m in data {
                if let Some(id) = m.get("id").and_then(|x| x.as_str()) {
                    models.push(id.to_string());
                }
            }
        }
        if let Some(arr) = body.get("models").and_then(|v| v.as_array()) {
            for m in arr {
                if let Some(s) = m.as_str() {
                    models.push(s.to_string());
                } else if let Some(id) = m.get("id").and_then(|x| x.as_str()) {
                    models.push(id.to_string());
                }
            }
        }
        // 纯数组 [string]
        if let Some(arr) = body.as_array() {
            for m in arr {
                if let Some(s) = m.as_str() {
                    models.push(s.to_string());
                }
            }
        }
        models.sort();
        models.dedup();
        return Ok(models);
    }
    // 全部候选失败：附上候选清单让排障不再靠猜。
    let tried = candidates.join(" | ");
    match last_err {
        Some(e) => Err(anyhow::anyhow!("{e}（全部候选: {tried}）")),
        None => Err(anyhow::anyhow!("无可用模型列表候选（base_url: {base}）")),
    }
    // 旧解析尾部已并入上面的候选循环（含纯数组形态）。
}

/// 拼 Anthropic messages 端点：base 以 `/v1` 结尾（挂载点已含）则不重复拼，否则补。
///
/// ⚠️ 只认 `ends_with("/v1")`（2026-08-15）：`contains("/v1/")` 过宽 —— base 中间含
/// `/v1/` 时（如 `https://x.com/v1/gateway`）旧逻辑误判「已含挂载点」，把 messages
/// 拼成 `/v1/gateway/messages`，而正确形态是补 `/v1/messages`。调用方保证 base
/// 已 trim 尾部斜杠。
fn messages_endpoint(base: &str) -> String {
    if base.ends_with("/v1") {
        format!("{base}/messages")
    } else {
        format!("{base}/v1/messages")
    }
}

/// 上游错误体的诊断串（lossy 副本 + trim）：供日志与调用侧 failover 分类。
/// 与透传回客户端的**原始字节**（`err_body_raw`）分离 —— 压缩字节绝不进这个串，
/// 客户端按白名单透传的 `content-encoding` 解压时不受诊断串影响。
fn err_diag_string(raw: &[u8]) -> String {
    String::from_utf8_lossy(raw).trim().to_string()
}

/// 构造「透传上游错误体」的响应：body 用**原始字节**（配合白名单透传的
/// `content-encoding`，客户端能正确解压），状态码与白名单头保持上游。
fn build_error_passthrough_response(
    status: StatusCode,
    content_type: &str,
    upstream_headers: &header::HeaderMap,
    raw_body: Vec<u8>,
) -> Response {
    apply_upstream_response_headers(
        Response::builder()
            .status(status)
            .header(header::CONTENT_TYPE, content_type),
        upstream_headers,
    )
    .body(Body::from(raw_body))
    .unwrap_or_else(|_| err_response(status, "构造上游错误响应失败"))
}

/// 构建一个 Anthropic 风格的错误响应(供透传失败时返回)。
///
/// 错误消息可配置化接入：所有本地构造错误（E1 缺 base_url / E2 出站校验失败 /
/// E3 首字节超时 / E4 连接层失败 / E8 非流式读取失败 / E9 构建失败）统一读
/// `passthrough_failed` key —— status/type/message/retryAfterSecs 均可配，
/// 默认值 = 各调用点的现状文案（未配置零行为变化）。
/// ⚠️ **上游错误原文透传（E5，build_error_passthrough_response）不经过本函数**：
/// 那是上游 status + 原始字节 + 白名单头，网关零构造，配置对其无效。
fn err_response(status: StatusCode, msg: &str) -> Response {
    let (status_cfg, error_type, message, retry_after) =
        crate::anthropic::handlers::resolve_msg(
            &crate::anthropic::handlers::current_error_messages(),
            "passthrough_failed",
            (status, "api_error", msg, None),
        );
    let body = serde_json::json!({
        "type": "error",
        "error": { "type": error_type, "message": message }
    });
    let mut resp = (status_cfg, axum::Json(body)).into_response();
    // 配置给了 retryAfterSecs 才带 Retry-After 头（默认 None 与现状一致——502 本地
    // 错误不带退避提示；管理员显式配置后，客户端可据此退避而非原样重发）。
    if let Some(ra) = retry_after {
        resp.headers_mut()
            .insert(header::RETRY_AFTER, ra.clamp(1, 300).to_string().parse().expect("u64 to_string 恒为合法 HeaderValue"));
    }
    resp
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;

    /// 回归：上游流中断必须**以错误终止** body，绝不能伪装成正常 EOF。
    ///
    /// **旧代码为何 FAIL**：原实现 `Err(e) => Ok(Bytes::new())` 把中断映射成一个正常的空 chunk。
    /// 空 chunk 在 HTTP 层不可见 → chunked body 正常收尾 → 客户端拿到 `200 OK` + 截断内容，
    /// 判定成功、不重试、把半截答案当完整结果用。旧代码下最后一项是 `Ok(b"")` 而非 `Err`，
    /// 本测试的 `is_err()` 断言必然 FAIL。
    ///
    /// 静默截断比报错危险：客户端对「成功但内容不全」没有任何恢复手段（它不知道出了问题）。
    /// 号池当前有三分之一请求在 429，截断并不罕见。
    ///
    /// 这里直接测 `map_err` 这一层的语义（与生产同款闭包），不依赖真实网络——
    /// `forward` 需要真上游，而缺陷恰恰在这个映射本身。
    #[tokio::test]
    async fn upstream_stream_interruption_terminates_body_with_error_not_silent_eof() {
        // 造「两个正常 chunk 后中断」的上游流，错误类型用 reqwest 的真实错误无法手工构造，
        // 故用 std::io::Error 代表传输层失败——map_err 的语义与错误具体类型无关。
        let upstream = futures::stream::iter(vec![
            Ok::<Bytes, std::io::Error>(Bytes::from_static(b"data: a\n\n")),
            Ok(Bytes::from_static(b"data: b\n\n")),
            Err(std::io::Error::other("connection reset by peer")),
        ]);

        // 与生产同款：错误传播而非吞成空 chunk。
        let mapped = upstream.map_err(axum::Error::new);
        let items: Vec<_> = mapped.collect().await;

        assert_eq!(items.len(), 3, "两个数据项 + 一个错误项");
        assert!(items[0].is_ok() && items[1].is_ok(), "正常 chunk 不受影响");
        assert!(
            items[2].is_err(),
            "上游中断必须传播为 Err（旧代码是 Ok(空 chunk) → 客户端把截断响应当成功）"
        );
        // 反向守卫：绝不能是"成功的空 chunk"这种最隐蔽的形式。
        assert!(
            !matches!(&items[2], Ok(b) if b.is_empty()),
            "空 chunk 在 HTTP 层不可见，等于静默截断"
        );
    }

    // ===== M7：错误体原始字节透传（2026-08-15）=====

    /// M7 守卫：非 2xx 错误体必须**逐字节**回给客户端（`content-encoding` 已由
    /// 白名单透传 —— 上游若回 gzip，客户端要拿到压缩字节才能解压）。
    ///
    /// 回退即 FAIL：把 body 换成 `String::from_utf8_lossy(&b)` 的字符串 ——
    /// 二进制字节被替换成 U+FFFD，body 与 `content-encoding: gzip` 矛盾，
    /// 客户端解压必失败。
    #[tokio::test]
    async fn error_response_body_preserves_raw_bytes() {
        // 合法的 gzip 流头（1f 8b ...）—— 非 UTF-8 字节，lossy 会破坏它。
        let raw: Vec<u8> = vec![0x1f, 0x8b, 0x08, 0x00, 0xde, 0xad, 0xbe, 0xef, 0x01, 0x00];
        let mut headers = axum::http::HeaderMap::new();
        headers.insert("content-encoding", axum::http::HeaderValue::from_static("gzip"));
        let resp = build_error_passthrough_response(
            StatusCode::TOO_MANY_REQUESTS,
            "application/json",
            &headers,
            raw.clone(),
        );
        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(
            resp.headers().get("content-encoding").map(|v| v.to_str().unwrap()),
            Some("gzip"),
            "content-encoding 必须透传（白名单契约）"
        );
        let body = axum::body::to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
        assert_eq!(
            body.as_ref(),
            raw.as_slice(),
            "错误体必须逐字节透传（gzip 场景客户端靠它解压）"
        );
    }

    /// M7：诊断串是 lossy + trim 的副本 —— 压缩/二进制字节进诊断串必须被替换
    /// （不能 panic、不能悄悄丢字节改变透传行为），文本错误体则无损保留。
    #[test]
    fn err_diag_string_is_lossy_trimmed_copy() {
        assert_eq!(err_diag_string(b"{\"a\":1}\n\n"), "{\"a\":1}");
        // 全非法 UTF-8 起始字节（压缩流的典型内容）：每个字节替换为 U+FFFD。
        let gzipish = [0xff, 0xfe, 0x80];
        let s = err_diag_string(&gzipish);
        assert_eq!(
            s,
            "\u{FFFD}\u{FFFD}\u{FFFD}",
            "lossy 是逐字节替换副本（合法 ASCII 如 gzip 魔数 0x1f 不替换）"
        );
    }

    // ===== MINOR 3：URL 拼接只认 ends_with(\"/v1\")（2026-08-15）=====

    /// 回退即 FAIL：改回 `contains("/v1/")` —— 中间含 `/v1/` 的 base 会被误判
    /// 「已含挂载点」，拼出错误端点。
    #[test]
    fn messages_endpoint_only_checks_trailing_v1() {
        assert_eq!(
            messages_endpoint("https://api.example.com/v1"),
            "https://api.example.com/v1/messages"
        );
        assert_eq!(
            messages_endpoint("https://api.example.com"),
            "https://api.example.com/v1/messages"
        );
        // 修复点：中间含 /v1/ 但非 /v1 结尾 —— 必须补 /v1/messages。
        assert_eq!(
            messages_endpoint("https://api.example.com/v1/gateway"),
            "https://api.example.com/v1/gateway/v1/messages"
        );
        assert_eq!(
            messages_endpoint("https://api.example.com"),
            "https://api.example.com/v1/messages"
        );
    }

    // ===== MINOR 4：配额头观测补 anthropic-ratelimit-*（2026-08-15）=====

    /// anthropic-ratelimit-requests-* 三键必须被观测到（Anthropic 兼容上游的配额
    /// 余量只发这一族，缺它则观测恒空）；x-ratelimit-* 优先。
    #[test]
    fn quota_headers_observes_both_namespaces() {
        use axum::http::HeaderMap;
        let mut h = HeaderMap::new();
        h.insert(
            "anthropic-ratelimit-requests-limit",
            "40".parse().unwrap(),
        );
        h.insert(
            "anthropic-ratelimit-requests-remaining",
            "32".parse().unwrap(),
        );
        h.insert(
            "anthropic-ratelimit-requests-reset",
            "2026-01-01T00:00:00Z".parse().unwrap(),
        );
        let (l, r, rs) = quota_headers(&h);
        assert_eq!(l, Some("40"), "anthropic-ratelimit-requests-limit 必须被观测");
        assert_eq!(r, Some("32"), "anthropic-ratelimit-requests-remaining 必须被观测");
        assert_eq!(
            rs,
            Some("2026-01-01T00:00:00Z"),
            "anthropic-ratelimit-requests-reset 必须被观测"
        );

        // 两族同发时 x-ratelimit-* 优先。
        let mut h2 = HeaderMap::new();
        h2.insert("x-ratelimit-limit", "10".parse().unwrap());
        h2.insert("anthropic-ratelimit-requests-limit", "40".parse().unwrap());
        let (l, _, _) = quota_headers(&h2);
        assert_eq!(l, Some("10"), "x-ratelimit-* 应优先于 anthropic-ratelimit-*");

        // 全缺 → 全 None（observe 据此静默返回）。
        let h3 = HeaderMap::new();
        let (l, r, rs) = quota_headers(&h3);
        assert!(l.is_none() && r.is_none() && rs.is_none());
    }
}
