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
    /// 泄漏 token 清洗（`tool_clean_leaked_tokens` 开启时）：当前文本处理位置是否在**行首/块首**。
    /// 泄漏控制 token（course/課/count/care）只出现在行首且与后文无空格粘连，故只在行首尝试剥离，
    /// 正常正文里的这些词（有空格分隔）绝不误删。初始 true（响应开头即行首），每段文本按是否以
    /// 换行结尾更新。非持久跨请求，随 StreamContext 生命周期。
    at_line_start: bool,
    /// 泄漏 token 诊断（可观测，不影响清洗剥离判据）：本请求累计真剥掉的泄漏 token 数。
    leaked_stripped: u32,
    /// 本请求检测到的 saturation 泄漏行数（整行就是纯泄漏词的行，#70544 整段退化的信号）。
    leaked_saturation_lines: u32,
    /// 本请求检测到「文本化工具调用」的 chunk 数(assistantResponseEvent 文本流里出现 <invoke/antml:/
    /// <parameter 标记)。这是决定要不要做 R4 重组层的取证依据——无条件累加(不受 KIRO_INVOKE_TRACE 限),
    /// 收尾经 recovery_metrics 暴露。
    textified_invoke_hits: u32,
    // ===== 文本化 invoke 重组(R4,移植 ZyphrZero__kiro.rs v0.6.5)=====
    /// 本次请求声明的工具名集合(=模型看到的名字)。重组硬护栏:解析出的工具名必须在此才允许捞回,
    /// 否则当文本吐——宁可漏捞不可把正文讨论的假命令误执行。
    known_tool_names: std::collections::HashSet<String>,
    /// invoke 嗅探缓冲:文本先进这里,决策安全(完整块过四道门 / 确认非泄漏)后才释放。跨 chunk 累积。
    invoke_sniff_buffer: String,
    /// 代码围栏(```)开合状态:围栏内的 <invoke> 是展示代码不捞回。跨 chunk 追踪奇偶。
    code_fence_open: bool,
    /// 围栏扫描的未完成行尾巴(等换行拼齐再判定是否围栏行)。
    fence_scan_partial: String,
    /// stray token(call/count/card/court)连续独占行复读计数,超阈值熔断本轮文本(治退化刷屏)。
    stray_repeat_last: String,
    stray_repeat_run: u32,
    /// 本请求真重组成结构化 tool_use 的次数 + stray 熔断触发次数(可观测)。
    reclaimed_invoke_count: u32,
    stray_guard_tripped: bool,
    /// stray 泄漏形态观测(纯统计不改输出):本请求见过的"独占 stray 行"数 / "句中紧贴 CJK 的 stray 词"数。
    /// 点亮句中泄漏黑洞——决定要不要开保守清洗的取证依据。收尾经 recovery_metrics 暴露。
    stray_standalone_seen: u32,
    stray_inline_seen: u32,
    /// 重组容错总开关(config tool_reclaim_textified_invoke;默认开)。关=退回纯转发(原样吐文本)。
    reclaim_enabled: bool,
}

/// 泄漏 token 剥离的命中信息（诊断计数用，不影响剥离判据）。
#[derive(Debug, Clone, Copy)]
struct StripHit {
    /// 是否真剥掉了泄漏词。
    stripped: bool,
    /// 是否为"独占整行"泄漏（saturation 信号：整行就是纯泄漏词，#70544 整段退化）。
    standalone: bool,
}

impl StripHit {
    fn none() -> Self {
        StripHit { stripped: false, standalone: false }
    }
}

impl StreamContext {
    /// invoke 嗅探缓冲 hold 上限(256 KiB):行首未闭合的 <invoke 累计超此值仍没等到 </invoke>,
    /// 放弃 hold 当普通文本吐,避免永不闭合的半块把流卡死。多行参数(apply_patch)是常态,故按字节非行数。
    const MAX_INVOKE_HOLD_BYTES: usize = 262_144;

    /// 创建启用thinking的StreamContext
    pub fn new_with_thinking(
        model: impl Into<String>,
        input_tokens: i32,
        thinking_enabled: bool,
        tool_name_map: HashMap<String, String>,
    ) -> Self {
        Self::new_full(model, input_tokens, thinking_enabled, tool_name_map, std::collections::HashSet::new())
    }

    /// 完整构造:额外接 known_tool_names(文本化 invoke 重组的工具名硬护栏)。
    /// new_with_thinking 是它的薄封装(空工具集=不启用重组捞回,兼容既有调用/测试)。
    pub fn new_full(
        model: impl Into<String>,
        input_tokens: i32,
        thinking_enabled: bool,
        tool_name_map: HashMap<String, String>,
        known_tool_names: std::collections::HashSet<String>,
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
            at_line_start: true,
            leaked_stripped: 0,
            leaked_saturation_lines: 0,
            textified_invoke_hits: 0,
            known_tool_names,
            invoke_sniff_buffer: String::new(),
            code_fence_open: false,
            fence_scan_partial: String::new(),
            stray_repeat_last: String::new(),
            stray_repeat_run: 0,
            reclaimed_invoke_count: 0,
            stray_guard_tripped: false,
            stray_standalone_seen: 0,
            stray_inline_seen: 0,
            reclaim_enabled: super::handlers::tool_reclaim_textified_invoke_enabled(),
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
    /// 已知的模型泄漏控制/规划 token（#70544 高多字节密度紧邻工具标签时,模型把内部规划 token
    /// 当可见文本吐出,以行首粘连形式漏进输出）。真实日志实测最高频是 `court`(独占整行 202 次),
    /// 及 course/count/care/card/call 粘 CJK。**注意:此现象发生在 Claude/opus 侧,故清洗必须对
    /// Claude 生效**(不能像 DSML 那样门控排除 Claude,否则正好漏掉主战场)。
    /// 删除死条目 `coursecount`(被 `course` 前缀遮蔽,strip_prefix 顺序遍历永不可达)。
    const LEAKED_CONTROL_TOKENS: &'static [&'static str] =
        &["court", "course", "count", "care", "card", "call", "課", "课"];

    /// 判断字符是否为「泄漏粘连」信号:CJK 表意文字 或 全角标点/字符(U+3000..U+303F、U+FF00..U+FFEF、
    /// U+4E00..U+9FFF 等)。**收严关键**:此前用「非空格非小写即剥」过宽,把 `count: 42`(冒号)、
    /// `countDown()`(大写)、`care2share`(数字)这类**正常英文**行首误删。现在只认 CJK/全角——
    /// 正常英文的 ASCII 冒号/数字/大写字母一律不触发,杜绝对 Claude 正文的误删。
    fn is_leak_glue_char(c: char) -> bool {
        let u = c as u32;
        // CJK 统一表意 + 扩展A + 兼容 + 全角/半角形式 + CJK 标点。
        (0x4E00..=0x9FFF).contains(&u)      // CJK 统一表意
            || (0x3400..=0x4DBF).contains(&u) // CJK 扩展 A
            || (0x3000..=0x303F).contains(&u) // CJK 标点(、。「」等)
            || (0xFF00..=0xFFEF).contains(&u) // 全角 ASCII + 全角标点(：！？，等)
            || (0x2E80..=0x2EFF).contains(&u) // CJK 部首补充
    }

    /// 保守清洗行首泄漏的控制 token（`tool_clean_leaked_tokens` 开启时）。
    ///
    /// 只处理**行首**：若文本（在行首位置）以某个已知泄漏词开头，且该词紧邻的下一个字符是
    /// 非 ASCII 字母/非空格（CJK、全角标点、冒号、大写字母跳变等跨类粘连），判定为泄漏并剥掉该词。
    /// 返回清洗后的文本。误删防护：词后是空格或普通 ASCII 小写延续（如 `counter`/`careful`）→ 不剥。
    fn clean_leaked_tokens(&mut self, content: &str) -> String {
        if !self.at_line_start {
            return content.to_string();
        }
        // 逐行处理：只对每行的行首做一次判定（保守，不递归剥多层）。
        let mut out = String::with_capacity(content.len());
        for (i, line) in content.split_inclusive('\n').enumerate() {
            // split_inclusive 保留了行尾 \n；首段是否真在行首由 at_line_start 保证（i==0）
            // 或前一段以 \n 结尾（i>0 必然行首）。
            let is_line_start = i > 0 || self.at_line_start;
            if is_line_start {
                // 诊断（可观测，不改剥离判据）：strip 返回是否命中 + 是否为独占整行泄漏，累加计数。
                let (cleaned, hit) = Self::strip_leaked_prefix(line);
                if hit.stripped {
                    self.leaked_stripped += 1;
                }
                if hit.standalone {
                    self.leaked_saturation_lines += 1;
                }
                out.push_str(&cleaned);
            } else {
                out.push_str(line);
            }
        }
        out
    }

