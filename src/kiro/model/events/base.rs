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
            "event" => Self::parse_event(frame),
            "error" => Self::parse_error(frame),
            "exception" => Self::parse_exception(frame),
            other => Err(ParseError::InvalidMessageType(other.to_string())),
        }
    }

    /// 解析事件类型消息
    fn parse_event(frame: Frame) -> ParseResult<Self> {
        let event_type_str = frame.event_type().unwrap_or("unknown");
        let event_type = EventType::from_str(event_type_str);

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
                // docs/CACHE-EXP0-RESULT.md）。本分支只剩真正未识别的类型。
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
