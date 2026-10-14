//! 事件基础定义
//!
//! 定义事件类型枚举、trait 和统一事件结构

use crate::kiro::parser::error::{ParseError, ParseResult};
use crate::kiro::parser::frame::Frame;

/// 已告警过的未识别事件类型名（进程级去重，仅用于日志抑制）。
///
/// 为什么需要：`reasoningContentEvent` 是上游的结构化 thinking 增量流，一次带思考的
/// 响应就有几十帧。逐帧 warn 实测在 30 分钟内刷出 22939 条、占全部日志 91.5%，
/// 把真实错误全部淹没。按类型去重后每种只告警一次，既能发现上游新增的事件类型，
/// 又不会刷屏。
///
/// 用 `Mutex<HashSet>` 而非无锁结构：这条路径每流仅在**首次**遇到新类型时写一次，
/// 其余都是读命中，竞争极低。
///
/// ⚠️ 集合必须有界：`:event-type` 是**上游可控**的字符串 header（wire 上限 64KiB），
/// 而本集合是进程级、只增不减 —— 无界的话「一串互不相同的类型名」就是一条慢性内存泄漏
/// （每个键最长 64KiB 且终身驻留）。上界见 [`MAX_TRACKED_UNKNOWN_TYPES`]。
static SEEN_UNKNOWN_EVENTS: std::sync::Mutex<Option<std::collections::HashSet<String>>> =
    std::sync::Mutex::new(None);

/// 去重表最多记多少个类型名。已知事件类型是个位数，64 对「发现上游新增类型」这个诊断
/// 目的远远够用；到顶后不再新增（也不再多告警，退化为 debug 级）。
const MAX_TRACKED_UNKNOWN_TYPES: usize = 64;

/// 入表的类型名长度上限。真实事件类型名都是短标识符；超长的只可能是损坏/恶意帧，
/// 不值得为它留 64KiB 常驻内存。
const MAX_TRACKED_EVENT_TYPE_LEN: usize = 128;

/// 「帧缺 `:message-type`」只告警一次（进程级）。
///
/// 为什么不能逐帧 warn：这个头要么一直有、要么一直缺，缺失时逐帧告警会重演
/// [`SEEN_UNKNOWN_EVENTS`] 注释里那场 22939 条刷屏。用独立的 `AtomicBool` 而不是
/// 复用去重表：那张表按事件类型名键控，塞一个哨兵键进去会污染「上游新增事件类型」
/// 这个诊断信号（该表的上界只有 64 项）。
static MISSING_MESSAGE_TYPE_WARNED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// 该未识别事件类型是否是**首次**出现（首次返回 true 并登记）。
///
/// 锁被毒化（另一线程持锁时 panic）时取 `into_inner` 继续用：这只是日志抑制，
/// 宁可多打几条日志，绝不能因为它让事件解析这条热路径失败。
fn first_sighting_of_unknown_event(event_type: &str) -> bool {
    let mut guard = match SEEN_UNKNOWN_EVENTS.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    let seen = guard.get_or_insert_with(std::collections::HashSet::new);
    first_sighting_in(seen, event_type)
}

/// 判定与登记的实体（集合由调用方提供）。
///
/// 为什么把集合抽成参数：进程级去重表被多个测试共享，往里灌满条目会**静默改变**其它
/// 测试的期望（它们各自用专属类型名假设「首次出现 = true」）。有了这个入口，上界行为
/// 可以用局部集合测，不碰全局状态。
fn first_sighting_in(
    seen: &mut std::collections::HashSet<String>,
    event_type: &str,
) -> bool {
    // 超长类型名不入表（见 MAX_TRACKED_EVENT_TYPE_LEN）：既不占常驻内存，也不告警。
    if event_type.len() > MAX_TRACKED_EVENT_TYPE_LEN {
        return false;
    }
    // 先 `contains` 再 `insert`：`insert` 要 owned key，会在**持锁期间**为每个已知的
    // 未识别类型再分配一次 `String`（本函数按帧调用）。`HashSet<String>` 可用
    // `Borrow<str>` 直接查，命中路径因此零分配、锁持有时间也更短。
    if seen.contains(event_type) || seen.len() >= MAX_TRACKED_UNKNOWN_TYPES {
        return false;
    }
    seen.insert(event_type.to_string());
    true
}

/// 事件类型枚举
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EventType {
    /// 助手响应事件
    AssistantResponse,
    /// 工具使用事件
    ToolUse,
    /// 计费事件
    Metering,
    /// 上下文使用率事件
    ContextUsage,
    /// 结构化思考增量事件（上游的 thinking 流，见 `reasoning.rs` 的模块文档）
    ReasoningContent,
    /// 流末元数据（payload 只有 stopReason，见 `metadata.rs`）
    Metadata,
    /// 未知事件类型
    Unknown,
}

