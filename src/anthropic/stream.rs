//! 流式响应处理模块
//!
//! 实现 Kiro → Anthropic 流式响应转换和 SSE 状态管理

use std::collections::HashMap;

use serde_json::json;
use uuid::Uuid;

use crate::kiro::model::events::Event;
use crate::usage::RequestOutcome;

/// 一次响应的**完成状态**，贯穿流式 / 缓冲 / 非流式三条收尾路径。
///
/// # 为什么需要它
///
/// 历史 BUG：上游在流中途发来 in-band `Event::Error`、或读流/解码中断时，收尾逻辑
/// 仍按 `message_stop` + HTTP 200 正常结束，用量埋点也硬编码 `outcome=Success`。
/// 下游 Claude Code 收到 200 + `end_turn` 就把**截断输出当成功**，既不重试、又污染
/// 熔断/健康信号（失败被记成成功）。
///
/// `CompletionStatus` 把「这次到底成没成」显式建模，收尾时据此统一决定三件事：
/// - 用量记账的 [`RequestOutcome`]（RateLimited / ServerError / NetworkError…）
/// - 回给客户端的 SSE `error` 事件类型（overloaded_error / api_error）
/// - 非流式响应的 HTTP 状态码（429 / 502）
///
/// # 铁律
///
/// `ContentLengthExceededException`（= max_tokens 干净收尾）**不是**失败，
/// 不设置任何非 `Ok` 状态；它照常走 message_stop + 200。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompletionStatus {
    /// 正常完成（含 max_tokens 干净收尾）
    Ok,
    /// 上游在响应流中 in-band 下发的错误事件（`:message-type=error`）
    UpstreamError { code: String, message: String },
    /// 读流 / 读响应体传输中断（未拿到完整响应）
    TransportError { message: String },
    /// 解码器连续错误超限、永久停止（响应必然截断）
    DecoderStopped { message: String },
}

impl CompletionStatus {
    /// 是否正常完成
    pub fn is_ok(&self) -> bool {
        matches!(self, CompletionStatus::Ok)
    }

    /// 映射为用量记账的最终结果分类。
    ///
    /// 上游错误按 code/message 关键字粗分：限流类 → `RateLimited`，其余 → `ServerError`；
    /// 传输中断 → `NetworkError`；解码器停止 → `ServerError`（响应被上游截断）。
    pub fn outcome(&self) -> RequestOutcome {
        match self {
            CompletionStatus::Ok => RequestOutcome::Success,
            CompletionStatus::UpstreamError { code, message } => {
                if is_rate_limit_signal(code) || is_rate_limit_signal(message) {
                    RequestOutcome::RateLimited
                } else {
                    RequestOutcome::ServerError
                }
            }
            CompletionStatus::TransportError { .. } => RequestOutcome::NetworkError,
            CompletionStatus::DecoderStopped { .. } => RequestOutcome::ServerError,
        }
    }

    /// 回给客户端的 SSE `error` 事件的 `type` 字段。
    ///
    /// 限流类用 `overloaded_error`（Claude Code 会按过载退避重试），其余用 `api_error`。
    pub fn sse_error_type(&self) -> &'static str {
        match self {
            CompletionStatus::Ok => "api_error",
            CompletionStatus::UpstreamError { code, message } => {
                if is_rate_limit_signal(code) || is_rate_limit_signal(message) {
                    "overloaded_error"
                } else {
                    "api_error"
                }
            }
            CompletionStatus::TransportError { .. } => "api_error",
            CompletionStatus::DecoderStopped { .. } => "api_error",
        }
    }

    /// 非流式响应的 HTTP 状态码：限流 429，其余 502。
    pub fn http_status_u16(&self) -> u16 {
        match self.outcome() {
            RequestOutcome::RateLimited => 429,
            _ => 502,
        }
    }

    /// 面向客户端的错误描述（用于 SSE error 事件 / 非流式错误体）。
    pub fn client_message(&self) -> String {
        match self {
            CompletionStatus::Ok => String::new(),
            CompletionStatus::UpstreamError { code, message } => {
                if message.is_empty() {
                    format!("上游返回错误: {}", code)
                } else {
                    format!("上游返回错误: {} - {}", code, message)
                }
            }
            CompletionStatus::TransportError { message } => {
                format!("上游响应流中断: {}", message)
            }
            CompletionStatus::DecoderStopped { message } => {
                format!("上游响应解析中断: {}", message)
            }
        }
    }
}

/// 判断错误 code/message 是否属于「限流/过载」信号（大小写不敏感的关键字匹配）。
fn is_rate_limit_signal(s: &str) -> bool {
    let lower = s.to_ascii_lowercase();
    lower.contains("throttl")
        || lower.contains("toomanyrequests")
        || lower.contains("too many requests")
        || lower.contains("ratelimit")
        || lower.contains("rate limit")
        || lower.contains("429")
        || lower.contains("overload")
        || lower.contains("quota")
        || lower.contains("exhaust")
}

/// thinking 块的 signature 占位字符串。
///
/// Anthropic 协议下，流式 `{type:"thinking"}` 块结束前必须发一个 `signature_delta`
/// 事件，SDK 会把它聚合进 thinking 块的 `signature` 字段。客户端（Claude Code）在
/// 下一轮把该 assistant 消息回传时会本地校验 thinking 块必须带**非空** signature，
/// 否则抛出 `The content[].thinking in the thinking mode must be passed back to the API`。
///
/// 上游 Kiro 不是 Anthropic 服务端，不下发真实签名，因此这里发一个非空占位字符串以
/// 满足客户端本地校验。该占位符只在客户端 ↔ KiroStudio 之间存在：回传时 converter 只读
/// `block.thinking`，`ContentBlock` 无 signature 字段且未 deny_unknown_fields，serde
/// 静默丢弃客户端回传的假签名，故永不转发给 Kiro。
pub(super) const THINKING_SIGNATURE_PLACEHOLDER: &str = "kirostudio-thinking-signature";

/// Prompt 缓存记账明细（历史：由影子缓存 tracker 推算注入响应 usage）。
///
/// 影子缓存记账已整体移除（不省钱且在大请求热路径同步跑 SHA256 拖慢传输）。此类型与
/// StreamContext 里的 `cache_usage` 字段现恒为 `None`，不再有任何计算路径写入它——保留为
/// 惰性载体避免改动流式收尾热路径的十余处读点（那会引入无收益的回归风险）。
#[allow(dead_code)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct CacheUsageBreakdown {
    pub cache_creation_input_tokens: i32,
    pub cache_read_input_tokens: i32,
    pub cache_creation_5m_input_tokens: i32,
    pub cache_creation_1h_input_tokens: i32,
}

/// 将总输入 token 转为 Anthropic usage 的 input_tokens 口径（剔除 cache 读写）
///
/// Anthropic 语义：`usage.input_tokens` 只计「未命中缓存、非本次新建缓存」的部分，
/// cache_read / cache_creation 单独列出。
#[allow(dead_code)] // 影子缓存记账移除后仅由恒 None 的 cache_usage 分支引用
pub(crate) fn billed_input_tokens(
    input_tokens: i32,
    cache_creation_input_tokens: i32,
    cache_read_input_tokens: i32,
) -> i32 {
    input_tokens
        .saturating_sub(cache_creation_input_tokens)
        .saturating_sub(cache_read_input_tokens)
        .max(0)
}

/// 找到小于等于目标位置的最近有效UTF-8字符边界
///
/// UTF-8字符可能占用1-4个字节，直接按字节位置切片可能会切在多字节字符中间导致panic。
/// 这个函数从目标位置向前搜索，找到最近的有效字符边界。
fn find_char_boundary(s: &str, target: usize) -> usize {
    if target >= s.len() {
        return s.len();
    }
    if target == 0 {
        return 0;
    }
    // 从目标位置向前搜索有效的字符边界
    let mut pos = target;
    while pos > 0 && !s.is_char_boundary(pos) {
        pos -= 1;
    }
    pos
}

/// 需要跳过的包裹字符
///
/// 当 thinking 标签被这些字符包裹时，认为是在引用标签而非真正的标签：
/// - 反引号 (`)：行内代码
/// - 双引号 (")：字符串
/// - 单引号 (')：字符串
const QUOTE_CHARS: &[u8] = &[
    b'`', b'"', b'\'', b'\\', b'#', b'!', b'@', b'$', b'%', b'^', b'&', b'*', b'(', b')', b'-',
    b'_', b'=', b'+', b'[', b']', b'{', b'}', b';', b':', b'<', b'>', b',', b'.', b'?', b'/',
];

/// 检查指定位置的字符是否是引用字符
fn is_quote_char(buffer: &str, pos: usize) -> bool {
    buffer
        .as_bytes()
        .get(pos)
        .map(|c| QUOTE_CHARS.contains(c))
        .unwrap_or(false)
}

/// 查找真正的 thinking 结束标签（不被引用字符包裹，且后面有双换行符）
///
/// 当模型在思考过程中提到 `</thinking>` 时，通常会用反引号、引号等包裹，
/// 或者在同一行有其他内容（如"关于 </thinking> 标签"）。
/// 这个函数会跳过这些情况，只返回真正的结束标签位置。
///
/// 跳过的情况：
/// - 被引用字符包裹（反引号、引号等）
/// - 后面没有双换行符（真正的结束标签后面会有 `\n\n`）
/// - 标签在缓冲区末尾（流式处理时需要等待更多内容）
///
/// # 参数
/// - `buffer`: 要搜索的字符串
///
/// # 返回值
/// - `Some(pos)`: 真正的结束标签的起始位置
/// - `None`: 没有找到真正的结束标签
fn find_real_thinking_end_tag(buffer: &str) -> Option<usize> {
    const TAG: &str = "</thinking>";
    let mut search_start = 0;

    while let Some(pos) = buffer[search_start..].find(TAG) {
        let absolute_pos = search_start + pos;

        // 检查前面是否有引用字符
        let has_quote_before = absolute_pos > 0 && is_quote_char(buffer, absolute_pos - 1);

        // 检查后面是否有引用字符
        let after_pos = absolute_pos + TAG.len();
        let has_quote_after = is_quote_char(buffer, after_pos);

        // 如果被引用字符包裹，跳过
        if has_quote_before || has_quote_after {
            search_start = absolute_pos + 1;
            continue;
        }

        // 检查后面的内容
        let after_content = &buffer[after_pos..];

        // 如果标签后面内容不足以判断是否有双换行符，等待更多内容
        if after_content.len() < 2 {
            return None;
        }

        // 真正的 thinking 结束标签后面会有双换行符 `\n\n`
        if after_content.starts_with("\n\n") {
            return Some(absolute_pos);
        }

        // 不是双换行符，跳过继续搜索
        search_start = absolute_pos + 1;
    }

    None
}

/// 查找缓冲区末尾的 thinking 结束标签（允许末尾只有空白字符）
///
/// 用于“边界事件”场景：例如 thinking 结束后立刻进入 tool_use，或流结束，
/// 此时 `</thinking>` 后面可能没有 `\n\n`，但结束标签依然应被识别并过滤。
///
/// 约束：只有当 `</thinking>` 之后全部都是空白字符时才认为是结束标签，
/// 以避免在 thinking 内容中提到 `</thinking>`（非结束标签）时误判。
fn find_real_thinking_end_tag_at_buffer_end(buffer: &str) -> Option<usize> {
    const TAG: &str = "</thinking>";
    let mut search_start = 0;

    while let Some(pos) = buffer[search_start..].find(TAG) {
        let absolute_pos = search_start + pos;

        // 检查前面是否有引用字符
        let has_quote_before = absolute_pos > 0 && is_quote_char(buffer, absolute_pos - 1);

        // 检查后面是否有引用字符
        let after_pos = absolute_pos + TAG.len();
        let has_quote_after = is_quote_char(buffer, after_pos);

        if has_quote_before || has_quote_after {
            search_start = absolute_pos + 1;
            continue;
        }

        // 只有当标签后面全部是空白字符时才认定为结束标签
        if buffer[after_pos..].trim().is_empty() {
            return Some(absolute_pos);
        }

        search_start = absolute_pos + 1;
    }

    None
}

/// 查找真正的 thinking 开始标签（不被引用字符包裹）
///
/// 与 `find_real_thinking_end_tag` 类似，跳过被引用字符包裹的开始标签。
fn find_real_thinking_start_tag(buffer: &str) -> Option<usize> {
    const TAG: &str = "<thinking>";
    let mut search_start = 0;

    while let Some(pos) = buffer[search_start..].find(TAG) {
        let absolute_pos = search_start + pos;

        // 检查前面是否有引用字符
        let has_quote_before = absolute_pos > 0 && is_quote_char(buffer, absolute_pos - 1);

        // 检查后面是否有引用字符
        let after_pos = absolute_pos + TAG.len();
        let has_quote_after = is_quote_char(buffer, after_pos);

        // 如果不被引用字符包裹，则是真正的开始标签
        if !has_quote_before && !has_quote_after {
            return Some(absolute_pos);
        }

        // 继续搜索下一个匹配
        search_start = absolute_pos + 1;
    }

    None
}

/// 从完整文本中提取 thinking 块（用于非流式响应）
///
/// 使用与流式处理相同的标签检测逻辑（引用字符过滤），确保一致性。
/// 非流式场景下文本已完整，无需处理跨 chunk 分割问题。
///
/// # 返回值
/// - `(Some(thinking_content), remaining_text)` — 检测到有效 thinking 块
/// - `(None, original_text)` — 未检测到，原样返回
pub(crate) fn extract_thinking_from_complete_text(text: &str) -> (Option<String>, String) {
    let start_pos = match find_real_thinking_start_tag(text) {
        Some(pos) => pos,
        None => return (None, text.to_string()),
    };

    let before = &text[..start_pos];
    let after_open = &text[start_pos + "<thinking>".len()..];

    // 查找结束标签：优先匹配带 \n\n 后缀的，退而使用末尾匹配
    let (thinking_raw, text_after) =
        if let Some(end_pos) = find_real_thinking_end_tag(after_open) {
            (
                &after_open[..end_pos],
                &after_open[end_pos + "</thinking>\n\n".len()..],
            )
        } else if let Some(end_pos) = find_real_thinking_end_tag_at_buffer_end(after_open) {
            let after_tag = end_pos + "</thinking>".len();
            (
                &after_open[..end_pos],
                after_open[after_tag..].trim_start(),
            )
        } else {
            // 找不到有效的结束标签，不做提取
            return (None, text.to_string());
        };

    // 剥离开头的换行符（与流式处理一致：模型输出 <thinking>\n）
    let thinking_content = thinking_raw
        .strip_prefix('\n')
        .unwrap_or(thinking_raw);

    // 组装剩余文本：跳过纯空白的 before 部分
    let mut remaining = String::new();
    if !before.trim().is_empty() {
        remaining.push_str(before);
    }
    remaining.push_str(text_after);

    if thinking_content.is_empty() {
        (None, remaining)
    } else {
        (Some(thinking_content.to_string()), remaining)
    }
}

/// SSE 事件
#[derive(Debug, Clone)]
pub struct SseEvent {
    pub event: String,
    pub data: serde_json::Value,
}

impl SseEvent {
    pub fn new(event: impl Into<String>, data: serde_json::Value) -> Self {
        Self {
            event: event.into(),
            data,
        }
    }

    /// 格式化为 SSE 字符串
    pub fn to_sse_string(&self) -> String {
        format!(
            "event: {}\ndata: {}\n\n",
            self.event,
            serde_json::to_string(&self.data).unwrap_or_default()
        )
    }

    /// 构造 Anthropic 规范的 SSE `error` 事件。
    ///
    /// 上游流中途失败(读流 Err)时用它显式告知客户端"本次响应未正常完成"，
    /// 而非把截断的输出当作 message_stop 正常收尾——后者会让 Claude Code 把半截结果当成功，
    /// 不触发重试。发了 error 事件，客户端(Claude Code)才会按 overloaded/api_error 退避重试。
    /// 形如 `{"type":"error","error":{"type":"overloaded_error","message":"..."}}`。
    pub fn error_event(error_type: &str, message: impl Into<String>) -> Self {
        Self::new(
            "error",
            serde_json::json!({
                "type": "error",
                "error": { "type": error_type, "message": message.into() },
            }),
        )
    }
}

/// 内容块状态
#[derive(Debug, Clone)]
struct BlockState {
    block_type: String,
    started: bool,
    stopped: bool,
}

impl BlockState {
    fn new(block_type: impl Into<String>) -> Self {
        Self {
            block_type: block_type.into(),
            started: false,
            stopped: false,
        }
    }
}

