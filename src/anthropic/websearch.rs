//! WebSearch 工具处理模块
//!
//! 实现 Anthropic WebSearch 请求到 Kiro MCP 的转换和响应生成。
//!
//! 三条路径：
//! 1. 单轮快路径 `handle_websearch_request`：请求带 web_search 且显式触发搜索
//!    （tool_choice 强制 / 纯单工具 / CC 前缀），网关内部直接调 MCP 拿结果、本地合成
//!    一条 SSE/JSON 响应，不等上游模型。覆盖「客户端已知要搜什么」的场景。
//! 2. 常规转发（本模块不动）：其余请求走 Kiro 主路径。历史上混合工具场景会把
//!    web_search 从 tools 里剔掉再转发（`strip_web_search_tools` / converter 的过滤），
//!    副作用是模型永远看不到搜索工具 → CC 的 WebSearch 静默失效。
//! 3. 多轮回灌 `run_web_search_loop`（本文件底部）：混合工具场景改走它，上游回
//!    web_search tool_use 时网关内部调 MCP → 把 web_search_tool_result 回灌进下一轮
//!    请求 → 重新转换重发，最多 [`MAX_WEB_SEARCH_ROUNDS`] 轮。非 web_search 的 tool_use
//!    照常回给客户端。
//!
//! 混合工具场景（web_search 与其他工具共存）的识别/剥离与路由判定思路，
//! 吸收自 Foxfishc__kiro.rs（MIT License），在此致谢。

use std::convert::Infallible;

use axum::{
    body::Body,
    http::{StatusCode, header},
    response::{IntoResponse, Json, Response},
};
use bytes::Bytes;
use futures::{Stream, stream, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use super::stream::SseEvent;
use super::types::{ErrorResponse, Message, MessagesRequest, Tool};

/// WebSearch agentic 回灌的最大轮数。超出后把当前轮的 web_search tool_use 原样
/// 回给客户端（客户端拿到它会再发一轮带结果的新请求，链路不丢、只是多一轮往返），
/// 防止上游持续要搜索时网关陷入无限循环。
const MAX_WEB_SEARCH_ROUNDS: usize = 5;

/// 一条已解码完成的上游 web_search tool_use。
#[derive(Debug, Clone)]
struct DecodedWebSearch {
    /// 上游 tool_use 的 id。回灌进历史时 assistant tool_use 与 user tool_result
    /// **必须**用它配对（converter 的 validate_tool_pairing 会丢弃孤立 tool_use）。
    id: String,
    query: String,
}

/// 一条已完成搜索的 web_search：上游 tool_use + MCP 结果 + 面向客户端的 server_tool_use id。
struct SearchedWebSearch {
    /// 上游 tool_use id（回灌给 Kiro 的历史配对用，见 [`DecodedWebSearch::id`]）
    upstream_id: String,
    query: String,
    /// MCP 调用生成的 `srvtoolu_*` id（面向客户端的 server_tool_use /
    /// web_search_tool_result 用它配对，与快路径 create_mcp_request 同来源）
    srv_id: String,
    results: Option<WebSearchResults>,
}

/// 判断一轮上游 tool_use 是否「纯 web_search」：非空且全部是 web_search。
///
/// 与参考仓 should_search_round（websearch_loop.rs:177）同语义：一旦混入 exec 等
/// 客户端工具，就不再回灌（exec 原样回给客户端，绝不吞掉）。`round_idx` 超限时
/// 即便纯 web_search 也不再继续回灌（见 MAX_WEB_SEARCH_ROUNDS）。
fn should_replay_round(round_idx: usize, web_search: &[DecodedWebSearch], has_client_tool_use: bool) -> bool {
    !web_search.is_empty()
        && !has_client_tool_use
        && round_idx < MAX_WEB_SEARCH_ROUNDS
}

/// 回灌辅助：把上一轮上游返回的 assistant 内容（文本 + web_search tool_use）追加进
/// `payload.messages`，紧跟着追加一条带 web_search_tool_result 的 user 消息，并返回
/// 面向客户端展示的 server_tool_use + web_search_tool_result 事件块。
///
/// 与参考仓 append_search_round（websearch_loop.rs:418）对齐：
/// - Kiro 历史要求 tool_use ↔ tool_result **配对**（converter 的 validate_tool_pairing /
///   remove_orphaned_tool_uses 会丢弃孤立 tool_use），所以 assistant 的 web_search
///   tool_use 必须紧跟一条 user tool_result；
/// - 回灌给上游的内容用搜索摘要（generate_search_summary），不是原始 SSE 块
///   （`web_search_tool_result` 是 Anthropic 客户端契约，上游不认）；
/// - 两套 id **不能混用**：回灌给上游的 tool_use/tool_result 用**上游**的 tool_use_id
///   （`SearchedWebSearch::upstream_id`，Kiro 侧配对靠它）；面向客户端的
///   server_tool_use / web_search_tool_result 用 MCP 生成的 `srvtoolu_*`
///   （`srv_id`，与快路径 create_mcp_request 同来源）。混用会让其中一侧配不上。
///
/// `assistant_text` 是本轮上游返回的正文（可能为空）：一并回灌，否则模型下一轮看不到
/// 自己刚说过的话，容易重复同一次搜索。
///
/// 返回面向客户端展示的事件块（每次搜索两个：server_tool_use + web_search_tool_result），
/// 由调用方累积起来在收尾时渲染。`payload` 被就地追加，即下一轮请求体。
fn append_search_round(
    payload: &mut MessagesRequest,
    assistant_text: &str,
    searched: &[SearchedWebSearch],
) -> Vec<Value> {
    // assistant：本轮正文 + 本轮的 web_search tool_use。
    // tool_use 必须与紧随其后的 user tool_result 成对，否则 converter 会把它当孤立
    // tool_use 从历史里删掉（remove_orphaned_tool_uses），下一轮上游看不到搜过什么。
    let mut assistant_content: Vec<Value> = Vec::new();
    if !assistant_text.is_empty() {
        assistant_content.push(json!({"type": "text", "text": assistant_text}));
    }
    for s in searched {
        assistant_content.push(json!({
            "type": "tool_use",
            "id": s.upstream_id,
            "name": "web_search",
            "input": {"query": s.query}
        }));
    }
    payload.messages.push(Message {
        role: "assistant".to_string(),
        content: Value::Array(assistant_content),
    });

    let mut presentation: Vec<Value> = Vec::new();
    let mut user_content: Vec<Value> = Vec::new();
    for s in searched {
        // 回灌给上游的是**摘要文本**，不是 web_search_tool_result 块 ——
        // 后者是 Anthropic 客户端契约，Kiro 上游只认 tool_result 的纯文本内容。
        let summary = generate_search_summary(&s.query, &s.results);
        user_content.push(json!({
            "type": "tool_result",
            "tool_use_id": s.upstream_id,
            "content": summary
        }));

        presentation.push(json!({
            "type": "server_tool_use",
            "id": s.srv_id,
            "name": "web_search",
            "input": {"query": s.query}
        }));
        // ⚠️ 带 tool_use_id 且等于前面 server_tool_use 的 id：这是快路径
        // generate_websearch_events 已经坐实的一条（见那里的注释：缺此字段严格
        // SDK 客户端无法配对、typed 反序列化失败）。参考仓 ref-grey 没带，
        // 我们**刻意不跟**，保持与本仓快路径同一契约。
        presentation.push(json!({
            "type": "web_search_tool_result",
            "tool_use_id": s.srv_id,
            "content": build_result_block(&s.results)
        }));
    }
    payload.messages.push(Message {
        role: "user".to_string(),
        content: Value::Array(user_content),
    });

    presentation
}

/// 把搜索结果转换成 web_search_result 块数组（Anthropic 客户端契约）。
///
/// 与参考仓 build_result_block（websearch_loop.rs:480）对齐；`page_age` 用
/// `publishedDate` 毫秒时间戳格式化为 "月 日, 年"。None / 无结果 → 空数组
/// （空结果对客户端是合法的"没搜到"）。
///
/// title/snippet 走 `normalize_html_text`：MCP 返回的是第三方网页抽取文本，常带
/// `<br>`/`&nbsp;` 残留。这与快路径 generate_websearch_events 的处置一致 ——
/// 参考仓 ref-grey 原样透传，我们不跟（否则客户端会看到裸标签）。
fn build_result_block(results: &Option<WebSearchResults>) -> Vec<Value> {
    match results {
        Some(r) => r
            .results
            .iter()
            .map(|item| {
                let page_age = item.published_date.and_then(|ms| {
                    chrono::DateTime::from_timestamp_millis(ms)
                        .map(|dt| dt.format("%B %-d, %Y").to_string())
                });
                json!({
                    "type": "web_search_result",
                    "title": normalize_html_text(&item.title),
                    "url": item.url,
                    "encrypted_content": item.snippet.as_deref().map(normalize_html_text).unwrap_or_default(),
                    "page_age": page_age
                })
            })
            .collect(),
        None => vec![],
    }
}

/// Claude Code 风格的 WebSearch 查询前缀
const WEB_SEARCH_PREFIX: &str = "Perform a web search for the query: ";

/// MCP 请求
#[derive(Debug, Serialize)]
pub struct McpRequest {
    pub id: String,
    pub jsonrpc: String,
    pub method: String,
    pub params: McpParams,
}

/// MCP 请求参数
#[derive(Debug, Serialize)]
pub struct McpParams {
    pub name: String,
    pub arguments: McpArguments,
}

/// MCP 参数
#[derive(Debug, Serialize)]
pub struct McpArguments {
    pub query: String,
}

/// MCP 响应
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct McpResponse {
    pub error: Option<McpError>,
    pub id: String,
    pub jsonrpc: String,
    pub result: Option<McpResult>,
}

/// MCP 错误
#[derive(Debug, Deserialize)]
pub struct McpError {
    pub code: Option<i32>,
    pub message: Option<String>,
}

/// MCP 结果
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct McpResult {
    pub content: Vec<McpContent>,
    #[serde(rename = "isError")]
    pub is_error: bool,
}

/// MCP 内容
#[derive(Debug, Deserialize)]
pub struct McpContent {
    #[serde(rename = "type")]
    pub content_type: String,
    pub text: String,
}

/// WebSearch 搜索结果
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct WebSearchResults {
    pub results: Vec<WebSearchResult>,
    #[serde(rename = "totalResults")]
    pub total_results: Option<i32>,
    pub query: Option<String>,
    pub error: Option<String>,
}

/// 单个搜索结果
#[derive(Debug, Deserialize, Clone)]
#[allow(dead_code)]
pub struct WebSearchResult {
    pub title: String,
    pub url: String,
    pub snippet: Option<String>,
    #[serde(rename = "publishedDate")]
    pub published_date: Option<i64>,
    pub id: Option<String>,
    pub domain: Option<String>,
    #[serde(rename = "maxVerbatimWordLimit")]
    pub max_verbatim_word_limit: Option<i32>,
    #[serde(rename = "publicDomain")]
    pub public_domain: Option<bool>,
}

/// 判断单个工具是否为 web_search 工具。
///
/// 兼容两种客户端形态：
/// - name 为 "web_search"
/// - name 缺失、仅通过 type（如 "web_search_20250305"）声明
fn tool_is_web_search(t: &Tool) -> bool {
    t.name == "web_search"
        || t.tool_type
            .as_deref()
            .is_some_and(|ty| ty.starts_with("web_search"))
}

/// 判断上游返回的 tool_use 是否 web_search。
///
/// 与入站 `tool_is_web_search` 同判据（name == "web_search"）：上游只认识 name，
/// 不会带 type；但历史消息里可能有客户端回灌的带 type 形态，两处都用同一谓词避免漂移。
fn tool_use_name_is_web_search(name: &str) -> bool {
    name == "web_search"
}

/// 检查请求的 tools 是否包含 WebSearch 工具。
///
/// 只要 tools 中出现 web_search（按 name 或 type 判断）即返回 true，
/// **不要求 web_search 是唯一工具**，因此可覆盖“web_search + 其他工具”的混合场景。
pub fn has_web_search_tool(req: &MessagesRequest) -> bool {
    req.tools
        .as_ref()
        .is_some_and(|tools| tools.iter().any(tool_is_web_search))
}

/// tool_choice 是否强制选择 web_search。
///
/// Anthropic 常见形态：{"type":"tool","name":"web_search"}
fn tool_choice_requests_web_search(req: &MessagesRequest) -> bool {
    let Some(choice) = req.tool_choice.as_ref() else {
        return false;
    };
    let Some(obj) = choice.as_object() else {
        return false;
    };

    let name = obj
        .get("name")
        .and_then(|v| v.as_str())
        .or_else(|| obj.get("tool_name").and_then(|v| v.as_str()));
    if name != Some("web_search") {
        return false;
    }

    // 若带 type 字段，仅当 type=tool 才视为“强制调用”
    match obj.get("type").and_then(|v| v.as_str()) {
        Some("tool") => true,
        Some(_) => false,
        None => true,
    }
}

/// tools 是否有且仅有一个 web_search 工具（兼容旧客户端的“纯 WebSearch”请求）。
fn is_only_web_search_tool(req: &MessagesRequest) -> bool {
    req.tools
        .as_ref()
        .is_some_and(|tools| tools.len() == 1 && tools.first().is_some_and(tool_is_web_search))
}

/// 取最后一条 user 消息的首个 text 内容块（多轮对话更准）。
fn extract_last_user_text(req: &MessagesRequest) -> Option<String> {
    let msg = req
        .messages
        .iter()
        .rev()
        .find(|m| m.role == "user")
        .or_else(|| req.messages.last())?;

    match &msg.content {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Array(arr) => {
            let first_block = arr.first()?;
            if first_block.get("type")?.as_str()? == "text" {
                Some(first_block.get("text")?.as_str()?.to_string())
            } else {
                None
            }
        }
        _ => None,
    }
}

/// 最后一条 user 消息是否以 Claude Code 风格前缀开头。
fn request_explicit_web_search_prefix(req: &MessagesRequest) -> bool {
    extract_last_user_text(req)
        .map(|t| t.trim_start().starts_with(WEB_SEARCH_PREFIX))
        .unwrap_or(false)
}

/// 判断当前请求是否应走“本地 WebSearch”处理。
///
/// 注意：`tools` 里包含 `web_search` 仅代表“可用工具”，并不代表这次一定要执行搜索。
/// 若不加额外条件，容易把普通对话/任务指令误当成搜索查询，导致 MCP 侧返回 -32602。
/// 因此在包含 web_search 的前提下，还需满足以下任一条件才本地处理：
/// 1. tool_choice 强制选择 web_search；
/// 2. 兼容旧客户端：tools 只含 web_search 单工具；
/// 3. 兼容 Claude Code 风格前缀（最后一条 user 消息以固定前缀开头）。
///
/// 其余“混合工具但未显式触发搜索”的请求走常规转发路径，配合多轮回灌
/// （`run_web_search_loop`，见文件底部）在上游回 web_search tool_use 时网关内部消化，
/// 不再剔除 web_search 工具（见 handlers 分派）。
/// 共享预算耗尽的 503 响应（2026-08-11 方案 A）：MCP 子调用路径不经过
/// `map_provider_error`，预算耗尽串在这里特判——落 502 会让客户端当故障立即重发。
fn budget_exhausted_response() -> axum::response::Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        [(header::RETRY_AFTER, "8")],
        Json(ErrorResponse::new(
            "api_error",
            "网关已就该请求打满上游调用预算（每请求上限），上游仍不可用。这是可重试的瞬态状态，请按 Retry-After 退避后重试。",
        )),
    )
        .into_response()
}

