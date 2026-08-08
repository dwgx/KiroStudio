//! passthrough 响应侧的 thinking 过滤（deepseek 归一化专用）。
//!
//! 设计约束：passthrough 的卖点是「字节流原样透传」、零协议转换（隔离铁律 3）。
//! 本过滤器**只在 `deepseek_normalize == Some(true)` 时启用** —— 因为 deepseek 类中转站
//! 在 thinking disabled 时仍可能吐 thinking 块，客户端（Claude Code）看到 thinking 会报
//! "Tool result missing"。其余 custom_api 凭据的响应仍原样回流，一行不解析。
//!
//! 两条路径：
//! - 流式（`text/event-stream`）：[`filter_sse_stream`] 逐事件解析 Anthropic SSE，
//!   状态机跟踪 thinking 块并丢弃其 start/delta/stop。
//! - 非流式（`application/json`）：[`filter_json_bytes`] 解析 content 数组，滤掉
//!   `type == "thinking"` 的块。
//!
//! 任何解析失败都 **fail-open 原样透传**（不中断流 / 不破坏响应）—— 过滤是增值优化，
//! 永远不能让一个原本能用的响应因解析器出错而变坏。

use std::collections::HashMap;
use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use futures::{Stream, StreamExt};

/// 流式 SSE thinking 过滤器：把上游 `Bytes` 事件流逐事件过滤，产出过滤后的 `Bytes`。
///
/// 内层 stream 的 `Err` 原样透传为 `axum::Error`（与 passthrough 既有的错误传播语义一致，
/// 客户端可据此判失败重试）。流结束后 flush 未闭合尾部（fail-open 原样透传）。
pub fn filter_sse_stream<S, E>(inner: S) -> impl Stream<Item = Result<Bytes, axum::Error>>
where
    S: Stream<Item = Result<Bytes, E>> + Send + 'static,
    E: std::error::Error + Send + Sync + 'static,
{
    SseThinkFilter {
        inner: Box::pin(inner),
        state: SseFilterState::new(),
    }
}

/// 过滤非流式 JSON 响应里的 thinking content blocks（fail-open：解析失败原样返回）。
pub fn filter_json_bytes(bytes: &[u8]) -> Bytes {
    let Ok(mut v) = serde_json::from_slice::<serde_json::Value>(bytes) else {
        return Bytes::copy_from_slice(bytes);
    };
    let Some(content) = v.get_mut("content").and_then(|c| c.as_array_mut()) else {
        return Bytes::copy_from_slice(bytes);
    };
    content.retain(|block| block.get("type").and_then(|t| t.as_str()) != Some("thinking"));
    serde_json::to_vec(&v)
        .map(Bytes::from)
        .unwrap_or_else(|_| Bytes::copy_from_slice(bytes))
}

/// 自定义 Stream：持有内层流 + SSE 过滤状态。
struct SseThinkFilter<S> {
    inner: Pin<Box<S>>,
    state: SseFilterState,
}