/// SSE 状态管理器
///
/// 确保 SSE 事件序列符合 Claude API 规范：
/// 1. message_start 只能出现一次
/// 2. content_block 必须先 start 再 delta 再 stop
/// 3. message_delta 只能出现一次，且在所有 content_block_stop 之后
/// 4. message_stop 在最后
#[derive(Debug)]
pub struct SseStateManager {
    /// message_start 是否已发送
    message_started: bool,
    /// message_delta 是否已发送
    message_delta_sent: bool,
    /// 活跃的内容块状态
    active_blocks: HashMap<i32, BlockState>,
    /// 消息是否已结束
    message_ended: bool,
    /// 下一个块索引
    next_block_index: i32,
    /// 当前 stop_reason
    stop_reason: Option<String>,
    /// 是否有工具调用
    has_tool_use: bool,
}

impl Default for SseStateManager {
    fn default() -> Self {
        Self::new()
    }
}

impl SseStateManager {
    pub fn new() -> Self {
        Self {
            message_started: false,
            message_delta_sent: false,
            active_blocks: HashMap::new(),
            message_ended: false,
            next_block_index: 0,
            stop_reason: None,
            has_tool_use: false,
        }
    }

    /// 判断指定块是否处于可接收 delta 的打开状态
    fn is_block_open_of_type(&self, index: i32, expected_type: &str) -> bool {
        self.active_blocks
            .get(&index)
            .is_some_and(|b| b.started && !b.stopped && b.block_type == expected_type)
    }

    /// 获取下一个块索引
    pub fn next_block_index(&mut self) -> i32 {
        let index = self.next_block_index;
        self.next_block_index += 1;
        index
    }

    /// 记录工具调用
    pub fn set_has_tool_use(&mut self, has: bool) {
        self.has_tool_use = has;
    }

    /// 设置 stop_reason
    pub fn set_stop_reason(&mut self, reason: impl Into<String>) {
        self.stop_reason = Some(reason.into());
    }

    /// 检查是否存在非 thinking 类型的内容块（如 text 或 tool_use）
    fn has_non_thinking_blocks(&self) -> bool {
        self.active_blocks
            .values()
            .any(|b| b.block_type != "thinking")
    }

    /// 获取最终的 stop_reason
    pub fn get_stop_reason(&self) -> String {
        if let Some(ref reason) = self.stop_reason {
            reason.clone()
        } else if self.has_tool_use {
            "tool_use".to_string()
        } else {
            "end_turn".to_string()
        }
    }

    /// 处理 message_start 事件
    pub fn handle_message_start(&mut self, event: serde_json::Value) -> Option<SseEvent> {
        if self.message_started {
            tracing::debug!("跳过重复的 message_start 事件");
            return None;
        }
        self.message_started = true;
        Some(SseEvent::new("message_start", event))
    }

    /// 处理 content_block_start 事件
    pub fn handle_content_block_start(
        &mut self,
        index: i32,
        block_type: &str,
        data: serde_json::Value,
    ) -> Vec<SseEvent> {
        let mut events = Vec::new();

        // 如果是 tool_use 块，先关闭之前的文本块
        if block_type == "tool_use" {
            self.has_tool_use = true;
            for (block_index, block) in self.active_blocks.iter_mut() {
                if block.block_type == "text" && block.started && !block.stopped {
                    // 自动发送 content_block_stop 关闭文本块
                    events.push(SseEvent::new(
                        "content_block_stop",
                        json!({
                            "type": "content_block_stop",
                            "index": block_index
                        }),
                    ));
                    block.stopped = true;
                }
            }
        }

        // 检查块是否已存在
        if let Some(block) = self.active_blocks.get_mut(&index) {
            if block.started {
                tracing::debug!("块 {} 已启动，跳过重复的 content_block_start", index);
                return events;
            }
            block.started = true;
        } else {
            let mut block = BlockState::new(block_type);
            block.started = true;
            self.active_blocks.insert(index, block);
        }

        events.push(SseEvent::new("content_block_start", data));
        events
    }

    /// 处理 content_block_delta 事件
    pub fn handle_content_block_delta(
        &mut self,
        index: i32,
        data: serde_json::Value,
    ) -> Option<SseEvent> {
        // 确保块已启动
        if let Some(block) = self.active_blocks.get(&index) {
            if !block.started || block.stopped {
                tracing::warn!(
                    "块 {} 状态异常: started={}, stopped={}",
                    index,
                    block.started,
                    block.stopped
                );
                return None;
            }
        } else {
            // 块不存在，可能需要先创建
            tracing::warn!("收到未知块 {} 的 delta 事件", index);
            return None;
        }

        Some(SseEvent::new("content_block_delta", data))
    }

    /// 处理 content_block_stop 事件
    pub fn handle_content_block_stop(&mut self, index: i32) -> Option<SseEvent> {
        if let Some(block) = self.active_blocks.get_mut(&index) {
            if block.stopped {
                tracing::debug!("块 {} 已停止，跳过重复的 content_block_stop", index);
                return None;
            }
            block.stopped = true;
            return Some(SseEvent::new(
                "content_block_stop",
                json!({
                    "type": "content_block_stop",
                    "index": index
                }),
            ));
        }
        None
    }

    /// 生成最终事件序列
    ///
    /// `input_tokens` 已是 billed 口径（剔除 cache 读写）；`cache_usage` 存在时
    /// 额外注入 cache_read / cache_creation 字段。
    pub fn generate_final_events(
        &mut self,
        input_tokens: i32,
        output_tokens: i32,
        cache_usage: Option<CacheUsageBreakdown>,
    ) -> Vec<SseEvent> {
        let mut events = Vec::new();

        // 关闭所有未关闭的块
        for (index, block) in self.active_blocks.iter_mut() {
            if block.started && !block.stopped {
                events.push(SseEvent::new(
                    "content_block_stop",
                    json!({
                        "type": "content_block_stop",
                        "index": index
                    }),
                ));
                block.stopped = true;
            }
        }

        // 发送 message_delta
        if !self.message_delta_sent {
            self.message_delta_sent = true;
            let mut usage_json = json!({
                "input_tokens": input_tokens,
                "output_tokens": output_tokens
            });
            if let Some(cache_usage) = cache_usage {
                usage_json["cache_creation_input_tokens"] =
                    json!(cache_usage.cache_creation_input_tokens);
                usage_json["cache_read_input_tokens"] =
                    json!(cache_usage.cache_read_input_tokens);
                usage_json["cache_creation"] = json!({
                    "ephemeral_5m_input_tokens": cache_usage.cache_creation_5m_input_tokens,
                    "ephemeral_1h_input_tokens": cache_usage.cache_creation_1h_input_tokens
                });
            }
            events.push(SseEvent::new(
                "message_delta",
                json!({
                    "type": "message_delta",
                    "delta": {
                        "stop_reason": self.get_stop_reason(),
                        "stop_sequence": null
                    },
                    "usage": usage_json
                }),
            ));
        }

        // 发送 message_stop
        if !self.message_ended {
            self.message_ended = true;
            events.push(SseEvent::new(
                "message_stop",
                json!({ "type": "message_stop" }),
            ));
        }

        events
    }
}

use super::converter::get_context_window_size;

/// 流处理上下文
/// 一次请求解析出的最终用量快照（供用量统计埋点消费）
#[derive(Debug, Clone, Copy)]
pub struct ResolvedUsage {
    /// 输入 tokens（优先精确值，回退估算）
    pub input_tokens: i32,
    /// 输出 tokens
    pub output_tokens: i32,
    /// 上游返回的真实 credit 消耗量（无 meteringEvent 时为 None）
    pub credits_used: Option<f64>,
    /// 本次命中缓存读取的 tokens（无缓存记账时为 0）
    pub cache_read_tokens: i32,
    /// 本次新建缓存写入的 tokens（无缓存记账时为 0）
    pub cache_creation_tokens: i32,
}

pub struct StreamContext {
    /// SSE 状态管理器
    pub state_manager: SseStateManager,
    /// 请求的模型名称
    pub model: String,
    /// 消息 ID
    pub message_id: String,
    /// 输入 tokens（估算值，未剔除 cache）
    pub input_tokens: i32,
    /// prompt 缓存记账明细（可选，注入响应 usage）
    pub cache_usage: Option<CacheUsageBreakdown>,
    /// 从 contextUsageEvent 计算的实际输入 tokens
    pub context_input_tokens: Option<i32>,
    /// 输出 tokens 累计
    pub output_tokens: i32,
    /// 从 meteringEvent 解析的真实 credit 消耗量（上游给出，token 估算无法替代）
    pub credits_used: Option<f64>,
    /// 工具块索引映射 (tool_id -> block_index)
    pub tool_block_indices: HashMap<String, i32>,
    /// 每个 tool_use_id 已经转发给客户端的 input JSON 累计内容。
    ///
    /// 用于修复 `Invalid tool parameters`：Kiro 的 `ToolUseEvent.input` 在同一 tool_use_id 上
    /// **可能是累积快照**（每帧带"到目前为止的完整 JSON"）而非纯增量。若原样把每帧当
    /// `input_json_delta` 转发，Claude Code 会把累积片段再拼一次 → JSON 重复损坏 → 报错。
    /// 这里记录已发内容，转发前做前缀检测：累积则只发差量，纯增量则原样发（自适应两种上游行为）。
    tool_input_sent: HashMap<String, String>,
    /// 工具名称反向映射（短名称 → 原始名称），用于响应时还原
    pub tool_name_map: HashMap<String, String>,
    /// thinking 是否启用
    pub thinking_enabled: bool,
    /// thinking 内容缓冲区
    pub thinking_buffer: String,
    /// 是否在 thinking 块内
    pub in_thinking_block: bool,
    /// thinking 块是否已提取完成
    pub thinking_extracted: bool,
    /// thinking 块索引
    pub thinking_block_index: Option<i32>,
    /// 文本块索引（thinking 启用时动态分配）
    pub text_block_index: Option<i32>,
    /// 是否需要剥离 thinking 内容开头的换行符
    /// 模型输出 `<thinking>\n` 时，`\n` 可能与标签在同一 chunk 或下一 chunk
    strip_thinking_leading_newline: bool,
    /// DSML 标记跨 chunk 探测缓冲:保留可能是半个 DeepSeek 工具标记(如 `<｜DSML` / `<｜tool▁`)
    /// 的文本尾巴,等下一个 chunk 拼上再判定,避免标记被从中间切开导致漏字或漏标记。详见 strip_dsml_markers。
    dsml_tail_buffer: String,
    /// tail 里的残留是否**已确认为 DSML 关键字标记**(而非"不确定的正文半标记")。
    /// true=流结束 flush 时丢弃(标记噪音,不补发,否则 `<｜DSML…` 会当正文泄漏);
    /// false=flush 时作普通文本补发(被误判的正文/末尾孤立 `<`,不吞字)。
    dsml_tail_is_marker: bool,
    /// 本次响应的完成状态（收尾时据此决定 outcome / SSE error / HTTP 码）。
    /// 默认 `Ok`；in-band `Event::Error` / 传输中断 / 解码器停止时被置为对应失败态。
    completion: CompletionStatus,
    /// 是否已向客户端内联发过 SSE `error` 事件。
    /// in-band `Event::Error`（及非 max_tokens 的 Exception）会在事件流中就地补发，
    /// 收尾逻辑据此避免重复补发同一个 error 事件。
    error_event_emitted: bool,
}

impl StreamContext {
    /// 创建启用thinking的StreamContext
    pub fn new_with_thinking(
        model: impl Into<String>,
        input_tokens: i32,
        thinking_enabled: bool,
        tool_name_map: HashMap<String, String>,
    ) -> Self {
        Self {
            state_manager: SseStateManager::new(),
            model: model.into(),
            message_id: format!("msg_{}", Uuid::new_v4().to_string().replace('-', "")),
            input_tokens,
            cache_usage: None,
            context_input_tokens: None,
            output_tokens: 0,
            credits_used: None,
            tool_block_indices: HashMap::new(),
            tool_input_sent: HashMap::new(),
            tool_name_map,
            thinking_enabled,
            thinking_buffer: String::new(),
            in_thinking_block: false,
            thinking_extracted: false,
            thinking_block_index: None,
            text_block_index: None,
            strip_thinking_leading_newline: false,
            dsml_tail_buffer: String::new(),
            dsml_tail_is_marker: false,
            completion: CompletionStatus::Ok,
            error_event_emitted: false,
        }
    }

    /// 设置 prompt 缓存记账明细（影子缓存已移除，现无调用方；保留避免改流式热路径）
    #[allow(dead_code)]
    pub fn set_cache_usage(&mut self, cache_usage: Option<CacheUsageBreakdown>) {
        self.cache_usage = cache_usage;
    }

    /// 生成 message_start 事件
    ///
    /// `input_tokens` 采用 billed 口径（剔除 cache 读写），并在有缓存记账时
    /// 注入 cache_read / cache_creation 字段。
    pub fn create_message_start_event(&self) -> serde_json::Value {
        let billed = self
            .cache_usage
            .map(|c| {
                billed_input_tokens(
                    self.input_tokens,
                    c.cache_creation_input_tokens,
                    c.cache_read_input_tokens,
                )
            })
            .unwrap_or(self.input_tokens);
        let mut usage = json!({
            "input_tokens": billed,
            "output_tokens": 1
        });
        if let Some(c) = self.cache_usage {
            usage["cache_creation_input_tokens"] = json!(c.cache_creation_input_tokens);
            usage["cache_read_input_tokens"] = json!(c.cache_read_input_tokens);
        }
        json!({
            "type": "message_start",
            "message": {
                "id": self.message_id,
                "type": "message",
                "role": "assistant",
                "content": [],
                "model": self.model,
                "stop_reason": null,
                "stop_sequence": null,
                "usage": usage
            }
        })
    }

    /// 生成初始事件序列 (message_start + 文本块 start)
    ///
    /// 当 thinking 启用时，不在初始化时创建文本块，而是等到实际收到内容时再创建。
    /// 这样可以确保 thinking 块（索引 0）在文本块（索引 1）之前。
    pub fn generate_initial_events(&mut self) -> Vec<SseEvent> {
        let mut events = Vec::new();

        // message_start
        let msg_start = self.create_message_start_event();
        if let Some(event) = self.state_manager.handle_message_start(msg_start) {
            events.push(event);
        }

        // 如果启用了 thinking，不在这里创建文本块
        // thinking 块和文本块会在 process_content_with_thinking 中按正确顺序创建
        if self.thinking_enabled {
            return events;
        }

        // 创建初始文本块（仅在未启用 thinking 时）
        let text_block_index = self.state_manager.next_block_index();
        self.text_block_index = Some(text_block_index);
        let text_block_events = self.state_manager.handle_content_block_start(
            text_block_index,
            "text",
            json!({
                "type": "content_block_start",
                "index": text_block_index,
                "content_block": {
                    "type": "text",
                    "text": ""
                }
            }),
        );
        events.extend(text_block_events);

        events
    }

    /// 处理 Kiro 事件并转换为 Anthropic SSE 事件
    pub fn process_kiro_event(&mut self, event: &Event) -> Vec<SseEvent> {
        match event {
            Event::AssistantResponse(resp) => self.process_assistant_response(&resp.content),
            Event::ToolUse(tool_use) => self.process_tool_use(tool_use),
            Event::ContextUsage(context_usage) => {
                // 从上下文使用百分比计算实际的 input_tokens
                let window_size = get_context_window_size(&self.model);
                let actual_input_tokens = (context_usage.context_usage_percentage
                    * (window_size as f64)
                    / 100.0) as i32;
                self.context_input_tokens = Some(actual_input_tokens);
                // 上下文使用量达到 100% 时，设置 stop_reason 为 model_context_window_exceeded
                if context_usage.context_usage_percentage >= 100.0 {
                    self.state_manager
                        .set_stop_reason("model_context_window_exceeded");
                }
                tracing::debug!(
                    "收到 contextUsageEvent: {}%, 计算 input_tokens: {}",
                    context_usage.context_usage_percentage,
                    actual_input_tokens
                );
                Vec::new()
            }
            Event::Error {
                error_code,
                error_message,
            } => {
                tracing::error!("收到 in-band 错误事件: {} - {}", error_code, error_message);
                // 记录完成状态为上游错误：收尾时据此把 outcome 记成失败、非流式返回非 200。
                // 幂等：只在首个错误落定，后续错误不覆盖（保留首因）。
                if self.completion.is_ok() {
                    self.completion = CompletionStatus::UpstreamError {
                        code: error_code.clone(),
                        message: error_message.clone(),
                    };
                }
                // in-band 错误已发生，立即内联发一个 SSE error 事件显式告知客户端
                // “本次响应未正常完成”，避免截断输出被当作 message_stop 正常收尾。
                // 标记已发，收尾路径据此不重复补发。
                self.error_event_emitted = true;
                vec![SseEvent::error_event(
                    self.completion.sse_error_type(),
                    self.completion.client_message(),
                )]
            }
            Event::Metering(metering) => {
                // 记录上游返回的真实 credit 消耗量（累加，兼容单请求多次计费事件）
                self.credits_used =
                    Some(self.credits_used.unwrap_or(0.0) + metering.usage);
                tracing::debug!(
                    "收到 meteringEvent: {} {}",
                    metering.usage,
                    metering.unit
                );
                Vec::new()
            }
            Event::Exception {
                exception_type,
                message,
            } => {
                // 铁律：ContentLengthExceededException = max_tokens 干净收尾，绝不算失败。
                // 它是模型正常耗尽输出预算，照常走 message_stop + 200。
                if exception_type == "ContentLengthExceededException" {
                    self.state_manager.set_stop_reason("max_tokens");
                    tracing::warn!("收到 ContentLengthExceededException：按 max_tokens 干净收尾");
                    return Vec::new();
                }
                // 其它异常是上游真实失败，等同 in-band 错误处理：置失败态 + 内联发 error 事件。
                tracing::error!("收到 in-band 异常事件: {} - {}", exception_type, message);
                if self.completion.is_ok() {
                    self.completion = CompletionStatus::UpstreamError {
                        code: exception_type.clone(),
                        message: message.clone(),
                    };
                }
                self.error_event_emitted = true;
                vec![SseEvent::error_event(
                    self.completion.sse_error_type(),
                    self.completion.client_message(),
                )]
            }
            _ => Vec::new(),
        }
    }

