//! 助手响应事件
//!
//! 处理 assistantResponseEvent 类型的事件

use serde::{Deserialize, Serialize};

use crate::kiro::parser::error::ParseResult;
use crate::kiro::parser::frame::Frame;

use super::base::EventPayload;

/// `<tool_use ...>...</tool_use>` 开标签的最小前缀。判据统一用这个常量，
/// 防止分散字面量在演化中漂移（另一处用法见 stream.rs 的 tool_use XML 泄漏过滤）。
pub(crate) const TOOL_USE_XML_PREFIX: &str = "<tool_use";
/// 对应的闭合标签。
pub(crate) const TOOL_USE_XML_CLOSE: &str = "</tool_use>";

/// 就地剥离 `content` 里**单帧内完整**的字面量 tool_use XML 泄漏块
/// （`<tool_use …>…</tool_use>`）。
///
/// 上游模型偶尔把工具调用当**正文文本**吐进 `assistantResponseEvent`（不是结构化
/// `toolUseEvent`），客户端会把泄漏的 XML 当普通文本渲染。参考仓 ref-grey 在帧层
/// （assistant.rs:12-90）先做一道剥离，本函数是那道兜底的等价物：整块泄漏落在同一帧
/// 里时，在最靠上游的位置就剥掉，不依赖下游任何状态。
///
/// # 🔴 只剥「完整块」是刻意的，改成「开了头就连坐剥到底」会引入跨帧泄漏
///
/// 本层跑在 `from_frame`，**早于** `anthropic/stream.rs` 的跨 chunk 状态机。若本层把
/// 「有开标签但本帧内没闭合」也一并丢弃（ref-grey assistant.rs:36-39 的做法），那么
/// 一个被上游切成两帧的泄漏块会这样走：
///   · 帧1 `before <tool_use id="x" name="Read"` → 本层丢掉半个开标签，只剩 `before`；
///   · 帧2 `>{"path":"/a"}</tool_use> after`  → **不含** `<tool_use`，本层原样透传；
///   · 流层状态机因为从没见过开标签，把帧2 的 JSON 与闭合标签**当正文发给客户端**。
/// 即两层各自"看起来都对"，合起来漏。所以分工必须是：**本层只剥完整块，跨帧形态一律
/// 留给流层状态机**（它有 `tool_use_xml_stripping` 跨 chunk 状态，能正确吃掉半标签）。
///
/// # 判据
///
/// `<tool_use` 后必须紧接 `>` 或空白才算开标签（`<tool_user>`、`<tool_uses>` 这类
/// "相似但不同"的正文**不剥**，避免误删普通文本）；闭合必须是原样的 `</tool_use>`。
/// 不做 trim —— 剥离不该顺手改动周围正文的空白（那会吃掉正文里有意义的首尾空格）。
pub(crate) fn strip_tool_use_xml_leaks(content: &str) -> String {
    let mut out = String::with_capacity(content.len());
    let mut rest = content;

    while let Some(start) = rest.find(TOOL_USE_XML_PREFIX) {
        let after_start = &rest[start..];
        let Some(open_end) = after_start.find('>') else {
            // 开标签在本帧内没到 `>`：跨帧形态，交给流层状态机，本层原样透传。
            break;
        };
        let tag_head = &after_start[..open_end];
        if !tag_head
            .get(TOOL_USE_XML_PREFIX.len()..)
            .is_some_and(|suffix| suffix.is_empty() || suffix.starts_with(char::is_whitespace))
        {
            // `<tool_user>` 之类：`<tool_use` 后紧跟非空白非 `>`，不是 tool_use 开标签。
            out.push_str(&rest[..start + TOOL_USE_XML_PREFIX.len()]);
            rest = &after_start[TOOL_USE_XML_PREFIX.len()..];
            continue;
        }
        let after_open = &after_start[open_end + 1..];
        let Some(close_start) = after_open.find(TOOL_USE_XML_CLOSE) else {
            // 有合法开标签但本帧内没闭合：同样是跨帧形态，留给流层，本层原样透传。
            break;
        };
        // 完整块：整段丢弃（块前文本保留，块后继续扫）。
        out.push_str(&rest[..start]);
        rest = &after_open[close_start + TOOL_USE_XML_CLOSE.len()..];
    }

    out.push_str(rest);
    out
}

