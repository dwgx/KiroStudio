//! passthrough 响应侧的 thinking 过滤（custom_api 透传专用）。
//!
//! 设计约束：passthrough 的卖点是「字节流原样透传」、零协议转换（隔离铁律 3）。
//! 所以本模块的两层处理**门控不同**（2026-08-10 拆开）：
//!
//! - **thinking 块过滤**（`filter_thinking_blocks`）：滤掉 thinking/redacted_thinking 块、
//!   重编号保留块 index、按滤掉字符扣减 usage。这**改协议结构**，按门控启用 ——
//!   因为 deepseek 类中转站在 thinking disabled 时仍可能吐 thinking 块，客户端
//!   （Claude Code）看到 thinking 会报 "Tool result missing"。其余凭据保持结构原样。
//! - **DSML 标记剥离**：所有 custom_api 凭据**无条件**启用。DeepSeek 会把
//!   `<｜DSML｜function_calls>` 这类工具协议标记当普通文本吐进 text_delta，泄漏给客户端
//!   就是可见的乱码 —— 这与「客户端要不要 thinking」「凭据开没开归一化」都无关，
//!   custom_api 的上游就是 DeepSeek 系，故不能门控。改前整个 filter 入口被
//!   `ds_cfg.is_some()` 挡住，未开归一化的号标记原样泄漏（用户走代挂号拉 OpenZ 的实际故障）。
//!
//! 代价（明确接受）：SSE/JSON 响应现在对所有 custom_api 号都逐事件解析。
//! **真正改写**的事件（滤 thinking、剥 DSML、index 重编号、usage 扣减、mock cache）
//! 会重序列化（JSON key 顺序可能重排）；未改写的事件与 `message_start` 一样回原字节。
//! 解析失败一律 fail-open。
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
    filter_sse_stream_with(inner, true, true, None)
}

/// [`filter_sse_stream`] 的可配置版本：
/// - `strip_inline_thinking`：是否剥 text_delta 里的内联 `<thinking>...</thinking>` 标签
///   （deepseek 可能以文本形式吐 thinking）；
/// - `filter_thinking_blocks`：是否滤掉 `content_block_start/delta/stop` 的 thinking 块。
///   **DSML 标记剥离不在此门控内，永远启用**（上游就是 DeepSeek，标记泄漏与客户端
///   模型名/归一化配置无关）——`filter_thinking_blocks=false` 时只关 thinking 块过滤，
///   保留完整字节流语义，仅剥 text 里的 DSML 标记；
/// - `mock_cache`：`Some((true, ratio))` 时对带 usage 的事件（message_start / message_delta）
///   注入模拟 cache 值（`cache_read_input_tokens = round(input_tokens × ratio)`、
///   creation 置 0）；`None`/`Some((false, _))` 时**零注入**。未改写的事件
///   （含 `content_block_delta` / `message_delta`）回原字节，与 `message_start` 对齐。
///   仅透传路径调用，Kiro 池四层缓存链不走本函数。
pub fn filter_sse_stream_with<S, E>(
    inner: S,
    strip_inline_thinking: bool,
    filter_thinking_blocks: bool,
    mock_cache: Option<(bool, f64)>,
) -> impl Stream<Item = Result<Bytes, axum::Error>>
where
    S: Stream<Item = Result<Bytes, E>> + Send + 'static,
    E: std::error::Error + Send + Sync + 'static,
{
    SseThinkFilter {
        inner: Box::pin(inner),
        state: SseFilterState::new(
            strip_inline_thinking,
            filter_thinking_blocks,
            mock_cache,
        ),
    }
}

/// 过滤非流式 JSON 响应里的 thinking content blocks（fail-open：解析失败原样返回）。
pub fn filter_json_bytes(bytes: &[u8]) -> Bytes {
    filter_json_bytes_with(bytes, true, true, None)
}

