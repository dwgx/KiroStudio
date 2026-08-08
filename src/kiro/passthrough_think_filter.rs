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

/// 单事件缓冲上限：上游发**无空行**的长流时，`buf` 会在找不到事件分隔符的情况下
/// 无限增长、且流结束前一个字节都不下发（客户端卡死）。超限即视为不可解析的垃圾流，
/// fail-open 整块透传并清空（对齐历史 #14 的孤立 `<` 教训 —— 缓冲必须有界）。
const MAX_EVENT_BUFFER_BYTES: usize = 1 * 1024 * 1024;

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
    filter_sse_stream_with(inner, true)
}

/// [`filter_sse_stream`] 的可配置版本：`strip_inline_thinking` 控制是否剥 text 里的
/// 内联 `<thinking>...</thinking>` 标签（deepseek 可能以文本形式吐 thinking）。
pub fn filter_sse_stream_with<S, E>(
    inner: S,
    strip_inline_thinking: bool,
) -> impl Stream<Item = Result<Bytes, axum::Error>>
where
    S: Stream<Item = Result<Bytes, E>> + Send + 'static,
    E: std::error::Error + Send + Sync + 'static,
{
    SseThinkFilter {
        inner: Box::pin(inner),
        state: SseFilterState::new(strip_inline_thinking),
    }
}

/// 过滤非流式 JSON 响应里的 thinking content blocks（fail-open：解析失败原样返回）。
pub fn filter_json_bytes(bytes: &[u8]) -> Bytes {
    filter_json_bytes_with(bytes, true)
}

/// [`filter_json_bytes`] 的可配置版本：`strip_inline` 控制是否剥 text 块里的
/// 内联 `<thinking>...</thinking>` 标签（与流式 [`filter_sse_stream_with`] 对齐）。
pub fn filter_json_bytes_with(bytes: &[u8], strip_inline: bool) -> Bytes {
    let Ok(mut v) = serde_json::from_slice::<serde_json::Value>(bytes) else {
        return Bytes::copy_from_slice(bytes);
    };
    let Some(content) = v.get_mut("content").and_then(|c| c.as_array_mut()) else {
        return Bytes::copy_from_slice(bytes);
    };
    // ⚠️ 与流式路径的 `in_thinking` 判定一致：`redacted_thinking`（超预算的合法类型）
    // 也必须滤掉，否则非流式请求下客户端同样报 "Tool result missing"。
    content.retain(|block| {
        !matches!(
            block.get("type").and_then(|t| t.as_str()),
            Some("thinking") | Some("redacted_thinking")
        )
    });
    // 剥 text 块里的内联 `<thinking>...</thinking>` 标签（deepseek 可能以文本形式吐）。
    // ⚠️ 受 `strip_inline` 控制，与流式路径一致（配置 false 时不剥）。
    if strip_inline {
        let mut in_inline_thinking = false;
        for block in content.iter_mut() {
            if block.get("type").and_then(|t| t.as_str()) == Some("text") {
                if let Some(text_val) = block.get_mut("text") {
                    if let Some(s) = text_val.as_str() {
                        *text_val = serde_json::Value::String(strip_inline_thinking(
                            s,
                            &mut in_inline_thinking,
                        ));
                    }
                }
            }
        }
    }
    serde_json::to_vec(&v)
        .map(Bytes::from)
        .unwrap_or_else(|_| Bytes::copy_from_slice(bytes))
}

/// 空流兜底：过滤后的流若**没有任何 content 事件**（上游只吐 thinking 被滤光，或上游
/// 真返回空流），流结束时空则补发一个 Anthropic `error` 事件 —— 否则客户端收到
/// "API Error: Stream ended without receiving any events" 而 agentic 循环卡死。
///
/// 对齐主路径（Kiro）的 `empty_response_error_event`：显式 error 替代静默空流。
pub fn guard_empty_stream<S, E>(
    inner: S,
    err_message: &'static str,
) -> impl Stream<Item = Result<Bytes, axum::Error>>
where
    S: Stream<Item = Result<Bytes, E>> + Send + 'static,
    E: std::error::Error + Send + Sync + 'static,
{
    EmptyStreamGuard {
        inner: Box::pin(inner),
        saw_content: false,
        guard_emitted: false,
        err_message,
    }
}

