//! OpenAI 兼容入站 handler(/v1/chat/completions)。
//!
//! 复用现有 Anthropic 管线:翻译 OpenAI 请求 → 调 `anthropic::handlers::post_messages`
//! (整条管线自动复用)→ 把返回的 Anthropic SSE(流式)/ Messages JSON(非流式)翻回 OpenAI。

use axum::{
    body::{Body, Bytes},
    extract::{ConnectInfo, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use futures::StreamExt;
use serde_json::{json, Value};
use std::net::SocketAddr;

use crate::anthropic::middleware::AppState;
use crate::anthropic::model_catalog;
use crate::openai::convert;
use crate::openai::types::ChatCompletionsPeek;

/// 读取上游响应体的硬上限(纵深防护):正常响应远小于此(受 max_tokens + 256MiB body 限约束),
/// 但显式封顶避免异常/恶意超大响应把整个响应体读进内存打爆(不用 usize::MAX)。
const MAX_RESP_BYTES: usize = 64 * 1024 * 1024;

/// POST /v1/chat/completions
pub async fn post_chat_completions(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    raw_body: Bytes,
) -> Response {
    // 解析原始请求(灵活 Value)+ 取 model/stream。
    let raw: Value = match serde_json::from_slice(&raw_body) {
        Ok(v) => v,
        Err(e) => return openai_error(StatusCode::BAD_REQUEST, "invalid_request_error", &format!("请求体解析失败: {e}")),
    };
    let peek: ChatCompletionsPeek = match serde_json::from_value(raw.clone()) {
        Ok(p) => p,
        Err(e) => return openai_error(StatusCode::BAD_REQUEST, "invalid_request_error", &format!("缺少必填字段: {e}")),
    };

    // model 经 catalog 归一(GPT-5.6 三变体已在表);未识别则原样透传给上游(由上游决定认不认)。
    let resolved_model = model_catalog::resolve_kiro_id(&peek.model)
        .map(|s| s.to_string())
        .unwrap_or_else(|| peek.model.clone());
    // 出站给客户端的 model 名回显客户端请求的原名(OpenAI 惯例)。
    let echo_model = peek.model.clone();

    // 翻译成 Anthropic 请求体。
    let anthropic_req = convert::openai_chat_to_anthropic(&resolved_model, &raw, peek.stream);
    let anthropic_bytes = match serde_json::to_vec(&anthropic_req) {
        Ok(b) => Bytes::from(b),
        Err(e) => return openai_error(StatusCode::INTERNAL_SERVER_ERROR, "internal_error", &format!("请求翻译失败: {e}")),
    };

    tracing::info!(
        model = %peek.model,
        resolved = %resolved_model,
        stream = %peek.stream,
        "Received POST /v1/chat/completions request"
    );

    // 复用现有 Anthropic 管线(custom_api 透传 / failover / 工具修复 / 泄漏清洗 / 用量埋点)。
    // 传入合成的 Anthropic 请求体 + 原始 headers(用于 CC 识别/client 画像/鉴权已在中间件过)。
    let anthropic_resp = crate::anthropic::handlers::post_messages(
        State(state),
        ConnectInfo(peer),
        headers,
        anthropic_bytes,
    )
    .await;

    // 非 2xx:把 Anthropic 错误体翻成 OpenAI 错误结构透出。
    if !anthropic_resp.status().is_success() {
        return translate_error_response(anthropic_resp).await;
    }

    if peek.stream {
        stream_openai_from_anthropic(anthropic_resp, echo_model).await
    } else {
        nonstream_openai_from_anthropic(anthropic_resp, echo_model).await
    }
}

/// POST /v1/responses(Codex 等走此端点)。
/// 与 chat/completions 同管线,区别在请求/响应用 Responses 协议转换器。
/// previous_response_id 无状态兼容:忽略(上游无状态,要求客户端发全量 input);回稳定 response.id。
pub async fn post_responses(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    raw_body: Bytes,
) -> Response {
    let raw: Value = match serde_json::from_slice(&raw_body) {
        Ok(v) => v,
        Err(e) => return openai_error(StatusCode::BAD_REQUEST, "invalid_request_error", &format!("请求体解析失败: {e}")),
    };
    let model = match raw.get("model").and_then(|v| v.as_str()) {
        Some(m) => m.to_string(),
        None => return openai_error(StatusCode::BAD_REQUEST, "invalid_request_error", "缺少必填字段 model"),
    };
    let stream = raw.get("stream").and_then(|v| v.as_bool()).unwrap_or(false);

    let resolved_model = model_catalog::resolve_kiro_id(&model).map(|s| s.to_string()).unwrap_or_else(|| model.clone());
    let echo_model = model.clone();

    let anthropic_req = convert::openai_responses_to_anthropic(&resolved_model, &raw, stream);
    let anthropic_bytes = match serde_json::to_vec(&anthropic_req) {
        Ok(b) => Bytes::from(b),
        Err(e) => return openai_error(StatusCode::INTERNAL_SERVER_ERROR, "internal_error", &format!("请求翻译失败: {e}")),
    };

    tracing::info!(model = %model, resolved = %resolved_model, stream = %stream, "Received POST /v1/responses request");

    let anthropic_resp = crate::anthropic::handlers::post_messages(
        State(state), ConnectInfo(peer), headers, anthropic_bytes,
    ).await;

    if !anthropic_resp.status().is_success() {
        return translate_error_response(anthropic_resp).await;
    }

    if stream {
        stream_responses_from_anthropic(anthropic_resp, echo_model).await
    } else {
        nonstream_responses_from_anthropic(anthropic_resp, echo_model).await
    }
}

/// 流式:Anthropic SSE → Responses SSE 事件序列(每事件 `event: T\ndata: {..}\n\n`)。
async fn stream_responses_from_anthropic(resp: Response, model: String) -> Response {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    let body = resp.into_body();
    let mut conv = convert::ResponsesStreamConverter::new(model);
    let error_seen = Arc::new(AtomicBool::new(false));
    let error_seen_cb = error_seen.clone();

    let out_stream = async_stream_from_body(body, error_seen, true, move |line, sink| {
        let payload = match line.strip_prefix("data:") { Some(p) => p.trim(), None => return };
        if payload.is_empty() { return; }
        if let Ok(ev) = serde_json::from_str::<Value>(payload) {
            for (event_type, data) in conv.push_event(&ev) {
                if event_type == "response.failed" {
                    error_seen_cb.store(true, Ordering::Relaxed);
                }
                // Responses SSE:带 event: 行(严格客户端按类型分派)。
                sink.push(format!("event: {}\ndata: {}\n\n", event_type, data));
            }
        }
    });

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONNECTION, "keep-alive")
        .body(Body::from_stream(out_stream))
        .unwrap()
}

/// 非流式:收齐 Anthropic body → 聚合成单个 Responses response JSON。
async fn nonstream_responses_from_anthropic(resp: Response, model: String) -> Response {
    let bytes = match axum::body::to_bytes(resp.into_body(), MAX_RESP_BYTES).await {
        Ok(b) => b,
        Err(e) => return openai_error(StatusCode::BAD_GATEWAY, "api_error", &format!("读取上游响应失败: {e}")),
    };
    let text = String::from_utf8_lossy(&bytes);
    let events = parse_sse_or_message(&text);
    let response = convert::aggregate_responses(&model, &events);
    (StatusCode::OK, Json(response)).into_response()
}

/// 流式:把 Anthropic SSE body 逐帧翻成 OpenAI chat.completion.chunk SSE。
async fn stream_openai_from_anthropic(resp: Response, model: String) -> Response {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    let body = resp.into_body();
    let mut conv = convert::ChatStreamConverter::new(model);
    // in-band 错误标志:转换器吐出 {"error":...} chunk 时置位,让流末尾**不发 [DONE]**
    // (上游中途 error 事件是正常 transport 读,stream_errored 抓不到,但同样不能当成功收尾)。
    let error_seen = Arc::new(AtomicBool::new(false));
    let error_seen_cb = error_seen.clone();

    // 逐行解析 Anthropic SSE(data: {json}),喂状态机,输出 OpenAI chunk;流末尾发 [DONE]。
    let out_stream = async_stream_from_body(body, error_seen, false, move |line, sink| {
        // 只处理 data: 行。
        let payload = match line.strip_prefix("data:") {
            Some(p) => p.trim(),
            None => return,
        };
        if payload.is_empty() {
            return;
        }
        if let Ok(ev) = serde_json::from_str::<Value>(payload) {
            for chunk in conv.push_event(&ev) {
                if chunk.get("error").is_some() {
                    error_seen_cb.store(true, Ordering::Relaxed);
                }
                sink.push(format!("data: {}\n\n", chunk));
            }
        }
    });

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONNECTION, "keep-alive")
        .body(Body::from_stream(out_stream))
        .unwrap()
}