impl EventType {
    /// 从事件类型字符串解析
    pub fn from_str(s: &str) -> Self {
        match s {
            "assistantResponseEvent" => Self::AssistantResponse,
            "toolUseEvent" => Self::ToolUse,
            "meteringEvent" => Self::Metering,
            "contextUsageEvent" => Self::ContextUsage,
            "reasoningContentEvent" => Self::ReasoningContent,
            "metadataEvent" => Self::Metadata,
            // 别名：上游历史版本用过长名字 `messageMetadataEvent`。
            // 为什么可以当同一个变体：对手实现（wnddd839/kiro.cache 的
            // `internal/kiro/stream.go`，`frame()` 里）写的是
            // `case "metadataEvent", "messageMetadataEvent":` —— 两个名字进**同一个分支**，
            // 取的是同一组字段（stopReason / tokenUsage），语义无差别，只是 wire 版本差异。
            // 不认它就会落 `Unknown` 被静默丢弃：`stopReason` 丢掉后干净 EOF 只能靠本地推断
            // （见 `metadata.rs` 模块注释），与「别再丢终止信号」的方向相反。
            // 别名命中会打一条 debug（见 `parse_event`），便于事后统计上游在用哪个名字。
            "messageMetadataEvent" => Self::Metadata,
            _ => Self::Unknown,
        }
    }

    /// 转换为事件类型字符串
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::AssistantResponse => "assistantResponseEvent",
            Self::ToolUse => "toolUseEvent",
            Self::Metering => "meteringEvent",
            Self::ContextUsage => "contextUsageEvent",
            Self::ReasoningContent => "reasoningContentEvent",
            Self::Metadata => "metadataEvent",
            Self::Unknown => "unknown",
        }
    }
}

impl std::fmt::Display for EventType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// 事件 payload trait
///
/// 所有具体事件类型都需要实现此 trait
pub trait EventPayload: Sized {
    /// 从帧解析事件负载
    fn from_frame(frame: &Frame) -> ParseResult<Self>;
}

/// 统一事件枚举
///
/// 封装所有可能的事件类型
#[derive(Debug, Clone)]
pub enum Event {
    /// 助手响应
    AssistantResponse(super::AssistantResponseEvent),
    /// 工具使用
    ToolUse(super::ToolUseEvent),
    /// 计费
    Metering(super::MeteringEvent),
    /// 上下文使用率
    ContextUsage(super::ContextUsageEvent),
    /// 结构化思考增量（上游 thinking 流；此前被当 Unknown 丢弃，见 E1）
    ReasoningContent(super::ReasoningContentEvent),
    /// 流末元数据（stopReason；此前被当 Unknown 丢弃）
    Metadata(super::MetadataEvent),
    /// 未知事件（**只保留分类结果，刻意不保留 payload 与类型名**）。
    ///
    /// 未知帧的信息只用于诊断，已在 `parse_event` 里按类型去重、截断后记一条日志
    /// （那是发现「上游新增事件类型」的唯一通道）。把整帧（上限 16MiB）搬进枚举会为一条
    /// 诊断信息付出每帧一次的大块内存代价，故不保留；`as_str()` 也恒为 `"unknown"`。
    Unknown {},
    /// 服务端错误
    Error {
        /// 错误代码
        error_code: String,
        /// 错误消息
        error_message: String,
    },
    /// 服务端异常
    Exception {
        /// 异常类型
        exception_type: String,
        /// 异常消息
        message: String,
    },
}

impl Event {
    /// 从帧解析事件
    pub fn from_frame(frame: Frame) -> ParseResult<Self> {
        // `:message-type` 缺失仍按 `event` 处理（宽松默认），但必须**可观测**：
        // 该头是 AWS Event Stream 的必需路由头，缺了它 error/exception 帧会被送进
        // `parse_event`；而那条路只在「连 `:event-type` 也缺」时才落 `Unknown`，
        // 且 `Unknown` 的 warn 按类型名（恒为 "unknown"）去重 —— 第一个之后只记 debug，
        // 持续缺失就看不见了。
        //
        // 为什么不判成 Err：Err 的处理是**跳过整帧**（`handlers_dispatch::decode_frames_into`），
        // toolUseEvent 还会被当成响应截断直接失败重试。只有路由头缺失、payload 完全可用的帧
        // 在真实 wire 上还没见过，先把异常变成可观测，等抓到样本再决定是否收紧。
        let message_type = match frame.message_type() {
            Some(mt) => mt,
            None => {
                if !MISSING_MESSAGE_TYPE_WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                    tracing::warn!(
                        event_type = ?frame.event_type(),
                        "帧缺少 :message-type（AWS Event Stream 必需路由头），已按 event 处理"
                    );
                }
                "event"
            }
        };