    /// 独占整行即视为泄漏的高置信 token:这几个词在正常英文里**极少独占一整行**,
    /// 但在 #70544 泄漏里恰恰大量独占行(court 实测 202 次全独占行)。仅这几个可"整行即剥",
    /// call/card/count/care/course 独占行可能是正常内容(标题/变量/列表),**不**享此特例。
    const LEAK_STANDALONE_TOKENS: &'static [&'static str] = &["court", "課", "课"];

    /// 剥掉单行行首的一个泄漏词（若命中粘连特征）。返回 (清洗后文本, 命中信息)。
    /// **剥离判据完全不变**（0.7.14 已收严），只额外返回命中标志供诊断计数用。
    fn strip_leaked_prefix(line: &str) -> (String, StripHit) {
        // 先处理"独占整行"特例:整行(去掉行尾 \n / 空白后)恰等于某高置信 token → 泄漏,整行剥空。
        let trimmed = line.trim_end_matches(['\n', '\r', ' ', '\t']);
        for &tok in Self::LEAK_STANDALONE_TOKENS {
            if trimmed == tok {
                // 保留行尾换行(维持行结构),只把词本身剥掉。standalone=true(saturation 信号)。
                return (line[tok.len()..].to_string(), StripHit { stripped: true, standalone: true });
            }
        }
        for &tok in Self::LEAKED_CONTROL_TOKENS {
            if let Some(rest) = line.strip_prefix(tok) {
                // 判定粘连：rest 的首字符必须是 **CJK / 全角** 粘连信号,才算泄漏(收严:
                // 排除 ASCII 冒号/数字/大写等——那些是正常英文,误删 count:42 / countDown())。
                match rest.chars().next() {
                    None => return (line.to_string(), StripHit::none()), // 行尾就是这个词→ 保守不剥。
                    Some(c) => {
                        if Self::is_leak_glue_char(c) {
                            return (rest.to_string(), StripHit { stripped: true, standalone: false }); // CJK/全角粘连 → 剥。
                        }
                        return (line.to_string(), StripHit::none()); // 其余→ 正常英文,不剥。
                    }
                }
            }
        }
        (line.to_string(), StripHit::none())
    }

    fn process_assistant_response(&mut self, content: &str) -> Vec<SseEvent> {
        // 【诊断探针·KIRO_INVOKE_TRACE】坐实「文本化工具调用」现象(#70544 变体):Claude 系模型
        // 偶发把工具调用语法当纯文本吐进 assistantResponseEvent(丢 antml: 前缀 + 夹 court 泄漏词),
        // 客户端拿到 <invoke.../> 文本解析不了直接断连。此探针在文本流里出现工具调用标记时如实记一条
        // (含现场文本片段),用于抓真实语料定性——上游到底走文本流还是 toolUseEvent。平时零开销。
        if contains_textified_tool_call(content) {
            // 无条件计数(取证:决定是否值得做 R4 文本化重组层)——不受 KIRO_INVOKE_TRACE 限。
            self.textified_invoke_hits += 1;
            crate::common::recovery_metrics::bump_textified_invoke();
            // 详细现场语料仅在探针开启时打(含文本片段,量大)。
            if invoke_trace_enabled() {
                let snippet: String = content.chars().take(200).collect();
                tracing::warn!(
                    target: "kiro::invoke_trace",
                    model = %self.model,
                    "[invoke_trace] assistantResponseEvent 文本流出现工具调用标记(疑似文本化 invoke 泄漏): {:?}",
                    snippet
                );
            }
        }
        // 【诊断探针·stray 泄漏形态观测】纯统计不改输出,零误删风险。目的:点亮"句中/独占 stray 词"黑洞——
        // 现有 leakedCleaned 只在**行首**清洗命中才计数,句中泄漏(如 `重读course了`)完全静默穿透、
        // 连计数都没进,导致线上全 0 无法区分"没泄漏"还是"泄漏了没检测到"。这里在**清洗前**扫原始
        // content,按形态分类计数(独占 stray 行 / 句中紧贴 CJK 的 stray 词),供运维页看真机泄漏形态,
        // 再据此决定要不要开保守清洗。开销:仅在 content 含已知 stray 词时才细扫(快路径先 contains)。
        observe_stray_leak_forms(content, &mut self.stray_standalone_seen, &mut self.stray_inline_seen);
        // 先剥离 DeepSeek DSML 工具协议标记(跨 chunk 安全),再走后续文本处理。
        let cleaned = self.strip_dsml_markers(content);
        // 泄漏控制 token 清洗（开关默认 true，见 handlers::tool_clean_leaked_tokens_enabled）：
        // 仅行首粘连特征命中才剥，误删风险极低。
        let cleaned = if super::handlers::tool_clean_leaked_tokens_enabled() {
            self.clean_leaked_tokens(&cleaned)
        } else {
            cleaned
        };
        // 更新行首标志：本段非空时，按是否以换行结尾决定下段起点是否在行首。
        if !cleaned.is_empty() {
            self.at_line_start = cleaned.ends_with('\n');
        }
        // stray 复读熔断:**所有路径的公共入口**(thinking / 无工具 / reclaim 都在此之后),
        // 治 Opus 退化刷屏(课/course/任意短词连写或独占行)。脱离 reclaim 路径独立生效——
        // 这修了审计发现的两个 HIGH 盲区:①thinking 提前 return 绕过 ②无工具请求绕过。
        // 熔断已 tripped → 返回空丢弃剩余;截断 → 只保留阈值前文本。
        let guarded = self.stray_guard_filter(&cleaned).into_owned();
        if guarded.is_empty() {
            return Vec::new();
        }
        let content = guarded.as_str();

        // 估算 tokens
        self.output_tokens += estimate_tokens(content);

        // 如果启用了thinking，需要处理thinking块
        if self.thinking_enabled {
            return self.process_content_with_thinking(content);
        }

        // 文本化 invoke 重组(开关开且本次请求带了工具):文本先进 sniff 缓冲,决策安全后才释放
        // (完整块过四道门重组 / 半块 hold 等闭合 / 非泄漏当文本)。开关关或无声明工具则走原路径。
        if self.reclaim_enabled && !self.known_tool_names.is_empty() {
            self.invoke_sniff_buffer.push_str(content);
            return self.drain_invoke_sniff_buffer(false);
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
                        } else if self.thinking_buffer.len() > MAX_THINKING_BUFFER_BYTES {
                            // review Finding 5 修复:上游若持续吐纯空白(无 <thinking>),纯空白分支
                            // 既不 emit 也不收缩 → thinking_buffer 无界增长 OOM(远程 DoS)。
                            // 超上限时把纯空白安全内容按普通文本吐出并收缩,只保留可能的半标签尾巴。
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

    // ===== 文本化 invoke 重组(R4,移植 ZyphrZero__kiro.rs v0.6.5)=====

    /// 把重组出的 (工具名, input_json) 合成为标准结构化 tool_use 的 6 步 SSE
    /// (content_block_start type:tool_use → input_json_delta → content_block_stop)。
    /// set_has_tool_use(true) → get_stop_reason 自然返回 tool_use(不用 borrow-retry,就地修复)。
    /// 工具名经 tool_name_map 还原(超长名缩短过的还原回客户端原名)。
    fn synthesize_tool_use(&mut self, parsed_name: String, input_json: String) -> Vec<SseEvent> {
        let mut events = Vec::new();
        self.state_manager.set_has_tool_use(true);
        self.reclaimed_invoke_count += 1;
        crate::common::recovery_metrics::bump_reclaimed_invoke();
        let block_index = self.state_manager.next_block_index();
        let tool_use_id = format!("toolu_{}", Uuid::new_v4().to_string().replace('-', ""));
        self.tool_block_indices.insert(tool_use_id.clone(), block_index);
        let name = self
            .tool_name_map
            .get(&parsed_name)
            .cloned()
            .unwrap_or(parsed_name);
        events.extend(self.state_manager.handle_content_block_start(
            block_index,
            "tool_use",
            json!({
                "type": "content_block_start",
                "index": block_index,
                "content_block": { "type": "tool_use", "id": tool_use_id, "name": name, "input": {} }
            }),
        ));
        if let Some(d) = self.state_manager.handle_content_block_delta(
            block_index,
            json!({
                "type": "content_block_delta",
                "index": block_index,
                "delta": { "type": "input_json_delta", "partial_json": input_json }
            }),
        ) {
            events.push(d);
        }
        if let Some(s) = self.state_manager.handle_content_block_stop(block_index) {
            events.push(s);
        }
        events
    }

    /// stray token 复读熔断:对即将作为文本吐出的内容,检测 call/count/card/court 连续独占行复读。
    /// 跨 chunk 维护 (stray_repeat_last, stray_repeat_run);超阈值后本请求剩余文本全丢(熔断已 tripped)。
    /// 返回截断后可安全吐出的文本(熔断已触发则返回空)。开关关或已 tripped 走各自快路径。
    fn stray_guard_filter<'a>(&mut self, text: &'a str) -> std::borrow::Cow<'a, str> {
        if self.stray_guard_tripped {
            return std::borrow::Cow::Borrowed("");
        }
        if !super::handlers::tool_stray_repeat_guard_enabled() {
            return std::borrow::Cow::Borrowed(text);
        }
        // 两条独立的复读检测,取先命中的截断点(取 min):
        // ① 逐行独占:同一 stray 行连续重复(跨 chunk 维护 run),覆盖 "课\n课\n课\n…" 形态。
        // ② 结构性签名:任意"短 token"(≤6 字符、纯字母或纯 CJK、无空格标点)连续重复 ≥阈值——
        //    **不依赖硬编码词表、不依赖换行**,覆盖 "课课课…"(单行连写)/"coursecourse…"/未来任何新退化词。
        //    这是治本:硬编码词表(course 都漏过)+ 独占行精确匹配(单行连写漏)双盲区的通用兜底。
        let line_cut = self.detect_stray_line_repeat(text);
        let sig_cut = detect_structural_flood(text);
        let cut_at = match (line_cut, sig_cut) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        match cut_at {
            Some(pos) => {
                self.stray_guard_tripped = true;
                crate::common::recovery_metrics::bump_stray_guard_tripped();
                tracing::warn!(target: "kiro::invoke_trace", model = %self.model,
                    "[invoke_reclaim] stray token 复读超阈值({}),熔断本轮剩余文本", REPEAT_GUARD_TRIP_THRESHOLD);
                std::borrow::Cow::Owned(text[..pos].to_string())
            }
            None => std::borrow::Cow::Borrowed(text),
        }
    }

    /// ① 逐行独占 stray 词复读检测(跨 chunk 维护 stray_repeat_last/run,保留已知词提前介入)。
    /// 返回截断字节偏移(命中阈值)或 None。
    fn detect_stray_line_repeat(&mut self, text: &str) -> Option<usize> {
        let mut offset = 0usize;
        for segment in text.split_inclusive('\n') {
            let line = segment.trim();
            if !line.is_empty() && (STRAY_INVOKE_TOKENS.contains(&line) || is_short_flood_token(line)) {
                if line == self.stray_repeat_last {
                    self.stray_repeat_run += 1;
                } else {
                    self.stray_repeat_last = line.to_string();
                    self.stray_repeat_run = 1;
                }
                if self.stray_repeat_run >= REPEAT_GUARD_TRIP_THRESHOLD {
                    return Some(offset);
                }
            } else if !line.is_empty() {
                self.stray_repeat_last = line.to_string();
                self.stray_repeat_run = 0;
            }
            offset += segment.len();
        }
        None
    }

    /// 重组路径里"当文本吐"的统一出口。stray 熔断已在 process_assistant_response 顶层对全部入站文本
    /// 统一执行过(进 sniff 缓冲的内容都已过滤),这里直接裸发,**不再重复跑有状态的 guard**
    /// (重复跑会二次累加 stray_repeat_run 导致误判/提前熔断)。
    fn emit_text_delta_guarded(&mut self, text: &str) -> Vec<SseEvent> {
        if text.is_empty() {
            return Vec::new();
        }
        self.create_text_delta_events(text)
    }

    /// invoke 嗅探缓冲驱动:文本进缓冲后,循环找完整/半 <invoke> 块,过四道门决定"重组捞回 vs 当文本"。
    /// flush=true 时流已结束,残留半块当普通文本吐(绝不静默吞)。移植 ZyphrZero drain_invoke_sniff_buffer。
    fn drain_invoke_sniff_buffer(&mut self, flush: bool) -> Vec<SseEvent> {
        let mut events = Vec::new();
        // 取出本地 buffer 一次性驱动(避免每轮 clone;退化大缓冲下省 O(n²))。
        let mut buf = std::mem::take(&mut self.invoke_sniff_buffer);
        loop {
            match find_invoke_start(&buf) {
                Some(start) => match find_invoke_block_end(&buf, start) {
                    Some(end) => {
                        // 完整块:过四道门。
                        let before = strip_trailing_stray_tokens(&buf[..start]).to_string();
                        let fence_after_before =
                            fence_open_after(self.code_fence_open, &self.fence_scan_partial, &before);
                        let parsed = parse_invoke_block(&buf[start..end]);
                        let name_known = parsed
                            .as_ref()
                            .map(|(n, _)| self.known_tool_names.contains(n))
                            .unwrap_or(false);
                        if invoke_looks_like_real_leak(&before) && !fence_after_before && name_known {
                            // 真泄漏:吐块前文本(剥掉尾部独立 stray 行)+ 合成 tool_use。
                            if !before.is_empty() {
                                // before 的围栏状态要并入(它会作为文本吐,推进围栏奇偶)。
                                advance_code_fence_state(&mut self.code_fence_open, &mut self.fence_scan_partial, &before);
                                events.extend(self.emit_text_delta_guarded(&before));
                            }
                            let (name, input_json) = parsed.expect("parsed is Some when name_known");
                            events.extend(self.synthesize_tool_use(name, input_json));
                        } else {
                            // 不捞回(句中/围栏内/工具名未知/解析失败)→ 整段当普通文本吐。
                            let seg = buf[..end].to_string();
                            advance_code_fence_state(&mut self.code_fence_open, &mut self.fence_scan_partial, &seg);
                            events.extend(self.emit_text_delta_guarded(&seg));
                        }
                        buf = buf[end..].to_string();
                        continue;
                    }
                    None => {
                        // 半块(未闭合)。行首判定:非行首/围栏内当文本直接吐,不 hold。
                        let before = strip_trailing_stray_tokens(&buf[..start]).to_string();
                        let fence_after_before =
                            fence_open_after(self.code_fence_open, &self.fence_scan_partial, &before);
                        if !invoke_looks_like_real_leak(&before) || fence_after_before {
                            if !buf.is_empty() {
                                let seg = buf.clone();
                                advance_code_fence_state(&mut self.code_fence_open, &mut self.fence_scan_partial, &seg);
                                events.extend(self.emit_text_delta_guarded(&seg));
                            }
                            break;
                        }
                        // 行首未闭合块:吐 start 前文本,保留 start.. 等闭合。
                        if start > 0 {
                            let seg = buf[..start].to_string();
                            advance_code_fence_state(&mut self.code_fence_open, &mut self.fence_scan_partial, &seg);
                            events.extend(self.emit_text_delta_guarded(&seg));
                        }
                        let remainder = buf[start..].to_string();
                        if flush {
                            if !remainder.is_empty() {
                                events.extend(self.emit_text_delta_guarded(&remainder));
                            }
                        } else if remainder.len() > Self::MAX_INVOKE_HOLD_BYTES {
                            // 纯字节上限兜底:永不闭合的 <invoke 不能无限 hold 卡死流。
                            events.extend(self.emit_text_delta_guarded(&remainder));
                        } else {
                            self.invoke_sniff_buffer = remainder;
                        }
                        break;
                    }
                },
                None => {
                    // 无 invoke 开标签。flush 全吐;否则保留可能是半个 <invoke 开标签的尾巴。
                    if flush {
                        if !buf.is_empty() {
                            let seg = buf.clone();
                            advance_code_fence_state(&mut self.code_fence_open, &mut self.fence_scan_partial, &seg);
                            events.extend(self.emit_text_delta_guarded(&seg));
                        }
                    } else {
                        let keep = partial_invoke_tag_suffix_len(&buf);
                        let emit_len = buf.len() - keep;
                        if emit_len > 0 {
                            let seg = buf[..emit_len].to_string();
                            advance_code_fence_state(&mut self.code_fence_open, &mut self.fence_scan_partial, &seg);
                            events.extend(self.emit_text_delta_guarded(&seg));
                        }
                        self.invoke_sniff_buffer = buf[emit_len..].to_string();
                    }
                    break;
                }
            }
        }
        events
    }

    /// 收尾 flush invoke 嗅探缓冲(流结束):残留半块当普通文本吐,绝不静默吞。
    fn flush_invoke_sniff_buffer(&mut self) -> Vec<SseEvent> {
        if self.invoke_sniff_buffer.is_empty() {
            return Vec::new();
        }
        self.drain_invoke_sniff_buffer(true)
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
            let model = self.model.clone();
            let buf = self.tool_input_sent.entry(tool_use.tool_use_id.clone()).or_default();
            // 帧探针（KIRO_TOOL_TRACE）：抓上游逐帧原文 + 合并轨迹，定性 Invalid tool parameters 真因。
            let buf_before = buf.clone();
            *buf = merge_tool_input(buf, &tool_use.input);
            trace_tool_frame(
                &model,
                &tool_use.tool_use_id,
                &tool_use.name,
                tool_use.stop,
                &tool_use.input,
                &buf_before,
                buf,
            );
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
    fn flush_tool_input(&mut self, block_index: i32, mut assembled: String) -> Vec<SseEvent> {
        if assembled.is_empty() {
            return Vec::new();
        }
        self.output_tokens += (assembled.len() as i32 + 3) / 4;
        if serde_json::from_str::<serde_json::Value>(&assembled).is_err() {
            // 拼装后仍非法：多因上游发了非法 JSON（如 JSON 不支持的 \x 转义 / 截断 \uXXXX / 裸控制符）
            // 或中间帧丢失截断。客户端拿坏 JSON 直接 parse 失败 → Invalid tool parameters。
            // 归因标签（纯可观测，绝不进控制流）：单遍 string-aware 扫描把非法串按责任方分流——
            // truncated=帧丢失/上游截断、illegal_chars=模型侧非法转义/裸控制符、两者兼有、其它畸形。
            // 判据与 repair 层同源，只用于日志定位真因（"修不好的残留到底是谁的责任"）。
            let defect = classify_tool_json_defect(&assembled);
            tracing::warn!(
                block_index,
                defect = defect.as_str(),
                "tool_use 拼装后 input 非合法 JSON（长度 {}），归因={}",
                assembled.len(),
                defect.as_str()
            );
            // 帧探针（KIRO_TOOL_TRACE）：非法时额外打印**完整拼装串**全文（含 model + 归因标签），
            // 用于坐实是类型 A（上游模型帧本身含非法转义/乱码 token）还是类型 C（合并逻辑洞，已修）。
            if tool_trace_enabled() {
                tracing::warn!(
                    target: "kiro::tool_trace",
                    model = %self.model,
                    block_index,
                    defect = defect.as_str(),
                    assembled_len = assembled.len(),
                    assembled = %assembled,
                    "[tool_trace] 拼装后非法 JSON 全文（定性 Invalid tool parameters）"
                );
            }
            // 缓解④（根治向，默认开）：先尝试把坏 JSON 修成合法（转义非法反斜杠/裸控制符、补全截断），
            // 修复后强制复验通过才用。成功则 assembled 已是合法 JSON、直接落到下方正常发送路径，
            // 完全跳过失败态对齐/暴露错误逻辑（客户端能正常 parse，无需退避重试）。
            if super::handlers::tool_repair_json_enabled() {
                if let Some(repaired) = repair_tool_json(&assembled) {
                    tracing::info!(
                        block_index,
                        orig_len = assembled.len(),
                        repaired_len = repaired.len(),
                        "tool_use 非法 JSON 已修复为合法 JSON（Invalid tool parameters 根治）"
                    );
                    // 修复成功:assembled 已合法。**不 early-return**——fall through 到函数尾的统一
                    // 出口(unwrap_double_encoded + 发送),否则 repair 结果恰好是双重编码串时会跳过
                    // 洞1 解包(review confirmed:如 \U 被修成字面后整体成 Value::String)。两条路径
                    // (原本合法 / 修复后)从此经同一 unwrap + 发送出口,消除路径不一致。
                    assembled = repaired;
                    // 修复成功 → 跳过下方缓解②/③/⑤(那些是给"修不好"的),直接走统一出口。
                    // 用标签跳出 is_err 块:此处 break 掉外层 if is_err 的剩余分支。
                }
                // 修不好 → 落入下方缓解②/③/⑤（与不开修复层等价，最坏情况不劣化）。
            }
            // 若 repair 已把 assembled 修成合法,则跳过缓解②/③/⑤(它们只服务"仍非法"的残留)。
            let repaired_ok =
                serde_json::from_str::<serde_json::Value>(&assembled).is_ok();
            if !repaired_ok {
            // 缓解⑤：截断跨轮恢复（开关默认关）。只在**修复层已启用且也补不回**（修复层开时走到这里
            // = 上面 repair 已返回 None）且归因为真截断（Truncated / TruncatedAndIllegal）时触发：
            // 不发不完整的 partial_json
            // （半截参数会被客户端当完整调用执行，比整轮失败更危险），改置失败态 + 收尾补发 SSE error，
            // 让客户端退避后重试整个请求。绝不 report_failure 连坐号（工具截断≠号坏，隔离铁律）。
            // 非截断的畸形（IllegalChars/Malformed）不归本开关管，仍走②/③按原语义处理。
            if should_recover_truncation(
                defect,
                super::handlers::tool_truncation_recovery_enabled(),
                super::handlers::tool_repair_json_enabled(),
            ) {
                tracing::warn!(
                    block_index,
                    defect = defect.as_str(),
                    "tool_use 参数真截断且修复层补不回：置失败态让客户端重试整轮（截断跨轮恢复）"
                );
                if self.completion.is_ok() {
                    self.completion = CompletionStatus::UpstreamError {
                        code: "INVALID_TOOL_INPUT".to_string(),
                        message: "工具调用参数被上游截断（缺整段值），请重试；如反复触发可拆小该调用。"
                            .to_string(),
                    };
                }
                // 不发这条截断的坏 partial_json（收尾兜底据失败态补发 SSE error）。
                return Vec::new();
            }
            // 缓解②：流式失败态对齐（开关默认关）。开启时把流式也置 UpstreamError{INVALID_TOOL_INPUT}
            // 失败态，与非流式对齐（收尾记 ServerError、不污染成功率、收尾兜底会补发 SSE error）。
            // 幂等：只在首个失败落定。绝不 report_failure 连坐号（工具非法≠号坏，隔离铁律）。
            if super::handlers::tool_stream_align_failure_enabled() && self.completion.is_ok() {
                self.completion = CompletionStatus::UpstreamError {
                    code: "INVALID_TOOL_INPUT".to_string(),
                    message: "工具调用参数非合法 JSON（模型侧生成异常）".to_string(),
                };
            }
            // 缓解③：如实暴露错误。**不发坏 JSON 的判据绑定"失败态已置"（completion.is_err），
            // 而非③开关本身**——消除②③拆开的两个矛盾组合（验证报告缺陷2/3）：
            //   ·②开③关(旧):置了失败态却 fall-through 发坏 JSON → 记账失败却发坏 JSON,自相矛盾;
            //   ·②关③开(旧):completion 仍 Ok 却 return 吞掉 → 记成功但客户端拿 input:{} 当成功执行(更危险)。
            // 新语义:只要②或⑤已置失败态(completion.is_err)→ 一律不发坏 partial_json(收尾据失败态补 SSE
            // error);completion 仍 Ok(②③都关)→ 保持现状原样发出交客户端(绝不静默吞成空参)。
            // ③开关现语义 = 是否额外"主动置失败态并暴露"(见下),与"不发坏 JSON"由失败态统一裁决解耦。
            if super::handlers::tool_expose_error_to_client_enabled() && self.completion.is_ok() {
                // ③开但②没置态(如②关):③自己置失败态,保证"暴露错误"语义成立(不发坏 JSON + 收尾补 error)。
                self.completion = CompletionStatus::UpstreamError {
                    code: "INVALID_TOOL_INPUT".to_string(),
                    message: "工具调用参数非合法 JSON（模型侧生成异常）".to_string(),
                };
            }
            // 统一出口:失败态已置(②/③/⑤任一)→ 不发坏 JSON。这一条兜住所有开关组合的自洽。
            if !self.completion.is_ok() {
                return Vec::new();
            }
            } // end if !repaired_ok(修复成功则跳过②/③/⑤)
        }
        // 洞1:整包双重编码解包。走到这里 assembled 已是合法 JSON(原本合法 / 修复后)。若它其实是
        // 「被再套一层字符串编码的 object/array」,解一层还原成客户端能按 object 消费的形态,消灭
        // 一类漏过 repair 层的 InputValidationError。
        // 【P2-1 解耦】此前裹在 tool_repair_json 开关下,导致用户为排查关掉 repair 时连带关掉解包——
        // 而解包不改语义(只剥误加的一层字符串编码)、对合法 object/array 是安全 no-op(as_str 返回
        // None 即不动),与"修坏 JSON"是正交能力,应独立恒开。故移出开关无条件跑。
        if let Some(unwrapped) = unwrap_double_encoded(&assembled) {
            tracing::info!(block_index, "tool_use 参数为双重编码,已解一层还原为 object/array");
            assembled = unwrapped;
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
        // 收尾 flush invoke 嗅探缓冲:流结束时残留的半块(未等到 </invoke>)当普通文本吐,绝不静默吞。
        events.extend(self.flush_invoke_sniff_buffer());

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

        // 泄漏 token 诊断收尾（可观测，不改任何已发内容）：本请求若清洗过泄漏 token / 命中 saturation,
        // 如实记一条——绝不黑箱。saturation（整段纯泄漏词行）= #70544 模型侧整段退化的信号,网关只能
        // 清洗单个粘连、救不了整段（Bug B），此处标注归因便于 dwgx 判"是模型抽风非网关问题"。
        if self.leaked_stripped > 0 || self.leaked_saturation_lines > 0 {
            // 可观测:本请求发生过泄漏清洗 / 命中 saturation 退化(各计一次请求级)。
            crate::common::recovery_metrics::bump_leaked_cleaned_request();
            if self.leaked_saturation_lines > 0 {
                crate::common::recovery_metrics::bump_leaked_saturation_request();
            }
            tracing::warn!(
                model = %self.model,
                leaked_stripped = self.leaked_stripped,
                saturation_lines = self.leaked_saturation_lines,
                "检测到 #70544 泄漏 token：已清洗 {} 个（其中 {} 行为整段纯泄漏词=模型侧整段退化，网关仅能清洗不能根治，建议该模型高多字节上下文场景 /clear 或换 sonnet）",
                self.leaked_stripped,
                self.leaked_saturation_lines,
            );
            if leak_trace_enabled() {
                tracing::warn!(
                    target: "kiro::leak_trace",
                    model = %self.model,
                    leaked_stripped = self.leaked_stripped,
                    saturation_lines = self.leaked_saturation_lines,
                    "[leak_trace] 本请求泄漏 token 清洗全貌"
                );
            }
        }
        // stray 泄漏形态观测收尾(请求级各计一次):点亮 clean 层够不到的句中/独占黑洞。
        // 这与 leaked_stripped 互补——leaked_stripped 只记行首真剥掉的,这里记"见到但可能没处理"的形态。
        if self.stray_standalone_seen > 0 {
            crate::common::recovery_metrics::bump_stray_standalone_seen();
        }
        if self.stray_inline_seen > 0 {
            crate::common::recovery_metrics::bump_stray_inline_seen();
            tracing::warn!(
                model = %self.model,
                inline_seen = self.stray_inline_seen,
                standalone_seen = self.stray_standalone_seen,
                "检测到句中/独占 stray 泄漏词(clean 层只清行首、句中未处理,此为观测取证:确认真机泄漏形态)"
            );
        }

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
        known_tool_names: std::collections::HashSet<String>,
    ) -> Self {
        let inner =
            StreamContext::new_full(model, estimated_input_tokens, thinking_enabled, tool_name_map, known_tool_names);
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

/// 非法工具参数 JSON 的缺陷归因（**纯可观测**，只写日志，绝不进控制流）。
///
/// 服务于「修不好的残留按责任方分流」定位真因：
/// - `Truncated`：结构未闭合（缺 `}`/`]` 或字符串未收尾）→ 指向上游 Kiro 截断/超时/网络侧，可查。
/// - `IllegalChars`：含非法转义（`\x`/`\U` 等）或裸控制符 → 指向模型侧生成异常，网关只能缓解。
/// - `TruncatedAndIllegal`：两者兼有。
/// - `Malformed`：结构闭合但仍非法（如 `}{` 粘连、键后无值）→ 归为其它畸形。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolJsonDefect {
    Truncated,
    IllegalChars,
    TruncatedAndIllegal,
    Malformed,
}

impl ToolJsonDefect {
    /// 供日志字段用的稳定短标签。
    fn as_str(&self) -> &'static str {
        match self {
            ToolJsonDefect::Truncated => "truncated",
            ToolJsonDefect::IllegalChars => "illegal_chars",
            ToolJsonDefect::TruncatedAndIllegal => "truncated_and_illegal",
            ToolJsonDefect::Malformed => "malformed",
        }
    }
}

/// 单遍 string-aware 扫描的诊断计数（判据与两层 repair 一一对应，不新造语义）。
struct ToolJsonScan {
    /// 结构未闭合：括号栈非空，或扫描结束仍在字符串内。
    truncated: bool,
    /// 含 JSON 非法转义（非九种合法转义）或裸控制符。
    illegal_chars: bool,
    /// 出现 `}{` / `} {` 粘连（非前缀双对象特征）。
    glued: bool,
}

/// 截断跨轮恢复的**纯决策**。三条件同时满足才触发：
/// 1. `recovery_on`：截断恢复开关开（默认关）。
/// 2. `repair_on`：JSON 修复层**已启用**——恢复的语义前提是「修复层也补不回」。修复层关时无法断言
///    此截断不可修（很多截断如未闭合字符串其实能被结构层补全），故修复层关则不触发，退回②/③原语义。
/// 3. 归因为真截断（Truncated / TruncatedAndIllegal）——非截断畸形（IllegalChars/Malformed）不归本开关管。
///
/// 抽成纯函数以便离线测试判据，避免在并行测试里 set/get 进程级开关造成互相污染。
fn should_recover_truncation(defect: ToolJsonDefect, recovery_on: bool, repair_on: bool) -> bool {
    recovery_on
        && repair_on
        && matches!(
            defect,
            ToolJsonDefect::Truncated | ToolJsonDefect::TruncatedAndIllegal
        )
}

/// 对已知非法的工具参数串做单遍扫描并归因。只在 `flush_tool_input` 的 `from_str` 已失败分支调用。
fn classify_tool_json_defect(s: &str) -> ToolJsonDefect {
    let scan = scan_tool_json(s);
    match (scan.truncated, scan.illegal_chars) {
        (true, true) => ToolJsonDefect::TruncatedAndIllegal,
        (true, false) => ToolJsonDefect::Truncated,
        (false, true) => ToolJsonDefect::IllegalChars,
        (false, false) => ToolJsonDefect::Malformed,
    }
}

/// 单遍 string-aware 扫描，判据与 repair 层同源（合法转义集 `" \ / b f n r t u`，其余非法；
/// 裸控制符 <0x20 非法；串未闭合 / 括号栈非空 / 末尾悬空转义 = 截断）。只归因，不改内容。
fn scan_tool_json(s: &str) -> ToolJsonScan {
    let mut in_string = false;
    let mut escaped = false;
    let mut depth: i32 = 0;
    let mut illegal_chars = false;
    let mut glued = false;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if in_string {
            if escaped {
                if !matches!(c, '"' | '\\' | '/' | 'b' | 'f' | 'n' | 'r' | 't' | 'u') {
                    illegal_chars = true;
                }
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            } else if (c as u32) < 0x20 {
                illegal_chars = true;
            }
            continue;
        }
        match c {
            '"' => in_string = true,
            '{' | '[' => depth += 1,
            '}' | ']' => {
                depth -= 1;
                if c == '}' {
                    let mut look = chars.clone();
                    while let Some(&n) = look.peek() {
                        if n.is_whitespace() {
                            look.next();
                        } else {
                            if n == '{' {
                                glued = true;
                            }
                            break;
                        }
                    }
                }
            }
            _ => {}
        }
    }
    let truncated = in_string || depth > 0 || escaped;
    ToolJsonScan {
        truncated,
        illegal_chars,
        glued,
    }
}

