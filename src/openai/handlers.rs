//! OpenAI 兼容入站 handler(/v1/chat/completions)。
//!
//! 复用现有 Anthropic 管线:翻译 OpenAI 请求 → 调 `anthropic::handlers::post_messages`
//! (整条管线自动复用)→ 把返回的 Anthropic SSE(流式)/ Messages JSON(非流式)翻回 OpenAI。

use axum::{
    Json,
    body::{Body, Bytes},
    extract::{ConnectInfo, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use futures::StreamExt;
use serde_json::{Value, json};
use std::net::SocketAddr;

use crate::anthropic::middleware::AppState;
use crate::anthropic::model_catalog;
use crate::openai::convert;
use crate::openai::types::ChatCompletionsPeek;
use uuid::Uuid;

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
        Err(e) => {
            return openai_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                &format!("请求体解析失败: {e}"),
            );
        }
    };
    let peek: ChatCompletionsPeek = match serde_json::from_value(raw.clone()) {
        Ok(p) => p,
        Err(e) => {
            return openai_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                &format!("缺少必填字段: {e}"),
            );
        }
    };

    // model 经 catalog 归一(GPT-5.6 三变体已在表);未识别则原样透传给上游(由上游决定认不认)。
    let resolved_model = model_catalog::resolve_kiro_id(&peek.model)
        .map(|s| s.to_string())
        .unwrap_or_else(|| peek.model.clone());
    // 出站给客户端的 model 名回显客户端请求的原名(OpenAI 惯例)。
    let echo_model = peek.model.clone();

    // 翻译成 Anthropic 请求体。会话 UUID 写入 metadata.user_id（converter 已认
    // `session_<uuid>`），未命中则不下发，conversationId 走现有派生/随机。
    let mut anthropic_req = convert::openai_chat_to_anthropic(&resolved_model, &raw, peek.stream);
    apply_session_metadata(&mut anthropic_req, &raw, &headers);
    let anthropic_bytes = match serde_json::to_vec(&anthropic_req) {
        Ok(b) => Bytes::from(b),
        Err(e) => {
            return openai_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                &format!("请求翻译失败: {e}"),
            );
        }
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
        let include_usage = convert::stream_include_usage(&raw);
        stream_openai_from_anthropic(anthropic_resp, echo_model, include_usage).await
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
        Err(e) => {
            return openai_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                &format!("请求体解析失败: {e}"),
            );
        }
    };
    let model = match raw.get("model").and_then(|v| v.as_str()) {
        Some(m) => m.to_string(),
        None => {
            return openai_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "缺少必填字段 model",
            );
        }
    };
    let stream = raw.get("stream").and_then(|v| v.as_bool()).unwrap_or(false);

    let resolved_model = model_catalog::resolve_kiro_id(&model)
        .map(|s| s.to_string())
        .unwrap_or_else(|| model.clone());
    let echo_model = model.clone();

    let mut anthropic_req = convert::openai_responses_to_anthropic(&resolved_model, &raw, stream);
    apply_session_metadata(&mut anthropic_req, &raw, &headers);
    let anthropic_bytes = match serde_json::to_vec(&anthropic_req) {
        Ok(b) => Bytes::from(b),
        Err(e) => {
            return openai_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                &format!("请求翻译失败: {e}"),
            );
        }
    };

    tracing::info!(model = %model, resolved = %resolved_model, stream = %stream, "Received POST /v1/responses request");

    let anthropic_resp = crate::anthropic::handlers::post_messages(
        State(state),
        ConnectInfo(peer),
        headers,
        anthropic_bytes,
    )
    .await;

    if !anthropic_resp.status().is_success() {
        return translate_error_response(anthropic_resp).await;
    }

    if stream {
        stream_responses_from_anthropic(anthropic_resp, echo_model).await
    } else {
        nonstream_responses_from_anthropic(anthropic_resp, echo_model).await
    }
}

/// OpenAI 会话键 → Anthropic `metadata.user_id`（`session_<uuid>`）。
///
/// 顺序：JSON `prompt_cache_key` → 头 `x-session-affinity` →
/// `x-client-request-id` → JSON `session_id`。非法值跳过；全未命中则不下发
/// （Kiro conversationId 保持 converter 派生/随机）。不改 converter 哈希。
fn apply_session_metadata(anthropic_req: &mut Value, raw: &Value, headers: &HeaderMap) {
    if let Some(user_id) = resolve_session_user_id(raw, headers) {
        anthropic_req["metadata"] = json!({"user_id": user_id});
    }
}

