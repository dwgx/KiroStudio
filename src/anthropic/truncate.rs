//! 请求体超限时的历史截断
//!
//! Kiro 上游对过大的请求体返回 `400 Input is too long.`
//! (`CONTENT_LENGTH_EXCEEDS_THRESHOLD`)，此前本项目会把它直接翻译成「上下文窗口
//! 已满」透传给客户端。Codex 侧的自动压缩要到它自己的 token 阈值才触发，而上游的
//! 拒绝点是**字节**级的，两者不对齐，于是长会话必然撞墙且无法自恢复。
//!
//! kiro-go 的做法（`proxy/translator.go` 的 `truncatePayloadToLimit`）是在网关侧
//! 主动丢弃最旧的历史轮次，并插入一条占位说明让模型知道上下文被省略过。实测同一个
//! 2MB 请求：kirostudio 拒绝，kiro-go 返回 200。本模块移植该机制。
//!
//! 保留策略（与 kiro-go 一致）：
//! - 始终保留最近 [`MIN_RECENT_HISTORY_TURNS`] 条历史；
//! - 被丢弃处插入一条 [`TRUNCATION_PLACEHOLDER`] 用户消息；
//! - 历史必须以 user 消息开头（上游要求 user/assistant 交替），故截断后若首条是
//!   assistant 则一并丢弃。

use crate::kiro::model::requests::conversation::{
    ConversationState, HistoryUserMessage, Message,
};

/// 序列化后请求体的字节上限。
///
/// 上游实测在 2MB 左右开始拒绝（1.1MB 仍可通过）。这里取 900KB，与 kiro-go 同值，
/// 保守地留出请求头与序列化开销的余量。
pub const MAX_PAYLOAD_BYTES: usize = 900 * 1024;

/// 截断时始终保留的最近历史条数。
pub const MIN_RECENT_HISTORY_TURNS: usize = 4;

/// 丢弃旧历史处插入的占位说明。
pub const TRUNCATION_PLACEHOLDER: &str = "[Earlier conversation history was truncated to fit the model's input limit. Older messages and tool activity have been omitted.]";

/// 估算单条历史的序列化字节数。
fn entry_size(entry: &Message) -> usize {
    serde_json::to_string(entry).map(|s| s.len()).unwrap_or(0)
}

/// 估算整个 `ConversationState` 的序列化字节数。
fn state_size(state: &ConversationState) -> usize {
    serde_json::to_string(state).map(|s| s.len()).unwrap_or(0)
}

/// 丢掉开头连续的 assistant 消息，保证历史以 user 开头。
fn drop_leading_assistant(mut tail: Vec<Message>) -> Vec<Message> {
    while matches!(tail.first(), Some(Message::Assistant(_))) {
        tail.remove(0);
    }
    tail
}

/// 若请求体超过 [`MAX_PAYLOAD_BYTES`]，丢弃最旧的历史轮次直至满足上限。
///
/// 返回被丢弃的条数（0 表示未截断）。当前消息本身超限时无能为力——那是单条用户
/// 输入过大，截断历史也救不了，此时返回已丢弃的条数并让请求照常发出（由上游给出
/// 明确错误），而不是静默改写用户的当前输入。
pub fn truncate_history_if_needed(state: &mut ConversationState, model_id: &str) -> usize {
    if state_size(state) <= MAX_PAYLOAD_BYTES {
        return 0;
    }

    let conversation = std::mem::take(&mut state.history);
    let total = conversation.len();
    if total == 0 {
        return 0;
    }

    let placeholder = Message::User(HistoryUserMessage::new(TRUNCATION_PLACEHOLDER, model_id));

    // 先量出「不含任何历史」的基线大小（含占位条目），再从最新往旧累加。
    let base = state_size(state) + entry_size(&placeholder);

    let sizes: Vec<usize> = conversation.iter().map(entry_size).collect();

    // 保留能放下的最长后缀，但不少于 MIN_RECENT_HISTORY_TURNS 条。
    let mut keep_from = total;
    let mut running = base;
    for i in (0..total).rev() {
        running += sizes[i];
        let kept = total - i;
        if running > MAX_PAYLOAD_BYTES && kept > MIN_RECENT_HISTORY_TURNS {
            break;
        }
        keep_from = i;
    }

    let tail = drop_leading_assistant(conversation[keep_from..].to_vec());

    let mut rebuilt = Vec::with_capacity(tail.len() + 1);
    if keep_from > 0 {
        rebuilt.push(placeholder);
    }
    rebuilt.extend(tail);
    state.history = rebuilt;

    let dropped = keep_from;
    if dropped > 0 {
        tracing::warn!(
            "请求体超过 {} KB，已丢弃最旧 {} 条历史（保留最近 {} 条）并插入占位说明",
            MAX_PAYLOAD_BYTES / 1024,
            dropped,
            total - dropped
        );
    }
    dropped
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kiro::model::requests::conversation::HistoryAssistantMessage;

    fn user(content: &str) -> Message {
        Message::User(HistoryUserMessage::new(content, "auto"))
    }

    fn assistant(content: &str) -> Message {
        Message::Assistant(HistoryAssistantMessage::new(content))
    }

    fn state_with(history: Vec<Message>) -> ConversationState {
        let mut s = ConversationState::new("11111111-1111-4111-8111-111111111111");
        s.history = history;
        s
    }

    #[test]
    fn test_no_truncation_when_small() {
        let mut s = state_with(vec![user("hi"), assistant("hello")]);
        assert_eq!(truncate_history_if_needed(&mut s, "auto"), 0);
        assert_eq!(s.history.len(), 2, "小请求不应被改动");
    }

    #[test]
    fn test_truncates_oversized_history() {
        let big = "x".repeat(200 * 1024);
        // 12 条大历史 ≈ 2.4MB，远超 900KB。
        let mut hist = Vec::new();
        for _ in 0..6 {
            hist.push(user(&big));
            hist.push(assistant(&big));
        }
        let mut s = state_with(hist);
        let dropped = truncate_history_if_needed(&mut s, "auto");

        assert!(dropped > 0, "超限请求必须被截断");
        assert!(
            state_size(&s) <= MAX_PAYLOAD_BYTES,
            "截断后仍超限: {} > {}",
            state_size(&s),
            MAX_PAYLOAD_BYTES
        );
        // 首条应是占位说明，让模型知道上下文被省略。
        match s.history.first() {
            Some(Message::User(m)) => assert_eq!(
                m.user_input_message.content, TRUNCATION_PLACEHOLDER,
                "被丢弃处应插入占位说明"
            ),
            other => panic!("首条应为占位 user 消息，实际: {other:?}"),
        }
    }

    #[test]
    fn test_history_starts_with_user_after_truncation() {
        // 构造截断后恰好以 assistant 开头的情况，验证它被丢掉
        // （上游要求 user/assistant 交替且以 user 起始）。
        let big = "y".repeat(150 * 1024);
        let mut hist = Vec::new();
        for _ in 0..10 {
            hist.push(assistant(&big));
            hist.push(user(&big));
        }
        let mut s = state_with(hist);
        truncate_history_if_needed(&mut s, "auto");

        // 跳过占位条后，其余历史不得以 assistant 开头。
        let rest: Vec<&Message> = s.history.iter().skip(1).collect();
        if let Some(first) = rest.first() {
            assert!(
                matches!(first, Message::User(_)),
                "占位之后必须紧跟 user 消息"
            );
        }
    }

    #[test]
    fn test_empty_history_is_noop() {
        let mut s = state_with(vec![]);
        assert_eq!(truncate_history_if_needed(&mut s, "auto"), 0);
    }
}