impl<S, E> Stream for SseThinkFilter<S>
where
    S: Stream<Item = Result<Bytes, E>>,
    E: std::error::Error + Send + Sync + 'static,
{
    type Item = Result<Bytes, axum::Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        // 优先吐已缓存的过滤结果。
        if let Some(out) = self.state.pop_pending() {
            return Poll::Ready(Some(Ok(out)));
        }
        loop {
            match self.inner.poll_next_unpin(cx) {
                Poll::Ready(Some(Ok(chunk))) => {
                    self.state.push(&chunk);
                    if let Some(out) = self.state.pop_pending() {
                        return Poll::Ready(Some(Ok(out)));
                    }
                    // 该 chunk 全被过滤 / 事件未闭合，继续 poll。
                }
                Poll::Ready(Some(Err(e))) => {
                    return Poll::Ready(Some(Err(axum::Error::new(e))));
                }
                Poll::Ready(None) => {
                    // 上游结束：flush 未闭合尾部（fail-open 原样透传，绝不丢）。
                    let tail = self.state.finish();
                    if !tail.is_empty() {
                        return Poll::Ready(Some(Ok(tail)));
                    }
                    return Poll::Ready(None);
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

/// SSE 事件切分 + thinking 过滤状态。
struct SseFilterState {
    /// 未闭合字节缓冲（等待事件结束空行）。
    buf: Vec<u8>,
    /// 已切出完整事件但尚未吐出的过滤结果。
    pending: Vec<Bytes>,
    /// 当前是否处于 thinking 块内（按 content_block 的 index）。
    in_thinking: HashMap<usize, bool>,
}

impl SseFilterState {
    fn new() -> Self {
        Self {
            buf: Vec::with_capacity(1024),
            pending: Vec::new(),
            in_thinking: HashMap::new(),
        }
    }

    /// 追加一批字节，切出完整 SSE 事件逐条过滤，保留的事件放入 `pending`（FIFO）。
    fn push(&mut self, chunk: &[u8]) {
        self.buf.extend_from_slice(chunk);
        // 反复切出「事件结束空行」之前的完整块，直到 buf 尾部只剩未闭合事件。
        while let Some(sep) = find_event_separator(&self.buf) {
            // 事件块 = sep 之前的内容（含 event:/data: 行），sep 本身（空行）作为分隔保留在输出。
            let block: Vec<u8> = self.buf.drain(..sep).collect();
            let sep_bytes = drain_separator(&mut self.buf);
            if self.should_keep(&block) {
                let mut event = block;
                event.extend_from_slice(&sep_bytes);
                self.pending.push(Bytes::from(event));
            }
        }
    }

    /// 取出下一条待吐结果（FIFO，保持事件顺序）。
    fn pop_pending(&mut self) -> Option<Bytes> {
        if self.pending.is_empty() {
            None
        } else {
            Some(self.pending.remove(0))
        }
    }

    /// 上游结束：flush 未闭合尾部（fail-open 原样透传）。
    fn finish(&mut self) -> Bytes {
        if self.buf.is_empty() {
            return Bytes::new();
        }
        let tail = std::mem::take(&mut self.buf);
        Bytes::from(tail)
    }

    /// 判断一个完整 SSE 事件块是否保留。
    ///
    /// 丢弃规则：
    /// - `content_block_start` 且 `content_block.type == "thinking"` → 记录该 index 在 thinking 内，丢弃；
    /// - `content_block_start` 其它 type → 该 index 不在 thinking 内，透传；
    /// - `content_block_delta` 且 `delta.type` ∈ {thinking_delta, signature_delta} 且该 index 在 thinking 内 → 丢弃；
    /// - `content_block_stop` 且该 index 在 thinking 内 → 丢弃（并清除该 index 状态）；
    /// - 其余（message_start/delta/stop、ping、error、未知）→ 透传。
    ///
    /// 解析失败（非 JSON data、缺 event 行）→ fail-open 透传。
    fn should_keep(&mut self, block: &[u8]) -> bool {
        let text = String::from_utf8_lossy(block);
        let mut event_type = "";
        let mut data = "";
        for line in text.lines() {
            if let Some(rest) = line.strip_prefix("event:") {
                event_type = rest.trim();
            } else if let Some(rest) = line.strip_prefix("data:") {
                data = rest.trim();
            }
        }
        if event_type.is_empty() || data.is_empty() {
            return true; // 不认识的形态，原样透传
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(data) else {
            return true; // data 不是 JSON，fail-open
        };
        match event_type {
            "content_block_start" => {
                let is_thinking = v
                    .get("content_block")
                    .and_then(|cb| cb.get("type"))
                    .and_then(|t| t.as_str())
                    == Some("thinking");
                let idx = v.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as usize;
                self.in_thinking.insert(idx, is_thinking);
                !is_thinking
            }
            "content_block_delta" => {
                let delta_type = v
                    .get("delta")
                    .and_then(|d| d.get("type"))
                    .and_then(|t| t.as_str());
                let idx = v.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as usize;
                let in_thinking = self.in_thinking.get(&idx).copied().unwrap_or(false);
                let is_thinking_delta =
                    matches!(delta_type, Some("thinking_delta") | Some("signature_delta"));
                !(in_thinking && is_thinking_delta)
            }
            "content_block_stop" => {
                let idx = v.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as usize;
                let was_thinking = self.in_thinking.remove(&idx).unwrap_or(false);
                !was_thinking
            }
            _ => true,
        }
    }
}

/// 在 buf 中找事件分隔符（`\n\n` 或 `\r\n\r\n`）的**起始**位置；找不到返 None。
fn find_event_separator(buf: &[u8]) -> Option<usize> {
    buf.windows(2)
        .position(|w| w == b"\n\n")
        .or_else(|| buf.windows(4).position(|w| w == b"\r\n\r\n"))
}

/// 把事件分隔符从 buf 里取出来（含完整空行，供输出时保留）。
fn drain_separator(buf: &mut Vec<u8>) -> Vec<u8> {
    if buf.starts_with(b"\r\n\r\n") {
        buf.drain(..4).collect()
    } else if buf.starts_with(b"\n\n") {
        buf.drain(..2).collect()
    } else {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(ev: &str, data: &str) -> String {
        format!("event: {ev}\ndata: {data}\n\n")
    }

    /// 把过滤后的流拼接成一个字符串（多 chunk 合并）。
    async fn collect_filtered(stream: impl Stream<Item = Result<Bytes, axum::Error>>) -> String {
        stream
            .filter_map(|r| async move { r.ok() })
            .collect::<Vec<_>>()
            .await
            .iter()
            .map(|b| String::from_utf8_lossy(b).into_owned())
            .collect()
    }

    /// 端到端：SSE 流里 thinking 块全滤，text 块透传，其它事件透传。
    #[tokio::test]
    async fn test_filter_sse_stream_removes_thinking_block() {
        let input = format!(
            "{}{}{}{}{}{}{}",
            event("message_start", r#"{"type":"message_start"}"#),
            event(
                "content_block_start",
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#
            ),
            event(
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"Let me think"}}"#
            ),
            event(
                "content_block_stop",
                r#"{"type":"content_block_stop","index":0}"#
            ),
            event(
                "content_block_start",
                r#"{"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}"#
            ),
            event(
                "content_block_delta",
                r#"{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"Hello"}}"#
            ),
            event("message_stop", r#"{"type":"message_stop"}"#),
        );
        let stream = futures::stream::iter(vec![Ok::<Bytes, std::io::Error>(Bytes::from(input))]);
        let joined = collect_filtered(filter_sse_stream(stream)).await;
        assert!(!joined.contains("thinking_delta"), "thinking delta 必须被滤掉");
        assert!(!joined.contains("\"type\":\"thinking\""), "thinking start 必须被滤掉");
        assert!(joined.contains("text_delta"), "text 块必须透传");
        assert!(joined.contains("\"type\":\"text\""), "text start 必须透传");
        assert!(joined.contains("message_start"), "message_start 必须透传");
        assert!(joined.contains("message_stop"), "message_stop 必须透传");
    }

    /// 交错块：thinking 与 text 交替，index 状态不能串。
    #[tokio::test]
    async fn test_filter_sse_stream_interleaved_indices() {
        let input = format!(
            "{}{}{}{}{}{}{}{}",
            event(
                "content_block_start",
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#
            ),
            event(
                "content_block_start",
                r#"{"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}"#
            ),
            event(
                "content_block_delta",
                r#"{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"A"}}"#
            ),
            event(
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"X"}}"#
            ),
            event(
                "content_block_stop",
                r#"{"type":"content_block_stop","index":1}"#
            ),
            event(
                "content_block_stop",
                r#"{"type":"content_block_stop","index":0}"#
            ),
            event(
                "content_block_start",
                r#"{"type":"content_block_start","index":2,"content_block":{"type":"text","text":""}}"#
            ),
            event(
                "content_block_delta",
                r#"{"type":"content_block_delta","index":2,"delta":{"type":"text_delta","text":"B"}}"#
            ),
        );
        let stream = futures::stream::iter(vec![Ok::<Bytes, std::io::Error>(Bytes::from(input))]);
        let filtered = collect_filtered(filter_sse_stream(stream)).await;
        assert!(!filtered.contains("thinking_delta"), "thinking delta 必须滤掉");
        assert!(filtered.contains("text_delta"), "text 块必须保留");
        assert!(filtered.contains("\"text\":\"A\""), "text A 保留");
        assert!(filtered.contains("\"text\":\"B\""), "text B 保留");
    }

    /// chunk 跨界：事件被拆到多个 chunk，仍能正确切分过滤。
    #[tokio::test]
    async fn test_filter_sse_stream_chunk_boundary() {
        let ev = format!(
            "{}{}",
            event(
                "content_block_start",
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#
            ),
            event(
                "content_block_start",
                r#"{"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}"#
            )
        );
        let bytes = Bytes::from(ev);
        let mid = bytes.len() / 2;
        let stream = futures::stream::iter(vec![
            Ok::<Bytes, std::io::Error>(bytes.slice(..mid)),
            Ok(bytes.slice(mid..)),
        ]);
        let filtered = collect_filtered(filter_sse_stream(stream)).await;
        assert!(!filtered.contains("thinking"), "thinking 块必须滤掉（跨 chunk 也应识别）");
        assert!(filtered.contains("text_delta") || filtered.contains("\"type\":\"text\""), "text 块必须保留");
    }

    /// 非流式 JSON：滤掉 thinking 块，text/tool_use 保留。
    #[test]
    fn test_filter_json_bytes_removes_thinking_blocks() {
        let input = serde_json::json!({
            "id": "msg_1",
            "content": [
                {"type": "thinking", "thinking": "let me think"},
                {"type": "text", "text": "Hello"},
                {"type": "tool_use", "id": "t1", "name": "fs_write", "input": {}}
            ],
            "stop_reason": "tool_use"
        });
        let out = filter_json_bytes(input.to_string().as_bytes());
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        let content = v["content"].as_array().unwrap();
        assert_eq!(content.len(), 2, "thinking 块被滤掉，text+tool_use 保留");
        assert!(content.iter().all(|b| b["type"] != "thinking"));
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[1]["type"], "tool_use");
        assert_eq!(v["stop_reason"], "tool_use", "其余字段不受影响");
    }

    /// JSON 解析失败 → fail-open 原样返回。
    #[test]
    fn test_filter_json_bytes_fail_open_on_invalid() {
        let raw = Bytes::from_static(b"not json");
        assert_eq!(filter_json_bytes(&raw), raw);
    }
}
