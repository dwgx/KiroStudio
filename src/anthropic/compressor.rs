//! 输入压缩管道
//!
//! 在协议转换完成后、发送到上游前，对 `ConversationState` 执行多层压缩，
//! 以规避 Kiro 上游请求体大小限制（实测约 5MiB 左右会触发 400）。
//!
//! 本模块吸收自 `reference/Foxfishc__kiro.rs/src/anthropic/compressor.rs`
//! （MIT 许可，致谢原作者）。我方**分层增量吸收**，当前实现 4 个 pass：
//! 1. 空白压缩（连续空行折叠、行尾空格移除，近乎无损）
//! 2. tool_result 智能截断（大工具结果保留头 N 行 + 尾 M 行，中间占位省略）
//! 3. repair_non_empty_content（压缩把 content 压空后补占位符，防空响应）
//! 4. 超长消息内容截断（`compress_long_messages_pass`，仅自适应二次压缩用，
//!    属最后手段：单条消息本身大到移除历史也救不回来时必须截断正文）
//!
//! TODO(后续批次)：
//! - ② thinking 块丢弃/截断（compress_thinking_pass）
//! - ④ tool_use input 截断（compress_tool_use_inputs_pass）
//! - ⑤ 历史轮次/字符截断（compress_history_pass）
//!   以及截断后 tool_use/tool_result 跨消息配对修复（repair_tool_pairing_pass）。
//!   这些层风险更高（可能破坏 tool 配对、丢失历史上下文），暂缓引入。

use crate::kiro::model::requests::conversation::{ConversationState, Message};
use crate::model::config::CompressionConfig;

/// 压缩统计信息
#[derive(Debug, Default, Clone)]
pub struct CompressionStats {
    /// 空白压缩节省的字节数
    pub whitespace_saved: usize,
    /// tool_result 截断节省的字节数
    pub tool_result_saved: usize,
}

impl CompressionStats {
    /// 总节省字节数
    pub fn total_saved(&self) -> usize {
        self.whitespace_saved + self.tool_result_saved
    }
}

/// 压缩管道入口
///
/// 按顺序执行已实现的各层压缩，返回统计信息。仅在 `config.enabled` 时生效；
/// 是否调用本函数（例如按 `trigger_bytes` 阈值）由调用方决定。
pub fn compress(state: &mut ConversationState, config: &CompressionConfig) -> CompressionStats {
    let mut stats = CompressionStats::default();

    if !config.enabled {
        return stats;
    }

    // 1. 空白压缩
    if config.whitespace_compression {
        stats.whitespace_saved = compress_whitespace_pass(state);
    }

    // 3. tool_result 智能截断
    if config.tool_result_max_chars > 0 {
        stats.tool_result_saved = compress_tool_results_pass(
            state,
            config.tool_result_max_chars,
            config.tool_result_head_lines,
            config.tool_result_tail_lines,
        );
    }

    // 兜底：空白压缩可能把「纯空白」content 压成空串，而 Kiro API 要求 content
    // 不能为空，否则上游返回 400。这里把压缩后变空的 content 修复为占位符 "."。
    let repaired = repair_non_empty_content_pass(state);
    if repaired > 0 {
        tracing::debug!(repaired, "压缩后已修复空 content 占位符");
    }

    stats
}

// ============ 空白压缩 ============

/// 空白压缩：连续空行(3+)→最多保留 2 个空行，行尾空格移除，保留行首缩进
fn compress_whitespace(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut consecutive_empty = 0u32;

    // 补分隔换行用「行号 > 0」判定，而不是「result 非空」：后者会把**开头**的空行
    // 整段吃掉（首行空 → result 仍空 → 不 push），而结尾空行却保留 2 个，
    // 同一份正文从首行还是尾行空起不该有两种口径。
    for (idx, line) in text.split('\n').enumerate() {
        let trimmed_end = line.trim_end();

        if trimmed_end.is_empty() {
            consecutive_empty += 1;
            if consecutive_empty <= 2 && idx > 0 {
                result.push('\n');
            }
        } else {
            consecutive_empty = 0;
            if idx > 0 {
                result.push('\n');
            }
            result.push_str(trimmed_end);
        }
    }

    result
}

