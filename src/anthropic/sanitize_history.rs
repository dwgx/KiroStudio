//! 历史中结构化工具调用的扁平化
//!
//! 上游只接受**一个活跃工具轮次**：最后一条 history assistant 的 `toolUses`
//! ⟺ 当前消息的 `toolResults`。历史里残留多组结构化 toolUses/toolResults 会被判
//! `400 REQUEST_BODY_INVALID`（kiro-go `proxy/translator.go` 的
//! `sanitizeKiroHistory` 注释明确了这一约束）。
//!
//! 本模块把历史里除活跃轮次外的所有结构化工具调用叙述为文本：
//! - assistant 的 `toolUses` 直接清空，**不**写入任何「调用了工具 X」的文本；
//! - user 的 `toolResults` 转成 `[工具名] 输出` 形式并入正文。
//!
//! 顺带大幅缩小请求体（结构化 JSON 比纯文本冗余得多），从根上降低触发截断的频率。
//!
//! # 两个反模式（照抄 kiro-go 踩坑结论，勿"优化"掉）
//!
//! 1. **不叙述工具调用**：若在 assistant 侧写 `[Called tool X ...]`，长历史会给模型
//!    几十个「用文本调用工具」的范例，它会开始模仿而不再发真正的结构化调用。工具
//!    身份改从结果侧用 `tool_use_id → name` 映射保留。
//! 2. **不回填占位符**：被清空的 assistant 轮次不要填 `"."` 之类占位，否则历史里
//!    出现几十个 `"."` 回复，模型同样会模仿。

use std::collections::{HashMap, HashSet};

use crate::kiro::model::requests::conversation::Message;

/// 把历史里的结构化工具调用扁平化为文本，只保留活跃轮次。
///
/// `current_tool_result_ids` 是**当前**消息携带的 `tool_use_id` 集合。当历史最后一条
/// 是 assistant 且其 toolUses 被该集合完全覆盖时，这条保持结构化（即活跃轮次）。
pub fn sanitize_history(history: &mut [Message], current_tool_result_ids: &HashSet<String>) {
    if history.is_empty() {
        return;
    }

    // 快速检查：历史里是否有任何工具调用/结果。无则跳过（避免无谓遍历）。
    let has_tools = history.iter().any(|m| match m {
        Message::Assistant(a) => a
            .assistant_response_message
            .tool_uses
            .as_ref()
            .is_some_and(|uses| !uses.is_empty()),
        Message::User(u) => !u
            .user_input_message
            .user_input_message_context
            .tool_results
            .is_empty(),
    });
    if !has_tools {
        return;
    }

    // 先建 tool_use_id → 工具名 的全量映射：即便某轮的 toolUses 被清空，
    // 其结果侧仍能标出来源工具。
    let mut tool_names: HashMap<String, String> = HashMap::new();
    for m in history.iter() {
        if let Message::Assistant(a) = m {
            if let Some(uses) = &a.assistant_response_message.tool_uses {
                for tu in uses {
                    if !tu.tool_use_id.is_empty() && !tu.name.is_empty() {
                        tool_names.insert(tu.tool_use_id.clone(), tu.name.clone());
                    }
                }
            }
        }
    }

    // 判定活跃轮次：最后一条 assistant 的 toolUses 全部被当前 toolResults 应答。
    let active_idx: Option<usize> = if current_tool_result_ids.is_empty() {
        None
    } else {
        let last = history.len() - 1;
        match &history[last] {
            Message::Assistant(a) => match &a.assistant_response_message.tool_uses {
                Some(uses) if !uses.is_empty() => uses
                    .iter()
                    .all(|tu| current_tool_result_ids.contains(&tu.tool_use_id))
                    .then_some(last),
                _ => None,
            },
            _ => None,
        }
    };

    for (i, m) in history.iter_mut().enumerate() {
        match m {
            Message::Assistant(a) => {
                if Some(i) == active_idx {
                    continue; // 活跃轮次保持结构化
                }
                // 清空结构化调用，且不写任何调用叙述（见模块文档反模式 1）。
                a.assistant_response_message.tool_uses = None;
            }
            Message::User(u) => {
                let ctx = &mut u.user_input_message.user_input_message_context;
                if !ctx.tool_results.is_empty() {
                    let narrated = narrate_tool_results(&ctx.tool_results, &tool_names);
                    if !narrated.is_empty() {
                        let content = &mut u.user_input_message.content;
                        if content.trim().is_empty() {
                            *content = narrated;
                        } else {
                            content.push_str("\n\n");
                            content.push_str(&narrated);
                        }
                    }
                    ctx.tool_results.clear();
                }
                // 历史条目不该携带工具规格（只有当前消息需要）。
                ctx.tools.clear();
            }
        }
    }
}