/// 助手响应事件
///
/// 包含 AI 助手的流式响应内容
///
/// # 设计说明
///
/// 此结构体只保留实际使用的 `content` 字段。serde 默认会忽略 JSON 中
/// 未声明的字段，因此其他 API 返回的字段被自动丢弃，反序列化不会失败，
/// 同时避免为每个高频流式帧额外分配一个捕获用的 map。
///
/// # 示例
///
/// ```rust
/// use kirostudio::kiro::model::events::AssistantResponseEvent;
///
/// let json = r#"{"content":"Hello, world!"}"#;
/// let event: AssistantResponseEvent = serde_json::from_str(json).unwrap();
/// assert_eq!(event.content, "Hello, world!");
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssistantResponseEvent {
    /// 响应内容片段
    #[serde(default)]
    pub content: String,
}

impl EventPayload for AssistantResponseEvent {
    fn from_frame(frame: &Frame) -> ParseResult<Self> {
        let mut event: Self = frame.payload_as_json()?;
        // 帧层兜底剥离：`assistantResponseEvent.content` 是**纯文本流**（结构化工具调用
        // 走独立的 `ToolUseEvent`），这里出现的 `<tool_use …>` 只可能是泄漏的正文。
        // 快速路径避免在干净帧上分配。流式路径的跨 chunk 状态机负责剥完整标签，
        // 这里兜底处理被切在帧边界 / 状态机遗漏的形态（参考仓 ref-grey 的双层设计）。
        if event.content.contains(TOOL_USE_XML_PREFIX) {
            event.content = strip_tool_use_xml_leaks(&event.content);
        }
        Ok(event)
    }
}

impl Default for AssistantResponseEvent {
    fn default() -> Self {
        Self {
            content: String::new(),
        }
    }
}