/// 对 ConversationState 中所有文本字段执行空白压缩，返回节省字节数
fn compress_whitespace_pass(state: &mut ConversationState) -> usize {
    let mut saved = 0usize;

    for msg in &mut state.history {
        match msg {
            Message::User(user_msg) => {
                saved += compress_string_field(&mut user_msg.user_input_message.content);
            }
            Message::Assistant(assistant_msg) => {
                saved +=
                    compress_string_field(&mut assistant_msg.assistant_response_message.content);
            }
        }
    }

    saved += compress_string_field(&mut state.current_message.user_input_message.content);
    saved
}

/// 压缩单个字符串字段，返回节省的字节数
///
/// 跳过仅为空格占位符 " " 的字段（Kiro API 要求 content 不能为空，
/// converter 使用 " " 作为占位符，不能被压成空串）。
fn compress_string_field(field: &mut String) -> usize {
    if field == " " {
        return 0;
    }
    let original_len = field.len();
    let compressed = compress_whitespace(field);
    if compressed.len() < original_len {
        let saved = original_len - compressed.len();
        *field = compressed;
        saved
    } else {
        0
    }
}

// ============ tool_result 智能截断 ============

/// 按行智能截断，保留头尾行，中间以占位符省略。
///
/// 返回 (截断后的文本, 节省字节数)。
fn smart_truncate_by_lines(
    text: &str,
    max_chars: usize,
    head_lines: usize,
    tail_lines: usize,
) -> (String, usize) {
    let char_count = text.chars().count();
    if char_count <= max_chars {
        return (text.to_string(), 0);
    }

    let lines: Vec<&str> = text.lines().collect();
    let total_lines = lines.len();

    // 行数不足以“头 + 尾”留白时，退化为按字符各留一半的硬截断
    if total_lines <= head_lines + tail_lines {
        // 🔴 省略标记**先占预算**再截正文（同 `truncate_long_content` 的写法）：若先按
        // max_chars 截头尾、再拼标记，最终长度 = max_chars + 标记长度 > max_chars，
        // 违反本模块「截断后 ≤ max_chars」的契约（大文本分支末尾那条硬截断兜底即为此设）。
        // 标记形如 `\n...[N字符已省略]...\n`，N 最多 20 位十进制 ⇒ 40 字符预留足够。
        const MARKER_RESERVE: usize = 40;
        let budget = max_chars.saturating_sub(MARKER_RESERVE);
        let half = budget / 2;
        let head = safe_char_truncate(text, half);
        let tail_chars = budget.saturating_sub(head.chars().count());
        // tail_chars == 0（max_chars 很小、预算被标记吃光）时显式取空尾：`.rev().nth(0)`
        // 是**最后一个**字符的起点而不是「空尾」，沿用旧写法会把整段文本当尾巴拼回去。
        let tail_start = if tail_chars == 0 {
            text.len()
        } else {
            text.char_indices()
                .rev()
                .nth(tail_chars - 1)
                .map(|(i, _)| i)
                .unwrap_or(0)
        };
        let tail = &text[tail_start..];
        let omitted = char_count.saturating_sub(head.chars().count() + tail.chars().count());
        // 文案必须是「字符」：这里按**字符数**各留一半，省略量也是字符数，写「行」会误导。
        let result = format!("{}\n...[{}字符已省略]...\n{}", head, omitted, tail);
        let saved = text.len().saturating_sub(result.len());
        return (result, saved);
    }

    let head_part: String = lines[..head_lines].join("\n");
    let tail_part: String = lines[total_lines - tail_lines..].join("\n");
    let omitted_lines = total_lines - head_lines - tail_lines;
    let omitted_chars =
        char_count.saturating_sub(head_part.chars().count() + tail_part.chars().count());

    let mut result = format!(
        "{}\n...[{}行已省略（{}字符）]...\n{}",
        head_part, omitted_lines, omitted_chars, tail_part
    );

    // 硬截断兜底：确保结果不超过 max_chars（极端超长单行头/尾场景）
    if result.chars().count() > max_chars {
        let truncated = safe_char_truncate(&result, max_chars);
        result = truncated.to_string();
    }

    let saved = text.len().saturating_sub(result.len());
    (result, saved)
}