/// 空流守卫：跟踪是否见过 `content_block` 事件（thinking 被滤后，透传的 `content_block_*`
/// 只可能是真实文本/工具调用块；`message_start` 的 `content: []` 不含 `content_block` 子串）。
struct EmptyStreamGuard<S> {
    inner: Pin<Box<S>>,
    saw_content: bool,
    guard_emitted: bool,
    err_message: &'static str,
}

impl<S, E> Stream for EmptyStreamGuard<S>
where
    S: Stream<Item = Result<Bytes, E>>,
    E: std::error::Error + Send + Sync + 'static,
{
    type Item = Result<Bytes, axum::Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match self.inner.poll_next_unpin(cx) {
            Poll::Ready(Some(Ok(chunk))) => {
                const MARKER: &[u8] = b"content_block";
                if !self.saw_content
                    && chunk
                        .windows(MARKER.len())
                        .any(|w| w == MARKER)
                {
                    self.saw_content = true;
                }
                Poll::Ready(Some(Ok(chunk)))
            }
            Poll::Ready(Some(Err(e))) => Poll::Ready(Some(Err(axum::Error::new(e)))),
            Poll::Ready(None) => {
                if !self.saw_content && !self.guard_emitted {
                    self.guard_emitted = true;
                    tracing::warn!(
                        "[透传] 过滤后流无任何 content 事件，补发 error 事件防客户端空流卡死"
                    );
                    let ev = format!(
                        "event: error\ndata: {}\n\n",
                        serde_json::json!({
                            "type": "error",
                            "error": { "type": "api_error", "message": self.err_message }
                        })
                    );
                    Poll::Ready(Some(Ok(Bytes::from(ev))))
                } else {
                    Poll::Ready(None)
                }
            }
            Poll::Pending => Poll::Pending,
        }
    }
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

/// SSE 事件切分 + thinking 过滤 + content_block index 重映射状态。
///
/// 除了滤掉 thinking 块，还要**重编号**保留块的 content_block index：
/// 上游在 thinking disabled 时可能发 `index=0 thinking, index=1 text`，直接滤掉 thinking
/// 会让 text 块从 index=1 开始、客户端 `content[0]` 缺失（Anthropic 协议要求 index 从 0
/// 连续）。故保留的块按出现顺序重新分配 0..N。
struct SseFilterState {
    /// 未闭合字节缓冲（等待事件结束空行）。
    buf: Vec<u8>,
    /// 已切出完整事件但尚未吐出的过滤结果。
    pending: Vec<Bytes>,
    /// 旧 index → 是否 thinking 块（用于丢弃 thinking 的 delta/stop）。
    in_thinking: HashMap<usize, bool>,
    /// 旧 index → 新 index（重编号映射）。
    index_map: HashMap<usize, usize>,
    /// 下一个新 index。
    next_index: usize,
    /// 是否剥 text_delta 里的内联 `<thinking>...</thinking>` 标签。
    strip_inline_thinking: bool,
    /// 跨 chunk 的"正在内联 thinking 内"状态。
    in_inline_thinking: bool,
    /// 滤掉的 thinking 文本累计**字符数**（估算 token = 字符数 / 4）。
    thinking_chars: u64,
}

