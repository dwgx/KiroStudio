//! WebSearch 工具处理模块
//!
//! 实现 Anthropic WebSearch 请求到 Kiro MCP 的转换和响应生成
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
use futures::{Stream, stream};
use serde::{Deserialize, Serialize};
use serde_json::json;
use uuid::Uuid;

use super::stream::SseEvent;
use super::types::{ErrorResponse, MessagesRequest, Tool};

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
/// 其余“混合工具但未显式触发搜索”的请求走常规转发路径（配合 strip_web_search_tools）。
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
    let search_results = match call_mcp_api(&provider, &mcp_request).await {
        Ok(response) => parse_search_results(&response),
        Err(e) => {
            tracing::warn!("MCP API 调用失败: {}", e);
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
) -> anyhow::Result<McpResponse> {
    let request_body = serde_json::to_string(request)?;

    tracing::debug!("MCP request: {}", request_body);

    let response = provider.call_mcp(&request_body).await?;

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

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(
            fn_body.contains("BAD_GATEWAY"),
            "MCP 上游调用失败必须返回非 200（502），不能落回 200 空结果伪装成「无结果」"
        );
        assert!(
            fn_body.contains("Ok(response) => parse_search_results(&response)"),
            "Ok 分支必须继续 parse_search_results，正常「无结果」仍走 200 空结果路径"
        );
    }
}