/// 遍历所有 tool_result 的 text 字段，执行智能截断，返回节省字节数
fn compress_tool_results_pass(
    state: &mut ConversationState,
    max_chars: usize,
    head_lines: usize,
    tail_lines: usize,
) -> usize {
    let mut saved = 0usize;

    for msg in &mut state.history {
        if let Message::User(user_msg) = msg {
            for result in &mut user_msg
                .user_input_message
                .user_input_message_context
                .tool_results
            {
                saved += truncate_tool_result_content(
                    &mut result.content,
                    max_chars,
                    head_lines,
                    tail_lines,
                );
            }
        }
    }

    for result in &mut state
        .current_message
        .user_input_message
        .user_input_message_context
        .tool_results
    {
        saved +=
            truncate_tool_result_content(&mut result.content, max_chars, head_lines, tail_lines);
    }

    saved
}

/// 截断单个 tool_result 的 content 数组中的 text 字段
fn truncate_tool_result_content(
    content: &mut [serde_json::Map<String, serde_json::Value>],
    max_chars: usize,
    head_lines: usize,
    tail_lines: usize,
) -> usize {
    let mut saved = 0usize;

    for map in content.iter_mut() {
        if let Some(serde_json::Value::String(text)) = map.get_mut("text") {
            if text.chars().count() > max_chars {
                let (truncated, s) =
                    smart_truncate_by_lines(text, max_chars, head_lines, tail_lines);
                saved += s;
                *text = truncated;
            }
        }
    }

    saved
}

// ============ 空 content 兜底修复 ============
/// 修复压缩后变空的 content 字段（Kiro API 要求 content 非空）。
///
/// 仅处理确实需要兜底的情况，尽量保留真实结构：
/// - history user_input_message.content（**无论有无 images/tool_results** 都兜底：
///   带 payload 的消息 content 被压空同样是 400 源，旧代码的 has_payload 门让这类漏掉）
/// - history assistant_response_message.content（仅当无 tool_uses 时兜底）
/// - current_message.user_input_message.content（同 user 历史，无条件兜底）
///
/// 返回被修复的字段数。
fn repair_non_empty_content_pass(state: &mut ConversationState) -> usize {
    let mut repaired = 0usize;

    for msg in &mut state.history {
        match msg {
            Message::User(user_msg) => {
                // 无条件兜底：`repair_content_field` 本身只在 trim 后为空时才写占位符，
                // 有真实内容的消息天然跳过，门条件删掉不会误伤。带 images/tool_results
                // 的消息此前被 has_payload 门放过 —— 其 content 压空后仍会 400。
                if repair_content_field(&mut user_msg.user_input_message.content) {
                    repaired += 1;
                }
            }
            Message::Assistant(assistant_msg) => {
                let has_tool_uses = assistant_msg
                    .assistant_response_message
                    .tool_uses
                    .as_ref()
                    .is_some_and(|tools| !tools.is_empty());
                if !has_tool_uses
                    && repair_content_field(&mut assistant_msg.assistant_response_message.content)
                {
                    repaired += 1;
                }
            }
        }
    }

    // 无条件兜底（同上：带 images/tool_results 的 current 消息压空同样要补占位符）。
    if repair_content_field(&mut state.current_message.user_input_message.content) {
        repaired += 1;
    }

    repaired
}

/// 若字段去除首尾空白后为空，替换为占位符 "."，返回是否修复
fn repair_content_field(field: &mut String) -> bool {
    // " " 是 converter 给「仅 tool_use」消息的占位符（`converter.rs` 的
    // `text_content.is_empty() && !tool_uses.is_empty()` 分支），它本身已满足上游
    // 「content 非空」的要求：`compress_string_field` / `truncate_long_content` 都
    // 特意跳过它，本 pass 若不跳过，`compress()` 末尾这一趟就会把占位符改写成 "."
    // ——assistant 侧现在只是被「有 tool_uses 就跳过本 pass」间接挡住（巧合，非契约）。
    if field == " " {
        return false;
    }
    if field.trim().is_empty() {
        *field = ".".to_string();
        return true;
    }
    false
}

