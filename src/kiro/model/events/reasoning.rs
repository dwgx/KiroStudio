//! 结构化思考（reasoning）事件
//!
//! 处理 `reasoningContentEvent` 类型的事件 —— 上游的**结构化 thinking 增量流**。
//!
//! # 为什么需要它（E1）
//!
//! 上游一直在发这个事件，而我们此前把它归到 `EventType::Unknown` 直接**丢弃 payload**，
//! 然后转而从正文文本里嗅探 `<thinking>` 标签把边界"猜"回来
//! （`anthropic::stream` 的 `invoke_sniff_buffer` 那一套）。即：
//! **上游给了结构化边界，我们扔掉后用启发式规则重新推导。**
//!
//! 那条嗅探路径的脆弱性有代码自证：要处理"模型在思考里提到 `</thinking>`"、
//! 要求"`</thinking>` 之后全是空白"才认结束、要防标签跨事件分割；
//! 已知致命缺陷 #14（`invoke_sniff_buffer` 无界持有导致整条流停摆）就出在这套嗅探上。
//!
//! 所以接入它不是"多一个功能"，而是**移除一整类缺陷的来源**。
//!
//! # payload 字段名是怎么确认的
//!
//! ⚠️ 不能想当然：`assistantResponseEvent` 读的是 `content`，而本事件读 `text`，**两者不同名**。
//! 该字段名由线上实帧确认（`base.rs` 的"未识别事件按类型 warn 一次"正是为此保留的诊断能力）：
//!
//! ```text
//! event_type=reasoningContentEvent payload_bytes=21 payload_sample={"text":"Everything"}
//! ```
//!
//! 与生态实现（Kiro-Go `kiro.go` 读 `event["text"]`）一致。

use serde::{Deserialize, Serialize};

use crate::kiro::parser::error::ParseResult;
use crate::kiro::parser::frame::Frame;

use super::base::EventPayload;

/// 结构化思考内容事件（纯增量 delta，非累积快照）。
///
/// 生态实测结论（Kiro-Go 的注释，针对真实上游流量验证过）：
/// `assistantResponseEvent` 与 `reasoningContentEvent` 都是**纯增量**，
/// 绝不是累积快照 —— 所以直接把每帧的 `text` 追加下发即可，不要做差分。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReasoningContentEvent {
    /// 本帧的思考内容增量。
    ///
    /// `#[serde(default)]`：上游偶发空帧（只有其它元字段）时不应让整条流解析失败 ——
    /// 与 `AssistantResponseEvent::content` 同款容错策略。
    #[serde(default)]
    pub text: String,
}

impl EventPayload for ReasoningContentEvent {
    fn from_frame(frame: &Frame) -> ParseResult<Self> {
        frame.payload_as_json()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 字段名必须是 `text`（线上实帧确证）。
    ///
    /// 这条测试的价值在于锁住那个**容易想当然写成 `content`** 的地方：
    /// 同一个事件家族里两种事件字段名不同，写错不会报错，只会让 thinking 恒为空。
    #[test]
    fn parses_text_field_not_content() {
        let e: ReasoningContentEvent =
            serde_json::from_str(r#"{"text":"Everything"}"#).expect("应可解析线上实帧格式");
        assert_eq!(e.text, "Everything");

        // 写成 content 的话拿不到内容（反向守卫：证明字段名确实是 text 起作用）。
        let wrong: ReasoningContentEvent =
            serde_json::from_str(r#"{"content":"nope"}"#).expect("未知字段应被忽略而非报错");
        assert!(
            wrong.text.is_empty(),
            "content 字段不该被当成思考内容 —— 两个事件字段名不同"
        );
    }

    /// 空帧/缺字段不得让整条流解析失败。
    #[test]
    fn missing_text_defaults_to_empty_instead_of_failing() {
        let e: ReasoningContentEvent =
            serde_json::from_str("{}").expect("缺字段应走 default，不应报错");
        assert!(e.text.is_empty());
    }
}