pub fn should_handle_websearch_request(req: &MessagesRequest) -> bool {
    if !has_web_search_tool(req) {
        return false;
    }
    tool_choice_requests_web_search(req)
        || is_only_web_search_tool(req)
        || request_explicit_web_search_prefix(req)
}

/// 从请求的 tools 列表中移除 web_search 工具。
///
/// ⚠️ 多轮回灌上线后，混合工具场景改走 `run_web_search_loop`，**不再**调用本函数
/// （见 handlers 分派）；本函数仅保留给测试与历史调用方。
///
/// 混合工具（web_search + 其他工具）在不本地处理时，需剔除 web_search 后再转发上游，
/// 否则原样下发给 Kiro 会触发 400 Improperly formed request。
/// 剔除后若 tools 为空，则置为 None。
pub fn strip_web_search_tools(req: &mut MessagesRequest) {
    if let Some(tools) = req.tools.as_mut() {
        tools.retain(|t| !tool_is_web_search(t));
        if tools.is_empty() {
            req.tools = None;
        }
    }
}

/// 从消息中提取搜索查询
///
/// 读取 messages 中最后一条 user 消息的首个内容块（更符合多轮对话场景），
/// 并去除 "Perform a web search for the query: " 前缀。
pub fn extract_search_query(req: &MessagesRequest) -> Option<String> {
    // 优先取最后一条 user 消息，否则回退到最后一条消息
    let msg = req
        .messages
        .iter()
        .rev()
        .find(|m| m.role == "user")
        .or_else(|| req.messages.last())?;

    // 提取文本内容
    let text = match &msg.content {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(arr) => {
            // 获取第一个内容块
            let first_block = arr.first()?;
            if first_block.get("type")?.as_str()? == "text" {
                first_block.get("text")?.as_str()?.to_string()
            } else {
                return None;
            }
        }
        _ => return None,
    };

    // 去除前缀 "Perform a web search for the query: "。
    // 用 trim_start() 与路由判定 request_explicit_web_search_prefix 对齐——
    // 后者用 t.trim_start().starts_with(PREFIX) 判前缀，此处若不 trim，带前导
    // 空格的请求会 strip_prefix 失配、把整句（含前缀）当查询词传给 MCP → 垃圾结果。
    let trimmed = text.trim_start();
    let query = trimmed
        .strip_prefix(WEB_SEARCH_PREFIX)
        .map(|s| s.to_string())
        .unwrap_or_else(|| trimmed.to_string());

    if query.is_empty() { None } else { Some(query) }
}

/// 生成22位大小写字母和数字的随机字符串
fn generate_random_id_22() -> String {
    const CHARSET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    (0..22)
        .map(|_| {
            let idx = fastrand::usize(..CHARSET.len());
            CHARSET[idx] as char
        })
        .collect()
}

/// 生成8位小写字母和数字的随机字符串
fn generate_random_id_8() -> String {
    const CHARSET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    (0..8)
        .map(|_| {
            let idx = fastrand::usize(..CHARSET.len());
            CHARSET[idx] as char
        })
        .collect()
}

/// 创建 MCP 请求
///
/// ID 格式: web_search_tooluse_{22位随机}_{毫秒时间戳}_{8位随机}
pub fn create_mcp_request(query: &str) -> (String, McpRequest) {
    let random_22 = generate_random_id_22();
    let timestamp = chrono::Utc::now().timestamp_millis();
    let random_8 = generate_random_id_8();

    let request_id = format!(
        "web_search_tooluse_{}_{}_{}",
        random_22, timestamp, random_8
    );

    // tool_use_id 使用相同格式
    let tool_use_id = format!(
        "srvtoolu_{}",
        Uuid::new_v4().to_string().replace('-', "")[..32].to_string()
    );

    let request = McpRequest {
        id: request_id,
        jsonrpc: "2.0".to_string(),
        method: "tools/call".to_string(),
        params: McpParams {
            name: "web_search".to_string(),
            arguments: McpArguments {
                query: query.to_string(),
            },
        },
    };

    (tool_use_id, request)
}

/// HTML 文本清洗：MCP 返回的第三方网页 snippet/title 本质是 HTML 抽取文本，
/// 常见残留 `<br>`/`<p>`/`&nbsp;` 等标签与实体，若原样拼进面向客户端的正文，
/// 用户会看到裸露的 `<br>`。这里统一做三件事：块级标签转真实换行、
/// 剥离其余标签（保留内部文字）、解码常见 HTML 实体，最后折叠残留的连续空白。
///
/// 调用顺序要求：**先清洗后截断**——截断发生在本函数之外的调用点，
/// 若先截断再清洗，200 字符的边界可能正好切在一个标签中间，
/// 清洗后会留下半个标签（如 `<b`）比原文更难看。
fn normalize_html_text(input: &str) -> String {
    let stripped = strip_html_tags(input);
    let decoded = decode_html_entities(&stripped);
    collapse_whitespace_runs(&decoded)
}

/// 剥离 HTML 标签。块级/换行类标签（`<br>`/`<p>`/`<div>`/`<li>` 等，大小写不敏感、
/// 兼容自闭合 `<br/>`/`<br />`）转换为真实换行，保留原文分段语义；其余标签
/// （如 `<b>`/`<a href="...">`）直接剥离，只保留标签包裹的文字内容。
///
/// 未闭合的孤立 `<`（找不到匹配的 `>`）视为普通文本字符原样保留，不 panic、
/// 不误吞后续内容——这也覆盖了纯文本里“a < b”这类非标签用法。
fn strip_html_tags(input: &str) -> String {
    // 块级/换行标签名单：命中即输出一个换行；不在名单里的标签只剥不换行。
    const BLOCK_TAGS: &[&str] = &[
        "br",
        "p",
        "div",
        "li",
        "ul",
        "ol",
        "tr",
        "table",
        "h1",
        "h2",
        "h3",
        "h4",
        "h5",
        "h6",
        "section",
        "article",
        "blockquote",
        "header",
        "footer",
    ];

    let chars: Vec<char> = input.chars().collect();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i] != '<' {
            out.push(chars[i]);
            i += 1;
            continue;
        }

        // 在 '<' 之后找匹配的 '>'，两者之间视为一个标签的内部内容
        match chars[i + 1..].iter().position(|&c| c == '>') {
            Some(rel) => {
                let close_idx = i + 1 + rel;
                let tag_inner: String = chars[i + 1..close_idx].iter().collect();
                let tag_name = tag_inner
                    .trim_start_matches('/')
                    .trim_end_matches('/')
                    .split(|c: char| c.is_whitespace())
                    .next()
                    .unwrap_or("")
                    .to_lowercase();
                if BLOCK_TAGS.contains(&tag_name.as_str()) {
                    out.push('\n');
                }
                i = close_idx + 1;
            }
            None => {
                // 没有匹配的 '>'，不是完整标签，把 '<' 当普通字符处理
                out.push('<');
                i += 1;
            }
        }
    }
    out
}

/// 解码常见 HTML 实体：`&nbsp;` `&amp;` `&lt;` `&gt;` `&quot;` `&apos;`，
/// 以及数字实体 `&#NN;`（十进制）/ `&#xHH;`（十六进制，大小写均可）。
/// 无法识别的 `&...;` 或没有 `;` 收尾的 `&` 原样保留，不 panic。
fn decode_html_entities(input: &str) -> String {
    // 实体名不会很长，给个扫描上限防止病态输入（一长串没有 ';' 的文本）退化成整段扫描
    const MAX_ENTITY_LEN: usize = 10;

    let chars: Vec<char> = input.chars().collect();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i] != '&' {
            out.push(chars[i]);
            i += 1;
            continue;
        }

        let scan_end = (i + 1 + MAX_ENTITY_LEN + 1).min(chars.len());
        let found = chars[i + 1..scan_end]
            .iter()
            .position(|&c| c == ';')
            .map(|rel| i + 1 + rel)
            .and_then(|semi_idx| {
                let name: String = chars[i + 1..semi_idx].iter().collect();
                decode_entity_name(&name).map(|ch| (semi_idx, ch))
            });

        match found {
            Some((semi_idx, ch)) => {
                out.push(ch);
                i = semi_idx + 1;
            }
            None => {
                // 不是可识别的实体，把 '&' 当普通字符处理，不吞掉后续内容
                out.push('&');
                i += 1;
            }
        }
    }
    out
}