/// 非流式:收齐 Anthropic SSE body → 聚合成单个 OpenAI chat.completion JSON。
async fn nonstream_openai_from_anthropic(resp: Response, model: String) -> Response {
    let bytes = match axum::body::to_bytes(resp.into_body(), MAX_RESP_BYTES).await {
        Ok(b) => b,
        Err(e) => return openai_error(StatusCode::BAD_GATEWAY, "api_error", &format!("读取上游响应失败: {e}")),
    };
    // 内部非流式路径可能直接返回 Anthropic Messages JSON(非 SSE),也可能是 SSE 行。
    // 先尝试当 SSE 行解析事件;若整体是一个 JSON 对象(message),转成单事件序列。
    let text = String::from_utf8_lossy(&bytes);
    let events = parse_sse_or_message(&text);
    let completion = convert::aggregate_chat_completion(&model, &events);
    (StatusCode::OK, Json(completion)).into_response()
}

/// 把响应体文本解析成 Anthropic 事件序列:优先按 SSE data: 行;否则把整个 Messages JSON 合成事件。
fn parse_sse_or_message(text: &str) -> Vec<Value> {
    let mut events: Vec<Value> = Vec::new();
    let mut saw_data = false;
    for line in text.lines() {
        if let Some(p) = line.strip_prefix("data:") {
            let p = p.trim();
            if p.is_empty() || p == "[DONE]" {
                continue;
            }
            if let Ok(ev) = serde_json::from_str::<Value>(p) {
                saw_data = true;
                events.push(ev);
            }
        }
    }
    if saw_data {
        return events;
    }
    // 整体是一个 Anthropic Messages 响应对象 → 合成 message_start + content_block_* + message_delta。
    if let Ok(msg) = serde_json::from_str::<Value>(text.trim()) {
        return synthesize_events_from_message(&msg);
    }
    events
}