// ============ 工具函数 ============

/// 安全 UTF-8 字符截断（按字符边界，不会切裂多字节字符）
fn safe_char_truncate(text: &str, max_chars: usize) -> &str {
    match text.char_indices().nth(max_chars) {
        Some((idx, _)) => &text[..idx],
        None => text,
    }
}

// ============ 超长消息内容截断（自适应二次压缩用） ============

/// 截断超长的用户消息正文（history 与 current_message），返回节省的字节数。
///
/// 这是**最后手段**的压缩层，仅由自适应二次压缩（`handlers.rs::adaptive_compress_loop`）
/// 的第三层调用——当单条 user content 本身就超过阈值、移除历史无法把请求落回阈值内时，
/// 直接截断正文。策略与参考仓一致（ref-mjy/compressor.rs:690）：保留头部、尾部附省略标记。
///
/// **只动 content 正文，不动 tool_results / tool_uses / images**，因此不会破坏
/// tool_use↔tool_result 的 id 配对（那是上游 400 的高频来源）。丢的是用户/助手正文的
/// 尾部文字——这一层本来就在「宁可丢部分上下文，也别整轮失败」的档位上。
///
/// `pub(crate)`：唯一调用方是同 crate 的 `handlers.rs::adaptive_compress_loop`，
/// 不是对外的 API 面。
pub(crate) fn compress_long_messages_pass(state: &mut ConversationState, max_chars: usize) -> usize {
    if max_chars == 0 {
        return 0;
    }

    let mut saved = 0usize;

    for msg in &mut state.history {
        if let Message::User(user_msg) = msg {
            saved += truncate_long_content(&mut user_msg.user_input_message.content, max_chars);
        }
    }

    saved += truncate_long_content(
        &mut state.current_message.user_input_message.content,
        max_chars,
    );

    saved
}