    /// 返回本次请求已解析出的最终用量（供统计埋点使用）
    ///
    /// - `input_tokens` 优先用 contextUsageEvent 计算的精确值，回退到估算
    /// - `output_tokens` 为流式累计
    /// - `credits_used` 为 meteringEvent 的真实计费量（可能为 None）
    pub fn resolved_usage(&self) -> ResolvedUsage {
        ResolvedUsage {
            input_tokens: self.context_input_tokens.unwrap_or(self.input_tokens),
            output_tokens: self.output_tokens,
            credits_used: self.credits_used,
            cache_read_tokens: self.cache_usage.map(|c| c.cache_read_input_tokens).unwrap_or(0),
            cache_creation_tokens: self
                .cache_usage
                .map(|c| c.cache_creation_input_tokens)
                .unwrap_or(0),
        }
    }

    /// 本次响应的完成状态（收尾时读取以决定 outcome / HTTP 码）
    pub fn completion(&self) -> &CompletionStatus {
        &self.completion
    }

    /// 用量记账应采用的最终结果分类（去掉硬编码 Success，改读真实完成状态）
    pub fn completion_outcome(&self) -> RequestOutcome {
        self.completion.outcome()
    }

    /// 是否已向客户端内联发过 SSE error 事件（收尾据此避免重复补发）
    pub fn error_event_emitted(&self) -> bool {
        self.error_event_emitted
    }

    /// 标记已向客户端发过 SSE error 事件（收尾路径手动补发后调用）
    pub fn mark_error_event_emitted(&mut self) {
        self.error_event_emitted = true;
    }

    /// 标记传输层中断（读流/读响应体 Err）：置失败态供收尾记账。
    /// 幂等：已是失败态则保留首因。
    pub fn mark_transport_error(&mut self, message: impl Into<String>) {
        if self.completion.is_ok() {
            self.completion = CompletionStatus::TransportError {
                message: message.into(),
            };
        }
    }

    /// 标记解码器永久停止（连续错误超限，响应必然截断）：置失败态供收尾记账。
    /// 幂等：已是失败态则保留首因。
    pub fn mark_decoder_stopped(&mut self, message: impl Into<String>) {
        if self.completion.is_ok() {
            self.completion = CompletionStatus::DecoderStopped {
                message: message.into(),
            };
        }
    }

    /// 剥离 DeepSeek 的工具调用协议标记(DSML 特殊 token),它们本是模型内部"要开始调工具"的
    /// 分隔符、不该出现在给用户的文本流里,但 Kiro 上游未过滤、当普通文本发下来(实测坐实:
    /// deepseek 调工具前先吐 `<｜DSML｜function_calls` / `<｜tool▁calls▁begin｜>` 家族标记,
    /// 之后才发真正的 toolUseEvent 帧)。原样透传会让客户端看到乱码标记。
    ///
    /// 标记用全角竖线 `｜`(U+FF5C)分隔,形如 `<｜DSML｜...` / `<｜tool▁calls▁begin｜>` /
    /// `<｜tool▁call▁begin｜>` / `<｜tool▁sep｜>` / `<｜tool▁call▁end｜>` 等。
    ///
    /// 跨 chunk 安全:标记可能被上游分帧从中间切开。策略——先把上轮留存的尾巴拼到本次内容前,
    /// 然后:①遇到 `<｜` 开头且已闭合 `｜>` 或后接 DSML/tool 关键字的完整标记 → 整段丢弃;
    /// ②末尾若是"半个可能的标记"(有 `<｜` 但还没闭合)→ 留到 dsml_tail_buffer 等下轮;
    /// ③其余正常文本原样输出。只对**含全角竖线的 `<｜` 序列**动手,绝不误伤正常 `<` 文本。
    /// 是否对本请求模型启用 DSML 剥离:**只对会吐 DSML 工具标记的国产模型**(deepseek/qwen/glm/
    /// minimax/kimi/moonshot 等)启用;Claude 系绝不剥离(它不产生这些标记,剥离只会误伤正文/吞字)。
    fn dsml_filter_applicable(&self) -> bool {
        let m = self.model.to_ascii_lowercase();
        // Claude 系明确排除(最主力路径,零风险优先)。
        if m.contains("claude") || m.contains("opus") || m.contains("sonnet") || m.contains("haiku") {
            return false;
        }
        m.contains("deepseek")
            || m.contains("qwen")
            || m.contains("glm")
            || m.contains("minimax")
            || m.contains("kimi")
            || m.contains("moonshot")
            || m.contains("deepglm") // 兜底泛化(未来国产名)
    }

    /// `<｜` 之后是否确为已知 DSML/工具协议标记关键字(白名单)。只有命中才剥离,
    /// 避免正文里合法的 `<｜…>`(CJK 排版 / 用户引用 token / 代码)被误删。
    /// DeepSeek 标记家族:`<｜DSML｜…` / `<｜tool▁calls▁begin｜>` / `<｜tool▁call▁begin｜>` /
    /// `<｜tool▁sep｜>` / `<｜tool▁call▁end｜>` / `<｜tool▁calls▁end｜>` 等,均以 `DSML`/`tool` 开头。
    fn is_dsml_keyword_after_pipe(rest: &str) -> bool {
        // rest = `<｜` 之后的内容(不含 `<｜`)。大小写不敏感前缀匹配。
        let r = rest.trim_start().to_ascii_lowercase();
        r.starts_with("dsml") || r.starts_with("tool") || r.starts_with("function")
    }

    /// DSML 尾巴缓冲的最大保留字符数:超过说明 `<｜` 后长期不闭合、大概率**不是**标记(正常正文),
    /// 应作为普通文本放行,避免无界囤积 + 静默吞正文。DeepSeek 标记都很短(<40 字符)。
    const DSML_TAIL_MAX: usize = 48;

    fn strip_dsml_markers(&mut self, content: &str) -> String {
        // 模型门控:非国产模型(尤其 Claude)完全不走剥离,原样返回,零风险零开销。
        if !self.dsml_filter_applicable() {
            return content.to_string();
        }
        // 快路径:无待处理尾巴且不含 `<`,直接返回。
        if self.dsml_tail_buffer.is_empty() && !content.contains('<') {
            return content.to_string();
        }
        let mut work = std::mem::take(&mut self.dsml_tail_buffer);
        self.dsml_tail_is_marker = false; // 取出后复位;若本轮再 hold 确认标记会重新置 true
        work.push_str(content);

        let mut out = String::with_capacity(work.len());
        let chars: Vec<char> = work.chars().collect();
        let mut i = 0;
        while i < chars.len() {
            // 探测 DSML 标记起点:`<` 紧跟全角竖线 `｜`(U+FF5C)。
            if chars[i] == '<' && i + 1 < chars.len() && chars[i + 1] == '\u{FF5C}' {
                let rest: String = chars[i + 2..].iter().collect();
                // 白名单校验:`<｜` 后必须确为 DSML/tool/function 关键字才当标记;否则是正文,原样输出。
                // 若关键字尚不完整(rest 太短还看不出)且没闭合,则 hold 到下轮再判。
                let looks_marker = Self::is_dsml_keyword_after_pipe(&rest);
                let closed = chars[i..].iter().position(|&c| c == '>');
                if looks_marker {
                    if let Some(rel_gt) = closed {
                        i += rel_gt + 1; // 完整标记 `<｜…>` 整段丢弃
                        continue;
    } else {
                        // 已**确认是 DSML/tool 关键字标记**但无 `>` 闭合:DeepSeek 的 `<｜DSML｜function_calls`
                        // 这类标记本就不以 `>` 收尾、以后续(转成真 toolUseEvent 帧)为界,文本流里到此即断。
                        // 它是标记噪音**不是正文**——丢弃本 chunk 从 `<｜` 起的余下全部,标记 tail 为
                        // "确认标记残留",使流结束 flush 时**丢弃而非补发**(补发会把 <｜DSML… 当正文泄漏)。
                        let held: String = chars[i..].iter().collect();
                        if held.chars().count() > Self::DSML_TAIL_MAX {
                            // 超长仍无 `>`:关键字命中大概率是误判(正文恰以 <｜tool… 开头且很长),
                            // 放行为正文避免吞掉大段合法内容(误判从宽,宁可偶尔漏个标记也不吞正文)。
                            out.push_str(&held);
                        } else {
                            self.dsml_tail_buffer = held;
                            self.dsml_tail_is_marker = true;
                        }
                        return out;
                    }
                } else {
                    // `<｜` 后不是关键字:可能是(a)正文里合法 `<｜…>`→原样输出这个 `<`,继续扫;
                    // (b)关键字还没到齐(rest 短且未闭合)→ hold 等下轮确认。
                    let undecided = closed.is_none() && rest.chars().count() < 8;
                    if undecided {
                        let held: String = chars[i..].iter().collect();
                        if held.chars().count() <= Self::DSML_TAIL_MAX {
                            self.dsml_tail_buffer = held;
                            return out;
                        }
                        // 超长:放行为正文
                    }
                    // 确定不是标记:原样输出 `<` 后继续(不跳过后续内容)。
                    out.push(chars[i]);
                    i += 1;
                    continue;
                }
            }
            out.push(chars[i]);
            i += 1;
        }
        // 边界:输出末尾孤立 `<`(标记 `<｜` 可能被从 `<` 与 `｜` 间切开)。hold 等下轮拼判。
        if out.ends_with('<') {
            out.pop();
            self.dsml_tail_buffer.push('<');
        }
        out
    }

    /// 流结束时把 DSML 尾巴缓冲里的残留作为**普通文本**补发,避免末尾孤立 `<`/未闭合半标记被静默吞掉。
    /// 收尾路径(generate_final_events/finish)调用。返回残留文本(空则无事)。
    pub fn flush_dsml_tail(&mut self) -> Vec<SseEvent> {
        if self.dsml_tail_buffer.is_empty() {
            return Vec::new();
        }
        let leftover = std::mem::take(&mut self.dsml_tail_buffer);
        let was_marker = self.dsml_tail_is_marker;
        self.dsml_tail_is_marker = false;
        if was_marker {
            // 已确认是 DSML 关键字标记的残留(如 `<｜DSML｜function_calls` 无 `>` 收尾):丢弃,
            // 绝不补发——补发会把标记当正文泄漏给客户端(这正是之前实测漏的那条)。
            return Vec::new();
        }
        // 否则是被误判为半标记的正文(或流在 `<` 后截断):按普通文本发出,不吞字。
        self.create_text_delta_events(&leftover)
    }

    /// 处理助手响应事件
    fn process_assistant_response(&mut self, content: &str) -> Vec<SseEvent> {
        // 先剥离 DeepSeek DSML 工具协议标记(跨 chunk 安全),再走后续文本处理。
        let cleaned = self.strip_dsml_markers(content);
        let content = cleaned.as_str();
        if content.is_empty() {
            return Vec::new();
        }

        // 估算 tokens
        self.output_tokens += estimate_tokens(content);

        // 如果启用了thinking，需要处理thinking块
        if self.thinking_enabled {
            return self.process_content_with_thinking(content);
        }

        // 非 thinking 模式同样复用统一的 text_delta 发送逻辑，
        // 以便在 tool_use 自动关闭文本块后能够自愈重建新的文本块，避免“吞字”。
        self.create_text_delta_events(content)
    }

    /// 处理包含thinking块的内容
    fn process_content_with_thinking(&mut self, content: &str) -> Vec<SseEvent> {
        let mut events = Vec::new();

        // 将内容添加到缓冲区进行处理
        self.thinking_buffer.push_str(content);

        loop {
            if !self.in_thinking_block && !self.thinking_extracted {
                // 查找 <thinking> 开始标签（跳过被反引号包裹的）
                if let Some(start_pos) = find_real_thinking_start_tag(&self.thinking_buffer) {
                    // 发送 <thinking> 之前的内容作为 text_delta
                    // 注意：如果前面只是空白字符（如 adaptive 模式返回的 \n\n），则跳过，
                    // 避免在 thinking 块之前产生无意义的 text 块导致客户端解析失败
                    let before_thinking = self.thinking_buffer[..start_pos].to_string();
                    if !before_thinking.is_empty() && !before_thinking.trim().is_empty() {
                        events.extend(self.create_text_delta_events(&before_thinking));
                    }

                    // 进入 thinking 块
                    self.in_thinking_block = true;
                    self.strip_thinking_leading_newline = true;
                    self.thinking_buffer =
                        self.thinking_buffer[start_pos + "<thinking>".len()..].to_string();

                    // 创建 thinking 块的 content_block_start 事件
                    let thinking_index = self.state_manager.next_block_index();
                    self.thinking_block_index = Some(thinking_index);
                    let start_events = self.state_manager.handle_content_block_start(
                        thinking_index,
                        "thinking",
                        json!({
                            "type": "content_block_start",
                            "index": thinking_index,
                            "content_block": {
                                "type": "thinking",
                                "thinking": ""
                            }
                        }),
                    );
                    events.extend(start_events);
                } else {
                    // 没有找到 <thinking>，检查是否可能是部分标签
                    // 保留可能是部分标签的内容
                    let target_len = self
                        .thinking_buffer
                        .len()
                        .saturating_sub("<thinking>".len());
                    let safe_len = find_char_boundary(&self.thinking_buffer, target_len);
                    if safe_len > 0 {
                        let safe_content = self.thinking_buffer[..safe_len].to_string();
                        // 如果 thinking 尚未提取，且安全内容只是空白字符，
                        // 则不发送为 text_delta，继续保留在缓冲区等待更多内容。
                        // 这避免了 4.6 模型中 <thinking> 标签跨事件分割时，
                        // 前导空白（如 "\n\n"）被错误地创建为 text 块，
                        // 导致 text 块先于 thinking 块出现的问题。
                        if !safe_content.is_empty() && !safe_content.trim().is_empty() {
                            events.extend(self.create_text_delta_events(&safe_content));
                            self.thinking_buffer = self.thinking_buffer[safe_len..].to_string();
                        }
                    }
                    break;
                }
            } else if self.in_thinking_block {
                // 剥离 <thinking> 标签后紧跟的换行符（可能跨 chunk）
                if self.strip_thinking_leading_newline {
                    if self.thinking_buffer.starts_with('\n') {
                        self.thinking_buffer = self.thinking_buffer[1..].to_string();
                        self.strip_thinking_leading_newline = false;
                    } else if !self.thinking_buffer.is_empty() {
                        // buffer 非空但不以 \n 开头，不再需要剥离
                        self.strip_thinking_leading_newline = false;
                    }
                    // buffer 为空时保留标志，等待下一个 chunk
                }

                // 在 thinking 块内，查找 </thinking> 结束标签（跳过被反引号包裹的）
                if let Some(end_pos) = find_real_thinking_end_tag(&self.thinking_buffer) {
                    // 提取 thinking 内容
                    let thinking_content = self.thinking_buffer[..end_pos].to_string();
                    if !thinking_content.is_empty() {
                        if let Some(thinking_index) = self.thinking_block_index {
                            events.push(
                                self.create_thinking_delta_event(thinking_index, &thinking_content),
                            );
                        }
                    }

                    // 结束 thinking 块
                    self.in_thinking_block = false;
                    self.thinking_extracted = true;

                    // 发送空的 thinking_delta 事件，然后发送 content_block_stop 事件
                    if let Some(thinking_index) = self.thinking_block_index {
                        // 先发送空的 thinking_delta
                        events.push(self.create_thinking_delta_event(thinking_index, ""));
                        // 再发送 signature_delta（满足客户端 thinking 模式本地校验）
                        events.push(self.create_signature_delta_event(thinking_index));
                        // 最后发送 content_block_stop
                        if let Some(stop_event) =
                            self.state_manager.handle_content_block_stop(thinking_index)
                        {
                            events.push(stop_event);
                        }
                    }

                    // 剥离 `</thinking>\n\n`（find_real_thinking_end_tag 已确认 \n\n 存在）
                    self.thinking_buffer =
                        self.thinking_buffer[end_pos + "</thinking>\n\n".len()..].to_string();
                } else {
                    // 没有找到结束标签，发送当前缓冲区内容作为 thinking_delta。
                    // 保留末尾可能是部分 `</thinking>\n\n` 的内容：
                    // find_real_thinking_end_tag 要求标签后有 `\n\n` 才返回 Some，
                    // 因此保留区必须覆盖 `</thinking>\n\n` 的完整长度（13 字节），
                    // 否则当 `</thinking>` 已在 buffer 但 `\n\n` 尚未到达时，
                    // 标签的前几个字符会被错误地作为 thinking_delta 发出。
                    let target_len = self
                        .thinking_buffer
                        .len()
                        .saturating_sub("</thinking>\n\n".len());
                    let safe_len = find_char_boundary(&self.thinking_buffer, target_len);
                    if safe_len > 0 {
                        let safe_content = self.thinking_buffer[..safe_len].to_string();
                        if !safe_content.is_empty() {
                            if let Some(thinking_index) = self.thinking_block_index {
                                events.push(
                                    self.create_thinking_delta_event(thinking_index, &safe_content),
                                );
                            }
                        }
                        self.thinking_buffer = self.thinking_buffer[safe_len..].to_string();
                    }
                    break;
                }
            } else {
                // thinking 已提取完成，剩余内容作为 text_delta
                if !self.thinking_buffer.is_empty() {
                    let remaining = self.thinking_buffer.clone();
                    self.thinking_buffer.clear();
                    events.extend(self.create_text_delta_events(&remaining));
                }
                break;
            }
        }

        events
    }