/// 把一个完整 Anthropic Messages 响应对象合成为聚合器能吃的事件序列。
fn synthesize_events_from_message(msg: &Value) -> Vec<Value> {
    let mut events = vec![json!({"type": "message_start", "message": msg})];
    if let Some(Value::Array(content)) = msg.get("content") {
        for (i, block) in content.iter().enumerate() {
            let idx = i as i64;
            match block.get("type").and_then(|v| v.as_str()) {
                Some("text") => {
                    let t = block.get("text").and_then(|v| v.as_str()).unwrap_or("");
                    events.push(json!({"type": "content_block_start", "index": idx, "content_block": {"type": "text"}}));
                    events.push(json!({"type": "content_block_delta", "index": idx, "delta": {"type": "text_delta", "text": t}}));
                    events.push(json!({"type": "content_block_stop", "index": idx}));
                }
                Some("thinking") => {
                    let t = block.get("thinking").and_then(|v| v.as_str()).unwrap_or("");
                    events.push(json!({"type": "content_block_start", "index": idx, "content_block": {"type": "thinking"}}));
                    events.push(json!({"type": "content_block_delta", "index": idx, "delta": {"type": "thinking_delta", "thinking": t}}));
                    events.push(json!({"type": "content_block_stop", "index": idx}));
                }
                Some("tool_use") => {
                    let id = block.get("id").and_then(|v| v.as_str()).unwrap_or("");
                    let name = block.get("name").and_then(|v| v.as_str()).unwrap_or("");
                    let input = block.get("input").cloned().unwrap_or_else(|| json!({}));
                    events.push(json!({"type": "content_block_start", "index": idx, "content_block": {"type": "tool_use", "id": id, "name": name}}));
                    events.push(json!({"type": "content_block_delta", "index": idx, "delta": {"type": "input_json_delta", "partial_json": input.to_string()}}));
                    events.push(json!({"type": "content_block_stop", "index": idx}));
                }
                _ => {}
            }
        }
    }
    let stop_reason = msg.get("stop_reason").and_then(|v| v.as_str()).unwrap_or("end_turn");
    let mut delta = json!({"type": "message_delta", "delta": {"stop_reason": stop_reason}});
    if let Some(u) = msg.get("usage") {
        delta["usage"] = u.clone();
    }
    events.push(delta);
    events
}