/// 把 toolResults 叙述成 `[工具名] 输出` 形式的文本。
fn narrate_tool_results(
    results: &[crate::kiro::model::requests::tool::ToolResult],
    names: &HashMap<String, String>,
) -> String {
    let mut parts: Vec<String> = Vec::with_capacity(results.len());
    for r in results {
        let mut texts: Vec<&str> = Vec::new();
        for c in &r.content {
            if let Some(t) = c.get("text").and_then(|v| v.as_str()) {
                if !t.trim().is_empty() {
                    texts.push(t);
                }
            }
        }
        let body = if texts.is_empty() {
            "(no output)".to_string()
        } else {
            texts.join("\n")
        };
        match names.get(&r.tool_use_id) {
            Some(name) if !name.is_empty() => parts.push(format!("[{name}] {body}")),
            _ => parts.push(body),
        }
    }
    parts.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kiro::model::requests::conversation::{
        HistoryAssistantMessage, HistoryUserMessage, UserInputMessageContext,
    };
    use crate::kiro::model::requests::tool::{ToolResult, ToolUseEntry};

    fn a_with_use(id: &str, name: &str) -> Message {
        let mut a = HistoryAssistantMessage::new("calling");
        a.assistant_response_message.tool_uses = Some(vec![ToolUseEntry {
            tool_use_id: id.into(),
            name: name.into(),
            input: serde_json::json!({}),
        }]);
        Message::Assistant(a)
    }

    fn u_with_result(id: &str, text: &str) -> Message {
        let mut m = HistoryUserMessage::new("", "auto");
        let mut ctx = UserInputMessageContext::default();
        let mut c = serde_json::Map::new();
        c.insert("text".into(), serde_json::json!(text));
        ctx.tool_results = vec![ToolResult {
            tool_use_id: id.into(),
            content: vec![c],
            status: Some("success".into()),
            is_error: false,
        }];
        m.user_input_message.user_input_message_context = ctx;
        Message::User(m)
    }

    /// 无活跃轮次时：所有结构化工具调用都被扁平化。
    #[test]
    fn test_flattens_all_when_no_active_turn() {
        let mut h = vec![
            a_with_use("tu_1", "read_file"),
            u_with_result("tu_1", "file contents"),
        ];
        sanitize_history(&mut h, &HashSet::new());

        match &h[0] {
            Message::Assistant(a) => assert!(
                a.assistant_response_message.tool_uses.is_none(),
                "非活跃轮次的 toolUses 必须清空"
            ),
            _ => panic!("shape"),
        }
        match &h[1] {
            Message::User(u) => {
                assert!(
                    u.user_input_message.user_input_message_context.tool_results.is_empty(),
                    "toolResults 必须被扁平化"
                );
                let c = &u.user_input_message.content;
                assert!(c.contains("file contents"), "结果文本须并入正文: {c}");
                assert!(c.contains("read_file"), "须标出来源工具名: {c}");
            }
            _ => panic!("shape"),
        }
    }

    /// 活跃轮次（末条 assistant 被当前 toolResults 应答）保持结构化。
    #[test]
    fn test_keeps_active_tool_turn_structured() {
        let mut h = vec![
            u_with_result("tu_old", "old result"),
            a_with_use("tu_now", "grep"),
        ];
        let mut ids = HashSet::new();
        ids.insert("tu_now".to_string());
        sanitize_history(&mut h, &ids);

        match &h[1] {
            Message::Assistant(a) => {
                let uses = a.assistant_response_message.tool_uses.as_ref();
                assert!(uses.is_some(), "活跃轮次必须保持结构化");
                assert_eq!(uses.unwrap()[0].tool_use_id, "tu_now");
            }
            _ => panic!("shape"),
        }
    }

    /// 反模式守卫：扁平化后 assistant 正文不得出现工具调用叙述，
    /// 否则模型会模仿「用文本调用工具」而不再发结构化调用。
    #[test]
    fn test_no_tool_call_narration_in_assistant() {
        let mut h = vec![a_with_use("tu_1", "read_file"), u_with_result("tu_1", "x")];
        sanitize_history(&mut h, &HashSet::new());
        if let Message::Assistant(a) = &h[0] {
            let c = &a.assistant_response_message.content;
            for bad in ["[Called", "read_file", "tool_use", "invoke"] {
                assert!(!c.contains(bad), "assistant 正文不得叙述工具调用，命中 {bad}: {c}");
            }
        }
    }
}