/// [`filter_json_bytes`] 的可配置版本（与流式 [`filter_sse_stream_with`] 对齐）：
/// - `strip_inline`：是否剥 text 块里的内联 `<thinking>...</thinking>` 标签；
/// - `filter_thinking_blocks`：是否滤掉 `type == "thinking"/"redacted_thinking"` 的块。
///   **DSML 标记剥离不在此门控内，永远启用**（理由同流式版本）；
/// - `mock_cache`：`Some((true, ratio))` 时对顶层 `usage` 注入模拟 cache 值（同流式）。
pub fn filter_json_bytes_with(
    bytes: &[u8],
    strip_inline: bool,
    filter_thinking_blocks: bool,
    mock_cache: Option<(bool, f64)>,
) -> Bytes {
    let Ok(mut v) = serde_json::from_slice::<serde_json::Value>(bytes) else {
        if let Some((true, _)) = mock_cache {
            tracing::debug!("[透传] 非流式响应 JSON 解析失败，模拟缓存注入跳过（fail-open 原样透传）");
        }
        return Bytes::copy_from_slice(bytes);
    };
    // 模拟缓存注入（透传路径专用，mockCacheEnabled）：usage.cache_read_input_tokens 上游
    // 恒 0（DeepSeek 不回传），下游看不到缓存分支——这里注入 round(input × ratio) 的
    // 伪造值、creation 置 0（模拟全命中不写缓存）。
    //
    // ⚠️ 必须先于 content 早退执行：注入只依赖顶层 usage，与 content 无关——上游 2xx
    // JSON 缺 content 字段时也必须注入，否则 mock 注入静默失效（旧实现注入代码在
    // content 早退之后，content 缺失时永远执行不到，且无任何日志）。流式路径按事件
    // 处理不依赖 content，两路径不对称即源于此。仅顶层 usage 有 input_tokens 时注入，
    // 缺失则整体跳过（零改动）。
    if let Some((true, ratio)) = mock_cache {
        if let Some(usage) = v.get_mut("usage").and_then(|u| u.as_object_mut()) {
            if let Some(input_tokens) = usage.get("input_tokens").and_then(|x| x.as_u64()) {
                inject_mock_cache_usage(usage, input_tokens, ratio);
            } else {
                tracing::debug!("[透传] mock 开启但 usage 缺 input_tokens，模拟缓存注入跳过");
            }
        } else {
            tracing::debug!("[透传] mock 开启但顶层缺 usage，模拟缓存注入跳过");
        }
    }
    let Some(content) = v.get_mut("content").and_then(|c| c.as_array_mut()) else {
        // content 缺失/非数组：mock 关闭时保持字节级原样（零改动）；开启时注入已完成，
        // 需重序列化以携带注入结果（JSON 语义等价，非字节级原样——键顺序可能重排，
        // 与流式路径一致）。
        if mock_cache.is_some_and(|(enabled, _)| enabled) {
            return serde_json::to_vec(&v)
                .map(Bytes::from)
                .unwrap_or_else(|_| Bytes::copy_from_slice(bytes));
        }
        return Bytes::copy_from_slice(bytes);
    };
    // ⚠️ 与流式路径的 `in_thinking` 判定一致：`redacted_thinking`（超预算的合法类型）
    // 也必须滤掉，否则非流式请求下客户端同样报 "Tool result missing"。
    if filter_thinking_blocks {
        content.retain(|block| {
            !matches!(
                block.get("type").and_then(|t| t.as_str()),
                Some("thinking") | Some("redacted_thinking")
            )
        });
    }
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
    // DSML 标记剥离（deepseek 把 `<｜DSML｜function_calls>` 工具协议标记当文本吐）。
    // 非流式整块读取、无跨 chunk 切分问题，用独立局部变量一次性剥净即可。
    // 与流式路径同语义：只剥本行内的标记（不吞换行后正文）、识别闭合标签、白名单守正文。
    {
        let mut dsml_pending = DsmlPending::default();
        for block in content.iter_mut() {
            if block.get("type").and_then(|t| t.as_str()) == Some("text") {
                if let Some(text_val) = block.get_mut("text") {
                    if let Some(s) = text_val.as_str() {
                        let stripped = strip_dsml_passthrough(s, &mut dsml_pending);
                        // 非流式无后续 chunk：残留的半截标记直接丢弃（补发会泄漏标记）。
                        dsml_pending.buf.clear();
                        dsml_pending.is_marker = false;
                        *text_val = serde_json::Value::String(stripped);
                    }
                }
            }
        }
    }
    // 末尾统一重序列化（注入已在函数头部完成）。
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
    /// 是否滤掉 thinking content block（start/delta/stop）并重编号保留块。
    ///
    /// `false` 时：thinking 块原样透传、**不重编号**
    /// （index_map 不参与），只做 DSML 标记剥离 —— 保住「零协议转换」的透传语义。
    filter_thinking_blocks: bool,
    /// 跨 chunk 的"正在内联 thinking 内"状态。
    in_inline_thinking: bool,
    /// 跨 chunk 的 DSML 半截标记/孤立 `<` 待拼接尾巴（与 `strip_inline_thinking` 的
    /// `in_inline_thinking` 同构）。DeepSeek 会把 `<｜DSML｜function_calls` 这类标记当文本吐，
    /// 可能被上游分帧从中间切开 —— 留到下一 chunk 拼上再判定。
    dsml_pending: DsmlPending,
    /// 滤掉的 thinking 文本累计**字符数**（估算 token = 字符数 / 4）。
    thinking_chars: u64,
    /// 模拟缓存注入配置：`Some((true, ratio))` = 开启，带 usage 的事件注入
    /// `cache_read_input_tokens = round(input_tokens × ratio)`、creation 置 0。
    /// 仅透传路径传入；关闭时零改动（原样透传）。
    mock_cache: Option<(bool, f64)>,
}