/// 尝试把模型吐出的**非法 JSON 工具参数**修成合法 JSON（根治 `Invalid tool parameters`）。
///
/// # 为什么做这个（对着 Claude Code 客户端源码 + 官方 issue 的确凿依据）
/// 客户端（2.1.207）拿到累积的 `partial_json` 后直接 `JSON.parse`（`Rq`+`JSON.parse`，仅剥 BOM、
/// **不做任何修复**），parse 失败即包成 `{__unparsedToolInput:{raw,len}}` → 渲染成 "Invalid tool
/// parameters"。官方源码 `HLy` 明列三类成因：**未转义反斜杠 / 未转义控制符 / 截断输出**，且
/// 对应 issue（#69522 长 unicode 转义、#20015 Windows 路径反斜杠、#29715 smart quote/控制符）
/// 全部 Open/not-planned——**官方不修**。这些请求经本网关时，我们在发给客户端前把坏 JSON 修好，
/// 客户端就能 parse 成功，"Invalid tool parameters" 从本侧消失。
///
/// # 安全契约（调用方 `flush_tool_input` 已保证 + 本函数复验）
/// - **只在 `from_str` 已失败时调用**：合法 JSON 永不进入本函数（对正常流零影响）。
/// - **修复后必须复验**：返回 `Some` 当且仅当修复结果 `from_str` 通过；修不好返回 `None`，
///   调用方退回现状（原样透传），**最坏情况 == 修复前行为**，不会更糟。
/// - **只修字符级噪声，绝不臆测语义**：仅转义字符串内的非法转义/裸控制符、补全结构截断，
///   不新增/删除/改写任何键值语义。
///
/// 整包双重编码解包(洞1):工具 input 契约上顶层必是 object。模型偶发把整个参数对象**再套一层
/// JSON 字符串编码**(double-encoded),如发出 `"{\"path\":\"a\"}"` 而非 `{"path":"a"}`——此时
/// `from_str` 会**成功**得到 `Value::String`,但客户端按 object 消费该工具参数就报
/// InputValidationError(参数类型不符)。这类**漏过 repair 层**(它 from_str 成功、不进修复)。
///
/// 本函数在 from_str **成功**后调用:若解析结果是 `Value::String(inner)` 且 `inner` 本身能再
/// parse 成 object/array,返回解一层后的合法 JSON 串;否则 `None`(不动)。
///
/// # 铁律
/// - **只解一层**:深层嵌套(`"\"nested\""`)保守不碰,避免过度解包改语义。
/// - **复验必 object/array 才用**:顶层数字/布尔/纯字符串 → None(工具 input 顶层不该是标量,
///   但也不臆测,原样交上层)。零语义损失(只是剥掉误加的一层字符串编码)。
pub(crate) fn unwrap_double_encoded(s: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(s).ok()?;
    let inner = v.as_str()?; // 顶层必须是 JSON 字符串才可能是双重编码
    let reparsed: serde_json::Value = serde_json::from_str(inner).ok()?;
    // 只有解出 object/array 才认定是"误套一层"的双重编码;标量不动。
    if reparsed.is_object() || reparsed.is_array() {
        serde_json::to_string(&reparsed).ok()
    } else {
        None
    }
}

/// 返回修复后的合法 JSON 串；无法修成合法则 `None`。
pub(crate) fn repair_tool_json(s: &str) -> Option<String> {
    // 空串不在本函数职责内（flush_tool_input 上游已处理空串），保守拒绝。
    if s.trim().is_empty() {
        return None;
    }
    // 第一层：字符级修复（转义字符串内非法转义 + 裸控制符）。
    let char_fixed = repair_json_char_level(s);
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&char_fixed) {
        return serde_json::to_string(&v).ok();
    }
    // 第二层：在字符级修复基础上再补全结构截断（缺 `}` / `]` / 收尾 `"`）。
    let struct_fixed = repair_json_structure(&char_fixed);
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&struct_fixed) {
        return serde_json::to_string(&v).ok();
    }
    None
}

/// JSON 字符级修复：状态机扫描，**只修字符串字面量内部**的非法字符，结构字符（`{}[]:,` 等）原样保留。
///
/// 修两类（对应客户端 `HLy` 列的成因）：
/// 1. **裸控制符**（U+0000..=U+001F 未转义，如真实换行/制表符混进字符串值）→ 转义成 `\n`/`\t`/`\uXXXX`。
/// 2. **非法反斜杠转义**：JSON 只认 `\" \\ \/ \b \f \n \r \t \uXXXX` 九种。其它 `\x` 一律非法——
///    - `\U`（Windows 路径 `C:\Users` 的典型泄漏）、`\x41`、`\.`、行尾孤立 `\` 等 → 把该反斜杠**再转义**
///      成 `\\`（还原成"字面反斜杠 + 原字符"，这是模型本想表达路径/字面量时的正确 JSON）。
///    - `\uXXXX` 若后随不足 4 位 hex（截断）→ 同样降级成字面 `\\u...`，交由结构层或复验兜底。
///
/// 字符串外的字符原样透传（不碰任何结构）。非字符串区的裸控制符（缩进空白等）JSON 本就允许，不动。
fn repair_json_char_level(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 16);
    let mut chars = s.chars().peekable();
    let mut in_string = false;
    while let Some(c) = chars.next() {
        if !in_string {
            // 结构区：只关心进入字符串的 `"`，其余原样。
            out.push(c);
            if c == '"' {
                in_string = true;
            }
            continue;
        }
        // 字符串内：
        match c {
            '"' => {
                // 字符串结束（未转义的引号）。
                out.push(c);
                in_string = false;
            }
            '\\' => {
                // 转义序列：看下一个字符决定合法性。
                match chars.next() {
                    None => {
                        // 行尾孤立反斜杠 → 转义成字面反斜杠。
                        out.push_str("\\\\");
                    }
                    Some(esc) => match esc {
                        // 九种合法转义原样保留。
                        '"' | '\\' | '/' | 'b' | 'f' | 'n' | 'r' | 't' => {
                            out.push('\\');
                            out.push(esc);
                        }
                        'u' => {
                            // 必须后随 4 位 hex，否则截断 → 降级字面。
                            let mut hex = String::new();
                            for _ in 0..4 {
                                match chars.peek() {
                                    Some(h) if h.is_ascii_hexdigit() => {
                                        hex.push(*h);
                                        chars.next();
                                    }
                                    _ => break,
                                }
                            }
                            if hex.len() == 4 {
                                // 洞4:UTF-16 代理对完整性(对应 #69522 长 unicode 转义 parse 失败)。
                                // serde_json 只接受**成对**的代理(高 D800-DBFF 紧跟低 DC00-DFFF);
                                // 孤立高代理 / 孤立低代理会被判非法 JSON。这里:
                                //   - 高代理 + 后随合法低代理 → 原样保留(合法 emoji 如 😀 不碰);
                                //   - 高代理但后面不是合法低代理 → 孤立,降级字面 \\uXXXX;
                                //   - 直接遇到低代理(没被前面的高代理配对消费) → 孤立,降级字面。
                                let cp = u32::from_str_radix(&hex, 16).unwrap_or(0);
                                if (0xD800..=0xDBFF).contains(&cp) {
                                    // 高代理:向前看是否紧跟 \uYYYY 且 YYYY 是合法低代理。
                                    if let Some(low_hex) = peek_low_surrogate(&mut chars) {
                                        out.push_str("\\u");
                                        out.push_str(&hex);
                                        out.push_str("\\u");
                                        out.push_str(&low_hex);
                                    } else {
                                        // 孤立高代理 → 降级字面。
                                        out.push_str("\\\\u");
                                        out.push_str(&hex);
                                    }
                                } else if (0xDC00..=0xDFFF).contains(&cp) {
                                    // 孤立低代理(合法对已在高代理分支被整体消费,能到这里必是孤立)→ 降级字面。
                                    out.push_str("\\\\u");
                                    out.push_str(&hex);
                                } else {
                                    // BMP 普通码位 → 原样保留。
                                    out.push_str("\\u");
                                    out.push_str(&hex);
                                }
                            } else {
                                // 截断的 \uXX → 字面反斜杠 + u + 已收集 hex。
                                out.push_str("\\\\u");
                                out.push_str(&hex);
                            }
                        }
                        // 非法转义（\U \x \. 等）→ 反斜杠降级字面，原字符正常写入
                        // （若原字符本身是控制符，落入下方 push_escaped_char 再处理）。
                        other => {
                            out.push_str("\\\\");
                            push_escaped_char(&mut out, other);
                        }
                    },
                }
            }
            // 裸控制符 → 转义。
            c if (c as u32) < 0x20 => {
                push_escaped_char(&mut out, c);
            }
            // 普通字符原样。
            _ => out.push(c),
        }
    }
    out
}