/// 单个 HTML 实体名（不含首尾 `&` `;`）解码为对应字符。
fn decode_entity_name(name: &str) -> Option<char> {
    match name {
        // &nbsp; 解码为普通空格而非 U+00A0：Unicode 把 NBSP 排除在 White_Space
        // 属性外，若保留 U+00A0 会导致后续空白折叠认不出它，视觉上残留大段空格。
        "nbsp" => return Some(' '),
        "amp" => return Some('&'),
        "lt" => return Some('<'),
        "gt" => return Some('>'),
        "quot" => return Some('"'),
        "apos" => return Some('\''),
        _ => {}
    }
    if let Some(hex) = name.strip_prefix("#x").or_else(|| name.strip_prefix("#X")) {
        return u32::from_str_radix(hex, 16).ok().and_then(char::from_u32);
    }
    if let Some(dec) = name.strip_prefix('#') {
        return dec.parse::<u32>().ok().and_then(char::from_u32);
    }
    None
}

/// 折叠清洗后残留的连续空白：同一行内连续空格/Tab 折叠为单个空格；
/// 连续换行折叠为单个（block 标签一开一合会连续产生两个换行，逐一剥离后
/// 若不折叠就是空行满天飞）；首尾空白裁掉。
fn collapse_whitespace_runs(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut last_was_space = false;
    let mut last_was_newline = false;

    for c in input.chars() {
        if c == '\n' {
            last_was_space = false;
            if !last_was_newline {
                out.push('\n');
            }
            last_was_newline = true;
            continue;
        }
        last_was_newline = false;
        if c.is_whitespace() {
            if !last_was_space {
                out.push(' ');
            }
            last_was_space = true;
        } else {
            out.push(c);
            last_was_space = false;
        }
    }

    out.trim().to_string()
}

/// 解析 MCP 响应中的搜索结果
///
/// 仅做反序列化，不在此处清洗 HTML——title/snippet 有三处不同的下游用法
/// （摘要文本的标题/正文、web_search_tool_result 块的 encrypted_content），
/// 清洗统一放在各自的使用点，便于单独核对“先清洗后截断”的顺序是否正确。
pub fn parse_search_results(mcp_response: &McpResponse) -> Option<WebSearchResults> {
    let result = mcp_response.result.as_ref()?;
    let content = result.content.first()?;

    if content.content_type != "text" {
        return None;
    }

    serde_json::from_str(&content.text).ok()
}

/// 生成 WebSearch SSE 响应流
pub fn create_websearch_sse_stream(
    model: String,
    query: String,
    tool_use_id: String,
    search_results: Option<WebSearchResults>,
    input_tokens: i32,
) -> impl Stream<Item = Result<Bytes, Infallible>> {
    let events =
        generate_websearch_events(&model, &query, &tool_use_id, search_results, input_tokens);

    stream::iter(
        events
            .into_iter()
            .map(|e| Ok(Bytes::from(e.to_sse_string()))),
    )
}

/// 生成 WebSearch SSE 事件序列
fn generate_websearch_events(
    model: &str,
    query: &str,
    tool_use_id: &str,
    search_results: Option<WebSearchResults>,
    input_tokens: i32,
) -> Vec<SseEvent> {
    let mut events = Vec::new();
    let message_id = format!(
        "msg_{}",
        Uuid::new_v4().to_string().replace('-', "")[..24].to_string()
    );

    // 1. message_start
    events.push(SseEvent::new(
        "message_start",
        json!({
            "type": "message_start",
            "message": {
                "id": message_id,
                "type": "message",
                "role": "assistant",
                "model": model,
                "content": [],
                "stop_reason": null,
                "usage": {
                    "input_tokens": input_tokens,
                    "output_tokens": 0,
                    "cache_creation_input_tokens": 0,
                    "cache_read_input_tokens": 0
                }
            }
        }),
    ));

    // 2. content_block_start (text - 搜索决策说明, index 0)
    let decision_text = format!("I'll search for \"{}\".", query);
    events.push(SseEvent::new(
        "content_block_start",
        json!({
            "type": "content_block_start",
            "index": 0,
            "content_block": {
                "type": "text",
                "text": ""
            }
        }),
    ));

    events.push(SseEvent::new(
        "content_block_delta",
        json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": {
                "type": "text_delta",
                "text": decision_text
            }
        }),
    ));

    events.push(SseEvent::new(
        "content_block_stop",
        json!({
            "type": "content_block_stop",
            "index": 0
        }),
    ));

    // 3. content_block_start (server_tool_use, index 1)
    // server_tool_use 是服务端工具，input 在 content_block_start 中一次性完整发送，
    // 不像客户端 tool_use 需要通过 input_json_delta 增量传输。
    events.push(SseEvent::new(
        "content_block_start",
        json!({
            "type": "content_block_start",
            "index": 1,
            "content_block": {
                "id": tool_use_id,
                "type": "server_tool_use",
                "name": "web_search",
                "input": {"query": query}
            }
        }),
    ));

    // 4. content_block_stop (server_tool_use)
    events.push(SseEvent::new(
        "content_block_stop",
        json!({
            "type": "content_block_stop",
            "index": 1
        }),
    ));

    // 5. content_block_start (web_search_tool_result, index 2)
    // 官方 Anthropic 协议里 web_search_tool_result 块**带** tool_use_id，且必须等于
    // 前面 server_tool_use 块的 id——客户端 SDK 据此把搜索结果配对回对应的工具调用、
    // 关联引用。缺此字段严格 SDK 客户端无法配对、typed 反序列化失败。此处补上。
    let search_content = if let Some(ref results) = search_results {
        results
            .results
            .iter()
            .map(|r| {
                let page_age = r.published_date.and_then(|ms| {
                    chrono::DateTime::from_timestamp_millis(ms)
                        .map(|dt| dt.format("%B %-d, %Y").to_string())
                });
                json!({
                    "type": "web_search_result",
                    "title": r.title,
                    "url": r.url,
                    "encrypted_content": r.snippet.as_deref().map(normalize_html_text).unwrap_or_default(),
                    "page_age": page_age
                })
            })
            .collect::<Vec<_>>()
    } else {
        vec![]
    };

    events.push(SseEvent::new(
        "content_block_start",
        json!({
            "type": "content_block_start",
            "index": 2,
            "content_block": {
                "type": "web_search_tool_result",
                "tool_use_id": tool_use_id,
                "content": search_content
            }
        }),
    ));

    // 6. content_block_stop (web_search_tool_result)
    events.push(SseEvent::new(
        "content_block_stop",
        json!({
            "type": "content_block_stop",
            "index": 2
        }),
    ));

    // 7. content_block_start (text, index 3)
    events.push(SseEvent::new(
        "content_block_start",
        json!({
            "type": "content_block_start",
            "index": 3,
            "content_block": {
                "type": "text",
                "text": ""
            }
        }),
    ));

    // 8. content_block_delta (text_delta) - 生成搜索结果摘要
    let summary = generate_search_summary(query, &search_results);

    // 分块发送文本
    let chunk_size = 100;
    for chunk in summary.chars().collect::<Vec<_>>().chunks(chunk_size) {
        let text: String = chunk.iter().collect();
        events.push(SseEvent::new(
            "content_block_delta",
            json!({
                "type": "content_block_delta",
                "index": 3,
                "delta": {
                    "type": "text_delta",
                    "text": text
                }
            }),
        ));
    }

    // 9. content_block_stop (text)
    events.push(SseEvent::new(
        "content_block_stop",
        json!({
            "type": "content_block_stop",
            "index": 3
        }),
    ));

    // 10. message_delta
    // 官方 API 的 message_delta.delta 中没有 stop_sequence 字段
    let output_tokens = (summary.len() as i32 + 3) / 4; // 简单估算
    events.push(SseEvent::new(
        "message_delta",
        json!({
            "type": "message_delta",
            "delta": {
                "stop_reason": "end_turn"
            },
            "usage": {
                "output_tokens": output_tokens,
                "server_tool_use": {
                    "web_search_requests": 1
                }
            }
        }),
    ));

    // 11. message_stop
    events.push(SseEvent::new(
        "message_stop",
        json!({
            "type": "message_stop"
        }),
    ));

    events
}

/// 生成搜索结果摘要
fn generate_search_summary(query: &str, results: &Option<WebSearchResults>) -> String {
    let mut summary = format!("Here are the search results for \"{}\":\n\n", query);

    if let Some(results) = results {
        for (i, result) in results.results.iter().enumerate() {
            let title = normalize_html_text(&result.title);
            summary.push_str(&format!("{}. **{}**\n", i + 1, title));
            if let Some(ref snippet) = result.snippet {
                // 先清洗 HTML 残留（<br>/&nbsp; 等），再截断——顺序不能反过来，
                // 否则 200 字符的截断边界可能正好切在标签中间，清洗后会留下
                // 半个标签（如 "<b"）比原文更难看。截断本身按 char 计数，
                // 安全处理 UTF-8 多字节字符。
                let cleaned = normalize_html_text(snippet);
                let truncated = match cleaned.char_indices().nth(200) {
                    Some((idx, _)) => format!("{}...", &cleaned[..idx]),
                    None => cleaned,
                };
                summary.push_str(&format!("   {}\n", truncated));
            }
            summary.push_str(&format!("   Source: {}\n\n", result.url));
        }
    } else {
        summary.push_str("No results found.\n");
    }

    summary.push_str("\nPlease note that these are web search results and may not be fully accurate or up-to-date.");

    summary
}

/// 处理 WebSearch 请求
pub async fn handle_websearch_request(
    provider: std::sync::Arc<crate::kiro::provider::KiroProvider>,
    payload: &MessagesRequest,
    input_tokens: i32,
    budget: &crate::kiro::provider::SharedRetryBudget,
) -> Response {
    // 1. 提取搜索查询
    let query = match extract_search_query(payload) {
        Some(q) => q,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse::new(
                    "invalid_request_error",
                    "无法从消息中提取搜索查询",
                )),
            )
                .into_response();
        }
    };

    tracing::info!(query = %query, "处理 WebSearch 请求");

    // 2. 创建 MCP 请求
    let (tool_use_id, mcp_request) = create_mcp_request(&query);

    // 3. 调用 Kiro MCP API
    // 🔴 上游 MCP 调用失败不能伪装成「搜索无结果」：客户端会把 200 空结果当成
    // 「真的没搜到」，掩盖网关/上游故障（已确认缺陷）。失败时返回 502 让客户端
    // 能区分「搜索无结果」与「搜索服务故障」；正常「无结果」仍是合法 200 空结果
    // （parse_search_results 返回 None，或 results 为空数组）。
    let search_results = match call_mcp_api(&provider, &mcp_request, budget).await {
        Ok(response) => parse_search_results(&response),
        Err(e) => {
            tracing::warn!("MCP API 调用失败: {}", e);
            // ⭐ 共享预算耗尽（2026-08-11 方案 A）：不能落 502 无退避信号（客户端当
            // 故障立即重发 = 拿全新预算再打一轮）。与主路径同款 503 + Retry-After。
            if e.to_string().contains("shared_budget_exhausted=1") {
                return budget_exhausted_response();
            }
            return (
                StatusCode::BAD_GATEWAY,
                Json(ErrorResponse::new(
                    "upstream_error",
                    format!("WebSearch 上游调用失败: {}", e),
                )),
            )
                .into_response();
        }
    };

    // 4. 生成 SSE 响应
    let model = payload.model.clone();
    let stream =
        create_websearch_sse_stream(model, query, tool_use_id, search_results, input_tokens);

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONNECTION, "keep-alive")
        .body(Body::from_stream(stream))
        .unwrap()
}