        match message_type {
            "event" => {
                // 事件名级错误：上游也可以**不用** `:message-type: error`，而把错误类型名
                // 直接写进 `:event-type`（对手实现 wnddd839/kiro.cache 的 `frame()` 里
                // `case "error", "throttlingError", "validationError", ...` 就是这一形态，
                // 名单见 [`is_event_name_error`]）。这些帧的 `:message-type` 仍是 `event`；
                // 不在此分流就会落 `EventType::Unknown` 被静默丢弃（进程内仅一条 warn）——
                // 一次限流 / 校验失败在上游眼里是错误，我们却照常收尾成 200，客户端把半截
                // 响应当成正常结束。
                //
                // 产出复用 `Event::Error`（即 `:message-type: error` 的产出）：下游
                // `stream.rs` / `handlers.rs` 的错误链（失败态 + 内联 SSE error + 429/502
                // 分类）原样生效，不新造平行链路。
                let named_error = match frame.event_type() {
                    Some(et) if is_event_name_error(et) => Some(et.to_string()),
                    _ => None,
                };
                match named_error {
                    Some(error_code) => Self::parse_named_error(frame, error_code),
                    None => Self::parse_event(frame),
                }
            }
            "error" => Self::parse_error(frame),
            "exception" => Self::parse_exception(frame),
            other => Err(ParseError::InvalidMessageType(other.to_string())),
        }
    }

    /// 解析事件类型消息
    fn parse_event(frame: Frame) -> ParseResult<Self> {
        let event_type_str = frame.event_type().unwrap_or("unknown");
        let event_type = EventType::from_str(event_type_str);

        // 别名命中必须**可观测**：否则「上游到底用哪个名字」只能靠抓包才知道，事后统计
        // （`messageMetadataEvent` 的出现率 / 何时可淘汰别名）无从谈起。
        // debug 级——正常流量不落盘，需要时开
        // `RUST_LOG=info,kirostudio::kiro::model::events=debug`。
        if event_type == EventType::Metadata && event_type_str != "metadataEvent" {
            tracing::debug!(
                event_type = %event_type_str,
                "metadataEvent 别名命中（上游使用了非标准事件名）"
            );
        }

        match event_type {
            EventType::AssistantResponse => {
                let payload = super::AssistantResponseEvent::from_frame(&frame)?;
                Ok(Self::AssistantResponse(payload))
            }
            EventType::ToolUse => {
                let payload = super::ToolUseEvent::from_frame(&frame)?;
                Ok(Self::ToolUse(payload))
            }
            EventType::Metering => {
                let payload = super::MeteringEvent::from_frame(&frame)?;
                Ok(Self::Metering(payload))
            }
            EventType::ContextUsage => {
                let payload = super::ContextUsageEvent::from_frame(&frame)?;
                Ok(Self::ContextUsage(payload))
            }
            EventType::ReasoningContent => {
                let payload = super::ReasoningContentEvent::from_frame(&frame)?;
                Ok(Self::ReasoningContent(payload))
            }
            EventType::Metadata => {
                let payload = super::MetadataEvent::from_frame(&frame)?;
                Ok(Self::Metadata(payload))
            }
            EventType::Unknown => {
                // 未知事件类型此前被静默丢弃——连类型名都不留，导致上游新增的事件
                // 我们永远看不见。已知的具体损失：AWS 官方 amazon-q-developer-cli 的
                // Smithy 客户端里有 `MetadataEvent.tokenUsage`，含
                // uncachedInputTokens / cacheReadInputTokens / cacheWriteInputTokens。
                // metadata 帧已单独分类（payload 只有 stopReason，见 metadata.rs /
                // docs/CACHE-EXP0-RESULT.md，已删；全文可用 git show f6aaf88^ 取回；结论现由
                // cache_truth.rs 的 C-2 观测面持续背书）。本分支只剩真正未识别的类型。
                //
                // ⚠️ **每种类型只 warn 一次**（进程内按类型去重）。
                // 原实现对**每一帧**都 warn，而 `reasoningContentEvent` 是上游的结构化
                // thinking 增量流——每次带思考的响应就有几十帧。实测线上 30 分钟内刷出
                // 22939 条，占全部日志的 91.5%：日志被彻底淹没，真实错误无从查找，
                // 面板实时日志也因此卡顿。
                //
                // 注释原本写的意图就是「每个新类型值得知道一次」，只是实现没做去重。
                // 现在按类型去重后既保留了发现新事件类型的能力，又不会刷屏。
                // 需要看每一帧时开 debug：
                //   RUST_LOG=info,kirostudio::kiro::model::events=debug
                const MAX_UNKNOWN_PAYLOAD_LOG: usize = 512;
                // 只解码样本需要的字节（512 字符 × 最多 4 字节/字符），不为一行日志
                // 分配整帧（上限 16MiB）。
                let payload = frame.payload_as_str_truncated(MAX_UNKNOWN_PAYLOAD_LOG * 4);
                let truncated: String = payload.chars().take(MAX_UNKNOWN_PAYLOAD_LOG).collect();

                if first_sighting_of_unknown_event(event_type_str) {
                    tracing::warn!(
                        event_type = %event_type_str,
                        payload_bytes = frame.payload.len(),
                        payload_sample = %truncated,
                        "上游返回了未识别的事件类型（已忽略，不影响本次响应；\
                         同类型后续帧只记 debug，不再重复告警）"
                    );
                } else {
                    // 同类型的后续帧：debug 级，正常流量下不落盘。
                    tracing::debug!(
                        event_type = %event_type_str,
                        payload = %truncated,
                        "未识别事件的 payload（截断至 {} 字符）",
                        MAX_UNKNOWN_PAYLOAD_LOG
                    );
                }
                Ok(Self::Unknown {})
            }
        }
    }

    /// 解析错误类型消息
    fn parse_error(frame: Frame) -> ParseResult<Self> {
        let error_code = frame
            .headers
            .error_code()
            .unwrap_or("UnknownError")
            .to_string();
        let error_message = frame.payload_as_str();

        Ok(Self::Error {
            error_code,
            error_message,
        })
    }

    /// 解析异常类型消息
    fn parse_exception(frame: Frame) -> ParseResult<Self> {
        let exception_type = frame
            .headers
            .exception_type()
            .unwrap_or("UnknownException")
            .to_string();
        let message = frame.payload_as_str();

        Ok(Self::Exception {
            exception_type,
            message,
        })
    }

    /// 解析「事件名级错误」消息（错误类型名写在 `:event-type` 里，见 [`is_event_name_error`]）。
    ///
    /// 产出与 [`Self::parse_error`] **同一个 `Event::Error` 变体**：下游只认这一个变体，
    /// 失败态、内联 SSE error、429/502 分类（`is_rate_limit_signal` 认 `throttlingError`
    /// 里的 `throttl`）全部沿用既有链路，本函数只负责把代码与文本填对。
    ///
    /// `error_code` 取事件名本身（对手实现 `exception()` 同样用事件名当 kind），
    /// 文本的回退顺序见 [`extract_error_text`]。
    fn parse_named_error(frame: Frame, error_code: String) -> ParseResult<Self> {
        let error_message = extract_error_text(&frame, &error_code);
        Ok(Self::Error {
            error_code,
            error_message,
        })
    }
}