/// 向前看：紧接的是否为 `\uYYYY` 且 YYYY 是合法低代理(DC00-DFFF)。是则**消费**这 6 个字符
/// (`\` `u` + 4 hex)并返回 `Some("YYYY")`;否则不消费任何字符、返回 `None`。
/// 用于 [`repair_json_char_level`] 判定高代理后是否紧跟合法低代理(合法代理对整体保留)。
fn peek_low_surrogate(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> Option<String> {
    // clone 迭代器做无损前瞻:先在 clone 上验证,确认合法才在真迭代器上消费。
    let mut look = chars.clone();
    if look.next() != Some('\\') {
        return None;
    }
    if look.next() != Some('u') {
        return None;
    }
    let mut hex = String::new();
    for _ in 0..4 {
        match look.next() {
            Some(h) if h.is_ascii_hexdigit() => hex.push(h),
            _ => return None,
        }
    }
    let cp = u32::from_str_radix(&hex, 16).ok()?;
    if (0xDC00..=0xDFFF).contains(&cp) {
        // 合法低代理 → 在真迭代器上消费掉这 6 个字符(\ u + 4 hex)。
        for _ in 0..6 {
            chars.next();
        }
        Some(hex)
    } else {
        None
    }
}

/// 把一个字符写进输出：控制符转义成 JSON 合法形式（`\n`/`\t`/`\uXXXX`），其余原样。
fn push_escaped_char(out: &mut String, c: char) {
    match c {
        '\n' => out.push_str("\\n"),
        '\r' => out.push_str("\\r"),
        '\t' => out.push_str("\\t"),
        '\u{08}' => out.push_str("\\b"),
        '\u{0C}' => out.push_str("\\f"),
        c if (c as u32) < 0x20 => {
            out.push_str(&format!("\\u{:04x}", c as u32));
        }
        _ => out.push(c),
    }
}

/// JSON 结构补全：针对**截断**（流被上游/网络在中途切断，缺尾部 `"`/`}`/`]`）。
///
/// 单遍扫描跟踪：是否在字符串内、括号栈（`{`/`[`）。扫完若仍在字符串内先补收尾 `"`，
/// 再按栈逆序补 `}`/`]`。假设**输入已过字符级修复**（转义已合法），故这里只需按结构闭合。
/// 保守边界：若结尾停在"键后无值"或"逗号后无元素"这类语义残缺处，闭合后仍非法 → 交由
/// 调用方复验 `from_str` 拒绝（返回 None 退回透传）。不猜测缺失的值，只补闭合符号。
fn repair_json_structure(s: &str) -> String {
    let mut in_string = false;
    let mut escaped = false;
    let mut stack: Vec<char> = Vec::new();
    for c in s.chars() {
        if in_string {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' => in_string = true,
            '{' => stack.push('}'),
            '[' => stack.push(']'),
            '}' | ']' => {
                stack.pop();
            }
            _ => {}
        }
    }
    let mut out = s.to_string();
    // 末尾悬空转义符（单个 `\`）→ 补成字面反斜杠，避免收尾 `"` 被它吃掉。
    if escaped {
        out.push('\\');
    }
    // 仍在字符串内 → 先闭合字符串。
    if in_string {
        out.push('"');
    }
    // 逆序补齐未闭合的括号。
    while let Some(closer) = stack.pop() {
        out.push(closer);
    }
    out
}

/// 工具帧探针总开关（环境变量 `KIRO_TOOL_TRACE` 非空即开）。用 `OnceLock` 缓存，避免每帧读环境变量。
///
/// 这是**常驻代码**的诊断探针（非临时旁挂），用于坐实 `Invalid tool parameters` 真因：
/// dwgx 现场复现时设 `KIRO_TOOL_TRACE=1` 重启网关，即可抓到上游 `toolUseEvent.input` 的**逐帧原文**
/// 与 `merge_tool_input` 的合并轨迹，据此定性：
///   - **类型 C（网关侧，已修）**：原始帧序列里出现「非前缀双完整对象」等，合并后仍为合法 JSON；
///   - **类型 A（模型抽风，网关修不了）**：某原始帧本身就含非法转义 / 乱码控制 token，
///     拼装后 `flush_tool_input` 报「非合法 JSON」——此时网关只能如实透传，责任在上游模型。
/// 平时零开销（未设环境变量时 `tool_trace_enabled()` 恒 false，探针整体短路）。
fn tool_trace_enabled() -> bool {
    use std::sync::OnceLock;
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var("KIRO_TOOL_TRACE")
            .map(|v| !v.trim().is_empty() && v != "0")
            .unwrap_or(false)
    })
}

/// 文本化工具调用诊断探针总开关(环境变量 `KIRO_INVOKE_TRACE` 非空即开)。平时零开销。
/// 开启时,assistantResponseEvent 文本流里出现工具调用标记(文本化 invoke)即记一条现场语料,
/// 用于坐实「模型把工具调用当纯文本吐出」现象(#70544 变体,致客户端断连)。
fn invoke_trace_enabled() -> bool {
    use std::sync::OnceLock;
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var("KIRO_INVOKE_TRACE")
            .map(|v| !v.trim().is_empty() && v != "0")
            .unwrap_or(false)
    })
}

// ============================================================================
// 文本化 invoke 解析纯函数集（从 ZyphrZero/kiro.rs 移植，逐字保真逻辑）
//
// 这批函数全部是纯函数：不触碰 StreamContext / 任何可变状态，只对入参字符串做
// 结构解析。用于从「模型把工具调用当纯文本吐出」的退化输出（#70544 变体）里把
// `<invoke name="...">...<parameter ...>...</parameter>...</invoke>` 结构捞回。
// 复用本文件既有的 `QUOTE_CHARS` / `is_quote_char`（与 kiro.rs 完全一致）。
//
// 本阶段只落地函数 + 单测（隔离验证），暂不接入任何状态机。
// ============================================================================

/// 检查 `name_pos`（指向标签名首字母）的前面是否构成合法的开标签起始，
/// 兼容裸写法 `<tag` 和带命名空间前缀的写法 `<prefix:tag`。
///
/// 返回 `Some(lt_pos)`（指向 `<` 的字节位置）表示合法；`None` 表示不是标签。
///
/// 注：本阶段这批 invoke 解析纯函数仅落地 + 单测隔离验证，尚未接入状态机，
/// 故统一 `#[allow(dead_code)]`；后续接线阶段移除。
#[allow(dead_code)]
fn open_tag_lt_pos(buffer: &str, name_pos: usize) -> Option<usize> {
    let bytes = buffer.as_bytes();
    if name_pos == 0 {
        return None;
    }
    let prev = bytes[name_pos - 1];
    if prev == b'<' {
        return Some(name_pos - 1);
    }
    // 形如 `<prefix:tag`：name 前面是 ':'，再往前是一段标识符，再往前是 '<'
    if prev == b':' {
        let i = name_pos - 1; // 指向 ':'
        let mut j = i; // 标识符左边界扫描
        while j > 0 && {
            let c = bytes[j - 1];
            c.is_ascii_alphanumeric() || c == b'_'
        } {
            j -= 1;
        }
        // 标识符非空，且其左边是 '<'
        if j < i && j > 0 && bytes[j - 1] == b'<' {
            return Some(j - 1);
        }
    }
    None
}

/// 查找未被引用字符包裹的 invoke 开标签，返回指向 `<` 的字节位置
///
/// 兼容裸 `<invoke ...>` 与带命名空间前缀 `<prefix:invoke ...>` 两种写法。
/// 复用 `is_quote_char`：若 `<` 前紧贴反引号/引号等包裹字符，视为引用，跳过。
#[allow(dead_code)]
fn find_invoke_start(buffer: &str) -> Option<usize> {
    let mut search = 0;
    while let Some(rel) = buffer[search..].find("invoke") {
        let name_pos = search + rel;
        if let Some(lt) = open_tag_lt_pos(buffer, name_pos) {
            // 标签名后必须是边界字符（空白或 '>'），避免误匹配 invoked 之类
            let after = name_pos + "invoke".len();
            let next_ok = buffer.as_bytes().get(after).map_or(true, |c| {
                c.is_ascii_whitespace() || *c == b'>' || *c == b'/'
            });
            let has_quote_before = lt > 0 && is_quote_char(buffer, lt - 1);
            if next_ok && !has_quote_before {
                return Some(lt);
            }
        }
        search = name_pos + "invoke".len();
    }
    None
}

/// 从 `start` 之后查找第一个 invoke 闭标签，返回结束位置（exclusive，含闭标签）
///
/// 兼容裸 `</invoke>` 与带前缀 `</prefix:invoke>`。找不到返回 `None`（块还没到齐）。
#[allow(dead_code)]
fn find_invoke_block_end(buffer: &str, start: usize) -> Option<usize> {
    // 块 A 的边界 = 下一个 `<invoke` 开标签（即下一个块 B 的起点），没有则到 buffer 结尾。
    // 这样连发 burst（A 紧跟 B）时，A 的搜索区间被 B 的开标签卡住，绝不会吃进 B。
    let boundary = match find_next_invoke_open(buffer, start) {
        Some(p) => p,
        None => buffer.len(),
    };
    // 在 [start, boundary) 区间里取【最后一个】 `</invoke>` 作为真闭合。
    // 贪婪取最后一个 → patch 正文里出现的字面 `</invoke>` 不会导致提前截断；
    // 区间被下一个块开标签卡住 → 不会跨块误合并。
    find_last_invoke_close(buffer, start, boundary)
}

/// 从 `start` 之后查找下一个真正的 `<invoke`（或 `<prefix:invoke`）开标签的字节位置。
/// 跳过 `start` 处当前块自身的开标签。
#[allow(dead_code)]
fn find_next_invoke_open(buffer: &str, start: usize) -> Option<usize> {
    // 先跳过当前块的开标签：从 start 之后第一个 '>' 之后开始找。
    let after_open = match buffer[start..].find('>') {
        Some(rel) => start + rel + 1,
        None => return None,
    };
    // 注意：不能复用 find_invoke_start——它对 `<` 前是 `>`（引用字符）的情况会拒绝，
    // 而连发 burst 里 B 的 `<invoke` 恰好紧跟在 A 的 `</invoke>` 的 `>` 后面。
    // 这里只认结构：`<invoke` 或 `<prefix:invoke`，开标签名后须是空白/`>`/`/` 边界。
    let region = &buffer[after_open..];
    let mut search = 0usize;
    while let Some(rel) = region[search..].find("invoke") {
        let name_pos = search + rel;
        if let Some(lt) = open_tag_lt_pos(region, name_pos) {
            let after = name_pos + "invoke".len();
            let next_ok = region.as_bytes().get(after).map_or(true, |c| {
                c.is_ascii_whitespace() || *c == b'>' || *c == b'/'
            });
            if next_ok {
                return Some(after_open + lt);
            }
        }
        search = name_pos + "invoke".len();
    }
    None
}

/// 在 `[from, boundary)` 区间内查找最后一个 `</invoke>` / `</prefix:invoke>` 的结束位置
/// （exclusive，含闭标签）。找不到返回 `None`（块还没到齐）。
#[allow(dead_code)]
fn find_last_invoke_close(buffer: &str, from: usize, boundary: usize) -> Option<usize> {
    let region_end = boundary.min(buffer.len());
    if from >= region_end {
        return None;
    }
    let region = &buffer[from..region_end];
    let bytes = region.as_bytes();
    let mut search = 0usize;
    let mut last: Option<usize> = None;
    while let Some(rel) = region[search..].find("invoke>") {
        let name_pos = search + rel;
        // '</invoke>' 形式
        if name_pos >= 2 && &region[name_pos - 2..name_pos] == "</" {
            last = Some(from + name_pos + "invoke>".len());
        } else if name_pos >= 1 && bytes[name_pos - 1] == b':' {
            // '</prefix:invoke>' 形式
            let mut j = name_pos - 1; // ':'
            while j > 0 && {
                let c = bytes[j - 1];
                c.is_ascii_alphanumeric() || c == b'_'
            } {
                j -= 1;
            }
            if j >= 2 && &region[j - 2..j] == "</" {
                last = Some(from + name_pos + "invoke>".len());
            }
        }
        search = name_pos + "invoke>".len();
    }
    last
}

/// 从标签字符串中抠出 `name="..."` 的值（取第一个匹配）
#[allow(dead_code)]
fn extract_name_attr(tag: &str) -> Option<String> {
    let needle = "name=\"";
    let rel = tag.find(needle)?;
    let start = rel + needle.len();
    let end_rel = tag[start..].find('"')?;
    Some(tag[start..start + end_rel].to_string())
}

/// 解析一个完整 invoke 块，抠出 (tool_name, input_json_string)
///
/// - tool name 来自 invoke 开标签的 `name="..."`（兼容 antml: 前缀）
/// - 参数为零个或多个 `<parameter name="K">V</parameter>`（兼容前缀）
/// - 参数值取到下一个参数开标签前的**最后一个** `</parameter>` 为界（贪婪），
///   允许多行 / 含 `<` / 中文 / 含字面 `</parameter>`（P0-1 修复）
/// - 用 serde_json 拼成 object（值都是字符串，自动转义）
/// - 无合法 name 或拼不出合法 JSON 返回 `None`
#[allow(dead_code)]
fn parse_invoke_block(block: &str) -> Option<(String, String)> {
    // invoke 开标签 = 块开头到第一个 '>'
    let open_end = block.find('>')?;
    let open_tag = &block[..=open_end];
    let tool_name = extract_name_attr(open_tag)?;
    if tool_name.is_empty() {
        return None;
    }

    let mut map = serde_json::Map::new();
    let body = &block[open_end + 1..];
    let mut cursor = 0usize;
    while let Some(rel) = body[cursor..].find("parameter name=\"") {
        let name_kw = cursor + rel;
        // 确认是真正的 '<parameter' 或 '<prefix:parameter' 开标签
        // name_kw 指向 'parameter'，往前应是 '<' 或 '<prefix:'
        // 确认是真正的开标签（'<parameter' / '<prefix:parameter'）；仅用于校验，不需要位置值
        if open_tag_lt_pos(body, name_kw).is_none() {
            cursor = name_kw + "parameter".len();
            continue;
        }
        // 找该参数开标签的 '>'
        let tag_gt = match body[name_kw..].find('>') {
            Some(r) => name_kw + r,
            None => break, // 开标签未闭合，停止
        };
        let param_open_tag = &body[name_kw..tag_gt + 1];
        // 从 'parameter name="..."' 抠 key（剥掉前缀干扰：直接找 name="）
        let key = match extract_name_attr(param_open_tag) {
            Some(k) => k,
            None => {
                cursor = tag_gt + 1;
                continue;
            }
        };
        // 参数值取到 </parameter>（兼容前缀）为界。find_param_close 较贵，只调一次，
        // 同时复用 (闭标签起始, 闭标签结束) 两个值：起始用于切值，结束用于推进游标。
        let val_start = tag_gt + 1;
        let (close_start, close_end) = match find_param_close(body, val_start) {
            Some(pair) => pair,
            None => break, // 值未闭合，停止
        };
        let value = &body[val_start..close_start];
        map.insert(key, serde_json::Value::String(value.to_string()));
        // 推进到闭标签之后
        cursor = close_end;
    }

    let obj = serde_json::Value::Object(map);
    let s = serde_json::to_string(&obj).ok()?;
    Some((tool_name, s))
}

/// 从 `from` 开始查找第一个 parameter 闭标签，返回 (起始位置, 结束位置 exclusive)
///
/// 兼容裸 `</parameter>` 与带前缀 `</prefix:parameter>`。
#[allow(dead_code)]
fn find_param_close(body: &str, from: usize) -> Option<(usize, usize)> {
    // P0-1：参数值（尤其 apply_patch 的 patch 正文）可能含字面 `</parameter>`。
    // 朴素「取第一个 </parameter>」会把值截断。改成「贪婪取边界内最后一个 </parameter>」：
    // 边界 = 下一个 `<parameter name="` 开标签（多参数场景），没有则到 body 结尾。
    // 这样：① 单参数（含 apply_patch）取到真正的最后一个闭合，内容里的字面闭合不误伤；
    //      ② 多参数仍按下一个参数开标签正确切分。
    // 局限（已诚实标注）：若参数值里同时含字面 `<parameter name="`，边界判定会偏早；
    // 实测 apply_patch 正文极少出现该字面串，可接受。
    let boundary = match find_next_param_open(body, from) {
        Some(p) => p,
        None => body.len(),
    };
    let region = &body[from..boundary];
    let kw = "parameter>";
    let mut last: Option<(usize, usize)> = None;
    let mut search = 0usize;
    let bytes = region.as_bytes();
    while let Some(rel) = region[search..].find(kw) {
        let name_pos = search + rel;
        // '</parameter>' 形式
        if name_pos >= 2 && &region[name_pos - 2..name_pos] == "</" {
            last = Some((from + name_pos - 2, from + name_pos + kw.len()));
        } else if name_pos >= 1 && bytes[name_pos - 1] == b':' {
            // '</prefix:parameter>' 形式
            let mut j = name_pos - 1; // ':'
            while j > 0 && {
                let c = bytes[j - 1];
                c.is_ascii_alphanumeric() || c == b'_'
            } {
                j -= 1;
            }
            if j >= 2 && &region[j - 2..j] == "</" {
                last = Some((from + j - 2, from + name_pos + kw.len()));
            }
        }
        search = name_pos + kw.len();
    }
    last
}

