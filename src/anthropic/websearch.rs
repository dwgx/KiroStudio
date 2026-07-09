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

/// 解析 MCP 响应中的搜索结果
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
                    "encrypted_content": r.snippet.clone().unwrap_or_default(),
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
            summary.push_str(&format!("{}. **{}**\n", i + 1, result.title));
            if let Some(ref snippet) = result.snippet {
                // 截断过长的摘要（安全处理 UTF-8 多字节字符）
                let truncated = match snippet.char_indices().nth(200) {
                    Some((idx, _)) => format!("{}...", &snippet[..idx]),
                    None => snippet.clone(),
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
    let search_results = match call_mcp_api(&provider, &mcp_request).await {
        Ok(response) => parse_search_results(&response),
        Err(e) => {
            tracing::warn!("MCP API 调用失败: {}", e);
            None
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
    fn mk_req(user_text: &str, tools: Option<Vec<crate::anthropic::types::Tool>>) -> MessagesRequest {
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
}