/// 判定 `:event-type` 是否是**事件名级错误**（错误类型名直接写在事件名里，而不是
/// `:message-type: error` + `:error-code`）。
///
/// 名单对齐对手实现 wnddd839/kiro.cache 的 `frame()`：
/// `case "error", "throttlingError", "validationError", "serviceUnavailableError",
/// "internalServerException":` —— 这里取其中四个「名字自带语义」的。
///
/// 裸 `error` **刻意未收**（real-deferred）：它在 AWS wire 上与 `:message-type: error`
/// 同形，真会以 `:event-type` 形态出现与否尚无实测样本；本仓也没有「事件名恰好叫 error」
/// 的正常事件。等抓到样本再收，避免为一个猜测增加一条正常帧被当错误的路径。
///
/// **精确匹配**：不做大小写归一、不做前缀/子串模糊。容忍上游别名不能变成模糊匹配——
/// 真的出现 `throttlingErrors` 这类新名字时，落 `Unknown` + 一次 warn 才是正确的信号
/// （它可能根本不是错误，猜错会把一次正常响应变成 502/429）。
fn is_event_name_error(event_type: &str) -> bool {
    matches!(
        event_type,
        "throttlingError"
            | "validationError"
            | "serviceUnavailableError"
            | "internalServerException"
    )
}

/// 错误事件 payload 里的可读文本字段。
///
/// 字段名对齐上游 wire：`message` 是 Kiro 错误体的标准字段（实测样本见
/// `handlers_tests.rs` 的 `__type`/`message`/`reason` 三元组），`Message` 是 AWS 风格
/// 首字母大写变体，`reason` 常与 message 同时出现（如 `USER_REQUEST_RATE_EXCEEDED`）。
/// 取字段口径与对手实现 `exception()`（message → Message → 原始 payload，再拼 reason）一致。
///
/// 未知键（如 `__type`）一律忽略：`__type` 是 `:error-code` 的重复信息，不是文本。
#[derive(Debug, Default, serde::Deserialize)]
struct ErrorPayloadText {
    #[serde(default)]
    message: Option<String>,
    #[serde(default, rename = "Message")]
    message_upper: Option<String>,
    #[serde(default)]
    reason: Option<String>,
}

/// 从错误帧提取可读文本：`message` → `Message`（+ 可选 `(reason)` 后缀）→ 原始 payload
/// → `fallback`（调用方传事件名）。
///
/// 为什么不报错：这条路径存在的理由就是「别把错误丢掉」，为一个畸形 payload 把整帧变成
/// `Err` 只会把可观测的错误变成不可观测的整帧跳过（`decode_frames_into` 对 Err 的处理）。
/// 为什么逐级回退而不是只取 `message`：拿不到字段时至少要留下「上游说了什么」或
/// 「是哪种错误」——空文本会让客户端错误体退化成 `上游返回错误: throttlingError - `。
fn extract_error_text(frame: &Frame, fallback: &str) -> String {
    if let Ok(fields) = frame.payload_as_json::<ErrorPayloadText>() {
        let base = fields
            .message
            .filter(|s| !s.trim().is_empty())
            .or(fields.message_upper.filter(|s| !s.trim().is_empty()));
        let reason = fields.reason.filter(|s| !s.trim().is_empty());
        return match (base, reason) {
            (Some(base), Some(reason)) => format!("{base} ({reason})"),
            (Some(base), None) => base,
            // 只有 reason：直接用它，不拼成 " (REASON)" 这种前导空格残句。
            (None, Some(reason)) => reason,
            (None, None) => raw_payload_text(frame, fallback),
        };
    }
    // 非 JSON（或类型不符）：回退到原始 payload。
    raw_payload_text(frame, fallback)
}

