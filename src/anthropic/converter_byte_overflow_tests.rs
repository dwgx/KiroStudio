    use super::*;
    use super::history_overflow::{drop_orphan_tool_results, state_size};
    use crate::kiro::model::requests::conversation::{
        HistoryAssistantMessage, HistoryUserMessage, UserInputMessageContext,
    };
    use crate::kiro::model::requests::tool::{ToolResult, ToolUseEntry};

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

    fn assistant_with_tool(content: &str, id: &str) -> Message {
        let m = HistoryAssistantMessage::new(content);
        let mut m = m;
        m.assistant_response_message.tool_uses = Some(vec![ToolUseEntry {
            tool_use_id: id.to_string(),
            name: "read_file".to_string(),
            input: serde_json::json!({"path": "a.rs"}),
        }]);
        Message::Assistant(m)
    }

    fn user_with_result(content: &str, id: &str) -> Message {
        let mut m = HistoryUserMessage::new(content, "auto");
        let mut ctx = UserInputMessageContext::default();
        ctx.tool_results = vec![ToolResult {
            tool_use_id: id.to_string(),
            content: vec![],
            status: Some("success".to_string()),
            is_error: false,
        }];
        m.user_input_message.user_input_message_context = ctx;
        Message::User(m)
    }

    /// 收集保留历史里出现的所有 toolResult，断言每个都有对应的 toolUse。
    fn assert_no_orphan_tool_results(state: &ConversationState) {
        let known: std::collections::HashSet<String> = state
            .history
            .iter()
            .filter_map(|m| match m {
                Message::Assistant(a) => a.assistant_response_message.tool_uses.as_ref(),
                _ => None,
            })
            .flatten()
            .map(|t| t.tool_use_id.clone())
            .collect();
        for m in &state.history {
            if let Message::User(u) = m {
                for r in &u.user_input_message.user_input_message_context.tool_results {
                    assert!(
                        known.contains(&r.tool_use_id),
                        "孤立 toolResult {} 无对应 toolUse，会触发上游 400",
                        r.tool_use_id
                    );
                }
            }
        }
    }

    /// 断言 user/assistant 严格交替（上游要求，否则 400 REQUEST_BODY_INVALID）。
    fn assert_alternation(history: &[Message]) {
        for (i, w) in history.windows(2).enumerate() {
            let same = matches!(
                (&w[0], &w[1]),
                (Message::User(_), Message::User(_))
                    | (Message::Assistant(_), Message::Assistant(_))
            );
            assert!(!same, "位置 {i} 出现连续同角色消息，会导致上游 400");
        }
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
        //
        // 尺寸核算（Review m5）：20 条 × 130KB ≈ 2.6MB。keep_from 循环逐条累加，
        // running 在 i=13（kept=7）时 > 900KB 且 kept > 4 → break，keep_from=14，
        // 索引 14 是 assistant（偶数位）→ tail 以 assistant 开头 → drop_leading
        // 必执行。旧值 150KB 时 break 恰落在 user 上（i=15, kept=6，150×6=900 未超
        // 900KB），drop_leading 从未触发，测试恒绿但测不到目标分支。
        let big = "y".repeat(130 * 1024);
        let mut hist = Vec::new();
        for _ in 0..10 {
            hist.push(assistant(&big));
            hist.push(user(&big));
        }
        let mut s = state_with(hist);
        truncate_history_if_needed(&mut s, "auto");

        // 占位(user) 之后紧跟应答(assistant)，再往后才是保留的 tail(user 开头)，
        // 这样才满足上游的严格交替要求。
        assert_alternation(&s.history);
        let rest: Vec<&Message> = s.history.iter().skip(1).collect();
        if let Some(first) = rest.first() {
            assert!(
                matches!(first, Message::Assistant(_)),
                "占位之后必须紧跟 assistant 应答，否则形成 user+user 触发上游 400"
            );
        }
        // drop_leading 执行证据：tail 首条必须是 user（若未删除开头 assistant，
        // 交替断言已会红；这里再钉一次首条形状）。
        match s.history.get(2) {
            Some(Message::User(m)) => assert!(
                !m.user_input_message.content.is_empty(),
                "tail 首条应为真实 user 消息"
            ),
            other => panic!("tail 首条应为 user（drop_leading 未执行），实际: {other:?}"),
        }
    }

    #[test]
    fn test_empty_history_is_noop() {
        let mut s = state_with(vec![]);
        assert_eq!(truncate_history_if_needed(&mut s, "auto"), 0);
    }

    #[test]
    fn test_alternation_preserved_after_truncation() {
        let big = "z".repeat(120 * 1024);
        let mut hist = Vec::new();
        for _ in 0..10 {
            hist.push(Message::User(HistoryUserMessage::new(&big, "auto")));
            hist.push(Message::Assistant(HistoryAssistantMessage::new(&big)));
        }
        let mut s = state_with(hist);
        truncate_history_if_needed(&mut s, "auto");

        assert!(!s.history.is_empty(), "截断后不得清空历史");
        assert_alternation(&s.history);
        // 占位必须保留（keep_from > 0 时插入），不能只删不补。
        match s.history.first() {
            Some(Message::User(m)) => assert_eq!(
                m.user_input_message.content, TRUNCATION_PLACEHOLDER,
                "截断后首条应为占位 user"
            ),
            _ => panic!("截断后首条应为占位 user"),
        }
    }

    /// 真实 Codex 会话形态：user → assistant(toolUse) → user(toolResult) → ...
    /// 按字节截断会切断配对，留下无主 toolResults → 上游 400 REQUEST_BODY_INVALID。
    #[test]
    fn test_no_orphan_tool_results_after_truncation() {
        let big = "q".repeat(100 * 1024);
        let mut hist = Vec::new();
        for i in 0..12 {
            let id = format!("tu_{i}");
            hist.push(Message::User(HistoryUserMessage::new(&big, "auto")));
            hist.push(assistant_with_tool(&big, &id));
            hist.push(user_with_result(&big, &id));
            hist.push(Message::Assistant(HistoryAssistantMessage::new(&big)));
        }
        let mut s = state_with(hist);
        let dropped = truncate_history_if_needed(&mut s, "auto");
        assert!(dropped > 0, "超限请求必须被截断");

        assert_no_orphan_tool_results(&s);
        assert_alternation(&s.history);
    }

    /// 切口落在配对中间：tail 以「引用了已丢弃 toolUse 的 user」开头。
    #[test]
    fn test_drops_orphan_leading_result() {
        let tail = vec![
            user_with_result("result", "tu_gone"), // 孤立：tu_gone 的 toolUse 已被丢
            Message::Assistant(HistoryAssistantMessage::new("ok")),
            Message::User(HistoryUserMessage::new("next", "auto")),
        ];
        let out = drop_orphan_tool_results(tail);
        if let Some(Message::User(u)) = out.first() {
            let rs = &u.user_input_message.user_input_message_context.tool_results;
            assert!(
                rs.is_empty() || rs.iter().all(|r| r.tool_use_id != "tu_gone"),
                "孤立 toolResult 未被清理"
            );
        }
    }

    /// 配对完整时不得误删。
    ///
    /// 注意：orphan 判定只查**首条** user。首条是无 toolResults 的普通 user 时，
    /// 即使其后有完整配对也整段保留——这正是「配对完整不误删」的正确形态
    /// （旧构造以 assistant 开头，orphan 判定对 assistant 恒为 false，测试恒绿无意义）。
    #[test]
    fn test_keeps_paired_results() {
        let tail = vec![
            Message::User(HistoryUserMessage::new("plain", "auto")),
            assistant_with_tool("calling", "tu_1"),
            user_with_result("result", "tu_1"),
        ];
        let out = drop_orphan_tool_results(tail);
        assert_eq!(out.len(), 3, "配对完整的历史不应被删");
    }

    /// 连锁删除行为测试（Review M1）：独立调用 `truncate_history_if_needed`（未先
    /// sanitize）时，切口处的孤立 user 会被删除，且连坐删除紧随的 assistant，
    /// 使保留段内配对完整的下一轮变成孤立 → 连锁。这是参考仓同款行为，生产路径
    /// 由 `apply_byte_overflow_guard` 的「sanitize 先行」规避（截断前无残留结构化
    /// 工具轮次，本路径成死代码）；本测试钉住的是：**不变量不破**——连锁删除后
    /// 仍无孤立 toolResult、交替仍保持（只是过度丢弃了保留段的完整配对）。
    #[test]
    fn test_drop_orphan_chain_reaction_keeps_invariants() {
        let tail = vec![
            user_with_result("result", "tu_gone"), // 孤立：toolUse 已在切口前被丢
            assistant_with_tool("calling", "tu_kept"),
            user_with_result("result", "tu_kept"),
            Message::User(HistoryUserMessage::new("plain", "auto")),
        ];
        let out = drop_orphan_tool_results(tail);
        assert_alternation(&out);
        // 无孤立 toolResult：保留段出现的每个 result 都必须在保留段内有对应 toolUse。
        let known: std::collections::HashSet<String> = out
            .iter()
            .filter_map(|m| match m {
                Message::Assistant(a) => a.assistant_response_message.tool_uses.as_ref(),
                _ => None,
            })
            .flatten()
            .map(|t| t.tool_use_id.clone())
            .collect();
        for m in &out {
            if let Message::User(u) = m {
                for r in &u.user_input_message.user_input_message_context.tool_results {
                    assert!(
                        known.contains(&r.tool_use_id),
                        "连锁删除后仍不得残留孤立 toolResult {}",
                        r.tool_use_id
                    );
                }
            }
        }
    }

    // ============ sanitize_history（旧工具轮次扁平化） ============

    fn a_with_use(id: &str, name: &str) -> Message {
        let mut a = HistoryAssistantMessage::new("calling");
        a.assistant_response_message.tool_uses = Some(vec![ToolUseEntry {
            tool_use_id: id.into(),
            name: name.into(),
            input: serde_json::json!({}),
        }]);
        Message::Assistant(a)
    }

    fn u_with_result_content(id: &str, text: &str) -> Message {
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
            u_with_result_content("tu_1", "file contents"),
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
                    u.user_input_message
                        .user_input_message_context
                        .tool_results
                        .is_empty(),
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
            u_with_result_content("tu_old", "old result"),
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
        let mut h = vec![a_with_use("tu_1", "read_file"), u_with_result_content("tu_1", "x")];
        sanitize_history(&mut h, &HashSet::new());
        if let Message::Assistant(a) = &h[0] {
            let c = &a.assistant_response_message.content;
            for bad in ["[Called", "read_file", "tool_use", "invoke"] {
                assert!(
                    !c.contains(bad),
                    "assistant 正文不得叙述工具调用，命中 {bad}: {c}"
                );
            }
        }
    }

    // ============ 触发点端到端：压缩重试路径的字节兜底 ============

    /// 兜底组合：sanitize（活跃轮次保持结构化、其余扁平化）→ 字节截断
    /// （≤ MAX_PAYLOAD_BYTES + 占位 + 交替 + 无孤立 toolResult）。
    #[test]
    fn test_apply_byte_overflow_guard_end_to_end() {
        let big = "m".repeat(90 * 1024);
        let mut hist = Vec::new();
        // 10 组完整工具轮次（含配对），总大小远超 900KB。
        for i in 0..10 {
            let id = format!("tu_{i}");
            hist.push(Message::User(HistoryUserMessage::new(&big, "auto")));
            hist.push(assistant_with_tool(&big, &id));
            hist.push(user_with_result(&big, &id));
            hist.push(Message::Assistant(HistoryAssistantMessage::new(&big)));
        }
        let mut s = state_with(hist);
        // 当前消息携带活跃轮次的 toolResults（tu_active 的历史 toolUse 不存在 →
        // 无活跃轮次，全部扁平化；这只影响结构化与否，不影响截断）。
        s.current_message
            .user_input_message
            .user_input_message_context
            .tool_results
            .push(ToolResult::success("tu_active", "out"));

        apply_byte_overflow_guard(&mut s);

        assert!(
            state_size(&s) <= MAX_PAYLOAD_BYTES,
            "字节兜底后仍超限: {} > {}",
            state_size(&s),
            MAX_PAYLOAD_BYTES
        );
        match s.history.first() {
            Some(Message::User(m)) => assert_eq!(
                m.user_input_message.content, TRUNCATION_PLACEHOLDER,
                "被丢弃处应插入占位说明"
            ),
            _ => panic!("首条应为占位 user 消息"),
        }
        assert_alternation(&s.history);
        assert_no_orphan_tool_results(&s);
    }

    /// 幂等：二次调用不得再改动（截断后 size ≤ 上限 → no-op）。
    #[test]
    fn test_apply_byte_overflow_guard_idempotent() {
        let big = "n".repeat(150 * 1024);
        let mut hist = Vec::new();
        for i in 0..6 {
            hist.push(Message::User(HistoryUserMessage::new(&big, "auto")));
            hist.push(assistant_with_tool(&big, &format!("tu_{i}")));
            hist.push(user_with_result(&big, &format!("tu_{i}")));
        }
        let mut s = state_with(hist);
        apply_byte_overflow_guard(&mut s);
        let first_pass = s.history.clone();
        let first_size = state_size(&s);

        apply_byte_overflow_guard(&mut s);
        assert_eq!(s.history.len(), first_pass.len(), "二次兜底不得再删历史");
        assert_eq!(state_size(&s), first_size, "二次兜底不得再改动体积");
    }

    /// 小请求（≤ 900KB）：兜底完全不动历史。
    #[test]
    fn test_apply_byte_overflow_guard_small_request_unchanged() {
        let mut s = state_with(vec![user("hi"), assistant("hello")]);
        apply_byte_overflow_guard(&mut s);
        assert_eq!(s.history.len(), 2, "小请求不应被改动");
    }
