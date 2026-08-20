    //! output_tokens 记账口径：thinking 内容必须计入（与非流式 estimate 口径对齐），
    //! 且工具参数只在**实际下发**时累计（失败态不下发的不能凭空记账）。
    use super::*;

    #[test]
    fn reasoning_flow_counts_toward_output_tokens() {
        // thinking 是真实下发的生成内容：Anthropic 官方 usage 口径 output_tokens
        // 含 thinking tokens。旧实现只计正文与工具参数，此处钉住累加。
        let mut c = StreamContext::new_with_thinking("claude-sonnet-5", 100, true, HashMap::new());
        assert_eq!(c.output_tokens, 0);
        let ev = ReasoningContentEvent {
            text: "先想一下".to_string(),
            signature: None,
            redacted_content: None,
        };
        let events = c.process_reasoning_content(&ev);
        assert!(!events.is_empty(), "thinking 帧必须下发 thinking_delta");
        assert!(
            c.output_tokens > 0,
            "thinking 内容必须计入 output_tokens（修复前恒 0）"
        );
        assert_eq!(c.output_tokens, estimate_tokens("先想一下"));
    }

    #[test]
    fn reasoning_discarded_when_thinking_disabled_does_not_count() {
        // thinking 未开启：整帧丢弃（不下发），不得累计 output_tokens。
        let mut c = StreamContext::new_with_thinking("claude-sonnet-5", 100, false, HashMap::new());
        let ev = ReasoningContentEvent {
            text: "secret".to_string(),
            signature: None,
            redacted_content: None,
        };
        let events = c.process_reasoning_content(&ev);
        assert!(events.is_empty());
        assert_eq!(c.output_tokens, 0, "丢弃的 thinking 不得计 output_tokens");
    }

    fn start_tool_block(c: &mut StreamContext) -> i32 {
        let idx = c.state_manager.next_block_index();
        c.state_manager.handle_content_block_start(
            idx,
            "tool_use",
            json!({
                "type": "content_block_start",
                "index": idx,
                "content_block": {"type": "tool_use"}
            }),
        );
        idx
    }

    #[test]
    fn flush_tool_input_counts_only_when_delivered() {
        // 合法 JSON：实际下发 → 计入 output_tokens。
        let mut c = StreamContext::new_with_thinking("claude-sonnet-5", 100, true, HashMap::new());
        let idx = start_tool_block(&mut c);
        let good = r#"{"command":"ls"}"#;
        let events = c.flush_tool_input(idx, good.to_string());
        assert!(!events.is_empty(), "合法 JSON 必须下发 input_json_delta");
        assert_eq!(c.output_tokens, (good.len() as i32 + 3) / 4);
    }

    #[test]
    fn flush_tool_input_failure_path_does_not_count() {
        // 非法 JSON 且修复层也补不回：②失败态对齐置态 → 不下发坏参数 →
        // 不得累计 output_tokens（修复前在函数入口就累计，面板凭空记 output）。
        let bad = r#"{"a": 1} trailing"#;
        assert!(
            repair_tool_json(bad).is_none(),
            "前置：该串必须修不好（否则走的是修复下发路径）"
        );
        let mut c = StreamContext::new_with_thinking("claude-sonnet-5", 100, true, HashMap::new());
        let idx = start_tool_block(&mut c);
        let events = c.flush_tool_input(idx, bad.to_string());
        assert!(events.is_empty(), "失败路径必须不下发坏 JSON");
        assert!(
            !c.completion().is_ok(),
            "失败路径必须已置失败态（②默认开）"
        );
        assert_eq!(
            c.output_tokens, 0,
            "失败路径不得累计 output_tokens（旧实现函数入口即累计）"
        );
    }

    #[test]
    fn sniffed_inline_thinking_counts_once() {
        // 内联标签形态的嗅探流（thinking_enabled）：thinking 文本必须只计一次。
        // 修复前入口对整块计一次、提取处各分支又计一次 → output_tokens 约 1.8 倍
        // 虚高。整块一次喂入时提取后实际下发恰好等于入口整块估算，严格断言相等
        // 即钉死「只计一次」（双计必 > 整块估算）。
        let mut c = StreamContext::new_with_thinking("claude-sonnet-5", 100, true, HashMap::new());
        let block = "<thinking>让我仔细权衡两种方案的利弊</thinking>\n\n答案是 42";
        let events = c.process_assistant_response(block);
        assert!(!events.is_empty(), "thinking 与正文都必须下发");
        assert_eq!(
            c.output_tokens,
            estimate_tokens(block),
            "嗅探路径必须只计一次：入口整块估算，提取处不得重复累加"
        );
    }

    #[test]
    fn sniffed_thinking_only_keeps_near_empty_detection() {
        // 大输入 + 只有 thinking 块、无正文：output_tokens 必须仍 < 30（近空判据），
        // is_empty_response 判 true。构造使「单计 < 30 而双计 ≥ 30」：
        // 修复前双计把 output 推到 30+，近空判定失效 → 200 当成功（回归表现）。
        let model = "claude-sonnet-5";
        let threshold = empty_response_oversized_threshold(model);
        let mut c = StreamContext::new_with_thinking(model, threshold, true, HashMap::new());
        let block = "<thinking>甲乙丙丁戊己庚辛壬癸子丑寅卯辰巳午未</thinking>\n\n";
        assert!(estimate_tokens(block) < NEAR_EMPTY_OUTPUT_THRESHOLD);
        let events = c.process_assistant_response(block);
        assert!(!events.is_empty(), "thinking 块必须下发");
        assert!(c.output_tokens < NEAR_EMPTY_OUTPUT_THRESHOLD);
        assert!(
            c.is_empty_response(),
            "大输入 + thinking-only 短响应必须判近空（双计后此断言失效）"
        );
    }