    /// 创建 text_delta 事件
    ///
    /// 如果文本块尚未创建，会先创建文本块。
    /// 当发生 tool_use 时，状态机会自动关闭当前文本块；后续文本会自动创建新的文本块继续输出。
    ///
    /// 返回值包含可能的 content_block_start 事件和 content_block_delta 事件。
    fn create_text_delta_events(&mut self, text: &str) -> Vec<SseEvent> {
        let mut events = Vec::new();

        // 如果当前 text_block_index 指向的块已经被关闭（例如 tool_use 开始时自动 stop），
        // 则丢弃该索引并创建新的文本块继续输出，避免 delta 被状态机拒绝导致“吞字”。
        if let Some(idx) = self.text_block_index {
            if !self.state_manager.is_block_open_of_type(idx, "text") {
                self.text_block_index = None;
            }
        }

        // 获取或创建文本块索引
        let text_index = if let Some(idx) = self.text_block_index {
            idx
        } else {
            // 文本块尚未创建，需要先创建
            let idx = self.state_manager.next_block_index();
            self.text_block_index = Some(idx);

            // 发送 content_block_start 事件
            let start_events = self.state_manager.handle_content_block_start(
                idx,
                "text",
                json!({
                    "type": "content_block_start",
                    "index": idx,
                    "content_block": {
                        "type": "text",
                        "text": ""
                    }
                }),
            );
            events.extend(start_events);
            idx
        };

        // 发送 content_block_delta 事件
        if let Some(delta_event) = self.state_manager.handle_content_block_delta(
            text_index,
            json!({
                "type": "content_block_delta",
                "index": text_index,
                "delta": {
                    "type": "text_delta",
                    "text": text
                }
            }),
        ) {
            events.push(delta_event);
        }

        events
    }

    /// 创建 thinking_delta 事件
    fn create_thinking_delta_event(&self, index: i32, thinking: &str) -> SseEvent {
        SseEvent::new(
            "content_block_delta",
            json!({
                "type": "content_block_delta",
                "index": index,
                "delta": {
                    "type": "thinking_delta",
                    "thinking": thinking
                }
            }),
        )
    }

    /// 创建 signature_delta 事件
    ///
    /// thinking 块流式结束前（`content_block_stop` 之前）必须发一个 signature_delta，
    /// 携带非空占位签名，满足客户端 thinking 模式下的本地校验。详见
    /// [`THINKING_SIGNATURE_PLACEHOLDER`]。
    fn create_signature_delta_event(&self, index: i32) -> SseEvent {
        SseEvent::new(
            "content_block_delta",
            json!({
                "type": "content_block_delta",
                "index": index,
                "delta": {
                    "type": "signature_delta",
                    "signature": THINKING_SIGNATURE_PLACEHOLDER
                }
            }),
        )
    }

    /// 处理工具使用事件
    fn process_tool_use(
        &mut self,
        tool_use: &crate::kiro::model::events::ToolUseEvent,
    ) -> Vec<SseEvent> {
        let mut events = Vec::new();

        self.state_manager.set_has_tool_use(true);

        // tool_use 必须发生在 thinking 结束之后。
        // 但当 `</thinking>` 后面没有 `\n\n`（例如紧跟 tool_use 或流结束）时，
        // thinking 结束标签会滞留在 thinking_buffer，导致后续 flush 时把 `</thinking>` 当作内容输出。
        // 这里在开始 tool_use block 前做一次“边界场景”的结束标签识别与过滤。
        if self.thinking_enabled && self.in_thinking_block {
            if let Some(end_pos) = find_real_thinking_end_tag_at_buffer_end(&self.thinking_buffer) {
                let thinking_content = self.thinking_buffer[..end_pos].to_string();
                if !thinking_content.is_empty() {
                    if let Some(thinking_index) = self.thinking_block_index {
                        events.push(
                            self.create_thinking_delta_event(thinking_index, &thinking_content),
                        );
                    }
                }

                // 结束 thinking 块
                self.in_thinking_block = false;
                self.thinking_extracted = true;

                if let Some(thinking_index) = self.thinking_block_index {
                    // 先发送空的 thinking_delta
                    events.push(self.create_thinking_delta_event(thinking_index, ""));
                    // 再发送 signature_delta（满足客户端 thinking 模式本地校验）
                    events.push(self.create_signature_delta_event(thinking_index));
                    // 最后发送 content_block_stop
                    if let Some(stop_event) =
                        self.state_manager.handle_content_block_stop(thinking_index)
                    {
                        events.push(stop_event);
                    }
                }

                // 把结束标签后的内容当作普通文本（通常为空或空白）
                let after_pos = end_pos + "</thinking>".len();
                let remaining = self.thinking_buffer[after_pos..].trim_start().to_string();
                self.thinking_buffer.clear();
                if !remaining.is_empty() {
                    events.extend(self.create_text_delta_events(&remaining));
                }
            }
        }

        // thinking 模式下，process_content_with_thinking 可能会为了探测 `<thinking>` 而暂存一小段尾部文本。
        // 如果此时直接开始 tool_use，状态机会自动关闭 text block，导致这段"待输出文本"看起来被 tool_use 吞掉。
        // 约束：只在尚未进入 thinking block、且 thinking 尚未被提取时，将缓冲区当作普通文本 flush。
        if self.thinking_enabled
            && !self.in_thinking_block
            && !self.thinking_extracted
            && !self.thinking_buffer.is_empty()
        {
            let buffered = std::mem::take(&mut self.thinking_buffer);
            events.extend(self.create_text_delta_events(&buffered));
        }

        // 获取或分配块索引
        let block_index = if let Some(&idx) = self.tool_block_indices.get(&tool_use.tool_use_id) {
            idx
        } else {
            let idx = self.state_manager.next_block_index();
            self.tool_block_indices
                .insert(tool_use.tool_use_id.clone(), idx);
            idx
        };

        // 还原工具名称（如果有映射）
        let original_name = self
            .tool_name_map
            .get(&tool_use.name)
            .cloned()
            .unwrap_or_else(|| tool_use.name.clone());

        // 发送 content_block_start
        let start_events = self.state_manager.handle_content_block_start(
            block_index,
            "tool_use",
            json!({
                "type": "content_block_start",
                "index": block_index,
                "content_block": {
                    "type": "tool_use",
                    "id": tool_use.tool_use_id,
                    "name": original_name,
                    "input": {}
                }
            }),
        );
        events.extend(start_events);

        // ⭐修复 Invalid tool parameters（根治，非逐片透传）：
        // 根因（4路研究 + kiro2api 参照实现结论）：Kiro 的 toolUseEvent.input 逐帧到达，逐片当
        // partial_json 原样透传时，一旦(a)上游帧非严格前缀单调（启发式 else 分支重复拼接）、或
        // (b)中间帧被静默丢弃/截断，客户端拼接后的**总 JSON** 就非法 → 报 Invalid tool parameters。
        // Anthropic 契约：客户端只在 content_block_stop 才把所有 partial_json 拼接后**一次性** parse，
        // 不要求逐片合法。故最稳做法（kiro2api 已验证）：按 tool_use_id **缓冲**到 stop，校验后
        // **一次性发单个 delta**。全程 String 级重组，绝不做字节切片，char-boundary panic 面彻底消除。
        //
        // 重组语义（与真实上游模式对齐，见 `merge_tool_input` 完备决策表）：
        //   累积快照 / 纯增量碎片 / 重复终帧 / 迟到旧短快照 / 非前缀重写 均被正确处理。
        //   关键：非前缀双完整对象不再被无脑 append 成 `}{` 粘连非法 JSON（Invalid tool parameters 类型 C）。
        if !tool_use.input.is_empty() {
            let buf = self.tool_input_sent.entry(tool_use.tool_use_id.clone()).or_default();
            *buf = merge_tool_input(buf, &tool_use.input);
        }

        // 仅在 stop 时把完整缓冲一次性发出 + 关闭块（此前只累积、不发 partial_json）。
        if tool_use.stop {
            let assembled = self
                .tool_input_sent
                .remove(&tool_use.tool_use_id)
                .unwrap_or_default();
            events.extend(self.flush_tool_input(block_index, assembled));
            if let Some(stop_event) = self.state_manager.handle_content_block_stop(block_index) {
                events.push(stop_event);
            }
        }

        events
    }

    /// 把某 tool_use 累积完整的 input 作为**单个** input_json_delta 发出（stop 时调用 / 截断收尾兜底）。
    ///
    /// 校验完整 JSON：合法→原样发；非法→告警并尽力发（不静默吞成空参数——空参数会让客户端把
    /// 一个失败的工具调用当成"无参数成功调用"执行，比报错更危险）。空串→不发（无参工具，客户端得 `{}`）。
    fn flush_tool_input(&mut self, block_index: i32, assembled: String) -> Vec<SseEvent> {
        if assembled.is_empty() {
            return Vec::new();
        }
        self.output_tokens += (assembled.len() as i32 + 3) / 4;
        if serde_json::from_str::<serde_json::Value>(&assembled).is_err() {
            // 拼装后仍非法：多因上游发了非法 JSON（如 JSON 不支持的 \x 转义 / 截断 \uXXXX / 裸控制符）
            // 或中间帧丢失致截断。如实告警便于定位；仍把原样串发出交由客户端处置，不静默改写成空参。
            tracing::warn!(
                block_index,
                "tool_use 拼装后 input 非合法 JSON（长度 {}），可能上游非法转义或帧丢失截断",
                assembled.len()
            );
        }
        self.state_manager
            .handle_content_block_delta(
                block_index,
                json!({
                    "type": "content_block_delta",
                    "index": block_index,
                    "delta": {
                        "type": "input_json_delta",
                        "partial_json": assembled
                    }
                }),
            )
            .into_iter()
            .collect()
    }

    /// 生成最终事件序列
    pub fn generate_final_events(&mut self) -> Vec<SseEvent> {
        let mut events = Vec::new();

        // 截断兜底：若某 tool_use 累积了 input 但流在 stop 之前就结束（上游截断/客户端断开），
        // 缓冲会残留、块未关闭。这里把残留 input 尽力发出并关闭块，避免客户端卡在未闭合的
        // tool_use 块上。（正常路径 stop 时已 flush + remove，此处只处理未收到 stop 的残留。）
        if !self.tool_input_sent.is_empty() {
            let pending: Vec<(String, String)> = self.tool_input_sent.drain().collect();
            for (tool_use_id, assembled) in pending {
                if let Some(&idx) = self.tool_block_indices.get(&tool_use_id) {
                    events.extend(self.flush_tool_input(idx, assembled));
                    if let Some(stop_event) = self.state_manager.handle_content_block_stop(idx) {
                        events.push(stop_event);
                    }
                }
            }
        }

        // Flush thinking_buffer 中的剩余内容
        if self.thinking_enabled && !self.thinking_buffer.is_empty() {
            if self.in_thinking_block {
                // 末尾可能残留 `</thinking>`（例如紧跟 tool_use 或流结束），需要在 flush 时过滤掉结束标签。
                if let Some(end_pos) =
                    find_real_thinking_end_tag_at_buffer_end(&self.thinking_buffer)
                {
                    let thinking_content = self.thinking_buffer[..end_pos].to_string();
                    if !thinking_content.is_empty() {
                        if let Some(thinking_index) = self.thinking_block_index {
                            events.push(
                                self.create_thinking_delta_event(thinking_index, &thinking_content),
                            );
                        }
                    }

                    // 关闭 thinking 块：先发送空的 thinking_delta，再发 signature_delta，最后 content_block_stop
                    if let Some(thinking_index) = self.thinking_block_index {
                        events.push(self.create_thinking_delta_event(thinking_index, ""));
                        events.push(self.create_signature_delta_event(thinking_index));
                        if let Some(stop_event) =
                            self.state_manager.handle_content_block_stop(thinking_index)
                        {
                            events.push(stop_event);
                        }
                    }

                    // 把结束标签后的内容当作普通文本（通常为空或空白）
                    let after_pos = end_pos + "</thinking>".len();
                    let remaining = self.thinking_buffer[after_pos..].trim_start().to_string();
                    self.thinking_buffer.clear();
                    self.in_thinking_block = false;
                    self.thinking_extracted = true;
                    if !remaining.is_empty() {
                        events.extend(self.create_text_delta_events(&remaining));
                    }
                } else {
                    // 如果还在 thinking 块内，发送剩余内容作为 thinking_delta
                    if let Some(thinking_index) = self.thinking_block_index {
                        events.push(
                            self.create_thinking_delta_event(thinking_index, &self.thinking_buffer),
                        );
                    }
                    // 关闭 thinking 块：先发送空的 thinking_delta，再发送 content_block_stop
                    if let Some(thinking_index) = self.thinking_block_index {
                        // 先发送空的 thinking_delta
                        events.push(self.create_thinking_delta_event(thinking_index, ""));
                        // 再发送 signature_delta（满足客户端 thinking 模式本地校验）
                        events.push(self.create_signature_delta_event(thinking_index));
                        // 最后发送 content_block_stop
                        if let Some(stop_event) =
                            self.state_manager.handle_content_block_stop(thinking_index)
                        {
                            events.push(stop_event);
                        }
                    }
                }
            } else {
                // 否则发送剩余内容作为 text_delta
                let buffer_content = self.thinking_buffer.clone();
                events.extend(self.create_text_delta_events(&buffer_content));
            }
            self.thinking_buffer.clear();
        }

        // Flush DSML 尾巴缓冲:把被误判为半标记而 hold 住的残留(或末尾孤立 `<`)作为普通文本补发,
        // 避免静默吞字。**必须放在 thinking 块收尾之后**:thinking 模式下 strip_dsml_markers 先于
        // process_content_with_thinking 执行,末尾残留 `<` 被 hold 进 dsml_tail_buffer(不进
        // thinking_buffer)。若在 thinking 块 stop 之前 flush,create_text_delta_events 会先开一个
        // text 块(更大索引),而更小索引的 thinking 块尚未 stop → SSE 出现「新块 start → 旧块 stop」
        // 交错,违反 Anthropic「先 stop 当前块再 start 下一块」契约,CC 可能解析报错。放在此处,
        // 残留 text 块在 thinking 块 stop 之后才 start,顺序合法;残留也使 has_non_thinking_blocks()
        // 变真,避免下方「仅 thinking」分支多补一个空格 text 块。
        events.extend(self.flush_dsml_tail());

        // 如果整个流中只产生了 thinking 块，没有 text 也没有 tool_use，
        // 则设置 stop_reason 为 max_tokens（表示模型耗尽了 token 预算在思考上），
        // 并补发一套完整的 text 事件（内容为一个空格），确保 content 数组中有 text 块
        if self.thinking_enabled
            && self.thinking_block_index.is_some()
            && !self.state_manager.has_non_thinking_blocks()
        {
            self.state_manager.set_stop_reason("max_tokens");
            events.extend(self.create_text_delta_events(" "));
        }

        // 使用从 contextUsageEvent 计算的 input_tokens，如果没有则使用估算值
        let final_input_tokens = self.context_input_tokens.unwrap_or(self.input_tokens);
        // 剔除 cache 读写，得到 Anthropic usage 的 input_tokens 口径
        let billed = self
            .cache_usage
            .map(|c| {
                billed_input_tokens(
                    final_input_tokens,
                    c.cache_creation_input_tokens,
                    c.cache_read_input_tokens,
                )
            })
            .unwrap_or(final_input_tokens);

        // 生成最终事件
        events.extend(self.state_manager.generate_final_events(
            billed,
            self.output_tokens,
            self.cache_usage,
        ));
        events
    }
}