impl SseFilterState {
    fn new(strip_inline_thinking: bool) -> Self {
        Self {
            buf: Vec::with_capacity(1024),
            pending: Vec::new(),
            in_thinking: HashMap::new(),
            index_map: HashMap::new(),
            next_index: 0,
            strip_inline_thinking,
            in_inline_thinking: false,
            thinking_chars: 0,
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
            if let Some(mut event) = self.process_block(&block) {
                // process_block 统一返回**不含**事件分隔符的内容；这里补回原空行。
                event.extend_from_slice(&sep_bytes);
                self.pending.push(Bytes::from(event));
            }
        }
        // ⚠️ 上限防护：while 循环结束后 buf 仍超限 ⇒ 无空行的长流（找不到分隔符）。
        // 无界增长 + 流结束前零下发 = 客户端卡死。fail-open 整块透传并清空。
        if self.buf.len() > MAX_EVENT_BUFFER_BYTES {
            let overflow = std::mem::take(&mut self.buf);
            self.pending.push(Bytes::from(overflow));
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

    /// 处理一个完整 SSE 事件块，返回 `None` = 丢弃（thinking 块），`Some(bytes)` = 透传
    /// （**不含**事件分隔符，由调用方统一补回）。
    ///
    /// 丢弃规则：
    /// - `content_block_start` 且 `content_block.type == "thinking"` → 记录该 index 在 thinking 内，丢弃；
    /// - `content_block_start` 其它 type → 分配连续新 index，**重写** data 里的 index，透传；
    /// - `content_block_delta` 且 `delta.type` ∈ {thinking_delta, signature_delta} 且该 index 在 thinking 内 → 丢弃；
    /// - `content_block_stop` 且该 index 在 thinking 内 → 丢弃（并清除该 index 状态）；
    /// - 其余（message_start/delta/stop、ping、error、未知）→ 透传。
    ///
    /// 解析失败（非 JSON data、缺 event 行）→ fail-open 原样透传。
    fn process_block(&mut self, block: &[u8]) -> Option<Vec<u8>> {
        let text = String::from_utf8_lossy(block);
        let mut event_type = "";
        let mut data = String::new();
        for line in text.lines() {
            if let Some(rest) = line.strip_prefix("event:") {
                event_type = rest.trim();
            } else if let Some(rest) = line.strip_prefix("data:") {
                // ⚠️ SSE 规范允许多个 `data:` 行拼成一个事件（用换行连接）。
                // 之前只保留最后一行：多行 data 的 JSON 会解析失败 → fail-open 泄漏
                // thinking；更坏的情况是末行恰好是合法 JSON 片段 → 用残缺 data 重写事件。
                if !data.is_empty() {
                    data.push('\n');
                }
                data.push_str(rest.trim());
            }
        }
        if event_type.is_empty() || data.is_empty() {
            return Some(block.to_vec());
        }
        let Ok(mut v) = serde_json::from_str::<serde_json::Value>(&data) else {
            return Some(block.to_vec());
        };
        let old_idx = v.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as usize;
        match event_type {
            "content_block_start" => {
                // ⚠️ `redacted_thinking`（thinking 超预算时上游发的合法类型）同样要滤：
                // 只认 `thinking` 会把它当保留块重编号透传，客户端同样报 "Tool result missing"。
                let is_thinking = matches!(
                    v.get("content_block")
                        .and_then(|cb| cb.get("type"))
                        .and_then(|t| t.as_str()),
                    Some("thinking") | Some("redacted_thinking")
                );
                self.in_thinking.insert(old_idx, is_thinking);
                if is_thinking {
                    return None;
                }
                // 保留块：分配连续新 index，重写 data。
                // ⚠️ 上游 data 可能是非 object 顶层（`[1,2]`/`"x"`/`5`）——裸 `v["index"] =`
                // 在 serde_json 里对非 Object 直接 panic（连接任务死、客户端拿到截断 200）。
                // 用 `as_object_mut` 守卫，非 object 走 fail-open 原样透传。
                let new_idx = self.next_index;
                self.next_index += 1;
                self.index_map.insert(old_idx, new_idx);
                let Some(o) = v.as_object_mut() else {
                    return Some(block.to_vec());
                };
                o.insert("index".to_string(), serde_json::json!(new_idx));
                Some(Self::rewrite_event(event_type, &v))
            }
            "content_block_delta" => {
                // ⚠️ 丢弃条件按 `in_thinking` 单独判定，不看 delta_type：
                // 被滤 thinking 块内的**所有** delta（含 thinking_delta、signature_delta，
                // 以及偶发的 text_delta）一律不下发 —— 若只滤 thinking_delta，thinking 块内
                // 混入的 text_delta 会因 index_map 无该旧 index 而带悬空旧 index 透传，
                // 客户端收到"孤儿 delta"（start 已被丢）甚至与新重编号块同 index 混淆。
                let in_thinking = self.in_thinking.get(&old_idx).copied().unwrap_or(false);
                if in_thinking {
                    // 累计滤掉的 thinking 文本字符（供 message_delta 扣减 usage.output_tokens）。
                    if let Some(t) = v
                        .get("delta")
                        .and_then(|d| d.get("thinking"))
                        .and_then(|x| x.as_str())
                    {
                        self.thinking_chars += t.chars().count() as u64;
                    }
                    return None;
                }
                // 非 thinking 块：剥 text_delta 里的内联 `<thinking>...</thinking>`。
                if self.strip_inline_thinking {
                    if let Some(delta) = v.get_mut("delta").and_then(|d| d.as_object_mut()) {
                        if delta.get("type").and_then(|x| x.as_str()) == Some("text_delta") {
                            if let Some(text_val) = delta.get_mut("text") {
                                if let Some(s) = text_val.as_str() {
                                    let stripped =
                                        strip_inline_thinking(s, &mut self.in_inline_thinking);
                                    *text_val = serde_json::Value::String(stripped);
                                    // 剥光（整段在 thinking 内）→ 丢弃该 delta，避免空 text。
                                    if text_val.as_str().is_some_and(|t| t.is_empty())
                                        && self.in_inline_thinking
                                    {
                                        return None;
                                    }
                                }
                            }
                        }
                    }
                }
                self.rewrite_index_or_passthrough(&mut v, event_type, block, old_idx)
            }
            "message_delta" => {
                // 滤掉 thinking 后，usage.output_tokens 仍含 thinking token → 扣减估算值
                //（字符数/4，Claude Code 约 4 字符/token）。message_start 的 usage 不动
                //（record 口径绑定）。
                //
                // ⚠️ **累计口径**：Anthropic 的 message_delta.usage.output_tokens 是**累计值**
                //（每条都是"到该点为止的总输出"，客户端读最后一条）。每条都含被滤掉的
                // thinking token ⇒ **每个 message_delta 都扣同一份**估算值（不是只扣第一条）。
                // 旧 `deducted_chars` 防重复逻辑假设"增量 usage"——累计口径下客户端读最后
                // 一条会丢扣减（最后一条没扣）。
                if self.thinking_chars > 0 {
                    if let Some(usage) = v.get_mut("usage").and_then(|u| u.as_object_mut()) {
                        if let Some(out) = usage.get("output_tokens").and_then(|x| x.as_u64()) {
                            let deduct = (self.thinking_chars / 4) as u64;
                            usage["output_tokens"] =
                                serde_json::json!(out.saturating_sub(deduct));
                        }
                    }
                }
                Some(Self::rewrite_event(event_type, &v))
            }
            "content_block_stop" => {
                let was_thinking = self.in_thinking.remove(&old_idx).unwrap_or(false);
                if was_thinking {
                    return None;
                }
                self.rewrite_index_or_passthrough(&mut v, event_type, block, old_idx)
            }
            _ => Some(block.to_vec()),
        }
    }

    /// delta/stop 透传时若该 index 有重映射则重写，否则原样。
    fn rewrite_index_or_passthrough(
        &self,
        v: &mut serde_json::Value,
        event_type: &str,
        block: &[u8],
        old_idx: usize,
    ) -> Option<Vec<u8>> {
        match self.index_map.get(&old_idx) {
            Some(&new_idx) => {
                // ⚠️ 同 L347：非 object 顶层 data 不可裸索引赋值（serde_json panic）。
                let Some(o) = v.as_object_mut() else {
                    return Some(block.to_vec());
                };
                o.insert("index".to_string(), serde_json::json!(new_idx));
                Some(Self::rewrite_event(event_type, v))
            }
            None => Some(block.to_vec()), // 未映射（理论不该发生），fail-open 原样
        }
    }

    /// 把重写后的 data 重组成 SSE 事件（`event:` + `data:`，**不含**结尾空行）。
    fn rewrite_event(event_type: &str, data: &serde_json::Value) -> Vec<u8> {
        format!("event: {event_type}\ndata: {data}").into_bytes()
    }
}

/// 剥掉文本里的内联 `<thinking>...</thinking>` 标签（含内容），跨 chunk 状态由 `in_thinking`
/// 携带。deepseek 可能以**文本**形式吐 thinking（非结构化块），对齐主路径
/// `strip_inline_thinking_when_disabled` 的口径 —— 客户端没要就不给。
fn strip_inline_thinking(text: &str, in_thinking: &mut bool) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    loop {
        if *in_thinking {
            match rest.find("</thinking>") {
                Some(i) => {
                    // 剥到 </thinking> 之后，退出 thinking 态，继续处理剩余（可能又有新标签）。
                    rest = &rest[i + "</thinking>".len()..];
                    *in_thinking = false;
                }
                None => break, // 整段在 thinking 内，剥光
            }
        } else {
            match rest.find("<thinking>") {
                Some(i) => {
                    out.push_str(&rest[..i]);
                    rest = &rest[i + "<thinking>".len()..];
                    *in_thinking = true;
                }
                None => {
                    out.push_str(rest);
                    break;
                }
            }
        }
    }
    out
}

/// 在 buf 中找事件分隔符（`\n\n` 或 `\r\n\r\n`）的**起始**位置；找不到返 None。
///
/// ⚠️ 取两者的 **min**：旧实现 `\n\n` 全 buf 首命中、`or_else` 仅无 `\n\n` 才试 `\r\n\r\n`，
/// 混合行尾流（先出现 `\r\n\r\n`、后面 data 里含 `\n\n`）会切在后面的 `\n\n` 处把事件
/// 拦腰截断粘连（两组 event 拼一起 → 解析失败 → fail-open 整块透传 → thinking 泄漏）。
fn find_event_separator(buf: &[u8]) -> Option<usize> {
    let lf = buf.windows(2).position(|w| w == b"\n\n");
    let crlf = buf.windows(4).position(|w| w == b"\r\n\r\n");
    match (lf, crlf) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
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

    /// 从过滤后的 SSE 文本提取每个 `data:` 行的 content_block `index`（按出现顺序）。
    /// 用于断言 index 重编号，规避 serde_json 重排 key 导致的子串顺序问题。
    fn extract_indices(s: &str) -> Vec<u64> {
        s.lines()
            .filter_map(|l| l.strip_prefix("data: "))
            .filter_map(|d| serde_json::from_str::<serde_json::Value>(d).ok())
            .filter_map(|v| v.get("index").and_then(|i| i.as_u64()))
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
        // ⭐ index 重映射：thinking(0) 被滤后，text 块从 index=1 重编号为 0。
        let indices = extract_indices(&joined);
        assert!(!indices.is_empty(), "保留的块应带 index");
        assert!(
            indices.iter().all(|&i| i == 0),
            "滤掉 thinking(0) 后所有保留块 index 应重编号为 0，实际 {indices:?}"
        );
    }

    /// 🔴 回归：上游发非 object 顶层 data（数组/字符串/数字）时**不得 panic**，
    /// fail-open 原样透传（旧代码裸 `v["index"] =` 在 serde_json 对非 Object 直接 panic）。
    #[tokio::test]
    async fn test_filter_sse_stream_non_object_top_level_data_no_panic() {
        let input = format!(
            "{}{}",
            event("message_start", r#"{"type":"message_start"}"#),
            // content_block_start + 数组 data（畸形但合法 JSON）
            event("content_block_start", "[1, 2, 3]"),
        );
        let stream = futures::stream::iter(vec![Ok::<Bytes, std::io::Error>(Bytes::from(input))]);
        let filtered = collect_filtered(filter_sse_stream(stream)).await;
        // 不 panic 且 fail-open 透传（数组块原样出现，而非被静默吞掉）
        assert!(
            filtered.contains("[1, 2, 3]"),
            "非 object data 必须 fail-open 透传而非 panic，实际: {filtered}"
        );
    }

    /// 混合行尾：`\r\n\r\n` 在前、`\n\n` 在后时，分隔符取 **min**（切在前者），
    /// 不粘连事件。旧实现 `\n\n` 首命中会切到后面、把两组 event 拼一起。
    #[test]
    fn test_find_event_separator_mixed_line_endings_takes_min() {
        // E1 用 CRLF 分隔，后面 data 里含 \n\n
        let buf = b"event: a\r\ndata: {\"x\":1}\r\n\r\nevent: b\ndata: {\"y\":2}\n\n";
        let sep = find_event_separator(buf).unwrap();
        // 首个 \r\n\r\n 的位置（=CRLF 事件的结束），应取它而不是后面的 \n\n
        let crlf_pos = buf
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .unwrap();
        assert_eq!(sep, crlf_pos, "混合行尾应取先出现的分隔符（min），实际切到 {sep}");
    }

    /// 🔴 回归：无空行的超长流触发 buf 上限 → fail-open 整块透传，不无界增长/卡死。
    #[tokio::test]
    async fn test_filter_sse_stream_buf_cap_fails_open() {
        // 构造一个超过 MAX_EVENT_BUFFER_BYTES 且不含事件分隔符的大块
        let big = vec![b'a'; MAX_EVENT_BUFFER_BYTES + 10];
        let stream =
            futures::stream::iter(vec![Ok::<Bytes, std::io::Error>(Bytes::from(big.clone()))]);
        let filtered = collect_filtered(filter_sse_stream(stream)).await;
        assert_eq!(
            filtered.len(),
            big.len(),
            "超限 buf 应 fail-open 整块透传（长度不变），而非静默丢弃"
        );
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
        // index 重映射：thinking(0) 被滤，text(1)→0、text(2)→1，无空洞。
        // 事件顺序：textA start(0) delta(0) stop(0) textB start(1) delta(1)。
        let indices = extract_indices(&filtered);
        assert_eq!(
            indices,
            vec![0, 0, 0, 1, 1],
            "text 块应重编号为连续 0,1（消除 thinking 造成的空洞），实际 {indices:?}"
        );
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

    /// 🔴 回归：被滤 thinking 块内混入 `text_delta`（非 thinking_delta）也必须丢弃，
    /// 否则会带悬空旧 index 透传成"孤儿 delta"（start 已被滤，index_map 无该旧 index）。
    #[tokio::test]
    async fn test_filter_sse_stream_drops_text_delta_inside_thinking() {
        let input = format!(
            "{}{}{}{}{}",
            event(
                "content_block_start",
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#
            ),
            event(
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"leak"}}"#
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
        let filtered = collect_filtered(filter_sse_stream(stream)).await;
        assert!(
            !filtered.contains("leak"),
            "thinking 块内的 text_delta 必须丢弃，不得泄漏（旧 index 悬空会混淆客户端）"
        );
        assert!(filtered.contains("Hello"), "真实 text 块必须保留");
        // 重编号后唯一 text 块应为 index 0。
        let indices = extract_indices(&filtered);
        assert!(
            indices.iter().all(|&i| i == 0),
            "泄漏 delta 滤掉后，所有保留块 index 应连续为 0，实际 {indices:?}"
        );
    }

    /// 🔴 回归：过滤后流**无任何 content 事件**（上游只吐 thinking 被滤光）→ 流结束补发
    /// error 事件，防客户端 "Stream ended without receiving any events" 卡死。
    #[tokio::test]
    async fn test_guard_empty_stream_emits_error_when_no_content() {
        // 只有 thinking 事件 → filter 滤光 → 空流 → guard 补 error
        let input = format!(
            "{}{}",
            event("message_start", r#"{"type":"message_start"}"#),
            event(
                "content_block_start",
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#
            ),
        );
        let stream = futures::stream::iter(vec![Ok::<Bytes, std::io::Error>(Bytes::from(input))]);
        let guarded = guard_empty_stream(filter_sse_stream(stream), "空响应测试");
        let filtered = collect_filtered(guarded).await;
        assert!(
            filtered.contains("\"type\":\"error\""),
            "空流必须补发 error 事件，实际: {filtered}"
        );
        assert!(filtered.contains("空响应测试"), "error 消息应透传");
    }

    /// 有真实 content 事件 → 不补发 error。
    #[tokio::test]
    async fn test_guard_empty_stream_keeps_content_unchanged() {
        let input = format!(
            "{}{}",
            event("message_start", r#"{"type":"message_start"}"#),
            event(
                "content_block_start",
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#
            ),
        );
        let stream = futures::stream::iter(vec![Ok::<Bytes, std::io::Error>(Bytes::from(input))]);
        let guarded = guard_empty_stream(filter_sse_stream(stream), "不应出现");
        let filtered = collect_filtered(guarded).await;
        assert!(
            !filtered.contains("\"type\":\"error\""),
            "有 content 事件不应补 error，实际: {filtered}"
        );
        assert!(filtered.contains("\"type\":\"text\""), "text 块保留");
    }

    /// 🔴 回归：滤掉 thinking 后，message_delta 的 usage.output_tokens 应扣减估算的
    /// thinking token（字符数/4），对齐主路径「剥离的 thinking 不计入 output_tokens」。
    #[tokio::test]
    async fn test_filter_sse_stream_deducts_thinking_from_usage() {
        let input = format!(
            "{}{}{}{}{}",
            event(
                "content_block_start",
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#
            ),
            event(
                "content_block_delta",
                // 40 个字符的 thinking → 估算 10 token
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}}"#
            ),
            event("content_block_stop", r#"{"type":"content_block_stop","index":0}"#),
            event(
                "content_block_start",
                r#"{"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}"#
            ),
            event(
                "message_delta",
                r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":500}}"#
            ),
        );
        let stream = futures::stream::iter(vec![Ok::<Bytes, std::io::Error>(Bytes::from(input))]);
        let filtered = collect_filtered(filter_sse_stream(stream)).await;
        assert!(
            filtered.contains("\"output_tokens\":490"),
            "output_tokens 应从 500 扣减 10（40 字符/4），实际: {filtered}"
        );
    }

    /// 🔴 回归：多个 message_delta（累计 usage，客户端读最后一条）时，thinking 扣减只扣一次，
    /// 且落在最后一条上。旧代码每条都扣（重复）；错误实现只扣第一条会丢扣减（客户端读最后）。
    #[tokio::test]
    async fn test_filter_sse_stream_deducts_thinking_once_across_multiple_deltas() {
        let input = format!(
            "{}{}{}{}{}{}",
            event(
                "content_block_start",
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#
            ),
            event(
                "content_block_delta",
                // 40 字符 thinking → 10 token
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}}"#
            ),
            event("content_block_stop", r#"{"type":"content_block_stop","index":0}"#),
            event(
                "content_block_start",
                r#"{"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}"#
            ),
            event(
                "message_delta",
                r#"{"type":"message_delta","delta":{"stop_reason":"max_tokens"},"usage":{"output_tokens":300}}"#
            ),
            event(
                "message_delta",
                r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":500}}"#
            ),
        );
        let stream = futures::stream::iter(vec![Ok::<Bytes, std::io::Error>(Bytes::from(input))]);
        let filtered = collect_filtered(filter_sse_stream(stream)).await;
        // 🔴 累计口径：output_tokens 是累计值、客户端读最后一条，**每条都扣同一份** thinking
        //（10 token）。第一条 300-10=290；第二条 500-10=490。旧 deducted_chars 逻辑只扣
        // 第一条 → 客户端读最后一条 500（未扣），扣减丢失。
        assert!(
            filtered.contains("\"output_tokens\":290"),
            "第一条 delta 应扣 10（300-10=290），实际: {filtered}"
        );
        assert!(
            filtered.contains("\"output_tokens\":490"),
            "累计口径下第二条 delta 也要扣 10（500-10=490），旧逻辑会保持 500 丢扣减，实际: {filtered}"
        );
    }

    /// 🔴 回归：无 usage 的 message_delta 不触发扣减；后续有 usage 的 delta 仍能扣。
    #[tokio::test]
    async fn test_filter_sse_stream_deducts_on_delta_with_usage_only() {
        let input = format!(
            "{}{}{}{}{}{}",
            event(
                "content_block_start",
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#
            ),
            event(
                "content_block_delta",
                // 40 字符 thinking → 10 token
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}}"#
            ),
            event("content_block_stop", r#"{"type":"content_block_stop","index":0}"#),
            event(
                "content_block_start",
                r#"{"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}"#
            ),
            event(
                "message_delta",
                r#"{"type":"message_delta","delta":{"stop_reason":"max_tokens"}}"#
            ),
            event(
                "message_delta",
                r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":300}}"#
            ),
        );
        let stream = futures::stream::iter(vec![Ok::<Bytes, std::io::Error>(Bytes::from(input))]);
        let filtered = collect_filtered(filter_sse_stream(stream)).await;
        assert!(
            filtered.contains("\"output_tokens\":290"),
            "无 usage 的首条 delta 不应前移 deducted_chars，第二条有 usage 应扣 10（300-10=290），实际: {filtered}"
        );
    }

    /// 内联 `<thinking>...</thinking>` 标签剥离（跨 chunk 状态）。
    #[tokio::test]
    async fn test_filter_sse_stream_strips_inline_thinking() {
        let input = format!(
            "{}{}",
            event(
                "content_block_start",
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#
            ),
            event(
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"A <thinking>secret reason</thinking> B"}}"#
            ),
        );
        let stream = futures::stream::iter(vec![Ok::<Bytes, std::io::Error>(Bytes::from(input))]);
        let filtered = collect_filtered(filter_sse_stream(stream)).await;
        assert!(
            !filtered.contains("secret reason"),
            "内联 thinking 内容必须剥掉，实际: {filtered}"
        );
        assert!(filtered.contains("A"), "标签前文本保留");
        assert!(filtered.contains("B"), "标签后文本保留");
    }

