//! 流式响应处理模块
//!
//! 实现 Kiro → Anthropic 流式响应转换和 SSE 状态管理

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};

use serde_json::json;
use uuid::Uuid;

use crate::kiro::model::events::{Event, ReasoningContentEvent};
use crate::usage::RequestOutcome;

#[path = "thinking_tags.rs"]
mod thinking_tags;
#[path = "dsml_leak.rs"]
mod dsml_leak;
#[path = "invoke_xml.rs"]
mod invoke_xml;

pub(crate) use thinking_tags::*;
pub(crate) use dsml_leak::*;
pub(crate) use invoke_xml::*;

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
    /// 传输层干净结束，但缺少终止信号（无 metadata stopReason、无 tool_use stop）。
    /// 部分正文已下发时不得再标成功 `end_turn`。
    Incomplete { message: String },
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
            CompletionStatus::Incomplete { .. } => RequestOutcome::ServerError,
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
            CompletionStatus::Incomplete { .. } => "api_error",
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
    ///
    /// ⚠️ **流内形态硬编码，不可配置（M5 决策，2026-08-15 对抗审查）**：
    /// D11-D14 的 in-band 错误是动态模板（`上游返回错误: {code} - {message}`、
    /// 按限流信号二选一的 type、429/502 二选一的 status），接配置需把本方法签名
    /// 从 `&'static str` 系改 `String` 并给流状态机（create_sse_stream 的 unfold
    /// 闭包 / emit_stream_usage / 非流式收尾）传配置快照，波及 >5 处且与
    /// `is_rate_limit_signal` 语义纠缠。故错误消息配置表**不设 stream_inband_* key**
    /// （model/error_messages.rs 模块头注释已记录）；HTTP 层错误（A/B/D/E/F 表）
    /// 可配，默认文案与流内保持一致（保 H12 契约：非流式完成态 429/502 + 同文案）。
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
            CompletionStatus::Incomplete { message } => {
                format!("上游响应不完整: {}", message)
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

/// Prompt 缓存记账明细（影子估算：凭 continuationId 前缀估算 Bedrock prefix cache 命中量）。
///
/// Bedrock prefix cache 是不透明的——上游不返回 cache_read_input_tokens。
/// 通过在本地估算 [系统提示 + 历史轮次] 的 token 数来填充此字段，让 Claude Code
/// 客户端的 "cache hits" 指标正常显示。
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

/// 返回给客户端的 token 类字段缩放系数。
///
/// 仅影响给客户端（如 Claude Code）看到的 usage.input_tokens / cache_* 字段，
/// 内部计费与 usage_tracker 入库仍写入真实值。`output_tokens` 刻意不缩放，
/// 避免影响客户端基于它的 max_tokens 计算。
///
/// Claude Code 4.6 窗口 200K，85% 触发 compact = 170K（原假设 83%，按实测更新）。
/// 缩放系数按比例上调以保持原真实触发点不变：0.65 × (85/83) ≈ 0.6657。
/// 真实 255K+ × 0.6657 ≈ 170K+ → 触发 compact。
const CLIENT_TOKEN_DISPLAY_SCALE: f64 = 0.6657;

/// SSE 头 `x-kirostudio-input-token-scale` 的值。与上一行同一十进制字面量，
/// 禁止 `format!("{}", f64)`（二进制浮点会写成 `0.6656999…`）。
pub(crate) const CLIENT_TOKEN_DISPLAY_SCALE_HEADER: &str = "0.6657";

/// 对客户端展示用的 token 值缩放（向上取整保证非零）。
pub(crate) fn scale_for_client(n: i32) -> i32 {
    if n <= 0 {
        return n.max(0);
    }
    ((n as f64) * CLIENT_TOKEN_DISPLAY_SCALE).ceil() as i32
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

/// SSE 事件
#[derive(Debug, Clone)]
pub struct SseEvent {
    pub event: String,
    pub data: serde_json::Value,
}

/// 判定一个 SSE 事件是否是「首 token」候选 —— 即**真实模型输出**的第一片。
///
/// 只认三种 delta：
/// - `text_delta`（正文）、`thinking_delta`（思考）：必须**非空**。空 delta 是关块占位
///   （见 `generate_final_events` 里补空 thinking_delta 的那几处），不代表有内容产出。
/// - `input_json_delta`（工具参数）：工具调用本身就是输出，无需判空。
///
/// 明确**不算**首 token 的：`message_start` / `ping` / `content_block_start`
/// （都不含模型输出，且 `message_start` 在流开始前就发了）、`signature_delta`
/// （thinking 块的签名收尾，不是内容）。
///
/// 判空是刻意的保险：`create_text_delta_events` 自身无空串守卫且有 8 处调用方，
/// 未逐一验证是否都传非空 —— 若某处传空，这里挡住，不会把空 delta 误当首 token。
fn is_first_content_delta(e: &SseEvent) -> bool {
    if e.event != "content_block_delta" {
        return false;
    }
    let d = &e.data["delta"];
    match d["type"].as_str() {
        Some("text_delta") => !d["text"].as_str().unwrap_or("").is_empty(),
        Some("thinking_delta") => !d["thinking"].as_str().unwrap_or("").is_empty(),
        Some("input_json_delta") => true,
        _ => false,
    }
}

/// 该事件是否为**用户可见正文**的 delta（text / tool_use 的 input，**不含** thinking）。
///
/// 与 [`is_first_content_delta`] 只差 `thinking_delta` 一项，但**不能复用它**：那个判据服务
/// TTFB 打点（thinking 也算"上游开始出货"），而这里服务「本轮到底有没有给用户看的东西」——
/// 空响应兜底的判据里若把 thinking 算进正文，`!thinking_enabled` 下恰好永远为假、
/// `thinking_enabled` 下又会把纯思考轮当成有正文，两头都错。
fn is_visible_body_delta(e: &SseEvent) -> bool {
    if e.event != "content_block_delta" {
        return false;
    }
    let d = &e.data["delta"];
    match d["type"].as_str() {
        Some("text_delta") => !d["text"].as_str().unwrap_or("").is_empty(),
        Some("input_json_delta") => true,
        _ => false,
    }
}

impl SseEvent {
    pub fn new(event: impl Into<String>, data: serde_json::Value) -> Self {
        Self {
            event: event.into(),
            data,
        }
    }

    /// 格式化为 SSE 字符串
    ///
    /// 一次分配直接拼好：with_capacity 预分配 + serde_json 直接序列化进缓冲，
    /// 避免旧实现（`to_string` 产生临时 String，`format!` 再拷贝一次）的二次复制——
    /// 流式路径逐事件走这里，长流下每帧省两次分配。
    pub fn to_sse_string(&self) -> String {
        use std::fmt::Write as _;
        let mut s = String::with_capacity(self.event.len() + 128);
        let _ = write!(s, "event: {}\n", self.event);
        s.push_str("data: ");
        let data_start = s.len();
        // serde_json::to_writer 需要 io::Write —— String 不实现它，用 Vec<u8> 中转
        // （一次性分配，写完转 String 零拷贝语义由 String::from_utf8 保证）。
        let mut buf = Vec::with_capacity(128);
        if serde_json::to_writer(&mut buf, &self.data).is_err() {
            // 理论不可达（Value 恒可序列化，Vec 写入不产生 IO 错误）；
            // 与旧 `unwrap_or_default` 同语义：退化 data 为空。
            s.truncate(data_start);
        } else {
            // Vec<u8> 无非法 UTF-8（serde_json 输出恒 UTF-8），失败仅理论不可达。
            s.push_str(&String::from_utf8(buf).unwrap_or_default());
        }
        s.push_str("\n\n");
        s
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
    /// 活跃的内容块状态（BTreeMap：多块同开时 stop 顺序按 index 稳定）
    active_blocks: BTreeMap<i32, BlockState>,
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
            active_blocks: BTreeMap::new(),
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

        events.extend(self.close_open_blocks());

        // 发送 message_delta
        if !self.message_delta_sent {
            self.message_delta_sent = true;
            let mut usage_json = json!({
                // 客户端展示缩放（output_tokens 不缩放，避免影响 max_tokens 计算）
                "input_tokens": scale_for_client(input_tokens),
                "output_tokens": output_tokens
            });
            if let Some(cache_usage) = cache_usage {
                usage_json["cache_creation_input_tokens"] =
                    json!(scale_for_client(cache_usage.cache_creation_input_tokens));
                usage_json["cache_read_input_tokens"] =
                    json!(scale_for_client(cache_usage.cache_read_input_tokens));
                usage_json["cache_creation"] = json!({
                    "ephemeral_5m_input_tokens": scale_for_client(cache_usage.cache_creation_5m_input_tokens),
                    "ephemeral_1h_input_tokens": scale_for_client(cache_usage.cache_creation_1h_input_tokens)
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

    /// 关闭仍打开的内容块。干净 EOF 不完整时只关块、不发成功 `message_delta`。
    /// 按 block index 升序补发 stop（`active_blocks` 是 BTreeMap）。
    fn close_open_blocks(&mut self) -> Vec<SseEvent> {
        let mut events = Vec::new();
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
        events
    }
}

use super::converter::get_context_window_size;

/// 空响应判定为「上下文过大」的输入 token 阈值（取窗口的 28%）。
///
/// 实测 input≈297K 时上游已频繁返回空/极短响应（4~13 tokens 无工具调用），
/// 取窗口的 28%（≈280K for 1M 窗口）作为判定阈值。
///
/// `pub(crate)`：非流式收尾（handlers.rs）的空响应兜底复用同一阈值，保证
/// 两条路径对「大输入 vs 偶发」的分界一致（口径分叉会产出两套空响应文案）。
pub(crate) fn empty_response_oversized_threshold(model: &str) -> i32 {
    (get_context_window_size(model) as f64 * 0.28) as i32
}

/// "近似空响应"的 output token 阈值。
///
/// 当上下文压力大时，模型可能返回极短的无意义文本（如 4~13 tokens）而非工具调用，
/// 导致客户端 agentic 循环卡住。output < 此阈值且无工具调用时，视为近似空响应。
const NEAR_EMPTY_OUTPUT_THRESHOLD: i32 = 30;

/// 流式与非流式共用的「空/近空响应」判据。
///
/// 判定语义与 [`StreamContext::is_empty_response`] 文档一致（完全空 / 近似空 + 上下文过大）。
/// **两条路径必须调用同一个函数**，不能各自实现一份 —— 本仓已多次踩「同一判据两份实现、
/// 只修了其中一份」的形态，分界一旦漂移就是「流式判空、非流式放行 → 200 空 body」。
pub(crate) fn near_empty_response(
    output_tokens: i32,
    has_tool_use: bool,
    input_tokens: i32,
    model: &str,
) -> bool {
    let no_tool_use = !has_tool_use;
    // 路径 1：完全空。
    if output_tokens == 0 && no_tool_use {
        return true;
    }
    // 路径 2：近似空 + 上下文过大（input_tokens 超过窗口 28% 阈值）。
    if output_tokens > 0
        && output_tokens < NEAR_EMPTY_OUTPUT_THRESHOLD
        && no_tool_use
        && input_tokens >= empty_response_oversized_threshold(model)
    {
        return true;
    }
    false
}

/// 流处理上下文
/// 一次请求解析出的最终用量快照（供用量统计埋点消费）
#[derive(Debug, Clone, Copy)]
pub struct ResolvedUsage {
    /// 输入 tokens —— **gross 口径**（含 cache 读写，优先 `contextUsageEvent` 精确值，回退估算）。
    ///
    /// ⚠ 与发给客户端的 `usage.input_tokens` 口径相反：响应体那个是 billed 口径
    /// （已经 [`billed_input_tokens`] 剔除过 cache 读写，与 `cache_read_input_tokens` 互斥），
    /// 本字段是**未剔除的全量**，落进 [`crate::usage::RequestRecord::input_tokens`] 也是 gross。
    /// 消费方算「总输入」直接用本字段，**不可再加 `cache_read_tokens`**（会把缓存计两次）。
    pub input_tokens: i32,
    /// 输出 tokens
    pub output_tokens: i32,
    /// 上游返回的真实 credit 消耗量（无 meteringEvent 时为 None）
    pub credits_used: Option<f64>,
    /// 本次命中缓存读取的 tokens（无缓存记账时为 0）。是 `input_tokens` 的子集，非增量。
    pub cache_read_tokens: i32,
    /// 本次新建缓存写入的 tokens（无缓存记账时为 0）。是 `input_tokens` 的子集，非增量。
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
    /// meteringEvent 携带的 cache_read 真值（Layer 1，缺失为 None）。
    /// 仅用于**入库**（`resolved_usage`）覆盖本地 prefix 估算；client 下发的 message_start
    /// 在流开始前就已用估算值发出，无法回溯改写。
    pub metering_cache_read: Option<i32>,
    /// meteringEvent 携带的 cache_creation 真值（Layer 1，缺失为 None）。
    pub metering_cache_creation: Option<i32>,
    /// 工具块索引映射 (tool_id -> block_index)
    pub tool_block_indices: HashMap<String, i32>,
    /// 出站工具名映射 (tool_use_id -> 还原后的 Claude Code 名)。
    ///
    /// 供 `generate_final_events` 的截断兜底用：残留 `tool_input_sent`（流在 stop 前结束）
    /// 需要把 Kiro 参数还原成客户端形态，而兜底路径没有 `ToolUseEvent` 只有 id —— 必须从
    /// 这里查名字。正常 stop 路径直接拿 `tool_use.name`，不依赖本 map。
    tool_use_names: HashMap<String, String>,
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
    /// 上游 `reasoningContentEvent` 携带的思考签名（若有）。
    ///
    /// 关闭 thinking 块时优先回传它 —— Foxfishc 实测（Round 6, 2026-05-13）：
    /// 「伪造签名不被上游识别，cache_read 仍 0」；真签名是多轮 cache 命中的关键。
    /// 若上游不发（`None`），`create_signature_delta_event` 回退占位符，行为与改动前逐字节一致。
    pub pending_reasoning_signature: Option<String>,
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
    // ===== tool_use XML 泄漏过滤(参考仓 ref-grey 双层防护的流层)=====
    /// tool_use XML 泄漏过滤的跨 chunk 缓冲:上游把工具调用当**正文文本**吐进
    /// `assistantResponseEvent`(非结构化 toolUseEvent)时,`<tool_use …></tool_use>` 会被客户端
    /// 当普通文本渲染。本缓冲保留"可能是半个 `<tool_use` 开标签"的文本尾巴,
    /// 等下一 chunk 拼上再判定是完整标签(剥掉)还是普通正文(吐掉),避免标签被上游分帧切
    /// 成两半时漏剥或误剥。
    tool_use_xml_buffer: String,
    /// 当前是否正处在一个已确认的 `<tool_use …` 开标签内(等待 `</tool_use>` 闭合)。
    /// 为 true 时其后所有文本都被剥掉,直到找到闭合标签。
    tool_use_xml_stripping: bool,
    /// 🔴 本轮响应已触发过 256 KiB 上限 ⇒ 永久放弃剥离（latch，不再重入）。
    /// 没有它会死循环：清空 carry 后下一 chunk 又命中 `<tool_use` 重新进剥离态。
    tool_use_xml_strip_disabled: bool,
    /// 本请求真剥掉的 tool_use XML 泄漏字节数(可观测;纯统计,不改剥离判据)。
    tool_use_xml_stripped: u64,
    /// 连续剥离态累积剥离字节数(上限兜底:永不闭合的开标签不能无限吞正文)。
    tool_use_xml_strip_run: usize,
    // ===== 文本化 invoke 重组(R4,移植 ZyphrZero__kiro.rs v0.6.5)=====
    /// 本次请求声明的工具名集合(=模型看到的名字)。重组硬护栏:解析出的工具名必须在此才允许捞回,
    /// 否则当文本吐——宁可漏捞不可把正文讨论的假命令误执行。
    known_tool_names: std::collections::HashSet<String>,
    /// `block_index` → **发给模型的工具名**（含缩短后的短名）。
    ///
    /// Bug C 校验需要在 `fail_bug_c_if_missing(block_index, ..)` 里知道「这个块是哪个工具」，
    /// 而那里只有 block_index。既有的 `tool_use_names` 不适用：它的 key 是 tool_use_id、
    /// 值是**还原后**的客户端名，且只在工具名被缩短时才记录（未缩短的不入表）。
    /// 本表无条件记录、口径与 [`Self::tool_required_fields`] 一致，故单独一张。
    tool_block_names: HashMap<i32, String>,
    /// 每个工具的**必需参数名**（来自客户端请求里 `tools[].input_schema.required`）。
    ///
    /// 用于 **Bug C** 校验：`tool_use` 的参数 JSON **完全合法但缺必需字段**
    /// （典型：`Bash` 只给了 `description` 却没有 `command`）。这一类既不是 Bug A
    /// （JSON 语法坏，`tool_repair_json` 能修）也不是 Bug B（连 tool_use 块都没吐，
    /// 网关碰不到），此前一直落在两者之间的盲区 —— 客户端拿到合法 JSON 后按 schema
    /// 校验失败，报 `The required parameter 'X' is missing`。
    ///
    /// 网关**手里就有 schema**（客户端请求里带的 `tools[].input_schema`），此前它只被
    /// 用来数 token（`token.rs`）与 OpenAI 层归一化，从未用于校验模型吐出的参数。
    ///
    /// 空表 = 不校验（未设置 / 无工具 / WebSearch 类工具无 `input_schema`）。
    /// key 用**模型看到的名字**（即可能被 `map_tool_name` 缩短过的短名），与
    /// `known_tool_names` 同口径，这样校验时无需再做名字还原。
    tool_required_fields: HashMap<String, Vec<String>>,
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
    /// 首个**真实内容** delta 落定的时刻（TTFB 打点）。
    ///
    /// `None` = 本轮从未产生内容（纯错误 / 空响应）→ `first_token_ms` 落库 NULL，这是正确的。
    /// 只认 text_delta / thinking_delta / input_json_delta；`message_start` / `ping` /
    /// `content_block_start` / `signature_delta` / 关块用的空 delta 都不算首 token。
    first_token_at: Option<std::time::Instant>,
    /// 从上游累计收到的传输字节（逐 chunk 经 [`Self::note_received_bytes`] 累加）。
    ///
    /// 与「中断字节数」配套：断流（传输错误/解码器停止/in-band 错误）收尾时，
    /// [`Self::interrupted_bytes`] 用本计数器回报「断流时点已收到多少」。
    /// 正常结束该字段不被消费（`interrupted_bytes` 返回 None）。
    received_bytes: u64,
    /// 本轮是否出现过**结构化** reasoning 流（`reasoningContentEvent`）。
    ///
    /// # 为什么需要这个标志（E1 的关键约束）
    ///
    /// 结构化 reasoning 是**纯增量且没有终止帧** —— 上游不会告诉我们"思考结束了"。
    /// 真实形态是：N 帧 `reasoningContentEvent`，紧接着 `assistantResponseEvent` 携带普通正文。
    ///
    /// 而文本嗅探路径的分支是 `else if self.in_thinking_block`：一旦结构化流开过 thinking 块，
    /// 后续**不带任何标签**的正文就会落进那个分支，被当作思考内容发成 `thinking_delta`
    /// → **用户可见的答案整段消失进思考面板**，且 `has_non_thinking_blocks()` 为 false 会让
    /// 收尾把 stop_reason 置成 max_tokens、只吐一个空格文本块 = 客户端显示空答案。
    ///
    /// 所以两条路径必须**互斥**而不是共享状态：本标志置位后，首个非空正文 delta
    /// 先关掉 reasoning 开的 thinking 块，再按普通文本走 —— 见 `process_assistant_response`。
    reasoning_stream_seen: bool,
    /// 本轮是否已向客户端发出过**用户可见正文**（非空 text_delta / tool_use 的 input_json_delta）。
    ///
    /// 只用于「空响应兜底」的判据，刻意**不复用** `first_token_at`：后者把 `thinking_delta`
    /// 也算内容（TTFB 口径），拿它当"有正文"会让兜底永不触发。
    /// 判定点在 `process_kiro_event`（流式常态）与 `generate_final_events`（整轮被 hold 到
    /// 收尾才 flush 的形态）两处，缺任一处都会漏判。
    body_content_seen: bool,
    /// 收到过带非空 stopReason 的 metadata 帧（终止信号；client 侧 reason 仍可能是 tool_use）。
    saw_upstream_stop: bool,
    /// 收到过 `toolUseEvent.stop=true`（或 XML 重组出的完整 tool_use）。未 stop 的残留不算。
    saw_tool_stop: bool,
    /// `!thinking_enabled` 时被丢弃的结构化 reasoning 原文（截断累积，上限
    /// [`Self::MAX_DISCARDED_REASONING_BYTES`]）。
    ///
    /// 存它**不是**为了下发给用户：正常情况（有正文）它到收尾就随 ctx 一起丢掉。
    /// 唯一用途是兜底「本轮只有 reasoning、正文为空」这一形态 —— 那时客户端会拿到
    /// **完全空的响应**，而空响应比"看到一段推理"更糟（Claude Code 侧表现为无输出且不重试）。
    discarded_reasoning: String,
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
        StripHit {
            stripped: false,
            standalone: false,
        }
    }
}

impl StreamContext {
    /// invoke 嗅探缓冲 hold 上限(256 KiB):行首未闭合的 <invoke 累计超此值仍没等到 </invoke>,
    /// 放弃 hold 当普通文本吐,避免永不闭合的半块把流卡死。多行参数(apply_patch)是常态,故按字节非行数。
    const MAX_INVOKE_HOLD_BYTES: usize = 262_144;

    /// `!thinking_enabled` 时丢弃的 reasoning 最多留多少字节（32 KiB）作空响应兜底素材。
    ///
    /// 有上限是必须的：这条路径**每帧都在累积一份注定不下发的文本**，无界即等于给
    /// 「上游只吐 reasoning 不吐正文」的退化响应开了一个按响应长度增长的内存放大器
    /// （同一形态的教训见已知问题 #14：`invoke_sniff_buffer` 无界持有把整条流卡死）。
    /// 32 KiB 足够承载一段可读的推理；超出部分丢弃 —— 兜底的目标是"别给空响应"，
    /// 不是"完整复现推理过程"。
    const MAX_DISCARDED_REASONING_BYTES: usize = 32 * 1024;

    /// 创建启用thinking的StreamContext
    pub fn new_with_thinking(
        model: impl Into<String>,
        input_tokens: i32,
        thinking_enabled: bool,
        tool_name_map: HashMap<String, String>,
    ) -> Self {
        Self::new_full(
            model,
            input_tokens,
            thinking_enabled,
            tool_name_map,
            std::collections::HashSet::new(),
        )
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
            metering_cache_read: None,
            metering_cache_creation: None,
            tool_block_indices: HashMap::new(),
            tool_use_names: HashMap::new(),
            tool_input_sent: HashMap::new(),
            tool_name_map,
            thinking_enabled,
            thinking_buffer: String::new(),
            in_thinking_block: false,
            thinking_extracted: false,
            thinking_block_index: None,
            pending_reasoning_signature: None,
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
            tool_use_xml_buffer: String::new(),
            tool_use_xml_stripping: false,
            tool_use_xml_strip_disabled: false,
            tool_use_xml_stripped: 0,
            tool_use_xml_strip_run: 0,
            known_tool_names,
            tool_block_names: HashMap::new(),
            // 默认空 = 不做 Bug C 校验；由 handler 在构造后经 `set_tool_required_fields` 注入
            // （与 `set_cache_usage` 同款：避免改动 13 个构造点的签名）。
            tool_required_fields: HashMap::new(),
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
            first_token_at: None,
            received_bytes: 0,
            reasoning_stream_seen: false,
            body_content_seen: false,
            saw_upstream_stop: false,
            saw_tool_stop: false,
            discarded_reasoning: String::new(),
        }
    }

    /// 设置 prompt 缓存记账明细（前缀估算注入；在 generate_initial_events 之前调用）
    pub fn set_cache_usage(&mut self, cache_usage: Option<CacheUsageBreakdown>) {
        self.cache_usage = cache_usage;
    }

    /// 注入每个工具的必需参数名，启用 **Bug C**（参数合法但缺必需字段）校验。
    ///
    /// 与 [`Self::set_cache_usage`] 同款「构造后注入」范式：这样不必改动 13 个
    /// `StreamContext::new*` 调用点的签名（其中 11 个是测试 fixture）。
    /// 传空表 = 不校验（与不调用本方法等价）。
    pub fn set_tool_required_fields(&mut self, required: HashMap<String, Vec<String>>) {
        self.tool_required_fields = required;
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
        // ⚠️ message_start 保持 billed **未缩放**：`resolved_usage()` 与
        // `record.billed_input_tokens()` 都绑定此值（`should_keep_gross_input_as_superset_*`
        // 测试钉死），缩放到会破坏「record 口径 = message_start 口径」的一致性。
        // message_delta 的 scale_for_client 是另一处既有展示缩放，两者故意不同步。
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

    /// 处理 Kiro 事件并转换为 Anthropic SSE 事件。
    ///
    /// 本函数是**唯一 choke point**：流式（handlers 的 SSE 循环）与 buffered
    /// （`BufferedStreamContext::process_and_buffer`）都走它，故 TTFB 打点放这里
    /// 一处即可覆盖两条路径，而不必在 4 个 delta 构造点分别插桩（易漏）。
    pub fn process_kiro_event(&mut self, event: &Event) -> Vec<SseEvent> {
        let events = self.process_kiro_event_inner(event);
        self.mark_first_token_if_content(&events);
        self.observe_visible_body(&events);
        events
    }

    /// 若本批事件含首个**真实内容** delta，则打点（幂等：只记第一次）。
    fn mark_first_token_if_content(&mut self, events: &[SseEvent]) {
        if self.first_token_at.is_some() {
            return;
        }
        if events.iter().any(is_first_content_delta) {
            self.first_token_at = Some(std::time::Instant::now());
        }
    }

    /// 记录「本轮已发出用户可见正文」（供空响应兜底判据用，单向置位）。
    fn observe_visible_body(&mut self, events: &[SseEvent]) {
        if self.body_content_seen {
            return;
        }
        // tool_use 也算"本轮有产出"（客户端会拿它去执行工具，不是空响应）。
        // 单看 delta 不够：入参为空的 tool_use 可能一个 input_json_delta 都不发。
        if self.state_manager.has_tool_use || events.iter().any(is_visible_body_delta) {
            self.body_content_seen = true;
        }
    }

    /// 首个真实内容 delta 落定的时刻（供 handler 算 `first_token_ms`）。
    pub fn first_token_at(&self) -> Option<std::time::Instant> {
        self.first_token_at
    }

    /// 累计一批已从上游收到的传输字节（handler 每拿到一个响应 chunk 调用一次）。
    pub fn note_received_bytes(&mut self, n: usize) {
        self.received_bytes += n as u64;
    }

    /// 断流时已收到的上游字节数；正常结束返回 `None`（未中断）。
    ///
    /// `Some(0)` = 断了但一个字节都没收到（首帧前断流）。判定依据是 completion
    /// 状态：传输错误 / 解码器停止 / in-band 错误均视为中断。
    pub fn interrupted_bytes(&self) -> Option<u64> {
        if self.completion.is_ok() {
            None
        } else {
            Some(self.received_bytes)
        }
    }

    fn process_kiro_event_inner(&mut self, event: &Event) -> Vec<SseEvent> {
        match event {
            Event::AssistantResponse(resp) => self.process_assistant_response(&resp.content),
            Event::ToolUse(tool_use) => self.process_tool_use(tool_use),
            // ⭐ E1：上游的**结构化** thinking 增量流。此前落 EventType::Unknown 被丢弃，
            // 我们转而从正文里嗅探 `<thinking>` 标签把边界猜回来（见 process_thinking_content）。
            // 现在直接用上游给的边界，不再猜。
            Event::ReasoningContent(reasoning) => self.process_reasoning_content(&reasoning),
            Event::ContextUsage(context_usage) => {
                // 从上下文使用百分比计算实际的 input_tokens
                let window_size = get_context_window_size(&self.model);
                let pct = context_usage.context_usage_percentage;

                // ⭐ 下界守卫：只有 `pct > 0 且有限` 才是**可用信号**，否则**不覆盖**已有值。
                //
                // 为什么必须有下界（原代码只判了 `>= 100.0` 上界）：
                // `ContextUsageEvent.context_usage_percentage` 带 `#[serde(default)]`，
                // 上游少发该字段 / 发 null / 发脏值时它一律是 **0.0**，而 0.0 会算出
                // `actual_input_tokens = 0` 并被无条件写进 `context_input_tokens`。
                // 那个字段是**最终计费口径的源头**：`generate_final_events` 与
                // `finish_and_get_all_events` 都走 `context_input_tokens.unwrap_or(本地估算)`，
                // 再由 `billed_input_tokens` 扣掉 cache 读写 —— 于是 input_tokens 与
                // cache 相关字段一起归零，客户端看到「这轮没吃 token」的假账。
                //
                // 为什么「上游明确说 0%」也归到脏值一侧：真实请求恒有 system prompt +
                // 至少一条 user message，占用率**不可能**为 0；且 serde 默认值与
                // 显式 0 在这里逐字节不可分辨，无法只放过其中一个。两者都当「没给」处理
                // 即可 —— 代价对称性明显：错信 0 = 计费信号错且不可逆（账已出），
                // 而丢一次真 0 只是退回本地估算（`unwrap_or` 的既有兜底），可逆。
                //
                // 负值与 NaN/inf 同理必须拦：负值会让 `billed_input_tokens` 拿到负数，
                // NaN/inf 经 `as i32` 是饱和转换（NaN→0、inf→i32::MAX），后者更糟。
                if let Some(actual_input_tokens) = context_input_tokens_from_pct(pct, window_size) {
                    self.context_input_tokens = Some(actual_input_tokens);
                    tracing::debug!(
                        "收到 contextUsageEvent: {}%, 计算 input_tokens: {}",
                        pct,
                        actual_input_tokens
                    );
                } else {
                    // 不覆盖 `context_input_tokens`：保留上一次的有效值，或让下游
                    // `unwrap_or` 退回本地估算。warn 而非 debug —— 这代表上游协议异常。
                    tracing::warn!(
                        "收到无效 contextUsageEvent（{}%，非正或非有限值），忽略该信号、\
                         不覆盖已有 input_tokens（避免计费口径被归零）",
                        pct
                    );
                }

                // 上下文使用量达到 100% 时，设置 stop_reason 为 model_context_window_exceeded。
                // 上界判定与上面的下界守卫**互不依赖**：即便将来下界改动，这条也照旧生效。
                if pct >= 100.0 {
                    self.state_manager
                        .set_stop_reason("model_context_window_exceeded");
                }
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
                self.credits_used = Some(self.credits_used.unwrap_or(0.0) + metering.usage);
                // Layer 1 cache 真值：上游 metering 事件可选携带（缺失保持 None）。
                // 入库侧 `resolved_usage` 用它覆盖本地 prefix 估算（见 metering_cache_read 注释）。
                if let Some(r) = metering.cache_read_input_tokens {
                    self.metering_cache_read = Some(r);
                }
                if let Some(c) = metering.cache_creation_input_tokens {
                    self.metering_cache_creation = Some(c);
                }
                tracing::debug!("收到 meteringEvent: {} {}", metering.usage, metering.unit);
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
            Event::Metadata(meta) => {
                if let Some(mapped) =
                    crate::kiro::model::events::map_metadata_stop_reason(meta.stop_reason.as_deref())
                {
                    self.saw_upstream_stop = true;
                    // 已有 tool_use 时客户端 stop_reason 仍走 tool_use；metadata 只标记流完整。
                    if !self.state_manager.has_tool_use {
                        self.state_manager.set_stop_reason(mapped);
                    }
                }
                Vec::new()
            }
            Event::Unknown {} => Vec::new(),
        }
    }

    /// 返回本次请求已解析出的最终用量（供统计埋点使用）
    ///
    /// - `input_tokens` 优先用 contextUsageEvent 计算的精确值，回退到估算；
    ///   **gross 口径**（含 cache），与 message_start/message_delta 里发给客户端的
    ///   billed 口径同名字段刻意不同，详见 [`ResolvedUsage::input_tokens`]
    /// - `output_tokens` 为流式累计
    /// - `credits_used` 为 meteringEvent 的真实计费量（可能为 None）
    pub fn resolved_usage(&self) -> ResolvedUsage {
        ResolvedUsage {
            input_tokens: self.context_input_tokens.unwrap_or(self.input_tokens),
            output_tokens: self.output_tokens,
            credits_used: self.credits_used,
            // Layer 1：上游 metering 真值优先（未缩放，落库真值），缺失才回落到本地估算。
            cache_read_tokens: self
                .metering_cache_read
                .unwrap_or_else(|| self.cache_usage.map(|c| c.cache_read_input_tokens).unwrap_or(0)),
            cache_creation_tokens: self.metering_cache_creation.unwrap_or_else(|| {
                self.cache_usage
                    .map(|c| c.cache_creation_input_tokens)
                    .unwrap_or(0)
            }),
        }
    }

    /// 检测上游是否返回了无效的空/近似空响应。
    ///
    /// 在 [`generate_final_events`] 收尾兜底**之后**判定 —— 推理降级下发、空格 thinking 块
    /// 等既有兜底已把「本可抢救」的响应变成非空，此刻仍为空才是真退化：
    /// 1. **完全空**：output_tokens == 0 且无工具调用。
    /// 2. **近似空 + 上下文过大**：output_tokens 极少（< 30）且无工具调用，
    ///    同时 input_tokens 超过「上下文过大」阈值（窗口的 28%）。此类响应是模型在
    ///    上下文压力下返回的无意义短文本（如几个空白 token），客户端拿到后会以为
    ///    end_turn 正常结束并继续对话，导致 agentic 循环反复卡住。
    pub fn is_empty_response(&self) -> bool {
        // 判定实现收敛到文件级共用函数 near_empty_response（非流式收尾同款），
        // 保证两条路径分界一致 —— 本方法只负责喂本上下文的量。
        let est = self.context_input_tokens.unwrap_or(self.input_tokens);
        near_empty_response(
            self.output_tokens,
            self.state_manager.has_tool_use,
            est,
            &self.model,
        )
    }

    /// 传输层干净结束、有部分可见产出，却没有任何终止信号。
    fn is_clean_eof_without_terminal(&self) -> bool {
        if !self.completion.is_ok() {
            return false;
        }
        if self.state_manager.stop_reason.is_some()
            || self.saw_upstream_stop
            || self.saw_tool_stop
        {
            return false;
        }
        self.body_content_seen || self.state_manager.has_tool_use
    }

    /// 空响应是否由「上下文过大」导致。
    ///
    /// 大输入空响应 → 不应重试（重试还是同样的大请求），应提示客户端压缩上下文；
    /// 小输入空响应 → 视为偶发，可重试。
    pub fn empty_response_is_oversized_context(&self) -> bool {
        let est = self.context_input_tokens.unwrap_or(self.input_tokens);
        est >= empty_response_oversized_threshold(&self.model)
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
    /// `strip_dsml_markers` 无条件启用（2026-08-10）：网关后面就是 DeepSeek 等国产模型
    /// （opencode Zen），客户端声明的模型名（`claude-sonnet-4-6` 之类）不能作为判断依据——
    /// 上游是 DeepSeek 就可能吐 `<｜DSML｜…>` 标记，与客户端叫它什么无关。剥离逻辑本身有
    /// 白名单 `is_dsml_keyword_after_pipe` 守正文（只剥 `dsml`/`tool`/`function` 前缀，
    /// `<｜注｜>` 这类 CJK 排版绝不误删），Claude 官方协议也不产这些标记，零误伤。

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
        // 无条件剥离（2026-08-10：上游就是 DeepSeek，客户端模型名无关）。
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
            // 探测 DSML 标记起点:`<` 紧跟全角竖线 `｜`(U+FF5C),或**闭合**形态 `</｜`。
            // 闭合形态必须一起认:上游吐完整结构时会带 `</｜DSML｜parameter>` / `</｜DSML｜invoke>`,
            // 只认 `<｜` 会让这些闭合标签原样泄漏成垃圾文本(实测
            // `<｜DSML｜parameter …>echo hi</｜DSML｜parameter>` → 客户端看到 `echo hi</｜DSML｜parameter>`)。
            let is_close_tag = chars[i] == '<'
                && i + 2 < chars.len()
                && chars[i + 1] == '/'
                && chars[i + 2] == '\u{FF5C}';
            if (chars[i] == '<' && i + 1 < chars.len() && chars[i + 1] == '\u{FF5C}') || is_close_tag
            {
                // 关键字起点:开标签跳过 `<｜`(2 字符),闭标签跳过 `</｜`(3 字符)。
                let kw_start = if is_close_tag { i + 3 } else { i + 2 };
                let rest: String = chars[kw_start..].iter().collect();
                // 白名单校验:`<｜` 后必须确为 DSML/tool/function 关键字才当标记;否则是正文,原样输出。
                // 若关键字尚不完整(rest 太短还看不出)且没闭合,则 hold 到下轮再判。
                let looks_marker = Self::is_dsml_keyword_after_pipe(&rest);
                // 🔴 闭合查找**限行**:半截标记(无 `>` 收尾)后如果跨行接正文,正文里任意 `>`
                // (a > b / => / markdown 引用)都会被误当标记闭合 → 从标记起整段吞掉,只剩 `>` 后残渣
                // (实测: `<｜DSML｜function_calls\n阅读\n如果 a > b 就返回` → 只回 `" b 就返回…"`)。
                // 遇到 `\n` 就停:半截标签只到**行尾**,换行后的正文绝不吞(fuckopencode 12c 同款教训)。
                let closed = chars[i..].iter().position(|&c| c == '>' || c == '\n');
                if looks_marker {
                    // `closed` 命中的是 `>` 还是行尾 `\n`,处置不同:
                    // - `>`  → 完整标记 `<｜…>`,连 `>` 一起丢弃;
                    // - `\n` → 标记在本行内没闭合(半截标记 + 换行接正文),只丢**本行内**的标记部分,
                    //          换行本身与后续正文必须保留(否则就是跨行吞正文那个 bug)。
                    let closed_by_gt = closed.map(|rel| chars[i + rel] == '>').unwrap_or(false);
                    if let Some(rel) = closed {
                        if closed_by_gt {
                            i += rel + 1; // 完整标记 `<｜…>` 整段丢弃(含 `>`)
                        } else {
                            // 停在 `\n` 前:标记行内部分丢弃。紧邻的换行是标记与正文的分隔符,
                            // 一并跳过,否则剥完会留一个孤立换行(`<｜DSML｜function_calls\n阅读` → `\n阅读`)。
                            i += rel;
                            if i < chars.len() && chars[i] == '\n' {
                                i += 1;
                            }
                        }
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
    const LEAKED_CONTROL_TOKENS: &'static [&'static str] = &[
        "court", "course", "count", "care", "card", "call", "課", "课",
    ];

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
                return (
                    line[tok.len()..].to_string(),
                    StripHit {
                        stripped: true,
                        standalone: true,
                    },
                );
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
                            return (
                                rest.to_string(),
                                StripHit {
                                    stripped: true,
                                    standalone: false,
                                },
                            ); // CJK/全角粘连 → 剥。
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
        observe_stray_leak_forms(
            content,
            &mut self.stray_standalone_seen,
            &mut self.stray_inline_seen,
        );
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

        // tool_use XML 泄漏过滤（跨 chunk 状态机）：上游把工具调用当**正文文本**吐时，
        // `<tool_use …></tool_use>` 会被客户端当普通文本渲染。这里在 token 估算与后续
        // 所有处理之前剥掉，保证：① 剥掉的字节不计入 output_tokens（记账=实际下发）；
        // ② 不喂给 invoke 重组层（那是"执行工具"，泄漏内容绝不能被重组回去凭空执行）；
        // ③ thinking 与正文路径都拿到同一份剥净后的文本。帧层（assistant.rs from_frame）
        // 已有一道兜底，这里处理跨 chunk 切开的标签——两层互补（参考仓 ref-grey 双层设计）。
        let content = self.filter_tool_use_xml_leaks(content);
        if content.is_empty() {
            return Vec::new();
        }
        let content = content.as_str();

        // 估算 tokens。⚠️ 这是嗅探路径（thinking_enabled）**唯一的** output_tokens 累计点：
        // 所有进 thinking_buffer 的内容（含内联 `<thinking>` 块）都来自本函数入参，
        // 在 `process_content_with_thinking` 等提取/下发处**不重复累计** —— 否则 thinking
        // 文本被计两次（整块 + 提取处），output_tokens 虚高、`is_empty_response` 的近空
        // 判定随之失效。结构化 reasoning 流（`process_reasoning_content`）不经过本函数，
        // 在它自己的下发点计一次（互不重复）。
        self.output_tokens += estimate_tokens(content);

        // 如果启用了thinking，需要处理thinking块
        if self.thinking_enabled {
            // ⭐ E1 关键约束：**结构化 reasoning 流与文本嗅探必须互斥**。
            //
            // 结构化 reasoning 无终止帧，真实形态是「N 帧 reasoning → 普通正文（无标签）」。
            // 而嗅探路径的分支是 `else if self.in_thinking_block`：若 reasoning 已开过 thinking 块，
            // 这段不带标签的正文会被当思考内容发成 thinking_delta →
            // **用户可见答案整段消失进思考面板**，且 has_non_thinking_blocks()=false 会让收尾
            // 把 stop_reason 置 max_tokens、只吐一个空格文本块 = 客户端显示空答案。
            //
            // 所以在这里先关掉 reasoning 开的块：首个非空正文 delta 即视为「思考结束」，
            // 之后正文照常走 text_delta。thinking_extracted 置位使嗅探路径也不再重新开块。
            if self.reasoning_stream_seen && self.in_thinking_block {
                let mut events = self.close_reasoning_thinking_block();
                events.extend(self.process_content_with_thinking(content));
                return events;
            }
            return self.process_content_with_thinking(content);
        }

        // 客户端没声明 thinking，但模型仍可能吐内联 `<thinking>` 标签 —— 剥掉，不当正文下发。
        // 口径与 `process_reasoning_content`(结构化帧在 !thinking_enabled 时直接丢弃)对齐，
        // 详见 `strip_inline_thinking_when_disabled` 的文档注释。
        // 放在 reclaim 之前:思考内容里可能含 `<invoke>` 样文本(模型在"想"要调什么工具),
        // 若先进 sniff 缓冲就会把思考里的假 invoke 重组成真 tool_use —— 那是凭空执行工具。
        let stripped = self.strip_inline_thinking_when_disabled(content);
        if stripped.is_empty() {
            return Vec::new();
        }
        let content = stripped.as_str();

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

    /// 客户端**没有**声明 thinking 时，把内联 `<thinking>…</thinking>` 从正文里剥掉。
    ///
    /// # 为什么必须做
    ///
    /// `thinking_enabled` 取自客户端请求体（`handlers.rs` 的 `payload.thinking.is_enabled()`），
    /// 而模型是否吐 `<thinking>` 标签**与客户端要不要无关** —— 它可能照吐。
    ///
    /// 而本仓对同一种内容已有明确口径：`process_reasoning_content` 在
    /// `!thinking_enabled` 时 **直接丢弃整帧**（"客户端没要就不给"）。内联标签若原样穿透，
    /// 就变成「结构化帧丢弃、内联标签泄漏」——同一种内容两套处置，模型的内部推理
    /// 被当正文吐给用户。这就是缺陷本体，本函数把口径对齐。
    ///
    /// # 状态复用是安全的
    ///
    /// 非 thinking 模式下 `thinking_buffer` / `in_thinking_block` 这两个字段**无人使用**
    /// （`process_content_with_thinking` 是唯一读写方，而它只在 `thinking_enabled` 时被调）。
    /// 故此处复用它们承载跨 chunk 状态，不新增字段、不与 thinking 路径互相干扰。
    ///
    /// # 与 thinking 路径共用同一套标签判据
    ///
    /// 起止标签都走 `find_real_thinking_start_tag` / `find_real_thinking_end_tag`
    /// （跳过被反引号包裹的、要求结束标签后跟 `\n\n`），**刻意不新写一套匹配**：
    /// 两套判据必然漂移，而漂移的后果是"某种形态在一条路径被剥、在另一条泄漏"。
    ///
    /// 尾部保留 `"</thinking>\n\n".len()`：标签可能跨 chunk 断开，过早放行会把半个
    /// 结束标签当正文吐出去。
    fn strip_inline_thinking_when_disabled(&mut self, content: &str) -> String {
        self.thinking_buffer.push_str(content);
        let mut visible = String::new();
        loop {
            if self.in_thinking_block {
                match find_real_thinking_end_tag(&self.thinking_buffer) {
                    Some(m) => {
                        // 丢弃思考内容本体 + 结束标签 + 其后的段落分隔换行（长度按实际形态算，
                        // 不能写死 13 —— 详见 `thinking_end_tag_consumed_len`）。
                        let cut =
                            m.start + thinking_end_tag_consumed_len(&self.thinking_buffer, &m);
                        self.thinking_buffer = self.thinking_buffer[cut..].to_string();
                        self.in_thinking_block = false;
                    }
                    None => {
                        // 严格判据不认，但可能是「等也没用」的形态（`</thinking>Answer`：
                        // 标签后的普通字符已就位，后续 chunk 改不了它）。那种形态若按下面
                        // 「整段丢弃」处理，答案会被**永久丢弃**且无痕 —— 见
                        // `find_permanently_unsatisfiable_end_tag`。
                        if let Some(m) =
                            find_permanently_unsatisfiable_end_tag(&self.thinking_buffer)
                        {
                            let cut =
                                m.start + thinking_end_tag_consumed_len(&self.thinking_buffer, &m);
                            self.thinking_buffer = self.thinking_buffer[cut..].to_string();
                            self.in_thinking_block = false;
                            continue;
                        }
                        // 结束标签未到：整段都还是思考内容（全部丢弃），只保留**真的可能是**
                        // 半个 `</thinking>` 的尾巴等下一个 chunk。
                        let keep = partial_thinking_tag_suffix_len(&self.thinking_buffer);
                        let cut = self.thinking_buffer.len() - keep;
                        self.thinking_buffer = self.thinking_buffer[cut..].to_string();
                        break;
                    }
                }
            } else {
                let open = find_real_thinking_start_tag(&self.thinking_buffer);
                // 孤立闭标签若排在开标签之前，必须先剥它 —— 否则它会被当成
                // 「开标签之前的正文」原样下发（实测泄漏形态①②）。
                let stray = find_stray_thinking_end_tag(&self.thinking_buffer)
                    .filter(|s| open.is_none_or(|o| s.start < o.start));
                if let Some(s) = stray {
                    visible.push_str(&self.thinking_buffer[..s.start]);
                    let cut = s.start + thinking_end_tag_consumed_len(&self.thinking_buffer, &s);
                    self.thinking_buffer = self.thinking_buffer[cut..].to_string();
                    continue;
                }
                match open {
                    Some(m) => {
                        visible.push_str(&self.thinking_buffer[..m.start]);
                        self.thinking_buffer = self.thinking_buffer[m.end()..].to_string();
                        self.in_thinking_block = true;
                    }
                    None => {
                        // 无起始标签：除**真的可能是**标签的尾巴外全部可见。
                        //
                        // ⚠️ 这里绝不能无条件扣 `"<thinking>".len()`=10 字节：`</invoke>` 只有
                        // 9 字节，会被整条扣住 → 下游重组层永远看不到闭合标签 → 工具不执行；
                        // 而 10 字节又盖不住 11 字节的 `</thinking>` ⇒ 孤立闭标签穿透。
                        // 两头都由 `partial_thinking_tag_suffix_len` 按标签语法判定。
                        let keep = partial_thinking_tag_suffix_len(&self.thinking_buffer);
                        let cut = self.thinking_buffer.len() - keep;
                        visible.push_str(&self.thinking_buffer[..cut]);
                        self.thinking_buffer = self.thinking_buffer[cut..].to_string();
                        break;
                    }
                }
            }
        }
        visible
    }

    /// 处理包含thinking块的内容
    fn process_content_with_thinking(&mut self, content: &str) -> Vec<SseEvent> {
        let mut events = Vec::new();

        // 将内容添加到缓冲区进行处理
        self.thinking_buffer.push_str(content);

        loop {
            if !self.in_thinking_block && !self.thinking_extracted {
                let open_tag = find_real_thinking_start_tag(&self.thinking_buffer);
                // 孤立闭标签（没有配对开标签）若排在开标签之前，必须先剥掉：否则它会被
                // 当成「开标签之前的正文」原样进 text_delta（实测泄漏形态①②）。
                // 两侧都是真正文，故只丢标签本身、内容全留。
                if let Some(s) = find_stray_thinking_end_tag(&self.thinking_buffer)
                    .filter(|s| open_tag.is_none_or(|o| s.start < o.start))
                {
                    let before = self.thinking_buffer[..s.start].to_string();
                    let cut = s.start + thinking_end_tag_consumed_len(&self.thinking_buffer, &s);
                    self.thinking_buffer = self.thinking_buffer[cut..].to_string();
                    // 与下面开标签分支同一口径：纯空白前缀不下发，避免 thinking 块之前
                    // 凭空多一个 text 块（会让客户端看到「新块 start → 旧块 stop」交错）。
                    if !before.trim().is_empty() {
                        events.extend(self.emit_non_thinking_text(&before));
                    }
                    continue;
                }
                // 查找 <thinking> 开始标签（跳过被反引号包裹的）
                if let Some(open) = open_tag {
                    // 发送 <thinking> 之前的内容作为 text_delta
                    // 注意：如果前面只是空白字符（如 adaptive 模式返回的 \n\n），则跳过，
                    // 避免在 thinking 块之前产生无意义的 text 块导致客户端解析失败
                    let before_thinking = self.thinking_buffer[..open.start].to_string();
                    if !before_thinking.is_empty() && !before_thinking.trim().is_empty() {
                        events.extend(self.emit_non_thinking_text(&before_thinking));
                    }

                    // 进入 thinking 块
                    self.in_thinking_block = true;
                    self.strip_thinking_leading_newline = true;
                    self.thinking_buffer = self.thinking_buffer[open.end()..].to_string();

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
                    //
                    // ⚠️ 这里原本无条件扣 `"<thinking>".len()` = **10 字节**，而
                    // `</thinking>` 是 **11 字节** ⇒ 孤立闭标签整条穿透进可见正文
                    // （实测泄漏形态①②的本体）。改为按标签语法判定实际可能的尾巴长度：
                    // 不可能是标签的散文尾巴扣留 0（首字节少等一个 chunk），而
                    // `</thinki`、`<thinking fo` 这类半标签一律扣住。
                    let keep = partial_thinking_tag_suffix_len(&self.thinking_buffer);
                    let safe_len = self.thinking_buffer.len() - keep;
                    if safe_len > 0 {
                        let safe_content = self.thinking_buffer[..safe_len].to_string();
                        // 如果 thinking 尚未提取，且安全内容只是空白字符，
                        // 则不发送为 text_delta，继续保留在缓冲区等待更多内容。
                        // 这避免了 4.6 模型中 <thinking> 标签跨事件分割时，
                        // 前导空白（如 "\n\n"）被错误地创建为 text 块，
                        // 导致 text 块先于 thinking 块出现的问题。
                        if !safe_content.is_empty() && !safe_content.trim().is_empty() {
                            events.extend(self.emit_non_thinking_text(&safe_content));
                            self.thinking_buffer = self.thinking_buffer[safe_len..].to_string();
                        } else if self.thinking_buffer.len() > MAX_THINKING_BUFFER_BYTES {
                            // review Finding 5 修复:上游若持续吐纯空白(无 <thinking>),纯空白分支
                            // 既不 emit 也不收缩 → thinking_buffer 无界增长 OOM(远程 DoS)。
                            // 超上限时把纯空白安全内容按普通文本吐出并收缩,只保留可能的半标签尾巴。
                            events.extend(self.emit_non_thinking_text(&safe_content));
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
                if let Some(end_m) = find_real_thinking_end_tag(&self.thinking_buffer) {
                    // 提取 thinking 内容
                    let thinking_content = self.thinking_buffer[..end_m.start].to_string();
                    if !thinking_content.is_empty() {
                        if let Some(thinking_index) = self.thinking_block_index {
                            // 思考文本已在入口（process_assistant_response 整块估算）计入，
                            // 此处不重复累加（双计会让 output_tokens 虚高、近空判定失效）。
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

                    // 剥离结束标签 + 其后的段落分隔换行（长度按实际形态算，见
                    // `thinking_end_tag_consumed_len`：后缀可能是 `\n\n` / `\n` / 空格 / `<`）
                    let cut =
                        end_m.start + thinking_end_tag_consumed_len(&self.thinking_buffer, &end_m);
                    self.thinking_buffer = self.thinking_buffer[cut..].to_string();
                } else {
                    // 没有找到结束标签，发送当前缓冲区内容作为 thinking_delta。
                    // 保留末尾可能是（半个或整个）结束标签的内容：
                    // find_real_thinking_end_tag 要求标签后跟含换行的空白才返回 Some，
                    // 因此 `</thinking>` 已到、`\n\n` 未到时也必须继续扣住，
                    // 否则标签字面量会被当作 thinking 内容发出。
                    //
                    // 长度**不能写死** `"</thinking>\n\n".len()`=13：带属性/带空白的闭标签
                    // （`</thinking >`）比它长，扣不住就漏；改为按标签语法判定。
                    let keep = partial_thinking_tag_suffix_len(&self.thinking_buffer);
                    let safe_len = self.thinking_buffer.len() - keep;
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
                // thinking 已提取完成，剩余内容作为 text_delta。
                // 这里曾是孤立闭标签的旁路：主循环剥了，本分支照样原样倒给客户端
                // （`<thinking>a</thinking>\n\n正文</thinking>尾` 这种二次闭标签）。
                if !self.thinking_buffer.is_empty() {
                    let remaining = self.thinking_buffer.clone();
                    self.thinking_buffer.clear();
                    let remaining = strip_stray_thinking_end_tags(&remaining);
                    if !remaining.is_empty() {
                        events.extend(self.emit_non_thinking_text(&remaining));
                    }
                }
                break;
            }
        }

        events
    }

    /// 非 thinking 文本的统一出口：当 reclaim 开关开且请求带工具时，
    /// 先进 invoke_sniff_buffer（与非 thinking 路径保持一致），否则直接发 text_delta。
    /// 这修复了 thinking 模式下 process_content_with_thinking 内部各文本分支
    /// 直接调用 create_text_delta_events 导致的 invoke_sniff_buffer 旁路 bug。
    fn emit_non_thinking_text(&mut self, text: &str) -> Vec<SseEvent> {
        if self.reclaim_enabled && !self.known_tool_names.is_empty() {
            self.invoke_sniff_buffer.push_str(text);
            self.drain_invoke_sniff_buffer(false)
        } else {
            self.create_text_delta_events(text)
        }
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
        self.saw_tool_stop = true;
        self.reclaimed_invoke_count += 1;
        crate::common::recovery_metrics::bump_reclaimed_invoke();
        let block_index = self.state_manager.next_block_index();
        let tool_use_id = format!("toolu_{}", Uuid::new_v4().to_string().replace('-', ""));
        self.tool_block_indices
            .insert(tool_use_id.clone(), block_index);
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
            if !line.is_empty()
                && (STRAY_INVOKE_TOKENS.contains(&line) || is_short_flood_token(line))
            {
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
                        let fence_after_before = fence_open_after(
                            self.code_fence_open,
                            &self.fence_scan_partial,
                            &before,
                        );
                        let parsed = parse_invoke_block(&buf[start..end]);
                        let name_known = parsed
                            .as_ref()
                            .map(|(n, _)| self.known_tool_names.contains(n))
                            .unwrap_or(false);
                        if invoke_looks_like_real_leak(&before) && !fence_after_before && name_known
                        {
                            // 真泄漏:吐块前文本(剥掉尾部独立 stray 行)+ 合成 tool_use。
                            if !before.is_empty() {
                                // before 的围栏状态要并入(它会作为文本吐,推进围栏奇偶)。
                                advance_code_fence_state(
                                    &mut self.code_fence_open,
                                    &mut self.fence_scan_partial,
                                    &before,
                                );
                                events.extend(self.emit_text_delta_guarded(&before));
                            }
                            let (name, input_json) =
                                parsed.expect("parsed is Some when name_known");
                            events.extend(self.synthesize_tool_use(name, input_json));
                        } else {
                            // 不捞回(句中/围栏内/工具名未知/解析失败)→ 整段当普通文本吐。
                            let seg = buf[..end].to_string();
                            advance_code_fence_state(
                                &mut self.code_fence_open,
                                &mut self.fence_scan_partial,
                                &seg,
                            );
                            events.extend(self.emit_text_delta_guarded(&seg));
                        }
                        buf = buf[end..].to_string();
                        continue;
                    }
                    None => {
                        // 半块(未闭合)。行首判定:非行首/围栏内当文本直接吐,不 hold。
                        let before = strip_trailing_stray_tokens(&buf[..start]).to_string();
                        let fence_after_before = fence_open_after(
                            self.code_fence_open,
                            &self.fence_scan_partial,
                            &before,
                        );
                        if !invoke_looks_like_real_leak(&before) || fence_after_before {
                            if !buf.is_empty() {
                                let seg = buf.clone();
                                advance_code_fence_state(
                                    &mut self.code_fence_open,
                                    &mut self.fence_scan_partial,
                                    &seg,
                                );
                                events.extend(self.emit_text_delta_guarded(&seg));
                            }
                            break;
                        }
                        // 行首未闭合块:吐 start 前文本,保留 start.. 等闭合。
                        if start > 0 {
                            let seg = buf[..start].to_string();
                            advance_code_fence_state(
                                &mut self.code_fence_open,
                                &mut self.fence_scan_partial,
                                &seg,
                            );
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
                            advance_code_fence_state(
                                &mut self.code_fence_open,
                                &mut self.fence_scan_partial,
                                &seg,
                            );
                            events.extend(self.emit_text_delta_guarded(&seg));
                        }
                    } else {
                        let keep = partial_invoke_tag_suffix_len(&buf);
                        let emit_len = buf.len() - keep;
                        if emit_len > 0 {
                            let seg = buf[..emit_len].to_string();
                            advance_code_fence_state(
                                &mut self.code_fence_open,
                                &mut self.fence_scan_partial,
                                &seg,
                            );
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

    // ===== tool_use XML 泄漏过滤（流层；帧层兜底在 kiro/model/events/assistant.rs）=====
    //
    // 参考仓 ref-grey 的两层设计：
    //   · 流层跨 chunk 状态机（ref-grey stream.rs:33-106 ToolUseXmlLeakFilter，:984 每帧文本过一遍，
    //     :1512 收尾 finish()）：完整标签立即剥，半标签 hold 等下 chunk；
    //   · 帧层 from_frame 就地剥（ref-grey assistant.rs:12-90）。
    // 我们此前只有 invoke/antml: 形态的文本化工具调用处理（invoke_sniff_buffer），
    // `<tool_use …>` 是另一种标签形态，全仓零命中（rg tool_use_xml 无结果）。
    //
    // 实现骨架复用 sniff 缓冲模式（与 drain_invoke_sniff_buffer 同一批函数演化出来的
    // 跨 chunk 安全策略），但**不做重组**：这里的目的纯粹是把泄漏标签从正文里剥掉，
    // 不把泄漏内容合成回结构化 tool_use（那会把上游当文本吐的工具调用凭空"执行"）。
    //
    // 判据（与帧层 strip_tool_use_xml_leaks 共享同一套常量）：
    //   开标签必须形如 `<tool_use` 后紧跟 `>` 或空白，`<tool_user>`/`<tool_uses>` 这类
    //   正文不剥；闭合必须等 `</tool_use>` 原样出现。只剥"字面量 tool_use 标签"，
    //   绝不触碰结构化 content block（结构化工具调用走 ToolUseEvent，不经过这段文本流）。

    /// tool_use XML 泄漏过滤开标签前缀/闭合标签。与帧层（kiro/model/events/assistant.rs）
    /// 共用同一组字面量，保证两条防线判定一致。
    const TOOL_USE_XML_PREFIX: &str = crate::kiro::model::events::TOOL_USE_XML_PREFIX;
    const TOOL_USE_XML_CLOSE: &str = crate::kiro::model::events::TOOL_USE_XML_CLOSE;
    /// 连续剥离态的单轮累积上限（256 KiB，与 `MAX_INVOKE_HOLD_BYTES` 同量级）：
    /// 永不闭合的 `<tool_use …` 不能无限吞正文（吞掉用户可见的整段回答 = 静默吞字），
    /// 超限即放弃剥离、把后续当普通文本放行。真实泄漏块远小于此。
    const MAX_TOOL_USE_XML_STRIP_BYTES: usize = 262_144;

    /// 跨 chunk 过滤 `content` 里的字面量 tool_use XML 泄漏，返回应下发的文本。
    ///
    /// 语义与 ref-grey `ToolUseXmlLeakFilter::filter` 对齐：保留可能跨 chunk 的
    /// `<tool_use` 半前缀，已确认的标签体则整段丢弃（含闭合前的内容 —— 那是工具调用的
    /// JSON 参数，不是该给用户的正文）。
    fn filter_tool_use_xml_leaks(&mut self, content: &str) -> String {
        // latch：本轮已放弃剥离 ⇒ 原样放行，不再扫描（防上限触发后重入死循环）。
        if self.tool_use_xml_strip_disabled {
            return content.to_string();
        }
        self.tool_use_xml_buffer.push_str(content);
        let mut out = String::with_capacity(self.tool_use_xml_buffer.len());
        let mut rest = self.tool_use_xml_buffer.as_str();

        loop {
            if self.tool_use_xml_stripping {
                // 正在剥一个 `<tool_use …` 块：找闭合标签。
                if let Some(close_start) = rest.find(Self::TOOL_USE_XML_CLOSE) {
                    self.tool_use_xml_stripped += close_start as u64;
                    crate::common::recovery_metrics::bump_tool_use_xml_stripped();
                    rest = &rest[close_start + Self::TOOL_USE_XML_CLOSE.len()..];
                    self.tool_use_xml_stripping = false;
                    self.tool_use_xml_strip_run = 0;
                    continue;
                }
                // 闭合还没到：剥掉本 chunk 除"可能是半个 </tool_use 闭合前缀"的尾巴以外的
                // 全部。**注意保留尾巴** —— ref-grey 这里清空 buffer（:51），闭合被切成
                // `<`+`/`+`tool`… 时永远拼不齐，响应余下全部被吞（我们实测复现）。而「剥
                // 1 字节、留 10 字节尾巴」也是安全的：`</tool_use` 最长前缀 10 字节，被误判
                // 为闭合前缀的普通正文最多 hold 10 字节、下一 chunk 就判定放行，不吞字。
                let keep = partial_tool_use_xml_close_suffix(rest);
                let dropped = rest.len() - keep;
                let carry = rest[rest.len() - keep..].to_string();
                self.tool_use_xml_stripped += dropped as u64;
                self.tool_use_xml_strip_run = self.tool_use_xml_strip_run.saturating_add(dropped);
                if self.tool_use_xml_strip_run > Self::MAX_TOOL_USE_XML_STRIP_BYTES {
                    // 🔴 上限兜底：永不闭合的开标签 → 放弃剥离，后续当普通文本放行。
                    //
                    // ⚠️ 必须同时**清空 carry 并置 latch**，否则会死循环：carry 里仍留着
                    // `</tool_use` 的部分前缀，下一 chunk 拼上后又在下面的 `find(PREFIX)`
                    // 命中 `<tool_use` → 重新进剥离态 → strip_run 归零重新计数 →
                    // 永远放不出正文（agent 写的
                    // `tool_use_xml_never_closing_tag_releases_after_cap` 正是抓到这个）。
                    //
                    // latch 语义：本轮响应一旦触发过上限，就**不再**对该轮启用剥离
                    // —— 一条流里出现超 256 KiB 不闭合标签，说明上游行为异常，
                    // 此时"宁可让客户端看到裸标签，也不能吞掉整段回答"。
                    self.tool_use_xml_stripping = false;
                    self.tool_use_xml_strip_run = 0;
                    self.tool_use_xml_strip_disabled = true;
                    self.tool_use_xml_buffer.clear();
                    return out;
                }
                self.tool_use_xml_buffer = carry;
                return out;
            }

            let Some(start) = rest.find(Self::TOOL_USE_XML_PREFIX) else {
                // 无开标签：保留可能是半个 `<tool_use` 前缀的尾巴，其余正常吐。
                let keep = partial_tool_use_xml_prefix_suffix(rest);
                let emit_len = rest.len().saturating_sub(keep);
                out.push_str(&rest[..emit_len]);
                self.tool_use_xml_buffer = rest[emit_len..].to_string();
                return out;
            };

            out.push_str(&rest[..start]);
            let after_start = &rest[start..];
            let Some(open_end) = after_start.find('>') else {
                // 开标签没到 `>`：看它像不像 `<tool_use` 前缀。像 → 进入剥离态等闭合
                //（合并不完整开标签本身也剥掉——它仍是泄漏）；不像 → 当普通文本放行。
                if is_potential_tool_use_xml_tag_start(after_start) {
                    self.tool_use_xml_stripping = true;
                    // 新块开始，累积计数从零起（上限是"单块"上限，不是整请求上限）。
                    self.tool_use_xml_strip_run = 0;
                    self.tool_use_xml_stripped += after_start.len() as u64;
                    self.tool_use_xml_buffer.clear();
                    return out;
                }
                out.push_str(&after_start[..Self::TOOL_USE_XML_PREFIX.len()]);
                rest = &after_start[Self::TOOL_USE_XML_PREFIX.len()..];
                continue;
            };

            let tag_head = &after_start[..open_end];
            // `<tool_user>` / `<tool_uses>` 不是 tool_use 开标签 → 按普通文本吐这 9 字节前缀。
            if !tag_head
                .get(Self::TOOL_USE_XML_PREFIX.len()..)
                .is_some_and(|suffix| suffix.is_empty() || suffix.starts_with(char::is_whitespace))
            {
                out.push_str(&after_start[..Self::TOOL_USE_XML_PREFIX.len()]);
                rest = &after_start[Self::TOOL_USE_XML_PREFIX.len()..];
                continue;
            }

            let after_open = &after_start[open_end + 1..];
            if let Some(close_start) = after_open.find(Self::TOOL_USE_XML_CLOSE) {
                // 完整 `<tool_use …>…</tool_use>`：整段剥掉，只留块后文本。
                self.tool_use_xml_stripped += (open_end + 1 + close_start
                    + Self::TOOL_USE_XML_CLOSE.len()) as u64;
                crate::common::recovery_metrics::bump_tool_use_xml_stripped();
                rest = &after_open[close_start + Self::TOOL_USE_XML_CLOSE.len()..];
            } else {
                // 有合法开标签但闭合还没到：进入剥离态，等闭合。
                self.tool_use_xml_stripping = true;
                // 新块开始，累积计数从零起（上限是"单块"上限，不是整请求上限）。
                self.tool_use_xml_strip_run = 0;
                self.tool_use_xml_stripped += after_start.len() as u64;
                self.tool_use_xml_buffer.clear();
                return out;
            }
        }
    }

    /// 收尾 flush tool_use XML 泄漏过滤缓冲（流结束）：已确认在标签内的残留整体丢弃
    ///（泄漏）；被误判为半前缀的正文残留补发，不吞字。
    fn finish_tool_use_xml_filter(&mut self) -> String {
        let residue = std::mem::take(&mut self.tool_use_xml_buffer);
        let was_stripping = self.tool_use_xml_stripping;
        self.tool_use_xml_stripping = false;
        self.tool_use_xml_strip_run = 0;
        if residue.is_empty() {
            return String::new();
        }
        if was_stripping {
            // 流在 `<tool_use …` 标签内就结束了：残留是泄漏，丢弃（不补发为正文）。
            self.tool_use_xml_stripped += residue.len() as u64;
            crate::common::recovery_metrics::bump_tool_use_xml_stripped();
            return String::new();
        }
        // 否则残留只是"可能是半个开标签"的正文尾巴：EOF 时已确定不是标签（没有下一
        // chunk 了），补发为普通文本，不吞字。
        crate::kiro::model::events::strip_tool_use_xml_leaks(&residue)
    }

    /// 处理上游 `reasoningContentEvent` 的一帧结构化思考增量（E1）。
    ///
    /// # 与文本嗅探路径的关系
    ///
    /// 文本嗅探（`process_thinking_content` 里找 `<thinking>` 标签）**保留作兜底**不删：
    /// 上游可能对某些模型仍走内联标签，两条路径都可能触发。因此两者**共用同一个**
    /// `in_thinking_block` / `thinking_block_index` 状态 —— 这是"绝不重复开块"的保证：
    /// 若嗅探路径已经开过 thinking 块，本函数直接往同一个 index 追加 delta。
    ///
    /// # 为什么不在这里关块
    ///
    /// 上游不发"思考结束"信号（`reasoningContentEvent` 是纯增量，没有终止帧），
    /// 收尾统一由 `generate_final_events` 处理（它会补 signature_delta + content_block_stop）——
    /// 与嗅探路径遇不到 `</thinking>` 时的收尾走同一条路，不新增第二套收尾逻辑。
    fn process_reasoning_content(&mut self, reasoning: &ReasoningContentEvent) -> Vec<SseEvent> {
        // 缓存上游真签名（若有）：关闭 thinking 块时 `create_signature_delta_event` 优先回传它。
        // Foxfishc 实测「伪造签名不被识别，cache_read 仍 0」——真签名是多轮 cache 命中的关键。
        // 上游不发则保持 None，收尾回退占位符，行为与改动前逐字节一致。
        if let Some(sig) = reasoning.signature.as_deref() {
            if !sig.is_empty() {
                self.pending_reasoning_signature = Some(sig.to_string());
            }
        }
        let text = &reasoning.text;
        // thinking 未开启（客户端没要 thinking）→ 本帧不下发。
        // 不能当正文发：那会把模型的内部推理混进用户可见回答里。
        //
        // 但**不能就地扔掉**：若本轮上游只吐 reasoning、一个字正文都没有，客户端会拿到
        // 完全空的响应。所以原文留一份（有上限），到 `generate_final_events` 再按
        // 「正文确实为空」判定要不要降级下发 —— 判定必须在收尾做，因为正文可能整轮被
        // hold 在 invoke_sniff_buffer 里、直到 flush 才出现。
        if !self.thinking_enabled {
            self.retain_discarded_reasoning(text);
            return Vec::new();
        }
        if text.is_empty() {
            return Vec::new();
        }

        // 标记本轮走过结构化流：供 process_assistant_response 判定「正文到来即关块」，
        // 使两条路径互斥（否则正文会被当思考内容，答案消失）。
        self.reasoning_stream_seen = true;

        let mut events = Vec::new();
        // 首帧才开块；若嗅探路径已开过，复用它的 index（不重复开块）。
        if !self.in_thinking_block {
            self.in_thinking_block = true;
            let idx = self.state_manager.next_block_index();
            self.thinking_block_index = Some(idx);
            events.extend(self.state_manager.handle_content_block_start(
                idx,
                "thinking",
                json!({
                    "type": "content_block_start",
                    "index": idx,
                    "content_block": { "type": "thinking", "thinking": "" }
                }),
            ));
        }

        if let Some(idx) = self.thinking_block_index {
            // thinking 也是真实下发的生成内容，计入 output_tokens（Anthropic 官方
            // usage 口径 output_tokens 含 thinking tokens）。旧代码只计正文与工具参数，
            // 客户端看到的 output_tokens 系统性偏低。此处已过 `text.is_empty()` 早退，
            // 不误计空的 thinking_delta。
            self.output_tokens += estimate_tokens(text);
            events.push(self.create_thinking_delta_event(idx, text));
        }
        events
    }

    /// 把 `!thinking_enabled` 下不下发的 reasoning 原文攒进 `discarded_reasoning`，
    /// 按 [`Self::MAX_DISCARDED_REASONING_BYTES`] 截断（UTF-8 边界安全）。
    ///
    /// 达上限后**丢弃后续帧而非滚动覆盖**：兜底要的是"开头那段能读懂的推理"，
    /// 保尾巴反而会给出一段没有上文的残句。
    fn retain_discarded_reasoning(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        let room =
            Self::MAX_DISCARDED_REASONING_BYTES.saturating_sub(self.discarded_reasoning.len());
        if room == 0 {
            return;
        }
        if text.len() <= room {
            self.discarded_reasoning.push_str(text);
        } else {
            // 直接按字节切会 panic 在多字节字符中间（中文推理是常态）。
            let cut = find_char_boundary(text, room);
            self.discarded_reasoning.push_str(&text[..cut]);
        }
    }

    /// 空响应兜底把 reasoning 降级成正文前，补上正文路径的两道文本清洗。
    ///
    /// # 为什么需要单独一个函数，而不是让兜底走 `process_assistant_response`
    ///
    /// 兜底刻意绕开整条正文链（理由见调用处：sniff 缓冲已 flush 过、推理里的
    /// `<invoke>` 不能被重组成真 tool_use）。但"绕开重组"不等于"该绕开清洗"——
    /// reasoning 与正文是**同一次生成**的产物，正文会遇到的模型侧退化它同样会遇到，
    /// 而降级之后它就是客户端眼里的正文了。所以这里只把两道清洗**单独**调一遍：
    ///
    /// ① `clean_leaked_tokens`（#70544 行首泄漏词，开关 `tool_clean_leaked_tokens`）——
    ///    它是无状态的逐行处理，可以独立调用，且会照常累加 `leaked_stripped` 计数，
    ///    使兜底路径的泄漏在收尾诊断里同样可见（不黑箱）。
    /// ② `strip_stray_thinking_end_tags`（孤立 `</thinking>` 标记）—— 推理文本里出现
    ///    闭标签字面量时，原样下发就是把标记泄漏给客户端（正文侧同一形态已有此清洗）。
    ///
    /// # 为什么**不**调 `strip_inline_thinking_when_disabled`
    ///
    /// 那个函数是**跨 chunk 有状态**的剥离器：遇到未闭合的 `<thinking>` 会把余下全部
    /// 内容当思考丢掉，遇到疑似半标签的尾巴会扣进 `thinking_buffer` 等下一个 chunk。
    /// 而此刻已经过了收尾 flush（本文件 :3159 那条），扣下的尾巴**再没有人 drain**。
    /// 更根本的是语义反了：兜底的全部目的就是"把推理当正文发出去"，再拿"剥掉推理"的
    /// 剥离器过一遍，最坏情况是整段被丢 ⇒ 重新变成它要修的那个空响应。
    fn sanitize_degraded_reasoning(&mut self, reasoning: &str) -> String {
        let cleaned = if super::handlers::tool_clean_leaked_tokens_enabled() {
            self.clean_leaked_tokens(reasoning)
        } else {
            reasoning.to_string()
        };
        strip_stray_thinking_end_tags(&cleaned).into_owned()
    }

    /// 关闭 thinking 块（结构化 reasoning 首个正文到来，或嗅探块在 tool_use 前尚无闭标签）。
    ///
    /// 收尾顺序与嗅探路径遇到 `</thinking>` 时一致：空 thinking_delta → signature_delta →
    /// content_block_stop，保证客户端侧 thinking 块结构完整（Anthropic SDK 会校验 signature 非空）。
    ///
    /// 置 `thinking_extracted = true` 使嗅探路径此后不再重新开块（`process_content_with_thinking`
    /// 的首个分支条件含 `!self.thinking_extracted`）。
    fn close_reasoning_thinking_block(&mut self) -> Vec<SseEvent> {
        let Some(idx) = self.thinking_block_index else {
            self.in_thinking_block = false;
            return Vec::new();
        };
        let mut events = Vec::new();
        events.push(self.create_thinking_delta_event(idx, ""));
        events.push(self.create_signature_delta_event(idx));
        events.extend(self.state_manager.handle_content_block_stop(idx));
        self.in_thinking_block = false;
        self.thinking_extracted = true;
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
    /// 携带非空签名，满足客户端 thinking 模式下的本地校验。详见
    /// [`THINKING_SIGNATURE_PLACEHOLDER`]。
    ///
    /// 优先回传**上游真签名**（若本流收到过 `reasoningContentEvent` 且带 `signature`）：
    /// Foxfishc 实测「伪造签名不被上游识别，cache_read 仍 0」，真签名是多轮 cache 命中的关键。
    /// 上游不发则回退占位符（`take` 只消费一次，thinking 块只在流末尾关一次）。
    fn create_signature_delta_event(&mut self, index: i32) -> SseEvent {
        let signature = self
            .pending_reasoning_signature
            .take()
            .unwrap_or_else(|| THINKING_SIGNATURE_PLACEHOLDER.to_string());
        SseEvent::new(
            "content_block_delta",
            json!({
                "type": "content_block_delta",
                "index": index,
                "delta": {
                    "type": "signature_delta",
                    "signature": signature
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
            if let Some(end_m) = find_real_thinking_end_tag_at_buffer_end(&self.thinking_buffer) {
                let thinking_content = self.thinking_buffer[..end_m.start].to_string();
                if !thinking_content.is_empty() {
                    if let Some(thinking_index) = self.thinking_block_index {
                        // 同 :2531 口径：思考文本已在入口整块估算时计入，此处不重复累加。
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
                let remaining = self.thinking_buffer[end_m.end()..].trim_start().to_string();
                self.thinking_buffer.clear();
                if !remaining.is_empty() {
                    events.extend(self.create_text_delta_events(&remaining));
                }
            } else if self.reasoning_stream_seen {
                // 结构化 reasoning 开的 thinking 块：内容直接以 thinking_delta 下发、不进
                // thinking_buffer，故上面的 `find_real_thinking_end_tag_at_buffer_end` 恒为 None。
                // 工具调用前必须先把块关掉 —— 否则 tool_use 块 start 时 thinking 块仍未 stop，
                // 违反 Anthropic SSE「先 stop 当前块、再 start 下一块」的顺序契约（CC 解析报错）。
                // 兜底 flush buffer：极边缘的「嗅探开块 → 又来 reasoning」混合场景里，
                // buffer 可能还有嗅探暂存的内容，关块前先作为 thinking_delta 补发（不吞字）。
                if !self.thinking_buffer.is_empty() {
                    if let Some(idx) = self.thinking_block_index {
                        // 同 :2531 口径：缓冲内容已在入口整块估算时计入，此处不重复累加。
                        events.push(self.create_thinking_delta_event(idx, &self.thinking_buffer));
                    }
                    self.thinking_buffer.clear();
                }
                events.extend(self.close_reasoning_thinking_block());
            } else {
                // 嗅探路径已开 thinking 块（`in_thinking_block && !reasoning_stream_seen`），
                // 尚无结束标签：内容可能已作为 thinking_delta 下发（buffer 为空），
                // 也可能还扣着半标签残留。关块顺序与上两支相同，不能另造第四种。
                if !self.thinking_buffer.is_empty() {
                    if let Some(idx) = self.thinking_block_index {
                        events.push(self.create_thinking_delta_event(idx, &self.thinking_buffer));
                    }
                    self.thinking_buffer.clear();
                }
                events.extend(self.close_reasoning_thinking_block());
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
        // 仅当入站映射过才记下 tool_use_id → 还原名，供 generate_final_events 截断兜底
        // 做参数还原。未映射的（Kiro 直接发同名工具）不记 —— 兜底据此区分「该还原」与
        // 「该原样透传」，避免把不认识的参数清空。
        if self.tool_name_map.contains_key(&tool_use.name) {
            self.tool_use_names
                .insert(tool_use.tool_use_id.clone(), original_name.clone());
        }
        // Bug C 校验用：无条件记 block_index → **发给模型的工具名**（`tool_use.name`，
        // 即可能被缩短过的短名），与 `tool_required_fields` 的 key 同口径。
        // 与上面那个 `tool_use_names` 的区别见字段文档（那个只记被缩短的、且存还原名）。
        self.tool_block_names
            .insert(block_index, tool_use.name.clone());

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
            let buf = self
                .tool_input_sent
                .entry(tool_use.tool_use_id.clone())
                .or_default();
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
            self.saw_tool_stop = true;
            let mut assembled = self
                .tool_input_sent
                .remove(&tool_use.tool_use_id)
                .unwrap_or_default();
            // Bug C 必须在出站还原之前、对 **Kiro 形态** assembled 校验（required 名单
            // 也是 Kiro 键：path/text）。先 map 再校验会把 Write 的 file_path/content
            // 拿去对 path/text，默认配置下六个内置工具流式调用恒被误杀。
            if !self.fail_bug_c_if_missing(block_index, &assembled) {
                // 出站参数还原：把 Kiro 参数形态还原成 Claude Code 参数形态
                // （fs_write 的 path/text → Write 的 file_path/content、read_file 的 start_line →
                // Read 的 offset）。
                // ⚠️ **仅当该 Kiro 工具名入站时映射过**（tool_name_map 有记录）才做还原；否则
                // （Kiro 直接发同名工具，未经历入站映射，如 DSML 调试场景的裸 "Write"）原样透传，
                // 避免 map_tool_input_from_kiro 把不认识的参数（code/note/DSML 标记）清空成 {}。
                // 仅对合法 JSON 生效；非法串交 flush_tool_input 的 repair 层。
                if !assembled.is_empty() && self.tool_name_map.contains_key(&tool_use.name) {
                    if let Ok(value) = serde_json::from_str::<serde_json::Value>(&assembled) {
                        assembled = crate::anthropic::converter::map_tool_input_from_kiro(
                            &original_name,
                            value,
                        )
                        .to_string();
                    }
                }
                events.extend(self.flush_tool_input(block_index, assembled));
            }
            if let Some(stop_event) = self.state_manager.handle_content_block_stop(block_index) {
                events.push(stop_event);
            }
        }

        events
    }

    /// Bug C 判据：该 block 的工具参数是否缺 `input_schema.required` 声明的字段。
    ///
    /// 返回 `Some(缺失字段名列表)` 表示确实缺（调用方据此置失败态）；`None` 表示无需干预。
    /// `assembled` 必须是 **Kiro 形态**（与 `tool_required_fields` 同源）；客户端形态
    /// （`file_path`/`content`）对不上 `path`/`text`，会假阳性。
    ///
    /// **五种情况一律返回 `None`（不干预）**，每条都是刻意的：
    /// 1. 这个 block 不是工具块（`tool_block_names` 无记录）—— 文本块无参数可校验；
    /// 2. 该工具没有必需参数（`tool_required_fields` 无记录）—— 包括 WebSearch 类
    ///    （它们没有 `input_schema`）与 `required` 为空的工具；
    /// 3. `assembled` 不是合法 JSON —— 那是 Bug A 的地盘，已由上游 repair 层处理过；
    /// 4. 顶层不是 object —— `required` 描述顶层属性，数组/标量形态不在本判据范围；
    /// 5. 所有必需字段都在 —— 正常路径。
    ///
    /// ⚠️ 只判「键是否存在」，**不判值的类型/内容**：类型不匹配的容错空间远大于
    /// 「字段整个缺失」，做全量 schema 校验会把上游的合理变体误判成失败。
    /// 显式 `null` 视为**存在**（客户端会把 null 当"给了但为空"，与"没给"语义不同）。
    fn find_missing_required_fields(
        &self,
        block_index: i32,
        assembled: &str,
    ) -> Option<Vec<String>> {
        let tool_name = self.tool_block_names.get(&block_index)?;
        let required = self.tool_required_fields.get(tool_name)?;
        let value: serde_json::Value = serde_json::from_str(assembled).ok()?;
        let obj = value.as_object()?;
        let missing: Vec<String> = required
            .iter()
            .filter(|k| !obj.contains_key(k.as_str()))
            .cloned()
            .collect();
        if missing.is_empty() {
            None
        } else {
            Some(missing)
        }
    }

    /// Bug C 闸门：对 **Kiro 形态** assembled 判缺。
    ///
    /// `true` = 已置 `INVALID_TOOL_INPUT`，调用方不得 `flush_tool_input`。
    /// 必须在 `map_tool_input_from_kiro` 之前调用；flush 只负责 JSON 修复与下发。
    fn fail_bug_c_if_missing(&mut self, block_index: i32, kiro_form_assembled: &str) -> bool {
        if !super::handlers::tool_stream_align_failure_enabled()
            || !self.completion.is_ok()
            || self.tool_required_fields.is_empty()
        {
            return false;
        }
        let Some(missing) = self.find_missing_required_fields(block_index, kiro_form_assembled)
        else {
            return false;
        };
        tracing::warn!(
            block_index,
            missing = %missing.join(","),
            "tool_use 参数合法但缺必需字段（Bug C）：置失败态让客户端重试整轮，不下发坏参数"
        );
        self.completion = CompletionStatus::UpstreamError {
            code: "INVALID_TOOL_INPUT".to_string(),
            message: format!(
                "工具调用缺少必需参数：{}（模型侧生成异常），请重试。",
                missing.join("、")
            ),
        };
        true
    }

    /// 把某 tool_use 累积完整的 input 作为**单个** input_json_delta 发出（stop 时调用 / 截断收尾兜底）。
    ///
    /// 校验完整 JSON：合法→原样发；非法→告警并尽力发（不静默吞成空参数——空参数会让客户端把
    /// 一个失败的工具调用当成"无参数成功调用"执行，比报错更危险）。空串→不发（无参工具，客户端得 `{}`）。
    /// Bug C（缺 required）不在本函数：调用方必须先对 Kiro 形态调用 `fail_bug_c_if_missing`。
    fn flush_tool_input(&mut self, block_index: i32, mut assembled: String) -> Vec<SseEvent> {
        if assembled.is_empty() {
            return Vec::new();
        }
        if serde_json::from_str::<serde_json::Value>(&assembled).is_err() {
            // 拼装后仍非法：多因上游发了非法 JSON（如 JSON 不支持的 \x 转义 / 截断 \uXXXX / 裸控制符）
            // 或中间帧丢失截断。客户端拿坏 JSON 直接 parse 失败 → Invalid tool parameters。
            // 归因标签（纯可观测，绝不进控制流）：单遍 string-aware 扫描把非法串按责任方分流——
            // truncated=帧丢失/上游截断、illegal_chars=模型侧非法转义/裸控制符、两者兼有、其它畸形。
            // 判据与 repair 层同源，只用于日志定位真因（"修不好的残留到底是谁的责任"）。
            let defect = classify_tool_json_defect(&assembled);
            // malformed 子型细分（纯诊断）：只在归因 Malformed 时算——它是"结构闭合+字符合法但仍非法"的
            // 兜底类,笼统 "malformed" 无法区分类型 A(上游吐坏)/类型 C(我们拼坏)。子型标签(glued/
            // trailing_comma/missing_comma/expected_value/...)直接指向责任方。其它归因输出 "-"（不适用）。
            let subkind = if defect == ToolJsonDefect::Malformed {
                malformed_subkind(&assembled)
            } else {
                "-"
            };
            tracing::warn!(
                block_index,
                defect = defect.as_str(),
                subkind,
                "tool_use 拼装后 input 非合法 JSON（长度 {}），归因={} 子型={}",
                assembled.len(),
                defect.as_str(),
                subkind
            );
            // 帧探针（KIRO_TOOL_TRACE）：非法时额外打印**完整拼装串**全文（含 model + 归因标签 + 子型），
            // 用于坐实是类型 A（上游模型帧本身含非法转义/乱码 token）还是类型 C（合并逻辑洞，已修）。
            if tool_trace_enabled() {
                tracing::warn!(
                    target: "kiro::tool_trace",
                    model = %self.model,
                    block_index,
                    defect = defect.as_str(),
                    subkind,
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
            let repaired_ok = serde_json::from_str::<serde_json::Value>(&assembled).is_ok();
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
                if super::handlers::tool_expose_error_to_client_enabled() && self.completion.is_ok()
                {
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
            tracing::info!(
                block_index,
                "tool_use 参数为双重编码,已解一层还原为 object/array"
            );
            assembled = unwrapped;
        }

        // 走到这里 `assembled` 已确定要实际下发（合法 / 修复成功 / 无失败态）：
        // 此时才计入 output_tokens（旧代码在函数入口累计，失败路径——截断恢复/
        // 失败态对齐——也凭空记账，面板 output_tokens 偏高）。 Bug C 缺参不下发
        // 已在调用方 `fail_bug_c_if_missing` 拦截，不会走到这里。
        self.output_tokens += (assembled.len() as i32 + 3) / 4;
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
            for (tool_use_id, mut assembled) in pending {
                if let Some(&idx) = self.tool_block_indices.get(&tool_use_id) {
                    // Bug C 在还原前校验 Kiro 形态（与 process_tool_use stop 同口径）。
                    // 截断兜底同样做参数还原：块 start 已用还原名（tool_use_names 记录，
                    // 且只记录映射过的），残留 input 需还原成客户端形态，否则名参错配
                    // （Write + {path,text}）。未映射的（tool_use_names 无记录）原样透传。
                    if !self.fail_bug_c_if_missing(idx, &assembled) {
                        if let Some(cname) = self.tool_use_names.get(&tool_use_id).cloned() {
                            if let Ok(value) =
                                serde_json::from_str::<serde_json::Value>(&assembled)
                            {
                                assembled = crate::anthropic::converter::map_tool_input_from_kiro(
                                    &cname,
                                    value,
                                )
                                .to_string();
                            }
                        }
                        events.extend(self.flush_tool_input(idx, assembled));
                    }
                    if let Some(stop_event) = self.state_manager.handle_content_block_stop(idx) {
                        events.push(stop_event);
                    }
                }
            }
        }

        // Flush thinking_buffer 中的剩余内容。
        //
        // ⚠️ 门不能只看 `!buffer.is_empty()`：扣留窗口改为「按标签语法判定」后，不可能是
        // 标签的思考内容会被**及时**下发，于是「`in_thinking_block=true` 而 buffer 为空」
        // 成了常态。只看 buffer 会让 thinking 块在收尾时**不被 stop**，而其后的 DSML /
        // reclaim 残留仍会 start 一个 text 块 ⇒ SSE 出现「新块 start → 旧块 stop」交错，
        // 违反 Anthropic「先 stop 当前块再 start 下一块」契约（CC 解析报错）。
        if self.thinking_enabled && (self.in_thinking_block || !self.thinking_buffer.is_empty()) {
            if self.in_thinking_block {
                // 末尾可能残留 `</thinking>`（例如紧跟 tool_use 或流结束），需要在 flush 时过滤掉结束标签。
                if let Some(end_m) = find_real_thinking_end_tag_at_buffer_end(&self.thinking_buffer)
                {
                    let thinking_content = self.thinking_buffer[..end_m.start].to_string();
                    if !thinking_content.is_empty() {
                        if let Some(thinking_index) = self.thinking_block_index {
                            // 同 :2531 口径：思考文本已在入口整块估算时计入，此处不重复累加。
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
                    let remaining = self.thinking_buffer[end_m.end()..].trim_start().to_string();
                    self.thinking_buffer.clear();
                    self.in_thinking_block = false;
                    self.thinking_extracted = true;
                    if !remaining.is_empty() {
                        events.extend(self.create_text_delta_events(&remaining));
                    }
                } else {
                    // 如果还在 thinking 块内，发送剩余内容作为 thinking_delta
                    // （buffer 可能为空 —— 内容已在流式期间及时下发，此处只负责收尾 stop）
                    if !self.thinking_buffer.is_empty() {
                        if let Some(thinking_index) = self.thinking_block_index {
                            // 同 :2531 口径：缓冲内容已在入口整块估算时计入，此处不重复累加。
                            events.push(self.create_thinking_delta_event(
                                thinking_index,
                                &self.thinking_buffer,
                            ));
                        }
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
                // 否则发送剩余内容 —— 走**统一出口** `emit_non_thinking_text`，不要直接
                // create_text_delta_events。
                //
                // 这里是 reclaim 旁路的最后一处：thinking 尚未出现时，
                // `process_content_with_thinking`(:1553) 会扣留 `"<thinking>".len()` = 10 字节
                // 当「可能是半标签」，而 `</invoke>` 只有 9 字节 —— 于是文本化 invoke 的**闭合标签**
                // 滞留在 thinking_buffer，只有前半截进了 invoke_sniff_buffer。若这里直接吐 text_delta，
                // 重组层拿到的永远是「未闭合的 `<invoke`」→ 当纯文本发给客户端 → **工具不执行**。
                // 线上实测 textifiedInvokeHits 17 : reclaimedInvokeCalls 1 即此。
                //
                // 并回 sniff 缓冲后，下方 :2466 的收尾 flush 会把完整块重组成 tool_use。
                //
                // 残留里可能有被扣留窗口按「可能是标签」扣住的**完整**孤立闭标签
                // （`</thinking>` 已到、`\n\n` 永远不会到了）。EOF 时它已确定是标记而非
                // 正文，原样下发即泄漏；判据复用 `find_stray_thinking_end_tag`。
                let buffer_content =
                    strip_stray_thinking_end_tags(&self.thinking_buffer).into_owned();
                if !buffer_content.is_empty() {
                    events.extend(self.emit_non_thinking_text(&buffer_content));
                }
            }
            self.thinking_buffer.clear();
        }

        // 客户端**没要** thinking 时剥离器（`strip_inline_thinking_when_disabled`）的残留收尾。
        //
        // ⚠️ 必须是独立分支，不能并进上面那个 `if`：那条的门是
        // `self.thinking_enabled && !buffer.is_empty()`，而剥离器**只在 `!thinking_enabled`
        // 时**往同一个 `thinking_buffer` 写 —— 两个条件互斥 ⇒ 那条 flush 对剥离器的残留
        // **永远不执行** ⇒ 残留静默蒸发。
        //
        // 两种残留都必须处理，否则都是**静默吞字**（面板记成功、客户端收到空/截断回答）：
        // ① `in_thinking_block=true`：思考块没闭合。丢思考本体，但标签**之后**的正文要下发
        //    （`</thinking>Answer` 这种零空白形态流式期间三个查找函数都不认，见
        //    `split_unclosed_thinking_residue_at_eof`）。
        // ② `in_thinking_block=false`：尾巴是被 `partial_tag_suffix_len` 扣住的「可能是半个
        //    `<thinking>`」。EOF 时它已确定**不是**标签（后续 chunk 不存在了），是正文的一部分
        //    —— 散文里一个孤立 `<`（"条件 a < b"）恰好落在流末尾就是这个形态。
        if !self.thinking_enabled && !self.thinking_buffer.is_empty() {
            let residue = std::mem::take(&mut self.thinking_buffer);
            let visible = if self.in_thinking_block {
                Cow::Borrowed(split_unclosed_thinking_residue_at_eof(&residue))
            } else {
                // 尾巴可能是被扣留窗口扣住的**完整**孤立闭标签（`\n\n` 永远不会到了）。
                // EOF 时它已确定是标记，原样下发即泄漏。
                strip_stray_thinking_end_tags(&residue)
            };
            self.in_thinking_block = false;
            if !visible.is_empty() {
                // 走统一出口：残留里可能含文本化 invoke 的尾巴，直接 create_text_delta_events
                // 会绕开重组层（与上面 :2595 那条注释记的是同一型缺陷）。
                let visible = visible.to_string();
                events.extend(self.emit_non_thinking_text(&visible));
            }
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
        // 收尾 flush tool_use XML 泄漏过滤:已确认在标签内的残留丢弃(泄漏);被误判为半前缀的
        // 正文残留补发(不吞字)。残留要按普通文本走统一出口 `emit_non_thinking_text` —— 直接
        // create_text_delta_events 会绕开 invoke 重组层(与上面 :2595 记的是同一型缺陷)。
        let tool_use_xml_residue = self.finish_tool_use_xml_filter();
        if !tool_use_xml_residue.is_empty() {
            self.output_tokens += estimate_tokens(&tool_use_xml_residue);
            events.extend(self.emit_non_thinking_text(&tool_use_xml_residue));
        }

        // ⭐ 空响应兜底（`!thinking_enabled`）：本轮**只有** reasoning、正文为空时，
        // 把攒下的 reasoning 降级成正文下发。
        //
        // 🔴 位置是判据的一部分，不能上移：上面三处 flush（thinking_buffer / dsml_tail /
        // invoke_sniff_buffer）都可能在此刻才吐出本轮唯一的正文 —— 整轮文本被 hold 到收尾
        // 是**已知常态**（行首未闭合的 `<invoke`、末尾孤立 `<`）。若把本分支排在 flush 之前，
        // `body_content_seen` 还是 false ⇒ 推理与正文**双份**下发，等于把内部推理泄漏给
        // 明确表示不想看它的用户。所以这里先按已产出的 events 补一次判定，再决定。
        //
        // 与下方「只有 thinking 块 → 补空格文本块」那条互斥（两条的门分别是
        // `!self.thinking_enabled` 与 `self.thinking_enabled`），谁在前都不影响结果；
        // 排这里只为紧跟它依赖的 flush。
        //
        // 🔴 第三道门 `completion.is_ok()`：**失败**的一轮不得被兜底塞进推理文本。
        // 本函数是所有收尾路径的公共出口，失败路径同样会走到这里：
        //   · 传输层读流 Err → `mark_transport_error` 后 `handlers.rs:1587` 直接调本函数；
        //   · decoder 永久停止 → `mark_decoder_stopped` 后由流末 `handlers.rs:1603` 调；
        //   · buffered 缓冲溢出 → `process_and_buffer` 内 `mark_decoder_stopped`（本文件
        //     :3455），收尾经 `finish_and_get_all_events` 调；
        //   · in-band Error/Exception 帧、工具参数真截断恢复（:2967）也都置了失败态。
        // 这些路径已各自补发了 SSE error 事件，客户端据此退避重试。若此时再补一段推理文本，
        // 客户端看到的是「error + 一段推理正文」，而记账侧仍按失败落库 —— 等于给一次失败
        // 凭空记上 output_tokens，且推理文本对客户端毫无用处（它本来就要重试整轮）。
        // 兜底的目标只有一个：避免**成功**的一轮返回空内容。
        self.observe_visible_body(&events);
        if !self.thinking_enabled && !self.body_content_seen && self.completion.is_ok() {
            let reasoning = std::mem::take(&mut self.discarded_reasoning);
            // 纯空白的 reasoning 降级下发没有意义（客户端看到的还是空），保持既有丢弃行为。
            if !reasoning.trim().is_empty() {
                tracing::warn!(
                    model = %self.model,
                    reasoning_bytes = reasoning.len(),
                    "本轮上游只吐结构化 reasoning、无正文；thinking 未开启故 reasoning 本应丢弃，\
                     但那会让客户端收到完全空的响应 —— 降级为正文下发以避免空响应"
                );
                let cleaned = self.sanitize_degraded_reasoning(&reasoning);
                // 清洗后可能只剩空白（整段都是泄漏词/标记）：那就退回丢弃，不发空 delta
                // （`create_text_delta_events` 自身无空串守卫，空 text_delta 只会让客户端
                // 多一个无意义的块）。
                if !cleaned.trim().is_empty() {
                    // 降级出来的文本**计入** output_tokens：它是真下发的内容，
                    // 而 `process_reasoning_content` 那条丢弃路径从不计数（正确，因为没发）。
                    // 计的是**清洗后**的量 —— 记账口径必须与实际下发的内容一致。
                    self.output_tokens += estimate_tokens(&cleaned);
                    // 刻意用 create_text_delta_events 而非 emit_non_thinking_text：
                    // ① 此刻 invoke_sniff_buffer 刚 flush 过，再往里塞就没人 drain 了（会静默吞）；
                    // ② 推理里出现 `<invoke …>` 是"模型在想要调什么工具"，绝不能被重组层
                    //    捞成真 tool_use —— 那是凭空执行工具（同一理由见
                    //    `process_assistant_response` 里剥离器排在 reclaim 之前那段注释）。
                    events.extend(self.create_text_delta_events(&cleaned));
                    // 本地产品选择：reasoning 降级成正文是完整一轮，不是截断。
                    if self.state_manager.stop_reason.is_none() {
                        self.state_manager.set_stop_reason("end_turn");
                    }
                }
            }
        }

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

        // 干净 EOF 有部分正文/未 stop 的 tool，却没有任何终止信号：不得发成功 end_turn。
        // 空响应当 `is_empty_response` 处理（completion 保持 Ok，由 handler 补 error）。
        // thinking-only / reasoning 降级已在上面显式 set_stop_reason，不会进本分支。
        if self.is_clean_eof_without_terminal() {
            events.extend(self.state_manager.close_open_blocks());
            if self.completion.is_ok() {
                self.completion = CompletionStatus::Incomplete {
                    message: "传输层干净结束，但缺少 stopReason 或 tool_use 终止信号".to_string(),
                };
            }
            if !self.error_event_emitted {
                events.push(SseEvent::error_event(
                    self.completion.sse_error_type(),
                    self.completion.client_message(),
                ));
                self.error_event_emitted = true;
            }
        } else {
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
        }

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
        // tool_use XML 泄漏剥离收尾（可观测，不改任何已发内容）：本请求真剥掉过泄漏标签就如实记一条。
        // 绝不黑箱 —— 剥离是"从用户可见正文里删内容"，必须留下能对账的痕迹（剥了多少字节）。
        // 与 textified_invoke_hits 是**两种不同标签形态**的取证：那套是 invoke/antml: 形态，
        // 这套是 `<tool_use …>` 形态（上游把工具调用当正文吐的另一种写法）。
        if self.tool_use_xml_stripped > 0 {
            tracing::warn!(
                model = %self.model,
                stripped_bytes = self.tool_use_xml_stripped,
                "上游把工具调用当正文吐出(<tool_use> XML 泄漏),已从文本流剥离 {} 字节——客户端不会看到泄漏标签",
                self.tool_use_xml_stripped,
            );
        }

        // TTFB 兜底：整轮文本被 hold 在 invoke_sniff_buffer / dsml_tail_buffer / thinking_buffer
        // 里、直到流末才 flush 的响应，其首个内容 delta 是在**本函数**产生的，不经过
        // process_kiro_event 的打点。不补这一处，这批响应的 first_token_ms 会永远是 NULL，
        // 等于把一个已知形态的缺陷藏进数据里。
        // 注：此时的数值 ≈ 整轮耗时（因为内容确实是到最后才吐出来的），语义正确但与
        // "TTFB 应该很小" 的直觉相反 —— 这反映的是真实用户体验，不是打点错误。
        self.mark_first_token_if_content(&events);

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
    /// 已缓冲事件的累计字节数（C4：内存上限守卫，超 [`MAX_BUFFERED_EVENT_BYTES`] 即停止缓冲）
    buffered_bytes: usize,
    /// 是否已因超出缓冲上限而截断（只标记一次，避免重复置失败态/重复告警）
    buffer_overflowed: bool,
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
        let inner = StreamContext::new_full(
            model,
            estimated_input_tokens,
            thinking_enabled,
            tool_name_map,
            known_tool_names,
        );
        Self {
            inner,
            event_buffer: Vec::new(),
            estimated_input_tokens,
            initial_events_generated: false,
            buffered_bytes: 0,
            buffer_overflowed: false,
        }
    }

    /// 估算一批事件的字节占用（event 名 + 序列化 data），用于缓冲上限累计。
    fn estimate_events_bytes(events: &[SseEvent]) -> usize {
        events
            .iter()
            .map(|e| e.event.len() + serde_json::to_string(&e.data).map(|s| s.len()).unwrap_or(0))
            .sum()
    }

    /// 是否已因缓冲超上限而截断（C4）。
    pub fn buffer_overflowed(&self) -> bool {
        self.buffer_overflowed
    }

    /// 返回本次请求解析出的最终用量（供用量统计埋点使用）
    /// 首个真实内容 delta 时刻（透传内部 StreamContext）。
    ///
    /// ⚠️ 语义：这是**上游首 token 到达网关**的时刻，**不是客户端看到的时刻** ——
    /// buffered 分发把整轮憋到流末才吐，客户端观测到的 TTFB ≈ latency_ms 总值。
    /// 两者的差正是 ccAutoBuffer 的代价，分开记录才能量化它。
    pub fn first_token_at(&self) -> Option<std::time::Instant> {
        self.inner.first_token_at()
    }

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

    /// 中断字节数（非流式路径恒 `None`：没有「SSE 流中途断流」概念，透传占位保持
    /// 与流式收尾埋点同模式，避免两处埋点代码分叉）。
    pub fn interrupted_bytes(&self) -> Option<u64> {
        None
    }

    /// 是否空/近似空响应（透传内部 StreamContext，供收尾补发 error 事件）。
    pub fn is_empty_response(&self) -> bool {
        self.inner.is_empty_response()
    }

    /// 空响应是否由「上下文过大」导致（透传）。
    pub fn empty_response_is_oversized_context(&self) -> bool {
        self.inner.empty_response_is_oversized_context()
    }

    /// 设置 prompt 缓存记账明细（前缀估算注入；在 process_and_buffer 之前调用）
    pub fn set_cache_usage(&mut self, cache_usage: Option<CacheUsageBreakdown>) {
        self.inner.set_cache_usage(cache_usage);
    }

    /// 转发给内部 `StreamContext`：buffered 只是把产出攒起来，工具校验逻辑与流式共用同一份。
    pub fn set_tool_required_fields(&mut self, required: HashMap<String, Vec<String>>) {
        self.inner.set_tool_required_fields(required);
    }

    /// 处理 Kiro 事件并缓冲结果
    ///
    /// 复用 StreamContext 的事件处理逻辑，但把结果缓存而不是立即发送。
    pub fn process_and_buffer(&mut self, event: &crate::kiro::model::events::Event) {
        // C4:已超缓冲上限 → 停止继续缓冲(丢弃后续事件),防 OOM。已置截断失败态,收尾按截断处理。
        if self.buffer_overflowed {
            return;
        }

        // 首次处理事件时，先生成初始事件（message_start 等）
        if !self.initial_events_generated {
            let initial_events = self.inner.generate_initial_events();
            self.buffered_bytes += Self::estimate_events_bytes(&initial_events);
            self.event_buffer.extend(initial_events);
            self.initial_events_generated = true;
        }

        // 处理事件并缓冲结果
        let events = self.inner.process_kiro_event(event);
        self.buffered_bytes += Self::estimate_events_bytes(&events);
        self.event_buffer.extend(events);

        // C4:累计字节超上限 → 按"响应截断"处置(复用 decoder_stopped 收尾:发 SSE error,
        // 不把半截缓冲当成功)。只置一次,后续事件在上面的 early-return 丢弃。
        if self.buffered_bytes > MAX_BUFFERED_EVENT_BYTES {
            self.buffer_overflowed = true;
            tracing::warn!(
                buffered_bytes = self.buffered_bytes,
                limit = MAX_BUFFERED_EVENT_BYTES,
                "缓冲流事件超出内存上限,按响应截断处置(停止继续缓冲,防 OOM)"
            );
            self.inner
                .mark_decoder_stopped("缓冲流事件超出内存上限(疑似异常超长响应)".to_string());
        }
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
        // ⚠️ 保持 billed **未缩放**，与 create_message_start_event / record 口径一致
        //（scale_for_client 只作用于 message_delta 的既有展示缩放）。
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

/// 密排文字：`is_leak_glue_char` 的汉字/全角/部首/标点，再加上假名与韩文。
/// 这些字符按 ~1.5 字/token 计；拉丁仍按 ~4 字/token。
fn is_cjk_dense_char(c: char) -> bool {
    if StreamContext::is_leak_glue_char(c) {
        return true;
    }
    matches!(
        c,
        '\u{3040}'..='\u{309F}' // Hiragana
            | '\u{30A0}'..='\u{30FF}' // Katakana
            | '\u{31F0}'..='\u{31FF}' // Katakana phonetic extensions
            | '\u{1100}'..='\u{11FF}' // Hangul Jamo
            | '\u{3130}'..='\u{318F}' // Hangul compatibility Jamo
            | '\u{AC00}'..='\u{D7AF}' // Hangul syllables
    )
}

/// 简单的 token 估算
fn estimate_tokens(text: &str) -> i32 {
    let mut chinese_count = 0;
    let mut other_count = 0;

    for c in text.chars() {
        if is_cjk_dense_char(c) {
            chinese_count += 1;
        } else {
            other_count += 1;
        }
    }

    // 中文/假名/韩文约 1.5 字符/token，英文约 4 字符/token
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

/// 【纯诊断·不进控制流】把归因为 `Malformed`(结构闭合+无非法字符但仍 parse 失败)的串**再细分**成
/// 可区分的子型,供日志定性「malformed 到底是哪种畸形」——这是解开「类型 A(上游模型帧本身吐坏,
/// 网关修不了)还是类型 C(我们合并逻辑把好帧拼坏,能修)」的钥匙:
///   - `glued`      : `}{` / `} {` 粘连(两个完整对象黏一起)——偏类型 C(合并/上游重写),`merge_tool_input`
///                     第 6 步本应消灭,若仍出现说明帧序列走了未覆盖分支,**优先怀疑我们侧**。
///   - `trailing_comma` : 尾逗号 `{"a":1,}` / `[1,2,]`——偏模型侧多吐一个逗号。
///   - `missing_comma`  : 缺分隔逗号 `{"a":1"b":2}`——偏模型侧漏吐逗号/上游丢了逗号帧。
///   - `expected_value` : 键后/数组位缺值 `{"a":}`——偏模型侧生成中断在值前。
///   - `expected_colon` : 键后缺冒号 `{"a" 1}`——偏模型侧。
///   - `key_not_string` : 键不是字符串 `{a:1}`——偏模型侧吐了裸键。
///   - `trailing_chars` : 首个完整值后还有多余字符(非 `}{` 粘连的其它尾随)——偏上游多发。
///   - `other`          : serde 消息未落入上述已知形态(留观测,收够样本再补分类)。
///
/// 判据源 = serde_json 的**官方错误 Display 消息**(稳定字符串,非自造启发式) + `scan.glued`(已算出)。
/// 只在 Malformed 分支调用(调用方保证 `from_str` 已失败),纯读不改内容。
fn malformed_subkind(s: &str) -> &'static str {
    // glued 优先:scan 已单遍算出 `}{` 粘连,语义比 serde 的 "trailing characters" 更精确。
    if scan_tool_json(s).glued {
        return "glued";
    }
    // 取 serde 官方错误消息做形态判据(Display 文本稳定,见 serde_json/src/error.rs ErrorCode::fmt)。
    let msg = match serde_json::from_str::<serde_json::Value>(s) {
        Ok(_) => return "other", // 理论不可达(调用方已确保非法),兜底不 panic。
        Err(e) => e.to_string(),
    };
    // 匹配官方消息里的稳定短语(全小写包含匹配,不依赖行列号)。
    if msg.contains("trailing comma") {
        "trailing_comma"
    } else if msg.contains("expected `:`") {
        "expected_colon"
    } else if msg.contains("expected `,` or `}`") || msg.contains("expected `,` or `]`") {
        // serde 在缺分隔逗号时报「expected `,` or `}`/`]`」——即两个值之间少了逗号。
        "missing_comma"
    } else if msg.contains("expected value") {
        "expected_value"
    } else if msg.contains("key must be a string") {
        "key_not_string"
    } else if msg.contains("trailing characters") {
        "trailing_chars"
    } else {
        "other"
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
    // 第三层：glued 粘连修复（`}{...}` 头部多余 `}` 来自上一个 JSON 对象泄漏）。
    // 仅在前两层均失败后触发，保守策略：剥离头部 `}` 及其后直到下一个 `{` 之间的垃圾字符，
    // 剩余部分必须能被 serde_json 解析为合法 JSON 才采用，否则放弃（不比前两层差）。
    if let Some(glued_fixed) = repair_json_glued(s) {
        return Some(glued_fixed);
    }
    None
}

/// glued 粘连修复：剥掉头部多余的 `}` 及其后到下一个 `{` 之间的任意字符。
///
/// 例：`}{\"path\": \"src/foo.rs\"}` → `{\"path\": \"src/foo.rs\"}`
///
/// 保守策略：
/// - 只在原串（trim 后）以 `}` 开头时尝试。
/// - 找到第一个 `{` 并截取子串，子串必须 serde_json 解析成功才返回，否则 `None`。
/// - 不修改原有字符级/结构级修复的结果，仅作为最后兜底。
fn repair_json_glued(s: &str) -> Option<String> {
    let trimmed = s.trim_start();
    if !trimmed.starts_with('}') {
        return None;
    }
    // 找到第一个 '{' 的位置（在原串中，而非 trim 后偏移，保持 slice 安全）。
    let brace_pos = s.find('{')?;
    let candidate = &s[brace_pos..];
    // 对候选串先做字符级修复再验证，与前两层保持一致的处理质量。
    let char_fixed = repair_json_char_level(candidate);
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&char_fixed) {
        return serde_json::to_string(&v).ok();
    }
    // 字符级修复后仍不合法，再尝试结构层补全（截断 + glued 同时出现的罕见情况）。
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


/// thinking 缓冲上限(review Finding 5):上游持续吐纯空白时,纯空白分支既不 emit 也不收缩会让
/// thinking_buffer 无界增长 OOM。超此上限即强制按普通文本吐出收缩。256KiB 远超正常 thinking 前导空白。
const MAX_THINKING_BUFFER_BYTES: usize = 262_144;

/// 缓冲流(BufferedStreamContext,/cc + CC auto-buffer)累计事件字节上限(C4 修复):
/// 缓冲模式把整段上游流收进内存再更正 message_start 的 input_tokens。无上限时超长流式工具
/// 参数 + 大 thinking(或异常上游持续推送)会让 event_buffer 无界增长直至 OOM(对比 thinking
/// 已有 256KiB 上限、decoder 16MB 上限)。64MiB 远超任何正常 Claude 响应(含大工具参数与长
/// thinking),超限即按"响应截断"处置(mark_decoder_stopped),复用既有截断→SSE error 收尾语义,
/// 不再继续吃内存。
const MAX_BUFFERED_EVENT_BYTES: usize = 64 * 1024 * 1024;


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
///   5. buf 已是完整合法 JSON，且以 frame 为前缀（frame 更短） → buf 不变（丢弃迟到的旧短快照）
///   6. buf 与 frame 各自都是完整合法 JSON → frame（非前缀重写，只留最新完整对象，消灭 `}{` 粘连）
///   7. 否则               → buf + frame 追加（真增量碎片，还原完整内容）
///
/// 注意第 6 步的前提是**两者各自都完整**：单个完整 JSON 对象无法再被增量扩展，因此第二个
/// 完整对象必然是重写而非续写；反之若 frame 仅是"看似完整"的内层片段（如 `{"inner":1}` 续在
/// `{"outer":` 之后），buf 尚不完整则不触发第 6 步，仍走第 7 步正确追加。
///
/// 第 5 步的 `buf 必须完整` 前置条件同理不可少：`buf.starts_with(frame)` 只是**字符串形状**
/// 判定，几乎所有正在拼接中的 JSON 对象 buf 都以 `{` 开头 —— 若 buf 尚不完整（例如
/// `{"outer":`），此时收到一个单独成帧的 `{`（如嵌套对象的开括号，甚至整个对象重发的开括号）
/// 会被误判成"比 frame 更全的缓冲"而丢弃这个 `{`，导致拼出来的串缺一个 `{`，轻则嵌套结构
/// 缺括号，重则客户端整段 `input` 解析失败退化成 `{}`。只有 buf **已经**是完整合法 JSON 时，
/// 后续再来的更短前缀帧才必然是迟到的旧快照，丢弃才安全。
/// `contextUsageEvent` 的百分比 → `input_tokens`。**无效信号返回 `None`（调用方必须不覆盖已有值）。**
///
/// # 为什么必须是一个共享函数，而不是两处各自判定
///
/// 这个判据有**两个**调用点：流式（`StreamContext::process_kiro_event`）与非流式
/// （`handlers.rs` 的缓冲聚合循环）。它们此前是两份独立实现，而只有流式那份有下界守卫 ——
/// 非流式那份直接 `pct * window / 100.0`，于是同一个上游异常在两条路径上表现不同：
/// 流式忽略脏值、非流式把 `input_tokens` 写成 0（或 NaN 饱和成的 `i32::MAX`）。
///
/// 本仓已多次踩「同一判据两份实现，只修了其中一份」这个形态（`endpoint_for` 与
/// `for_credentials`、`restart_fields` 与 reload restore 表、`cleanup_verdict` 与
/// `batch-delete`）。所以这里不是「给第二处也加个守卫」，而是**让两处物理上共用同一段代码**，
/// 使分叉在结构上不可能发生。源码守卫见 `context_usage_predicate_must_be_shared`。
///
/// # 判据本身
///
/// - `pct <= 0`：真实请求恒有 system prompt + 至少一条 user message，占用率**不可能**为 0。
///   而 `ContextUsageEvent.context_usage_percentage` 带 `#[serde(default)]` ⇒ 上游少发该字段 /
///   发 null 时它就是 `0.0`，与显式 0 逐字节不可分辨。两者都当「没给」处理：
///   错信 0 = 计费信号错且不可逆（账已出），丢一次真 0 只是退回本地估算（可逆）。
/// - `!pct.is_finite()`：NaN/inf 经 `as i32` 是**饱和**转换（NaN→0、inf→`i32::MAX`），后者更糟。
/// - 负值：会让 `billed_input_tokens` 拿到负数。
/// - 有效 pct 钳到 **[1, 200]**：`pct=1000` 会把计费 input 写成 10 倍窗口；超 200 记 warn 后按 200% 算。
pub(crate) fn context_input_tokens_from_pct(pct: f64, window_size: i32) -> Option<i32> {
    if pct > 0.0 && pct.is_finite() {
        if pct > 200.0 {
            tracing::warn!(
                pct,
                window_size,
                "contextUsageEvent percentage > 200; clamping to 200 to avoid 10x billed input_tokens"
            );
        }
        let pct = pct.clamp(1.0, 200.0);
        Some((pct * (window_size as f64) / 100.0) as i32)
    } else {
        None
    }
}

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
    // 5. 迟到的旧短快照：缓冲以本帧为前缀（本帧更短）且缓冲**已是完整合法 JSON**
    //    → 保留更全的缓冲，丢弃本帧。前置条件 `is_complete_json(buf)` 不可省：buf 尚在
    //    拼接中途时（如 `{"outer":`）若来一个单独成帧的 `{`，纯形状判定会把这个 `{` 误判为
    //    "迟到的旧短快照"而吞掉，导致最终串缺开头的 `{`（Invalid tool parameters 根因之一）。
    if buf.len() > frame.len() && buf.starts_with(frame) && is_complete_json(buf) {
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
#[path = "stream_usage_caliber_tests.rs"]
mod usage_caliber_tests;

#[cfg(test)]
#[path = "stream_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "stream_ported_k2cc_empty_response_tests.rs"]
mod ported_k2cc_empty_response_tests;

#[cfg(test)]
#[path = "stream_output_tokens_accounting_tests.rs"]
mod output_tokens_accounting_tests;