/// 缓冲流处理上下文 - 用于 /cc/v1/messages 流式请求
///
/// 与 `StreamContext` 不同，此上下文会缓冲所有事件直到流结束，
/// 然后用从 `contextUsageEvent` 计算的正确 `input_tokens` 更正 `message_start` 事件。
///
/// 工作流程：
/// 1. 使用 `StreamContext` 正常处理所有 Kiro 事件
/// 2. 把生成的 SSE 事件缓存起来（而不是立即发送）
/// 3. 流结束时，找到 `message_start` 事件并更新其 `input_tokens`
/// 4. 一次性返回所有事件
pub struct BufferedStreamContext {
    /// 内部流处理上下文（复用现有的事件处理逻辑）
    inner: StreamContext,
    /// 缓冲的所有事件（包括 message_start、content_block_start 等）
    event_buffer: Vec<SseEvent>,
    /// 估算的 input_tokens（用于回退）
    estimated_input_tokens: i32,
    /// 是否已经生成了初始事件
    initial_events_generated: bool,
}

impl BufferedStreamContext {
    /// 创建缓冲流上下文
    pub fn new(
        model: impl Into<String>,
        estimated_input_tokens: i32,
        thinking_enabled: bool,
        tool_name_map: HashMap<String, String>,
    ) -> Self {
        let inner =
            StreamContext::new_with_thinking(model, estimated_input_tokens, thinking_enabled, tool_name_map);
        Self {
            inner,
            event_buffer: Vec::new(),
            estimated_input_tokens,
            initial_events_generated: false,
        }
    }

    /// 返回本次请求解析出的最终用量（供用量统计埋点使用）
    pub fn resolved_usage(&self) -> ResolvedUsage {
        self.inner.resolved_usage()
    }

    /// 本次响应的完成状态（透传内部 StreamContext）
    pub fn completion(&self) -> &CompletionStatus {
        self.inner.completion()
    }

    /// 用量记账应采用的最终结果分类（透传，去硬编码 Success）
    pub fn completion_outcome(&self) -> RequestOutcome {
        self.inner.completion_outcome()
    }

    /// 是否已内联发过 SSE error 事件（透传）
    pub fn error_event_emitted(&self) -> bool {
        self.inner.error_event_emitted()
    }

    /// 标记已发过 SSE error 事件（透传）
    pub fn mark_error_event_emitted(&mut self) {
        self.inner.mark_error_event_emitted();
    }

    /// 标记传输层中断（透传）
    pub fn mark_transport_error(&mut self, message: impl Into<String>) {
        self.inner.mark_transport_error(message);
    }

    /// 标记解码器永久停止（透传）
    pub fn mark_decoder_stopped(&mut self, message: impl Into<String>) {
        self.inner.mark_decoder_stopped(message);
    }

    /// 设置 prompt 缓存记账明细（影子缓存已移除，现无调用方；保留避免改流式热路径）
    #[allow(dead_code)]
    pub fn set_cache_usage(&mut self, cache_usage: Option<CacheUsageBreakdown>) {
        self.inner.set_cache_usage(cache_usage);
    }

    /// 处理 Kiro 事件并缓冲结果
    ///
    /// 复用 StreamContext 的事件处理逻辑，但把结果缓存而不是立即发送。
    pub fn process_and_buffer(&mut self, event: &crate::kiro::model::events::Event) {
        // 首次处理事件时，先生成初始事件（message_start 等）
        if !self.initial_events_generated {
            let initial_events = self.inner.generate_initial_events();
            self.event_buffer.extend(initial_events);
            self.initial_events_generated = true;
        }

        // 处理事件并缓冲结果
        let events = self.inner.process_kiro_event(event);
        self.event_buffer.extend(events);
    }

    /// 完成流处理并返回所有事件
    ///
    /// 此方法会：
    /// 1. 生成最终事件（message_delta, message_stop）
    /// 2. 用正确的 input_tokens 更正 message_start 事件
    /// 3. 返回所有缓冲的事件
    pub fn finish_and_get_all_events(&mut self) -> Vec<SseEvent> {
        // 如果从未处理过事件，也要生成初始事件
        if !self.initial_events_generated {
            let initial_events = self.inner.generate_initial_events();
            self.event_buffer.extend(initial_events);
            self.initial_events_generated = true;
        }

        // 生成最终事件
        let final_events = self.inner.generate_final_events();
        self.event_buffer.extend(final_events);

        // 获取正确的 input_tokens
        let final_input_tokens = self
            .inner
            .context_input_tokens
            .unwrap_or(self.estimated_input_tokens);
        // 剔除 cache 读写得到 billed 口径（与 message_delta 保持一致）
        let cache_usage = self.inner.cache_usage;
        let billed = cache_usage
            .map(|c| {
                billed_input_tokens(
                    final_input_tokens,
                    c.cache_creation_input_tokens,
                    c.cache_read_input_tokens,
                )
            })
            .unwrap_or(final_input_tokens);

        // 更正 message_start 事件中的 input_tokens（并补齐 cache 字段）
        for event in &mut self.event_buffer {
            if event.event == "message_start" {
                if let Some(message) = event.data.get_mut("message") {
                    if let Some(usage) = message.get_mut("usage") {
                        usage["input_tokens"] = serde_json::json!(billed);
                        if let Some(c) = cache_usage {
                            usage["cache_creation_input_tokens"] =
                                serde_json::json!(c.cache_creation_input_tokens);
                            usage["cache_read_input_tokens"] =
                                serde_json::json!(c.cache_read_input_tokens);
                        }
                    }
                }
            }
        }

        std::mem::take(&mut self.event_buffer)
    }
}

/// 简单的 token 估算
fn estimate_tokens(text: &str) -> i32 {
    let chars: Vec<char> = text.chars().collect();
    let mut chinese_count = 0;
    let mut other_count = 0;

    for c in &chars {
        if *c >= '\u{4E00}' && *c <= '\u{9FFF}' {
            chinese_count += 1;
        } else {
            other_count += 1;
        }
    }

    // 中文约 1.5 字符/token，英文约 4 字符/token
    let chinese_tokens = (chinese_count * 2 + 2) / 3;
    let other_tokens = (other_count + 3) / 4;

    (chinese_tokens + other_tokens).max(1)
}

/// 判定一段字符串是否为一个**完整合法**的 JSON 值（对象/数组/标量均可）。
/// 用于 `merge_tool_input` 识别「非前缀重写」：两帧各自都完整时不能追加。
fn is_complete_json(s: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(s).is_ok()
}

