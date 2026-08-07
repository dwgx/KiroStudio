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
    /// 思考块签名（上游若下发则带上）。
    ///
    /// # 加它的理由：不加就永远观测不到
    ///
    /// 本结构体原先只有 `text`，于是上游若在帧里带了 `signature`，serde 会**静默丢弃**
    /// 它 —— 不报错、不打日志。而我们下发给客户端的是自造的
    /// `THINKING_SIGNATURE_PLACEHOLDER`（`anthropic::stream:140`）。两者叠加的后果是：
    /// **如果上游一直在发真签名，我们一直在用假的盖住它，且无法发现。**
    ///
    /// # 上游到底发不发？未知，且刻意不假定
    ///
    /// 对照实现 `~/Documents/Project/_study/kiro-rs` 的同名结构体有这个字段
    /// （`reasoning.rs:21`），但它的消费点 `stream.rs:2079-2082` 同样是
    /// `.unwrap_or_else(|| THINKING_SIGNATURE_PLACEHOLDER)` —— 即**它也不知道上游发不发**，
    /// 只是留了通路。它的两条测试只证明「若上游发了则能解析」，不证明上游会发。
    ///
    /// 所以这里只做**解析**，不改下发逻辑：解析是零风险的（`Option` + `default`，
    /// 缺失即 `None`），而它把「上游是否发真签名」从推测变成可观测。
    #[serde(default)]
    pub signature: Option<String>,
    /// 上游返回的加密思考内容（若有）。
    ///
    /// 同 `signature`：先解析、先能观测，暂不接入下发。真要下发需先确认上游确实发它、
    /// 以及它的编码形态，那需要真实样本 —— 而拿到样本的前提就是先解析它。
    #[serde(default)]
    pub redacted_content: Option<String>,
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
        // 两个新字段同样必须容缺（上游绝大多数帧只有 text）。
        assert!(e.signature.is_none());
        assert!(e.redacted_content.is_none());
    }

    /// ⭐ 回归：`signature` 必须被解析而不是被 serde 静默丢弃。
    ///
    /// 本结构体原先只有 `text`，于是上游若在同一帧带了 `signature`，它在反序列化时
    /// **无声消失** —— 不报错、不打日志，只是永远拿不到。而我们下发给客户端的是自造的
    /// `THINKING_SIGNATURE_PLACEHOLDER`，若上游其实一直在发真签名，那就是拿假的盖住真的。
    ///
    /// 删掉 `pub signature` 字段 → 本测试编译失败（比断言失败更早暴露）。
    #[test]
    fn parses_signature_instead_of_silently_dropping_it() {
        let e: ReasoningContentEvent =
            serde_json::from_str(r#"{"text":"reasoning","signature":"real-sig-from-upstream"}"#)
                .expect("应可解析带 signature 的帧");
        assert_eq!(e.text, "reasoning");
        assert_eq!(
            e.signature.as_deref(),
            Some("real-sig-from-upstream"),
            "signature 必须被解析 —— 旧结构体会把它静默丢掉"
        );
    }

    /// ⭐ 回归：`redactedContent`（camelCase）必须映射到 `redacted_content`。
    ///
    /// 承重点在**命名转换**：结构体有 `#[serde(rename_all = "camelCase")]`，
    /// 若哪天有人把它删掉或给字段加了错的 `rename`，这个字段会静默变成 None
    /// （加密思考内容整块丢失，而且没有任何报错）。
    #[test]
    fn parses_redacted_content_with_camel_case_mapping() {
        let e: ReasoningContentEvent =
            serde_json::from_str(r#"{"redactedContent":"encrypted-blob"}"#)
                .expect("应可解析加密思考帧");
        assert_eq!(e.redacted_content.as_deref(), Some("encrypted-blob"));
        // 反向守卫：snake_case 写法**不该**命中（证明 camelCase 转换确实在起作用，
        // 而不是恰好因为字段名相同而通过）。
        let snake: ReasoningContentEvent =
            serde_json::from_str(r#"{"redacted_content":"nope"}"#).expect("未知字段应被忽略");
        assert!(
            snake.redacted_content.is_none(),
            "snake_case 键不该命中 —— 上游用的是 camelCase"
        );
    }
}