/// 调用 Kiro MCP API
async fn call_mcp_api(
    provider: &crate::kiro::provider::KiroProvider,
    request: &McpRequest,
    budget: &crate::kiro::provider::SharedRetryBudget,
) -> anyhow::Result<McpResponse> {
    let request_body = serde_json::to_string(request)?;

    tracing::debug!("MCP request: {}", request_body);

    let response = provider.call_mcp(&request_body, budget).await?;

    let body = response.text().await?;
    tracing::debug!("MCP response: {}", body);

    let mcp_response: McpResponse = serde_json::from_str(&body)?;

    if let Some(ref error) = mcp_response.error {
        anyhow::bail!(
            "MCP error: {} - {}",
            error.code.unwrap_or(-1),
            error.message.as_deref().unwrap_or("Unknown error")
        );
    }

    Ok(mcp_response)
}

// ==================== WebSearch agentic 多轮回灌 ====================
//
// 机制（参考 ref-grey websearch_loop.rs:689 run_web_search_loop，最简化实现）：
//
//   for round in 0..=MAX_WEB_SEARCH_ROUNDS:
//     转换 payload → 打上游 → 缓冲解码整轮
//     若本轮 tool_use 全是 web_search（且未超轮数上限）：
//         对每条调 MCP 搜 → append_search_round 回灌进 payload → continue
//     否则（含非 web_search 工具 / 无 tool_use / 超上限）：
//         把累积的 presentation + 本轮文本/tool_use 渲染给客户端，收尾返回
//
// **刻意没照抄参考仓的 1269 行**（本轮范围控制）：不做 thinking/redacted_thinking 回灌、
// 不做 metering/cache 精算、不做首字节 marker 的 SSE 提前握手。这些都是既有 StreamContext
// 已覆盖或本轮不必要的能力，先让机制跑通。

/// 一轮上游响应的缓冲解码结果。
struct RoundOutcome {
    /// 累积的正文
    text: String,
    /// 累积的结构化思考流（reasoningContentEvent）。
    ///
    /// ⚠️ 必须收集：不收就是把 thinking 输出静默丢掉 —— 主路径（StreamContext）会下发它，
    /// 回灌路径若不下发，同一个开 thinking 的请求走进这条路就"思考不见了"。
    reasoning: String,
    /// 上游下发的真思考签名（若有）。缺失时用占位符，见 THINKING_SIGNATURE_PLACEHOLDER。
    reasoning_signature: Option<String>,
    /// 本轮完成的 web_search tool_use
    web_search: Vec<DecodedWebSearch>,
    /// 本轮完成的非 web_search tool_use（原样回客户端，绝不吞）
    client_tool_use: Vec<Value>,
    /// contextUsageEvent 反推的真实 input_tokens
    context_input_tokens: Option<i32>,
    /// meteringEvent 累计 credit
    credits: f64,
    /// stop_reason 覆盖（max_tokens / model_context_window_exceeded）
    stop_reason_override: Option<String>,
    /// 上游流中途读失败：本轮内容是**半截**的，不能当成功回灌
    stream_error: bool,
    /// in-band 错误/异常
    upstream_error: Option<String>,
}

/// 缓冲解码一轮上游流式响应。
///
/// 复用 `merge_tool_input`（与 stream.rs / 非流式路径同源的完备决策表：累积快照 /
/// 纯增量 / 重复终帧 / 迟到旧短快照 / 非前缀重写），不自己写一份拼接逻辑 ——
/// 本仓历史上「同一逻辑各写一份」正是 Invalid tool parameters 反复出现的成因。
///
/// 工具名经 `tool_name_map` 还原成客户端原名（超长缩短过的 / CC 内置映射过的）。
async fn decode_round(
    response: reqwest::Response,
    model: &str,
    tool_name_map: &std::collections::HashMap<String, String>,
) -> RoundOutcome {
    use crate::kiro::model::events::Event;
    use crate::kiro::parser::decoder::EventStreamDecoder;

    let mut body_stream = response.bytes_stream();
    let mut decoder = EventStreamDecoder::new();

    let mut out = RoundOutcome {
        text: String::new(),
        reasoning: String::new(),
        reasoning_signature: None,
        web_search: Vec::new(),
        client_tool_use: Vec::new(),
        context_input_tokens: None,
        credits: 0.0,
        stop_reason_override: None,
        stream_error: false,
        upstream_error: None,
    };
    // tool_use_id → (还原后的工具名, 累积的 input JSON)
    let mut tool_buffers: std::collections::HashMap<String, (String, String)> =
        std::collections::HashMap::new();

    while let Some(chunk) = body_stream.next().await {
        let chunk = match chunk {
            Ok(c) => c,
            Err(e) => {
                tracing::error!("WebSearch 回灌读取上游流失败: {}", e);
                out.stream_error = true;
                break;
            }
        };
        if let Err(e) = decoder.feed(&chunk) {
            tracing::warn!("WebSearch 回灌缓冲区溢出: {}", e);
        }
        for result in decoder.decode_iter() {
            let frame = match result {
                Ok(f) => f,
                Err(e) => {
                    tracing::warn!("WebSearch 回灌解码事件失败: {}", e);
                    continue;
                }
            };
            let event = match Event::from_frame(frame) {
                Ok(ev) => ev,
                Err(_) => continue,
            };
            match event {
                Event::AssistantResponse(resp) => out.text.push_str(&resp.content),
                Event::ReasoningContent(r) => {
                    out.reasoning.push_str(&r.text);
                    // 优先回传上游真签名（实测真签名让多轮 cache 命中、伪造的 cache_read 恒 0）。
                    if let Some(sig) = r.signature.as_deref() {
                        if !sig.is_empty() {
                            out.reasoning_signature = Some(sig.to_string());
                        }
                    }
                }
                Event::ToolUse(tu) => {
                    let original_name = tool_name_map
                        .get(&tu.name)
                        .cloned()
                        .unwrap_or_else(|| tu.name.clone());
                    let entry = tool_buffers
                        .entry(tu.tool_use_id.clone())
                        .or_insert_with(|| (original_name.clone(), String::new()));
                    if !tu.input.is_empty() {
                        entry.1 = super::stream::merge_tool_input(&entry.1, &tu.input);
                    }
                    if tu.stop {
                        let (name, assembled) = tool_buffers
                            .remove(&tu.tool_use_id)
                            .unwrap_or((original_name, String::new()));
                        let input: Value = if assembled.is_empty() {
                            json!({})
                        } else {
                            serde_json::from_str(&assembled).unwrap_or_else(|e| {
                                tracing::warn!(
                                    "WebSearch 回灌工具参数非法 JSON(tool_use_id={}): {}",
                                    tu.tool_use_id,
                                    e
                                );
                                json!({})
                            })
                        };
                        if tool_use_name_is_web_search(&name) {
                            let query = input
                                .get("query")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();
                            out.web_search.push(DecodedWebSearch {
                                id: tu.tool_use_id.clone(),
                                query,
                            });
                        } else {
                            // 出站参数还原（Kiro 形态 → CC 形态），仅当入站映射过；
                            // 与 stream.rs 的 stop 分支同口径，避免把不认识的参数清空。
                            let client_input = if tool_name_map.contains_key(&tu.name) {
                                super::converter::map_tool_input_from_kiro(&name, input)
                            } else {
                                input
                            };
                            out.client_tool_use.push(json!({
                                "type": "tool_use",
                                "id": tu.tool_use_id,
                                "name": name,
                                "input": client_input
                            }));
                        }
                    }
                }
                Event::ContextUsage(cu) => {
                    // 与两条主路径共用同一个判据函数（脏值不覆盖已有值）。
                    // window_size 必须按**真实模型名**取，传空串会拿到默认窗口 →
                    // 反推的 input_tokens 整体偏移，记账口径与主路径分叉。
                    if let Some(v) = super::stream::context_input_tokens_from_pct(
                        cu.context_usage_percentage,
                        super::converter::get_context_window_size(model),
                    ) {
                        out.context_input_tokens = Some(v);
                    }
                    if cu.context_usage_percentage >= 100.0 {
                        out.stop_reason_override =
                            Some("model_context_window_exceeded".to_string());
                    }
                }
                Event::Metering(m) => out.credits += m.usage,
                Event::Exception {
                    exception_type,
                    message,
                } => {
                    // 铁律：ContentLengthExceededException = max_tokens 干净收尾，不算失败。
                    if exception_type == "ContentLengthExceededException" {
                        out.stop_reason_override = Some("max_tokens".to_string());
                    } else if out.upstream_error.is_none() {
                        out.upstream_error = Some(format!("{}: {}", exception_type, message));
                    }
                }
                Event::Error {
                    error_code,
                    error_message,
                } => {
                    if out.upstream_error.is_none() {
                        out.upstream_error = Some(format!("{}: {}", error_code, error_message));
                    }
                }
                _ => {}
            }
        }
    }
    // 解码器永久停止：后续帧必然丢失，本轮内容截断，不能当成功回灌。
    if decoder.is_stopped() {
        out.stream_error = true;
    }
    out
}

/// 回灌循环成功收尾时的结果（渲染成 SSE 或 JSON 由调用方决定）。
pub(super) struct WebSearchLoopSuccess {
    pub model: String,
    /// 最终 content 数组：各轮 server_tool_use/web_search_tool_result + 末轮文本 + 末轮 tool_use
    pub content: Vec<Value>,
    pub stop_reason: String,
    pub input_tokens: i32,
    pub output_tokens: i32,
    /// 实际服务末轮的凭据 ID（用量埋点用）
    pub credential_id: u64,
    /// 各轮累计 credit
    pub credits: f64,
    /// 累计上游往返次数（含首轮），供埋点看放大倍数
    pub rounds: u32,
}

/// 单轮请求：转换 payload → 打上游 → 缓冲解码。
///
/// 转换/序列化失败返回 400/500；上游调用失败交 `map_provider_error` 统一映射
/// （与两条主路径同口径，不自己造错误码）。
async fn run_round(
    provider: &std::sync::Arc<crate::kiro::provider::KiroProvider>,
    payload: &MessagesRequest,
    budget: &crate::kiro::provider::SharedRetryBudget,
) -> Result<(RoundOutcome, u64), Response> {
    let conversion = super::converter::convert_request(payload).map_err(|e| {
        tracing::warn!("WebSearch 回灌请求转换失败: {}", e);
        (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::new(
                "invalid_request_error",
                format!("WebSearch 回灌请求转换失败: {}", e),
            )),
        )
            .into_response()
    })?;
    let tool_name_map = conversion.tool_name_map;

    let request_body = super::handlers::build_kiro_request_body_for_websearch(
        conversion.conversation_state,
        conversion.additional_model_request_fields,
    )
    .map_err(|e| {
        tracing::error!("WebSearch 回灌序列化请求失败: {}", e);
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse::new(
                "internal_error",
                format!("序列化请求失败: {}", e),
            )),
        )
            .into_response()
    })?;

    let is_1m = crate::anthropic::model_catalog::resolve_is_1m(&payload.model);
    let (response, meta) = provider
        .call_api_stream(&request_body, is_1m, budget)
        .await
        .map_err(super::handlers::map_provider_error_for_websearch)?;

    let outcome = decode_round(response, &payload.model, &tool_name_map).await;

    // 上游 in-band 错误 / 流截断：不能把半截内容当成功回灌下一轮（回灌了等于把
    // 截断的假事实写进历史，模型后续全部基于错误前提推理）。
    if let Some(err) = &outcome.upstream_error {
        return Err((
            StatusCode::BAD_GATEWAY,
            Json(ErrorResponse::new(
                "upstream_error",
                format!("WebSearch 回灌上游返回错误: {}", err),
            )),
        )
            .into_response());
    }
    if outcome.stream_error {
        return Err((
            StatusCode::BAD_GATEWAY,
            Json(ErrorResponse::new(
                "upstream_error",
                "WebSearch 回灌期间上游响应流意外中断（内容不完整，未回灌）",
            )),
        )
            .into_response());
    }

    Ok((outcome, meta.credential_id))
}