/// 截断单个 content 字段，返回节省的字节数。
///
/// 跳过仅为空格占位符 " " 的字段（与 `compress_string_field` 一致，
/// converter 用 " " 表示仅 tool_use 消息，不能被压成空串）。
fn truncate_long_content(field: &mut String, max_chars: usize) -> usize {
    if field == " " {
        return 0;
    }
    let char_count = field.chars().count();
    if char_count <= max_chars {
        return 0;
    }

    let original_len = field.len();
    // 🔴 先为省略标记**预留字符预算**，再截正文 —— 否则"截到 max_chars 再拼标记"会让
    // 最终长度 = max_chars + 标记长度 > max_chars，违反"截断后 ≤ max_chars"的契约
    // （agent 加的测试 `test_compress_long_messages_truncates_current_and_history` 就抓到了）。
    let marker = format!("\n...[content truncated, {} chars omitted]", char_count);
    let marker_chars = marker.chars().count();
    // 正文最多留 max_chars 里的 (max_chars - marker_chars) 个字符；若 max_chars 本身
    // 比标记还短（极小阈值场景），就只保留标记、正文全丢（仍是合法的"超长已截断"）。
    let body_budget = max_chars.saturating_sub(marker_chars);
    let truncated = safe_char_truncate(field, body_budget);
    let omitted = char_count.saturating_sub(body_budget);
    *field = format!("{}\n...[content truncated, {} chars omitted]", truncated, omitted);
    original_len.saturating_sub(field.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kiro::model::requests::conversation::*;
    use crate::kiro::model::requests::tool::ToolResult;

    fn make_simple_state(history_content: Vec<(&str, &str)>, current: &str) -> ConversationState {
        let mut history = Vec::new();
        for (user, assistant) in history_content {
            history.push(Message::User(HistoryUserMessage::new(
                user,
                "claude-sonnet-4.5",
            )));
            history.push(Message::Assistant(HistoryAssistantMessage::new(assistant)));
        }
        ConversationState::new("test-conv")
            .with_current_message(CurrentMessage::new(UserInputMessage::new(
                current,
                "claude-sonnet-4.5",
            )))
            .with_history(history)
    }

    #[test]
    fn test_compress_whitespace_consecutive_empty_lines() {
        let input = "line1\n\n\n\n\nline2";
        let result = compress_whitespace(input);
        // 5 个空行 → 最多保留 2 个（line1 + 2 个 \n + line2）
        assert_eq!(result, "line1\n\n\nline2");
    }

    #[test]
    fn test_compress_whitespace_trailing_spaces() {
        let input = "hello   \nworld  ";
        let result = compress_whitespace(input);
        assert_eq!(result, "hello\nworld");
    }

    #[test]
    fn test_compress_whitespace_preserves_indentation() {
        let input = "    indented\n        more indented";
        let result = compress_whitespace(input);
        assert_eq!(result, "    indented\n        more indented");
    }

    /// 首尾对称：开头 ≤2 个空行与结尾一样保留。
    ///
    /// 旧实现用「result 非空」当分隔判定，首行空时 result 仍空 → 不 push，
    /// 开头空行被整段吃掉；结尾却保留 2 个 —— 同一份正文从首行还是尾行空起
    /// 不该有两种口径（首行缩进本来就由 `trim_end` 单独保护，与此无关）。
    #[test]
    fn test_compress_whitespace_keeps_leading_empty_lines() {
        assert_eq!(compress_whitespace("\n\ncode"), "\n\ncode");
        // 开头 3 个空行同样折叠到 2 个（与中间/结尾同口径）
        assert_eq!(compress_whitespace("\n\n\ncode"), "\n\ncode");
        // 首尾同时为空：各留 2 个
        assert_eq!(compress_whitespace("\n\na\n\n\n\n"), "\n\na\n\n");
    }

    #[test]
    fn test_safe_char_truncate_utf8() {
        let input = "你好世界abcd";
        let result = safe_char_truncate(input, 4);
        assert_eq!(result, "你好世界");
    }

    #[test]
    fn test_smart_truncate_short_content_unchanged() {
        let input = "short text";
        let (result, saved) = smart_truncate_by_lines(input, 100, 5, 3);
        assert_eq!(result, input);
        assert_eq!(saved, 0);
    }

    #[test]
    fn test_smart_truncate_preserves_head_tail() {
        let lines: Vec<String> = (0..200).map(|i| format!("line {}", i)).collect();
        let input = lines.join("\n");
        let (result, _saved) = smart_truncate_by_lines(&input, 100, 3, 2);
        assert!(result.starts_with("line 0\nline 1\nline 2\n"));
        assert!(result.ends_with("line 198\nline 199"));
        assert!(result.contains("行已省略"));
    }

    /// 行数不足（≤ head + tail）分支：走按字符各留一半的硬截断，结果仍必须 ≤ max_chars。
    ///
    /// 旧实现先按 max_chars 截头尾、再把省略标记拼在外面 → 最终长度恒为
    /// max_chars + 标记长度（约 20 字符），违反「截断后 ≤ max_chars」契约；且标记
    /// 写的是「{}行已省略」而实际传的是**字符数**，文案与语义不符。
    #[test]
    fn test_smart_truncate_few_lines_respects_max_chars() {
        let text = "a".repeat(500); // 单行、500 字符 → 落入行数不足分支
        let (result, saved) = smart_truncate_by_lines(&text, 100, 3, 3);
        assert!(
            result.chars().count() <= 100,
            "行数不足分支也必须 ≤ max_chars，实际 {}",
            result.chars().count()
        );
        assert!(
            result.contains("字符已省略"),
            "省略量是字符数，文案不得写「行」: {result}"
        );
        assert!(saved > 0, "截断应省下字节");
        assert!(result.starts_with("aaa") && result.ends_with("aaa"), "头尾都要保留");

        // 极小 max_chars（比标记还短）：只保留标记、正文全丢 —— 与大文本分支同约定
        // （不保证 ≤ max_chars，因为标记本身已超）。
        let (tiny, _) = smart_truncate_by_lines(&text, 1, 3, 3);
        assert!(
            tiny.starts_with("\n...[500字符已省略]..."),
            "正文应全丢、只留标记: {tiny}"
        );
        assert!(!tiny.contains("aaa"));
    }

    #[test]
    fn test_whitespace_pass_via_compress() {
        let content = "a\n\n\n\n\nb   ";
        let mut state = make_simple_state(vec![("hi", content)], "next");
        let config = CompressionConfig {
            whitespace_compression: true,
            tool_result_max_chars: 0,
            ..Default::default()
        };
        let stats = compress(&mut state, &config);
        assert!(stats.whitespace_saved > 0);
        if let Message::Assistant(a) = &state.history[1] {
            assert_eq!(a.assistant_response_message.content, "a\n\n\nb");
        } else {
            panic!("expected Assistant message");
        }
    }

    #[test]
    fn test_tool_result_truncation_head_tail() {
        // 构造 500 行文本作为超大 tool_result
        let long_text = (0..500)
            .map(|i| format!("row {}", i))
            .collect::<Vec<_>>()
            .join("\n");
        let mut state = ConversationState::new("test")
            .with_current_message(CurrentMessage::new(
                UserInputMessage::new("msg", "claude-sonnet-4.5").with_context(
                    UserInputMessageContext::new()
                        .with_tool_results(vec![ToolResult::success("t1", &long_text)]),
                ),
            ))
            .with_history(Vec::new());

        // max_chars 需大于「头 80 行 + 尾 40 行」的实际字符数（~870），否则会触发
        // 硬截断兜底把尾部也砍掉；真实默认为 8000，这里用 2500 足够容纳头尾结构。
        let config = CompressionConfig {
            enabled: true,
            trigger_bytes: 0,
            whitespace_compression: false,
            tool_result_max_chars: 2500,
            tool_result_head_lines: 80,
            tool_result_tail_lines: 40,
        };
        let stats = compress(&mut state, &config);
        assert!(stats.tool_result_saved > 0);

        let text = state
            .current_message
            .user_input_message
            .user_input_message_context
            .tool_results[0]
            .content[0]["text"]
            .as_str()
            .unwrap();
        // 保留头 80 行 + 尾 40 行
        assert!(text.starts_with("row 0\nrow 1\n"));
        assert!(text.contains("row 79"));
        assert!(text.contains("row 460"));
        assert!(text.ends_with("row 498\nrow 499"));
        assert!(text.contains("行已省略"));
    }

    #[test]
    fn test_small_body_unchanged() {
        // 小 tool_result 与普通文本不应被改动
        let mut state = ConversationState::new("test")
            .with_current_message(CurrentMessage::new(
                UserInputMessage::new("hello world", "claude-sonnet-4.5").with_context(
                    UserInputMessageContext::new()
                        .with_tool_results(vec![ToolResult::success("t1", "short result")]),
                ),
            ))
            .with_history(vec![
                Message::User(HistoryUserMessage::new("user text", "claude-sonnet-4.5")),
                Message::Assistant(HistoryAssistantMessage::new("assistant text")),
            ]);

        let config = CompressionConfig::default();
        let stats = compress(&mut state, &config);
        assert_eq!(stats.total_saved(), 0);

        assert_eq!(
            state.current_message.user_input_message.content,
            "hello world"
        );
        let text = state
            .current_message
            .user_input_message
            .user_input_message_context
            .tool_results[0]
            .content[0]["text"]
            .as_str()
            .unwrap();
        assert_eq!(text, "short result");
        if let Message::User(u) = &state.history[0] {
            assert_eq!(u.user_input_message.content, "user text");
        }
    }

    #[test]
    fn test_disabled_no_change() {
        let content = "line1\n\n\n\n\nline2   ";
        let mut state = make_simple_state(vec![("hi", content)], "next");
        let original = content.to_string();

        let config = CompressionConfig {
            enabled: false,
            ..Default::default()
        };
        let stats = compress(&mut state, &config);
        assert_eq!(stats.total_saved(), 0);
        if let Message::Assistant(a) = &state.history[1] {
            assert_eq!(a.assistant_response_message.content, original);
        }
    }

    #[test]
    fn test_whitespace_only_content_repaired_to_placeholder() {
        // 纯空白 content 经空白压缩后会变空，必须兜底为 "." 避免上游 400
        let mut state = make_simple_state(vec![("   \n\n", "assistant")], "current");
        let config = CompressionConfig {
            whitespace_compression: true,
            tool_result_max_chars: 0,
            ..Default::default()
        };
        let _stats = compress(&mut state, &config);
        if let Message::User(u) = &state.history[0] {
            assert_eq!(u.user_input_message.content, ".");
        } else {
            panic!("expected User message");
        }
    }

    #[test]
    fn test_space_placeholder_not_touched() {
        // converter 用 " " 作为「仅 tool_use」占位符，空白压缩不应把它压掉
        let saved = compress_string_field(&mut " ".to_string());
        assert_eq!(saved, 0);
    }

    #[test]
    fn test_tool_result_content_disabled_when_zero() {
        let long_text = (0..500)
            .map(|i| format!("row {}", i))
            .collect::<Vec<_>>()
            .join("\n");
        let original = long_text.clone();
        let mut state = ConversationState::new("test")
            .with_current_message(CurrentMessage::new(
                UserInputMessage::new("msg", "claude-sonnet-4.5").with_context(
                    UserInputMessageContext::new()
                        .with_tool_results(vec![ToolResult::success("t1", &long_text)]),
                ),
            ))
            .with_history(Vec::new());

        let config = CompressionConfig {
            enabled: true,
            trigger_bytes: 0,
            whitespace_compression: false,
            tool_result_max_chars: 0, // 关闭该层
            tool_result_head_lines: 80,
            tool_result_tail_lines: 40,
        };
        let stats = compress(&mut state, &config);
        assert_eq!(stats.tool_result_saved, 0);
        let text = state
            .current_message
            .user_input_message
            .user_input_message_context
            .tool_results[0]
            .content[0]["text"]
            .as_str()
            .unwrap();
        assert_eq!(text, original);
    }

    #[test]
    fn test_compress_long_messages_truncates_current_and_history() {
        let mut state = make_simple_state(
            vec![(&format!("user {}", "x".repeat(9000)), "assistant")],
            &"y".repeat(9000),
        );
        let saved = compress_long_messages_pass(&mut state, 2000);
        assert!(saved > 0);
        // 两条超长正文都应被截断到 ≤2000 字符
        assert!(state.current_message.user_input_message.content.chars().count() <= 2000);
        if let Message::User(u) = &state.history[0] {
            assert!(u.user_input_message.content.chars().count() <= 2000);
        }
        // 省略标记存在
        assert!(state.current_message.user_input_message.content.contains("chars omitted"));
    }

    #[test]
    fn test_compress_long_messages_short_unchanged() {
        let mut state = make_simple_state(vec![("hi", "hello")], "cur");
        let saved = compress_long_messages_pass(&mut state, 1000);
        assert_eq!(saved, 0);
        assert_eq!(state.current_message.user_input_message.content, "cur");
        if let Message::User(u) = &state.history[0] {
            assert_eq!(u.user_input_message.content, "hi");
        }
    }

    #[test]
    fn test_compress_long_messages_zero_disabled() {
        let mut state = make_simple_state(vec![("hi", "hello")], &"x".repeat(5000));
        let saved = compress_long_messages_pass(&mut state, 0);
        assert_eq!(saved, 0);
        assert_eq!(state.current_message.user_input_message.content.len(), 5000);
    }

    #[test]
    fn test_compress_long_messages_skips_space_placeholder() {
        let mut state = make_simple_state(vec![(" ", "a")], "cur");
        let saved = compress_long_messages_pass(&mut state, 1);
        assert_eq!(saved, 0);
        if let Message::User(u) = &state.history[0] {
            assert_eq!(u.user_input_message.content, " ");
        }
    }

    /// 清单 10 回归：带 images/tool_results 的消息 content 被压空后**也必须**补占位符。
    ///
    /// 旧代码的 has_payload 门只看「有无 images/tool_results」就跳过 content 修复 ——
    /// 但 content 空 + 带 payload 的消息同样违反 Kiro「content 非空」契约，上游照样 400。
    #[test]
    fn repair_non_empty_content_fixes_empty_content_even_with_images() {
        let mut history_user = HistoryUserMessage::new("", "claude-sonnet-4.5");
        history_user.user_input_message.images =
            vec![KiroImage::from_base64("png", "iVBORw0KGgo=")];
        let mut state = ConversationState::new("test")
            .with_current_message(CurrentMessage::new(
                UserInputMessage::new("", "claude-sonnet-4.5")
                    .with_images(vec![KiroImage::from_base64("png", "iVBORw0KGgo=")]),
            ))
            .with_history(vec![
                Message::User(history_user),
                Message::Assistant(HistoryAssistantMessage::new("assistant text")),
            ]);

        let repaired = repair_non_empty_content_pass(&mut state);
        assert_eq!(
            repaired, 2,
            "history user + current 两条带 images 的空 content 都必须补占位符（旧代码 has_payload 门放过）"
        );
        assert_eq!(state.current_message.user_input_message.content, ".");
        assert!(!state.current_message.user_input_message.images.is_empty());
        if let Message::User(u) = &state.history[0] {
            assert_eq!(u.user_input_message.content, ".");
            assert!(!u.user_input_message.images.is_empty());
        }
    }

    /// 对照：带 images 但 content 非空的消息不得被误补占位符
    /// （repair_content_field 只动 trim 后为空的字段）。
    #[test]
    fn repair_non_empty_content_skips_real_content_even_with_images() {
        let mut state = ConversationState::new("test")
            .with_current_message(CurrentMessage::new(
                UserInputMessage::new("real question", "claude-sonnet-4.5")
                    .with_images(vec![KiroImage::from_base64("png", "iVBORw0KGgo=")]),
            ))
            .with_history(Vec::new());

        let repaired = repair_non_empty_content_pass(&mut state);
        assert_eq!(repaired, 0);
        assert_eq!(
            state.current_message.user_input_message.content,
            "real question"
        );
    }

    /// 对照：带 tool_results 但 content 为空的历史 user 消息同样要补（has_payload 门
    /// 判据的另一个输入）。
    #[test]
    fn repair_non_empty_content_fixes_empty_content_with_tool_results() {
        let mut history_user = HistoryUserMessage::new("", "claude-sonnet-4.5");
        history_user.user_input_message.user_input_message_context =
            UserInputMessageContext::new()
                .with_tool_results(vec![ToolResult::success("t1", "output")]);
        let mut state = ConversationState::new("test")
            .with_current_message(CurrentMessage::new(UserInputMessage::new(
                "",
                "claude-sonnet-4.5",
            )))
            .with_history(vec![Message::User(history_user)]);

        let repaired = repair_non_empty_content_pass(&mut state);
        assert_eq!(
            repaired, 2,
            "带 tool_results 的空 content 也必须补占位符（旧代码 has_payload 门放过）"
        );
        if let Message::User(u) = &state.history[0] {
            assert_eq!(u.user_input_message.content, ".");
            assert!(!u.user_input_message.user_input_message_context.tool_results.is_empty());
        }
    }

    /// " " 是 converter 的「仅 tool_use」占位符，**不是**本 pass 要修的「压空」情形：
    /// 修空 content 的兜底不得把它改写成 "."（否则 compress_string_field /
    /// truncate_long_content 特意跳过它的写法就白做了）。
    #[test]
    fn repair_non_empty_content_keeps_space_placeholder() {
        let mut placeholder = " ".to_string();
        assert!(!repair_content_field(&mut placeholder));
        assert_eq!(placeholder, " ");

        // 对照：真正空 / 纯空白的字段仍要补 "."
        let mut empty = String::new();
        assert!(repair_content_field(&mut empty));
        assert_eq!(empty, ".");
        let mut blanks = "  \n ".to_string();
        assert!(repair_content_field(&mut blanks));
        assert_eq!(blanks, ".");
    }

    /// 同上的整趟回归：历史 user + current 都带 " " 占位符时，本 pass 一个都不许动。
    #[test]
    fn repair_non_empty_content_skips_space_placeholder_via_pass() {
        let mut state = make_simple_state(vec![(" ", "a")], " ");
        let repaired = repair_non_empty_content_pass(&mut state);
        assert_eq!(repaired, 0, "占位符 \" \" 不该被当成空 content 修掉");
        if let Message::User(u) = &state.history[0] {
            assert_eq!(u.user_input_message.content, " ");
        }
        assert_eq!(state.current_message.user_input_message.content, " ");
    }
}