impl std::fmt::Display for AssistantResponseEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.content)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_deserialize_simple() {
        let json = r#"{"content":"Hello, world!"}"#;
        let event: AssistantResponseEvent = serde_json::from_str(json).unwrap();
        assert_eq!(event.content, "Hello, world!");
    }

    #[test]
    fn test_deserialize_with_extra_fields() {
        // 确保包含额外字段时反序列化不会失败
        let json = r#"{
            "content": "Done",
            "conversationId": "conv-123",
            "messageId": "msg-456",
            "messageStatus": "COMPLETED",
            "followupPrompt": {
                "content": "Would you like me to explain further?",
                "userIntent": "EXPLAIN_CODE_SELECTION"
            }
        }"#;
        let event: AssistantResponseEvent = serde_json::from_str(json).unwrap();
        assert_eq!(event.content, "Done");
    }

    #[test]
    fn test_serialize_minimal() {
        let event = AssistantResponseEvent::default();
        let event = AssistantResponseEvent {
            content: "Test".to_string(),
            ..event
        };

        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"content\":\"Test\""));
        // extra 字段不应该被序列化
        assert!(!json.contains("extra"));
    }

    #[test]
    fn test_display() {
        let event = AssistantResponseEvent {
            content: "test".to_string(),
            ..Default::default()
        };
        assert_eq!(format!("{}", event), "test");
    }

    #[test]
    fn test_strip_tool_use_xml_leaks() {
        // 完整块剥掉，块前/块后正文原样保留（本层不 trim，不吞正文空格）。
        let content = "before\n\
            <tool_use id=\"toolu_1\" name=\"Read\">\n\
            {\"path\":\"/tmp/a\"}\n\
            </tool_use>\n\
            after";
        // 块内的换行（含闭合标签前那个）都属于被剥的块；块前的 `\n` 与块后的 `\nafter` 保留。
        assert_eq!(strip_tool_use_xml_leaks(content), "before\n\nafter");
    }

    #[test]
    fn test_strip_tool_use_xml_leaks_multiple() {
        let content = "<tool_use id=\"a\" name=\"A\">x</tool_use> mid <tool_use id=\"b\" name=\"B\">y</tool_use>";
        // 多个完整块全部剥掉，只剩中段与两侧空格（不 trim）。
        assert_eq!(strip_tool_use_xml_leaks(content), " mid ");
    }

    #[test]
    fn test_strip_tool_use_xml_leaks_keeps_similar_text() {
        // `<tool_user>` / `<tool_uses>` 是普通正文，绝不误剥（参考仓同类测试的等价物）。
        assert_eq!(
            strip_tool_use_xml_leaks("use <tool_user> as an example"),
            "use <tool_user> as an example"
        );
        assert_eq!(strip_tool_use_xml_leaks("the <tool_uses> tag"), "the <tool_uses> tag");
        // 普通含 `<` 的文本（比较运算符/散文）不被触碰。
        assert_eq!(strip_tool_use_xml_leaks("if a < b then b > c"), "if a < b then b > c");
    }

    #[test]
    fn test_strip_tool_use_xml_leaks_passes_through_partial_tags() {
        // 🔴 跨帧形态必须原样透传（不能连坐剥到底）：帧层跑在流层状态机**之前**，
        // 吞掉半个开标签会让流层永远看不到开标签，泄漏的中间帧被当正文下发。
        // 见本函数文档注释的「只剥完整块是刻意的」一节。
        assert_eq!(
            strip_tool_use_xml_leaks("before <tool_use id=\"toolu_1\" name=\"Write\""),
            "before <tool_use id=\"toolu_1\" name=\"Write\""
        );
        assert_eq!(
            strip_tool_use_xml_leaks("head <tool_use id=\"a\" name=\"A\">tail"),
            "head <tool_use id=\"a\" name=\"A\">tail"
        );
    }

    #[test]
    fn test_from_frame_strips_tool_use_xml_leak() {
        // 帧层集成：assistantResponseEvent 文本流里出现 tool_use XML 泄漏，从帧解析即剥掉。
        let content = "before <tool_use name=\"Read\">{\"path\":\"/a\"}</tool_use> after";
        // ⚠️ payload 必须是 `{"content": "..."}` 对象，不是裸字符串 ——
        // `AssistantResponseEvent` 是 struct，喂裸字符串会 PayloadDeserialize 失败。
        let json = serde_json::json!({ "content": content }).to_string();
        let frame = build_test_frame("assistantResponseEvent", &json);
        let event = AssistantResponseEvent::from_frame(&frame).unwrap();
        assert_eq!(event.content, "before  after");
    }

    #[test]
    fn test_from_frame_keeps_normal_text() {
        // 普通正文（含 `<`）从帧解析原样保留，不做任何处理。
        let frame = build_test_frame("assistantResponseEvent", r#"{"content":"Hello, world!"}"#);
        let event = AssistantResponseEvent::from_frame(&frame).unwrap();
        assert_eq!(event.content, "Hello, world!");
    }

    /// 构造一个可被 `parse_frame` 解析的测试帧（AWS Event Stream wire 格式：
    /// total_length + header_length + prelude_crc + headers + payload + message_crc）。
    /// 事件类型通过 `:event-type` 字符串 header 声明（wire 布局见
    /// `src/kiro/parser/header.rs::parse_headers`：name_len + name + type(7=String) + len + value）。
    fn build_test_frame(event_type: &str, payload: &str) -> Frame {
        use crate::kiro::parser::crc::crc32;
        use crate::kiro::parser::frame::parse_frame;
        use crate::kiro::parser::frame::PRELUDE_SIZE;

        const NAME: &[u8] = b":event-type";
        let mut headers = Vec::new();
        headers.push(NAME.len() as u8);
        headers.extend_from_slice(NAME);
        headers.push(7u8); // HeaderValueType::String
        headers.extend_from_slice(&(event_type.len() as u16).to_be_bytes());
        headers.extend_from_slice(event_type.as_bytes());

        let header_length = headers.len() as u32;
        let total_length = (PRELUDE_SIZE + headers.len() + payload.len() + 4) as u32;

        let mut buf = Vec::new();
        buf.extend_from_slice(&total_length.to_be_bytes());
        buf.extend_from_slice(&header_length.to_be_bytes());
        let prelude_crc = crc32(&buf[..8]);
        buf.extend_from_slice(&prelude_crc.to_be_bytes());
        buf.extend_from_slice(&headers);
        buf.extend_from_slice(payload.as_bytes());
        let msg_crc = crc32(&buf);
        buf.extend_from_slice(&msg_crc.to_be_bytes());

        let (frame, _consumed) = parse_frame(&buf).expect("parse frame").expect("frame");
        frame
    }
}