/// 从 `from` 开始查找下一个 `<parameter name="`（或 `<prefix:parameter name="`）开标签的字节位置。
/// 用于 `find_param_close` 的贪婪边界：当前参数值最多吃到下一个参数开标签之前。
#[allow(dead_code)]
fn find_next_param_open(body: &str, from: usize) -> Option<usize> {
    let mut search = from;
    while let Some(rel) = body[search..].find("parameter name=\"") {
        let kw_pos = search + rel;
        // 必须是真正的开标签：'parameter' 前面是 '<' 或 '<prefix:'
        if let Some(lt) = open_tag_lt_pos(body, kw_pos) {
            return Some(lt);
        }
        search = kw_pos + "parameter".len();
    }
    None
}

/// 剥掉块前文本尾部的独立 stray token 行（单独一行的 `call` / `count` / `card` / `court`）
///
/// 实测里 `<invoke>` 前常出现一行裸 `call`/`count`，需要从块前叙述文本里剥掉，
/// 避免泄漏给客户端。只剥“尾部、且独占一行”的 stray token，前面的正常叙述保留。
/// 已实测到的 stray token 集合：Opus 长上下文退化时，泄漏的 `<invoke>` 前常有一行裸的
/// `call` / `count` / `card`。集合形式便于以后扩充。
///
/// 生产语料（KiroStudio #70544 变体）里 `court` 是最主要的 stray token，故并入集合。
/// 中文变体 `課`/`课` 也是我们实测到的高置信泄漏词（见 LEAKED_CONTROL_TOKENS），一并纳入熔断计数，
/// 否则中文退化刷屏时逐字清洗能剥、但复读熔断（32 次截断止血）抓不到 → 仍会耗尽 max_tokens。
#[allow(dead_code)]
const STRAY_INVOKE_TOKENS: &[&str] = &["call", "count", "card", "court", "課", "课"];

/// thinking 缓冲上限(review Finding 5):上游持续吐纯空白时,纯空白分支既不 emit 也不收缩会让
/// thinking_buffer 无界增长 OOM。超此上限即强制按普通文本吐出收缩。256KiB 远超正常 thinking 前导空白。
const MAX_THINKING_BUFFER_BYTES: usize = 262_144;

/// 复读熔断阈值：同一个 stray token（call/count/card/court）连续作为独占一行重复出现
/// 超过这么多次，判定为「Opus 长上下文退化复读死循环」，立即熔断本轮文本输出。
///
/// 取值权衡：正常工具调用前最多出现 1 个引导词行（偶有 2~3），绝不会连续几十次。
/// 设为 32 远高于正常上限、又远低于退化时的数万次，既不误伤正常引导词，又能尽早止血。
#[allow(dead_code)]
const REPEAT_GUARD_TRIP_THRESHOLD: u32 = 32;

/// stray 泄漏观测词表(与 clean 层 LEAKED_CONTROL_TOKENS 对齐,纯观测用)。
const STRAY_OBSERVE_TOKENS: &[&str] = &["court", "course", "count", "care", "card", "call", "課", "课"];

/// 判断字符是否 CJK 表意文字(观测"stray 词紧贴 CJK"的判据,与 clean 层 is_leak_glue_char 同族)。
fn is_cjk_ideograph(c: char) -> bool {
    matches!(c, '\u{3400}'..='\u{9FFF}' | '\u{F900}'..='\u{FAFF}')
}

/// 【纯观测】扫 content 里的 stray 泄漏形态,累加两类计数(**不修改 content**):
/// - standalone:某 stray 词**独占一整行**(trim 后整行 == 词)——高置信泄漏(court 实测全独占行)。
/// - inline:某 stray 词出现在句中且**紧贴 CJK 表意字**(如 `重读course课`/`值是count的`)——
///   正常中英混排会有空格分隔,紧贴 CJK 是泄漏特征。用于点亮 clean 层够不到的句中黑洞。
/// 快路径:先 contains 任一词才细扫,正常文本零开销。
fn observe_stray_leak_forms(content: &str, standalone: &mut u32, inline: &mut u32) {
    // 快路径:一个都不含直接返回。
    if !STRAY_OBSERVE_TOKENS.iter().any(|t| content.contains(*t)) {
        return;
    }
    // 独占行:逐行 trim 后整行等于某 stray 词。
    for line in content.split('\n') {
        let t = line.trim();
        if STRAY_OBSERVE_TOKENS.contains(&t) {
            *standalone = standalone.saturating_add(1);
        }
    }
    // 句中紧贴 CJK:词出现处,其紧邻(前或后)是 CJK 表意字。
    for tok in STRAY_OBSERVE_TOKENS {
        let tb = tok.as_bytes();
        let mut from = 0usize;
        while let Some(rel) = content[from..].find(*tok) {
            let start = from + rel;
            let end = start + tb.len();
            let before_cjk = content[..start].chars().next_back().is_some_and(is_cjk_ideograph);
            let after_cjk = content[end..].chars().next().is_some_and(is_cjk_ideograph);
            if before_cjk || after_cjk {
                *inline = inline.saturating_add(1);
            }
            from = end;
        }
    }
}

/// 判断一个 trim 后的行是否"看起来像退化刷屏 token":短(≤6 字符)、且全为字母或全为 CJK 表意文字,
/// 无空格/标点/数字。用于逐行检测里放宽词表(不止已知的 call/count/card/court/課/课),
/// 但仍保守(要求整行就是这么个短纯词),正常句子/代码不会整行是这种。
fn is_short_flood_token(line: &str) -> bool {
    let n = line.chars().count();
    if n == 0 || n > 6 {
        return false;
    }
    let all_ascii_alpha = line.chars().all(|c| c.is_ascii_alphabetic());
    // CJK 统一表意文字区(含扩展 A):课/課 等中文单字刷屏。
    let all_cjk = line
        .chars()
        .all(|c| matches!(c, '\u{3400}'..='\u{9FFF}' | '\u{F900}'..='\u{FAFF}'));
    all_ascii_alpha || all_cjk
}

/// ② 结构性洪水检测:**不依赖换行、不依赖词表**。扫描文本里"同一个短 token 连续紧邻重复"的最长游程,
/// 覆盖单行连写 "课课课…课" / "coursecoursecourse…" / 逐字符重复,任意退化词都抓。
/// 命中(游程 ≥ 阈值)返回该游程起点的字节偏移(从那里截断)。
///
/// 算法:对每个可能的 token 长度(1..=6 字符),检测是否有从某位置起、同一 token 连续重复 ≥阈值次。
/// 优先抓最靠前的命中点。中文单字(len=1 char)刷屏是最常见形态,单独快速扫一遍。
fn detect_structural_flood(text: &str) -> Option<usize> {
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let n = chars.len();
    if n < REPEAT_GUARD_TRIP_THRESHOLD as usize {
        return None;
    }
    let thresh = REPEAT_GUARD_TRIP_THRESHOLD as usize;
    // 单字符游程(最常见:中文"课"连写、单字母连写)。只对"字母或 CJK"的字符计游程,
    // 避免把正常重复(如 "----" 分隔线、"...")误判——那些是标点不在此列。
    let is_floodable = |c: char| {
        c.is_ascii_alphabetic() || matches!(c, '\u{3400}'..='\u{9FFF}' | '\u{F900}'..='\u{FAFF}')
    };
    let mut i = 0usize;
    while i < n {
        let (byte_start, ch) = chars[i];
        if is_floodable(ch) {
            let mut j = i + 1;
            while j < n && chars[j].1 == ch {
                j += 1;
            }
            if j - i >= thresh {
                return Some(byte_start);
            }
            i = j;
        } else {
            i += 1;
        }
    }
    // 多字符 token 连写(如 "coursecourse…"):对 token 长度 2..=6 char 滑窗检测连续相等块。
    for tok_len in 2..=6usize {
        if n < tok_len * thresh {
            continue;
        }
        let mut i = 0usize;
        while i + tok_len <= n {
            // 当前 token = chars[i..i+tok_len],要求全 floodable(纯词,不含空格标点)。
            if !chars[i..i + tok_len].iter().all(|(_, c)| is_floodable(*c)) {
                i += 1;
                continue;
            }
            let tok: Vec<char> = chars[i..i + tok_len].iter().map(|(_, c)| *c).collect();
            let mut reps = 1usize;
            let mut k = i + tok_len;
            while k + tok_len <= n
                && chars[k..k + tok_len].iter().map(|(_, c)| *c).eq(tok.iter().copied())
            {
                reps += 1;
                k += tok_len;
            }
            if reps >= thresh {
                return Some(chars[i].0);
            }
            i = if reps > 1 { k } else { i + 1 };
        }
    }
    None
}

/// 块级复读折叠：对「已完整的整段文本」做一次性复读熔断。
///
/// 用于非流式 / web_search loop 路径（`extract_invoke_content_blocks` 入口）——
/// 那条路不经过流式 `emit_text_delta_raw` 的逐 chunk 熔断，所以在这里独立兜一次。
///
/// 规则与流式版一致：同一个 `STRAY_INVOKE_TOKENS`（call/count/card/court）连续作为独占一行
/// 重复超过 `REPEAT_GUARD_TRIP_THRESHOLD` 次，判定为 Opus 退化复读，**从超阈值处截断**，
/// 丢弃其后的全部复读垃圾（断雪球、不灌历史）。阈值内的少量引导词重复原样保留。
#[allow(dead_code)]
fn collapse_stray_token_floods(text: &str) -> std::borrow::Cow<'_, str> {
    let mut last_line = "";
    let mut run: u32 = 0;
    let mut cut_at: Option<usize> = None;
    let mut offset = 0usize;
    for segment in text.split_inclusive('\n') {
        let line = segment.trim();
        if STRAY_INVOKE_TOKENS.contains(&line) {
            if line == last_line {
                run += 1;
            } else {
                last_line = line;
                run = 1;
            }
            if run >= REPEAT_GUARD_TRIP_THRESHOLD {
                // 从「本段（这一行）开头」截断：保留阈值内已累计的内容。
                cut_at = Some(offset);
                break;
            }
        } else if !line.is_empty() {
            last_line = line;
            run = 0;
        }
        offset += segment.len();
    }
    match cut_at {
        Some(pos) => std::borrow::Cow::Owned(text[..pos].to_string()),
        None => std::borrow::Cow::Borrowed(text),
    }
}

/// 剥掉块前文本尾部独占一行的 stray token（保留其前一行的换行）
#[allow(dead_code)]
fn strip_trailing_stray_tokens(before: &str) -> &str {
    let mut end = before.len();
    loop {
        let bytes = before.as_bytes();
        // 先跳过尾部的换行符，定位“最后一行”的真实结束位置
        let mut e = end;
        while e > 0 && (bytes[e - 1] == b'\n' || bytes[e - 1] == b'\r') {
            e -= 1;
        }
        let line_start = before[..e].rfind('\n').map(|p| p + 1).unwrap_or(0);
        let last_line = before[line_start..e].trim();
        // Opus 长上下文退化时，泄漏的 <invoke> 前常有一个孤立的 stray token 行。
        // 实测样本里出现过 call / count / card / court；用集合便于以后扩充。
        if STRAY_INVOKE_TOKENS.contains(&last_line) {
            // 只剥 stray token 行本身，【保留】前一行末尾的换行符。
            // 旧实现用 line_start - 1 把前一行的换行也吞掉，会把前面的叙述正文和
            // 后续 <invoke> 挤到同一行，导致 invoke_looks_like_real_leak 的“行首”判定
            // 失败、漏捞真泄漏（narrative\ncall\n<invoke>）。改成 end = line_start：
            //   "some text\ncall" -> "some text\n"（行首信号保留）
            //   "call"（无前导正文）-> ""（line_start==0）
            end = line_start;
            if end == 0 {
                return "";
            }
        } else {
            break;
        }
    }
    &before[..end]
}

/// 判定一个 `<invoke>` 块到底像“真泄漏的工具调用”还是“正文里讨论的文本”
///
/// 实测真泄漏的 `<invoke>` 都出现在**行首**（前面是流的开头、或上一行已经换行结束），
/// 而正文讨论里的 `<invoke>` 一般**嵌在一句话中间**——前面同一行还有普通文字。
///
/// 判定规则（输入 `before` 是 `<invoke>` 之前、已剥过 stray token 的文本）：
/// - `before` 为空（`<invoke>` 在流开头）→ 像真泄漏，抓。
/// - `before` 去掉尾部空格/制表符后以换行结尾（`<invoke>` 独占新行）→ 抓。
/// - 否则（同一行前面还有非空白正文）→ 像讨论文本，不抓。
///
/// 注意：这里的“尾部空白”只剥行内空白（空格 / 制表符），不剥换行；
/// 换行结尾才是“另起一行”的信号。
#[allow(dead_code)]
fn invoke_looks_like_real_leak(before: &str) -> bool {
    // 剥掉尾部的行内空白（空格 / 制表符），但保留换行
    let trimmed = before.trim_end_matches([' ', '\t']);
    // 行首：要么前面什么都没有，要么上一行已经以换行结束
    trimmed.is_empty() || trimmed.ends_with('\n') || trimmed.ends_with('\r')
}

/// 推进「代码围栏」奇偶状态，对切分到多个 chunk 的 ``` 分隔符鲁棒。
///
/// 只在遇到换行符时才对「已重组的完整行」判定是否为围栏行（行首去空白后以 ``` 开头）。
/// 未遇换行的尾部留在 `partial` 里，等后续 chunk 拼齐——所以即使 ``` 被切成
/// `` `` `` + `` ` `` 两个 chunk，重组成完整行后仍能正确翻转 `open`。
///
/// 返回值仅在内部使用；主要副作用是更新 `open` 与 `partial`。
#[allow(dead_code)]
fn advance_code_fence_state(open: &mut bool, partial: &mut String, text: &str) {
    // review Finding 6 修复:围栏判定只需"行首若干字节是否 ```",无换行的超长行会让 partial 无界增长。
    // 一旦当前行已超过判定所需长度(远大于 "```" + 缩进),就不再累积字符(围栏与否已定),防无界 String。
    const FENCE_SCAN_LINE_CAP: usize = 256;
    for ch in text.chars() {
        if ch == '\n' {
            if partial.trim_start().starts_with("```") {
                *open = !*open;
            }
            partial.clear();
        } else if partial.len() < FENCE_SCAN_LINE_CAP {
            partial.push(ch);
        }
        // 超过 cap 的同一行剩余字符丢弃(围栏判定不需要;遇换行才重置)。
    }
}

/// 纯函数：在不改动真实状态的前提下，试算「把 `text` 走完之后围栏是否打开」。
/// 用于 drain 决策处判断某个 `<invoke>` 是否落在围栏内。
#[allow(dead_code)]
fn fence_open_after(open: bool, partial: &str, text: &str) -> bool {
    let mut o = open;
    let mut p = partial.to_string();
    advance_code_fence_state(&mut o, &mut p, text);
    // 还要考虑：partial 里残留的「未换行行」如果本身已经是 ``` 开头，
    // 它在遇到换行前不算翻转（保守：只有完整行才翻转）。这里返回已翻转的 o。
    o
}

/// 计算缓冲区末尾“可能是部分 `<invoke` 开标签前缀”的字节数，需要保留等待更多内容
///
/// 例如缓冲区以 `<inv` / `<` / `<i` 结尾时，可能是被切碎的 invoke 开标签，
/// 保留这段尾巴等下一个 chunk 拼齐，避免把半个标签当文本吐出去。
#[allow(dead_code)]
fn partial_invoke_tag_suffix_len(buf: &str) -> usize {
    // 任何形如 `<...`（最后一个 '<' 之后没有 '>'）的尾巴都可能是部分开标签
    if let Some(lt) = buf.rfind('<') {
        if !buf[lt..].contains('>') {
            return buf.len() - lt;
        }
    }
    0
}


/// 检测文本片段里是否出现「文本化的工具调用标记」。
/// 覆盖:Anthropic 工具调用语法 `<invoke`/`</invoke>`/`<parameter name=`(不论是否带 antml: 前缀),
/// 及 `<function_calls>` 包裹。仅诊断用(探针),不改控制流。
fn contains_textified_tool_call(text: &str) -> bool {
    text.contains("<invoke")
        || text.contains("</invoke")
        || text.contains("<parameter name=")
        || text.contains("function_calls>")
        || text.contains("antml:")
}