/// 原始 payload 文本（trim 后）；拿不到可读内容时回退 `fallback`。
///
/// 「可读」判据是**至少含一个字母/数字**（Unicode 口径，中文也算）：空 payload、纯空白、
/// 纯标点、以及坏帧里那种 `\u{FFFD}` 残片都不算文本，此时用事件名更有信息量。
fn raw_payload_text(frame: &Frame, fallback: &str) -> String {
    let raw = frame.payload_as_str();
    let trimmed = raw.trim();
    if trimmed.chars().any(char::is_alphanumeric) {
        trimmed.to_string()
    } else {
        fallback.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kiro::parser::crc::crc32;
    use crate::kiro::parser::frame::{parse_frame, PRELUDE_SIZE};

    #[test]
    fn test_event_type_from_str() {
        assert_eq!(
            EventType::from_str("assistantResponseEvent"),
            EventType::AssistantResponse
        );
        assert_eq!(EventType::from_str("toolUseEvent"), EventType::ToolUse);
        assert_eq!(EventType::from_str("meteringEvent"), EventType::Metering);
        assert_eq!(
            EventType::from_str("contextUsageEvent"),
            EventType::ContextUsage
        );
        // E1：结构化 thinking 流必须被识别，绝不能再落 Unknown（落 Unknown = payload 被丢弃）。
        // 这条断言是 E1 的**接线守卫**：`process_kiro_event` 那几个测试直接构造
        // `Event::ReasoningContent`，绕过了 from_str，所以只有这里能抓住"变体加了但没接线"。
        assert_eq!(
            EventType::from_str("reasoningContentEvent"),
            EventType::ReasoningContent,
            "reasoningContentEvent 必须被识别为结构化思考流（落 Unknown 会让 payload 被丢弃）"
        );
        assert_eq!(
            EventType::from_str("metadataEvent"),
            EventType::Metadata,
            "metadataEvent 必须被识别（落 Unknown 会丢掉 stopReason）"
        );
        assert_eq!(EventType::from_str("unknown_type"), EventType::Unknown);
    }

    #[test]
    fn test_event_type_as_str() {
        assert_eq!(
            EventType::AssistantResponse.as_str(),
            "assistantResponseEvent"
        );
        assert_eq!(EventType::ToolUse.as_str(), "toolUseEvent");
        assert_eq!(EventType::Metadata.as_str(), "metadataEvent");
    }

    /// 接线守卫：from_str / parse_event 必须把该事件类型接到 Metadata 变体。
    ///
    /// 原测试锁定 Unknown（只加可观测性）。现已接入解析，反向锁定分类，
    /// 避免再掉回 Unknown 把 stopReason 丢掉。
    /// 未识别事件的告警必须**按类型去重**：同一类型只告警一次。
    ///
    /// 回归背景：原实现逐帧 warn，而 reasoningContentEvent 是结构化 thinking 增量流，
    /// 每次带思考的响应几十帧。实测线上 30 分钟刷出 22939 条、占全部日志 91.5%，
    /// 真实错误被完全淹没，面板实时日志也因此卡顿。
    #[test]
    fn should_warn_once_per_unknown_event_type() {
        // 用本测试专属的类型名，避免与其它测试/真实流量共享的进程级集合互相干扰
        let t1 = "testOnlyEventAlpha";
        let t2 = "testOnlyEventBeta";

        assert!(
            first_sighting_of_unknown_event(t1),
            "首次出现应告警（返回 true）"
        );
        for i in 0..50 {
            assert!(
                !first_sighting_of_unknown_event(t1),
                "同类型第 {} 次重复出现不应再告警",
                i + 2
            );
        }
        // 不同类型互不影响：新类型仍应告警一次
        assert!(
            first_sighting_of_unknown_event(t2),
            "另一个新类型应独立告警一次"
        );
        assert!(!first_sighting_of_unknown_event(t2));
    }

    /// 去重表必须有界：`:event-type` 上游可控（wire 上限 64KiB），无界的只增集合 = 慢性
    /// 内存泄漏。用**局部集合**测上界，不碰进程级表（见 `first_sighting_in` 的注释）。
    #[test]
    fn unknown_event_dedup_is_bounded() {
        let mut seen = std::collections::HashSet::new();
        for i in 0..MAX_TRACKED_UNKNOWN_TYPES {
            assert!(
                first_sighting_in(&mut seen, &format!("boundedType{i}")),
                "上界内第 {i} 个新类型应告警一次"
            );
        }
        assert_eq!(seen.len(), MAX_TRACKED_UNKNOWN_TYPES);
        // 到顶后既不登记也不再告警（回退即 FAIL：集合会继续无界增长）。
        assert!(!first_sighting_in(&mut seen, "boundedTypeOverflow"));
        assert_eq!(
            seen.len(),
            MAX_TRACKED_UNKNOWN_TYPES,
            "到顶后集合大小不得再增长"
        );

        // 超长类型名不入表（回退即 FAIL：一个 64KiB 的键会常驻进程内存）。
        let mut fresh = std::collections::HashSet::new();
        let long = "x".repeat(MAX_TRACKED_EVENT_TYPE_LEN + 1);
        assert!(!first_sighting_in(&mut fresh, &long));
        assert!(fresh.is_empty(), "超长类型名不得入表");
    }

    #[test]
    fn test_metadata_event_is_classified() {
        assert_eq!(
            EventType::from_str("metadataEvent"),
            EventType::Metadata,
            "若解析被拆掉，本断言会红；请同时核对 parse_event 的 Metadata 分支"
        );
    }

    #[test]
    fn metadata_event_parse_branch_is_wired() {
        let src = include_str!("base.rs");
        let prod = src
            .split_once("\n#[cfg(test)]")
            .map(|(p, _)| p)
            .unwrap_or(src);
        let prod: String = prod
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        let from_str_arm = format!("\"{}{}\" => Self::{}", "metadata", "Event", "Metadata");
        let parse_arm = format!("{}::{}", "EventType", "Metadata");
        assert!(
            prod.contains(&from_str_arm),
            "from_str 必须把该事件类型接到 Metadata"
        );
        assert!(
            prod.contains(&parse_arm),
            "parse_event 必须有 Metadata 分支（变体加了但没接线会把 payload 丢掉）"
        );
    }

    /// 别名 `messageMetadataEvent` 必须与 `metadataEvent` 解析成同一形态。
    ///
    /// 为什么容忍：对手实现（wnddd839/kiro.cache 的 `frame()`）里
    /// `case "metadataEvent", "messageMetadataEvent":` 是同一个分支（同取 stopReason /
    /// tokenUsage），两个名字语义无差别，只是上游版本差异。
    ///
    /// FAIL 条件（两条都要能红）：别名落 Unknown → 第一个断言红；别名接上了但 payload
    /// 没被真解析 → 后两个断言红（只断言「不是 Unknown」不足以发现 payload 被丢）。
    #[test]
    fn message_metadata_alias_parses_like_metadata_event() {
        assert_eq!(
            EventType::from_str("messageMetadataEvent"),
            EventType::Metadata,
            "别名必须解析成 Metadata（落 Unknown 会丢掉 stopReason）"
        );
        assert_eq!(
            EventType::Metadata.as_str(),
            "metadataEvent",
            "别名不得改变规范名（as_str 是日志/统计口径）"
        );

        let raw = build_frame(
            &[
                (":message-type", "event"),
                (":event-type", "messageMetadataEvent"),
            ],
            br#"{"stopReason":"max_tokens","tokenUsage":{"cacheReadInputTokens":1234,"uncachedInputTokens":7}}"#,
        );
        let (frame, _) = parse_frame(&raw).expect("parse_frame").expect("整帧");
        match Event::from_frame(frame).expect("别名帧不该失败") {
            Event::Metadata(m) => {
                assert_eq!(
                    m.stop_reason.as_deref(),
                    Some("max_tokens"),
                    "别名帧的 stopReason 必须真的解析出来（只判变体类型不够）"
                );
                let usage = m.token_usage.expect("tokenUsage 必须被解析出来");
                assert_eq!(usage.cache_read_input_tokens, Some(1234));
                assert_eq!(usage.uncached_input_tokens, Some(7));
            }
            other => panic!("messageMetadataEvent 应解析为 Metadata，实际 {other:?}"),
        }
    }

    /// 4 个「事件名级错误」必须进入错误路径（`Event::Error`），且文本可读。
    ///
    /// 为什么是错误路径而不是 Unknown：这些名字自带「这是错误」的语义，落 Unknown 会被
    /// 静默丢弃（仅一条 warn），一次限流 / 校验失败就被照常收尾成 200。
    ///
    /// FAIL 条件：任一名字落 Unknown → 对应断言红；错误代码没取事件名、或文本没从
    /// `message` / `Message` / `reason` 取出来 → 对应断言红。
    #[test]
    fn event_name_level_errors_take_error_path() {
        // (事件名, payload, 期望错误文本)
        let cases: [(&str, &str, &str); 4] = [
            (
                "throttlingError",
                r#"{"message":"Too many requests, please wait before trying again.","reason":"USER_REQUEST_RATE_EXCEEDED"}"#,
                "Too many requests, please wait before trying again. (USER_REQUEST_RATE_EXCEEDED)",
            ),
            (
                "validationError",
                r#"{"message":"The provided input is not valid."}"#,
                "The provided input is not valid.",
            ),
            (
                // AWS 风格首字母大写变体字段
                "serviceUnavailableError",
                r#"{"Message":"Service is temporarily unavailable."}"#,
                "Service is temporarily unavailable.",
            ),
            (
                // 真实错误体形态（与 handlers_tests 里的样本同形，多一个 __type 未知键）
                "internalServerException",
                r#"{"__type":"com.amazon.aws.codewhisperer#InternalServerException","message":"Encountered an unexpected error."}"#,
                "Encountered an unexpected error.",
            ),
        ];

        for (event_name, payload, expected_text) in cases {
            let raw = build_frame(
                &[(":message-type", "event"), (":event-type", event_name)],
                payload.as_bytes(),
            );
            let (frame, _) = parse_frame(&raw).expect("parse_frame").expect("整帧");
            match Event::from_frame(frame).expect("事件名级错误帧不该失败") {
                Event::Error {
                    error_code,
                    error_message,
                } => {
                    assert_eq!(error_code, event_name, "错误代码应取事件名本身");
                    assert_eq!(
                        error_message, expected_text,
                        "{event_name} 的错误文本必须可读且取自 payload 字段"
                    );
                }
                other => panic!("{event_name} 应进入错误路径，实际 {other:?}"),
            }
        }
    }

    /// 事件名级错误的文本回退链：`message`/`Message` → 原始 payload → 事件名本身；
    /// 缺 `:message-type` 时也按事件名路由（宽松默认与既有行为一致）。
    ///
    /// FAIL 条件：拿不到字段时返回空串（客户端错误体退化成
    /// `上游返回错误: throttlingError - `，等于没信息）或返回 Err（整帧被跳过 = 错误不可观测）。
    #[test]
    fn event_name_level_error_text_falls_back() {
        // (事件名, payload, 期望错误文本)
        let cases: [(&str, &str, &str); 3] = [
            // ① 非 JSON：回退到原始 payload（trim 后）
            ("throttlingError", "slow down\n", "slow down"),
            // ② 空 payload：回退到事件名本身
            ("serviceUnavailableError", "", "serviceUnavailableError"),
            // ③ 只有 reason：单独成文，不拼成带前导空格的 " (REASON)"
            (
                "validationError",
                r#"{"reason":"INVALID_MODEL_ID"}"#,
                "INVALID_MODEL_ID",
            ),
        ];

        for (event_name, payload, expected_text) in cases {
            let raw = build_frame(
                &[(":message-type", "event"), (":event-type", event_name)],
                payload.as_bytes(),
            );
            let (frame, _) = parse_frame(&raw).expect("parse_frame").expect("整帧");
            match Event::from_frame(frame).expect("事件名级错误帧不该失败") {
                Event::Error {
                    error_code,
                    error_message,
                } => {
                    assert_eq!(error_code, event_name);
                    assert_eq!(error_message, expected_text);
                }
                other => panic!("{event_name} 应进入错误路径，实际 {other:?}"),
            }
        }

        // ④ 缺 :message-type 的帧：仍按事件名进入错误路径
        let raw = build_frame(
            &[(":event-type", "internalServerException")],
            br#"{"message":"boom"}"#,
        );
        let (frame, _) = parse_frame(&raw).expect("parse_frame").expect("整帧");
        match Event::from_frame(frame).expect("事件名级错误帧不该失败") {
            Event::Error {
                error_code,
                error_message,
            } => {
                assert_eq!(error_code, "internalServerException");
                assert_eq!(error_message, "boom");
            }
            other => panic!("缺 :message-type 时也应进入错误路径，实际 {other:?}"),
        }
    }

    /// 负例：拼写相近的假名字**仍落 Unknown** —— 容忍不能变成模糊匹配。
    ///
    /// FAIL 条件：任何一条被「宽松」识别成 Metadata / 错误路径（例如改成前缀匹配、
    /// 大小写不敏感、或把 Unknown 兜底改成 error），对应断言立刻红。走错方向比漏认更贵：
    /// 把正常帧当错误会把一次 200 变成 502/429。
    ///
    /// 15 条**全部**走 `build_frame` → `Event::from_frame` 整链路（此前只有 2 条走，
    /// 其余 13 条只钉分类函数）：`from_str` 不是唯一入口 —— `Event::from_frame` 先按
    /// `:message-type` 分流、再过 `is_event_name_error` 名单，只钉 `from_str` 的话
    /// 「在 from_frame 里另加宽松分支」这类改动不会被发现。载荷按「若名字被宽松接受、
    /// 会走哪条错路」选：metadata 系假名带 `stopReason`（被当 metadata 就会真解析出
    /// stop_reason），错误系假名带 `message`（被当事件名级错误就会产出 Event::Error）。
    #[test]
    fn near_miss_event_names_still_fall_to_unknown() {
        // (假名字, 载荷, 近似方向)
        let cases: [(&str, &str, &str); 15] = [
            ("metadataEvents", r#"{"stopReason":"max_tokens"}"#, "复数"),
            (
                "messageMetadata",
                r#"{"stopReason":"max_tokens"}"#,
                "缺 Event 后缀",
            ),
            (
                "messageMetadataEvents",
                r#"{"stopReason":"max_tokens"}"#,
                "复数",
            ),
            (
                "MetadataEvent",
                r#"{"stopReason":"max_tokens"}"#,
                "大小写不同",
            ),
            ("metadataevent", r#"{"stopReason":"max_tokens"}"#, "全小写"),
            ("throttlingErrors", r#"{"message":"not an error"}"#, "复数"),
            (
                "ThrottlingError",
                r#"{"message":"not an error"}"#,
                "大小写不同",
            ),
            ("throttling", r#"{"message":"not an error"}"#, "前缀"),
            ("validationErrors", r#"{"message":"not an error"}"#, "复数"),
            (
                "ValidationError",
                r#"{"message":"not an error"}"#,
                "大小写不同",
            ),
            (
                "serviceUnavailableException",
                r#"{"message":"not an error"}"#,
                "换了后缀的近似名",
            ),
            (
                "serviceUnavailableErrors",
                r#"{"message":"not an error"}"#,
                "复数+后缀",
            ),
            (
                "internalServerExceptions",
                r#"{"message":"not an error"}"#,
                "复数",
            ),
            (
                "InternalServerException",
                r#"{"message":"not an error"}"#,
                "大小写不同",
            ),
            (
                "internal_server_exception",
                r#"{"message":"not an error"}"#,
                "下划线分隔",
            ),
        ];

        for (name, payload, direction) in cases {
            // ① 分类函数：假名字必须落 Unknown（容忍不得变成模糊匹配）。
            assert_eq!(
                EventType::from_str(name),
                EventType::Unknown,
                "{name}（{direction}）是拼写相近的假名字，必须仍落 Unknown"
            );

            // ② 整链路：build_frame → Event::from_frame 同样必须落 Unknown。
            let raw = build_frame(
                &[(":message-type", "event"), (":event-type", name)],
                payload.as_bytes(),
            );
            let (frame, _) = parse_frame(&raw).expect("parse_frame").expect("整帧");
            assert!(
                matches!(
                    Event::from_frame(frame).expect("未知事件名必须按 Unknown 返回，而不是 Err"),
                    Event::Unknown {}
                ),
                "{name}（{direction}）不得被整链路识别成 Metadata/错误路径（走错方向比漏认更贵）"
            );
        }
    }

    /// 回归守卫：`message-type: error` 老路径原样不动（`:error-code` 优先、原始 payload
    /// 当文本、不解析 JSON 字段）。同时钉住路由优先级：`:message-type` 先判。
    ///
    /// FAIL 条件：新分支挪动了这条路径的取值口径（丢掉 `:error-code`、开始解析 payload
    /// 字段）→ 断言红；帧同时带 `:event-type: throttlingError` 时若被新分支截走 → 也红。
    #[test]
    fn message_type_error_path_unchanged() {
        let raw = build_frame(
            &[
                (":message-type", "error"),
                (":error-code", "InternalServerException"),
                // 同时带事件名级错误名：不得改变取值来源（`:message-type` 优先）
                (":event-type", "throttlingError"),
            ],
            b"upstream exploded",
        );
        let (frame, _) = parse_frame(&raw).expect("parse_frame").expect("整帧");
        match Event::from_frame(frame).expect("error 帧不该失败") {
            Event::Error {
                error_code,
                error_message,
            } => {
                assert_eq!(
                    error_code, "InternalServerException",
                    ":message-type: error 路径仍以 :error-code 为准"
                );
                assert_eq!(
                    error_message, "upstream exploded",
                    "该路径的错误文本仍是原始 payload（不做 JSON 字段解析）"
                );
            }
            other => panic!(":message-type: error 应产出 Event::Error，实际 {other:?}"),
        }
    }

    /// 构造一个只带指定 headers 的 event-stream 帧。
    fn build_frame(headers: &[(&str, &str)], payload: &[u8]) -> Vec<u8> {
        let mut hs = Vec::new();
        for (name, value) in headers {
            hs.push(name.len() as u8);
            hs.extend_from_slice(name.as_bytes());
            hs.push(7u8); // String
            hs.extend_from_slice(&(value.len() as u16).to_be_bytes());
            hs.extend_from_slice(value.as_bytes());
        }
        let total = (PRELUDE_SIZE + hs.len() + payload.len() + 4) as u32;
        let mut buf = Vec::new();
        buf.extend_from_slice(&total.to_be_bytes());
        buf.extend_from_slice(&(hs.len() as u32).to_be_bytes());
        buf.extend_from_slice(&crc32(&buf[..8]).to_be_bytes());
        buf.extend_from_slice(&hs);
        buf.extend_from_slice(payload);
        buf.extend_from_slice(&crc32(&buf).to_be_bytes());
        buf
    }

    /// 缺 `:message-type` 的帧仍按 `:event-type` 路由 —— 这是**刻意保留**的宽松默认。
    ///
    /// 回退即 FAIL：把缺失判成 `Err` 会让「只有路由头缺失、payload 完全可用」的帧被整帧
    /// 跳过（`decode_frames_into` 对 Err 的处理），toolUseEvent 更会被当成响应截断直接失败。
    /// 本测试同时覆盖「缺失时打一次 warn」那条分支不会改变返回值。
    #[test]
    fn missing_message_type_still_routes_by_event_type() {
        let raw = build_frame(
            &[(":event-type", "assistantResponseEvent")],
            br#"{"content":"hi"}"#,
        );
        let (frame, _) = parse_frame(&raw).expect("parse_frame").expect("整帧");
        assert_eq!(
            frame.message_type(),
            None,
            "前置：本帧确实缺 :message-type（否则测的不是目标分支）"
        );
        match Event::from_frame(frame).expect("缺路由头不该让帧失败") {
            Event::AssistantResponse(ev) => assert_eq!(ev.content, "hi"),
            other => panic!("应按 :event-type 路由到 AssistantResponse，实际 {other:?}"),
        }
    }
}