impl SseFilterState {
    fn new(
        strip_inline_thinking: bool,
        filter_thinking_blocks: bool,
        mock_cache: Option<(bool, f64)>,
    ) -> Self {
        Self {
            buf: Vec::with_capacity(1024),
            pending: Vec::new(),
            in_thinking: HashMap::new(),
            index_map: HashMap::new(),
            next_index: 0,
            strip_inline_thinking,
            filter_thinking_blocks,
            in_inline_thinking: false,
            dsml_pending: DsmlPending::default(),
            thinking_chars: 0,
            mock_cache,
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

    /// 上游结束：flush 未闭合尾部（fail-open 原样透传）+ DSML 待拼接尾巴收尾。
    fn finish(&mut self) -> Bytes {
        // DSML 尾巴收尾：确认标记残留丢弃；被 hold 的正文按 text_delta 补发（不吞字）。
        // 挂到最后一个已保留 text 块的 index（`next_index - 1`；无则补发到 index 0）。
        if !self.dsml_pending.is_empty() {
            let leftover = self.dsml_pending.flush();
            if !leftover.is_empty() {
                let idx = self.next_index.saturating_sub(1);
                let ev = Self::rewrite_event(
                    "content_block_delta",
                    &serde_json::json!({
                        "type": "content_block_delta",
                        "index": idx,
                        "delta": { "type": "text_delta", "text": leftover }
                    }),
                );
                let mut tail = if self.buf.is_empty() {
                    ev
                } else {
                    let mut b = std::mem::take(&mut self.buf);
                    b.extend_from_slice(&ev);
                    b
                };
                // 补全事件分隔符（rewrite_event 不含结尾空行，SSE 客户端依赖它分帧）。
                tail.extend_from_slice(b"\n\n");
                return Bytes::from(tail);
            }
        }
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
            // thinking 块过滤关闭时：不滤 thinking、
            // 不重编号，start/stop 一律原样透传 —— 只有 delta 会走下面的 DSML 剥离。
            "content_block_start" | "content_block_stop" if !self.filter_thinking_blocks => {
                Some(block.to_vec())
            }
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
                //
                // thinking 块过滤关闭时 `in_thinking` 恒空（start 分支根本没记录），
                // 故这里自然不丢任何 delta —— 只往下走 DSML 剥离。
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
                // 只在文本真的变了才写回 —— 未改写则后面回原字节（P1-11）。
                let mut payload_changed = false;
                if self.strip_inline_thinking {
                    if let Some(delta) = v.get_mut("delta").and_then(|d| d.as_object_mut()) {
                        if delta.get("type").and_then(|x| x.as_str()) == Some("text_delta") {
                            if let Some(text_val) = delta.get_mut("text") {
                                if let Some(s) = text_val.as_str() {
                                    let stripped =
                                        strip_inline_thinking(s, &mut self.in_inline_thinking);
                                    if stripped.is_empty() && self.in_inline_thinking {
                                        return None;
                                    }
                                    if stripped != s {
                                        *text_val = serde_json::Value::String(stripped);
                                        payload_changed = true;
                                    }
                                }
                            }
                        }
                    }
                }
                // DSML 标记剥离：DeepSeek 把 `<｜DSML｜function_calls>` 等工具协议标记当文本吐。
                // 透传层此前零处理 → 标记原样泄漏（用户走 custom_api 拉 OpenZ 时看到的裸标记）。
                // 跨 chunk：半截标记/孤立 `<` 留 `dsml_pending` 等下轮拼接。
                if let Some(delta) = v.get_mut("delta").and_then(|d| d.as_object_mut()) {
                    if delta.get("type").and_then(|x| x.as_str()) == Some("text_delta") {
                        if let Some(text_val) = delta.get_mut("text") {
                            if let Some(s) = text_val.as_str() {
                                let stripped = strip_dsml_passthrough(s, &mut self.dsml_pending);
                                if stripped.is_empty() && self.dsml_pending.is_empty() {
                                    return None;
                                }
                                if stripped != s {
                                    *text_val = serde_json::Value::String(stripped);
                                    payload_changed = true;
                                }
                            }
                        }
                    }
                }
                self.rewrite_index_or_passthrough(
                    &mut v,
                    event_type,
                    block,
                    old_idx,
                    payload_changed,
                )
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
                let mut payload_changed = false;
                if self.thinking_chars > 0 {
                    if let Some(usage) = v.get_mut("usage").and_then(|u| u.as_object_mut()) {
                        if let Some(out) = usage.get("output_tokens").and_then(|x| x.as_u64()) {
                            let deduct = (self.thinking_chars / 4) as u64;
                            usage["output_tokens"] =
                                serde_json::json!(out.saturating_sub(deduct));
                            payload_changed = true;
                        }
                    }
                }
                // 模拟缓存注入（与 message_start 共用同一逻辑，见下）：
                // Anthropic 的 message_delta.usage 通常只有 output_tokens —— 没有
                // input_tokens 时不注入（零改动）；个别上游带 input_tokens 时同样注入。
                if let Some((true, ratio)) = self.mock_cache {
                    if let Some(usage) = v.get_mut("usage").and_then(|u| u.as_object_mut()) {
                        if let Some(input_tokens) =
                            usage.get("input_tokens").and_then(|x| x.as_u64())
                        {
                            inject_mock_cache_usage(usage, input_tokens, ratio);
                            payload_changed = true;
                        }
                    }
                }
                if payload_changed {
                    Some(Self::rewrite_event(event_type, &v))
                } else {
                    Some(block.to_vec())
                }
            }
            "message_start" => {
                // 模拟缓存注入：message_start 的 usage 带 input_tokens，是下游读 cache 分支的
                // 主入口。开启时注入 round(input × ratio) + creation 置 0；关闭时原样透传
                //（返回原始字节，零改动）。
                //
                // ⚠️ **usage 的定位**（线上实测坐实）：真实 Anthropic message_start 的 usage
                // **嵌套在 `message` 内**（`{"type":"message_start","message":{"id":...,"usage":{...}}}`
                // ——顶层无 usage）。旧实现只读顶层 `v["usage"]`，注入条件永不满足 ⇒ 上游
                // 原样透传、cache_read 恒 0。先探 `message.usage`（真实形态），顶层形态
                // （个别上游/旧客户端）回退兼容。先只读探测路径再取可变引用，避免两个
                // `&mut` 分支借用冲突。
                if let Some((true, ratio)) = self.mock_cache {
                    let usage = if v
                        .get("message")
                        .and_then(|m| m.get("usage"))
                        .is_some()
                    {
                        v.get_mut("message").and_then(|m| m.get_mut("usage"))
                    } else {
                        v.get_mut("usage")
                    };
                    if let Some(usage) = usage.and_then(|u| u.as_object_mut()) {
                        if let Some(input_tokens) =
                            usage.get("input_tokens").and_then(|x| x.as_u64())
                        {
                            inject_mock_cache_usage(usage, input_tokens, ratio);
                            return Some(Self::rewrite_event(event_type, &v));
                        }
                    }
                }
                Some(block.to_vec())
            }
            "content_block_stop" => {
                let was_thinking = self.in_thinking.remove(&old_idx).unwrap_or(false);
                if was_thinking {
                    return None;
                }
                self.rewrite_index_or_passthrough(&mut v, event_type, block, old_idx, false)
            }
            _ => Some(block.to_vec()),
        }
    }

    /// delta/stop 透传时若该 index 有**真正不同**的重映射则重写；payload 未改且
    /// index 未变则回原字节（P1-11，与 message_start 对齐）。
    ///
    /// `payload_changed`：调用方已在 `v` 上剥掉内联 thinking / DSML。为 true 时
    /// **不能** `block.to_vec()` 回退原始字节（否则剥离静默丢弃、标记原样泄漏）。
    fn rewrite_index_or_passthrough(
        &self,
        v: &mut serde_json::Value,
        event_type: &str,
        block: &[u8],
        old_idx: usize,
        payload_changed: bool,
    ) -> Option<Vec<u8>> {
        let mapped = self.index_map.get(&old_idx).copied();
        if let Some(new_idx) = mapped.filter(|&n| n != old_idx) {
            // ⚠️ 同 content_block_start：非 object 顶层 data 不可裸索引赋值（serde_json panic）。
            let Some(o) = v.as_object_mut() else {
                return Some(block.to_vec());
            };
            o.insert("index".to_string(), serde_json::json!(new_idx));
            return Some(Self::rewrite_event(event_type, v));
        }
        if payload_changed {
            if v.as_object().is_none() {
                return Some(block.to_vec());
            }
            return Some(Self::rewrite_event(event_type, v));
        }
        Some(block.to_vec())
    }

    /// 把重写后的 data 重组成 SSE 事件（`event:` + `data:`，**不含**结尾空行）。
    fn rewrite_event(event_type: &str, data: &serde_json::Value) -> Vec<u8> {
        format!("event: {event_type}\ndata: {data}").into_bytes()
    }
}