/// WebSearch agentic 回灌循环（机制主体）。
///
/// 混合工具场景（web_search + 其他工具）的入口：常规转发一轮，若上游回的 tool_use
/// **全是** web_search 就网关内部调 MCP 拿结果、回灌成 tool_result 重发，最多
/// [`MAX_WEB_SEARCH_ROUNDS`] 轮；一旦出现非 web_search 的 tool_use（Bash/Edit 等）
/// 或没有 tool_use，就收尾把内容渲染给客户端 —— 客户端工具**绝不**被吞。
pub(super) async fn run_web_search_loop(
    provider: std::sync::Arc<crate::kiro::provider::KiroProvider>,
    mut payload: MessagesRequest,
    fallback_input_tokens: i32,
    budget: &crate::kiro::provider::SharedRetryBudget,
) -> Result<WebSearchLoopSuccess, Response> {
    let mut presentation: Vec<Value> = Vec::new();
    let mut last_credential_id: u64 = 0;
    let mut last_context_input: Option<i32> = None;
    let mut total_credits = 0.0;
    // 客户端是否声明 thinking（决定收尾是否下发 thinking 块）。必须在循环外取：
    // 循环内 payload 会被追加回灌消息，但 thinking 声明本身不变。
    let thinking_enabled = payload.thinking.as_ref().is_some_and(|t| t.is_enabled());

    // 0..=MAX 而不是 0..MAX：上限那一轮仍要**发出去**（拿到最终回答），只是不再回灌。
    for round_idx in 0..=MAX_WEB_SEARCH_ROUNDS {
        let (round, credential_id) = run_round(&provider, &payload, budget).await?;
        last_credential_id = credential_id;
        last_context_input = round.context_input_tokens.or(last_context_input);
        total_credits += round.credits;

        if should_replay_round(round_idx, &round.web_search, !round.client_tool_use.is_empty()) {
            // 真搜索：任一条失败就整体报错，绝不把失败静默降级成「没搜到」——
            // 客户端会把空结果当"真的没搜到"，掩盖网关/上游故障（与快路径同一条铁律）。
            let mut searched: Vec<SearchedWebSearch> = Vec::with_capacity(round.web_search.len());
            for ws in &round.web_search {
                let (srv_id, mcp_request) = create_mcp_request(&ws.query);
                match call_mcp_api(&provider, &mcp_request, budget).await {
                    Ok(resp) => searched.push(SearchedWebSearch {
                        upstream_id: ws.id.clone(),
                        query: ws.query.clone(),
                        srv_id,
                        results: parse_search_results(&resp),
                    }),
                    Err(e) => {
                        tracing::warn!("WebSearch 回灌 MCP 调用失败: {}", e);
                        // ⭐ 共享预算耗尽（同快路径）：503 + Retry-After，客户端可退避。
                        if e.to_string().contains("shared_budget_exhausted=1") {
                            return Err(budget_exhausted_response());
                        }
                        return Err((
                            StatusCode::BAD_GATEWAY,
                            Json(ErrorResponse::new(
                                "upstream_error",
                                format!("WebSearch 上游调用失败: {}", e),
                            )),
                        )
                            .into_response());
                    }
                }
            }
            tracing::info!(
                round = round_idx + 1,
                searches = searched.len(),
                "WebSearch 回灌：搜索完成，结果回灌进下一轮请求"
            );
            presentation.extend(append_search_round(&mut payload, &round.text, &searched));
            continue;
        }

        // 收尾：本轮不是纯 web_search（或已达轮数上限）→ 渲染给客户端。
        let stop_reason = round.stop_reason_override.clone().unwrap_or_else(|| {
            if round.client_tool_use.is_empty() && round.web_search.is_empty() {
                "end_turn".to_string()
            } else {
                "tool_use".to_string()
            }
        });

        // 用 take 而不是直接 move：`presentation` 声明在循环**外**，直接 move 出去
        // 依赖借用检查器证明"move 之后必定 return"。虽然当前成立，但一旦有人在
        // 下面插一句 continue 就会变成 use-after-move 的编译错误，改动成本莫名其妙。
        // take 让所有权语义与控制流解耦，零额外开销。
        let mut content: Vec<Value> = std::mem::take(&mut presentation);
        // thinking 只在客户端**声明**了 thinking 时下发（与主路径同口径：客户端没声明
        // 却收到 thinking 块，Anthropic SDK 侧属协议违规）。
        if thinking_enabled && !round.reasoning.is_empty() {
            content.push(json!({
                "type": "thinking",
                "thinking": round.reasoning,
                "signature": round
                    .reasoning_signature
                    .clone()
                    .unwrap_or_else(|| super::stream::THINKING_SIGNATURE_PLACEHOLDER.to_string())
            }));
        }
        if !round.text.is_empty() {
            content.push(json!({"type": "text", "text": round.text}));
        }
        // 客户端工具原样回（含混合轮里与 web_search 并列的那些）。
        content.extend(round.client_tool_use);
        // 达到轮数上限时本轮的 web_search 不再由网关消化，原样回给客户端 ——
        // 客户端自己有 WebSearch 实现，会补 tool_result 再发一轮，链路不丢。
        for ws in &round.web_search {
            content.push(json!({
                "type": "tool_use",
                "id": ws.id,
                "name": "web_search",
                "input": {"query": ws.query}
            }));
        }

        let output_tokens = crate::token::estimate_output_tokens(&content);
        let model = payload.model.clone();
        return Ok(WebSearchLoopSuccess {
            model,
            content,
            stop_reason,
            input_tokens: last_context_input.unwrap_or(fallback_input_tokens),
            output_tokens,
            credential_id: last_credential_id,
            credits: total_credits,
            rounds: round_idx as u32 + 1,
        });
    }

    // 理论不可达：上面的 for 在 round_idx == MAX 那轮必然走收尾分支返回
    // （回灌判定的 round_idx < MAX 已假）。留显式错误而非 unreachable!()，
    // 万一将来改了上限比较符也只是 500 而非 panic 掉整个 worker。
    tracing::error!("WebSearch 回灌循环异常退出（不应发生）");
    Err((
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(ErrorResponse::new(
            "internal_error",
            "WebSearch 回灌循环异常退出",
        )),
    )
        .into_response())
}

/// 把回灌循环的最终 content 渲染成一串 SSE 事件（客户端要 stream 时用）。
///
/// 逐块 content_block_start → delta → stop，最后 message_delta + message_stop。
/// 与参考仓 build_sse_events（websearch_loop.rs:846）同结构。
pub(super) fn build_loop_sse_events(success: &WebSearchLoopSuccess) -> Vec<SseEvent> {
    let mut events = Vec::new();
    let message_id = format!(
        "msg_{}",
        Uuid::new_v4().to_string().replace('-', "")[..24].to_string()
    );

    events.push(SseEvent::new(
        "message_start",
        json!({
            "type": "message_start",
            "message": {
                "id": message_id,
                "type": "message",
                "role": "assistant",
                "model": success.model,
                "content": [],
                "stop_reason": null,
                "stop_sequence": null,
                "usage": {
                    "input_tokens": success.input_tokens,
                    "output_tokens": 0
                }
            }
        }),
    ));

    for (index, block) in success.content.iter().enumerate() {
        let index = index as i32;
        let btype = block.get("type").and_then(|v| v.as_str()).unwrap_or("");
        match btype {
            "thinking" => {
                let thinking = block.get("thinking").and_then(|v| v.as_str()).unwrap_or("");
                let signature = block
                    .get("signature")
                    .and_then(|v| v.as_str())
                    .unwrap_or(super::stream::THINKING_SIGNATURE_PLACEHOLDER);
                events.push(SseEvent::new(
                    "content_block_start",
                    json!({
                        "type": "content_block_start", "index": index,
                        "content_block": {"type": "thinking", "thinking": ""}
                    }),
                ));
                if !thinking.is_empty() {
                    events.push(SseEvent::new(
                        "content_block_delta",
                        json!({
                            "type": "content_block_delta", "index": index,
                            "delta": {"type": "thinking_delta", "thinking": thinking}
                        }),
                    ));
                }
                // signature_delta 必须在关块前发：客户端 thinking 模式本地校验要求非空签名。
                events.push(SseEvent::new(
                    "content_block_delta",
                    json!({
                        "type": "content_block_delta", "index": index,
                        "delta": {"type": "signature_delta", "signature": signature}
                    }),
                ));
                events.push(SseEvent::new(
                    "content_block_stop",
                    json!({"type": "content_block_stop", "index": index}),
                ));
            }
            "text" => {
                let text = block.get("text").and_then(|v| v.as_str()).unwrap_or("");
                events.push(SseEvent::new(
                    "content_block_start",
                    json!({
                        "type": "content_block_start", "index": index,
                        "content_block": {"type": "text", "text": ""}
                    }),
                ));
                events.push(SseEvent::new(
                    "content_block_delta",
                    json!({
                        "type": "content_block_delta", "index": index,
                        "delta": {"type": "text_delta", "text": text}
                    }),
                ));
                events.push(SseEvent::new(
                    "content_block_stop",
                    json!({"type": "content_block_stop", "index": index}),
                ));
            }
            "tool_use" => {
                // 参数一次性发单个 input_json_delta（不逐片切）：与 stream.rs 的
                // flush_tool_input 同策略，根治客户端拼接后 JSON 非法的 Invalid tool parameters。
                let input = block.get("input").cloned().unwrap_or_else(|| json!({}));
                let partial = serde_json::to_string(&input).unwrap_or_else(|_| "{}".to_string());
                events.push(SseEvent::new(
                    "content_block_start",
                    json!({
                        "type": "content_block_start", "index": index,
                        "content_block": {
                            "type": "tool_use",
                            "id": block.get("id").and_then(|v| v.as_str()).unwrap_or(""),
                            "name": block.get("name").and_then(|v| v.as_str()).unwrap_or(""),
                            "input": {}
                        }
                    }),
                ));
                events.push(SseEvent::new(
                    "content_block_delta",
                    json!({
                        "type": "content_block_delta", "index": index,
                        "delta": {"type": "input_json_delta", "partial_json": partial}
                    }),
                ));
                events.push(SseEvent::new(
                    "content_block_stop",
                    json!({"type": "content_block_stop", "index": index}),
                ));
            }
            "server_tool_use" | "web_search_tool_result" => {
                // 这两类没有 delta 形态，整块放在 content_block_start 里
                // （与快路径 generate_websearch_events 一致）。
                events.push(SseEvent::new(
                    "content_block_start",
                    json!({
                        "type": "content_block_start", "index": index,
                        "content_block": block
                    }),
                ));
                events.push(SseEvent::new(
                    "content_block_stop",
                    json!({"type": "content_block_stop", "index": index}),
                ));
            }
            _ => {}
        }
    }

    events.push(SseEvent::new(
        "message_delta",
        json!({
            "type": "message_delta",
            "delta": {"stop_reason": success.stop_reason, "stop_sequence": null},
            "usage": {"output_tokens": success.output_tokens}
        }),
    ));
    events.push(SseEvent::new(
        "message_stop",
        json!({"type": "message_stop"}),
    ));

    events
}