/// 把 Anthropic 错误响应翻成 OpenAI 错误结构。
async fn translate_error_response(resp: Response) -> Response {
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), MAX_RESP_BYTES).await.unwrap_or_default();
    let text = String::from_utf8_lossy(&bytes);
    // Anthropic 错误体形如 {"type":"error","error":{"type":..,"message":..}} 或 {"error":{...}}。
    let (msg, typ) = serde_json::from_str::<Value>(text.trim())
        .ok()
        .and_then(|v| {
            let e = v.get("error").cloned().unwrap_or(v);
            let m = e.get("message").and_then(|x| x.as_str()).map(String::from);
            let t = e.get("type").and_then(|x| x.as_str()).map(String::from);
            m.map(|m| (m, t.unwrap_or_else(|| "api_error".into())))
        })
        .unwrap_or_else(|| (text.trim().to_string(), "api_error".to_string()));
    openai_error(status, &typ, &msg)
}

/// 构造 OpenAI 错误响应。
fn openai_error(status: StatusCode, err_type: &str, message: &str) -> Response {
    (
        status,
        Json(json!({"error": {"message": message, "type": err_type}})),
    )
        .into_response()
}

/// 把一个 axum Body(Anthropic SSE)按行喂给回调,回调把要发出的 OpenAI SSE 字符串 push 进 sink,
/// 末尾自动追加 `data: [DONE]`。返回一个 `Stream<Item=Result<Bytes,Infallible>>`。
///
/// **按字节缓冲**(而非每 chunk `from_utf8_lossy`):上游/透传的原始网络 chunk 可能在多字节字符
/// (中文/emoji)中间切断,逐 chunk 解码会把跨界字符变成 U+FFFD 永久损坏。故缓冲 `Vec<u8>`、
/// 只在完整 `\n\n` 事件边界处解码(SSE 事件本身是 UTF-8 完整的),彻底规避跨界损坏。
/// `is_responses`:true=Responses 协议(终结帧带 `event:` 行,断开发 `event: response.failed`,
/// 正常结束不发 `[DONE]`——Responses 终止信号是 response.completed);false=chat/completions
/// (断开发裸 `data:{error}`,正常结束发 `data: [DONE]`)。
fn async_stream_from_body(
    body: Body,
    error_seen: std::sync::Arc<std::sync::atomic::AtomicBool>,
    is_responses: bool,
    mut on_line: impl FnMut(&str, &mut Vec<String>) + Send + 'static,
) -> impl futures::Stream<Item = Result<Bytes, std::convert::Infallible>> + Send {
    use async_stream::stream;
    stream! {
        let mut data_stream = body.into_data_stream();
        let mut buf: Vec<u8> = Vec::new();
        let mut stream_errored = false;
        while let Some(chunk) = data_stream.next().await {
            let chunk = match chunk {
                Ok(c) => c,
                Err(_) => {
                    // 上游/客户端断开:标记异常结束(不发 [DONE],避免把截断当正常收尾)。
                    stream_errored = true;
                    break;
                }
            };
            buf.extend_from_slice(&chunk);
            // 按 SSE 事件分隔(\n\n 或 \r\n\r\n)切分,保留未完整的尾巴;只对完整块解码。
            while let Some((pos, sep_len)) = find_sse_boundary(&buf) {
                let block: Vec<u8> = buf.drain(..pos + sep_len).collect();
                let block_str = String::from_utf8_lossy(&block);
                for line in block_str.lines() {
                    let mut sink: Vec<String> = Vec::new();
                    on_line(line, &mut sink);
                    for s in sink {
                        yield Ok(Bytes::from(s));
                    }
                }
            }
            // 纵深防护:未切帧的缓冲不该无界增长(异常上游/无分隔符)。超上限即当异常终止,
            // 避免把整段响应堆进内存(与非流式 MAX_RESP_BYTES 同口径)。
            if buf.len() > MAX_RESP_BYTES {
                tracing::warn!("OpenAI 流式:未切帧缓冲超上限 {} 字节,终止", MAX_RESP_BYTES);
                stream_errored = true;
                break;
            }
        }
        // flush 残留(无 \n\n 结尾的最后一块)。
        if !buf.is_empty() {
            let tail = String::from_utf8_lossy(&buf);
            for line in tail.lines() {
                let mut sink: Vec<String> = Vec::new();
                on_line(line, &mut sink);
                for s in sink {
                    yield Ok(Bytes::from(s));
                }
            }
        }
        // 收尾:两种「不算正常完成」的情况都不发正常终止帧(避免客户端把截断当成功):
        //   ① transport 层断开(stream_errored);② in-band error 事件已吐 error(error_seen)。
        if stream_errored {
            // transport 断开:补发协议对应的失败终结事件显式告知。
            if is_responses {
                let err = serde_json::json!({"type": "response.failed",
                    "response": {"status": "failed", "error": {"message": "上游响应中断(stream interrupted)"}}});
                yield Ok(Bytes::from(format!("event: response.failed\ndata: {}\n\n", err)));
            } else {
                let err = serde_json::json!({"error": {"message": "上游响应中断(stream interrupted)", "type": "api_error"}});
                yield Ok(Bytes::from(format!("data: {}\n\n", err)));
            }
        } else if error_seen.load(std::sync::atomic::Ordering::Relaxed) {
            // in-band error 已发(chat 的 error chunk / responses 的 response.failed),不补正常终止帧。
        } else if !is_responses {
            // chat/completions 正常结束发 [DONE];Responses 靠 response.completed 收尾,不发 [DONE]。
            yield Ok(Bytes::from("data: [DONE]\n\n"));
        }
    }
}

/// 在字节缓冲里找第一个 SSE 事件分隔符,返回 (起始位置, 分隔符长度)。
/// 兼容 `\n\n`(len 2)和 `\r\n\r\n`(len 4)——SSE 规范两者都合法,custom_api 透传的上游
/// 可能用 CRLF 分帧;只认 `\n\n` 会让 CRLF 流永不切帧、整段缓冲直到结束(流式失效+无界内存)。
fn find_sse_boundary(buf: &[u8]) -> Option<(usize, usize)> {
    // 优先找 \r\n\r\n(4字节),再找 \n\n(2字节),取更靠前的。
    let crlf = buf.windows(4).position(|w| w == b"\r\n\r\n");
    let lf = buf.windows(2).position(|w| w == b"\n\n");
    match (crlf, lf) {
        (Some(c), Some(l)) => {
            if c <= l { Some((c, 4)) } else { Some((l, 2)) }
        }
        (Some(c), None) => Some((c, 4)),
        (None, Some(l)) => Some((l, 2)),
        (None, None) => None,
    }
}