/// 向 usage 对象注入模拟 cache 值（透传路径 mockCacheEnabled，流式/非流式共用）。
///
/// - `cache_read_input_tokens = round(input_tokens × ratio)`（ratio 已由镜像 setter
///   清洗到 [0.0, 1.0]，read ≤ input 恒成立）；
/// - `cache_creation_input_tokens = 0`（模拟"全部命中不写缓存"，避免 read + creation
///   > input 的矛盾）；
/// - 上游自带的 `cache_creation_5m_input_tokens` / `cache_creation_1h_input_tokens`
///   若存在也置 0（与 creation 同语义）。
///
/// **伪造值，仅供下游展示，不是真实计费依据**（与主路径 prompt_cache 估算下发同性质）。
/// 调用方必须先确认 usage 含 `input_tokens`（缺 input_tokens 不注入，整体零改动）。
fn inject_mock_cache_usage(
    usage: &mut serde_json::Map<String, serde_json::Value>,
    input_tokens: u64,
    ratio: f64,
) {
    let read = ((input_tokens as f64) * ratio).round() as u64;
    usage.insert("cache_read_input_tokens".to_string(), serde_json::json!(read));
    usage.insert("cache_creation_input_tokens".to_string(), serde_json::json!(0));
    if usage.contains_key("cache_creation_5m_input_tokens") {
        usage.insert("cache_creation_5m_input_tokens".to_string(), serde_json::json!(0));
    }
    if usage.contains_key("cache_creation_1h_input_tokens") {
        usage.insert("cache_creation_1h_input_tokens".to_string(), serde_json::json!(0));
    }
}

/// 透传层 DSML 标记剥离（跨 chunk 安全）。
///
/// 为什么需要：custom_api 透传路径（passthrough.rs）把上游字节流**原样回传**，不进主路径
/// `StreamContext`。而 DeepSeek 上游会先把 `<｜DSML｜function_calls>` 这类工具协议标记当
/// 普通文本吐进 text_delta（实测坐实，同主路径 `strip_dsml_markers` 的怪癖）。主路径剥离
/// 够不到透传层，标记就原样泄漏给客户端 —— 用户经 kirostudio custom_api 拉 OpenZ 订阅时
/// 看到的裸 `<｜DSML｜function_calls` 正是这条路径。
///
/// 语义（对齐主路径 `strip_dsml_markers` 的 2026-08-09 修复 + fuckopencode 12c 教训）：
/// - 完整标记 `<｜DSML｜function_calls>` / `<｜tool▁calls▁begin｜>` 等（单行内 `>` 闭合）→ 整段丢弃；
/// - 闭合标签 `</｜DSML｜parameter>` → 整段丢弃（此前 `</｜` 前缀不识别导致闭合标签泄漏）；
/// - 半截标记（无 `>` 收尾）→ 只剥**本行内**的标记部分，换行及之后正文**绝不吞**
///   （正文里任意 `>` 如 `a > b` / `=>` 不能触发跨行吞掉整段）；
/// - 末尾孤立 `<`（可能是 `<｜` 被切开）→ hold 进 `dsml_pending` 等下轮拼接判定；
/// - 非 DSML 关键字的 `<｜…>`（CJK 排版 / 正文引用）绝不误删 —— 白名单 `dsml/tool/function` 前缀。
///
/// 跨 chunk 状态由 `dsml_pending`（待拼接尾巴）携带，与 `strip_inline_thinking` 的
/// `in_thinking` 同构。返回剥净后的文本（不含尾巴）。

/// 透传层 DSML 跨 chunk 待拼接状态。`buf` 是尾巴文本，`is_marker` 标记 `buf` 是
/// **已确认的 DSML 标记残留**（流结束 flush 时应丢弃）还是**被 hold 待判定的正文**
/// （流结束 flush 时应补发）——缺失这个标志是 2026-08-10 对抗评审发现的根因之一
/// （透传层此前只有文本没有判定，半截标记+同行正文被静默吞掉 / 真标记被超长放行泄漏）。
#[derive(Default)]
struct DsmlPending {
    buf: String,
    is_marker: bool,
}

impl DsmlPending {
    fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// 流结束收尾：`is_marker` 尾巴（确认标记残留）丢弃；否则按正文补发。
    fn flush(&mut self) -> String {
        let was_marker = self.is_marker;
        let leftover = std::mem::take(&mut self.buf);
        self.is_marker = false;
        if was_marker {
            return String::new();
        }
        leftover
    }
}