/// 合并同一 tool_use_id 逐帧到达的 input，返回合并后的新缓冲值。
///
/// 上游 `toolUseEvent.input` 的到达模式并不统一：可能是**纯增量碎片**（每帧只带新片段）、
/// **累积快照**（每帧是"到目前为止的完整 JSON"）、偶发**重复终帧**、迟到的**旧短快照**，
/// 甚至**非前缀重写**（同一 id 先发一个完整对象、再发另一个措辞不同的完整对象）。
/// 旧实现只有「前缀替换 / 否则 append」两步，遇到非前缀双完整对象会拼成 `}{` 粘连的非法 JSON
/// → 客户端 `JSON.parse` 失败 → **Invalid tool parameters（类型 C）**。
///
/// 完备决策表（顺序敏感）：
///   1. frame 空           → buf 不变
///   2. buf 空             → frame
///   3. frame == buf       → buf 不变（重复终帧，不翻倍）
///   4. frame 以 buf 为前缀且更长 → frame（累积快照，取最新最全）
///   5. buf 以 frame 为前缀（frame 更短） → buf 不变（丢弃迟到的旧短快照）
///   6. buf 与 frame 各自都是完整合法 JSON → frame（非前缀重写，只留最新完整对象，消灭 `}{` 粘连）
///   7. 否则               → buf + frame 追加（真增量碎片，还原完整内容）
///
/// 注意第 6 步的前提是**两者各自都完整**：单个完整 JSON 对象无法再被增量扩展，因此第二个
/// 完整对象必然是重写而非续写；反之若 frame 仅是"看似完整"的内层片段（如 `{"inner":1}` 续在
/// `{"outer":` 之后），buf 尚不完整则不触发第 6 步，仍走第 7 步正确追加。
pub(crate) fn merge_tool_input(buf: &str, frame: &str) -> String {
    // 1. 空帧 → 缓冲不变
    if frame.is_empty() {
        return buf.to_string();
    }
    // 2. 缓冲空 → 取本帧
    if buf.is_empty() {
        return frame.to_string();
    }
    // 3. 完全重复终帧 → 不变（避免翻倍）
    if frame == buf {
        return buf.to_string();
    }
    // 4. 累积快照：本帧以缓冲为前缀且更长 → 用本帧整体替换
    if frame.len() > buf.len() && frame.starts_with(buf) {
        return frame.to_string();
    }
    // 5. 迟到的旧短快照：缓冲以本帧为前缀（本帧更短）→ 保留更全的缓冲，丢弃本帧
    if buf.len() > frame.len() && buf.starts_with(frame) {
        return buf.to_string();
    }
    // 6. 非前缀重写：缓冲与本帧各自都是完整合法 JSON → 只留最新完整对象（消灭 `}{` 粘连）
    if is_complete_json(buf) && is_complete_json(frame) {
        return frame.to_string();
    }
    // 7. 真增量碎片：追加还原完整内容
    let mut merged = String::with_capacity(buf.len() + frame.len());
    merged.push_str(buf);
    merged.push_str(frame);
    merged
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sse_event_format() {
        let event = SseEvent::new("message_start", json!({"type": "message_start"}));
        let sse_str = event.to_sse_string();

        assert!(sse_str.starts_with("event: message_start\n"));
        assert!(sse_str.contains("data: "));
        assert!(sse_str.ends_with("\n\n"));
    }

    fn mk_ctx() -> StreamContext {
        StreamContext::new_with_thinking("deepseek", 10, false, HashMap::new())
    }

    #[test]
    fn test_strip_dsml_full_marker_in_one_chunk() {
        // DeepSeek 工具协议标记应被整段剥离,正常文本保留。
        let mut ctx = mk_ctx();
        let out = ctx.strip_dsml_markers("先看目录。\n\n<｜DSML｜function_calls｜>后续");
        assert_eq!(out, "先看目录。\n\n后续", "DSML 完整标记应被剥离,前后正常文本保留");
        assert!(ctx.dsml_tail_buffer.is_empty());
    }

    #[test]
    fn test_strip_dsml_tool_calls_family() {
        let mut ctx = mk_ctx();
        let out = ctx.strip_dsml_markers("<｜tool▁calls▁begin｜><｜tool▁call▁begin｜>正文");
        assert_eq!(out, "正文", "tool_calls 家族标记应全部剥离");
    }

    #[test]
    fn test_strip_dsml_cross_chunk_split() {
        // 标记被上游从中间切成两个 chunk:第一块留半个标记到 tail,第二块拼上闭合后整段剥离。
        let mut ctx = mk_ctx();
        let out1 = ctx.strip_dsml_markers("正常文字<｜DSML｜func");
        assert_eq!(out1, "正常文字", "闭合前只输出正常文字,半个标记留 tail");
        assert!(!ctx.dsml_tail_buffer.is_empty(), "半个标记应留在 tail 缓冲");
        let out2 = ctx.strip_dsml_markers("tion_calls｜>之后");
        assert_eq!(out2, "之后", "拼上闭合后整段标记被剥离,只剩后续文本");
        assert!(ctx.dsml_tail_buffer.is_empty());
    }

    #[test]
    fn test_strip_dsml_split_at_angle_bracket() {
        // 实测坐实的分帧:`<` 单独在前一帧末尾,`｜DSML…` 在下一帧。
        let mut ctx = mk_ctx();
        let out1 = ctx.strip_dsml_markers("创建网页。\n\n<");
        assert_eq!(out1, "创建网页。\n\n", "末尾孤立 < 应 hold 到 tail,不输出");
        assert_eq!(ctx.dsml_tail_buffer, "<");
        let out2 = ctx.strip_dsml_markers("｜DSML｜function_calls｜>正文");
        assert_eq!(out2, "正文", "拼上后 <｜DSML…> 整段剥离");
        assert!(ctx.dsml_tail_buffer.is_empty());
    }

    #[test]
    fn test_strip_dsml_trailing_angle_then_normal() {
        // 末尾 < 被 hold,但下一帧是正常文本(非｜)→ < 应被还原输出,不丢字。
        let mut ctx = mk_ctx();
        let out1 = ctx.strip_dsml_markers("比较 a <");
        assert_eq!(out1, "比较 a ");
        let out2 = ctx.strip_dsml_markers(" b");
        assert_eq!(out2, "< b", "孤立 < 后接正常文本应还原,不误吞");
    }

    #[test]
    fn test_strip_dsml_does_not_touch_normal_text() {
        // 不含 DSML 的正常文本(哪怕有普通 < 号)绝不被改动。
        let mut ctx = mk_ctx();
        let out = ctx.strip_dsml_markers("if a < b && c > d 这是正常代码");
        assert_eq!(out, "if a < b && c > d 这是正常代码");
        assert!(ctx.dsml_tail_buffer.is_empty());
    }

    #[test]
    fn test_strip_dsml_claude_model_never_filtered() {
        // 门控:Claude 系模型完全不剥离——哪怕内容里恰好含 <｜…>(如用户让 Claude 解释 DSML)也原样保留。
        let mut ctx = StreamContext::new_with_thinking("claude-sonnet-4.6", 10, false, HashMap::new());
        let s = "DeepSeek 的标记写作 <｜DSML｜function_calls｜> 你看";
        assert_eq!(ctx.strip_dsml_markers(s), s, "Claude 模型不应剥离任何 <｜…>");
        assert!(ctx.dsml_tail_buffer.is_empty());
    }

    #[test]
    fn test_strip_dsml_keyword_whitelist_preserves_normal_fullwidth() {
        // 国产模型下,<｜ 后不是 DSML/tool/function 关键字的正文(CJK 排版)不被误删。
        let mut ctx = mk_ctx(); // deepseek
        let s = "见 <｜注｜关于x｜> 说明";
        assert_eq!(ctx.strip_dsml_markers(s), s, "非关键字的 <｜…> 属正文,应保留");
    }

    #[test]
    fn test_strip_dsml_flush_recovers_leftover() {
        // 末尾孤立 < 被 hold 后,若流结束(无下一帧),flush_dsml_tail 应把它作为普通文本补发,不吞字。
        let mut ctx = mk_ctx();
        let out = ctx.strip_dsml_markers("结尾是 a <");
        assert_eq!(out, "结尾是 a ");
        assert_eq!(ctx.dsml_tail_buffer, "<");
        // 模拟流结束:flush 应产出含 "<" 的 text_delta 事件,tail 清空。
        let flushed = ctx.flush_dsml_tail();
        assert!(!flushed.is_empty(), "flush 应补发残留 <,不静默吞字");
        assert!(ctx.dsml_tail_buffer.is_empty());
    }

    #[test]
    fn test_strip_dsml_marker_no_gt_discarded_on_flush() {
        // 实测漏的形态:<｜DSML｜function_calls 单帧到达且不以 > 收尾(DeepSeek 标记本就以后续为界)。
        // 应被识别为标记 hold 到 tail 且标记 is_marker,流结束 flush 时**丢弃不补发**,不泄漏。
        let mut ctx = mk_ctx(); // deepseek
        let out = ctx.strip_dsml_markers("我来看看目录。\n\n<｜DSML｜function_calls");
        assert_eq!(out, "我来看看目录。\n\n", "正文保留,标记 hold 不输出");
        assert!(ctx.dsml_tail_is_marker, "应标记为确认标记");
        let flushed = ctx.flush_dsml_tail();
        assert!(flushed.is_empty(), "确认标记的残留 flush 时丢弃,绝不当正文补发(否则泄漏)");
        assert!(ctx.dsml_tail_buffer.is_empty());
    }

    #[test]
    fn test_strip_dsml_unclosed_marker_bounded_flush() {
        // 是关键字但超长不闭合 → 不无界囤积,放行为正文(防吞正文/防无界)。
        let mut ctx = mk_ctx();
        let long = format!("<｜tool{}", "x".repeat(60)); // >DSML_TAIL_MAX 且无 >
        let out = ctx.strip_dsml_markers(&long);
        assert!(out.contains("tool"), "超长未闭合应放行为正文,不吞");
    }

    #[test]
    fn test_dsml_flush_after_thinking_close_keeps_block_order() {
        // 回归(对抗 review #2):国产模型 + thinking 开启,流在 thinking 块内结束,且最后一帧
        // 内容以孤立 `<` 收尾(被 strip_dsml_markers hold 进 dsml_tail_buffer,不进 thinking_buffer)。
        // generate_final_events 必须先 stop thinking 块,再把 DSML 残留作为 text 块 start——
        // 否则会出现「text 块 start(大索引)→ thinking 块 stop(小索引)」交错,违反 Anthropic
        // 「先 stop 当前块再 start 下一块」契约,CC 解析报错。
        let mut ctx = StreamContext::new_with_thinking("deepseek", 10, true, HashMap::new());

        // 驱动进入 thinking 块并停在块内;末尾孤立 `<` 会被 DSML 逻辑 hold 到 tail。
        let _ = ctx.process_assistant_response("<thinking>我在想 <");
        assert!(ctx.in_thinking_block, "应仍处于 thinking 块内");
        assert_eq!(ctx.dsml_tail_buffer, "<", "末尾孤立 < 应被 hold 到 DSML tail,而非进 thinking_buffer");

        let events = ctx.generate_final_events();

        // 收集块生命周期事件,校验:同一 index 的 text start 必须在 thinking stop 之后。
        // 找出 thinking 块 stop 的位置与任何 text 块 start 的位置。
        let mut thinking_stop_pos: Option<usize> = None;
        let mut text_start_pos: Option<usize> = None;
        for (pos, e) in events.iter().enumerate() {
            let idx = e.data.get("index").and_then(|v| v.as_i64());
            match e.event.as_str() {
                "content_block_stop" => {
                    // thinking 块索引来自 ctx.thinking_block_index
                    if idx == ctx.thinking_block_index.map(|i| i as i64) {
                        thinking_stop_pos = Some(pos);
                    }
                }
                "content_block_start" => {
                    let is_text = e
                        .data
                        .get("content_block")
                        .and_then(|cb| cb.get("type"))
                        .and_then(|t| t.as_str())
                        == Some("text");
                    if is_text && text_start_pos.is_none() {
                        text_start_pos = Some(pos);
                    }
                }
                _ => {}
            }
        }

        // 残留 `<` 会作为 text 块补发,thinking 块必然先 stop。
        let ts = thinking_stop_pos.expect("thinking 块应被 stop");
        if let Some(txs) = text_start_pos {
            assert!(
                ts < txs,
                "thinking 块 stop(pos={}) 必须早于 DSML 残留 text 块 start(pos={}),否则块顺序交错",
                ts,
                txs
            );
        }
        assert!(ctx.dsml_tail_buffer.is_empty(), "flush 后 tail 应清空");
    }

    #[test]
    fn test_sse_state_manager_message_start() {
        let mut manager = SseStateManager::new();

        // 第一次应该成功
        let event = manager.handle_message_start(json!({"type": "message_start"}));
        assert!(event.is_some());

        // 第二次应该被跳过
        let event = manager.handle_message_start(json!({"type": "message_start"}));
        assert!(event.is_none());
    }

    #[test]
    fn test_sse_state_manager_block_lifecycle() {
        let mut manager = SseStateManager::new();

        // 创建块
        let events = manager.handle_content_block_start(0, "text", json!({}));
        assert_eq!(events.len(), 1);

        // delta
        let event = manager.handle_content_block_delta(0, json!({}));
        assert!(event.is_some());

        // stop
        let event = manager.handle_content_block_stop(0);
        assert!(event.is_some());

        // 重复 stop 应该被跳过
        let event = manager.handle_content_block_stop(0);
        assert!(event.is_none());
    }

    #[test]
    fn test_tool_name_reverse_mapping_in_stream() {
        use crate::kiro::model::events::ToolUseEvent;

        let mut map = HashMap::new();
        map.insert("short_abc12345".to_string(), "mcp__very_long_original_tool_name".to_string());

        let mut ctx = StreamContext::new_with_thinking("test-model", 1, false, map);
        let _ = ctx.generate_initial_events();

        // 模拟 Kiro 返回短名称的 tool_use
        let tool_event = Event::ToolUse(ToolUseEvent {
            name: "short_abc12345".to_string(),
            tool_use_id: "toolu_01".to_string(),
            input: r#"{"key":"value"}"#.to_string(),
            stop: true,
        });

        let events = ctx.process_kiro_event(&tool_event);

        // content_block_start 中的 name 应该是原始长名称
        let start_event = events.iter().find(|e| e.event == "content_block_start").unwrap();
        assert_eq!(
            start_event.data["content_block"]["name"],
            "mcp__very_long_original_tool_name",
            "应还原为原始工具名称"
        );
    }

    /// 跑一串 tool_use 帧，返回 (拼接出的 partial_json 全文, 发出的 input_json_delta 事件数)。
    /// 根治后应恒为「单个 delta 在 stop 时发出」，故 delta 数应为 0(空参)或 1(有参)。
    fn run_tool_frames(ctx: &mut StreamContext, frames: &[(&str, bool)]) -> (String, usize) {
        use crate::kiro::model::events::ToolUseEvent;
        let mut out = String::new();
        let mut delta_count = 0usize;
        for (input, stop) in frames {
            let ev = Event::ToolUse(ToolUseEvent {
                name: "t".to_string(),
                tool_use_id: "toolu_x".to_string(),
                input: input.to_string(),
                stop: *stop,
            });
            for e in ctx.process_kiro_event(&ev) {
                if e.event == "content_block_delta"
                    && e.data["delta"]["type"] == "input_json_delta"
                {
                    out.push_str(e.data["delta"]["partial_json"].as_str().unwrap_or(""));
                    delta_count += 1;
                }
            }
        }
        (out, delta_count)
    }

    /// 兼容旧断言：只取拼接全文。
    fn collect_tool_partial_json(ctx: &mut StreamContext, frames: &[(&str, bool)]) -> String {
        run_tool_frames(ctx, frames).0
    }

    #[test]
    fn test_tool_input_cumulative_snapshots() {
        // 上游发累积快照：每帧是"到目前为止的完整 JSON"。转发拼接后不应重复,应恰为最终完整 JSON。
        let mut ctx = StreamContext::new_with_thinking("m", 1, false, HashMap::new());
        let _ = ctx.generate_initial_events();
        let joined = collect_tool_partial_json(
            &mut ctx,
            &[
                (r#"{"a""#, false),
                (r#"{"a":1"#, false),
                (r#"{"a":1,"b":2}"#, true),
            ],
        );
        assert_eq!(joined, r#"{"a":1,"b":2}"#, "累积模式:拼接后应为完整 JSON,无重复");
    }

    #[test]
    fn test_tool_input_repeated_final_frame() {
        // 累积模式常见收尾：stop 帧重复带上完整 JSON（与上一帧相同）→ 不应重复发。
        let mut ctx = StreamContext::new_with_thinking("m", 1, false, HashMap::new());
        let _ = ctx.generate_initial_events();
        let joined = collect_tool_partial_json(
            &mut ctx,
            &[
                (r#"{"a":1}"#, false),
                (r#"{"a":1}"#, true), // 完全重复帧
            ],
        );
        assert_eq!(joined, r#"{"a":1}"#, "重复帧不应二次转发");
    }

    #[test]
    fn test_tool_input_pure_deltas() {
        // 上游发纯增量:每帧是不同片段。转发原样,拼接后仍为完整 JSON。
        let mut ctx = StreamContext::new_with_thinking("m", 1, false, HashMap::new());
        let _ = ctx.generate_initial_events();
        let joined = collect_tool_partial_json(
            &mut ctx,
            &[
                (r#"{"a""#, false),
                (r#":1,"#, false),
                (r#""b":2}"#, true),
            ],
        );
        assert_eq!(joined, r#"{"a":1,"b":2}"#, "增量模式:原样转发,拼接后为完整 JSON");
    }

    #[test]
    fn test_tool_input_single_full_snapshot() {
        // 单帧完整 JSON（最常见）:原样一次发出。
        let mut ctx = StreamContext::new_with_thinking("m", 1, false, HashMap::new());
        let _ = ctx.generate_initial_events();
        let joined = collect_tool_partial_json(&mut ctx, &[(r#"{"k":"v"}"#, true)]);
        assert_eq!(joined, r#"{"k":"v"}"#);
    }

    #[test]
    fn test_tool_input_single_delta_invariant() {
        // 根治不变式：无论上游发几帧，最终只在 stop 发**一个** input_json_delta（缓冲到 stop 再发）。
        let mut ctx = StreamContext::new_with_thinking("m", 1, false, HashMap::new());
        let _ = ctx.generate_initial_events();
        let (joined, n) = run_tool_frames(
            &mut ctx,
            &[(r#"{"a""#, false), (r#"{"a":1"#, false), (r#"{"a":1,"b":2}"#, true)],
        );
        assert_eq!(joined, r#"{"a":1,"b":2}"#);
        assert_eq!(n, 1, "应只发一个 delta（缓冲到 stop 一次性发）");
    }

    #[test]
    fn test_tool_input_non_prefix_trap() {
        // 旧逐片启发式的致命陷阱:上游第二帧不以第一帧为前缀(非单调重写)。
        // 根治后（merge_tool_input 第 6 步）：两帧各自都是完整合法 JSON → 视为"重写",
        // 只保留最新完整对象,消灭 `}{` 粘连非法串(Invalid tool parameters 类型 C 根因)。
        let mut ctx = StreamContext::new_with_thinking("m", 1, false, HashMap::new());
        let _ = ctx.generate_initial_events();
        let (joined, n) = run_tool_frames(
            &mut ctx,
            &[(r#"{"path":"/a"}"#, false), (r#"{"path":"/b"}"#, true)],
        );
        // 只发一个 delta,且结果是合法 JSON(第二帧),不再是 `}{` 粘连串。
        assert_eq!(n, 1, "非前缀帧也只发一个 delta");
        assert_eq!(joined, r#"{"path":"/b"}"#, "非前缀双完整对象只留最新完整对象");
        assert!(
            serde_json::from_str::<serde_json::Value>(&joined).is_ok(),
            "结果必须是合法 JSON"
        );
    }

    #[test]
    fn test_tool_input_illegal_json_at_stop_still_emitted() {
        // 上游发本就非法的 JSON（如 JSON 不支持的 \x 转义）→ 校验失败但仍原样发出（告警),
        // 绝不静默吞成空参数（空参会让客户端把失败工具调用当无参成功执行,更危险）。
        let mut ctx = StreamContext::new_with_thinking("m", 1, false, HashMap::new());
        let _ = ctx.generate_initial_events();
        let (joined, n) = run_tool_frames(&mut ctx, &[(r#"{"a":"\xd7"}"#, true)]);
        assert_eq!(n, 1, "非法 JSON 也要发出(不静默空参)");
        assert_eq!(joined, r#"{"a":"\xd7"}"#, "原样发出交客户端处置");
    }

    #[test]
    fn test_tool_input_truncated_stream_flushes_on_final() {
        // 截断:tool_use 帧永不带 stop,流结束。generate_final_events 应把残留缓冲发出并关闭块,
        // 客户端不会卡在未闭合 tool_use 块上。
        use crate::kiro::model::events::ToolUseEvent;
        let mut ctx = StreamContext::new_with_thinking("m", 1, false, HashMap::new());
        let _ = ctx.generate_initial_events();
        // 两帧累积,均无 stop
        for input in [r#"{"a""#, r#"{"a":1}"#] {
            let ev = Event::ToolUse(ToolUseEvent {
                name: "t".to_string(),
                tool_use_id: "toolu_x".to_string(),
                input: input.to_string(),
                stop: false,
            });
            let evs = ctx.process_kiro_event(&ev);
            // stop 前不应发任何 input_json_delta
            assert!(!evs.iter().any(|e| e.event == "content_block_delta"
                && e.data["delta"]["type"] == "input_json_delta"));
        }
        let finals = ctx.generate_final_events();
        let delta = finals.iter().find(|e| e.event == "content_block_delta"
            && e.data["delta"]["type"] == "input_json_delta");
        assert!(delta.is_some(), "截断收尾应 flush 残留 tool input");
        assert_eq!(delta.unwrap().data["delta"]["partial_json"], r#"{"a":1}"#);
        // 块应被关闭
        assert!(finals.iter().any(|e| e.event == "content_block_stop"), "截断应关闭 tool 块");
    }

    #[test]
    fn test_text_delta_after_tool_use_restarts_text_block() {
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, false, HashMap::new());

        let initial_events = ctx.generate_initial_events();
        assert!(
            initial_events
                .iter()
                .any(|e| e.event == "content_block_start"
                    && e.data["content_block"]["type"] == "text")
        );

        let initial_text_index = ctx
            .text_block_index
            .expect("initial text block index should exist");

        // tool_use 开始会自动关闭现有 text block
        let tool_events = ctx.process_tool_use(&crate::kiro::model::events::ToolUseEvent {
            name: "test_tool".to_string(),
            tool_use_id: "tool_1".to_string(),
            input: "{}".to_string(),
            stop: false,
        });
        assert!(
            tool_events.iter().any(|e| {
                e.event == "content_block_stop"
                    && e.data["index"].as_i64() == Some(initial_text_index as i64)
            }),
            "tool_use should stop the previous text block"
        );

        // 之后再来文本增量，应自动创建新的 text block 而不是往已 stop 的块里写 delta
        let text_events = ctx.process_assistant_response("hello");
        let new_text_start_index = text_events.iter().find_map(|e| {
            if e.event == "content_block_start" && e.data["content_block"]["type"] == "text" {
                e.data["index"].as_i64()
            } else {
                None
            }
        });
        assert!(
            new_text_start_index.is_some(),
            "should start a new text block"
        );
        assert_ne!(
            new_text_start_index.unwrap(),
            initial_text_index as i64,
            "new text block index should differ from the stopped one"
        );
        assert!(
            text_events.iter().any(|e| {
                e.event == "content_block_delta"
                    && e.data["delta"]["type"] == "text_delta"
                    && e.data["delta"]["text"] == "hello"
            }),
            "should emit text_delta after restarting text block"
        );
    }

    #[test]
    fn test_tool_use_flushes_pending_thinking_buffer_text_before_tool_block() {
        // thinking 模式下，短文本可能被暂存在 thinking_buffer 以等待 `<thinking>` 的跨 chunk 匹配。
        // 当紧接着出现 tool_use 时，应先 flush 这段文本，再开始 tool_use block。
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new());
        let _initial_events = ctx.generate_initial_events();

        // 两段短文本（各 2 个中文字符），总长度仍可能不足以满足 safe_len>0 的输出条件，
        // 因而会留在 thinking_buffer 中等待后续 chunk。
        let ev1 = ctx.process_assistant_response("有修");
        assert!(
            ev1.iter().all(|e| e.event != "content_block_delta"),
            "short prefix should be buffered under thinking mode"
        );
        let ev2 = ctx.process_assistant_response("改：");
        assert!(
            ev2.iter().all(|e| e.event != "content_block_delta"),
            "short prefix should still be buffered under thinking mode"
        );

        let events = ctx.process_tool_use(&crate::kiro::model::events::ToolUseEvent {
            name: "Write".to_string(),
            tool_use_id: "tool_1".to_string(),
            input: "{}".to_string(),
            stop: false,
        });

        let text_start_index = events.iter().find_map(|e| {
            if e.event == "content_block_start" && e.data["content_block"]["type"] == "text" {
                e.data["index"].as_i64()
            } else {
                None
            }
        });
        let pos_text_delta = events.iter().position(|e| {
            e.event == "content_block_delta" && e.data["delta"]["type"] == "text_delta"
        });
        let pos_text_stop = text_start_index.and_then(|idx| {
            events.iter().position(|e| {
                e.event == "content_block_stop" && e.data["index"].as_i64() == Some(idx)
            })
        });
        let pos_tool_start = events.iter().position(|e| {
            e.event == "content_block_start" && e.data["content_block"]["type"] == "tool_use"
        });

        assert!(
            text_start_index.is_some(),
            "should start a text block to flush buffered text"
        );
        assert!(
            pos_text_delta.is_some(),
            "should flush buffered text as text_delta"
        );
        assert!(
            pos_text_stop.is_some(),
            "should stop text block before tool_use block starts"
        );
        assert!(pos_tool_start.is_some(), "should start tool_use block");

        let pos_text_delta = pos_text_delta.unwrap();
        let pos_text_stop = pos_text_stop.unwrap();
        let pos_tool_start = pos_tool_start.unwrap();

        assert!(
            pos_text_delta < pos_text_stop && pos_text_stop < pos_tool_start,
            "ordering should be: text_delta -> text_stop -> tool_use_start"
        );

        assert!(
            events.iter().any(|e| {
                e.event == "content_block_delta"
                    && e.data["delta"]["type"] == "text_delta"
                    && e.data["delta"]["text"] == "有修改："
            }),
            "flushed text should equal the buffered prefix"
        );
    }

    #[test]
    fn test_estimate_tokens() {
        assert!(estimate_tokens("Hello") > 0);
        assert!(estimate_tokens("你好") > 0);
        assert!(estimate_tokens("Hello 你好") > 0);
    }

    #[test]
    fn test_find_real_thinking_start_tag_basic() {
        // 基本情况：正常的开始标签
        assert_eq!(find_real_thinking_start_tag("<thinking>"), Some(0));
        assert_eq!(find_real_thinking_start_tag("prefix<thinking>"), Some(6));
    }

    #[test]
    fn test_find_real_thinking_start_tag_with_backticks() {
        // 被反引号包裹的应该被跳过
        assert_eq!(find_real_thinking_start_tag("`<thinking>`"), None);
        assert_eq!(find_real_thinking_start_tag("use `<thinking>` tag"), None);

        // 先有被包裹的，后有真正的开始标签
        assert_eq!(
            find_real_thinking_start_tag("about `<thinking>` tag<thinking>content"),
            Some(22)
        );
    }

    #[test]
    fn test_find_real_thinking_start_tag_with_quotes() {
        // 被双引号包裹的应该被跳过
        assert_eq!(find_real_thinking_start_tag("\"<thinking>\""), None);
        assert_eq!(find_real_thinking_start_tag("the \"<thinking>\" tag"), None);

        // 被单引号包裹的应该被跳过
        assert_eq!(find_real_thinking_start_tag("'<thinking>'"), None);

        // 混合情况
        assert_eq!(
            find_real_thinking_start_tag("about \"<thinking>\" and '<thinking>' then<thinking>"),
            Some(40)
        );
    }

    #[test]
    fn test_find_real_thinking_end_tag_basic() {
        // 基本情况：正常的结束标签后面有双换行符
        assert_eq!(find_real_thinking_end_tag("</thinking>\n\n"), Some(0));
        assert_eq!(
            find_real_thinking_end_tag("content</thinking>\n\n"),
            Some(7)
        );
        assert_eq!(
            find_real_thinking_end_tag("some text</thinking>\n\nmore text"),
            Some(9)
        );

        // 没有双换行符的情况
        assert_eq!(find_real_thinking_end_tag("</thinking>"), None);
        assert_eq!(find_real_thinking_end_tag("</thinking>\n"), None);
        assert_eq!(find_real_thinking_end_tag("</thinking> more"), None);
    }

    #[test]
    fn test_find_real_thinking_end_tag_with_backticks() {
        // 被反引号包裹的应该被跳过
        assert_eq!(find_real_thinking_end_tag("`</thinking>`\n\n"), None);
        assert_eq!(
            find_real_thinking_end_tag("mention `</thinking>` in code\n\n"),
            None
        );

        // 只有前面有反引号
        assert_eq!(find_real_thinking_end_tag("`</thinking>\n\n"), None);

        // 只有后面有反引号
        assert_eq!(find_real_thinking_end_tag("</thinking>`\n\n"), None);
    }

    #[test]
    fn test_find_real_thinking_end_tag_with_quotes() {
        // 被双引号包裹的应该被跳过
        assert_eq!(find_real_thinking_end_tag("\"</thinking>\"\n\n"), None);
        assert_eq!(
            find_real_thinking_end_tag("the string \"</thinking>\" is a tag\n\n"),
            None
        );

        // 被单引号包裹的应该被跳过
        assert_eq!(find_real_thinking_end_tag("'</thinking>'\n\n"), None);
        assert_eq!(
            find_real_thinking_end_tag("use '</thinking>' as marker\n\n"),
            None
        );

        // 混合情况：双引号包裹后有真正的标签
        assert_eq!(
            find_real_thinking_end_tag("about \"</thinking>\" tag</thinking>\n\n"),
            Some(23)
        );

        // 混合情况：单引号包裹后有真正的标签
        assert_eq!(
            find_real_thinking_end_tag("about '</thinking>' tag</thinking>\n\n"),
            Some(23)
        );
    }

    #[test]
    fn test_find_real_thinking_end_tag_mixed() {
        // 先有被包裹的，后有真正的结束标签
        assert_eq!(
            find_real_thinking_end_tag("discussing `</thinking>` tag</thinking>\n\n"),
            Some(28)
        );

        // 多个被包裹的，最后一个是真正的
        assert_eq!(
            find_real_thinking_end_tag("`</thinking>` and `</thinking>` done</thinking>\n\n"),
            Some(36)
        );

        // 多种引用字符混合
        assert_eq!(
            find_real_thinking_end_tag(
                "`</thinking>` and \"</thinking>\" and '</thinking>' done</thinking>\n\n"
            ),
            Some(54)
        );
    }

    #[test]
    fn test_tool_use_immediately_after_thinking_filters_end_tag_and_closes_thinking_block() {
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new());
        let _initial_events = ctx.generate_initial_events();

        let mut all_events = Vec::new();

        // thinking 内容以 `</thinking>` 结尾，但后面没有 `\n\n`（模拟紧跟 tool_use 的场景）
        all_events.extend(ctx.process_assistant_response("<thinking>abc</thinking>"));

        let tool_events = ctx.process_tool_use(&crate::kiro::model::events::ToolUseEvent {
            name: "Write".to_string(),
            tool_use_id: "tool_1".to_string(),
            input: "{}".to_string(),
            stop: false,
        });
        all_events.extend(tool_events);

        all_events.extend(ctx.generate_final_events());

        // 不应把 `</thinking>` 当作 thinking 内容输出
        assert!(
            all_events.iter().all(|e| {
                !(e.event == "content_block_delta"
                    && e.data["delta"]["type"] == "thinking_delta"
                    && e.data["delta"]["thinking"] == "</thinking>")
            }),
            "`</thinking>` should be filtered from output"
        );

        // thinking block 必须在 tool_use block 之前关闭
        let thinking_index = ctx
            .thinking_block_index
            .expect("thinking block index should exist");
        let pos_thinking_stop = all_events.iter().position(|e| {
            e.event == "content_block_stop"
                && e.data["index"].as_i64() == Some(thinking_index as i64)
        });
        let pos_tool_start = all_events.iter().position(|e| {
            e.event == "content_block_start" && e.data["content_block"]["type"] == "tool_use"
        });
        assert!(
            pos_thinking_stop.is_some(),
            "thinking block should be stopped"
        );
        assert!(pos_tool_start.is_some(), "tool_use block should be started");
        assert!(
            pos_thinking_stop.unwrap() < pos_tool_start.unwrap(),
            "thinking block should stop before tool_use block starts"
        );
    }

    #[test]
    fn test_final_flush_filters_standalone_thinking_end_tag() {
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new());
        let _initial_events = ctx.generate_initial_events();

        let mut all_events = Vec::new();
        all_events.extend(ctx.process_assistant_response("<thinking>abc</thinking>"));
        all_events.extend(ctx.generate_final_events());

        assert!(
            all_events.iter().all(|e| {
                !(e.event == "content_block_delta"
                    && e.data["delta"]["type"] == "thinking_delta"
                    && e.data["delta"]["thinking"] == "</thinking>")
            }),
            "`</thinking>` should be filtered during final flush"
        );
    }

    #[test]
    fn test_thinking_strips_leading_newline_same_chunk() {
        // <thinking>\n 在同一个 chunk 中，\n 应被剥离
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new());
        let _initial_events = ctx.generate_initial_events();

        let events = ctx.process_assistant_response("<thinking>\nHello world");

        // 找到所有 thinking_delta 事件
        let thinking_deltas: Vec<_> = events
            .iter()
            .filter(|e| {
                e.event == "content_block_delta" && e.data["delta"]["type"] == "thinking_delta"
            })
            .collect();

        // 拼接所有 thinking 内容
        let full_thinking: String = thinking_deltas
            .iter()
            .map(|e| e.data["delta"]["thinking"].as_str().unwrap_or(""))
            .collect();

        assert!(
            !full_thinking.starts_with('\n'),
            "thinking content should not start with \\n, got: {:?}",
            full_thinking
        );
    }

    #[test]
    fn test_thinking_strips_leading_newline_cross_chunk() {
        // <thinking> 在第一个 chunk 末尾，\n 在第二个 chunk 开头
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new());
        let _initial_events = ctx.generate_initial_events();

        let events1 = ctx.process_assistant_response("<thinking>");
        let events2 = ctx.process_assistant_response("\nHello world");

        let mut all_events = Vec::new();
        all_events.extend(events1);
        all_events.extend(events2);

        let thinking_deltas: Vec<_> = all_events
            .iter()
            .filter(|e| {
                e.event == "content_block_delta" && e.data["delta"]["type"] == "thinking_delta"
            })
            .collect();

        let full_thinking: String = thinking_deltas
            .iter()
            .map(|e| e.data["delta"]["thinking"].as_str().unwrap_or(""))
            .collect();

        assert!(
            !full_thinking.starts_with('\n'),
            "thinking content should not start with \\n across chunks, got: {:?}",
            full_thinking
        );
    }

    #[test]
    fn test_thinking_no_strip_when_no_leading_newline() {
        // <thinking> 后直接跟内容（无 \n），内容应完整保留
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new());
        let _initial_events = ctx.generate_initial_events();

        let events = ctx.process_assistant_response("<thinking>abc</thinking>\n\ntext");

        let thinking_deltas: Vec<_> = events
            .iter()
            .filter(|e| {
                e.event == "content_block_delta" && e.data["delta"]["type"] == "thinking_delta"
            })
            .collect();

        let full_thinking: String = thinking_deltas
            .iter()
            .filter(|e| !e.data["delta"]["thinking"].as_str().unwrap_or("").is_empty())
            .map(|e| e.data["delta"]["thinking"].as_str().unwrap_or(""))
            .collect();

        assert_eq!(full_thinking, "abc", "thinking content should be 'abc'");
    }

    #[test]
    fn test_text_after_thinking_strips_leading_newlines() {
        // `</thinking>\n\n` 后的文本不应以 \n\n 开头
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new());
        let _initial_events = ctx.generate_initial_events();

        let events =
            ctx.process_assistant_response("<thinking>\nabc</thinking>\n\n你好");

        let text_deltas: Vec<_> = events
            .iter()
            .filter(|e| {
                e.event == "content_block_delta" && e.data["delta"]["type"] == "text_delta"
            })
            .collect();

        let full_text: String = text_deltas
            .iter()
            .map(|e| e.data["delta"]["text"].as_str().unwrap_or(""))
            .collect();

        assert!(
            !full_text.starts_with('\n'),
            "text after thinking should not start with \\n, got: {:?}",
            full_text
        );
        assert_eq!(full_text, "你好");
    }

    /// 辅助函数：从事件列表中提取所有 thinking_delta 的拼接内容
    fn collect_thinking_content(events: &[SseEvent]) -> String {
        events
            .iter()
            .filter(|e| {
                e.event == "content_block_delta" && e.data["delta"]["type"] == "thinking_delta"
            })
            .map(|e| e.data["delta"]["thinking"].as_str().unwrap_or(""))
            .filter(|s| !s.is_empty())
            .collect()
    }

    /// 辅助函数：从事件列表中提取所有 text_delta 的拼接内容
    fn collect_text_content(events: &[SseEvent]) -> String {
        events
            .iter()
            .filter(|e| {
                e.event == "content_block_delta" && e.data["delta"]["type"] == "text_delta"
            })
            .map(|e| e.data["delta"]["text"].as_str().unwrap_or(""))
            .collect()
    }

    #[test]
    fn test_end_tag_newlines_split_across_events() {
        // `</thinking>\n` 在 chunk 1，`\n` 在 chunk 2，`text` 在 chunk 3
        // 确保 `</thinking>` 不会被部分当作 thinking 内容发出
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new());
        let _initial_events = ctx.generate_initial_events();

        let mut all = Vec::new();
        all.extend(ctx.process_assistant_response("<thinking>\nabc</thinking>\n"));
        all.extend(ctx.process_assistant_response("\n"));
        all.extend(ctx.process_assistant_response("你好"));
        all.extend(ctx.generate_final_events());

        let thinking = collect_thinking_content(&all);
        assert_eq!(thinking, "abc", "thinking should be 'abc', got: {:?}", thinking);

        let text = collect_text_content(&all);
        assert_eq!(text, "你好", "text should be '你好', got: {:?}", text);
    }

    #[test]
    fn test_end_tag_alone_in_chunk_then_newlines_in_next() {
        // `</thinking>` 单独在一个 chunk，`\n\ntext` 在下一个 chunk
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new());
        let _initial_events = ctx.generate_initial_events();

        let mut all = Vec::new();
        all.extend(ctx.process_assistant_response("<thinking>\nabc</thinking>"));
        all.extend(ctx.process_assistant_response("\n\n你好"));
        all.extend(ctx.generate_final_events());

        let thinking = collect_thinking_content(&all);
        assert_eq!(thinking, "abc", "thinking should be 'abc', got: {:?}", thinking);

        let text = collect_text_content(&all);
        assert_eq!(text, "你好", "text should be '你好', got: {:?}", text);
    }

    #[test]
    fn test_start_tag_newline_split_across_events() {
        // `\n\n` 在 chunk 1，`<thinking>` 在 chunk 2，`\n` 在 chunk 3
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new());
        let _initial_events = ctx.generate_initial_events();

        let mut all = Vec::new();
        all.extend(ctx.process_assistant_response("\n\n"));
        all.extend(ctx.process_assistant_response("<thinking>"));
        all.extend(ctx.process_assistant_response("\n"));
        all.extend(ctx.process_assistant_response("abc</thinking>\n\ntext"));
        all.extend(ctx.generate_final_events());

        let thinking = collect_thinking_content(&all);
        assert_eq!(thinking, "abc", "thinking should be 'abc', got: {:?}", thinking);

        let text = collect_text_content(&all);
        assert_eq!(text, "text", "text should be 'text', got: {:?}", text);
    }

    #[test]
    fn test_full_flow_maximally_split() {
        // 极端拆分：每个关键边界都在不同 chunk
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new());
        let _initial_events = ctx.generate_initial_events();

        let mut all = Vec::new();
        // \n\n<thinking>\n 拆成多段
        all.extend(ctx.process_assistant_response("\n"));
        all.extend(ctx.process_assistant_response("\n"));
        all.extend(ctx.process_assistant_response("<thin"));
        all.extend(ctx.process_assistant_response("king>"));
        all.extend(ctx.process_assistant_response("\n"));
        all.extend(ctx.process_assistant_response("hello"));
        // </thinking>\n\n 拆成多段
        all.extend(ctx.process_assistant_response("</thi"));
        all.extend(ctx.process_assistant_response("nking>"));
        all.extend(ctx.process_assistant_response("\n"));
        all.extend(ctx.process_assistant_response("\n"));
        all.extend(ctx.process_assistant_response("world"));
        all.extend(ctx.generate_final_events());

        let thinking = collect_thinking_content(&all);
        assert_eq!(thinking, "hello", "thinking should be 'hello', got: {:?}", thinking);

        let text = collect_text_content(&all);
        assert_eq!(text, "world", "text should be 'world', got: {:?}", text);
    }

    #[test]
    fn test_thinking_only_sets_max_tokens_stop_reason() {
        // 整个流只有 thinking 块，没有 text 也没有 tool_use，stop_reason 应为 max_tokens
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new());
        let _initial_events = ctx.generate_initial_events();

        let mut all_events = Vec::new();
        all_events.extend(ctx.process_assistant_response("<thinking>\nabc</thinking>"));
        all_events.extend(ctx.generate_final_events());

        let message_delta = all_events
            .iter()
            .find(|e| e.event == "message_delta")
            .expect("should have message_delta event");

        assert_eq!(
            message_delta.data["delta"]["stop_reason"], "max_tokens",
            "stop_reason should be max_tokens when only thinking is produced"
        );

        // 应补发一套完整的 text 事件（content_block_start + delta 空格 + content_block_stop）
        assert!(
            all_events.iter().any(|e| {
                e.event == "content_block_start" && e.data["content_block"]["type"] == "text"
            }),
            "should emit text content_block_start"
        );
        assert!(
            all_events.iter().any(|e| {
                e.event == "content_block_delta"
                    && e.data["delta"]["type"] == "text_delta"
                    && e.data["delta"]["text"] == " "
            }),
            "should emit text_delta with a single space"
        );
        // text block 应被 generate_final_events 自动关闭
        let text_block_index = all_events
            .iter()
            .find_map(|e| {
                if e.event == "content_block_start" && e.data["content_block"]["type"] == "text" {
                    e.data["index"].as_i64()
                } else {
                    None
                }
            })
            .expect("text block should exist");
        assert!(
            all_events.iter().any(|e| {
                e.event == "content_block_stop"
                    && e.data["index"].as_i64() == Some(text_block_index)
            }),
            "text block should be stopped"
        );
    }

    #[test]
    fn test_thinking_with_text_keeps_end_turn_stop_reason() {
        // thinking + text 的情况，stop_reason 应为 end_turn
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new());
        let _initial_events = ctx.generate_initial_events();

        let mut all_events = Vec::new();
        all_events.extend(ctx.process_assistant_response("<thinking>\nabc</thinking>\n\nHello"));
        all_events.extend(ctx.generate_final_events());

        let message_delta = all_events
            .iter()
            .find(|e| e.event == "message_delta")
            .expect("should have message_delta event");

        assert_eq!(
            message_delta.data["delta"]["stop_reason"], "end_turn",
            "stop_reason should be end_turn when text is also produced"
        );
    }

    #[test]
    fn test_thinking_with_tool_use_keeps_tool_use_stop_reason() {
        // thinking + tool_use 的情况，stop_reason 应为 tool_use
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new());
        let _initial_events = ctx.generate_initial_events();

        let mut all_events = Vec::new();
        all_events.extend(ctx.process_assistant_response("<thinking>\nabc</thinking>"));
        all_events.extend(ctx.process_tool_use(&crate::kiro::model::events::ToolUseEvent {
            name: "test_tool".to_string(),
            tool_use_id: "tool_1".to_string(),
            input: "{}".to_string(),
            stop: true,
        }));
        all_events.extend(ctx.generate_final_events());

        let message_delta = all_events
            .iter()
            .find(|e| e.event == "message_delta")
            .expect("should have message_delta event");

        assert_eq!(
            message_delta.data["delta"]["stop_reason"], "tool_use",
            "stop_reason should be tool_use when tool_use is present"
        );
    }

    /// B3 回归：流式 thinking 块结束前必须发一个非空 signature_delta，且排在
    /// content_block_stop 之前。否则客户端下一轮回传时本地校验失败报错
    /// "The content[].thinking in the thinking mode must be passed back"。
    #[test]
    fn test_thinking_block_emits_signature_delta_before_stop() {
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new());
        let _initial_events = ctx.generate_initial_events();

        let mut all_events = Vec::new();
        all_events.extend(ctx.process_assistant_response("<thinking>\nabc</thinking>\n\nHello"));
        all_events.extend(ctx.generate_final_events());

        let thinking_index = ctx
            .thinking_block_index
            .expect("thinking block index should exist");

        // 存在一个非空 signature_delta，index 指向 thinking 块
        let pos_sig = all_events.iter().position(|e| {
            e.event == "content_block_delta"
                && e.data["delta"]["type"] == "signature_delta"
                && e.data["delta"]["signature"]
                    .as_str()
                    .map(|s| !s.is_empty())
                    .unwrap_or(false)
                && e.data["index"].as_i64() == Some(thinking_index as i64)
        });
        assert!(pos_sig.is_some(), "应发出非空 signature_delta");

        // signature_delta 必须排在 thinking 块的 content_block_stop 之前
        let pos_stop = all_events.iter().position(|e| {
            e.event == "content_block_stop"
                && e.data["index"].as_i64() == Some(thinking_index as i64)
        });
        assert!(pos_stop.is_some(), "thinking 块应被关闭");
        assert!(
            pos_sig.unwrap() < pos_stop.unwrap(),
            "signature_delta 必须排在 content_block_stop 之前"
        );
    }

    // ============ 「截断即成功」修复：CompletionStatus 回归 ============

    #[test]
    fn test_completion_status_outcome_and_http_mapping() {
        // 上游限流类错误 → RateLimited / overloaded_error / 429
        let rl = CompletionStatus::UpstreamError {
            code: "ThrottlingException".to_string(),
            message: "rate exceeded".to_string(),
        };
        assert_eq!(rl.outcome(), RequestOutcome::RateLimited);
        assert_eq!(rl.sse_error_type(), "overloaded_error");
        assert_eq!(rl.http_status_u16(), 429);

        // 普通上游错误 → ServerError / api_error / 502
        let se = CompletionStatus::UpstreamError {
            code: "InternalServerException".to_string(),
            message: "boom".to_string(),
        };
        assert_eq!(se.outcome(), RequestOutcome::ServerError);
        assert_eq!(se.sse_error_type(), "api_error");
        assert_eq!(se.http_status_u16(), 502);

        // 传输中断 → NetworkError / 502
        let te = CompletionStatus::TransportError {
            message: "connection reset".to_string(),
        };
        assert_eq!(te.outcome(), RequestOutcome::NetworkError);
        assert_eq!(te.http_status_u16(), 502);

        // 解码器停止 → ServerError / 502
        let ds = CompletionStatus::DecoderStopped {
            message: "too many errors".to_string(),
        };
        assert_eq!(ds.outcome(), RequestOutcome::ServerError);
        assert_eq!(ds.http_status_u16(), 502);

        // Ok → Success
        assert_eq!(CompletionStatus::Ok.outcome(), RequestOutcome::Success);
        assert!(CompletionStatus::Ok.is_ok());
    }

    #[test]
    fn test_inband_error_event_sets_failure_and_emits_error_event() {
        // 回归 BUG①/③：in-band Event::Error 应内联发 SSE error 事件，并把 completion 置失败态，
        // 使收尾 outcome 不再是硬编码的 Success。
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, false, HashMap::new());
        let _ = ctx.generate_initial_events();

        let events = ctx.process_kiro_event(&Event::Error {
            error_code: "InternalServerException".to_string(),
            error_message: "upstream boom".to_string(),
        });

        // 内联发出了 error 事件
        assert!(
            events.iter().any(|e| e.event == "error"
                && e.data["error"]["type"] == "api_error"),
            "in-band 错误应内联发出 SSE error 事件"
        );
        assert!(ctx.error_event_emitted(), "应标记已发 error 事件");
        // completion 置为失败态，outcome 不再是 Success
        assert!(!ctx.completion().is_ok());
        assert_eq!(ctx.completion_outcome(), RequestOutcome::ServerError);
    }

    #[test]
    fn test_content_length_exceeded_is_not_failure() {
        // 铁律：ContentLengthExceededException = max_tokens 干净收尾，绝不算失败。
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, false, HashMap::new());
        let _ = ctx.generate_initial_events();

        let events = ctx.process_kiro_event(&Event::Exception {
            exception_type: "ContentLengthExceededException".to_string(),
            message: "max tokens".to_string(),
        });

        assert!(events.is_empty(), "CL 异常不应发 error 事件");
        assert!(ctx.completion().is_ok(), "CL 异常不应置失败态");
        assert_eq!(ctx.completion_outcome(), RequestOutcome::Success);
        assert!(!ctx.error_event_emitted());
    }

    #[test]
    fn test_non_cl_exception_marks_failure_and_emits_error() {
        // 非 CL 异常是上游真实失败：置失败态 + 内联发 error 事件。
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, false, HashMap::new());
        let _ = ctx.generate_initial_events();

        let events = ctx.process_kiro_event(&Event::Exception {
            exception_type: "ThrottlingException".to_string(),
            message: "slow down".to_string(),
        });

        assert!(
            events.iter().any(|e| e.event == "error"
                && e.data["error"]["type"] == "overloaded_error"),
            "限流类异常应发 overloaded_error"
        );
        assert_eq!(ctx.completion_outcome(), RequestOutcome::RateLimited);
    }

    #[test]
    fn test_mark_transport_and_decoder_stopped_are_idempotent() {
        // 传输中断 / 解码器停止的 setter 应置失败态，且幂等保留首因。
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, false, HashMap::new());
        let _ = ctx.generate_initial_events();

        ctx.mark_transport_error("reset");
        assert_eq!(ctx.completion_outcome(), RequestOutcome::NetworkError);
        // 首因已定，后续 mark 不覆盖
        ctx.mark_decoder_stopped("later error");
        assert_eq!(
            ctx.completion_outcome(),
            RequestOutcome::NetworkError,
            "幂等：应保留首个失败原因"
        );
    }

    #[test]
    fn test_buffered_context_delegates_completion() {
        // BufferedStreamContext 应把完成状态透传给内部 StreamContext。
        let mut ctx = BufferedStreamContext::new("test-model", 1, false, HashMap::new());
        ctx.process_and_buffer(&Event::Error {
            error_code: "InternalServerException".to_string(),
            error_message: "boom".to_string(),
        });
        assert!(!ctx.completion().is_ok());
        assert_eq!(ctx.completion_outcome(), RequestOutcome::ServerError);
        assert!(ctx.error_event_emitted());
    }

    // ==================== merge_tool_input 决策表回归（Invalid tool parameters 类型 C 根治） ====================

    /// 累积快照三帧：每帧是"到目前为止的完整 JSON" → 最终取最后最全的一帧。
    #[test]
    fn test_merge_cumulative_snapshots() {
        let mut buf = String::new();
        buf = merge_tool_input(&buf, r#"{"path""#);
        buf = merge_tool_input(&buf, r#"{"path":"a.txt""#);
        buf = merge_tool_input(&buf, r#"{"path":"a.txt","content":"hi"}"#);
        assert_eq!(buf, r#"{"path":"a.txt","content":"hi"}"#);
        assert!(serde_json::from_str::<serde_json::Value>(&buf).is_ok());
    }

    /// 纯增量碎片三帧：每帧只带新片段 → 追加拼成完整 JSON。
    #[test]
    fn test_merge_pure_increments() {
        let mut buf = String::new();
        buf = merge_tool_input(&buf, r#"{"path":"#);
        buf = merge_tool_input(&buf, r#""a.txt","content""#);
        buf = merge_tool_input(&buf, r#":"hi"}"#);
        assert_eq!(buf, r#"{"path":"a.txt","content":"hi"}"#);
        assert!(serde_json::from_str::<serde_json::Value>(&buf).is_ok());
    }

    /// 重复终帧：同一完整快照来两次 → 不翻倍。
    #[test]
    fn test_merge_duplicate_final_frame() {
        let full = r#"{"a":1,"b":2}"#;
        let mut buf = String::new();
        buf = merge_tool_input(&buf, full);
        buf = merge_tool_input(&buf, full);
        assert_eq!(buf, full, "重复终帧不应翻倍");
    }

    /// 核心：两个各自完整、彼此非前缀的对象 → 结果是第二帧，而不是 `{"a":1}{"a":2}` 粘连串。
    #[test]
    fn test_merge_nonprefix_double_object_keeps_latest() {
        let buf = merge_tool_input(r#"{"a":1}"#, r#"{"a":2}"#);
        assert_eq!(buf, r#"{"a":2}"#, "非前缀双完整对象应只留最新，消灭 object 粘连");
        assert!(
            serde_json::from_str::<serde_json::Value>(&buf).is_ok(),
            "结果必须是合法 JSON"
        );
    }

    /// 完整对象之后来一个更短的旧前缀快照 → 保持完整，不被旧短帧覆盖。
    #[test]
    fn test_merge_full_then_shorter_prefix_kept() {
        let full = r#"{"path":"a.txt","content":"hi"}"#;
        let buf = merge_tool_input(full, r#"{"path""#);
        assert_eq!(buf, full, "迟到的旧短前缀快照应被丢弃，保留更全缓冲");
    }

    /// 空帧不改变缓冲；空缓冲取本帧。
    #[test]
    fn test_merge_empty_edges() {
        assert_eq!(merge_tool_input("abc", ""), "abc", "空帧 → 缓冲不变");
        assert_eq!(merge_tool_input("", "abc"), "abc", "空缓冲 → 取本帧");
        assert_eq!(merge_tool_input("", ""), "", "双空 → 空");
    }

    /// 真增量碎片（各帧本身非法）→ append 后拼成合法整体，不被第 6 步误判。
    #[test]
    fn test_merge_illegal_fragments_append() {
        // 第一帧是未闭合的合法前缀，第二帧续上闭合：两者都不是完整 JSON → 走追加。
        let buf = merge_tool_input(r#"{"x":[1,2"#, r#",3]}"#);
        assert_eq!(buf, r#"{"x":[1,2,3]}"#);
        assert!(serde_json::from_str::<serde_json::Value>(&buf).is_ok());
    }

    /// process_tool_use 端到端：非前缀双对象场景，flush 出的 partial_json 必须是合法 JSON（第二帧）。
    #[test]
    fn test_process_tool_use_nonprefix_double_object_emits_legal_json() {
        use crate::kiro::model::events::ToolUseEvent;
        let mut ctx =
            StreamContext::new_with_thinking("claude-sonnet-4.6", 1, false, HashMap::new());
        let _ = ctx.generate_initial_events();
        let _ = ctx.process_tool_use(&ToolUseEvent {
            name: "AskUserQuestion".to_string(),
            tool_use_id: "toolu_np".to_string(),
            input: r#"{"a":1}"#.to_string(),
            stop: false,
        });
        let evs = ctx.process_tool_use(&ToolUseEvent {
            name: "AskUserQuestion".to_string(),
            tool_use_id: "toolu_np".to_string(),
            input: r#"{"a":2}"#.to_string(),
            stop: true,
        });
        let delta = evs
            .iter()
            .find(|e| {
                e.event == "content_block_delta"
                    && e.data["delta"]["type"] == "input_json_delta"
            })
            .expect("应有 input_json_delta");
        let assembled = delta.data["delta"]["partial_json"].as_str().unwrap();
        assert_eq!(assembled, r#"{"a":2}"#, "非前缀双对象只发最新完整对象");
        assert!(
            serde_json::from_str::<serde_json::Value>(assembled).is_ok(),
            "流式 flush 的 partial_json 必须合法"
        );
    }

    /// 隔离回归：tool_use input 含小于号 + 全角竖线 DSML 起始标记（U+FF5C）时，
    /// 拼装完全不经过 strip_dsml_markers，原样保留（Claude 系）。
    #[test]
    fn test_tool_input_not_stripped_by_dsml_claude() {
        use crate::kiro::model::events::ToolUseEvent;
        let mut ctx =
            StreamContext::new_with_thinking("claude-sonnet-4.6", 1, false, HashMap::new());
        let _ = ctx.generate_initial_events();
        let payload = r#"{"code":"if a < b","note":"<｜DSML｜function_calls｜>","x":"a<｜tool"}"#;
        let evs = ctx.process_tool_use(&ToolUseEvent {
            name: "Write".to_string(),
            tool_use_id: "toolu_d".to_string(),
            input: payload.to_string(),
            stop: true,
        });
        let delta = evs
            .iter()
            .find(|e| {
                e.event == "content_block_delta"
                    && e.data["delta"]["type"] == "input_json_delta"
            })
            .expect("delta");
        let assembled = delta.data["delta"]["partial_json"].as_str().unwrap();
        assert_eq!(assembled, payload, "tool input 应原样保留，DSML 未碰");
    }

    /// 隔离回归：国产模型（deepseek）下 DSML 门控放行，但 tool input 拼装路径同样不经过剥离。
    #[test]
    fn test_tool_input_not_stripped_by_dsml_deepseek() {
        use crate::kiro::model::events::ToolUseEvent;
        let mut ctx = StreamContext::new_with_thinking("deepseek-v3", 1, false, HashMap::new());
        let _ = ctx.generate_initial_events();
        let payload = r#"{"code":"if a < b","note":"<｜DSML｜function_calls｜>"}"#;
        let evs = ctx.process_tool_use(&ToolUseEvent {
            name: "Write".to_string(),
            tool_use_id: "toolu_d2".to_string(),
            input: payload.to_string(),
            stop: true,
        });
        let delta = evs
            .iter()
            .find(|e| {
                e.event == "content_block_delta"
                    && e.data["delta"]["type"] == "input_json_delta"
            })
            .expect("delta");
        let assembled = delta.data["delta"]["partial_json"].as_str().unwrap();
        assert_eq!(
            assembled, payload,
            "国产模型下 tool input 也应原样，DSML 只作用于 text/thinking"
        );
    }
}