/// 泄漏 token 探针总开关（环境变量 `KIRO_LEAK_TRACE` 非空即开）。仿 `KIRO_TOOL_TRACE`,平时零开销。
/// 开启时收尾额外打印本请求泄漏 token 清洗全貌，用于坐实 #70544 在流经网关的下游泄漏程度。
fn leak_trace_enabled() -> bool {
    use std::sync::OnceLock;
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var("KIRO_LEAK_TRACE")
            .map(|v| !v.trim().is_empty() && v != "0")
            .unwrap_or(false)
    })
}

/// 记录一帧 `toolUseEvent.input` 的合并轨迹（仅 `KIRO_TOOL_TRACE` 开启时）。
///
/// 输出到 `tracing` 的 `kiro::tool_trace` target（`RUST_LOG=kiro::tool_trace=trace` 可单独放行），
/// 逐帧打印：model / tool_use_id / name / stop / 原始帧原文 / 合并前后缓冲，非前缀重写与非法 JSON
/// 额外标注。原文可能含用户数据，故仅在显式开探针时输出。
fn trace_tool_frame(
    model: &str,
    tool_use_id: &str,
    name: &str,
    stop: bool,
    raw_frame: &str,
    buf_before: &str,
    buf_after: &str,
) {
    if !tool_trace_enabled() {
        return;
    }
    let frame_ok = is_complete_json(raw_frame);
    let after_ok = is_complete_json(buf_after);
    // `}{` 粘连是类型 C 的典型非法特征，单独标注便于一眼识别。
    let glued = buf_after.contains("}{") || buf_after.contains("} {");
    tracing::trace!(
        target: "kiro::tool_trace",
        model = model,
        tool_use_id = tool_use_id,
        tool_name = name,
        stop = stop,
        raw_frame_len = raw_frame.len(),
        raw_frame_json_ok = frame_ok,
        buf_before_len = buf_before.len(),
        buf_after_len = buf_after.len(),
        buf_after_json_ok = after_ok,
        buf_after_glued = glued,
        raw_frame = %raw_frame,
        buf_after = %buf_after,
        "[tool_trace] 帧合并轨迹"
    );
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

    // ========================================================================
    // 文本化 invoke 解析纯函数单测（从 ZyphrZero/kiro.rs 移植 + KiroStudio 补充）
    // 这些函数是纯函数，直接对字符串断言，不经过 StreamContext 状态机。
    // 命名统一含 `invoke`，便于 `cargo test -- invoke` 精准挑选。
    // ========================================================================

    #[test]
    fn test_invoke_parse_complete_block() {
        // 🟢 完整块：<invoke name="Bash"><parameter name="command">ls</parameter></invoke>
        let block = r#"<invoke name="Bash"><parameter name="command">ls</parameter></invoke>"#;
        let (name, input) = parse_invoke_block(block).expect("应解析出 tool");
        assert_eq!(name, "Bash");
        let parsed: serde_json::Value = serde_json::from_str(&input).expect("input 应为合法 JSON");
        assert_eq!(parsed["command"], "ls");
    }

    #[test]
    fn test_invoke_parse_antml_prefix_tolerated() {
        // 🟢 带 antml: 命名空间前缀应被容忍（开/闭标签均带前缀）。
        // 用拼接构造标签，避免源码里出现字面工具调用标记。
        let ns = "antml:";
        let block = format!(
            "<{ns}invoke name=\"X\"><{ns}parameter name=\"y\">v</{ns}parameter></{ns}invoke>"
        );
        let (name, input) = parse_invoke_block(&block).expect("带前缀的块应能解析");
        assert_eq!(name, "X");
        let parsed: serde_json::Value = serde_json::from_str(&input).expect("input 应为合法 JSON");
        assert_eq!(parsed["y"], "v");
    }

    #[test]
    fn test_invoke_parse_param_value_with_lt_multiline_chinese() {
        // 🟢 参数值含 `<`、多行、中文 → 不被截断
        let value = "第一行 a < b\n第二行 路径 /tmp/中文";
        let block = format!(
            "<invoke name=\"write_file\"><parameter name=\"content\">{value}</parameter></invoke>"
        );
        let (name, input) = parse_invoke_block(&block).expect("应解析出 tool");
        assert_eq!(name, "write_file");
        let parsed: serde_json::Value = serde_json::from_str(&input).expect("input 应为合法 JSON");
        assert_eq!(parsed["content"], value, "参数值应完整保留（含 < / 多行 / 中文）");
    }

    #[test]
    fn test_invoke_parse_apply_patch_literal_close_tag_survives() {
        // 🟢 P0-1：apply_patch 正文里含字面 </parameter> —— 贪婪取最后一个闭合，不被提前截断。
        let closing = format!("</{}>", "parameter");
        let value = format!("patch line 1\n此处有字面 {closing} 标记\npatch line 3");
        let block = format!(
            "<invoke name=\"apply_patch\"><parameter name=\"input\">{value}</parameter></invoke>"
        );
        let (name, input) = parse_invoke_block(&block).expect("应解析出 tool");
        assert_eq!(name, "apply_patch");
        let parsed: serde_json::Value = serde_json::from_str(&input).expect("input 应为合法 JSON");
        assert_eq!(parsed["input"], value, "含字面闭合标签的正文应完整保留");
    }

    #[test]
    fn test_invoke_parse_two_params() {
        // 🟢 多参数：按下一个参数开标签正确切分
        let block = r#"<invoke name="t"><parameter name="a">1</parameter><parameter name="b">2</parameter></invoke>"#;
        let (name, input) = parse_invoke_block(block).expect("应解析出 tool");
        assert_eq!(name, "t");
        let parsed: serde_json::Value = serde_json::from_str(&input).expect("input 应为合法 JSON");
        assert_eq!(parsed["a"], "1");
        assert_eq!(parsed["b"], "2");
    }

    #[test]
    fn test_invoke_parse_no_params() {
        // 🟢 零参数块 → 合法但 input 为空对象
        let block = r#"<invoke name="noop"></invoke>"#;
        let (name, input) = parse_invoke_block(block).expect("应解析出 tool");
        assert_eq!(name, "noop");
        assert_eq!(input, "{}");
    }

    #[test]
    fn test_invoke_parse_empty_name_rejected() {
        // 🔴 name 为空 → None
        let block = r#"<invoke name=""><parameter name="x">v</parameter></invoke>"#;
        assert!(parse_invoke_block(block).is_none(), "空 name 应被拒绝");
    }

    #[test]
    fn test_invoke_find_start_bare_and_prefixed() {
        // 🟢 裸 `<invoke` 与带前缀 `<prefix:invoke` 都能定位到 '<'
        assert_eq!(find_invoke_start("<invoke name=\"x\">"), Some(0));
        assert_eq!(find_invoke_start("abc\n<invoke name=\"x\">"), Some(4));
        let prefixed = "<invoke name=\"x\">";
        assert_eq!(find_invoke_start(prefixed), Some(0));
    }

    #[test]
    fn test_invoke_find_start_backtick_wrapped_is_skipped() {
        // 🔴 被反引号包裹的 <invoke 视为引用，跳过
        assert_eq!(find_invoke_start("示例：`<invoke name=\"x\">`"), None);
    }

    #[test]
    fn test_invoke_find_start_ignores_invoked_word() {
        // 🔴 `invoked` 这类词不构成开标签（标签名后需边界字符）
        assert_eq!(find_invoke_start("the model invoked a tool"), None);
    }

    #[test]
    fn test_invoke_block_end_greedy_and_unclosed() {
        // 🟢 完整块 → 返回含闭标签的结束位置；未闭合 → None
        let full = r#"<invoke name="x"><parameter name="c">ls</parameter></invoke>"#;
        let end = find_invoke_block_end(full, 0).expect("完整块应有结束位置");
        assert_eq!(end, full.len());

        let unclosed = r#"<invoke name="x"><parameter name="c">ls"#;
        assert!(find_invoke_block_end(unclosed, 0).is_none(), "未闭合块应返回 None");
    }

    #[test]
    fn test_invoke_next_open_finds_second_burst() {
        // 🟢 连发 burst：A 紧跟 B，find_next_invoke_open 跳过 A 自身开标签，定位到 B
        let s = r#"<invoke name="a"><parameter name="x">1</parameter></invoke><invoke name="b"><parameter name="y">2</parameter></invoke>"#;
        let b_pos = find_next_invoke_open(s, 0).expect("应找到第二个块开标签");
        assert_eq!(&s[b_pos..b_pos + "<invoke name=\"b\"".len()], "<invoke name=\"b\"");
    }

    #[test]
    fn test_invoke_two_blocks_parsed_via_block_end() {
        // 🟢 用 find_invoke_block_end + parse_invoke_block 串起两块，各自独立解析
        let s = r#"<invoke name="a"><parameter name="x">1</parameter></invoke><invoke name="b"><parameter name="y">2</parameter></invoke>"#;
        let start_a = find_invoke_start(s).unwrap();
        let end_a = find_invoke_block_end(s, start_a).unwrap();
        let (na, _) = parse_invoke_block(&s[start_a..end_a]).unwrap();
        assert_eq!(na, "a");

        let start_b = find_next_invoke_open(s, start_a).unwrap();
        let end_b = find_invoke_block_end(s, start_b).unwrap();
        let (nb, _) = parse_invoke_block(&s[start_b..end_b]).unwrap();
        assert_eq!(nb, "b");
        assert_eq!(end_b, s.len());
    }

    #[test]
    fn test_invoke_last_close_greedy_skips_literal() {
        // 🟢 区间内含字面 </invoke> → find_last_invoke_close 取最后一个真闭合
        let s = format!(
            "<invoke name=\"x\"><parameter name=\"c\">正文里有字面 {} 标记</parameter></invoke>",
            "</invoke>"
        );
        let end = find_last_invoke_close(&s, 0, s.len()).expect("应找到最后一个闭合");
        assert_eq!(end, s.len());
    }

    #[test]
    fn test_invoke_open_tag_lt_pos_bare_and_prefixed() {
        // 🟢 open_tag_lt_pos：裸 `<tag` 与 `<prefix:tag` 都能回溯到 '<'
        let bare = "<invoke";
        let name_pos = bare.find("invoke").unwrap();
        assert_eq!(open_tag_lt_pos(bare, name_pos), Some(0));

        let prefixed = "<invoke";
        let np = prefixed.find("invoke").unwrap();
        assert_eq!(open_tag_lt_pos(prefixed, np), Some(0));

        // 前面不是 '<' 也不是合法前缀 → None
        let bad = "xinvoke";
        let bp = bad.find("invoke").unwrap();
        assert_eq!(open_tag_lt_pos(bad, bp), None);
    }

    #[test]
    fn test_invoke_extract_name_attr() {
        assert_eq!(extract_name_attr(r#"<invoke name="Bash">"#), Some("Bash".to_string()));
        assert_eq!(extract_name_attr(r#"<parameter name="cmd">"#), Some("cmd".to_string()));
        assert_eq!(extract_name_attr("<invoke>"), None);
    }

    #[test]
    fn test_invoke_next_param_open() {
        // 🟢 find_next_param_open 定位下一个参数开标签的 '<'
        let body = r#"<parameter name="a">1</parameter><parameter name="b">2</parameter>"#;
        // 从第一个参数值区起找下一个参数开标签
        let first_val_start = body.find('>').unwrap() + 1;
        let next = find_next_param_open(body, first_val_start).expect("应找到第二个参数开标签");
        assert_eq!(&body[next..next + "<parameter name=\"b\"".len()], "<parameter name=\"b\"");
    }

    #[test]
    fn test_invoke_looks_like_real_leak_line_start() {
        // 🟢 行首（空 / 换行结尾）→ 像真泄漏；句中 → 不像
        assert!(invoke_looks_like_real_leak(""));
        assert!(invoke_looks_like_real_leak("some text\n"));
        assert!(invoke_looks_like_real_leak("some text\n   "));
        assert!(invoke_looks_like_real_leak("some text\r"));
        assert!(!invoke_looks_like_real_leak("讨论 "));
        assert!(!invoke_looks_like_real_leak("- "));
    }

    #[test]
    fn test_invoke_strip_trailing_stray_preserves_newline() {
        // 回归：narrative\ncall → 只剥 stray 行，保留前一行换行（行首信号不丢）
        let got = strip_trailing_stray_tokens("some text\ncall");
        assert_eq!(got, "some text\n", "必须保留叙述行末的换行");
        assert!(invoke_looks_like_real_leak(got), "剥完仍应像行首泄漏");
    }

    #[test]
    fn test_invoke_strip_trailing_stray_court_token() {
        // 🟢 KiroStudio 生产语料：court 是主要 stray token，应被剥
        assert_eq!(strip_trailing_stray_tokens("先看结果。\ncourt"), "先看结果。\n");
        assert_eq!(strip_trailing_stray_tokens("court"), "");
        // 多个连续 stray 行全部剥掉
        assert_eq!(strip_trailing_stray_tokens("正文\ncall\ncourt"), "正文\n");
    }

    #[test]
    fn test_invoke_strip_trailing_stray_keeps_non_stray() {
        // 🔴 非 stray 的末行不剥
        assert_eq!(strip_trailing_stray_tokens("hello world"), "hello world");
    }

    #[test]
    fn test_invoke_collapse_stray_token_floods() {
        // 🟢 复读死循环：court 独占一行连续 100 次 → 从超阈值处截断
        let mut s = String::from("正文引导\n");
        for _ in 0..100 {
            s.push_str("court\n");
        }
        s.push_str("<invoke name=\"x\">");
        let collapsed = collapse_stray_token_floods(&s);
        // 截断后应只保留阈值内内容，court 出现次数远小于 100
        let court_count = collapsed.matches("court").count();
        assert!(
            court_count < 100,
            "复读应被熔断截断，court 次数={court_count}"
        );
        assert!(!collapsed.contains("<invoke"), "超阈值后的内容应被丢弃");
    }

    #[test]
    fn test_invoke_collapse_stray_token_chinese_flood() {
        // 🟢 中文变体 課/课 独占行复读也应被熔断(修复:原集合漏了中文,逐字清洗剥得掉但熔断抓不到)。
        for tok in ["課", "课"] {
            let mut s = String::from("正文\n");
            for _ in 0..100 {
                s.push_str(tok);
                s.push('\n');
            }
            let collapsed = collapse_stray_token_floods(&s);
            let cnt = collapsed.matches(tok).count();
            assert!(cnt < 100, "中文 {tok} 复读应被熔断截断,次数={cnt}");
        }
    }

    #[test]
    fn test_invoke_collapse_stray_token_below_threshold() {
        // 🔴 阈值内的少量引导词重复原样保留
        let s = "call\ncall\n<invoke name=\"x\">";
        let collapsed = collapse_stray_token_floods(s);
        assert_eq!(collapsed, s, "阈值内不应截断");
    }

    #[test]
    fn test_invoke_code_fence_state_toggle() {
        // 🟢 代码围栏奇偶翻转：一对 ``` 归零
        let mut open = false;
        let mut partial = String::new();
        advance_code_fence_state(&mut open, &mut partial, "```rust\nlet x = 1;\n```\n");
        assert!(!open, "一对围栏后应回到关闭态");

        // 单个开围栏 → 打开
        let mut open2 = false;
        let mut partial2 = String::new();
        advance_code_fence_state(&mut open2, &mut partial2, "```\n代码\n");
        assert!(open2, "单个开围栏后应为打开态");
    }

    #[test]
    fn test_invoke_fence_open_after_pure() {
        // 🟢 fence_open_after 纯试算，不改传入状态
        assert!(fence_open_after(false, "", "```\n"), "进入围栏");
        assert!(!fence_open_after(true, "", "```\n"), "离开围栏");
        assert!(!fence_open_after(false, "", "普通文本\n"), "普通文本不翻转");
    }

    #[test]
    fn test_invoke_partial_tag_suffix_len() {
        // 🟢 缓冲区末尾的半个开标签应被识别为需保留的尾巴
        assert_eq!(partial_invoke_tag_suffix_len("hello<inv"), 4);
        assert_eq!(partial_invoke_tag_suffix_len("hello<"), 1);
        // 已闭合的标签结尾 → 无需保留
        assert_eq!(partial_invoke_tag_suffix_len("<invoke>"), 0);
        assert_eq!(partial_invoke_tag_suffix_len("no angle bracket"), 0);
    }

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

    // ===== 文本化 invoke 重组端到端(旧代码上会失败:旧代码把 <invoke> 当纯文本吐,不重组)=====

    /// 造一个开了重组 + 声明了工具 Bash 的 ctx。
    fn mk_reclaim_ctx() -> StreamContext {
        let mut known = std::collections::HashSet::new();
        known.insert("Bash".to_string());
        StreamContext::new_full("claude-opus-4.6", 10, false, HashMap::new(), known)
    }

    /// 判定事件流里是否有结构化 tool_use 的 content_block_start。
    fn has_tool_use_block(events: &[SseEvent]) -> bool {
        events.iter().any(|e| {
            e.event == "content_block_start" && e.data["content_block"]["type"] == "tool_use"
        })
    }

    #[test]
    fn test_reclaim_textified_invoke_to_tool_use() {
        // 行首完整 <invoke name="Bash"><parameter name="command">ls</parameter></invoke> + 工具名已声明
        // → 应重组成结构化 tool_use,且收尾 stop_reason=tool_use。用 concat 拼避免源码里出现字面工具标签。
        let mut ctx = mk_reclaim_ctx();
        let lt = "<";
        let block = format!(
            "{lt}invoke name=\"Bash\">{lt}parameter name=\"command\">ls -la{lt}/parameter>{lt}/invoke>"
        );
        let mut events = ctx.process_assistant_response(&block);
        events.extend(ctx.flush_invoke_sniff_buffer());
        assert!(has_tool_use_block(&events), "行首完整 invoke 块应重组成 tool_use");
        assert_eq!(ctx.reclaimed_invoke_count, 1);
        assert_eq!(ctx.state_manager.get_stop_reason(), "tool_use", "重组后 stop_reason 应为 tool_use");
    }

    #[test]
    fn test_reclaim_gated_by_unknown_tool_name() {
        // 工具名硬护栏:解析出的工具名不在声明表里 → 不重组,当普通文本吐(宁可漏捞不误执行)。
        let mut known = std::collections::HashSet::new();
        known.insert("Read".to_string()); // 只声明 Read,没声明 Bash
        let mut ctx = StreamContext::new_full("claude-opus-4.6", 10, false, HashMap::new(), known);
        let lt = "<";
        let block = format!("{lt}invoke name=\"Bash\">{lt}parameter name=\"x\">1{lt}/parameter>{lt}/invoke>");
        let mut events = ctx.process_assistant_response(&block);
        events.extend(ctx.flush_invoke_sniff_buffer());
        assert!(!has_tool_use_block(&events), "未声明的工具名不应被重组执行");
        assert_eq!(ctx.reclaimed_invoke_count, 0);
    }

    #[test]
    fn test_reclaim_split_across_chunks() {
        // 跨 chunk 切分的 invoke 块:分片到达仍应重组(sniff 缓冲 hold 到闭合)。
        let mut ctx = mk_reclaim_ctx();
        let lt = "<";
        let mut events = Vec::new();
        events.extend(ctx.process_assistant_response(&format!("{lt}invoke name=\"Ba")));
        events.extend(ctx.process_assistant_response(&format!("sh\">{lt}parameter name=\"command\">echo hi")));
        events.extend(ctx.process_assistant_response(&format!("{lt}/parameter>{lt}/invoke>")));
        events.extend(ctx.flush_invoke_sniff_buffer());
        assert!(has_tool_use_block(&events), "跨 chunk 分片的 invoke 应重组成 tool_use");
    }

    #[test]
    fn test_reclaim_disabled_when_no_tools_declared() {
        // 未声明任何工具(known 空)→ 不进重组路径,<invoke> 原样当文本吐(new_with_thinking 空集=不启用)。
        let mut ctx = StreamContext::new_with_thinking("claude-opus-4.6", 10, false, HashMap::new());
        let lt = "<";
        let block = format!("{lt}invoke name=\"Bash\">{lt}parameter name=\"c\">x{lt}/parameter>{lt}/invoke>");
        let events = ctx.process_assistant_response(&block);
        assert!(!has_tool_use_block(&events), "无声明工具时不重组");
        assert!(ctx.invoke_sniff_buffer.is_empty(), "不启用重组则不进 sniff 缓冲");
    }

    // ===== 结构性 stray 熔断(治 course/课 打地鼠 + 修 thinking/无工具盲区,旧代码上会失败)=====

    #[test]
    fn test_structural_flood_single_line_cjk() {
        // 单行连写「课」×100(无换行)——旧独占行匹配漏,结构性检测应抓到并从游程起点截断。
        let s = format!("正常开头 {}", "课".repeat(100));
        let cut = detect_structural_flood(&s);
        assert!(cut.is_some(), "单行连写课刷屏应被结构性检测命中");
    }

    #[test]
    fn test_structural_flood_multichar_course() {
        // "coursecourse…" ×40(词表里根本没 course)——多字符 token 连写应被抓。
        let s = "course".repeat(40);
        assert!(detect_structural_flood(&s).is_some(), "course 连写应被结构性检测命中(不靠词表)");
    }

    #[test]
    fn test_structural_flood_normal_text_safe() {
        // 正常文本(含少量重复词)不应误判。
        assert!(detect_structural_flood("这是一段正常的中文回复,讲解代码逻辑和实现细节。").is_none());
        assert!(detect_structural_flood("the quick brown fox jumps over the lazy dog").is_none());
        assert!(detect_structural_flood("aaa bbb ccc").is_none(), "短重复但未达阈值不误判");
    }

    #[test]
    fn test_observe_stray_leak_forms() {
        let mut sa = 0u32;
        let mut il = 0u32;
        // 独占行:course 单独一行 → standalone。
        observe_stray_leak_forms("正常\ncourse\n继续", &mut sa, &mut il);
        assert_eq!(sa, 1, "course 独占行应计 standalone");
        // 句中紧贴 CJK:`重读course了` 里 course 前后都贴 CJK → inline。
        let (mut sa2, mut il2) = (0u32, 0u32);
        observe_stray_leak_forms("重读course了", &mut sa2, &mut il2);
        assert_eq!(il2, 1, "句中紧贴 CJK 的 course 应计 inline");
        // 正常英文(有空格分隔)不误判:"the course is" 里 course 两侧是空格非 CJK。
        let (mut sa3, mut il3) = (0u32, 0u32);
        observe_stray_leak_forms("the course is good", &mut sa3, &mut il3);
        assert_eq!(sa3, 0, "正常英文散文不计 standalone");
        assert_eq!(il3, 0, "有空格分隔的正常 course 不计 inline");
        // 完全不含 stray 词:零开销快路径 + 零计数。
        let (mut sa4, mut il4) = (0u32, 0u32);
        observe_stray_leak_forms("一段完全正常的中文回复讲解逻辑", &mut sa4, &mut il4);
        assert_eq!(sa4 + il4, 0);
    }

    #[test]
    fn test_stray_guard_covers_thinking_path() {
        // 核心盲区修复:thinking 开着时,课刷屏也要被熔断(旧代码 thinking 提前 return 完全绕过)。
        let mut ctx = StreamContext::new_with_thinking("claude-opus-4.6", 10, true, HashMap::new());
        let flood = format!("{}", "课".repeat(200));
        let events = ctx.process_assistant_response(&flood);
        assert!(ctx.stray_guard_tripped, "thinking 路径的课刷屏也应触发熔断");
        // 熔断后本轮应几乎不吐正文(截断在游点起点)。
        let _ = events;
    }

    #[test]
    fn test_thinking_buffer_bounded_on_whitespace_flood() {
        // review Finding 5 修复:上游持续吐纯空白(无 <thinking>)时 thinking_buffer 不应无界增长。
        let mut ctx = StreamContext::new_with_thinking("claude-opus-4.6", 10, true, HashMap::new());
        // 每次喂一大块纯空白(不含 <thinking>),多轮累积。旧代码 buffer 只涨不裁。
        for _ in 0..20 {
            let _ = ctx.process_content_with_thinking(&" ".repeat(50_000));
        }
        assert!(
            ctx.thinking_buffer.len() <= MAX_THINKING_BUFFER_BYTES + 50_000,
            "纯空白洪水下 thinking_buffer 应被上限约束,实测 {} 字节",
            ctx.thinking_buffer.len()
        );
    }

    #[test]
    fn test_stray_guard_covers_no_tools_path() {
        // 核心盲区修复:无工具声明请求(known_tool_names 空)的课刷屏也要被处理。
        // 用单行连写(泄漏清洗器只剥行首独占,够不到单行连写)专门验证 guard 生效——
        // 逐行独占的课会被 clean_leaked_tokens 先剥掉(那也是有效清除路径),故此处用连写测 guard。
        let mut ctx = StreamContext::new_with_thinking("claude-opus-4.6", 10, false, HashMap::new());
        let flood = format!("正文{}", "课".repeat(200));
        let events = ctx.process_assistant_response(&flood);
        assert!(ctx.stray_guard_tripped, "无工具请求的单行课连写刷屏应触发 guard 熔断");
        // 熔断后吐出的文本里课的数量应远少于 200(截断在游程起点)。
        let emitted: String = events.iter()
            .filter_map(|e| e.data["delta"]["text"].as_str())
            .collect();
        assert!(emitted.matches('课').count() < 200, "熔断应截断掉大部分课");
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
    fn test_tool_input_illegal_json_at_stop_repaired_by_default() {
        // 修复层默认开：上游发本就非法的 JSON（\x 是 JSON 不支持的转义）→ 修复层介入修成合法后发出，
        // 客户端能正常 parse，不再报 Invalid tool parameters。`\xd7` 的 `\x` 非法 → 降级字面 `\\xd7`。
        let mut ctx = StreamContext::new_with_thinking("m", 1, false, HashMap::new());
        let _ = ctx.generate_initial_events();
        let (joined, n) = run_tool_frames(&mut ctx, &[(r#"{"a":"\xd7"}"#, true)]);
        assert_eq!(n, 1, "非法 JSON 也要发出(不静默空参)");
        assert!(
            serde_json::from_str::<serde_json::Value>(&joined).is_ok(),
            "修复层默认开：发给客户端的必须是合法 JSON，实际={}",
            joined
        );
        // 值语义：`\x` 非法转义降级为字面反斜杠，值为字面 `\xd7`。
        let v: serde_json::Value = serde_json::from_str(&joined).unwrap();
        assert_eq!(v["a"].as_str().unwrap(), r"\xd7", "非法 \\x 转义降级为字面反斜杠");
    }

    // 注：修复层"关闭时原样透传"的行为不单独用 static 开关测——进程级 static 在并行测试下会互相
    // 污染（一个测试 set(false) 期间别的 ON 前提测试恰好在跑就会假失败）。透传分支的正确性由
    // flush_tool_input 里 `if tool_repair_json_enabled()` 的显式门控保证（关则完全不调 repair），
    // 修复函数本身的正确性由上面的纯函数注入测试独立覆盖，两者组合已充分且无并发风险。

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
        let mut ctx = BufferedStreamContext::new("test-model", 1, false, HashMap::new(), std::collections::HashSet::new());
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

    // ============ JSON 修复层（缓解④，根治向）离线注入测试 ============
    // 数据源：Claude Code 官方 issue 坐实的真实坏帧成因——
    //   #20015 Windows 路径反斜杠（\U 非法转义）、#69522 长 unicode 转义、#29715 裸控制符/smart quote。
    // 契约：repair_tool_json 只在 from_str 已失败时被调用；返回 Some 必为合法 JSON，否则 None。

    /// 铁律①：合法 JSON 绝不进入修复函数——但即便误入，也应原样往返、语义不变（幂等安全网）。
    #[test]
    fn test_repair_noop_on_valid_json() {
        let valid = r#"{"path":"C:/Users/foo","content":"hello world"}"#;
        // repair_tool_json 内部先 char_level 再复验；合法 JSON 的 char_level 不改结构、复验即过。
        let repaired = repair_tool_json(valid).expect("合法 JSON 应能往返");
        let a: serde_json::Value = serde_json::from_str(valid).unwrap();
        let b: serde_json::Value = serde_json::from_str(&repaired).unwrap();
        assert_eq!(a, b, "合法 JSON 修复往返后语义必须完全一致");
    }

    /// #20015：Windows 路径反斜杠 `C:\Users`（模型直接吐字面 `\U`，JSON 非法转义）。
    /// 现状：客户端 JSON.parse 失败 → Invalid tool parameters。修复后合法，客户端不再报错。
    ///
    /// 诚实边界：`\U`、`\d` 是 JSON 非法转义 → 修复层把反斜杠降级成字面 `\\`，值正确还原。
    /// 但 `\t`（如 `\test.txt`）是 JSON **合法**转义（制表符）——修复层**不碰合法转义**（碰了会破坏
    /// 正常场景），故这里用 `program.exe` 这类**不含合法转义字符**的路径，锁死"非法转义被正确还原"。
    #[test]
    fn test_repair_windows_path_backslash() {
        // content 值里 C:\Users\dwgx\program.exe —— \U \d \p 都是 JSON 非法转义（无合法转义歧义）。
        let bad = r#"{"file_path":"C:\Users\dwgx\program.exe"}"#;
        assert!(
            serde_json::from_str::<serde_json::Value>(bad).is_err(),
            "前提：Windows 反斜杠路径确为非法 JSON"
        );
        let fixed = repair_tool_json(bad).expect("Windows 路径反斜杠应可修复");
        let v: serde_json::Value = serde_json::from_str(&fixed).expect("修复后必合法");
        assert_eq!(
            v["file_path"].as_str().unwrap(),
            r"C:\Users\dwgx\program.exe",
            "修复后路径值应还原成字面反斜杠（非法转义 \\U\\d\\p 降级为字面）"
        );
    }

    /// #29715：裸控制符（真实换行/制表符混进字符串值，未转义）→ JSON 非法。修复后转义成 \n/\t。
    #[test]
    fn test_repair_bare_control_chars() {
        // content 值里有真实换行和 tab（裸控制符），JSON 字符串内非法。
        let bad = "{\"content\":\"line1\nline2\tend\"}";
        assert!(
            serde_json::from_str::<serde_json::Value>(bad).is_err(),
            "前提：裸控制符确为非法 JSON"
        );
        let fixed = repair_tool_json(bad).expect("裸控制符应可修复");
        let v: serde_json::Value = serde_json::from_str(&fixed).expect("修复后必合法");
        assert_eq!(
            v["content"].as_str().unwrap(),
            "line1\nline2\tend",
            "修复后控制符应还原为真实换行/制表符（值语义不变）"
        );
    }

    /// 截断输出（流被中途切断，缺收尾 `"` 和 `}`）→ 结构层补全。
    #[test]
    fn test_repair_truncated_structure() {
        let bad = r#"{"path":"a.txt","content":"unfinished"#;
        assert!(
            serde_json::from_str::<serde_json::Value>(bad).is_err(),
            "前提：截断串确为非法 JSON"
        );
        let fixed = repair_tool_json(bad).expect("截断应可补全");
        let v: serde_json::Value = serde_json::from_str(&fixed).expect("补全后必合法");
        assert_eq!(v["path"].as_str().unwrap(), "a.txt");
        assert_eq!(v["content"].as_str().unwrap(), "unfinished");
    }

    /// #69522：截断的 `\u` 转义（`\uD83`——不足 4 位 hex）→ 降级字面，复验兜底。
    #[test]
    fn test_repair_truncated_unicode_escape() {
        let bad = r#"{"q":"emoji \uD83"}"#;
        assert!(
            serde_json::from_str::<serde_json::Value>(bad).is_err(),
            "前提：截断 \\u 转义确为非法 JSON"
        );
        // 能修成合法即达标（降级为字面 \uD83 文本），语义退化可接受、总比客户端整个报错强。
        let fixed = repair_tool_json(bad).expect("截断 unicode 转义应可修成合法");
        assert!(
            serde_json::from_str::<serde_json::Value>(&fixed).is_ok(),
            "修复后必为合法 JSON"
        );
    }

    /// 洞4:合法 UTF-16 代理对(😀 = 😀)必须**原样保留**,不被误降级。
    #[test]
    fn test_repair_keeps_valid_surrogate_pair() {
        // 构造一个整体非法(裸控制符触发 repair)、但含合法代理对的串,验证代理对不被破坏。
        let bad = "{\"emoji\":\"\\uD83D\\uDE00\nx\"}"; // 含真实换行=裸控制符→非法,触发 repair
        assert!(serde_json::from_str::<serde_json::Value>(bad).is_err());
        let fixed = repair_tool_json(bad).expect("应可修复");
        let v: serde_json::Value = serde_json::from_str(&fixed).expect("修复后合法");
        // 合法代理对应解码成 😀,不被降级为字面。
        assert!(v["emoji"].as_str().unwrap().contains('😀'), "合法代理对必须保留为 emoji");
    }

    /// 洞4:孤立高代理(无低代理配对)→ 降级字面,修成合法 JSON。
    #[test]
    fn test_repair_isolated_high_surrogate() {
        let bad = r#"{"x":"\uD83Dnext"}"#; // 高代理后跟普通文本,孤立
        assert!(serde_json::from_str::<serde_json::Value>(bad).is_err(), "前提:孤立高代理非法");
        let fixed = repair_tool_json(bad).expect("孤立高代理应可降级修复");
        assert!(serde_json::from_str::<serde_json::Value>(&fixed).is_ok(), "修复后必合法");
    }

    /// 洞4:孤立低代理 → 降级字面,修成合法 JSON。
    #[test]
    fn test_repair_isolated_low_surrogate() {
        let bad = r#"{"x":"\uDE00abc"}"#;
        assert!(serde_json::from_str::<serde_json::Value>(bad).is_err(), "前提:孤立低代理非法");
        let fixed = repair_tool_json(bad).expect("孤立低代理应可降级修复");
        assert!(serde_json::from_str::<serde_json::Value>(&fixed).is_ok(), "修复后必合法");
    }

    /// 洞1:整包双重编码解包——顶层是被字符串编码的 object → 解一层还原。
    #[test]
    fn test_unwrap_double_encoded_object() {
        // 双重编码:整个 {"path":"a.txt"} 被再套一层字符串编码。
        let double = r#""{\"path\":\"a.txt\"}""#;
        // 顶层 from_str 成功但得到 String(漏过 repair 层)。
        assert!(serde_json::from_str::<serde_json::Value>(double).unwrap().is_string());
        let unwrapped = unwrap_double_encoded(double).expect("应解一层");
        let v: serde_json::Value = serde_json::from_str(&unwrapped).unwrap();
        assert_eq!(v["path"].as_str().unwrap(), "a.txt");
    }

    /// review confirmed 回归(端到端):合法的双重编码串经 flush_tool_input 必须被 unwrap 成 object
    /// 再发出。这锁死"unwrap 在函数尾统一出口执行"——修复前 repair 成功分支 early-return 会绕过它,
    /// 修复后两条路径(原本合法 / 修复后)都经同一 unwrap 出口。此处走"原本合法"路径(from_str 成功
    /// → 跳过 repair → 命中尾部 unwrap),验证出口本身正确;repair 分支的 fall-through 由结构保证
    /// (assembled=repaired 后不再 return,与本路径汇合到同一 unwrap)。
    #[test]
    fn test_flush_unwraps_double_encoded_at_exit() {
        let mut ctx = StreamContext::new_with_thinking("claude-sonnet-4.6", 1, false, HashMap::new());
        let _ = ctx.generate_initial_events();
        // 合法的双重编码:整个 {"path":"a.txt"} 被再套一层字符串编码(from_str 成功但得 String)。
        let double = r#""{\"path\":\"a.txt\"}""#;
        assert!(
            serde_json::from_str::<serde_json::Value>(double).unwrap().is_string(),
            "前提:顶层 from_str 成功但是 String(双重编码,会漏过 repair)"
        );
        let evs = ctx.flush_tool_input(0, double.to_string());
        let delta = evs
            .iter()
            .find(|e| e.data["delta"]["type"] == "input_json_delta")
            .expect("应发出 input_json_delta");
        let partial = delta.data["delta"]["partial_json"].as_str().unwrap();
        let v: serde_json::Value = serde_json::from_str(partial).expect("发出的必是合法 JSON");
        assert!(v.is_object(), "双重编码必须在出口被 unwrap 成 object,实际={}", partial);
        assert_eq!(v["path"].as_str().unwrap(), "a.txt");
    }

    /// 洞1:双重编码 array 也解;标量/普通 object 不动。
    #[test]
    fn test_unwrap_double_encoded_boundaries() {
        // array 双重编码 → 解。
        let arr = r#""[1,2,3]""#;
        assert!(unwrap_double_encoded(arr).is_some());
        // 正常 object(非双重编码)→ 不动(顶层不是 String)。
        assert!(unwrap_double_encoded(r#"{"a":1}"#).is_none());
        // 顶层是字符串但内层是标量(不是 object/array)→ 不动(不臆测)。
        assert!(unwrap_double_encoded(r#""hello""#).is_none());
        assert!(unwrap_double_encoded(r#""42""#).is_none());
    }

    /// 端到端：flush_tool_input 收到非法 JSON（开关默认开）→ 修复成功 → 发出的 partial_json 必合法。
    /// 这是客户端真正消费的字节，锁死"客户端不再报 Invalid tool parameters"。
    #[test]
    fn test_flush_tool_input_repairs_before_sending() {
        let mut ctx = StreamContext::new_with_thinking("claude-sonnet-4.6", 1, false, HashMap::new());
        let _ = ctx.generate_initial_events();
        // Windows 路径非法转义（#20015 真实成因）。
        let bad = r#"{"file_path":"C:\Users\x\a.txt"}"#.to_string();
        let evs = ctx.flush_tool_input(0, bad);
        let delta = evs
            .iter()
            .find(|e| e.data["delta"]["type"] == "input_json_delta")
            .expect("应发出 input_json_delta");
        let partial = delta.data["delta"]["partial_json"].as_str().unwrap();
        assert!(
            serde_json::from_str::<serde_json::Value>(partial).is_ok(),
            "flush 发给客户端的 partial_json 必须是合法 JSON（修复层已介入）：{}",
            partial
        );
    }

    /// 真实上游模式回归（2026-07-13 本地网关 KIRO_TOOL_TRACE 抓包坐实）：Kiro toolUseEvent.input
    /// 是**纯增量碎片**——每帧只带新片段（`{"path": "` → `test.txt"` → `, "con` → …），buf 单调增长、
    /// 全程无 `}{` 粘连，最后一帧拼成完整合法 JSON。含反斜杠 / emoji / 多语言 / 引号亦无碍。
    /// 这是 Invalid tool parameters 类型 C 修复覆盖的主路径，此测试锁死其正确性防回归。
    #[test]
    fn test_merge_real_upstream_incremental_capture() {
        // 逐帧照抄一次真实抓包的碎片序列（含转义反斜杠与 emoji）。
        let frames = [
            r#"{"path": ""#,
            r#"test.txt""#,
            r#", "con"#,
            r#"tent": "H"#,
            "ello World! ",
            r#"🌍\n\nend"#,
            r#""}"#,
        ];
        let mut buf = String::new();
        let mut glued_ever = false;
        for f in frames {
            buf = merge_tool_input(&buf, f);
            if buf.contains("}{") {
                glued_ever = true;
            }
        }
        assert!(!glued_ever, "纯增量拼装全程不应出现 }}{{ 粘连");
        assert_eq!(buf, r#"{"path": "test.txt", "content": "Hello World! 🌍\n\nend"}"#);
        assert!(
            serde_json::from_str::<serde_json::Value>(&buf).is_ok(),
            "纯增量碎片最终应拼成合法 JSON（类型 C 主路径）"
        );
    }

    /// 曾经的"类型 A 只能透传"契约，已被修复层（缓解④，默认开）**升级为根治**：上游模型帧含 JSON
    /// 非法转义（`\x` —— JSON 只认 `\uXXXX`）时，`flush_tool_input` 先修成合法 JSON 再发，客户端能
    /// 正常 parse，不再报 Invalid tool parameters。此测试锁死"端到端非法 → 发出的必是合法 JSON"。
    /// （修复层关闭时的原样透传行为由纯函数契约 + repair_off 专测覆盖。）
    #[test]
    fn test_process_tool_use_type_a_illegal_escape_repaired() {
        use crate::kiro::model::events::ToolUseEvent;
        let mut ctx =
            StreamContext::new_with_thinking("claude-sonnet-4.6", 1, false, HashMap::new());
        let _ = ctx.generate_initial_events();
        // `\x41` 是 JSON 非法转义（合法应为 `A`）——模拟上游模型控制 token 抽风产出的非法串。
        let illegal = r#"{"path":"a.txt","content":"bad\x41escape"}"#;
        assert!(
            serde_json::from_str::<serde_json::Value>(illegal).is_err(),
            "前提：该串确为非法 JSON（\\x 转义）"
        );
        let evs = ctx.process_tool_use(&ToolUseEvent {
            name: "write_file".to_string(),
            tool_use_id: "toolu_typea".to_string(),
            input: illegal.to_string(),
            stop: true,
        });
        let delta = evs
            .iter()
            .find(|e| {
                e.event == "content_block_delta" && e.data["delta"]["type"] == "input_json_delta"
            })
            .expect("应发出 input_json_delta（修复后，不吞）");
        let assembled = delta.data["delta"]["partial_json"].as_str().unwrap();
        assert!(
            serde_json::from_str::<serde_json::Value>(assembled).is_ok(),
            "修复层默认开：发给客户端的必是合法 JSON，实际={}",
            assembled
        );
    }

    /// 泄漏 token 清洗（收严高信号）：行首泄漏词直贴 **CJK/全角** 粘连 → 剥离；正常英文用法（含
    /// ASCII 冒号/数字/大写）→ 绝不误删。court/課/课 独占整行 → 剥（高置信 #70544 泄漏）。
    #[test]
    fn test_strip_leaked_prefix() {
        // 辅助：只取清洗后文本（新签名返回 (String, StripHit)）。
        let s = |line: &str| StreamContext::strip_leaked_prefix(line).0;
        // CJK/全角粘连 → 剥离。
        assert_eq!(s("course重读文件"), "重读文件");
        assert_eq!(s("課我加的是"), "我加的是");
        assert_eq!(s("care：我把"), "：我把"); // 全角冒号
        assert_eq!(s("count你好"), "你好");
        assert_eq!(s("court重读"), "重读"); // 新增 court
        assert_eq!(s("card表格"), "表格"); // 新增 card
        assert_eq!(s("call调用"), "调用"); // 新增 call
        // court/課/课 独占整行 → 剥空(保留换行)。
        assert_eq!(s("court\n"), "\n");
        assert_eq!(s("court"), "");
        assert_eq!(s("課\n"), "\n");
        // 【收严关键】正常英文含 ASCII 冒号/数字/大写 → 绝不误删(旧逻辑会误剥)。
        assert_eq!(s("count: 42"), "count: 42"); // 半角冒号
        assert_eq!(s("countDown()"), "countDown()"); // 大写
        assert_eq!(s("care2share"), "care2share"); // 数字
        assert_eq!(s("courseCatalog"), "courseCatalog"); // 大写
        assert_eq!(s("card#1"), "card#1"); // ASCII 标点
        // 正常英文：词后空格 / 小写延续 → 原样保留。
        assert_eq!(s("count the items"), "count the items");
        assert_eq!(s("counter offer"), "counter offer");
        assert_eq!(s("careful now"), "careful now");
        assert_eq!(s("call me"), "call me");
        // call/card/count/care/course 独占整行 → **不**享特例(可能是正常内容),保守不剥。
        assert_eq!(s("count"), "count");
        assert_eq!(s("card"), "card");
        assert_eq!(s("call"), "call");
        // 非泄漏词开头 → 原样。
        assert_eq!(s("hello世界"), "hello世界");
    }

    /// 诊断计数：strip 命中信息（stripped / standalone）正确——供收尾泄漏诊断计数。
    #[test]
    fn test_strip_leaked_prefix_hit_flags() {
        let (_, hit) = StreamContext::strip_leaked_prefix("court\n"); // 独占整行
        assert!(hit.stripped && hit.standalone, "独占整行 court 应 stripped+standalone");
        let (_, hit) = StreamContext::strip_leaked_prefix("course重读"); // 粘连非独占
        assert!(hit.stripped && !hit.standalone, "粘连剥离应 stripped 但非 standalone");
        let (_, hit) = StreamContext::strip_leaked_prefix("count: 42"); // 正常英文不剥
        assert!(!hit.stripped && !hit.standalone, "正常英文不应命中");
    }

    /// clean_leaked_tokens：只在行首处理，多行时每行行首各判一次；并累加诊断计数。
    #[test]
    fn test_clean_leaked_tokens_multiline() {
        let mut ctx = StreamContext::new_with_thinking("claude-sonnet-4.6", 1, false, HashMap::new());
        let input = "course重读\nnormal line\ncount你好";
        assert_eq!(ctx.clean_leaked_tokens(input), "重读\nnormal line\n你好");
        assert_eq!(ctx.leaked_stripped, 2, "剥了 course / count 两个");
        assert_eq!(ctx.leaked_saturation_lines, 0, "无独占整行泄漏");
    }

    /// saturation 计数：满屏纯 court 独占行 → 每行计入 saturation。
    #[test]
    fn test_clean_leaked_tokens_saturation_count() {
        let mut ctx = StreamContext::new_with_thinking("claude-opus-4.8", 1, false, HashMap::new());
        let input = "court\ncourt\ncourt\n";
        ctx.clean_leaked_tokens(input);
        assert_eq!(ctx.leaked_saturation_lines, 3, "3 行纯 court 独占行=saturation 信号");
        assert_eq!(ctx.leaked_stripped, 3);
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

    // ============ 截断诊断归因标签（短板 2.5，纯可观测）离线测试 ============
    // classify_tool_json_defect 只在 from_str 已失败分支被调、只写日志，绝不进控制流。
    // 判据与 repair 层同源：truncated=结构未闭合/串未终结、illegal_chars=非法转义或裸控制符。

    /// 截断（缺收尾 `"` 和 `}`）→ 归因 Truncated。
    #[test]
    fn test_classify_defect_truncated() {
        let s = r#"{"path":"a.txt","content":"unfinished"#;
        assert_eq!(classify_tool_json_defect(s), ToolJsonDefect::Truncated);
    }

    /// 非法转义（`\x`）+ 结构完整闭合 → 归因 IllegalChars。
    #[test]
    fn test_classify_defect_illegal_chars() {
        let s = r#"{"path":"bad\x41escape"}"#;
        assert_eq!(classify_tool_json_defect(s), ToolJsonDefect::IllegalChars);
    }

    /// 裸控制符（真实换行未转义）+ 结构完整 → 归因 IllegalChars。
    #[test]
    fn test_classify_defect_bare_control() {
        let s = "{\"content\":\"line1\nline2\"}";
        assert_eq!(classify_tool_json_defect(s), ToolJsonDefect::IllegalChars);
    }

    /// 既含非法转义又结构截断 → 归因 TruncatedAndIllegal。
    #[test]
    fn test_classify_defect_truncated_and_illegal() {
        let s = r#"{"path":"C:\Users\x"#;
        assert_eq!(
            classify_tool_json_defect(s),
            ToolJsonDefect::TruncatedAndIllegal
        );
    }

    /// 结构闭合、字符合法，但 `}{` 粘连（非前缀双对象）→ 归因 Malformed。
    #[test]
    fn test_classify_defect_malformed_glued() {
        let s = r#"{"a":1}{"b":2}"#;
        let scan = scan_tool_json(s);
        assert!(scan.glued, "应识别出 }}{{ 粘连");
        assert_eq!(classify_tool_json_defect(s), ToolJsonDefect::Malformed);
    }

    /// 短板2：截断跨轮恢复纯决策——恢复开关开 + 修复层开 + 归因真截断,三条件全满足才触发。
    #[test]
    fn test_contains_textified_tool_call_detector() {
        // 文本化 invoke 标记(不论是否带 antml: 前缀)应命中。
        assert!(contains_textified_tool_call(r#"<invoke name="Bash">"#));
        assert!(contains_textified_tool_call(r#"<invoke name="Bash">"#));
        assert!(contains_textified_tool_call("</invoke>"));
        assert!(contains_textified_tool_call(r#"<parameter name="command">"#));
        assert!(contains_textified_tool_call("</function_calls>"));
        // 正常文本不误命中。
        assert!(!contains_textified_tool_call("这是一段正常的助手回复,讲 invoke 概念但无标签"));
        assert!(!contains_textified_tool_call("函数调用 function calls 讨论"));
        assert!(!contains_textified_tool_call(""));
    }

    #[test]
    fn test_should_recover_truncation_decision() {
        use ToolJsonDefect::*;
        // 恢复开关关 → 任何情况都不触发（默认行为不变）。
        assert!(!should_recover_truncation(Truncated, false, true));
        assert!(!should_recover_truncation(TruncatedAndIllegal, false, true));
        // 修复层关 → 不触发（无法断言"修复也补不回",退回②/③原语义）。
        assert!(!should_recover_truncation(Truncated, true, false));
        assert!(!should_recover_truncation(TruncatedAndIllegal, true, false));
        // 恢复开 + 修复开 + 真截断 → 触发。
        assert!(should_recover_truncation(Truncated, true, true));
        assert!(should_recover_truncation(TruncatedAndIllegal, true, true));
        // 恢复开 + 修复开但非截断畸形 → 不归本开关管（仍走②/③）。
        assert!(!should_recover_truncation(IllegalChars, true, true));
        assert!(!should_recover_truncation(Malformed, true, true));
    }

    /// 稳定短标签：日志字段值不随重构漂移。
    #[test]
    fn test_defect_as_str_labels() {
        assert_eq!(ToolJsonDefect::Truncated.as_str(), "truncated");
        assert_eq!(ToolJsonDefect::IllegalChars.as_str(), "illegal_chars");
        assert_eq!(
            ToolJsonDefect::TruncatedAndIllegal.as_str(),
            "truncated_and_illegal"
        );
        assert_eq!(ToolJsonDefect::Malformed.as_str(), "malformed");
    }
}