/// 把回灌循环结果渲染成非流式 JSON 响应体。
pub(super) fn build_loop_json_body(success: &WebSearchLoopSuccess) -> Value {
    json!({
        "id": format!("msg_{}", Uuid::new_v4().to_string().replace('-', "")),
        "type": "message",
        "role": "assistant",
        "content": success.content,
        "model": success.model,
        "stop_reason": success.stop_reason,
        "stop_sequence": null,
        "usage": {
            "input_tokens": success.input_tokens,
            "output_tokens": success.output_tokens
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// WebSearch 回灌轮次的 native effort 字段携带（deep 审计补测，2026-08-11）：
    /// 回灌重建 body 时 `additionalModelRequestFields` 不得丢（P1 移植 × WebSearch
    /// 交叉点）。无字段时输出不含该键（默认行为不回归）。
    #[test]
    fn test_websearch_rebuild_keeps_additional_model_request_fields() {
        use crate::anthropic::handlers::build_kiro_request_body_for_websearch;
        use crate::kiro::model::requests::kiro::{AdditionalModelRequestFields, KiroOutputConfig};

        let state = crate::kiro::model::requests::conversation::ConversationState::new("conv-ws");
        let fields = Some(AdditionalModelRequestFields {
            output_config: Some(KiroOutputConfig {
                effort: "high".to_string(),
            }),
        });
        let body = build_kiro_request_body_for_websearch(state.clone(), fields)
            .expect("回灌序列化应成功");
        assert!(
            body.contains("additionalModelRequestFields") && body.contains("output_config"),
            "WebSearch 回灌重建 body 必须保留 native effort 字段"
        );
        let body_without = build_kiro_request_body_for_websearch(state, None)
            .expect("无字段序列化应成功");
        assert!(
            !body_without.contains("additionalModelRequestFields"),
            "无字段时输出不得凭空出现该键"
        );
    }

    #[test]
    fn test_has_web_search_tool_only_one() {
        use crate::anthropic::types::{Message, Tool};

        let req = MessagesRequest {
            model: "claude-sonnet-4".to_string(),
            max_tokens: 1024,
            messages: vec![Message {
                role: "user".to_string(),
                content: serde_json::json!("test"),
            }],
            stream: true,
            system: None,
            tools: Some(vec![Tool {
                tool_type: Some("web_search_20250305".to_string()),
                name: "web_search".to_string(),
                description: String::new(),
                input_schema: Default::default(),
                max_uses: Some(8),
                cache_control: None,
            }]),
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: None,
        };

        assert!(has_web_search_tool(&req));
    }

    // 测试辅助：构造一个 web_search 工具
    #[cfg(test)]
    fn mk_web_search_tool() -> crate::anthropic::types::Tool {
        crate::anthropic::types::Tool {
            tool_type: Some("web_search_20250305".to_string()),
            name: "web_search".to_string(),
            description: String::new(),
            input_schema: Default::default(),
            max_uses: Some(8),
            cache_control: None,
        }
    }

    // 测试辅助：构造一个普通工具
    #[cfg(test)]
    fn mk_plain_tool(name: &str) -> crate::anthropic::types::Tool {
        crate::anthropic::types::Tool {
            tool_type: None,
            name: name.to_string(),
            description: format!("{} tool", name),
            input_schema: Default::default(),
            max_uses: None,
            cache_control: None,
        }
    }

    // 测试辅助：构造一个带 user 消息与指定 tools 的请求
    #[cfg(test)]
    fn mk_req(
        user_text: &str,
        tools: Option<Vec<crate::anthropic::types::Tool>>,
    ) -> MessagesRequest {
        use crate::anthropic::types::Message;
        MessagesRequest {
            model: "claude-sonnet-4".to_string(),
            max_tokens: 1024,
            messages: vec![Message {
                role: "user".to_string(),
                content: serde_json::json!(user_text),
            }],
            stream: true,
            system: None,
            tools,
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: None,
        }
    }

    #[test]
    fn test_has_web_search_tool_matches_mixed_tools() {
        // 混合工具：web_search + 其他工具，应被识别为“包含 web_search”
        let req = mk_req(
            "test",
            Some(vec![mk_web_search_tool(), mk_plain_tool("other_tool")]),
        );
        assert!(has_web_search_tool(&req));
    }

    #[test]
    fn test_has_web_search_tool_matches_type_only() {
        // name 缺失、仅靠 type 声明的 web_search 也应识别
        let mut tool = mk_web_search_tool();
        tool.name = String::new();
        let req = mk_req("test", Some(vec![tool, mk_plain_tool("other_tool")]));
        assert!(has_web_search_tool(&req));
    }

    #[test]
    fn test_should_handle_websearch_only_web_search() {
        // 纯 web_search 单工具：应本地处理
        let req = mk_req("weather today", Some(vec![mk_web_search_tool()]));
        assert!(should_handle_websearch_request(&req));
    }

    #[test]
    fn test_should_handle_websearch_mixed_without_trigger_is_false() {
        // 混合工具但未显式触发搜索：不本地处理，走常规转发
        let req = mk_req(
            "please refactor this function",
            Some(vec![mk_web_search_tool(), mk_plain_tool("Edit")]),
        );
        assert!(has_web_search_tool(&req));
        assert!(!should_handle_websearch_request(&req));
    }

    #[test]
    fn test_should_handle_websearch_mixed_with_prefix() {
        // 混合工具 + Claude Code 前缀：应本地处理
        let req = mk_req(
            "Perform a web search for the query: rust 2026",
            Some(vec![mk_web_search_tool(), mk_plain_tool("Edit")]),
        );
        assert!(should_handle_websearch_request(&req));
    }

    #[test]
    fn test_should_handle_websearch_mixed_with_tool_choice() {
        // 混合工具 + tool_choice 强制 web_search：应本地处理
        let mut req = mk_req(
            "some task",
            Some(vec![mk_web_search_tool(), mk_plain_tool("Edit")]),
        );
        req.tool_choice = Some(serde_json::json!({"type": "tool", "name": "web_search"}));
        assert!(should_handle_websearch_request(&req));
    }

    #[test]
    fn test_strip_web_search_tools_keeps_others() {
        // 剥离 web_search，保留其余工具
        let mut req = mk_req(
            "task",
            Some(vec![
                mk_web_search_tool(),
                mk_plain_tool("Edit"),
                mk_plain_tool("Write"),
            ]),
        );
        strip_web_search_tools(&mut req);
        let tools = req.tools.expect("其余工具应保留");
        assert_eq!(tools.len(), 2);
        assert!(!tools.iter().any(tool_is_web_search));
        assert!(tools.iter().any(|t| t.name == "Edit"));
        assert!(tools.iter().any(|t| t.name == "Write"));
    }

    #[test]
    fn test_strip_web_search_tools_empties_to_none() {
        // 仅有 web_search 时剥离后应置为 None
        let mut req = mk_req("task", Some(vec![mk_web_search_tool()]));
        strip_web_search_tools(&mut req);
        assert!(req.tools.is_none());
    }

    #[test]
    fn test_extract_search_query_with_prefix() {
        use crate::anthropic::types::Message;

        let req = MessagesRequest {
            model: "claude-sonnet-4".to_string(),
            max_tokens: 1024,
            messages: vec![Message {
                role: "user".to_string(),
                content: serde_json::json!([{
                    "type": "text",
                    "text": "Perform a web search for the query: rust latest version 2026"
                }]),
            }],
            stream: true,
            system: None,
            tools: None,
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: None,
        };

        let query = extract_search_query(&req);
        // 前缀应该被去除
        assert_eq!(query, Some("rust latest version 2026".to_string()));
    }

    #[test]
    fn test_extract_search_query_plain_text() {
        use crate::anthropic::types::Message;

        let req = MessagesRequest {
            model: "claude-sonnet-4".to_string(),
            max_tokens: 1024,
            messages: vec![Message {
                role: "user".to_string(),
                content: serde_json::json!("What is the weather today?"),
            }],
            stream: true,
            system: None,
            tools: None,
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: None,
        };

        let query = extract_search_query(&req);
        assert_eq!(query, Some("What is the weather today?".to_string()));
    }

    #[test]
    fn test_create_mcp_request() {
        let (tool_use_id, request) = create_mcp_request("test query");

        assert!(tool_use_id.starts_with("srvtoolu_"));
        assert_eq!(request.jsonrpc, "2.0");
        assert_eq!(request.method, "tools/call");
        assert_eq!(request.params.name, "web_search");
        assert_eq!(request.params.arguments.query, "test query");

        // 验证 ID 格式: web_search_tooluse_{22位}_{时间戳}_{8位}
        assert!(request.id.starts_with("web_search_tooluse_"));
    }

    #[test]
    fn test_mcp_request_id_format() {
        let (_, request) = create_mcp_request("test");

        // 格式: web_search_tooluse_{22位}_{毫秒时间戳}_{8位}
        let id = &request.id;
        assert!(id.starts_with("web_search_tooluse_"));

        let suffix = &id["web_search_tooluse_".len()..];
        let parts: Vec<&str> = suffix.split('_').collect();
        assert_eq!(parts.len(), 3, "应该有3个部分: 22位随机_时间戳_8位随机");

        // 第一部分: 22位大小写字母和数字
        assert_eq!(parts[0].len(), 22);
        assert!(parts[0].chars().all(|c| c.is_ascii_alphanumeric()));

        // 第二部分: 毫秒时间戳
        assert!(parts[1].parse::<i64>().is_ok());

        // 第三部分: 8位小写字母和数字
        assert_eq!(parts[2].len(), 8);
        assert!(
            parts[2]
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        );
    }

    #[test]
    fn test_parse_search_results() {
        let response = McpResponse {
            error: None,
            id: "test_id".to_string(),
            jsonrpc: "2.0".to_string(),
            result: Some(McpResult {
                content: vec![McpContent {
                    content_type: "text".to_string(),
                    text: r#"{"results":[{"title":"Test","url":"https://example.com","snippet":"Test snippet"}],"totalResults":1}"#.to_string(),
                }],
                is_error: false,
            }),
        };

        let results = parse_search_results(&response);
        assert!(results.is_some());
        let results = results.unwrap();
        assert_eq!(results.results.len(), 1);
        assert_eq!(results.results[0].title, "Test");
    }

    #[test]
    fn test_generate_search_summary() {
        let results = WebSearchResults {
            results: vec![WebSearchResult {
                title: "Test Result".to_string(),
                url: "https://example.com".to_string(),
                snippet: Some("This is a test snippet".to_string()),
                published_date: None,
                id: None,
                domain: None,
                max_verbatim_word_limit: None,
                public_domain: None,
            }],
            total_results: Some(1),
            query: Some("test".to_string()),
            error: None,
        };

        let summary = generate_search_summary("test", &Some(results));

        assert!(summary.contains("Test Result"));
        assert!(summary.contains("https://example.com"));
        assert!(summary.contains("This is a test snippet"));
    }

    // ---- normalize_html_text 及内部辅助函数的回归测试 ----
    // 这些测试在修复前会失败：旧代码对 snippet/title 零清洗，<br> 等标签、
    // &nbsp; 等实体会原样出现在结果里。

    #[test]
    fn test_br_variants_become_newline() {
        assert_eq!(normalize_html_text("a<br>b"), "a\nb");
        assert_eq!(normalize_html_text("a<br/>b"), "a\nb");
        assert_eq!(normalize_html_text("a<br />b"), "a\nb");
        assert_eq!(normalize_html_text("a<BR />b"), "a\nb");
        assert_eq!(normalize_html_text("a<Br>b"), "a\nb");
    }

    #[test]
    fn test_block_tags_become_newline() {
        assert_eq!(normalize_html_text("<p>hello</p>"), "hello");
        assert_eq!(normalize_html_text("<div>x</div><div>y</div>"), "x\ny");
        assert_eq!(normalize_html_text("<li>one</li><li>two</li>"), "one\ntwo");
    }

    #[test]
    fn test_entity_decoding() {
        assert_eq!(normalize_html_text("a&nbsp;b"), "a b");
        assert_eq!(normalize_html_text("a&amp;b"), "a&b");
        assert_eq!(normalize_html_text("a&lt;b&gt;c"), "a<b>c");
        assert_eq!(normalize_html_text("a&quot;b&apos;c"), "a\"b'c");
        assert_eq!(normalize_html_text("it&#39;s"), "it's");
        assert_eq!(normalize_html_text("it&#x27;s"), "it's");
        assert_eq!(normalize_html_text("it&#X27;s"), "it's");
    }

    #[test]
    fn test_malformed_and_unclosed_tags_do_not_panic() {
        // 未闭合的标签
        assert_eq!(normalize_html_text("<b>bold"), "bold");
        // 完整但嵌套的标签，剥离标签保留内部文字
        assert_eq!(normalize_html_text(r#"<a href="x">link</a>"#), "link");
        // 孤立的 '<'，找不到匹配的 '>'，应原样保留而不是吞掉后续内容
        assert_eq!(normalize_html_text("a < b"), "a < b");
        // 未识别的 & 实体，没有 ';' 收尾
        assert_eq!(normalize_html_text("a & b"), "a & b");
    }

    #[test]
    fn test_empty_and_tag_only_input() {
        assert_eq!(normalize_html_text(""), "");
        assert_eq!(normalize_html_text("<p></p>"), "");
        assert_eq!(normalize_html_text("<div><span></span></div>"), "");
    }

    #[test]
    fn test_whitespace_collapsing_after_strip() {
        // 剥离标签后不应留下大片空行
        let input = "<p>first</p><p>second</p><p></p><p>third</p>";
        let out = normalize_html_text(input);
        assert!(!out.contains("\n\n\n"));
        assert_eq!(out, "first\nsecond\nthird");
    }

    #[test]
    fn test_summary_snippet_br_is_cleaned() {
        // 端到端：generate_search_summary 输出不应包含裸的 <br>
        let results = WebSearchResults {
            results: vec![WebSearchResult {
                title: "Title<br>With Break".to_string(),
                url: "https://example.com".to_string(),
                snippet: Some("Line one<br>Line two&nbsp;end".to_string()),
                published_date: None,
                id: None,
                domain: None,
                max_verbatim_word_limit: None,
                public_domain: None,
            }],
            total_results: Some(1),
            query: Some("test".to_string()),
            error: None,
        };

        let summary = generate_search_summary("test", &Some(results));
        assert!(!summary.contains("<br"));
        assert!(!summary.contains("&nbsp;"));
        assert!(summary.contains("Line one\nLine two end"));
    }

    #[test]
    fn test_clean_then_truncate_does_not_cut_tag_in_half() {
        // 构造一个 snippet：清洗前第 200 字符恰好落在标签中间。
        // 若先截断后清洗，会残留半个标签；先清洗后截断则不会出现 '<' 或 '>'。
        let mut snippet = "x".repeat(198);
        snippet.push_str("<br>");
        snippet.push_str(&"y".repeat(50));

        let results = WebSearchResults {
            results: vec![WebSearchResult {
                title: "T".to_string(),
                url: "https://example.com".to_string(),
                snippet: Some(snippet),
                published_date: None,
                id: None,
                domain: None,
                max_verbatim_word_limit: None,
                public_domain: None,
            }],
            total_results: Some(1),
            query: Some("test".to_string()),
            error: None,
        };

        let summary = generate_search_summary("test", &Some(results));
        assert!(!summary.contains('<'));
        assert!(!summary.contains('>'));
    }

    #[test]
    fn test_truncate_does_not_split_multibyte_chars_pure_cjk() {
        // 纯中文：201 个汉字，清洗后应截断为 200 字符 + "..."，且不 panic
        // （UTF-8 每个汉字 3 字节，若按字节截断会切碎字符导致 panic 或乱码）
        let snippet: String = "中".repeat(201);
        let cleaned = normalize_html_text(&snippet);
        let truncated = match cleaned.char_indices().nth(200) {
            Some((idx, _)) => format!("{}...", &cleaned[..idx]),
            None => cleaned,
        };
        assert_eq!(truncated.chars().filter(|&c| c == '中').count(), 200);
        assert!(truncated.ends_with("..."));
    }

    #[test]
    fn test_truncate_does_not_split_multibyte_chars_mixed_width() {
        // 混合宽度：ASCII + 中文交替，同样验证不会在多字节字符中间截断
        let snippet: String = "a中".repeat(150); // 300 字符
        let cleaned = normalize_html_text(&snippet);
        let truncated = match cleaned.char_indices().nth(200) {
            Some((idx, _)) => format!("{}...", &cleaned[..idx]),
            None => cleaned,
        };
        // 不 panic 即说明截断边界落在字符边界上；再校验字符数与内容正确
        assert_eq!(truncated.chars().count(), 203); // 200 + "..."
        assert!(truncated.starts_with("a中a中"));
    }

    /// 源码级守卫：MCP 上游调用失败必须返回非 200，不得伪装成「搜索无结果」的
    /// 200 空结果 SSE（已确认缺陷）。
    ///
    /// 单测覆盖不到该分支（需要真实 KiroProvider + 上游），用源码断言钉死：
    /// 回归成「Err → None 落回 200」时本条 FAIL。
    #[test]
    fn websearch_upstream_failure_must_not_be_swallowed_as_empty_200() {
        let full = include_str!("websearch.rs");
        // 只查生产代码段：把本测试自身的 needle 排除在命中集外。
        let prod = full
            .split_once("\n#[cfg(test)]")
            .map(|(a, _)| a)
            .unwrap_or(full);
        let fn_body = prod
            .split("async fn handle_websearch_request")
            .nth(1)
            .expect("handle_websearch_request 不应被改名");
        // ⚠️ 截到下一个顶层函数为止（2026-08-11 审计修复）：不截的话 fn_body 延伸到
        // 文件末尾，`call_mcp_api`/`decode_round` 里的 BAD_GATEWAY（1322/1332/1386）
        // 会让断言在**删掉目标分支后**仍然满足 → 守卫静默变绿。同文件
        // `replay_loop_must_not_swallow_mcp_failure` 的注释记录过这个坑。
        let fn_body = fn_body
            .split("\nasync fn ")
            .next()
            .expect("split 至少一段");
        assert!(
            fn_body.contains("BAD_GATEWAY"),
            "MCP 上游调用失败必须返回非 200（502），不能落回 200 空结果伪装成「无结果」"
        );
        assert!(
            fn_body.contains("Ok(response) => parse_search_results(&response)"),
            "Ok 分支必须继续 parse_search_results，正常「无结果」仍走 200 空结果路径"
        );
    }

    // ==================== WebSearch agentic 多轮回灌 ====================

    /// 测试辅助：构造一条已完成搜索的 web_search（带 2 条结果）。
    fn mk_searched(upstream_id: &str, query: &str) -> SearchedWebSearch {
        SearchedWebSearch {
            upstream_id: upstream_id.to_string(),
            query: query.to_string(),
            srv_id: format!("srvtoolu_{}", upstream_id),
            results: Some(WebSearchResults {
                results: vec![
                    WebSearchResult {
                        title: "Rust 1.97 发布<br>公告".to_string(),
                        url: "https://blog.rust-lang.org/1.97".to_string(),
                        snippet: Some("Rust&nbsp;1.97 已发布".to_string()),
                        published_date: Some(1_700_000_000_000),
                        id: None,
                        domain: Some("blog.rust-lang.org".to_string()),
                        max_verbatim_word_limit: None,
                        public_domain: None,
                    },
                    WebSearchResult {
                        title: "Release notes".to_string(),
                        url: "https://example.invalid/notes".to_string(),
                        snippet: None,
                        published_date: None,
                        id: None,
                        domain: None,
                        max_verbatim_word_limit: None,
                        public_domain: None,
                    },
                ],
                total_results: Some(2),
                query: Some(query.to_string()),
                error: None,
            }),
        }
    }

    #[test]
    fn append_search_round_feeds_paired_tool_use_and_tool_result() {
        // 回灌的核心不变量：assistant 的 web_search tool_use 必须与紧随其后的
        // user tool_result **按 upstream_id 配对** —— 否则 converter 的
        // remove_orphaned_tool_uses 会把它从历史里删掉，下一轮上游看不到搜过什么。
        let mut req = mk_req(
            "rust 最新版本是多少",
            Some(vec![mk_web_search_tool(), mk_plain_tool("Bash")]),
        );
        let before_len = req.messages.len();
        let searched = vec![mk_searched("tooluse_abc", "rust latest version")];

        let presentation = append_search_round(&mut req, "让我搜一下。", &searched);

        // 恰好追加两条：assistant（正文 + tool_use）+ user（tool_result）
        assert_eq!(req.messages.len(), before_len + 2);
        let assistant = &req.messages[before_len];
        assert_eq!(assistant.role, "assistant");
        let a_blocks = assistant.content.as_array().expect("assistant 应为块数组");
        assert_eq!(
            a_blocks[0]["type"], "text",
            "上游本轮正文要一并回灌，否则模型下一轮会重复同一次搜索"
        );
        assert_eq!(a_blocks[0]["text"], "让我搜一下。");
        assert_eq!(a_blocks[1]["type"], "tool_use");
        assert_eq!(a_blocks[1]["name"], "web_search");
        assert_eq!(a_blocks[1]["id"], "tooluse_abc");
        assert_eq!(a_blocks[1]["input"]["query"], "rust latest version");

        let user = &req.messages[before_len + 1];
        assert_eq!(user.role, "user");
        let u_blocks = user.content.as_array().expect("user 应为块数组");
        assert_eq!(u_blocks[0]["type"], "tool_result");
        assert_eq!(
            u_blocks[0]["tool_use_id"], "tooluse_abc",
            "tool_result 必须用**上游** tool_use_id 配对，不能用 srvtoolu_ 那个展示 id"
        );
        // 回灌给上游的是摘要文本（Kiro 只认 tool_result 的纯文本），不是结构化块
        let summary = u_blocks[0]["content"]
            .as_str()
            .expect("回灌给上游的 tool_result content 必须是纯文本摘要");
        assert!(summary.contains("rust latest version"));
        assert!(summary.contains("blog.rust-lang.org"));

        // 客户端展示块：server_tool_use + web_search_tool_result，一次搜索两块
        assert_eq!(presentation.len(), 2);
        assert_eq!(presentation[0]["type"], "server_tool_use");
        assert_eq!(presentation[0]["name"], "web_search");
        assert_eq!(presentation[0]["id"], "srvtoolu_tooluse_abc");
        assert_eq!(presentation[1]["type"], "web_search_tool_result");
        assert_eq!(
            presentation[1]["tool_use_id"], "srvtoolu_tooluse_abc",
            "web_search_tool_result 必须带 tool_use_id 且等于前面 server_tool_use 的 id\
             （与快路径 generate_websearch_events 同契约，严格 SDK 客户端据此配对）"
        );
        let blocks = presentation[1]["content"]
            .as_array()
            .expect("content 应为 web_search_result 数组");
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0]["type"], "web_search_result");
        assert!(
            !blocks[0]["title"].as_str().unwrap().contains("<br>"),
            "title 必须过 normalize_html_text，否则客户端渲染出裸 HTML 标签"
        );
        assert!(
            !blocks[0]["encrypted_content"]
                .as_str()
                .unwrap()
                .contains("&nbsp;"),
            "snippet 同样要清洗 HTML 实体"
        );
        assert!(
            blocks[0]["page_age"].is_string(),
            "有 publishedDate 时应格式化出 page_age"
        );
        assert!(
            blocks[1]["page_age"].is_null(),
            "无 publishedDate 时 page_age 应为 null，不能编一个"
        );
    }

    #[test]
    fn append_search_round_pairs_every_search_in_parallel_round() {
        // 上游一轮可能并发发多条 web_search。每条都要各自配对，
        // 漏掉一条 → 那条 tool_use 变孤立 → 整条历史被 converter 改写。
        let mut req = mk_req("对比 A 和 B", Some(vec![mk_web_search_tool()]));
        let searched = vec![mk_searched("t1", "query A"), mk_searched("t2", "query B")];

        let presentation = append_search_round(&mut req, "", &searched);

        let last = req.messages.last().unwrap();
        let u_blocks = last.content.as_array().unwrap();
        assert_eq!(u_blocks.len(), 2, "两条搜索要有两条 tool_result");
        assert_eq!(u_blocks[0]["tool_use_id"], "t1");
        assert_eq!(u_blocks[1]["tool_use_id"], "t2");

        let assistant = &req.messages[req.messages.len() - 2];
        let a_blocks = assistant.content.as_array().unwrap();
        assert_eq!(
            a_blocks.len(),
            2,
            "assistant_text 为空时不应插入空 text 块，只有两个 tool_use"
        );
        assert!(a_blocks.iter().all(|b| b["type"] == "tool_use"));
        assert_eq!(presentation.len(), 4, "两条搜索 → 4 个展示块");
    }

    #[test]
    fn should_replay_only_when_round_is_pure_web_search() {
        let ws = vec![DecodedWebSearch {
            id: "t1".to_string(),
            query: "q".to_string(),
        }];

        // 纯 web_search 且未超上限 → 回灌
        assert!(should_replay_round(0, &ws, false));
        assert!(should_replay_round(MAX_WEB_SEARCH_ROUNDS - 1, &ws, false));

        // 🔴 混入客户端工具（Bash/Edit 等）→ 绝不回灌，整轮回给客户端。
        // 回灌会把客户端工具**吞掉**，用户看到的现象是"CC 说要跑命令但什么都没发生"。
        assert!(
            !should_replay_round(0, &ws, true),
            "混入非 web_search 工具时必须收尾，不能吞掉客户端工具"
        );

        // 没有 web_search tool_use → 正常回答，收尾
        assert!(!should_replay_round(0, &[], false));

        // 达到轮数上限 → 停止回灌（防上游持续要搜索导致无限循环）
        assert!(
            !should_replay_round(MAX_WEB_SEARCH_ROUNDS, &ws, false),
            "到达 MAX_WEB_SEARCH_ROUNDS 必须停止回灌"
        );
    }

    #[test]
    fn build_result_block_handles_missing_and_empty_results() {
        // MCP 返回 None（解析失败/无结果）→ 空数组，不能 panic 也不能编造结果
        assert!(build_result_block(&None).is_empty());

        let empty = Some(WebSearchResults {
            results: vec![],
            total_results: Some(0),
            query: Some("nothing".to_string()),
            error: None,
        });
        assert!(build_result_block(&empty).is_empty());
    }

    #[test]
    fn loop_sse_events_render_each_block_type_in_order() {
        // 收尾渲染：presentation 块 + 正文 + 客户端 tool_use，index 必须按 content 顺序递增，
        // 且每块都要 start/stop 成对（缺 stop 客户端会一直等那个块结束）。
        let success = WebSearchLoopSuccess {
            model: "claude-sonnet-5".to_string(),
            content: vec![
                json!({"type": "server_tool_use", "id": "srvtoolu_1", "name": "web_search",
                       "input": {"query": "q"}}),
                json!({"type": "web_search_tool_result", "tool_use_id": "srvtoolu_1",
                       "content": []}),
                json!({"type": "text", "text": "搜到了这些"}),
                json!({"type": "tool_use", "id": "tu_1", "name": "Bash",
                       "input": {"command": "ls"}}),
            ],
            stop_reason: "tool_use".to_string(),
            input_tokens: 1234,
            output_tokens: 56,
            credential_id: 7,
            credits: 0.25,
            rounds: 3,
        };

        let events = build_loop_sse_events(&success);

        assert_eq!(events.first().unwrap().event, "message_start");
        assert_eq!(
            events.first().unwrap().data["message"]["usage"]["input_tokens"],
            json!(1234)
        );
        assert_eq!(events[events.len() - 2].event, "message_delta");
        assert_eq!(
            events[events.len() - 2].data["delta"]["stop_reason"],
            "tool_use"
        );
        assert_eq!(
            events[events.len() - 2].data["usage"]["output_tokens"],
            json!(56)
        );
        assert_eq!(events.last().unwrap().event, "message_stop");

        // 每个 index 都要有 start 与 stop
        for idx in 0..4i32 {
            assert!(
                events.iter().any(|e| e.event == "content_block_start"
                    && e.data["index"] == json!(idx)),
                "index {} 缺 content_block_start",
                idx
            );
            assert!(
                events.iter().any(|e| e.event == "content_block_stop"
                    && e.data["index"] == json!(idx)),
                "index {} 缺 content_block_stop（客户端会一直等该块结束）",
                idx
            );
        }

        // 客户端 tool_use 的参数一次性发单个 input_json_delta（合法完整 JSON）
        let delta = events
            .iter()
            .find(|e| {
                e.event == "content_block_delta" && e.data["delta"]["type"] == "input_json_delta"
            })
            .expect("客户端 tool_use 必须下发 input_json_delta");
        let partial = delta.data["delta"]["partial_json"].as_str().unwrap();
        let parsed: Value = serde_json::from_str(partial).expect("partial_json 必须是合法 JSON");
        assert_eq!(parsed["command"], "ls");

        // 🔴 客户端工具必须原样出现在最终 content 里，绝不被回灌吞掉
        let has_bash = events.iter().any(|e| {
            e.event == "content_block_start" && e.data["content_block"]["name"] == "Bash"
        });
        assert!(has_bash, "非 web_search 的 tool_use 必须回给客户端");
    }

    #[test]
    fn loop_sse_events_render_thinking_block() {
        // thinking 块必须：start(thinking:"") → thinking_delta → signature_delta → stop。
        // 缺 signature_delta 客户端 thinking 模式本地校验会失败；缺 stop 客户端一直等块结束。
        let success = WebSearchLoopSuccess {
            model: "claude-sonnet-5".to_string(),
            content: vec![json!({
                "type": "thinking",
                "thinking": "搜之前先想一下",
                "signature": "sig_abc"
            })],
            stop_reason: "end_turn".to_string(),
            input_tokens: 10,
            output_tokens: 5,
            credential_id: 0,
            credits: 0.0,
            rounds: 1,
        };
        let events = build_loop_sse_events(&success);
        assert!(events.iter().any(|e| {
            e.event == "content_block_start"
                && e.data["content_block"]["type"] == "thinking"
                && e.data["content_block"]["thinking"] == ""
        }));
        assert!(events.iter().any(|e| {
            e.event == "content_block_delta"
                && e.data["delta"]["type"] == "thinking_delta"
                && e.data["delta"]["thinking"] == "搜之前先想一下"
        }));
        assert!(events.iter().any(|e| {
            e.event == "content_block_delta"
                && e.data["delta"]["type"] == "signature_delta"
                && e.data["delta"]["signature"] == "sig_abc"
        }));
        assert_eq!(
            events
                .iter()
                .filter(|e| e.event == "content_block_stop")
                .count(),
            1,
            "thinking 块必须有且仅有一个 stop"
        );
    }

    #[test]
    fn loop_json_body_carries_content_and_usage() {
        let success = WebSearchLoopSuccess {
            model: "claude-sonnet-5".to_string(),
            content: vec![json!({"type": "text", "text": "done"})],
            stop_reason: "end_turn".to_string(),
            input_tokens: 100,
            output_tokens: 20,
            credential_id: 1,
            credits: 0.0,
            rounds: 1,
        };
        let body = build_loop_json_body(&success);
        assert_eq!(body["type"], "message");
        assert_eq!(body["role"], "assistant");
        assert_eq!(body["model"], "claude-sonnet-5");
        assert_eq!(body["stop_reason"], "end_turn");
        assert_eq!(body["content"][0]["text"], "done");
        assert_eq!(body["usage"]["input_tokens"], json!(100));
        assert_eq!(body["usage"]["output_tokens"], json!(20));
    }

    /// 源码级守卫：MCP 搜索失败不得静默降级成「没搜到」再继续回灌。
    ///
    /// 回灌链路里这个降级比快路径更危险：把"搜索服务故障"当成"没搜到"回灌进历史后，
    /// 模型会基于错误前提继续推理若干轮，最终给出自信的错误答案，且客户端拿到 200。
    /// 单测覆盖不到该分支（需真实 provider + MCP），用源码断言钉死。
    #[test]
    fn replay_loop_must_not_swallow_mcp_failure() {
        let full = include_str!("websearch.rs");
        let prod = full
            .split_once("\n#[cfg(test)]")
            .map(|(a, _)| a)
            .unwrap_or(full);
        let after = prod
            .split("pub(super) async fn run_web_search_loop")
            .nth(1)
            .expect("run_web_search_loop 不应被改名");
        // 截到下一个顶层 fn 为止：后面 build_loop_sse_events 里也有 BAD_GATEWAY-free 的
        // 代码，但不截会让断言被**其它函数**里的字面量满足 → 守卫形同虚设。
        let fn_body = after
            .split("\npub(super) fn ")
            .next()
            .expect("split 至少一段");
        assert!(
            fn_body.contains("BAD_GATEWAY"),
            "MCP 调用失败必须整体返错（502），不能把失败当「没搜到」继续回灌"
        );
        assert!(
            fn_body.contains("should_replay_round"),
            "是否继续回灌必须走 should_replay_round，不得在循环里内联另一套判据"
        );
    }

    /// 源码级守卫：上游 in-band 错误 / 流截断的轮次不得被回灌。
    ///
    /// 半截内容回灌进历史 = 把假事实写进上下文，后续所有轮都基于错误前提。
    #[test]
    fn replay_round_must_reject_truncated_upstream_round() {
        let full = include_str!("websearch.rs");
        let prod = full
            .split_once("\n#[cfg(test)]")
            .map(|(a, _)| a)
            .unwrap_or(full);
        // 截到下一个顶层 fn 为止：否则 split 出来的尾巴包含 run_web_search_loop，
        // 断言会被**别的函数**里的同名字符串满足 → 守卫形同虚设。
        let after = prod
            .split("async fn run_round")
            .nth(1)
            .expect("run_round 不应被改名");
        let fn_body = after
            .split("\npub(super) async fn ")
            .next()
            .expect("split 至少一段");
        assert!(
            fn_body.contains("outcome.stream_error"),
            "流中断/解码器停止的轮次必须报错返回，不能当成功回灌"
        );
        assert!(
            fn_body.contains("outcome.upstream_error"),
            "上游 in-band 错误的轮次必须报错返回，不能当成功回灌"
        );
    }
}