    /// 非流式 JSON：内联 thinking 标签剥离。
    #[test]
    fn test_filter_json_bytes_strips_inline_thinking() {
        let input = serde_json::json!({
            "id": "msg_1",
            "content": [
                {"type": "text", "text": "A <thinking>secret</thinking> B"}
            ]
        });
        let out = filter_json_bytes(input.to_string().as_bytes());
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        let text = v["content"][0]["text"].as_str().unwrap();
        assert!(!text.contains("secret"), "内联 thinking 必须剥，实际: {text}");
        assert!(text.contains("A") && text.contains("B"), "标签外文本保留");
    }

    /// 🔴 回归：`redacted_thinking`（超预算的合法类型）流式路径也要整块滤掉。
    #[tokio::test]
    async fn test_filter_sse_stream_removes_redacted_thinking() {
        let input = format!(
            "{}{}{}{}{}",
            event(
                "content_block_start",
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"redacted_thinking","data":""}}"#
            ),
            event(
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"redacted_thinking_delta","data":"x"}}"#
            ),
            event("content_block_stop", r#"{"type":"content_block_stop","index":0}"#),
            event(
                "content_block_start",
                r#"{"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}"#
            ),
            event(
                "content_block_delta",
                r#"{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"Hello"}}"#
            ),
        );
        let stream = futures::stream::iter(vec![Ok::<Bytes, std::io::Error>(Bytes::from(input))]);
        let filtered = collect_filtered(filter_sse_stream(stream)).await;
        assert!(
            !filtered.contains("redacted_thinking"),
            "redacted_thinking start/delta/stop 必须整块滤掉"
        );
        assert!(filtered.contains("Hello"), "text 块必须保留");
        let indices = extract_indices(&filtered);
        assert!(
            indices.iter().all(|&i| i == 0),
            "滤掉 redacted_thinking 后 text 块应重编号为 0，实际 {indices:?}"
        );
    }

    /// 🔴 回归：多行 `data:` 事件（SSE 规范允许）应拼接成完整 JSON 再解析，
    /// 而不是只取最后一行（旧行为会让 JSON 解析失败 → thinking 泄漏）。
    #[tokio::test]
    async fn test_filter_sse_stream_multiline_data_concatenated() {
        // 一个 data 事件被拆成两行（token 边界拆行），拼接后是合法 JSON。
        let multiline = format!(
            "event: content_block_start\ndata: {{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{{\"type\":\"text\",\"text\":\"\"}}}}\n\n"
        );
        // 构造：thinking start（正常单行）+ 一个多行 data 的 text start
        let input = format!(
            "{}{}",
            event(
                "content_block_start",
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#
            ),
            // 把 text start 的 data 拆成两行（`"content_block"` 后断行）
            multiline
        );
        let stream = futures::stream::iter(vec![Ok::<Bytes, std::io::Error>(Bytes::from(input))]);
        let filtered = collect_filtered(filter_sse_stream(stream)).await;
        // 多行 data 拼接后应能解析成 text 块并保留；thinking 块滤掉。
        assert!(
            filtered.contains("\"type\":\"text\""),
            "多行 data 拼接后应解析为 text 块保留，实际: {filtered}"
        );
        assert!(
            !filtered.contains("thinking"),
            "thinking 块仍应滤掉"
        );
    }

    /// 非流式 JSON：滤掉 thinking/redacted_thinking 块，text/tool_use 保留。
    #[test]
    fn test_filter_json_bytes_removes_thinking_blocks() {
        let input = serde_json::json!({
            "id": "msg_1",
            "content": [
                {"type": "thinking", "thinking": "let me think"},
                {"type": "redacted_thinking", "data": "secret"},
                {"type": "text", "text": "Hello"},
                {"type": "tool_use", "id": "t1", "name": "fs_write", "input": {}}
            ],
            "stop_reason": "tool_use"
        });
        let out = filter_json_bytes(input.to_string().as_bytes());
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        let content = v["content"].as_array().unwrap();
        assert_eq!(
            content.len(),
            2,
            "thinking+redacted_thinking 块都被滤掉，text+tool_use 保留"
        );
        assert!(content.iter().all(|b| b["type"] != "thinking"));
        assert!(
            content.iter().all(|b| b["type"] != "redacted_thinking"),
            "redacted_thinking 非流式也必须滤掉（与流式路径一致）"
        );
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