fn resolve_session_user_id(raw: &Value, headers: &HeaderMap) -> Option<String> {
    let json_str = |key: &str| raw.get(key).and_then(Value::as_str);
    let header_str = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
    [
        json_str("prompt_cache_key"),
        header_str("x-session-affinity"),
        header_str("x-client-request-id"),
        json_str("session_id"),
    ]
    .into_iter()
    .flatten()
    .find_map(parse_session_uuid)
}

fn parse_session_uuid(candidate: &str) -> Option<String> {
    let candidate = candidate.trim();
    let raw_uuid = candidate.strip_prefix("session_").unwrap_or(candidate);
    let uuid = Uuid::parse_str(raw_uuid.trim()).ok()?;
    Some(format!("session_{uuid}"))
}

/// 流式:Anthropic SSE → Responses SSE 事件序列(每事件 `event: T\ndata: {..}\n\n`)。
async fn stream_responses_from_anthropic(resp: Response, model: String) -> Response {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    let body = resp.into_body();
    let mut conv = convert::ResponsesStreamConverter::new(model);
    let error_seen = Arc::new(AtomicBool::new(false));
    let error_seen_cb = error_seen.clone();

    let out_stream = async_stream_from_body(body, error_seen, true, move |line, sink| {
        let payload = match line.strip_prefix("data:") {
            Some(p) => p.trim(),
            None => return,
        };
        if payload.is_empty() {
            return;
        }
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
        Err(e) => {
            return openai_error(
                StatusCode::BAD_GATEWAY,
                "api_error",
                &format!("读取上游响应失败: {e}"),
            );
        }
    };
    let text = String::from_utf8_lossy(&bytes);
    let events = parse_sse_or_message(&text);
    let response = convert::aggregate_responses(&model, &events);
    (StatusCode::OK, Json(response)).into_response()
}

/// 流式:把 Anthropic SSE body 逐帧翻成 OpenAI chat.completion.chunk SSE。
async fn stream_openai_from_anthropic(
    resp: Response,
    model: String,
    include_usage: Option<bool>,
) -> Response {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    let body = resp.into_body();
    let mut conv = convert::ChatStreamConverter::new(model).with_include_usage(include_usage);
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
        Err(e) => {
            return openai_error(
                StatusCode::BAD_GATEWAY,
                "api_error",
                &format!("读取上游响应失败: {e}"),
            );
        }
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
    let stop_reason = msg
        .get("stop_reason")
        .and_then(|v| v.as_str())
        .unwrap_or("end_turn");
    let mut delta = json!({"type": "message_delta", "delta": {"stop_reason": stop_reason}});
    if let Some(u) = msg.get("usage") {
        delta["usage"] = u.clone();
    }
    events.push(delta);
    events
}

/// 把 Anthropic 错误响应翻成 OpenAI 错误结构。
///
/// ⭐ **必须保留 `Retry-After`**：内层 `anthropic::handlers::map_provider_error` 已经为
/// 全池冷却 / 上游 429 / 403 临时风控 / 上游 5xx / 吸收层耗尽这五类各自算好了退避秒数
/// 并挂在响应头上。此前本函数只取 `status` + body 重新构造响应，**整份响应头连同
/// `Retry-After` 一起被丢掉** —— 于是走 OpenAI 协议的客户端（Codex / Cline / Roo /
/// OpenAI SDK）拿到的是「429 但没说等多久」，只能按自己的默认节奏瞎重试，
/// 而 Anthropic 侧同一条链路是**有**这个头的：两条协议路径行为不一致。
///
/// 判据刻意**不在这里另写一套**（不判错误串、不设本地常数）：只透传内层已渲染好的头。
/// 新写一套必然与 `map_provider_error` 的分支顺序漂移，那正是本类缺陷反复出现的成因。
async fn translate_error_response(resp: Response) -> Response {
    let status = resp.status();
    let retry_after = resp
        .headers()
        .get(header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let bytes = axum::body::to_bytes(resp.into_body(), MAX_RESP_BYTES)
        .await
        .unwrap_or_default();
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
    openai_error_with_retry_after(status, &typ, &msg, retry_after.as_deref())
}

/// 构造 OpenAI 错误响应。
fn openai_error(status: StatusCode, err_type: &str, message: &str) -> Response {
    openai_error_with_retry_after(status, err_type, message, None)
}

/// 构造 OpenAI 错误响应，并可选地带上 `Retry-After`。
///
/// 只有从上游/内层**透传**来的秒数才该走这个入口（见 `translate_error_response`）；
/// 本地构造的 400/500 类错误没有"等多久会好"的语义，仍用 [`openai_error`]。
fn openai_error_with_retry_after(
    status: StatusCode,
    err_type: &str,
    message: &str,
    retry_after: Option<&str>,
) -> Response {
    let mut resp = (
        status,
        Json(json!({"error": {"message": message, "type": err_type}})),
    )
        .into_response();
    if let Some(secs) = retry_after {
        // 头值来自内层自己渲染的十进制秒数（`u64::to_string()`），正常必然合法；
        // 万一非法则**丢弃该头而不是丢掉整个响应**（客户端拿不到退避提示 ≪ 拿不到响应）。
        match header::HeaderValue::from_str(secs) {
            Ok(v) => {
                resp.headers_mut().insert(header::RETRY_AFTER, v);
            }
            Err(e) => tracing::warn!(
                retry_after = %secs,
                error = %e,
                "内层 Retry-After 头值非法，已丢弃该头（响应仍正常返回）"
            ),
        }
    }
    resp
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
            if c <= l {
                Some((c, 4))
            } else {
                Some((l, 2))
            }
        }
        (Some(c), None) => Some((c, 4)),
        (None, Some(l)) => Some((l, 2)),
        (None, None) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 造一个「内层 Anthropic 出口」的错误响应：状态码 + 可选 Retry-After + Anthropic 错误体。
    fn anthropic_err_response(status: StatusCode, retry_after: Option<&str>) -> Response {
        let body = json!({"type":"error","error":{"type":"rate_limit_error","message":"全池冷却"}});
        let mut b = Response::builder()
            .status(status)
            .header(header::CONTENT_TYPE, "application/json");
        if let Some(v) = retry_after {
            b = b.header(header::RETRY_AFTER, v);
        }
        b.body(Body::from(body.to_string())).unwrap()
    }

    /// 🔴 回归（A7）：OpenAI 错误出口必须透传内层已算好的 `Retry-After`。
    ///
    /// **旧代码为何 FAIL**：`translate_error_response` 只读 `resp.status()` 与 body，
    /// 再用 `openai_error` 重新构造响应 —— 整份响应头（含 `Retry-After`）被丢掉。
    /// 于是 OpenAI 协议客户端（Codex / Cline / Roo / OpenAI SDK）在上游 429 时
    /// 拿不到退避秒数，只能瞎重试；而 Anthropic 侧同一条链路是**有**这个头的。
    /// 旧代码下 `Retry-After` 断言必然 FAIL（头不存在）。
    #[tokio::test]
    async fn error_exit_preserves_retry_after_header() {
        let out = translate_error_response(anthropic_err_response(
            StatusCode::TOO_MANY_REQUESTS,
            Some("17"),
        ))
        .await;

        assert_eq!(
            out.status(),
            StatusCode::TOO_MANY_REQUESTS,
            "状态码必须原样透传"
        );
        assert_eq!(
            out.headers()
                .get(header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok()),
            Some("17"),
            "内层算好的 Retry-After 必须透传给 OpenAI 客户端（丢了它客户端只能瞎重试）"
        );

        // 错误体仍必须是 OpenAI 结构（透头不该把翻译搞坏）。
        let bytes = axum::body::to_bytes(out.into_body(), MAX_RESP_BYTES)
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["error"]["type"], "rate_limit_error");
        assert_eq!(v["error"]["message"], "全池冷却");
    }

    /// 对照组（A7）：内层**没给** `Retry-After` 时不得自己造一个。
    ///
    /// 承重：给不可重试的错误（配额耗尽 429、404 模型不支持）编一个退避秒数，
    /// 等于让客户端做无意义的短退避反复砸号 —— 内层刻意不给头就是不给。
    #[tokio::test]
    async fn error_exit_does_not_fabricate_retry_after() {
        let out =
            translate_error_response(anthropic_err_response(StatusCode::NOT_FOUND, None)).await;
        assert_eq!(out.status(), StatusCode::NOT_FOUND);
        assert!(
            out.headers().get(header::RETRY_AFTER).is_none(),
            "内层未给 Retry-After 时绝不能自造（会把永久态伪装成可重试）"
        );
    }

    /// 本地构造的错误（请求体解析失败等）不带 `Retry-After`。
    ///
    /// 这类错误没有"等多久会好"的语义：带上退避秒数会让客户端把一个必然再失败的
    /// 400 反复重发。
    #[test]
    fn locally_built_errors_carry_no_retry_after() {
        let out = openai_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "缺少必填字段 model",
        );
        assert_eq!(out.status(), StatusCode::BAD_REQUEST);
        assert!(out.headers().get(header::RETRY_AFTER).is_none());
    }

    /// 非法头值只丢头、不丢响应（内层不该给出非法值，但降级方向必须是安全的那边）。
    #[test]
    fn illegal_retry_after_value_is_dropped_not_fatal() {
        let out = openai_error_with_retry_after(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limit_error",
            "x",
            Some("bad\nvalue"),
        );
        assert_eq!(
            out.status(),
            StatusCode::TOO_MANY_REQUESTS,
            "非法头值不得影响响应本身"
        );
        assert!(out.headers().get(header::RETRY_AFTER).is_none());
    }

    /// SSE 帧边界解析：`\n\n` 与 `\r\n\r\n` 都要认，且取更靠前的那个。
    ///
    /// 这条与 A7 无关，是本文件（全仓唯一零测试文件）的首批基础覆盖：
    /// `find_sse_boundary` 只认 `\n\n` 时，CRLF 分帧的上游会永不切帧 →
    /// 流式失效 + 缓冲无界增长（见该函数注释）。
    #[test]
    fn sse_boundary_accepts_both_lf_and_crlf() {
        assert_eq!(find_sse_boundary(b"data: 1\n\nrest"), Some((7, 2)));
        assert_eq!(find_sse_boundary(b"data: 1\r\n\r\nrest"), Some((7, 4)));
        assert_eq!(find_sse_boundary(b"data: incomplete"), None);
        // CRLF 里含 \n\n？不含 —— 但 `\n\r\n` 这类混合下必须取更靠前的边界。
        let mixed = b"a\r\n\r\nb\n\nc";
        assert_eq!(find_sse_boundary(mixed), Some((1, 4)));
    }

    /// 非 SSE 的整份 Anthropic Messages JSON 也要能被解析成事件序列。
    /// （非流式内部路径确实可能直接返回 Messages JSON，见 `parse_sse_or_message` 注释。）
    #[test]
    fn parse_sse_or_message_handles_plain_message_json() {
        let msg = json!({
            "type": "message",
            "content": [{"type": "text", "text": "hi"}],
            "stop_reason": "end_turn"
        })
        .to_string();
        let events = parse_sse_or_message(&msg);
        assert_eq!(
            events.first().and_then(|e| e["type"].as_str()),
            Some("message_start")
        );
        assert!(
            events
                .iter()
                .any(|e| e["delta"]["text"].as_str() == Some("hi")),
            "text 块必须被合成成 text_delta，否则非流式聚合拿不到正文"
        );
        assert_eq!(
            events
                .last()
                .and_then(|e| e["delta"]["stop_reason"].as_str()),
            Some("end_turn")
        );
    }

    const UUID_A: &str = "550e8400-e29b-41d4-a716-446655440000";
    const UUID_B: &str = "67e55044-10b1-426f-9247-bb680e5fe0c8";
    const UUID_C: &str = "123e4567-e89b-12d3-a456-426614174000";
    const UUID_D: &str = "123e4567-e89b-12d3-a456-426614174001";

    fn session_raw(cache: Option<&str>, session: Option<&str>) -> Value {
        let mut raw = json!({"model": "m", "messages": [{"role": "user", "content": "hi"}]});
        if let Some(c) = cache {
            raw["prompt_cache_key"] = json!(c);
        }
        if let Some(s) = session {
            raw["session_id"] = json!(s);
        }
        raw
    }

    fn session_headers(affinity: Option<&str>, client_req: Option<&str>) -> HeaderMap {
        let mut headers = HeaderMap::new();
        if let Some(v) = affinity {
            headers.insert("x-session-affinity", v.parse().unwrap());
        }
        if let Some(v) = client_req {
            headers.insert("x-client-request-id", v.parse().unwrap());
        }
        headers
    }

    fn user_id(raw: &Value, headers: &HeaderMap) -> Option<String> {
        resolve_session_user_id(raw, headers)
    }

    /// P2-7: JSON `prompt_cache_key` 优先于头与 JSON `session_id`。
    #[test]
    fn session_affinity_prompt_cache_key_wins() {
        let raw = session_raw(Some(UUID_A), Some(UUID_D));
        let headers = session_headers(Some(UUID_B), Some(UUID_C));
        assert_eq!(
            user_id(&raw, &headers).as_deref(),
            Some("session_550e8400-e29b-41d4-a716-446655440000")
        );
    }

    /// P2-7: `prompt_cache_key` 非法时头 `x-session-affinity` 胜出。
    #[test]
    fn session_affinity_x_session_affinity_wins() {
        let raw = session_raw(Some("not-a-uuid"), Some(UUID_D));
        let headers = session_headers(Some(UUID_B), Some(UUID_C));
        assert_eq!(
            user_id(&raw, &headers).as_deref(),
            Some("session_67e55044-10b1-426f-9247-bb680e5fe0c8")
        );
    }

    /// P2-7: 亲和头非法时 `x-client-request-id` 胜出。
    #[test]
    fn session_affinity_x_client_request_id_wins() {
        let raw = session_raw(Some("garbage"), Some(UUID_D));
        let headers = session_headers(Some("also-garbage"), Some(UUID_C));
        assert_eq!(
            user_id(&raw, &headers).as_deref(),
            Some("session_123e4567-e89b-12d3-a456-426614174000")
        );
    }

    /// P2-7: 头全非法/缺失时 JSON `session_id` 胜出。
    #[test]
    fn session_affinity_json_session_id_wins() {
        let raw = session_raw(Some("nope"), Some(UUID_D));
        let headers = session_headers(Some("nope"), Some("nope"));
        assert_eq!(
            user_id(&raw, &headers).as_deref(),
            Some("session_123e4567-e89b-12d3-a456-426614174001")
        );
    }

    /// P2-7: 非法值跳过；四来源全空或全垃圾 → 不下发（现有派生/随机）。
    #[test]
    fn session_affinity_garbage_ignored() {
        let raw = session_raw(Some("not-a-uuid"), Some("session_not-a-uuid"));
        let mut headers = session_headers(Some("also-bad"), Some("still-bad"));
        headers.insert(
            "x-session-affinity",
            axum::http::HeaderValue::from_bytes(&[0xff]).unwrap(),
        );
        assert!(user_id(&raw, &headers).is_none());
        assert!(user_id(&session_raw(None, None), &HeaderMap::new()).is_none());
    }

    /// `session_<uuid>` 与大写 UUID 归一成 converter 已认的 `session_<小写 uuid>`。
    #[test]
    fn session_affinity_normalizes_session_prefix_and_case() {
        let raw = session_raw(Some("session_550E8400-E29B-41D4-A716-446655440000"), None);
        assert_eq!(
            user_id(&raw, &HeaderMap::new()).as_deref(),
            Some("session_550e8400-e29b-41d4-a716-446655440000")
        );
    }

    /// 命中时写入翻译后 Anthropic body 的 `metadata.user_id`；未命中不造 metadata。
    #[test]
    fn session_affinity_sets_metadata_on_translated_body() {
        let raw = session_raw(Some(UUID_A), None);
        let mut anth = convert::openai_chat_to_anthropic("m", &raw, false);
        assert!(
            anth.get("metadata").is_none(),
            "convert 本身不得写 metadata（亲和只在 handler 注入）"
        );
        apply_session_metadata(&mut anth, &raw, &HeaderMap::new());
        assert_eq!(
            anth["metadata"]["user_id"].as_str(),
            Some("session_550e8400-e29b-41d4-a716-446655440000")
        );

        let raw2 = session_raw(None, None);
        let mut anth2 = convert::openai_chat_to_anthropic("m", &raw2, false);
        apply_session_metadata(&mut anth2, &raw2, &HeaderMap::new());
        assert!(
            anth2.get("metadata").is_none(),
            "无会话键不得下发 metadata，conversationId 走派生/随机"
        );

        let raw3 = json!({
            "model": "m",
            "input": "hi",
            "prompt_cache_key": UUID_B
        });
        let mut anth3 = convert::openai_responses_to_anthropic("m", &raw3, false);
        apply_session_metadata(&mut anth3, &raw3, &HeaderMap::new());
        assert_eq!(
            anth3["metadata"]["user_id"].as_str(),
            Some("session_67e55044-10b1-426f-9247-bb680e5fe0c8")
        );
    }

    /// chat 与 responses 两个入口都必须注入会话 metadata（防一侧漏接）。
    #[test]
    fn session_affinity_both_openai_handlers_apply() {
        let src = include_str!("handlers.rs");
        let tests_start = src.find("mod tests").unwrap_or(src.len());
        let impl_region = &src[..tests_start];
        let needle = format!("apply_session{}", "_metadata(");
        assert_eq!(
            impl_region.matches(&needle).count(),
            3,
            "定义 1 次 + post_chat_completions / post_responses 各调 1 次；当前 {} 次",
            impl_region.matches(&needle).count()
        );
    }
}