fn strip_dsml_passthrough(text: &str, dsml_pending: &mut DsmlPending) -> String {
    // 快路径：无待处理尾巴且不含 `<`，原样返回。
    if dsml_pending.is_empty() && !text.contains('<') {
        return text.to_string();
    }
    let mut work = std::mem::take(&mut dsml_pending.buf);
    dsml_pending.is_marker = false;
    work.push_str(text);

    let mut out = String::with_capacity(work.len());
    let chars: Vec<char> = work.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        // 探测 DSML 标记起点：`<｜`（开）或 `</｜`（闭）。
        let is_close_tag =
            chars[i] == '<' && i + 2 < chars.len() && chars[i + 1] == '/' && chars[i + 2] == '\u{FF5C}';
        if (chars[i] == '<' && i + 1 < chars.len() && chars[i + 1] == '\u{FF5C}') || is_close_tag {
            let kw_start = if is_close_tag { i + 3 } else { i + 2 };
            let rest: String = chars[kw_start..].iter().collect();
            // 白名单：`<｜` 后必须是 dsml/tool/function 关键字才当标记，否则是正文。
            let r = rest.trim_start().to_ascii_lowercase();
            let looks_marker = r.starts_with("dsml") || r.starts_with("tool") || r.starts_with("function");
            // 闭合查找限行：遇到 `>` 或 `\n` 停。跨行的正文绝不能因为正文里任意 `>` 被当标记吞掉。
            let closed = chars[i..].iter().position(|&c| c == '>' || c == '\n');
            if looks_marker {
                if let Some(rel) = closed {
                    let closed_by_gt = chars[i + rel] == '>';
                    if closed_by_gt {
                        i += rel + 1; // 完整标记（含 `>`）整段丢弃
                    } else {
                        // 半截标记 + 换行：剥本行内标记部分，换行本身也跳过（分隔符），正文保留。
                        i += rel;
                        if i < chars.len() && chars[i] == '\n' {
                            i += 1;
                        }
                    }
                    continue;
                } else {
                    // 已确认 DSML 关键字标记但无 `>` 闭合：标记噪音，丢弃本 chunk 从 `<｜` 起的
                    // 余下全部，尾巴标为「确认标记残留」→ 流结束 flush 时丢弃而非补发。
                    let held: String = chars[i..].iter().collect();
                    if held.chars().count() > 48 {
                        // 超长仍无 `>`：关键字命中大概率是误判（正文恰以 <｜tool… 开头且很长），
                        // 放行为正文避免吞掉大段合法内容（误判从宽）。
                        out.push_str(&held);
                    } else {
                        dsml_pending.buf = held;
                        dsml_pending.is_marker = true;
                    }
                    return out;
                }
            }
            // 非关键字：可能是(a)正文里合法 `<｜…>`→原样输出这个 `<`，继续扫；
            // (b)关键字还没到齐（rest 短且未闭合）→ hold 等下轮确认。
            let undecided = closed.is_none() && rest.chars().count() < 8;
            if undecided {
                let held: String = chars[i..].iter().collect();
                if held.chars().count() <= 48 {
                    dsml_pending.buf = held;
                    return out;
                }
                // 超长：放行为正文
            }
            // 确定不是标记：原样输出 `<` 后继续扫（`</` 也原样输出，不误删正文里合法的 `</` 组合）。
            out.push(chars[i]);
            i += 1;
            continue;
        }
        out.push(chars[i]);
        i += 1;
    }
    // 边界：输出末尾孤立 `<`（可能是 `</` 或 `<｜` 被切开）→ hold 等下轮。
    if out.ends_with('<') {
        out.pop();
        dsml_pending.buf.push('<');
    }
    out
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

    // ===== 透传层 DSML 剥离回归(2026-08-09 实测坐实,此前透传层零处理)=====
    //
    // 用户走 custom_api 透传拉 OpenZ 订阅时,DeepSeek 把 `<｜DSML｜function_calls>` 标记当
    // 普通文本吐进 text_delta。透传层此前只滤 thinking,标记原样泄漏。以下测试覆盖:
    // 完整/半截/闭合/跨 chunk/跨行正文不吞/白名单守正文。

    /// 流式:完整 DSML 标记从 text_delta 剥离。
    #[tokio::test]
    async fn test_filter_sse_stream_strips_dsml_full_marker() {
        let input = format!(
            "{}{}{}",
            event(
                "content_block_start",
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#
            ),
            event(
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"<｜DSML｜function_calls｜>后续"}}"#
            ),
            event(
                "content_block_stop",
                r#"{"type":"content_block_stop","index":0}"#
            ),
        );
        let stream = futures::stream::iter(vec![Ok::<Bytes, std::io::Error>(Bytes::from(input))]);
        let filtered = collect_filtered(filter_sse_stream(stream)).await;
        assert!(filtered.contains("后续"), "标记剥掉,正文保留,实际: {filtered}");
        assert!(!filtered.contains("DSML"), "标记不得泄漏");
    }

    /// 流式:半截标记 + 换行正文(含 `>`),绝不能跨行吞正文。
    #[tokio::test]
    async fn test_filter_sse_stream_strips_dsml_half_marker_no_swallow() {
        let input = format!(
            "{}{}",
            event(
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"<｜DSML｜function_calls\n阅读\n如果 a > b 就返回"}}"#
            ),
            event(
                "content_block_stop",
                r#"{"type":"content_block_stop","index":0}"#
            ),
        );
        let stream = futures::stream::iter(vec![Ok::<Bytes, std::io::Error>(Bytes::from(input))]);
        let filtered = collect_filtered(filter_sse_stream(stream)).await;
        assert!(
            filtered.contains("阅读") && filtered.contains("a > b"),
            "换行后正文含 `>` 不能被吞,实际: {filtered}"
        );
        assert!(!filtered.contains("DSML"), "标记不得泄漏");
    }

    /// 流式:跨 chunk 切开的 DSML 标记(第一块 hold、第二块拼上剥净)。
    #[tokio::test]
    async fn test_filter_sse_stream_strips_dsml_cross_chunk() {
        // 两个事件各自独立成 chunk:第一个 chunk 的 text_delta 只到 `<｜DSML｜func`(半截),
        // 第二个 chunk 拼上 `tion_calls｜>之后`。dsml_pending 必须跨 chunk hold 尾巴。
        let chunk1 = event(
            "content_block_delta",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"<｜DSML｜func"}}"#,
        );
        let chunk2 = event(
            "content_block_delta",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"tion_calls｜>之后"}}"#,
        );
        let stream = futures::stream::iter(vec![
            Ok::<Bytes, std::io::Error>(Bytes::from(chunk1)),
            Ok::<Bytes, std::io::Error>(Bytes::from(chunk2)),
        ]);
        let filtered = collect_filtered(filter_sse_stream(stream)).await;
        assert!(
            filtered.contains("之后"),
            "跨 chunk 标记剥净,后续文本保留,实际: {filtered}"
        );
        assert!(!filtered.contains("DSML") && !filtered.contains("func"), "标记不得泄漏");
    }

    /// 流式:闭合标签 `</｜DSML｜parameter>` 也被剥(此前只认 `<｜` 导致闭合标签泄漏)。
    #[tokio::test]
    async fn test_filter_sse_stream_strips_dsml_close_tag() {
        let input = format!(
            "{}{}",
            event(
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"echo hi</｜DSML｜parameter>\n回答"}}"#
            ),
            event(
                "content_block_stop",
                r#"{"type":"content_block_stop","index":0}"#
            ),
        );
        let stream = futures::stream::iter(vec![Ok::<Bytes, std::io::Error>(Bytes::from(input))]);
        let filtered = collect_filtered(filter_sse_stream(stream)).await;
        assert!(
            filtered.contains("echo hi") && filtered.contains("回答"),
            "命令正文与后续保留,实际: {filtered}"
        );
        assert!(!filtered.contains("DSML") && !filtered.contains("parameter"), "闭合标签不得泄漏,实际: {filtered:?}");
    }

    /// 非流式:JSON text 块里的 DSML 标记被剥,正文保留。
    #[test]
    fn test_filter_json_bytes_strips_dsml() {
        let json = r#"{"content":[{"type":"text","text":"<｜DSML｜function_calls\n已运行 2 命令\n结果 => 成功"}]}"#;
        let filtered = filter_json_bytes(json.as_bytes());
        let v: serde_json::Value = serde_json::from_slice(&filtered).unwrap();
        let text = v["content"][0]["text"].as_str().unwrap();
        assert_eq!(text, "已运行 2 命令\n结果 => 成功", "标记剥净,正文保留");
    }

    /// 正常文本(含普通 < >)与全角 CJK 标记不被改写。
    #[test]
    fn test_filter_json_bytes_dsml_does_not_touch_normal() {
        let normal = r#"{"content":[{"type":"text","text":"if a < b && c > d 正常代码"}]}"#;
        let filtered = filter_json_bytes(normal.as_bytes());
        let v: serde_json::Value = serde_json::from_slice(&filtered).unwrap();
        assert_eq!(v["content"][0]["text"].as_str().unwrap(), "if a < b && c > d 正常代码");

        let fullwidth = r#"{"content":[{"type":"text","text":"见 <｜注｜关于x｜> 说明"}]}"#;
        let filtered = filter_json_bytes(fullwidth.as_bytes());
        let v: serde_json::Value = serde_json::from_slice(&filtered).unwrap();
        assert_eq!(v["content"][0]["text"].as_str().unwrap(), "见 <｜注｜关于x｜> 说明");
    }

    // ===== `filter_thinking_blocks = false`（未开启 thinking 过滤的 custom_api 号）=====
    //
    // 这组坐实门控拆分的语义：DSML 标记剥离**无条件生效**，thinking 块过滤与 index
    // 重编号**不生效**（保住零协议转换的透传语义）。改前入口整个被 `ds_cfg.is_some()`
    // 门控，这些号的 DSML 标记原样泄漏给客户端。

    /// 流式:门控关闭时 thinking 块原样透传(不滤、不重编号),但 DSML 标记仍被剥。
    #[tokio::test]
    async fn test_filter_sse_dsml_only_keeps_thinking_blocks() {
        let input = format!(
            "{}{}{}{}",
            event(
                "content_block_start",
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#
            ),
            event(
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"推理内容"}}"#
            ),
            event(
                "content_block_start",
                r#"{"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}"#
            ),
            event(
                "content_block_delta",
                r#"{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"<｜DSML｜function_calls｜>正文"}}"#
            ),
        );
        let stream = futures::stream::iter(vec![Ok::<Bytes, std::io::Error>(Bytes::from(input))]);
        let filtered = collect_filtered(filter_sse_stream_with(stream, false, false, None)).await;
        assert!(
            filtered.contains("thinking") && filtered.contains("推理内容"),
            "门控关闭:thinking 块必须原样透传,实际: {filtered}"
        );
        assert!(filtered.contains("正文"), "正文保留,实际: {filtered}");
        assert!(!filtered.contains("DSML"), "DSML 标记仍必须剥掉,实际: {filtered:?}");
        // index 不重编号：原始 0/1 保持。
        assert_eq!(extract_indices(&filtered), vec![0, 0, 1, 1], "门控关闭不得重编号");
    }

    /// 流式:门控开启时 thinking 块被滤 + 重编号(既有行为不回退)。
    #[tokio::test]
    async fn test_filter_sse_thinking_gate_on_still_filters_and_renumbers() {
        let input = format!(
            "{}{}",
            event(
                "content_block_start",
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#
            ),
            event(
                "content_block_start",
                r#"{"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}"#
            ),
        );
        let stream = futures::stream::iter(vec![Ok::<Bytes, std::io::Error>(Bytes::from(input))]);
        let filtered = collect_filtered(filter_sse_stream_with(stream, false, true, None)).await;
        assert!(!filtered.contains("thinking"), "门控开启:thinking 块被滤,实际: {filtered}");
        assert_eq!(extract_indices(&filtered), vec![0], "保留块重编号到 0");
    }

    /// 非流式:门控关闭时 thinking 块保留,DSML 仍被剥。
    #[test]
    fn test_filter_json_dsml_only_keeps_thinking_blocks() {
        let json = r#"{"content":[{"type":"thinking","thinking":"推理"},{"type":"text","text":"<｜DSML｜function_calls｜>正文"}]}"#;
        let filtered = filter_json_bytes_with(json.as_bytes(), false, false, None);
        let v: serde_json::Value = serde_json::from_slice(&filtered).unwrap();
        let content = v["content"].as_array().unwrap();
        assert_eq!(content.len(), 2, "门控关闭:thinking 块不得被滤");
        assert_eq!(content[0]["type"].as_str().unwrap(), "thinking");
        assert_eq!(content[1]["text"].as_str().unwrap(), "正文", "DSML 标记仍被剥");
    }

    /// 非流式:门控开启时 thinking 块被滤(既有行为不回退)。
    #[test]
    fn test_filter_json_thinking_gate_on_still_filters() {
        let json = r#"{"content":[{"type":"thinking","thinking":"推理"},{"type":"text","text":"正文"}]}"#;
        let filtered = filter_json_bytes_with(json.as_bytes(), false, true, None);
        let v: serde_json::Value = serde_json::from_slice(&filtered).unwrap();
        let content = v["content"].as_array().unwrap();
        assert_eq!(content.len(), 1, "门控开启:thinking 块被滤");
        assert_eq!(content[0]["type"].as_str().unwrap(), "text");
    }

    // ==================== 模拟缓存注入（透传路径 mockCacheEnabled）====================

    fn mock_json(input_tokens: u64, extra_usage: &str) -> String {
        format!(
            r#"{{"content":[{{"type":"text","text":"hi"}}],"usage":{{"input_tokens":{input_tokens},"output_tokens":9{extra_usage}}}}}"#
        )
    }

    /// 非流式：ratio=0.5 → cache_read = round(input×0.5)，creation 置 0。
    #[test]
    fn test_mock_cache_json_ratio_half() {
        let out = filter_json_bytes_with(
            mock_json(10, "").as_bytes(),
            true,
            true,
            Some((true, 0.5)),
        );
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["usage"]["cache_read_input_tokens"], 5, "10×0.5=5");
        assert_eq!(v["usage"]["cache_creation_input_tokens"], 0);
        // 既有 usage 字段必须保留（input/output 不被误改）
        assert_eq!(v["usage"]["input_tokens"], 10);
        assert_eq!(v["usage"]["output_tokens"], 9);
    }

    /// 非流式：ratio=1.0 → cache_read = input（100% 全命中）。
    #[test]
    fn test_mock_cache_json_ratio_one_reads_full_input() {
        let out = filter_json_bytes_with(
            mock_json(10, "").as_bytes(),
            true,
            true,
            Some((true, 1.0)),
        );
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["usage"]["cache_read_input_tokens"], 10);
        assert_eq!(v["usage"]["cache_creation_input_tokens"], 0);
    }

    /// 非流式：关闭（None）→ 原样透传（解析等价，键顺序可能被 serde_json 重排不算改动）。
    #[test]
    fn test_mock_cache_json_disabled_unchanged() {
        let input = mock_json(10, "");
        let out = filter_json_bytes_with(input.as_bytes(), true, true, None);
        let v_in: serde_json::Value = serde_json::from_str(&input).unwrap();
        let v_out: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(
            v_in, v_out,
            "关闭时必须零改动（usage 一个字段都不变，cache 字段不出现）"
        );
        assert!(
            v_out["usage"].get("cache_read_input_tokens").is_none(),
            "关闭时不得注入 cache_read"
        );
        // Some((false, _)) 与 None 同语义
        let out2 = filter_json_bytes_with(input.as_bytes(), true, true, Some((false, 0.7)));
        let v_out2: serde_json::Value = serde_json::from_slice(&out2).unwrap();
        assert_eq!(v_in, v_out2);
    }

    /// 非流式：usage 无 input_tokens → 整体不注入（cache 字段缺失而非置 0）。
    #[test]
    fn test_mock_cache_json_no_input_tokens_skips() {
        let json = r#"{"content":[{"type":"text","text":"hi"}],"usage":{"output_tokens":9}}"#;
        let out = filter_json_bytes_with(json.as_bytes(), true, true, Some((true, 0.5)));
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert!(
            v["usage"].get("cache_read_input_tokens").is_none(),
            "无 input_tokens 时不得注入 cache_read（字段缺失语义，见 prompt_cache 先例）"
        );
        assert!(v["usage"].get("cache_creation_input_tokens").is_none());
    }

    /// 非流式：上游自带的 creation（含 5m/1h 拆分键）被覆盖为 0，避免 read+creation > input。
    #[test]
    fn test_mock_cache_json_creation_overwritten_to_zero() {
        let json = r#"{"content":[{"type":"text","text":"hi"}],"usage":{"input_tokens":20,"output_tokens":1,"cache_read_input_tokens":3,"cache_creation_input_tokens":9,"cache_creation_5m_input_tokens":7,"cache_creation_1h_input_tokens":2}}"#;
        let out = filter_json_bytes_with(json.as_bytes(), true, true, Some((true, 1.0)));
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["usage"]["cache_read_input_tokens"], 20, "read 覆盖为 input×1.0");
        assert_eq!(v["usage"]["cache_creation_input_tokens"], 0, "creation 覆盖为 0");
        assert_eq!(v["usage"]["cache_creation_5m_input_tokens"], 0);
        assert_eq!(v["usage"]["cache_creation_1h_input_tokens"], 0);
        // 不变量：read + creation ≤ input
        let read = v["usage"]["cache_read_input_tokens"].as_u64().unwrap();
        let creation = v["usage"]["cache_creation_input_tokens"].as_u64().unwrap();
        assert!(read + creation <= 20, "read+creation 不得超过 input");
    }

    /// 非流式：**content 缺失** + mock 开启 → usage 仍被注入（回归 MAJOR-2：旧实现注入
    /// 代码在 content 早退之后，上游 2xx JSON 缺 content 字段时注入静默失效）。
    #[test]
    fn test_mock_cache_json_no_content_still_injects() {
        let json = r#"{"id":"msg_1","type":"message","role":"assistant","usage":{"input_tokens":100,"output_tokens":50}}"#;
        let out = filter_json_bytes_with(json.as_bytes(), true, true, Some((true, 0.5)));
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["usage"]["cache_read_input_tokens"], 50, "100×0.5=50 必须注入");
        assert_eq!(v["usage"]["cache_creation_input_tokens"], 0);
        assert_eq!(v["usage"]["input_tokens"], 100, "既有字段不得被误改");
        assert!(v.get("content").is_none(), "content 缺失保持缺失（注入不引入 content）");
    }

    /// 非流式：content 缺失 + mock 关闭 → 字节级原样（零改动，不做多余重序列化）。
    #[test]
    fn test_mock_cache_json_no_content_disabled_keeps_bytes() {
        let json = br#"{"id":"msg_1","usage":{"input_tokens":100,"output_tokens":50}}"#;
        let out = filter_json_bytes_with(json, true, true, None);
        assert_eq!(out.as_ref(), json, "mock 关闭 + content 缺失必须字节级原样");
        let out2 = filter_json_bytes_with(json, true, true, Some((false, 0.7)));
        assert_eq!(out2.as_ref(), json, "Some((false, _)) 与 None 同语义");
    }

    /// 非流式：解析失败 + mock 开启 → fail-open 原样返回（注入不注入、绝不破坏响应）。
    #[test]
    fn test_mock_cache_json_parse_failure_fail_open() {
        let garbage = b"{not valid json";
        let out = filter_json_bytes_with(garbage, true, true, Some((true, 0.5)));
        assert_eq!(out.as_ref(), garbage, "解析失败必须 fail-open 原样透传");
    }

    /// 流式：message_start 的 usage 注入（下游读 cache 分支的主入口）。
    ///
    /// ⚠️ 测试夹具用**真实 Anthropic 结构**：usage 嵌套在 `message` 内（顶层无 usage，
    /// 线上原始字节坐实）。旧夹具把 usage 放顶层，与真实结构不符 ⇒ 旧实现只读顶层
    /// usage 时测试全绿但线上注入恒 0（回归 MAJOR）。
    #[tokio::test]
    async fn test_mock_cache_sse_message_start_injected() {
        let input = format!(
            "{}",
            event(
                "message_start",
                r#"{"type":"message_start","message":{"id":"m1","usage":{"input_tokens":10,"output_tokens":0}}}"#
            )
        );
        let stream = futures::stream::iter(vec![Ok::<Bytes, std::io::Error>(Bytes::from(input))]);
        let filtered = collect_filtered(filter_sse_stream_with(stream, true, true, Some((true, 0.5))))
            .await;
        let data_line = filtered
            .lines()
            .find(|l| l.starts_with("data: "))
            .unwrap_or_else(|| panic!("过滤结果应有 data 行，实际: {filtered}"));
        let v: serde_json::Value =
            serde_json::from_str(data_line.strip_prefix("data: ").unwrap()).unwrap();
        assert_eq!(
            v["message"]["usage"]["cache_read_input_tokens"], 5,
            "message.usage 必须注入 read=round(10×0.5)=5，实际: {filtered}"
        );
        assert_eq!(v["message"]["usage"]["cache_creation_input_tokens"], 0);
        assert_eq!(v["message"]["usage"]["input_tokens"], 10, "既有字段不得被误改");
    }

    /// 流式：message_start 顶层 usage 的旧形态（无 message.usage）同样注入 —— 兼容回退分支。
    #[tokio::test]
    async fn test_mock_cache_sse_message_start_top_level_usage_fallback_injected() {
        let input = format!(
            "{}",
            event(
                "message_start",
                r#"{"type":"message_start","usage":{"input_tokens":10,"output_tokens":0}}"#
            )
        );
        let stream = futures::stream::iter(vec![Ok::<Bytes, std::io::Error>(Bytes::from(input))]);
        let filtered = collect_filtered(filter_sse_stream_with(stream, true, true, Some((true, 0.5))))
            .await;
        let data_line = filtered
            .lines()
            .find(|l| l.starts_with("data: "))
            .unwrap_or_else(|| panic!("过滤结果应有 data 行，实际: {filtered}"));
        let v: serde_json::Value =
            serde_json::from_str(data_line.strip_prefix("data: ").unwrap()).unwrap();
        assert_eq!(
            v["usage"]["cache_read_input_tokens"], 5,
            "顶层 usage 形态（回退分支）必须注入 read=5，实际: {filtered}"
        );
        assert_eq!(v["usage"]["cache_creation_input_tokens"], 0);
    }

    /// 流式：message_delta 的 usage（个别上游带 input_tokens）同样注入。
    #[tokio::test]
    async fn test_mock_cache_sse_message_delta_injected() {
        let input = format!(
            "{}",
            event(
                "message_delta",
                r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"input_tokens":10,"output_tokens":9}}"#
            )
        );
        let stream = futures::stream::iter(vec![Ok::<Bytes, std::io::Error>(Bytes::from(input))]);
        let filtered = collect_filtered(filter_sse_stream_with(stream, true, true, Some((true, 1.0))))
            .await;
        assert!(
            filtered.contains("\"cache_read_input_tokens\":10"),
            "message_delta usage 带 input_tokens 时必须注入，实际: {filtered}"
        );
    }

    /// 流式：无 input_tokens 的事件（真实 message_delta 只有 output_tokens）不注入。
    #[tokio::test]
    async fn test_mock_cache_sse_no_input_tokens_not_injected() {
        let input = format!(
            "{}{}",
            event(
                "message_start",
                r#"{"type":"message_start","message":{"id":"m1","usage":{"input_tokens":10,"output_tokens":0}}}"#
            ),
            event(
                "message_delta",
                r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":9}}"#
            )
        );
        let stream = futures::stream::iter(vec![Ok::<Bytes, std::io::Error>(Bytes::from(input))]);
        let filtered = collect_filtered(filter_sse_stream_with(stream, true, true, Some((true, 0.7))))
            .await;
        assert!(
            filtered.contains("\"cache_read_input_tokens\":7"),
            "message_start 注入 7"
        );
        // message_delta 只有 output_tokens：注入块跳过，output_tokens 保持原值
        assert!(filtered.contains("\"output_tokens\":9"));
        assert_eq!(
            filtered.matches("cache_read_input_tokens").count(),
            1,
            "cache_read_input_tokens 只能出现一次（message_delta 未注入），实际: {filtered}"
        );
    }

    /// 流式：关闭（None）→ message_start 原样透传（字节级零改动）。
    #[tokio::test]
    async fn test_mock_cache_sse_disabled_unchanged() {
        let input = format!(
            "{}",
            event(
                "message_start",
                r#"{"type":"message_start","usage":{"input_tokens":10,"output_tokens":0}}"#
            )
        );
        let stream =
            futures::stream::iter(vec![Ok::<Bytes, std::io::Error>(Bytes::from(input.clone()))]);
        let filtered = collect_filtered(filter_sse_stream_with(stream, true, true, None)).await;
        assert_eq!(filtered, input, "关闭时必须原样透传");
    }

    /// P1-11：filter 未改写的 `content_block_delta` / `message_delta` 回原字节
    /// （与 message_start 对齐；非字典序 key 不得被 serde_json 重排）。
    #[tokio::test]
    async fn test_sse_unchanged_delta_preserves_original_bytes() {
        let delta = event(
            "content_block_delta",
            r#"{"z":1,"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hello"}}"#,
        );
        let md = event(
            "message_delta",
            r#"{"z":1,"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":9}}"#,
        );
        let input = format!("{delta}{md}");
        let stream =
            futures::stream::iter(vec![Ok::<Bytes, std::io::Error>(Bytes::from(input.clone()))]);
        let filtered = collect_filtered(filter_sse_stream_with(stream, true, true, None)).await;
        assert_eq!(
            filtered, input,
            "未改写的 delta 必须回原字节（含非字典序 key），实际: {filtered}"
        );
    }
}
