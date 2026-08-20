    use super::*;

    /// 测试辅助：把 [`ThinkingTagMatch`] 压成起始字节位置。
    ///
    /// 查找函数改为返回「起点 + **实际**长度」（大小写不敏感 + 容属性后标签不再定长），
    /// 而既有位置断言锁的是起点语义 —— 用这个投影保留它们的原值，不改判据强度。
    fn tag_start(m: Option<ThinkingTagMatch>) -> Option<usize> {
        m.map(|m| m.start)
    }

    // ========================================================================
    // 文本化 invoke 解析纯函数单测（从 ZyphrZero/kiro.rs 移植 + KiroStudio 补充）
    // 这些函数是纯函数，直接对字符串断言，不经过 StreamContext 状态机。
    // 命名统一含 `invoke`，便于 `cargo test -- invoke` 精准挑选。
    // ========================================================================

    #[test]
    fn test_invoke_parse_complete_block() {
        // 🟢 完整块：<invoke name="Bash"><parameter name="command">ls</parameter></invoke>
        let block = r#"<invoke name="Bash"><parameter name="command">ls</parameter></invoke>"#;
        let (name, input) = parse_invoke_block(block).expect("应解析出 tool");
        assert_eq!(name, "Bash");
        let parsed: serde_json::Value = serde_json::from_str(&input).expect("input 应为合法 JSON");
        assert_eq!(parsed["command"], "ls");
    }

    #[test]
    fn test_invoke_parse_antml_prefix_tolerated() {
        // 🟢 带 antml: 命名空间前缀应被容忍（开/闭标签均带前缀）。
        // 用拼接构造标签，避免源码里出现字面工具调用标记。
        let ns = "antml:";
        let block = format!(
            "<{ns}invoke name=\"X\"><{ns}parameter name=\"y\">v</{ns}parameter></{ns}invoke>"
        );
        let (name, input) = parse_invoke_block(&block).expect("带前缀的块应能解析");
        assert_eq!(name, "X");
        let parsed: serde_json::Value = serde_json::from_str(&input).expect("input 应为合法 JSON");
        assert_eq!(parsed["y"], "v");
    }

    #[test]
    fn test_invoke_parse_param_value_with_lt_multiline_chinese() {
        // 🟢 参数值含 `<`、多行、中文 → 不被截断
        let value = "第一行 a < b\n第二行 路径 /tmp/中文";
        let block = format!(
            "<invoke name=\"write_file\"><parameter name=\"content\">{value}</parameter></invoke>"
        );
        let (name, input) = parse_invoke_block(&block).expect("应解析出 tool");
        assert_eq!(name, "write_file");
        let parsed: serde_json::Value = serde_json::from_str(&input).expect("input 应为合法 JSON");
        assert_eq!(
            parsed["content"], value,
            "参数值应完整保留（含 < / 多行 / 中文）"
        );
    }

    #[test]
    fn test_invoke_parse_apply_patch_literal_close_tag_survives() {
        // 🟢 P0-1：apply_patch 正文里含字面 </parameter> —— 贪婪取最后一个闭合，不被提前截断。
        let closing = format!("</{}>", "parameter");
        let value = format!("patch line 1\n此处有字面 {closing} 标记\npatch line 3");
        let block = format!(
            "<invoke name=\"apply_patch\"><parameter name=\"input\">{value}</parameter></invoke>"
        );
        let (name, input) = parse_invoke_block(&block).expect("应解析出 tool");
        assert_eq!(name, "apply_patch");
        let parsed: serde_json::Value = serde_json::from_str(&input).expect("input 应为合法 JSON");
        assert_eq!(parsed["input"], value, "含字面闭合标签的正文应完整保留");
    }

    #[test]
    fn test_invoke_parse_two_params() {
        // 🟢 多参数：按下一个参数开标签正确切分
        let block = r#"<invoke name="t"><parameter name="a">1</parameter><parameter name="b">2</parameter></invoke>"#;
        let (name, input) = parse_invoke_block(block).expect("应解析出 tool");
        assert_eq!(name, "t");
        let parsed: serde_json::Value = serde_json::from_str(&input).expect("input 应为合法 JSON");
        assert_eq!(parsed["a"], "1");
        assert_eq!(parsed["b"], "2");
    }

    #[test]
    fn test_invoke_parse_no_params() {
        // 🟢 零参数块 → 合法但 input 为空对象
        let block = r#"<invoke name="noop"></invoke>"#;
        let (name, input) = parse_invoke_block(block).expect("应解析出 tool");
        assert_eq!(name, "noop");
        assert_eq!(input, "{}");
    }

    #[test]
    fn test_invoke_parse_empty_name_rejected() {
        // 🔴 name 为空 → None
        let block = r#"<invoke name=""><parameter name="x">v</parameter></invoke>"#;
        assert!(parse_invoke_block(block).is_none(), "空 name 应被拒绝");
    }

    #[test]
    fn test_invoke_find_start_bare_and_prefixed() {
        // 🟢 裸 `<invoke` 与带前缀 `<prefix:invoke` 都能定位到 '<'
        assert_eq!(find_invoke_start("<invoke name=\"x\">"), Some(0));
        assert_eq!(find_invoke_start("abc\n<invoke name=\"x\">"), Some(4));
        let prefixed = "<invoke name=\"x\">";
        assert_eq!(find_invoke_start(prefixed), Some(0));
    }

    #[test]
    fn test_invoke_find_start_backtick_wrapped_is_skipped() {
        // 🔴 被反引号包裹的 <invoke 视为引用，跳过
        assert_eq!(find_invoke_start("示例：`<invoke name=\"x\">`"), None);
    }

    #[test]
    fn test_invoke_find_start_ignores_invoked_word() {
        // 🔴 `invoked` 这类词不构成开标签（标签名后需边界字符）
        assert_eq!(find_invoke_start("the model invoked a tool"), None);
    }

    #[test]
    fn test_invoke_block_end_greedy_and_unclosed() {
        // 🟢 完整块 → 返回含闭标签的结束位置；未闭合 → None
        let full = r#"<invoke name="x"><parameter name="c">ls</parameter></invoke>"#;
        let end = find_invoke_block_end(full, 0).expect("完整块应有结束位置");
        assert_eq!(end, full.len());

        let unclosed = r#"<invoke name="x"><parameter name="c">ls"#;
        assert!(
            find_invoke_block_end(unclosed, 0).is_none(),
            "未闭合块应返回 None"
        );
    }

    #[test]
    fn test_invoke_next_open_finds_second_burst() {
        // 🟢 连发 burst：A 紧跟 B，find_next_invoke_open 跳过 A 自身开标签，定位到 B
        let s = r#"<invoke name="a"><parameter name="x">1</parameter></invoke><invoke name="b"><parameter name="y">2</parameter></invoke>"#;
        let b_pos = find_next_invoke_open(s, 0).expect("应找到第二个块开标签");
        assert_eq!(
            &s[b_pos..b_pos + "<invoke name=\"b\"".len()],
            "<invoke name=\"b\""
        );
    }

    #[test]
    fn test_invoke_two_blocks_parsed_via_block_end() {
        // 🟢 用 find_invoke_block_end + parse_invoke_block 串起两块，各自独立解析
        let s = r#"<invoke name="a"><parameter name="x">1</parameter></invoke><invoke name="b"><parameter name="y">2</parameter></invoke>"#;
        let start_a = find_invoke_start(s).unwrap();
        let end_a = find_invoke_block_end(s, start_a).unwrap();
        let (na, _) = parse_invoke_block(&s[start_a..end_a]).unwrap();
        assert_eq!(na, "a");

        let start_b = find_next_invoke_open(s, start_a).unwrap();
        let end_b = find_invoke_block_end(s, start_b).unwrap();
        let (nb, _) = parse_invoke_block(&s[start_b..end_b]).unwrap();
        assert_eq!(nb, "b");
        assert_eq!(end_b, s.len());
    }

    #[test]
    fn test_invoke_last_close_greedy_skips_literal() {
        // 🟢 区间内含字面 </invoke> → find_last_invoke_close 取最后一个真闭合
        let s = format!(
            "<invoke name=\"x\"><parameter name=\"c\">正文里有字面 {} 标记</parameter></invoke>",
            "</invoke>"
        );
        let end = find_last_invoke_close(&s, 0, s.len()).expect("应找到最后一个闭合");
        assert_eq!(end, s.len());
    }

    #[test]
    fn test_invoke_open_tag_lt_pos_bare_and_prefixed() {
        // 🟢 open_tag_lt_pos：裸 `<tag` 与 `<prefix:tag` 都能回溯到 '<'
        let bare = "<invoke";
        let name_pos = bare.find("invoke").unwrap();
        assert_eq!(open_tag_lt_pos(bare, name_pos), Some(0));

        let prefixed = "<invoke";
        let np = prefixed.find("invoke").unwrap();
        assert_eq!(open_tag_lt_pos(prefixed, np), Some(0));

        // 前面不是 '<' 也不是合法前缀 → None
        let bad = "xinvoke";
        let bp = bad.find("invoke").unwrap();
        assert_eq!(open_tag_lt_pos(bad, bp), None);
    }

    #[test]
    fn test_invoke_extract_name_attr() {
        assert_eq!(
            extract_name_attr(r#"<invoke name="Bash">"#),
            Some("Bash".to_string())
        );
        assert_eq!(
            extract_name_attr(r#"<parameter name="cmd">"#),
            Some("cmd".to_string())
        );
        assert_eq!(extract_name_attr("<invoke>"), None);
    }

    #[test]
    fn test_invoke_next_param_open() {
        // 🟢 find_next_param_open 定位下一个参数开标签的 '<'
        let body = r#"<parameter name="a">1</parameter><parameter name="b">2</parameter>"#;
        // 从第一个参数值区起找下一个参数开标签
        let first_val_start = body.find('>').unwrap() + 1;
        let next = find_next_param_open(body, first_val_start).expect("应找到第二个参数开标签");
        assert_eq!(
            &body[next..next + "<parameter name=\"b\"".len()],
            "<parameter name=\"b\""
        );
    }

    #[test]
    fn test_invoke_looks_like_real_leak_line_start() {
        // 🟢 行首（空 / 换行结尾）→ 像真泄漏；句中 → 不像
        assert!(invoke_looks_like_real_leak(""));
        assert!(invoke_looks_like_real_leak("some text\n"));
        assert!(invoke_looks_like_real_leak("some text\n   "));
        assert!(invoke_looks_like_real_leak("some text\r"));
        assert!(!invoke_looks_like_real_leak("讨论 "));
        assert!(!invoke_looks_like_real_leak("- "));
    }

    #[test]
    fn test_invoke_strip_trailing_stray_preserves_newline() {
        // 回归：narrative\ncall → 只剥 stray 行，保留前一行换行（行首信号不丢）
        let got = strip_trailing_stray_tokens("some text\ncall");
        assert_eq!(got, "some text\n", "必须保留叙述行末的换行");
        assert!(invoke_looks_like_real_leak(got), "剥完仍应像行首泄漏");
    }

    #[test]
    fn test_invoke_strip_trailing_stray_court_token() {
        // 🟢 KiroStudio 生产语料：court 是主要 stray token，应被剥
        assert_eq!(
            strip_trailing_stray_tokens("先看结果。\ncourt"),
            "先看结果。\n"
        );
        assert_eq!(strip_trailing_stray_tokens("court"), "");
        // 多个连续 stray 行全部剥掉
        assert_eq!(strip_trailing_stray_tokens("正文\ncall\ncourt"), "正文\n");
    }

    #[test]
    fn test_invoke_strip_trailing_stray_keeps_non_stray() {
        // 🔴 非 stray 的末行不剥
        assert_eq!(strip_trailing_stray_tokens("hello world"), "hello world");
    }

    #[test]
    fn test_invoke_collapse_stray_token_floods() {
        // 🟢 复读死循环：court 独占一行连续 100 次 → 从超阈值处截断
        let mut s = String::from("正文引导\n");
        for _ in 0..100 {
            s.push_str("court\n");
        }
        s.push_str("<invoke name=\"x\">");
        let collapsed = collapse_stray_token_floods(&s);
        // 截断后应只保留阈值内内容，court 出现次数远小于 100
        let court_count = collapsed.matches("court").count();
        assert!(
            court_count < 100,
            "复读应被熔断截断，court 次数={court_count}"
        );
        assert!(!collapsed.contains("<invoke"), "超阈值后的内容应被丢弃");
    }

    #[test]
    fn test_invoke_collapse_stray_token_chinese_flood() {
        // 🟢 中文变体 課/课 独占行复读也应被熔断(修复:原集合漏了中文,逐字清洗剥得掉但熔断抓不到)。
        for tok in ["課", "课"] {
            let mut s = String::from("正文\n");
            for _ in 0..100 {
                s.push_str(tok);
                s.push('\n');
            }
            let collapsed = collapse_stray_token_floods(&s);
            let cnt = collapsed.matches(tok).count();
            assert!(cnt < 100, "中文 {tok} 复读应被熔断截断,次数={cnt}");
        }
    }

    #[test]
    fn test_invoke_collapse_stray_token_below_threshold() {
        // 🔴 阈值内的少量引导词重复原样保留
        let s = "call\ncall\n<invoke name=\"x\">";
        let collapsed = collapse_stray_token_floods(s);
        assert_eq!(collapsed, s, "阈值内不应截断");
    }

    #[test]
    fn test_invoke_code_fence_state_toggle() {
        // 🟢 代码围栏奇偶翻转：一对 ``` 归零
        let mut open = false;
        let mut partial = String::new();
        advance_code_fence_state(&mut open, &mut partial, "```rust\nlet x = 1;\n```\n");
        assert!(!open, "一对围栏后应回到关闭态");

        // 单个开围栏 → 打开
        let mut open2 = false;
        let mut partial2 = String::new();
        advance_code_fence_state(&mut open2, &mut partial2, "```\n代码\n");
        assert!(open2, "单个开围栏后应为打开态");
    }

    #[test]
    fn test_invoke_fence_open_after_pure() {
        // 🟢 fence_open_after 纯试算，不改传入状态
        assert!(fence_open_after(false, "", "```\n"), "进入围栏");
        assert!(!fence_open_after(true, "", "```\n"), "离开围栏");
        assert!(!fence_open_after(false, "", "普通文本\n"), "普通文本不翻转");
    }

    #[test]
    fn test_invoke_partial_tag_suffix_len() {
        // 🟢 缓冲区末尾的半个开标签应被识别为需保留的尾巴
        assert_eq!(partial_invoke_tag_suffix_len("hello<inv"), 4);
        assert_eq!(partial_invoke_tag_suffix_len("hello<"), 1);
        // 已闭合的标签结尾 → 无需保留
        assert_eq!(partial_invoke_tag_suffix_len("<invoke>"), 0);
        assert_eq!(partial_invoke_tag_suffix_len("no angle bracket"), 0);
    }

    #[test]
    fn test_partial_invoke_tag_suffix_bounded_no_stream_stall() {
        // ⭐流停摆回归(旧代码必失败):`<` 之后的尾巴一旦超过"最长可能的半个开标签",
        // 就一定不是被切碎的 invoke 标签,而是正文里的普通 `<`(中文散文的"a < b"、
        // 数学式、代码里的比较运算符)。旧代码无上限地把它全部 hold 住:
        //   一旦这个 `<` 落到缓冲区首位,keep=buf.len()、emit_len=0 → 此后**整条响应
        //   的所有文本都不再下发**,全部囤到流结束才 flush,且缓冲无界增长。
        // 修复后超过 64 字节即判定"不是标签",返回 0 让正文正常吐出。

        // 短尾巴(可能是真的半个标签)→ 仍然保留,行为不变。
        assert_eq!(partial_invoke_tag_suffix_len("text<inv"), 4);
        assert_eq!(partial_invoke_tag_suffix_len("text<parameter na"), 13);

        // 长尾巴(正文里的普通 `<`)→ 必须返回 0(不 hold),否则流停摆。
        let prose = format!("条件 a < b 时触发{}", "后面还有很多正文".repeat(20));
        assert_eq!(
            partial_invoke_tag_suffix_len(&prose),
            0,
            "正文中的普通 `<` 后跟大量文本时绝不能 hold(旧代码在此无界持有 → 整条流停摆)"
        );

        // 关键退化场景:`<` 恰好在缓冲区**首位**且后面全是正文。
        // 旧代码此时 keep=len、emit_len=0 → 一个字节都不输出。
        let stuck = format!("<{}", "x".repeat(500));
        assert_eq!(
            partial_invoke_tag_suffix_len(&stuck),
            0,
            "`<` 在首位且尾巴超长时必须返回 0,否则 emit_len=0 → 流永久停摆"
        );

        // 边界:恰好 64 字节的尾巴仍视为可能的标签;65 字节则不再 hold。
        let at_limit = format!("<{}", "a".repeat(63)); // 尾巴 = 64 字节
        assert_eq!(partial_invoke_tag_suffix_len(&at_limit), 64);
        let over_limit = format!("<{}", "a".repeat(64)); // 尾巴 = 65 字节
        assert_eq!(partial_invoke_tag_suffix_len(&over_limit), 0);
    }

    #[test]
    fn test_sse_event_format() {
        let event = SseEvent::new("message_start", json!({"type": "message_start"}));
        let sse_str = event.to_sse_string();

        assert!(sse_str.starts_with("event: message_start\n"));
        assert!(sse_str.contains("data: "));
        assert!(sse_str.ends_with("\n\n"));
    }

    fn mk_ctx() -> StreamContext {
        StreamContext::new_with_thinking("deepseek", 10, false, HashMap::new())
    }

    /// 回归：TTFB 打点必须在**首个真实内容 delta** 落定时发生，且只记第一次。
    ///
    /// **旧代码为何 FAIL**：`first_token_ms` 全仓 **0 个生产赋值点**，线上 traces.db
    /// 24 小时 59458 条该列**全 NULL** —— 所有延迟分析失效，并且它阻塞了
    /// 「用量/额度动态刷新」与缓存实验两条线（都需要能分解端到端延迟）。
    /// 旧代码下 `first_token_at()` 恒为 None，第二个断言必然 FAIL。
    #[test]
    fn first_token_is_marked_on_first_real_content_delta_only() {
        let mut ctx = mk_thinking_ctx();
        assert!(ctx.first_token_at().is_none(), "初始未产出内容，不该有打点");

        // 首个真实内容（thinking_delta）→ 应打点
        ctx.process_kiro_event(&Event::ReasoningContent(
            crate::kiro::model::events::ReasoningContentEvent {
                text: "思考".into(),
                ..Default::default()
            },
        ));
        let first = ctx
            .first_token_at()
            .expect("首个真实内容 delta 后必须有打点");

        // 再来内容 → 打点**不得**被刷新（TTFB 是"第一次"，不是"最后一次"）
        std::thread::sleep(std::time::Duration::from_millis(5));
        ctx.process_kiro_event(&Event::ReasoningContent(
            crate::kiro::model::events::ReasoningContentEvent {
                text: "更多思考".into(),
                ..Default::default()
            },
        ));
        assert_eq!(
            ctx.first_token_at(),
            Some(first),
            "打点必须幂等：后续内容不得覆盖首 token 时刻，否则测的是最后一片而非 TTFB"
        );
    }

    /// 非内容事件（metering / contextUsage）绝不能被当成首 token。
    ///
    /// 这条守住的是最容易错的边界：这些事件也会经过同一个 choke point，
    /// 若判据写成"有事件产出就打点"，TTFB 会变成"首个任意帧"，数值系统性偏小。
    #[test]
    fn non_content_events_do_not_mark_first_token() {
        let mut ctx = mk_thinking_ctx();
        ctx.process_kiro_event(&Event::ContextUsage(
            crate::kiro::model::events::ContextUsageEvent {
                context_usage_percentage: 12.0,
            },
        ));
        assert!(
            ctx.first_token_at().is_none(),
            "contextUsageEvent 不含模型输出，不得触发 TTFB 打点"
        );
    }

    /// 回归（A8）：`contextUsagePercentage` 的**非正/非有限**值绝不能覆盖已算出的
    /// `context_input_tokens`。
    ///
    /// **旧代码为何 FAIL**：该 match arm 只判 `>= 100.0` 上界，无条件执行
    /// `self.context_input_tokens = Some(pct * window / 100.0)`。而该字段带
    /// `#[serde(default)]`，上游少发/发脏值时恒为 `0.0` ⇒ 写进 `Some(0)` ⇒
    /// 下游 `context_input_tokens.unwrap_or(本地估算)` 拿到 0（而非退回估算）⇒
    /// `billed_input_tokens` 连 cache 字段一起归零 = 计费信号错。
    /// 旧代码下第二个断言拿到的是 `Some(0)`，必然 FAIL。
    #[test]
    fn invalid_context_usage_percentage_must_not_overwrite_input_tokens() {
        use crate::kiro::model::events::ContextUsageEvent;
        let mut ctx = mk_ctx();

        // 先来一个**有效**信号，坐实正常路径仍会写入（对照组：防守卫写成"一律不写"）。
        ctx.process_kiro_event(&Event::ContextUsage(ContextUsageEvent {
            context_usage_percentage: 10.0,
        }));
        let good = ctx
            .context_input_tokens
            .expect("有效百分比必须算出 input_tokens");
        assert!(good > 0, "10% 的窗口占用必须是正 token 数，实得 {good}");

        // 脏值（serde 默认值 / 上游漏发都长这样）绝不能把它清零。
        for bad in [0.0_f64, -1.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            ctx.process_kiro_event(&Event::ContextUsage(ContextUsageEvent {
                context_usage_percentage: bad,
            }));
            assert_eq!(
                ctx.context_input_tokens,
                Some(good),
                "无效百分比 {bad} 覆盖了已有 input_tokens（计费口径被归零）"
            );
        }
    }

    /// 配套（A8）：加了下界守卫后，`>= 100.0` 的上界行为**不得**被连带改坏。
    ///
    /// 这条防的是「改分支时把上界判定挪进 else 里」这类顺序性回归：
    /// 100% 必须仍然置 `model_context_window_exceeded`，且 input_tokens 照常写入。
    #[test]
    fn context_usage_at_full_still_sets_stop_reason() {
        use crate::kiro::model::events::ContextUsageEvent;
        let mut ctx = mk_ctx();
        ctx.process_kiro_event(&Event::ContextUsage(ContextUsageEvent {
            context_usage_percentage: 100.0,
        }));
        assert_eq!(
            ctx.state_manager.get_stop_reason(),
            "model_context_window_exceeded",
            "100% 占用必须置 model_context_window_exceeded"
        );
        assert!(
            ctx.context_input_tokens.is_some_and(|t| t > 0),
            "100% 是有效信号，仍必须写入 input_tokens"
        );
    }

    /// 空 delta（关块占位）不算首 token。
    #[test]
    fn empty_delta_is_not_first_token() {
        assert!(
            !is_first_content_delta(&SseEvent::new(
                "content_block_delta",
                json!({"delta": {"type": "thinking_delta", "thinking": ""}})
            )),
            "空 thinking_delta 是关块占位，不代表有内容产出"
        );
        assert!(
            is_first_content_delta(&SseEvent::new(
                "content_block_delta",
                json!({"delta": {"type": "text_delta", "text": "hi"}})
            )),
            "非空 text_delta 应算首 token（对照组，防止判据过严把真内容也挡掉）"
        );
        assert!(
            !is_first_content_delta(&SseEvent::new(
                "message_start",
                json!({"type": "message_start"})
            )),
            "message_start 在内容产出前就发了，绝不能算首 token"
        );
    }

    /// 回归（E1）：`reasoningContentEvent` 必须产出 `thinking_delta`。
    ///
    /// **旧代码为何 FAIL**：该事件此前落 `EventType::Unknown`，payload 被**直接丢弃**
    /// （只按类型 warn 一次），产出零个 SSE 事件。本测试断言至少有一个
    /// `content_block_delta` 且 `delta.type == "thinking_delta"`，旧代码下事件列表是空的。
    ///
    /// 价值不在"多一个功能"：上游本就给了结构化思考边界，我们扔掉后改用文本嗅探
    /// `<thinking>` 标签把边界猜回来 —— 已知致命缺陷 #14（invoke_sniff_buffer 无界持有
    /// 导致整条流停摆）就出在那套嗅探上。接入结构化流是**移除一整类缺陷的来源**。
    /// thinking **开启**的 ctx（mk_ctx 的第三参 thinking_enabled 是 false，E1 需要 true）。
    fn mk_thinking_ctx() -> StreamContext {
        StreamContext::new_with_thinking("deepseek", 10, true, HashMap::new())
    }

    #[test]
    fn should_emit_thinking_delta_from_reasoning_content_event() {
        let mut ctx = mk_thinking_ctx();
        let ev = Event::ReasoningContent(crate::kiro::model::events::ReasoningContentEvent {
            text: "让我先看一下目录结构".to_string(),
            ..Default::default()
        });
        let out = ctx.process_kiro_event(&ev);

        assert!(
            !out.is_empty(),
            "结构化思考帧不应产出零事件（旧代码丢弃 payload）"
        );
        let joined: String = out.iter().map(|e| e.to_sse_string()).collect();
        assert!(
            joined.contains("\"type\":\"thinking_delta\""),
            "应产出 thinking_delta，实际: {joined}"
        );
        assert!(
            joined.contains("让我先看一下目录结构"),
            "思考内容应被下发，实际: {joined}"
        );
    }

    /// 回归（E1）：结构化流与文本嗅探**共用**同一个 thinking 块，绝不重复开块。
    ///
    /// 两条路径都保留（上游可能对某些模型仍走内联标签），所以必须共享
    /// `in_thinking_block` / `thinking_block_index`。若各自开块，客户端会收到两个
    /// `content_block_start(thinking)`，Anthropic SDK 侧属协议违规。
    #[test]
    fn should_not_double_open_thinking_block_when_both_paths_fire() {
        let mut ctx = mk_thinking_ctx();
        let mut all = String::new();

        // 先走结构化流（开块）
        all.push_str(
            &ctx.process_kiro_event(&Event::ReasoningContent(
                crate::kiro::model::events::ReasoningContentEvent {
                    text: "结构化思考".into(),
                    ..Default::default()
                },
            ))
            .iter()
            .map(|e| e.to_sse_string())
            .collect::<String>(),
        );
        // 再喂含 <thinking> 的正文（嗅探路径）
        all.push_str(
            &ctx.process_kiro_event(&Event::AssistantResponse(
                crate::kiro::model::events::AssistantResponseEvent {
                    content: "<thinking>内联思考</thinking>正文".into(),
                },
            ))
            .iter()
            .map(|e| e.to_sse_string())
            .collect::<String>(),
        );

        let opens = all.matches("\"type\":\"thinking\"").count();
        assert!(
            opens <= 1,
            "thinking 块只应被开一次，实际匹配 {opens} 次；两条路径必须共用同一个块。全文: {all}"
        );
    }

    /// 回归（E1 最严重的坑）：结构化 reasoning 之后的**普通正文**必须走 `text_delta`，
    /// 绝不能被当成思考内容。
    ///
    /// **旧代码（E1 首版）为何 FAIL**：结构化 reasoning 是**纯增量、无终止帧**的，
    /// 真实上游形态是「N 帧 reasoningContentEvent → assistantResponseEvent 携带普通正文」。
    /// 而文本嗅探路径的分支是 `else if self.in_thinking_block` —— reasoning 一旦开过 thinking 块，
    /// 后续**不带任何标签**的正文就落进那个分支，被发成 `thinking_delta`。
    ///
    /// 实测旧代码的输出（本测试抓到的原始报文）：
    /// ```text
    /// data: {"delta":{"thinking":"这是给用","type":"thinking_delta"},...}
    /// ```
    /// 即**用户可见的答案整段消失进思考面板**；更糟的是 `has_non_thinking_blocks()` 为 false
    /// 会让收尾把 `stop_reason` 置成 `max_tokens` 并只吐一个空格文本块 → 客户端显示**空答案**。
    ///
    /// 修法是让两条路径**互斥**（`reasoning_stream_seen` + 首个正文即关块），
    /// 而不是共享 `in_thinking_block` 状态。原先那个"共用同一个块"的测试只喂了含完整
    /// `<thinking>…</thinking>` 配对的正文 —— 恰好是代码能处理的那种，所以漏掉了这个形态。
    #[test]
    fn prose_after_reasoning_stream_goes_to_text_not_thinking() {
        let mut ctx = mk_thinking_ctx();
        ctx.process_kiro_event(&Event::ReasoningContent(
            crate::kiro::model::events::ReasoningContentEvent {
                text: "先想一下".into(),
                ..Default::default()
            },
        ));
        // 真实上游形态：reasoning 无终止符，紧接着就是普通正文（不带任何标签）
        let out = ctx.process_kiro_event(&Event::AssistantResponse(
            crate::kiro::model::events::AssistantResponseEvent {
                content: "这是给用户看的答案".into(),
            },
        ));
        let j: String = out.iter().map(|e| e.to_sse_string()).collect();
        assert!(
            j.contains("text_delta") && j.contains("这是给用户看的答案"),
            "正文必须作为 text_delta 下发，否则答案会消失进 thinking 面板。实际: {j}"
        );
    }

    /// thinking 未开启时结构化思考帧必须**整帧丢弃**，绝不能混进用户可见正文。
    #[test]
    fn reasoning_content_is_dropped_when_thinking_disabled() {
        // thinking_enabled = false
        let mut ctx = StreamContext::new_with_thinking("deepseek", 10, false, HashMap::new());
        let out = ctx.process_kiro_event(&Event::ReasoningContent(
            crate::kiro::model::events::ReasoningContentEvent {
                text: "内部推理".into(),
                ..Default::default()
            },
        ));
        let joined: String = out.iter().map(|e| e.to_sse_string()).collect();
        assert!(
            !joined.contains("内部推理"),
            "未开 thinking 时内部推理绝不能下发给客户端，实际: {joined}"
        );
    }

    // ===== M1：`!thinking_enabled` 且本轮只有 reasoning ⇒ 空响应兜底 =====

    fn reasoning_ev(text: &str) -> Event {
        Event::ReasoningContent(crate::kiro::model::events::ReasoningContentEvent {
            text: text.to_string(),
            ..Default::default()
        })
    }

    fn text_ev(content: &str) -> Event {
        Event::AssistantResponse(crate::kiro::model::events::AssistantResponseEvent {
            content: content.to_string(),
        })
    }

    /// 完整一轮的上游终止信号。XML 泄漏测试测的是剥离，不是截断；夹具必须带它。
    fn meta_end_turn() -> Event {
        Event::Metadata(crate::kiro::model::events::MetadataEvent {
            stop_reason: Some("end_turn".into()),
        })
    }

    /// 把一轮事件跑完（含真实收尾），返回全部 SSE 报文拼串。
    fn run_turn(ctx: &mut StreamContext, events: &[Event]) -> String {
        let mut all = ctx.generate_initial_events();
        for ev in events {
            all.extend(ctx.process_kiro_event(ev));
        }
        all.extend(ctx.generate_final_events());
        all.iter().map(|e| e.to_sse_string()).collect()
    }

    // ===== tool_use XML 泄漏过滤（流层）=====

    /// ① 开标签与闭合分跨两个 chunk：必须能完整剥掉，且用户看不到标签、工具参数或泄漏词。
    ///
    /// 旧实现（无本过滤器）会把 `before <tool_use …>…</tool_use> after` 原样发给客户端，
    /// 泄漏 XML 被当普通文本渲染（甚至被 Claude Code 当结构化指令解析）。
    #[test]
    fn tool_use_xml_leak_across_chunks_is_stripped() {
        let mut ctx = StreamContext::new_with_thinking("claude-opus-4.6", 10, false, HashMap::new());
        let joined = run_turn(
            &mut ctx,
            &[
                text_ev("before "),
                // 开标签被切在 `>` 之前（第 2 帧还看不出是不是完整标签）
                text_ev(r##"<tool_use id="toolu_1" name="Read""##),
                // 第 3 帧才补齐 `>` 与整个块体 + 闭合
                text_ev(r#">{"path":"/tmp/a"}</tool_use>"#),
                text_ev(" after"),
                meta_end_turn(),
            ],
        );
        assert!(joined.contains("before"), "块前文本必须保留: {joined}");
        assert!(joined.contains("after"), "块后文本必须保留: {joined}");
        assert!(
            !joined.contains("tool_use") && !joined.contains("/tmp/a"),
            "开标签/参数/闭合都不能泄漏给客户端。实际: {joined}"
        );
        assert!(
            ctx.tool_use_xml_stripped > 0,
            "剥离计数器应记录本次剥离的字节数（可观测，确保剥离真的发生）"
        );
    }

    /// ② 合法内容（含小于号的普通文本）不被误剥。
    #[test]
    fn tool_use_xml_keeps_normal_text() {
        let mut ctx = StreamContext::new_with_thinking("claude-opus-4.6", 10, false, HashMap::new());
        let joined = run_turn(
            &mut ctx,
            &[text_ev("if a < b then b > c"), text_ev(" and use <tool_user> ok")],
        );
        assert!(
            joined.contains("if a < b then b > c"),
            "含 < 的普通正文不能被误剥。实际: {joined}"
        );
        assert!(
            joined.contains("<tool_user>"),
            "<tool_user> 是普通文本不是 tool_use 开标签，不能误剥。实际: {joined}"
        );
    }

    /// ③ 流在 `<tool_use …` 标签内结束（未闭合）：残留丢弃，不把泄漏 XML 补发成正文。
    #[test]
    fn tool_use_xml_unclosed_at_eof_is_dropped() {
        let mut ctx = StreamContext::new_with_thinking("claude-opus-4.6", 10, false, HashMap::new());
        let joined = run_turn(
            &mut ctx,
            // ⚠️ 用 r##"…"## 双井号：内容以 `"` 结尾，单井号的 `"#` 会被提前当作结束符。
            // 上游已发 metadata 收尾：未闭合泄漏仍丢弃，整轮是成功 turn（不是截断）。
            &[
                text_ev("head "),
                text_ev(r##"<tool_use id="a" name="Write">{"path":"##),
                meta_end_turn(),
            ],
        );
        assert!(joined.contains("head"), "块前文本保留: {joined}");
        assert!(
            !joined.contains("tool_use") && !joined.contains("Write"),
            "未闭合的泄漏标签不能补发。实际: {joined}"
        );
    }

    /// ③' 🔴 闭合标签被逐字节切开：必须仍能退出剥离态，其后正文不能被吞。
    ///
    /// 这条是我们与参考仓 ref-grey 的**实质差异**的回归钉子：ref-grey 在剥离态下每个
    /// chunk 都清空缓冲（ref-grey stream.rs:51），闭合被切成 `<` + `/` + `tool`… 时
    /// 永远拼不齐 ⇒ `stripping` 再也退不出 ⇒ **响应余下全部正文被静默吞掉**。
    /// 若把 `partial_tool_use_xml_close_suffix` 那段尾巴保留改回 clear()，本测试即 FAIL。
    #[test]
    fn tool_use_xml_close_tag_split_byte_by_byte_still_recovers() {
        let mut ctx = StreamContext::new_with_thinking("claude-opus-4.6", 10, false, HashMap::new());
        let joined = run_turn(
            &mut ctx,
            &[
                text_ev(r#"<tool_use name="A">payload"#),
                text_ev("<"),
                text_ev("/"),
                text_ev("tool"),
                text_ev("_use"),
                text_ev(">"),
                text_ev("VISIBLE"),
                meta_end_turn(),
            ],
        );
        assert!(
            joined.contains("VISIBLE"),
            "闭合标签跨 chunk 切碎后必须退出剥离态，其后正文不能被吞。实际: {joined}"
        );
        assert!(
            !joined.contains("payload") && !joined.contains("tool_use"),
            "泄漏块本体与标签都不能下发。实际: {joined}"
        );
    }

    /// ③'' 上限兜底：永不闭合的 `<tool_use` 不能无限吞正文（否则整段回答静默消失）。
    #[test]
    fn tool_use_xml_never_closing_tag_releases_after_cap() {
        let mut ctx = StreamContext::new_with_thinking("claude-opus-4.6", 10, false, HashMap::new());
        let mut events = vec![text_ev(r#"<tool_use name="A">"#)];
        // 累积超过 MAX_TOOL_USE_XML_STRIP_BYTES（256 KiB）且始终不闭合。
        // ⚠️ 内容不能用重复字符（如 `x`×N）—— 那会先被 `stray_guard_filter` 的
        // 复读熔断拦下（"刷屏"），根本到不了 XML 过滤器，测的是错的守卫。
        // 用随机化但非重复的文本。
        let mut filler = String::with_capacity(8 * 1024);
        let mut seed = 0x5eedu32;
        for _ in 0..(8 * 1024) {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            let c = b'a' + (seed >> 24) as u8 % 26;
            filler.push(c as char);
        }
        for _ in 0..40 {
            events.push(text_ev(&filler));
        }
        events.push(text_ev("LATER"));
        let joined = run_turn(&mut ctx, &events);
        assert!(
            joined.contains("LATER"),
            "超上限后必须放弃剥离、放行后续文本，否则永不闭合的标签吞掉整段回答"
        );
    }

    /// ④ 过滤结果不计入 output_tokens（记账=实际下发）。
    #[test]
    fn tool_use_xml_leak_not_counted_in_output_tokens() {
        let mut ctx = StreamContext::new_with_thinking("claude-opus-4.6", 10, false, HashMap::new());
        let _ = run_turn(
            &mut ctx,
            &[
                text_ev("hi "),
                text_ev(r#"<tool_use name="Read">{"path":"/a"}</tool_use>"#),
            ],
        );
        // 泄漏的字节数（>20）不应被算进 output_tokens。
        assert!(
            ctx.output_tokens < 10,
            "剥掉的泄漏内容不能计入 output_tokens（记账=实际下发），实际: {}",
            ctx.output_tokens
        );
    }

    /// ① thinking 关 + 本轮**只有** reasoning（无正文）→ 必须有非空输出。
    ///
    /// **旧代码为何 FAIL**：`process_reasoning_content` 在 `!thinking_enabled` 时
    /// `return Vec::new()` 就地丢帧，且没有任何别处留底 ⇒ 整轮只剩
    /// message_start / content_block_start / message_delta / message_stop，
    /// **一个 text_delta 都没有** ⇒ 客户端（Claude Code）显示完全空的回答且不重试。
    #[test]
    fn reasoning_only_turn_degrades_to_text_when_thinking_disabled() {
        let mut ctx = StreamContext::new_with_thinking("deepseek", 10, false, HashMap::new());
        let joined = run_turn(
            &mut ctx,
            &[
                reasoning_ev("先分析一下这个问题"),
                reasoning_ev("然后得出结论"),
            ],
        );

        assert!(
            joined.contains("\"type\":\"text_delta\""),
            "只有 reasoning 的一轮必须降级出正文，否则客户端拿到空响应。实际: {joined}"
        );
        assert!(
            joined.contains("先分析一下这个问题") && joined.contains("然后得出结论"),
            "降级下发应保留 reasoning 原文（多帧要拼齐）。实际: {joined}"
        );
        assert!(
            !joined.contains("thinking_delta"),
            "降级走的是 text_delta，绝不能凭空造 thinking 块（客户端没要 thinking）。实际: {joined}"
        );
    }

    /// ② thinking 关 + **有正文** → reasoning 仍被丢弃（防推理泄漏给用户）。
    ///
    /// 这是兜底的反向护栏：兜底若写成"无条件转正文"（kiro-rs 的做法），
    /// 用户明确不想看的内部推理就会混进可见回答里。
    #[test]
    fn reasoning_stays_dropped_when_body_text_present() {
        let mut ctx = StreamContext::new_with_thinking("deepseek", 10, false, HashMap::new());
        let joined = run_turn(
            &mut ctx,
            &[
                reasoning_ev("内部推理不该给用户看"),
                text_ev("这是给用户看的答案"),
            ],
        );

        assert!(
            joined.contains("这是给用户看的答案"),
            "正文必须下发。实际: {joined}"
        );
        assert!(
            !joined.contains("内部推理不该给用户看"),
            "有正文时 reasoning 必须保持丢弃，否则内部推理泄漏给明确没要 thinking 的客户端。实际: {joined}"
        );
    }

    /// ②' 顺序护栏：正文整轮被 hold 在 `invoke_sniff_buffer`、只在收尾 flush 才出现时，
    /// 兜底**不得**误判为"无正文"。
    ///
    /// 这条测的是**分支顺序**而非分支内容：兜底判据依赖 `body_content_seen`，而该标志在
    /// flush 之前必然为 false（正文还在缓冲里）。把兜底分支上移到
    /// `flush_invoke_sniff_buffer()` 之前，本测试即 FAIL —— 输出会同时出现推理与正文（双份）。
    /// 行首未闭合的 `<invoke` 被 hold 到收尾是**已知常态**（见 MAX_INVOKE_HOLD_BYTES 注释），
    /// 不是构造出来的边角形态。
    #[test]
    fn fallback_must_run_after_sniff_flush_not_before() {
        let mut known = std::collections::HashSet::new();
        known.insert("Bash".to_string());
        let mut ctx = StreamContext::new_full("claude-opus-4.6", 10, false, HashMap::new(), known);
        // 显式置位：本测试的前提是"正文被 hold 进 sniff 缓冲"，而 `reclaim_enabled` 默认值
        // 来自进程级 `AtomicBool`（`tool_reclaim_textified_invoke_enabled()`）。若有人把默认
        // 改成 false（或同进程别的测试改了它），文本会直接下发、`body_content_seen` 在 flush
        // 之前就为真 ⇒ 这条顺序守卫**静默失效**（照样 PASS，但已不测顺序）。
        ctx.reclaim_enabled = true;
        let lt = "<";
        // 行首未闭合的 invoke 半块 → 整轮 hold 在 sniff 缓冲，收尾 flush 才当文本吐。
        let held = format!("{lt}invoke name=\"Bash\">{lt}parameter name=\"command\">ls");
        let mut all = ctx.generate_initial_events();
        all.extend(ctx.process_kiro_event(&reasoning_ev("推理绝不能双份下发")));
        all.extend(ctx.process_kiro_event(&text_ev(&held)));
        // 前提校验：正文此刻**确实**还在 sniff 缓冲里、且兜底判据仍认为"无正文"。
        // 少了这两条，`reclaim_enabled` 一旦失效本测试就退化成"随便一轮都能过"。
        assert!(
            !ctx.invoke_sniff_buffer.is_empty(),
            "前提校验：正文应被 hold 在 sniff 缓冲（否则测的不是收尾顺序）"
        );
        assert!(
            !ctx.body_content_seen,
            "前提校验：flush 之前 body_content_seen 必须仍为 false（这正是顺序敏感的原因）"
        );
        all.extend(ctx.generate_final_events());
        let joined: String = all.iter().map(|e| e.to_sse_string()).collect();

        assert!(
            joined.contains("ls"),
            "被 hold 的正文必须在收尾 flush 出来。实际: {joined}"
        );
        assert!(
            !joined.contains("推理绝不能双份下发"),
            "正文只是晚到（hold 在 sniff 缓冲），不是没有 —— 兜底必须排在 flush 之后。实际: {joined}"
        );
    }

    /// ③ thinking 开 → 行为完全不变（reasoning 走 thinking_delta，不受兜底影响）。
    #[test]
    fn reasoning_only_turn_unchanged_when_thinking_enabled() {
        let mut ctx = mk_thinking_ctx();
        let joined = run_turn(&mut ctx, &[reasoning_ev("思考内容")]);

        assert!(
            joined.contains("\"type\":\"thinking_delta\"") && joined.contains("思考内容"),
            "thinking 开启时 reasoning 照旧走 thinking_delta。实际: {joined}"
        );
        // 既有行为：只有 thinking 块时收尾补一个空格 text 块 + stop_reason=max_tokens。
        // 兜底若误伤这条路径（把思考内容再当正文发一遍），这里会抓到第二份"思考内容"。
        assert_eq!(
            joined.matches("思考内容").count(),
            1,
            "思考内容只应出现一次（thinking_delta 里），不得被兜底重复成正文。实际: {joined}"
        );
        assert!(
            joined.contains("max_tokens"),
            "thinking-only 轮的既有收尾（stop_reason=max_tokens）不得被改变。实际: {joined}"
        );
    }

    /// ④ thinking 关 + 只有 tool_use（无 text）→ 不是空响应，兜底不得触发。
    ///
    /// tool_use 轮客户端会去执行工具，本身就是有效产出；此时下发推理是纯泄漏。
    #[test]
    fn tool_use_only_turn_does_not_trigger_reasoning_fallback() {
        let mut ctx =
            StreamContext::new_with_thinking("claude-opus-4.6", 10, false, HashMap::new());
        let joined = run_turn(
            &mut ctx,
            &[
                reasoning_ev("推理：应该调用 Bash"),
                Event::ToolUse(crate::kiro::model::events::ToolUseEvent {
                    name: "Bash".into(),
                    tool_use_id: "toolu_1".into(),
                    input: "{\"command\":\"ls\"}".into(),
                    stop: true,
                }),
            ],
        );

        assert!(
            joined.contains("tool_use"),
            "tool_use 块必须下发。实际: {joined}"
        );
        assert!(
            !joined.contains("推理：应该调用 Bash"),
            "有 tool_use 即非空响应，推理必须保持丢弃。实际: {joined}"
        );
    }

    /// 兜底素材有上限，且截断必须落在 UTF-8 边界（中文推理是常态，切错即 panic）。
    #[test]
    fn discarded_reasoning_is_capped_at_utf8_boundary() {
        let mut ctx = StreamContext::new_with_thinking("deepseek", 10, false, HashMap::new());
        // 每帧 3 KiB 级中文，喂到远超上限。
        let frame: String = "推".repeat(4096); // 3 字节/字 → 12 KiB/帧
        for _ in 0..8 {
            ctx.process_kiro_event(&reasoning_ev(&frame));
        }
        assert!(
            ctx.discarded_reasoning.len() <= StreamContext::MAX_DISCARDED_REASONING_BYTES,
            "丢弃素材必须有上限，实际 {} 字节",
            ctx.discarded_reasoning.len()
        );
        // 能走到这里说明每次截断都落在字符边界（否则 push_str 前的切片已 panic）；
        // 再显式确认内容没有被切出半个字符。
        assert!(
            ctx.discarded_reasoning.chars().all(|c| c == '推'),
            "截断后不得出现残缺字符"
        );
    }

    /// buffered 路径（`/cc/v1` 的 ccAutoBuffer）同样要有兜底。
    ///
    /// 两条 ctx 都把事件喂给同一个 `StreamContext`（buffered 只是把产出攒起来），
    /// 所以修在 `generate_final_events` 一处即可覆盖 —— 这条测试就是那个"即可"的凭据。
    #[test]
    fn buffered_path_also_degrades_reasoning_only_turn() {
        let mut ctx = BufferedStreamContext::new(
            "deepseek",
            10,
            false,
            HashMap::new(),
            std::collections::HashSet::new(),
        );
        ctx.process_and_buffer(&reasoning_ev("buffered 路径的推理"));
        let joined: String = ctx
            .finish_and_get_all_events()
            .iter()
            .map(|e| e.to_sse_string())
            .collect();

        assert!(
            joined.contains("\"type\":\"text_delta\"") && joined.contains("buffered 路径的推理"),
            "buffered 路径也必须避免空响应。实际: {joined}"
        );
    }

    // ===== P5-a：失败的一轮不得被兜底塞进推理文本 =====

    /// 传输层读流 Err（`handlers.rs:1578` 的 `mark_transport_error` → :1587 直接调
    /// `generate_final_events`）：这一轮已经补发过 SSE error 事件、记账按 NetworkError 落库，
    /// 兜底若照样触发，客户端看到的是「error + 一段推理正文」，而面板上这是一次失败
    /// —— 等于给失败凭空记上 output_tokens。
    ///
    /// 用真实链路的置态入口 `mark_transport_error` 而非直接改字段，串也用 reqwest 那类
    /// 错误的实际形态（handlers 传的是 `e.to_string()`）。
    #[test]
    fn transport_error_turn_does_not_get_reasoning_fallback() {
        let mut ctx = StreamContext::new_with_thinking("deepseek", 10, false, HashMap::new());
        let mut all = ctx.generate_initial_events();
        all.extend(ctx.process_kiro_event(&reasoning_ev("失败轮的推理不该下发")));
        // 与 handlers.rs:1578 同构：读流 Err 后置传输失败态，再收尾。
        ctx.mark_transport_error("error reading a body from connection: connection reset by peer");
        all.extend(ctx.generate_final_events());
        let joined: String = all.iter().map(|e| e.to_sse_string()).collect();

        assert!(
            !ctx.completion().is_ok(),
            "前提校验：传输失败态必须已置位，否则本测试测的不是失败路径"
        );
        assert!(
            !joined.contains("失败轮的推理不该下发"),
            "失败的一轮绝不能被兜底补上推理正文（客户端本来就要重试整轮）。实际: {joined}"
        );
        assert_eq!(
            ctx.resolved_usage().output_tokens,
            0,
            "没下发任何内容就不得记 output_tokens"
        );
    }

    /// 对照组：同样的事件序列，只是**没有**失败态 → 兜底必须照旧触发。
    ///
    /// 没有这条对照，上面那条用「把兜底整块删掉」也能过 —— 它证明的是"失败才不发"，
    /// 而不是"永远不发"。
    #[test]
    fn successful_reasoning_only_turn_still_gets_fallback() {
        let mut ctx = StreamContext::new_with_thinking("deepseek", 10, false, HashMap::new());
        let joined = run_turn(&mut ctx, &[reasoning_ev("失败轮的推理不该下发")]);
        assert!(
            ctx.completion().is_ok(),
            "前提校验：本轮应为成功态（对照组）"
        );
        assert!(
            joined.contains("失败轮的推理不该下发"),
            "成功轮的兜底不得被 completion 门误伤。实际: {joined}"
        );
    }

    /// decoder 永久停止（`handlers.rs:1552` / buffered 缓冲溢出 :3455 共用
    /// `mark_decoder_stopped`）：响应必然截断，同样不得补推理。
    ///
    /// buffered 路径走 `finish_and_get_all_events`，与流式共用同一个 `generate_final_events`，
    /// 故这里直接测 buffered —— 覆盖溢出那条真实入口的收尾形态。
    #[test]
    fn decoder_stopped_buffered_turn_does_not_get_reasoning_fallback() {
        let mut ctx = BufferedStreamContext::new(
            "deepseek",
            10,
            false,
            HashMap::new(),
            std::collections::HashSet::new(),
        );
        ctx.process_and_buffer(&reasoning_ev("截断轮的推理不该下发"));
        // 与 stream.rs:3455（缓冲溢出）/ handlers.rs:1552（解码器停止）同构的置态入口。
        ctx.mark_decoder_stopped("缓冲流事件超出内存上限(疑似异常超长响应)");
        let joined: String = ctx
            .finish_and_get_all_events()
            .iter()
            .map(|e| e.to_sse_string())
            .collect();

        assert!(
            !joined.contains("截断轮的推理不该下发"),
            "截断（decoder 停止 / 缓冲溢出）的一轮不得被兜底补推理。实际: {joined}"
        );
    }

    // ===== B1：buffered 收尾必须用 billed 口径更正 message_start 的 usage =====

    /// buffered 收尾（`finish_and_get_all_events`）必须用**最终 billed 口径**更正
    /// message_start 的 input_tokens。
    ///
    /// 背景：buffered 路径（`/cc/v1` 的 ccAutoBuffer）的存在理由就是「message_start 即精确
    /// input_tokens」（handlers.rs:4224-4225 注释）。初始事件生成时只有本地估算，上游
    /// contextUsage 反推的精确值必须在收尾回填。此前该路径零测试：若收尾被误改成读顶层
    /// `data["usage"]`（message_start 注入事故的同类误改），全部测试依然绿、线上 buffered
    /// 退回本地估算 —— 本条把「更正发生在 `message.usage` 嵌套路径」钉死。
    ///
    /// 夹具结构与真实上游一致：usage **嵌套在 `message` 内**、顶层无 usage
    /// （docs/PROTOCOL.md:419 + passthrough_think_filter.rs:1760 同款结构）。
    #[test]
    fn buffered_finish_corrects_nested_message_start_usage() {
        let mut ctx = BufferedStreamContext::new(
            "deepseek",
            100,
            false,
            HashMap::new(),
            std::collections::HashSet::new(),
        );
        // 上游 contextUsage 已反推精确 input_tokens（真实入口是 Event::ContextUsage，
        // 这里直接注入其产物，避免依赖模型窗口常数；测试先例见 should_prefer_context_usage
        // _over_estimate_for_gross_input）。
        ctx.inner.context_input_tokens = Some(1_000);
        ctx.set_cache_usage(Some(CacheUsageBreakdown {
            cache_creation_input_tokens: 20,
            cache_read_input_tokens: 30,
            cache_creation_5m_input_tokens: 0,
            cache_creation_1h_input_tokens: 0,
        }));
        // 初始事件生成时只有本地估算口径 billed(100, 20, 30) = 50。
        ctx.initial_events_generated = true;
        ctx.event_buffer.push(SseEvent::new(
            "message_start",
            json!({
                "type": "message_start",
                "message": {
                    "id": "msg_mock",
                    "type": "message",
                    "usage": {"input_tokens": 50, "output_tokens": 0}
                }
            }),
        ));

        let events = ctx.finish_and_get_all_events();
        let msg_start = events
            .iter()
            .find(|e| e.event == "message_start")
            .unwrap_or_else(|| panic!("收尾结果必须含 message_start"));

        // billed(1_000, 20, 30) = 950 —— 与初始 50 差异巨大，更正循环被删必然红。
        assert_eq!(
            msg_start.data["message"]["usage"]["input_tokens"], 950,
            "收尾必须用最终 billed 口径更正嵌套 message.usage 的 input_tokens"
        );
        assert_eq!(
            msg_start.data["message"]["usage"]["cache_creation_input_tokens"], 20,
            "cache 字段必须随更正补齐"
        );
        assert_eq!(
            msg_start.data["message"]["usage"]["cache_read_input_tokens"], 30,
            "cache 字段必须随更正补齐"
        );
        assert!(
            msg_start.data.get("usage").is_none(),
            "message_start 保持真实嵌套形态：usage 只存在于 message 内，顶层不得有 usage"
        );
    }

    /// 顶层 usage 的旧形态**不是** buffered 收尾的更正对象：原样保留，不被触碰。
    ///
    /// 与 passthrough_think_filter 不同（那里对个别上游有顶层回退分支），buffered 收尾
    /// 只有 `message.usage` 嵌套路径（stream.rs:4279-4293）。若未来误加顶层写入
    /// （以为能兼容旧形态），本条立即红，提醒重新审视语义。
    #[test]
    fn buffered_finish_leaves_top_level_usage_shape_untouched() {
        let mut ctx = BufferedStreamContext::new(
            "deepseek",
            100,
            false,
            HashMap::new(),
            std::collections::HashSet::new(),
        );
        ctx.inner.context_input_tokens = Some(1_000);
        ctx.set_cache_usage(Some(CacheUsageBreakdown {
            cache_creation_input_tokens: 20,
            cache_read_input_tokens: 30,
            cache_creation_5m_input_tokens: 0,
            cache_creation_1h_input_tokens: 0,
        }));
        ctx.initial_events_generated = true;
        ctx.event_buffer.push(SseEvent::new(
            "message_start",
            json!({
                "type": "message_start",
                "id": "msg_old_shape",
                "usage": {"input_tokens": 111, "output_tokens": 0}
            }),
        ));

        let events = ctx.finish_and_get_all_events();
        let msg_start = events
            .iter()
            .find(|e| e.event == "message_start")
            .unwrap_or_else(|| panic!("收尾结果必须含 message_start"));

        assert_eq!(
            msg_start.data["usage"]["input_tokens"], 111,
            "顶层 usage 形态不是更正对象：input_tokens 必须原样保留（收尾只认嵌套路径）"
        );
        assert!(
            msg_start.data["message"]["usage"].is_null(),
            "该事件没有 message 对象：不得凭空创建嵌套 usage"
        );
    }

    /// in-band Error 帧（上游在流里直接报错）：`process_kiro_event` 已置 UpstreamError
    /// 并内联发了 error 事件，兜底同样不得触发。
    #[test]
    fn inband_error_turn_does_not_get_reasoning_fallback() {
        let mut ctx = StreamContext::new_with_thinking("deepseek", 10, false, HashMap::new());
        let mut all = ctx.generate_initial_events();
        all.extend(ctx.process_kiro_event(&reasoning_ev("in-band 失败轮的推理")));
        all.extend(ctx.process_kiro_event(&Event::Error {
            error_code: "ThrottlingException".into(),
            error_message: "Too many requests".into(),
        }));
        all.extend(ctx.generate_final_events());
        let joined: String = all.iter().map(|e| e.to_sse_string()).collect();

        assert!(
            !joined.contains("in-band 失败轮的推理"),
            "上游 in-band 报错的一轮不得被兜底补推理。实际: {joined}"
        );
    }

    // ===== P5-b：兜底文本必须过两道正文清洗 =====

    /// #70544 行首泄漏词（`court` 粘 CJK）在兜底文本里同样要被清洗。
    ///
    /// 正文路径由 `process_assistant_response` → `clean_leaked_tokens` 处理，而兜底刻意
    /// 绕开整条正文链 ⇒ 同一形态在兜底里原样泄漏。词表与粘连判据取自
    /// `LEAKED_CONTROL_TOKENS` / `is_leak_glue_char`（实测 `court` 独占行 202 次）。
    #[test]
    fn degraded_reasoning_gets_leaked_token_cleaning() {
        let mut ctx =
            StreamContext::new_with_thinking("claude-opus-4.6", 10, false, HashMap::new());
        let joined = run_turn(&mut ctx, &[reasoning_ev("court需要先读文件再改")]);

        assert!(
            joined.contains("需要先读文件再改"),
            "推理正文本体必须保留。实际: {joined}"
        );
        assert!(
            !joined.contains("court"),
            "行首泄漏词必须被清洗，兜底不得绕过 clean_leaked_tokens。实际: {joined}"
        );
        assert!(
            ctx.leaked_stripped > 0,
            "兜底路径的泄漏清洗也要计数（收尾诊断可见，不黑箱）"
        );
    }

    /// 孤立 `</thinking>` 标记不得随兜底文本泄漏给客户端。
    ///
    /// 判据复用 `strip_stray_thinking_end_tags`（正文侧几条收尾路径已在用），
    /// 两侧真正文都保留。
    #[test]
    fn degraded_reasoning_strips_stray_thinking_end_tag() {
        let mut ctx = StreamContext::new_with_thinking("deepseek", 10, false, HashMap::new());
        let joined = run_turn(&mut ctx, &[reasoning_ev("前半段推理</thinking>后半段推理")]);

        assert!(
            joined.contains("前半段推理") && joined.contains("后半段推理"),
            "标记两侧的正文都必须保留。实际: {joined}"
        );
        assert!(
            !joined.contains("</thinking>"),
            "孤立闭标签是标记不是正文，兜底不得原样下发。实际: {joined}"
        );
    }

    /// 清洗把整段吃空时退回丢弃，不发空 text_delta。
    ///
    /// `create_text_delta_events` 自身无空串守卫，发空 delta 只会给客户端多一个无意义的块。
    #[test]
    fn degraded_reasoning_all_stripped_emits_no_empty_delta() {
        let mut ctx =
            StreamContext::new_with_thinking("claude-opus-4.6", 10, false, HashMap::new());
        // 整行就是高置信独占泄漏词（LEAK_STANDALONE_TOKENS，实测 court 全独占行）。
        let joined = run_turn(&mut ctx, &[reasoning_ev("court\n")]);
        assert!(
            !joined.contains("\"type\":\"text_delta\""),
            "清洗后只剩空白时不得发 text_delta。实际: {joined}"
        );
        assert_eq!(
            ctx.resolved_usage().output_tokens,
            0,
            "没下发内容就不得记 output_tokens"
        );
    }

    // ===== P5-c：兜底轮的 TTFB 打点 =====

    /// 只有兜底文本的一轮**确实有输出**，`first_token_at` 不得为 None。
    ///
    /// 兜底文本是在 `generate_final_events` 里产生的，不经过 `process_kiro_event` 的打点；
    /// 覆盖它靠的是该函数末尾那一处 `mark_first_token_if_content`。把那一行删掉，
    /// 本测试即 FAIL（面板 `first_token_ms` 落 NULL = 有输出却测不到延迟）。
    #[test]
    fn fallback_only_turn_marks_first_token() {
        let mut ctx = StreamContext::new_with_thinking("deepseek", 10, false, HashMap::new());
        ctx.process_kiro_event(&reasoning_ev("只有推理的一轮"));
        assert!(
            ctx.first_token_at().is_none(),
            "前提校验：!thinking_enabled 下 reasoning 帧不产出任何 delta，此刻不该有打点"
        );
        ctx.generate_final_events();
        assert!(
            ctx.first_token_at().is_some(),
            "兜底文本是真下发的内容，收尾必须补上 TTFB 打点，否则 first_token_ms 恒为 NULL"
        );
    }

    /// 反向：被 completion 门挡住的失败轮**没有**内容产出，打点必须保持 None。
    #[test]
    fn gated_fallback_turn_does_not_mark_first_token() {
        let mut ctx = StreamContext::new_with_thinking("deepseek", 10, false, HashMap::new());
        ctx.process_kiro_event(&reasoning_ev("失败轮的推理"));
        ctx.mark_transport_error("connection reset by peer");
        ctx.generate_final_events();
        assert!(
            ctx.first_token_at().is_none(),
            "失败轮没下发任何内容，first_token_ms 应为 NULL（打点判据只认非空 delta）"
        );
    }

    #[test]
    fn test_strip_dsml_full_marker_in_one_chunk() {
        // DeepSeek 工具协议标记应被整段剥离,正常文本保留。
        let mut ctx = mk_ctx();
        let out = ctx.strip_dsml_markers("先看目录。\n\n<｜DSML｜function_calls｜>后续");
        assert_eq!(
            out, "先看目录。\n\n后续",
            "DSML 完整标记应被剥离,前后正常文本保留"
        );
        assert!(ctx.dsml_tail_buffer.is_empty());
    }

    #[test]
    fn test_strip_dsml_tool_calls_family() {
        let mut ctx = mk_ctx();
        let out = ctx.strip_dsml_markers("<｜tool▁calls▁begin｜><｜tool▁call▁begin｜>正文");
        assert_eq!(out, "正文", "tool_calls 家族标记应全部剥离");
    }

    #[test]
    fn test_strip_dsml_cross_chunk_split() {
        // 标记被上游从中间切成两个 chunk:第一块留半个标记到 tail,第二块拼上闭合后整段剥离。
        let mut ctx = mk_ctx();
        let out1 = ctx.strip_dsml_markers("正常文字<｜DSML｜func");
        assert_eq!(out1, "正常文字", "闭合前只输出正常文字,半个标记留 tail");
        assert!(!ctx.dsml_tail_buffer.is_empty(), "半个标记应留在 tail 缓冲");
        let out2 = ctx.strip_dsml_markers("tion_calls｜>之后");
        assert_eq!(out2, "之后", "拼上闭合后整段标记被剥离,只剩后续文本");
        assert!(ctx.dsml_tail_buffer.is_empty());
    }

    #[test]
    fn test_strip_dsml_split_at_angle_bracket() {
        // 实测坐实的分帧:`<` 单独在前一帧末尾,`｜DSML…` 在下一帧。
        let mut ctx = mk_ctx();
        let out1 = ctx.strip_dsml_markers("创建网页。\n\n<");
        assert_eq!(out1, "创建网页。\n\n", "末尾孤立 < 应 hold 到 tail,不输出");
        assert_eq!(ctx.dsml_tail_buffer, "<");
        let out2 = ctx.strip_dsml_markers("｜DSML｜function_calls｜>正文");
        assert_eq!(out2, "正文", "拼上后 <｜DSML…> 整段剥离");
        assert!(ctx.dsml_tail_buffer.is_empty());
    }

    #[test]
    fn test_strip_dsml_trailing_angle_then_normal() {
        // 末尾 < 被 hold,但下一帧是正常文本(非｜)→ < 应被还原输出,不丢字。
        let mut ctx = mk_ctx();
        let out1 = ctx.strip_dsml_markers("比较 a <");
        assert_eq!(out1, "比较 a ");
        let out2 = ctx.strip_dsml_markers(" b");
        assert_eq!(out2, "< b", "孤立 < 后接正常文本应还原,不误吞");
    }

    #[test]
    fn test_strip_dsml_does_not_touch_normal_text() {
        // 不含 DSML 的正常文本(哪怕有普通 < 号)绝不被改动。
        let mut ctx = mk_ctx();
        let out = ctx.strip_dsml_markers("if a < b && c > d 这是正常代码");
        assert_eq!(out, "if a < b && c > d 这是正常代码");
        assert!(ctx.dsml_tail_buffer.is_empty());
    }

    #[test]
    fn test_strip_dsml_claude_model_name_still_filters() {
        // 2026-08-10 门控删除：上游是 DeepSeek，客户端声明的模型名（claude-sonnet-4.6 之类）
        // 不决定是否剥离——哪怕是 claude 名，DeepSeek 后端照样可能吐 <｜DSML｜…> 标记。
        let mut ctx =
            StreamContext::new_with_thinking("claude-sonnet-4.6", 10, false, HashMap::new());
        let s = "DeepSeek 的标记写作 <｜DSML｜function_calls｜> 你看";
        assert_eq!(
            ctx.strip_dsml_markers(s),
            "DeepSeek 的标记写作  你看",
            "Claude 模型名也应剥离真实 DSML 标记（无条件剥离），标记本身剥净、两侧空格保留"
        );
        // 但白名单仍守正文：非关键字 <｜…> 在 claude 名下同样不误删。
        let mut ctx =
            StreamContext::new_with_thinking("claude-sonnet-4.6", 10, false, HashMap::new());
        let s = "见 <｜注｜关于x｜> 说明";
        assert_eq!(ctx.strip_dsml_markers(s), s, "非关键字 <｜…> 属正文，不误删");
        assert!(ctx.dsml_tail_buffer.is_empty());
    }

    #[test]
    fn test_strip_dsml_keyword_whitelist_preserves_normal_fullwidth() {
        // 国产模型下,<｜ 后不是 DSML/tool/function 关键字的正文(CJK 排版)不被误删。
        let mut ctx = mk_ctx(); // deepseek
        let s = "见 <｜注｜关于x｜> 说明";
        assert_eq!(
            ctx.strip_dsml_markers(s),
            s,
            "非关键字的 <｜…> 属正文,应保留"
        );
    }

    // ===== 半截标记跨行吞正文回归(2026-08-09 实测坐实,旧代码上全部 FAIL)=====
    //
    // 旧代码 `closed = position(|c| c == '>')` **不限行**:半截标记后跨行接正文时,正文里
    // 任意 `>`(`a > b` / `=>` / markdown 引用)被误当标记闭合 ⇒ 从标记起整段吞掉,只剩 `>`
    // 之后的残渣。实测 `<｜DSML｜function_calls\n阅读\n如果 a > b 就返回` → 只回 `" b 就返回"`。
    // 修复:闭合查找遇 `\n` 即停,半截标记只吃到**行尾**,换行及之后正文原样保留。

    #[test]
    fn test_strip_dsml_half_marker_does_not_swallow_next_lines_with_gt() {
        let mut ctx = mk_ctx();
        let out = ctx.strip_dsml_markers("<｜DSML｜function_calls\n阅读\n如果 a > b 就返回");
        assert_eq!(
            out, "阅读\n如果 a > b 就返回",
            "半截标记只剥本行,换行后正文(含 `>`)绝不吞"
        );
    }

    #[test]
    fn test_strip_dsml_half_marker_keeps_arrow_text() {
        let mut ctx = mk_ctx();
        let out = ctx.strip_dsml_markers("<｜DSML｜function_calls\n结果 => 成功\n后续");
        assert_eq!(out, "结果 => 成功\n后续", "正文里的 `=>` 不能被当标记闭合");
    }

    #[test]
    fn test_strip_dsml_half_marker_keeps_markdown_quote() {
        let mut ctx = mk_ctx();
        let out = ctx.strip_dsml_markers("<｜DSML｜function_calls\n分析\n> 引用\n继续");
        assert_eq!(out, "分析\n> 引用\n继续", "markdown 引用行首 `>` 不能被当标记闭合");
    }

    #[test]
    fn test_strip_dsml_double_half_marker() {
        // 实测形态:连续两个半截 function_calls 标记,后面跟正文。
        let mut ctx = mk_ctx();
        let out =
            ctx.strip_dsml_markers("<｜DSML｜function_calls\n<｜DSML｜function_calls\nFound header");
        assert_eq!(out, "Found header", "双重半截标记全剥,正文保留");
    }

    #[test]
    fn test_strip_dsml_close_tag_is_stripped() {
        // 闭合标签 `</｜DSML｜parameter>` 以 `</` 开头,旧代码只认 `<｜` ⇒ 闭合标签泄漏成垃圾文本。
        let mut ctx = mk_ctx();
        let out = ctx.strip_dsml_markers("echo hi</｜DSML｜parameter>\n回答");
        assert_eq!(out, "echo hi\n回答", "闭合标签必须被剥,不能泄漏");
    }

    #[test]
    fn test_strip_dsml_from_complete_text_non_stream() {
        // 非流式路径(handle_non_stream_request)此前零 DSML 处理,标记逐字泄漏。
        assert_eq!(
            strip_dsml_from_complete_text("<｜DSML｜function_calls｜>后续"),
            "后续"
        );
        assert_eq!(
            strip_dsml_from_complete_text("<｜DSML｜function_calls\n阅读\n如果 a > b 就返回"),
            "阅读\n如果 a > b 就返回",
            "非流式同样不能跨行吞正文"
        );
        assert_eq!(
            strip_dsml_from_complete_text("echo hi</｜DSML｜parameter>\n回答"),
            "echo hi\n回答",
            "非流式同样要剥闭合标签"
        );
        assert_eq!(
            strip_dsml_from_complete_text("现在绝对还有<｜DSML｜function_calls"),
            "现在绝对还有",
            "流尾半截标记丢弃,前面正文保留"
        );
        let normal = "if a < b && c > d 这是正常代码";
        assert_eq!(
            strip_dsml_from_complete_text(normal),
            normal,
            "正常文本(含普通 < >)绝不改写"
        );
        let fullwidth = "见 <｜注｜关于x｜> 说明";
        assert_eq!(
            strip_dsml_from_complete_text(fullwidth),
            fullwidth,
            "非关键字全角标记属正文,保留"
        );
    }

    // ===== 文本化 invoke 重组端到端(旧代码上会失败:旧代码把 <invoke> 当纯文本吐,不重组)=====

    /// 造一个开了重组 + 声明了工具 Bash 的 ctx。
    fn mk_reclaim_ctx() -> StreamContext {
        let mut known = std::collections::HashSet::new();
        known.insert("Bash".to_string());
        StreamContext::new_full("claude-opus-4.6", 10, false, HashMap::new(), known)
    }

    /// 判定事件流里是否有结构化 tool_use 的 content_block_start。
    fn has_tool_use_block(events: &[SseEvent]) -> bool {
        events.iter().any(|e| {
            e.event == "content_block_start" && e.data["content_block"]["type"] == "tool_use"
        })
    }

    #[test]
    fn test_reclaim_textified_invoke_to_tool_use() {
        // 行首完整 <invoke name="Bash"><parameter name="command">ls</parameter></invoke> + 工具名已声明
        // → 应重组成结构化 tool_use,且收尾 stop_reason=tool_use。用 concat 拼避免源码里出现字面工具标签。
        let mut ctx = mk_reclaim_ctx();
        let lt = "<";
        let block = format!(
            "{lt}invoke name=\"Bash\">{lt}parameter name=\"command\">ls -la{lt}/parameter>{lt}/invoke>"
        );
        let mut events = ctx.process_assistant_response(&block);
        events.extend(ctx.flush_invoke_sniff_buffer());
        assert!(
            has_tool_use_block(&events),
            "行首完整 invoke 块应重组成 tool_use"
        );
        assert_eq!(ctx.reclaimed_invoke_count, 1);
        assert_eq!(
            ctx.state_manager.get_stop_reason(),
            "tool_use",
            "重组后 stop_reason 应为 tool_use"
        );
    }

    #[test]
    fn test_reclaim_still_works_when_thinking_enabled() {
        // 回归:thinking 开启时重组层必须同样生效。
        //
        // 真机形态就是这个:Claude Code 恒带 thinking,所以 thinking_enabled=true 是**生产常态**。
        // 旧代码在 `if self.thinking_enabled { return process_content_with_thinking(..) }` 处提前
        // 返回,永远走不到下面的 reclaim 分支 → 文本化 invoke 全部当纯文本吐给客户端,工具不执行
        // (线上实测 textifiedInvokeHits=17 / reclaimedInvokeCalls=1)。
        // 既有 reclaim 测试全传 thinking_enabled=false,恰好绕开了这条唯一的生产路径。
        let mut known = std::collections::HashSet::new();
        known.insert("Bash".to_string());
        let mut ctx = StreamContext::new_full("claude-opus-4.6", 10, true, HashMap::new(), known);
        let lt = "<";
        let block = format!(
            "{lt}invoke name=\"Bash\">{lt}parameter name=\"command\">ls -la{lt}/parameter>{lt}/invoke>"
        );
        let mut events = ctx.process_assistant_response(&block);
        // ⚠️ 必须走 `generate_final_events()`（真实收尾路径），**不要**直接调
        // `flush_invoke_sniff_buffer()`：后者会跳过 generate_final_events 里
        // thinking_buffer 的排空分支，而那里正是最后一处 reclaim 旁路所在
        // （:2451 曾直接 create_text_delta_events）。直接调 flush 的写法能被
        // 「只在 flush 处并回尾巴」的假修复骗过 —— 那种修法在生产上完全无效，
        // 因为收尾时 thinking_buffer 早已被 :2453 清空。
        events.extend(ctx.generate_final_events());
        assert!(
            has_tool_use_block(&events),
            "thinking 开启时行首完整 invoke 块同样应重组成 tool_use(生产常态路径)"
        );
        assert_eq!(ctx.reclaimed_invoke_count, 1);
    }

    #[test]
    fn test_reclaim_works_for_invoke_after_thinking_close_midstream() {
        // 回归:`</thinking>\n\n` 之后紧跟文本化 invoke 块 —— Claude Code 的**生产形态**
        // (先思考,再调工具)。走 process_content_with_thinking 的
        // 「thinking 已提取」分支(:1649),该分支把残留整段交给 emit_non_thinking_text。
        //
        // 注:generate_final_events 里 `</thinking>` 之后那条 remaining 分支(:2425)
        // **不可达**于非空正文 —— find_real_thinking_end_tag_at_buffer_end 要求标签后
        // 全为空白,故 remaining.trim_start() 恒空。不要为它写测试(写了也只会
        // 走到本条覆盖的 :1649 路径上,是假覆盖)。
        let mut known = std::collections::HashSet::new();
        known.insert("Read".to_string());
        let mut ctx = StreamContext::new_full("claude-opus-4.6", 10, true, HashMap::new(), known);
        let lt = "<";
        // `find_real_thinking_end_tag` 要求 `</thinking>` 后紧跟 `\n\n` 才判定为真结束标签。
        let content = format!(
            "{lt}thinking>盘算一下{lt}/thinking>\n\n{lt}invoke name=\"Read\">\
             {lt}parameter name=\"file_path\">/tmp/a.txt{lt}/parameter>{lt}/invoke>"
        );
        let mut events = ctx.process_assistant_response(&content);
        events.extend(ctx.generate_final_events());
        assert!(
            has_tool_use_block(&events),
            "thinking 闭合后紧跟的 invoke 块应重组成 tool_use(Claude Code 生产形态)"
        );
        assert_eq!(ctx.reclaimed_invoke_count, 1);
    }

    #[test]
    fn thinking_tags_stripped_when_client_did_not_request_thinking() {
        // ⭐ 回归（删掉 strip_inline_thinking_when_disabled 的调用即必失败）：
        // 客户端没声明 thinking 时，模型仍可能吐内联 `<thinking>` 标签。
        // 本仓对同一种内容已有口径 —— process_reasoning_content 在 !thinking_enabled
        // 时**直接丢弃整帧**。内联标签若原样穿透就成了「结构化帧丢弃、内联标签泄漏」，
        // 模型的内部推理被当正文吐给用户。
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, false, HashMap::new());
        let _ = ctx.generate_initial_events();
        // `find_real_thinking_end_tag` 要求结束标签后跟 `\n\n`。
        let events =
            ctx.process_assistant_response("前言<thinking>内部推理不该外泄</thinking>\n\n正文");
        let text: String = events
            .iter()
            .filter(|e| e.event == "content_block_delta")
            .filter_map(|e| e.data["delta"]["text"].as_str())
            .collect();
        assert!(
            !text.contains("<thinking>") && !text.contains("</thinking>"),
            "thinking 标签不该出现在正文里，实际: {text:?}"
        );
        assert!(
            !text.contains("内部推理不该外泄"),
            "思考内容不该作为正文下发（口径须与 process_reasoning_content 的丢弃一致），实际: {text:?}"
        );
        assert!(
            text.contains("前言") && text.contains("正文"),
            "标签外的正文必须完整保留，实际: {text:?}"
        );
    }

    #[test]
    fn stripper_must_not_withhold_invoke_closing_tag() {
        // ⭐ 回归：剥离器扣留"半个标签"尾巴时若无条件扣 `"<thinking>".len()`=10 字节，
        // 会连带扣住 `</invoke>`（**只有 9 字节**）→ 重组层永远看到未闭合的 `<invoke`
        // → 当纯文本吐出 → **工具不执行**。这是 generate_final_events 那条 reclaim
        // 旁路的同型缺陷，第一版实现真的踩了（3 个既有 reclaim 测试当场变红）。
        //
        // 判据落在 partial_tag_suffix_len 上：不含 `<thinking>` 真前缀的尾巴必须立刻放行。
        assert_eq!(
            partial_thinking_tag_suffix_len("...</parameter></invoke>"),
            0,
            "`></invoke>` 这类尾巴不可能长成 thinking 标签，必须零扣留"
        );
        // 真的是半个标签 → 必须扣住，否则半标签会当正文外泄。
        assert_eq!(partial_thinking_tag_suffix_len("正文<thin"), 5);
        assert_eq!(partial_thinking_tag_suffix_len("正文<"), 1);
        // 多字节边界不得panic、不得切坏字符。
        assert_eq!(partial_thinking_tag_suffix_len("正文中文"), 0);
        // 大小写不敏感 + 容属性后仍必须扣住这些半标签，否则形态③④跨 chunk 时会外泄。
        assert_eq!(partial_thinking_tag_suffix_len("正文</THINKI"), 8);
        assert_eq!(partial_thinking_tag_suffix_len("正文<thinking fo"), 12);
        // 上界：属性区超过 MAX_THINKING_TAG_INNER_BYTES 即判定「不是标签」，
        // 否则扣留窗口无界增长 → 整条流停摆（已知问题 #14 同型）。
        let flood = format!(
            "正文<thinking {}",
            "x".repeat(MAX_THINKING_TAG_INNER_BYTES + 5)
        );
        assert_eq!(partial_thinking_tag_suffix_len(&flood), 0);
    }

    /// 🔬 取证探针：真机上到底哪种形态会让 thinking 标签泄漏到客户端可见文本里。
    ///
    /// 用户报告仍看到 thinking tag，而已有修复覆盖的是三种形态
    /// （`</thinking>Answer` 零空白 / 收尾残留蒸发 / 非流式零剥离）。本条把**尚未覆盖**
    /// 的候选形态一次全喂进去，断言"客户端可见文本里绝不含 thinking 标签字面量"，
    /// 让失败输出直接指出是哪一种。
    ///
    /// 不是回归测试，是**测量**：先让它告诉我真形态，再据此写修复与真回归。
    #[test]
    fn probe_which_shapes_leak_thinking_tags() {
        let lt = "<";
        let shapes: Vec<(&str, String)> = vec![
            // ① 孤立结束标签（无开标签）—— 「还没进 thinking 块」那条路径只扣留
            //    `"<thinking>".len()`=10 字节，而 `</thinking>` 是 11 字节。
            ("孤立闭标签", format!("答案开始{lt}/thinking>答案继续")),
            // ② 孤立结束标签跨 chunk 断开
            ("孤立闭标签跨chunk", format!("答案{lt}/think|ing>继续")),
            // ③ 只有开标签、永不闭合（流结束）
            ("开标签不闭合", format!("前言{lt}thinking>思考到一半就断了")),
            // ④ 嵌套/重复开标签
            (
                "重复开标签",
                format!("{lt}thinking>外{lt}thinking>内{lt}/thinking>\n\n正文"),
            ),
            // ⑤ 大写/混合大小写
            (
                "大写标签",
                format!("前言{lt}THINKING>思考{lt}/THINKING>\n\n正文"),
            ),
            // ⑥ 带属性的开标签
            (
                "带属性",
                format!("前言{lt}thinking foo=\"1\">思考{lt}/thinking>\n\n正文"),
            ),
            // ⑦ 标签前后有空格
            (
                "标签内空格",
                format!("前言{lt} thinking >思考{lt} /thinking >\n\n正文"),
            ),
        ];

        let mut leaked: Vec<String> = Vec::new();
        for thinking_enabled in [true, false] {
            for (name, input) in &shapes {
                let mut ctx = StreamContext::new_with_thinking(
                    "claude-sonnet-5",
                    1,
                    thinking_enabled,
                    HashMap::new(),
                );
                let mut ev = ctx.generate_initial_events();
                // 含 `|` 的用例按管道位置切成两个 chunk，模拟跨 chunk 到达。
                if let Some(pos) = input.find('|') {
                    ev.extend(ctx.process_assistant_response(&input[..pos]));
                    ev.extend(ctx.process_assistant_response(&input[pos + 1..]));
                } else {
                    ev.extend(ctx.process_assistant_response(input));
                }
                ev.extend(ctx.generate_final_events());
                // 只取客户端可见的 text_delta（thinking_delta 里出现标签不算泄漏）
                let visible: String = ev
                    .iter()
                    .filter(|e| e.event == "content_block_delta")
                    .filter_map(|e| e.data["delta"]["text"].as_str())
                    .collect();
                let low = visible.to_ascii_lowercase();
                if low.contains("<thinking") || low.contains("</thinking") {
                    leaked.push(format!(
                        "  [thinking_enabled={thinking_enabled}] {name}\n    可见文本: {visible:?}"
                    ));
                }
            }
        }
        assert!(
            leaked.is_empty(),
            "以下形态会把 thinking 标签泄漏进客户端可见文本：\n{}",
            leaked.join("\n")
        );
    }

    /// 走真实流路径取客户端**可见正文**（`text_delta` 拼接）。
    ///
    /// 探针只断言「不含标签字面量」——那挡不住「标签剥了但正文也被吃掉」的假修复。
    /// 下面四条按形态各自断言**精确**可见文本。
    fn visible_text_through_stream(thinking_enabled: bool, chunks: &[&str]) -> String {
        let mut ctx = StreamContext::new_with_thinking(
            "claude-sonnet-5",
            1,
            thinking_enabled,
            HashMap::new(),
        );
        let mut ev = ctx.generate_initial_events();
        for c in chunks {
            ev.extend(ctx.process_assistant_response(c));
        }
        ev.extend(ctx.generate_final_events());
        ev.iter()
            .filter(|e| e.event == "content_block_delta")
            .filter_map(|e| e.data["delta"]["text"].as_str())
            .collect()
    }

    /// ⭐ 回归 · 泄漏形态①：**孤立闭标签**（没有配对开标签）。
    ///
    /// 「还没进 thinking 块」那条路径原先无条件扣留 `"<thinking>".len()` = **10 字节**
    /// 当可能的半标签，而 `</thinking>` 是 **11 字节** ⇒ 整条标签穿透进 `text_delta`，
    /// 客户端逐字看到 `答案开始</thinking>答案继续`（实测）。
    ///
    /// 处置口径：孤立闭标签两侧都是**真正文**，故只丢标签本身、内容全留。
    ///
    /// 把 `partial_thinking_tag_suffix_len` 换回「固定扣 10 字节」→ 本测试必 FAILED。
    #[test]
    fn stray_end_tag_without_open_tag_must_be_stripped_keeping_both_sides() {
        for thinking_enabled in [true, false] {
            assert_eq!(
                visible_text_through_stream(thinking_enabled, &["答案开始</thinking>答案继续"]),
                "答案开始答案继续",
                "thinking={thinking_enabled}：孤立闭标签必须剥掉，两侧正文都不得丢"
            );
            // 标签后带段落分隔：与真结束标签同口径，吃掉 `\n\n`
            assert_eq!(
                visible_text_through_stream(thinking_enabled, &["答案</thinking>\n\n继续"]),
                "答案继续",
                "thinking={thinking_enabled}：孤立闭标签后的段落分隔应被一并消耗"
            );
            // 对照：被反引号包裹的是**正文在引用标签**，不得剥
            assert_eq!(
                visible_text_through_stream(thinking_enabled, &["用 `</thinking>` 收尾"]),
                "用 `</thinking>` 收尾",
                "thinking={thinking_enabled}：引用包裹的标签是正文，不得当标记剥掉"
            );
        }
    }

    /// ⭐ 回归 · 泄漏形态②：孤立闭标签**跨 chunk 断开**。
    ///
    /// 扣留窗口必须够得住 `</thinking>` 的 11 字节，否则前半截（`</think`）先被当正文
    /// 发走，客户端看到 `答案</thinking>继续`（实测）。
    ///
    /// 逐字节切开喂入 —— 任何一个切点上扣留不足都会漏出半个标签。
    #[test]
    fn stray_end_tag_split_across_chunks_must_not_leak_half_tag() {
        let input = "答案</thinking>继续";
        for thinking_enabled in [true, false] {
            // 在每个字符边界切成两段，穷举所有跨 chunk 形态
            for cut in 1..input.len() {
                if !input.is_char_boundary(cut) {
                    continue;
                }
                let got =
                    visible_text_through_stream(thinking_enabled, &[&input[..cut], &input[cut..]]);
                assert_eq!(
                    got, "答案继续",
                    "thinking={thinking_enabled} 切点={cut}：跨 chunk 的孤立闭标签不得泄漏半截"
                );
            }
        }
    }

    /// ⭐ 回归 · 泄漏形态③：**大写 / 混合大小写**标签。
    ///
    /// 原实现用写死字面量 `"<thinking>"` 做精确 `find`，全文件 thinking 路径上**一处
    /// 大小写归一化都没有** ⇒ `<THINKING>` 匹配不上 ⇒ 标签**连同思考内容一起**泄漏：
    /// 客户端看到 `前言<THINKING>思考</THINKING>\n\n正文`（实测，"思考"二字进了正文）。
    ///
    /// 把匹配改回大小写敏感 → 本测试必 FAILED。
    #[test]
    fn uppercase_thinking_tags_must_be_recognized() {
        for (name, chunks) in [
            ("全大写", vec!["前言<THINKING>思考</THINKING>\n\n正文"]),
            ("混合", vec!["前言<Thinking>思考</ThInKiNg>\n\n正文"]),
            (
                "大写跨chunk",
                vec!["前言<THINK", "ING>思考</THINKING>\n\n正文"],
            ),
        ] {
            for thinking_enabled in [true, false] {
                assert_eq!(
                    visible_text_through_stream(thinking_enabled, &chunks),
                    "前言正文",
                    "形态「{name}」thinking={thinking_enabled}：\
                     大写标签与其思考内容都不得进可见正文"
                );
            }
        }
        // thinking 开启时思考内容必须**进 thinking 块**（不是被丢掉）
        let mut ctx = StreamContext::new_with_thinking("claude-sonnet-5", 1, true, HashMap::new());
        let mut ev = ctx.generate_initial_events();
        ev.extend(ctx.process_assistant_response("前言<THINKING>思考</THINKING>\n\n正文"));
        ev.extend(ctx.generate_final_events());
        assert_eq!(
            collect_thinking_content(&ev),
            "思考",
            "大写标签内的内容应作为 thinking 下发，而不是被静默丢弃"
        );
    }

    /// ⭐ 回归 · 泄漏形态④：**带属性**的开标签。
    ///
    /// 精确 `find("<thinking>")` 匹配不上 `<thinking foo="1">` ⇒ 标签连同思考内容一起
    /// 泄漏（实测：`前言<thinking foo="1">思考</thinking>\n\n正文`）。
    ///
    /// 这条同时钉住**长度必须取自匹配结果**：属性使开标签变成 18 字节，若调用方仍按
    /// 写死的 10 字节跳过标签，属性残片 `foo="1">` 会留在缓冲里当思考/正文 → FAILED。
    #[test]
    fn thinking_tag_with_attributes_must_be_recognized() {
        for (name, chunks) in [
            (
                "单属性",
                vec!["前言<thinking foo=\"1\">思考</thinking>\n\n正文"],
            ),
            (
                "多属性",
                vec!["前言<thinking a=\"1\" b='2'>思考</thinking>\n\n正文"],
            ),
            (
                "闭标签带空白",
                vec!["前言<thinking>思考</thinking >\n\n正文"],
            ),
            (
                "属性跨chunk",
                vec!["前言<thinking fo", "o=\"1\">思考</thinking>\n\n正文"],
            ),
        ] {
            for thinking_enabled in [true, false] {
                assert_eq!(
                    visible_text_through_stream(thinking_enabled, &chunks),
                    "前言正文",
                    "形态「{name}」thinking={thinking_enabled}：\
                     带属性标签、属性残片、思考内容都不得进可见正文"
                );
            }
        }
        // 长度取自匹配结果的直接证据：属性残片不得出现在 thinking 内容里
        let mut ctx = StreamContext::new_with_thinking("claude-sonnet-5", 1, true, HashMap::new());
        let mut ev = ctx.generate_initial_events();
        ev.extend(
            ctx.process_assistant_response("前言<thinking foo=\"1\">思考</thinking>\n\n正文"),
        );
        ev.extend(ctx.generate_final_events());
        assert_eq!(
            collect_thinking_content(&ev),
            "思考",
            "属性残片（foo=\"1\">）不得留在 thinking 内容里 —— 说明跳标签用的是写死长度"
        );
    }

    /// ⭐ 回归：放宽匹配**不得**误伤散文里的 `<`，也不得让扣留窗口无界增长。
    ///
    /// 容属性后「可能是半个标签」失去了 10 字节的天然上界（`<thinking foo="...">` 可任意长）。
    /// 无上界时一个永不闭合的 `<thinking xxxx…` 会把整条可见文本囤死不下发 ——
    /// 这正是已知问题 #14（`invoke_sniff_buffer` 无界持有 → 流停摆）的同型。
    #[test]
    fn relaxed_matching_must_not_swallow_prose_or_stall_stream() {
        for thinking_enabled in [true, false] {
            // 散文里的比较运算符：`<` 后接空格/字母都不可能长成 thinking 标签
            assert_eq!(
                visible_text_through_stream(thinking_enabled, &["条件 a < b 时成立"]),
                "条件 a < b 时成立",
                "thinking={thinking_enabled}：散文里的 `<` 不得被吞"
            );
            // 别的标签名不得被当成 thinking
            assert_eq!(
                visible_text_through_stream(thinking_enabled, &["<thinkingfoo>正文"]),
                "<thinkingfoo>正文",
                "thinking={thinking_enabled}：`<thinkingfoo>` 是别的标签名，不得误剥"
            );
            // 超长属性区（永不闭合）必须放行为正文，不得囤住整条流。
            //
            // ⚠️ 填充串刻意**不用**单字符重复（`"x".repeat(200)`）：那会命中与本 case 无关的
            // 反刷屏熔断 `detect_structural_flood`（同一字母连续 ≥阈值即截断并置
            // `stray_guard_tripped`），于是断言失败的原因不是标签匹配。用递增编号做填充，
            // 既无单字符游程也无重复 token，且远超 `MAX_THINKING_TAG_INNER_BYTES`。
            let filler: String = (0..40).map(|i| format!("a{i} ")).collect();
            assert!(
                filler.len() > MAX_THINKING_TAG_INNER_BYTES,
                "填充串必须超过属性区上限才能测到"
            );
            let flood = format!("正文<thinking {filler}");
            let got = visible_text_through_stream(thinking_enabled, &[&flood]);
            assert!(
                got.contains("正文") && got.contains("a39"),
                "thinking={thinking_enabled}：永不闭合的超长伪标签必须放行为正文（否则流停摆），\
                 实际 {} 字节: {got:?}",
                got.len()
            );
        }
    }

    /// ⭐ 回归（**走真实流路径**）：thinking 关闭 + `</thinking>` 后**紧跟普通字符**时，
    /// 答案不得被永久丢弃。
    ///
    /// 这是 `strip_inline_thinking_when_disabled` 里最严重的形态：严格判据要求结束标签后
    /// 跟「含换行的空白」或 `<`，而 `</thinking>ANSWER` 两者都不是 ⇒ 判据返回 `None` ⇒
    /// 剥离器走「整段都还是思考内容」分支**丢弃全部内容** ⇒ 客户端收到**空回答**。
    /// 而后续 chunk 再来多少内容都改不了这个位置的判据（普通字符已就位）——
    /// 所以这不是"等一等就好"，是**永久丢弃**，且面板记为一次成功，完全无痕。
    ///
    /// 删掉 `find_permanently_unsatisfiable_end_tag` 那个分支 → 本测试必 FAILED。
    #[test]
    fn stripper_must_not_drop_answer_when_end_tag_hugs_text() {
        for (name, input, want) in [
            ("紧跟字母", "<thinking>盘算</thinking>ANSWER", "ANSWER"),
            ("紧跟中文", "<thinking>盘算</thinking>答案在此", "答案在此"),
            (
                "前言+紧跟",
                "前言<thinking>盘算</thinking>ANSWER",
                "前言ANSWER",
            ),
        ] {
            let mut ctx =
                StreamContext::new_with_thinking("claude-sonnet-5", 1, false, HashMap::new());
            let mut ev = ctx.generate_initial_events();
            ev.extend(ctx.process_assistant_response(input));
            ev.extend(ctx.generate_final_events());
            let text: String = ev
                .iter()
                .filter(|e| e.event == "content_block_delta")
                .filter_map(|e| e.data["delta"]["text"].as_str())
                .collect();
            assert_eq!(
                text, want,
                "形态「{name}」：thinking 关闭时答案必须完整下发（旧代码整段丢弃 → 空回答）"
            );
        }
    }

    /// ⭐ 回归：thinking 关闭时剥离器的**残留收尾**不得静默蒸发。
    ///
    /// `generate_final_events` 里原有的 flush 门是
    /// `self.thinking_enabled && !self.thinking_buffer.is_empty()`，而剥离器**只在
    /// `!thinking_enabled` 时**往同一个 `thinking_buffer` 写 —— 两个条件**互斥** ⇒
    /// 那条 flush 对剥离器的残留**永远不执行**。
    ///
    /// 删掉 `!self.thinking_enabled && !self.thinking_buffer.is_empty()` 那个分支
    /// → 本测试必 FAILED。
    #[test]
    fn stripper_residue_must_not_evaporate_at_stream_end() {
        // ① 尾巴是被 partial_tag_suffix_len 扣住的孤立 `<`（散文 "条件 a < b" 落在流末尾）。
        //    EOF 时它已确定不是标签 —— 是正文的一部分，必须下发。
        {
            let mut ctx =
                StreamContext::new_with_thinking("claude-sonnet-5", 1, false, HashMap::new());
            let mut ev = ctx.generate_initial_events();
            ev.extend(ctx.process_assistant_response("条件 a <"));
            ev.extend(ctx.generate_final_events());
            let text: String = ev
                .iter()
                .filter(|e| e.event == "content_block_delta")
                .filter_map(|e| e.data["delta"]["text"].as_str())
                .collect();
            assert_eq!(
                text, "条件 a <",
                "末尾孤立 `<` 必须在收尾时补发，不得静默吞字"
            );
        }
        // ② 未闭合的 thinking 块：思考本体丢弃（客户端没要），但**标签之后**的正文要下发。
        //    分两个 chunk 送达，逼真实跨 chunk 状态。
        {
            let mut ctx =
                StreamContext::new_with_thinking("claude-sonnet-5", 1, false, HashMap::new());
            let mut ev = ctx.generate_initial_events();
            ev.extend(ctx.process_assistant_response("<thinking>盘算一"));
            ev.extend(ctx.process_assistant_response("下"));
            ev.extend(ctx.generate_final_events());
            let text: String = ev
                .iter()
                .filter(|e| e.event == "content_block_delta")
                .filter_map(|e| e.data["delta"]["text"].as_str())
                .collect();
            assert!(
                !text.contains("盘算") && !text.contains("<thinking>"),
                "未闭合思考块的内容与标签都不得下发，实际: {text:?}"
            );
        }
    }

    /// ⭐ 回归：EOF 兜底只在**真的到了流末尾**才放宽，不得在流式期间抢判。
    ///
    /// 对照条：`split_unclosed_thinking_residue_at_eof` 用的是字面量 `</thinking>`，
    /// 若把它挪进流式热路径，正文里顺口提到该标签就会被当成真结束。
    #[test]
    fn eof_residue_split_is_literal_and_only_at_eof() {
        // 有标签 → 标签之后的内容作为可见尾巴（trim_start 掉紧随的空白）
        assert_eq!(
            split_unclosed_thinking_residue_at_eof("思考</thinking>\n\nANSWER"),
            "ANSWER"
        );
        assert_eq!(
            split_unclosed_thinking_residue_at_eof("思考</thinking>ANSWER"),
            "ANSWER"
        );
        // 无标签 → 整段都在未闭合思考块里，全部丢弃（口径同 process_reasoning_content 丢帧）
        assert_eq!(
            split_unclosed_thinking_residue_at_eof("思考到一半就断了"),
            ""
        );
        // 多字节内容不得 panic
        assert_eq!(split_unclosed_thinking_residue_at_eof("中文思考"), "");
    }

    /// ⭐ 回归：「等也没用」判据必须只对**永久不可满足**的位置放行。
    ///
    /// 放宽过头会把「该等」的形态提前判定，把段落分隔的后半个换行漏进正文
    /// （`\n` 在本 chunk、第二个 `\n` 在下一个）。
    #[test]
    fn permanently_unsatisfiable_end_tag_distinguishes_wait_from_hopeless() {
        // 该等：标签后什么都没到
        assert_eq!(
            find_permanently_unsatisfiable_end_tag("思考</thinking>"),
            None
        );
        // 该等：空白顶到缓冲末尾且未攒够 `\n\n`
        assert_eq!(
            find_permanently_unsatisfiable_end_tag("思考</thinking>\n"),
            None
        );
        assert_eq!(
            find_permanently_unsatisfiable_end_tag("思考</thinking> "),
            None
        );
        // 交给严格判据：含换行的空白 / 紧跟 `<`
        assert_eq!(
            find_permanently_unsatisfiable_end_tag("思考</thinking>\n\nA"),
            None
        );
        assert_eq!(
            find_permanently_unsatisfiable_end_tag("思考</thinking><invoke"),
            None
        );
        // 等也没用：普通字符已就位
        assert_eq!(
            tag_start(find_permanently_unsatisfiable_end_tag(
                "思考</thinking>ANSWER"
            )),
            Some("思考".len())
        );
        // 纯行内空白后还有内容 → 严格判据判为"正文顺口提到标签"，这里不抢
        assert_eq!(
            find_permanently_unsatisfiable_end_tag("思考</thinking> more"),
            None
        );
    }

    /// ⭐ 回归：**非流式**路径同样必须剥离内联 thinking。
    ///
    /// 剥离逻辑此前只在流式路径存在，非流式 `handlers.rs` 的 `!thinking_enabled` 分支把
    /// 上游文本**原样**塞进响应 ⇒ 标签与模型内部推理逐字泄漏。
    ///
    /// 本条同时钉住「判据必须复用」——若非流式另写一套匹配，下面任一形态都会漂移。
    #[test]
    fn non_stream_path_strips_inline_thinking_when_disabled() {
        for (name, input, want) in [
            ("双换行", "<thinking>盘算</thinking>\n\nANSWER", "ANSWER"),
            ("单换行", "<thinking>盘算</thinking>\nANSWER", "ANSWER"),
            ("紧跟正文", "<thinking>盘算</thinking>ANSWER", "ANSWER"),
            (
                "紧跟标签",
                "<thinking>盘算</thinking><invoke n=\"R\">",
                "<invoke n=\"R\">",
            ),
            (
                "前言保留",
                "前言<thinking>盘算</thinking>\n\nANSWER",
                "前言ANSWER",
            ),
            ("无标签原样", "普通回答", "普通回答"),
            ("未闭合全丢", "<thinking>盘算到一半", ""),
        ] {
            assert_eq!(
                strip_thinking_from_complete_text(input),
                want,
                "形态「{name}」非流式剥离结果不符"
            );
        }
        // 被反引号包裹的标签是**正文里的引用**，不得当真标签剥掉（与流式判据一致）。
        let quoted = "用 `<thinking>` 包裹推理";
        assert_eq!(strip_thinking_from_complete_text(quoted), quoted);
    }

    /// ⭐ 源码级守卫：非流式 `!thinking_enabled` 分支**必须**调剥离函数。
    ///
    /// 上面那条只断言纯函数自身 —— 把 `handlers.rs` 的调用点改回
    /// `"text": text_content` 它依然全绿（纸面测试，本仓已踩过四次）。
    /// 这条钉住调用点本身。
    #[test]
    fn non_stream_handler_must_call_the_stripper() {
        let src = include_str!("handlers.rs");
        // needle 运行时拼接，避免本断言自身成为 grep 的假命中源。
        let call = ["strip_thinking_from_complete_text", "(&text_content)"].concat();
        assert!(
            src.contains(&call),
            "handlers.rs 的非流式分支必须调 strip_thinking_from_complete_text(&text_content)，\
             否则 thinking 标签在非流式路径逐字泄漏"
        );
        // 承重：不得再有「原样塞入」的写法（那正是缺陷本体）。
        //
        // ⚠️ 必须先切掉注释行再搜：第一版裸搜全文，命中的是**修复本身留下的那句
        // 说明注释**（"此前这里是 ... 原样塞入"）⇒ 修复正确却报 FAILED。
        // 这正是本仓反复踩的「锚点选到散文」，只是方向相反（假阳性而非假阴性）。
        let code_only: String = src
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        let raw = ["\"text\": text_content"].concat();
        assert!(
            !code_only.contains(&raw),
            "handlers.rs 不得把 text_content 原样塞进响应（内联 thinking 标签会泄漏）"
        );
    }

    #[test]
    fn test_reclaim_gated_by_unknown_tool_name() {
        // 工具名硬护栏:解析出的工具名不在声明表里 → 不重组,当普通文本吐(宁可漏捞不误执行)。
        let mut known = std::collections::HashSet::new();
        known.insert("Read".to_string()); // 只声明 Read,没声明 Bash
        let mut ctx = StreamContext::new_full("claude-opus-4.6", 10, false, HashMap::new(), known);
        let lt = "<";
        let block = format!(
            "{lt}invoke name=\"Bash\">{lt}parameter name=\"x\">1{lt}/parameter>{lt}/invoke>"
        );
        let mut events = ctx.process_assistant_response(&block);
        events.extend(ctx.flush_invoke_sniff_buffer());
        assert!(!has_tool_use_block(&events), "未声明的工具名不应被重组执行");
        assert_eq!(ctx.reclaimed_invoke_count, 0);
    }

    #[test]
    fn test_reclaim_split_across_chunks() {
        // 跨 chunk 切分的 invoke 块:分片到达仍应重组(sniff 缓冲 hold 到闭合)。
        let mut ctx = mk_reclaim_ctx();
        let lt = "<";
        let mut events = Vec::new();
        events.extend(ctx.process_assistant_response(&format!("{lt}invoke name=\"Ba")));
        events.extend(
            ctx.process_assistant_response(&format!("sh\">{lt}parameter name=\"command\">echo hi")),
        );
        events.extend(ctx.process_assistant_response(&format!("{lt}/parameter>{lt}/invoke>")));
        events.extend(ctx.flush_invoke_sniff_buffer());
        assert!(
            has_tool_use_block(&events),
            "跨 chunk 分片的 invoke 应重组成 tool_use"
        );
    }

    #[test]
    fn test_reclaim_disabled_when_no_tools_declared() {
        // 未声明任何工具(known 空)→ 不进重组路径,<invoke> 原样当文本吐(new_with_thinking 空集=不启用)。
        let mut ctx =
            StreamContext::new_with_thinking("claude-opus-4.6", 10, false, HashMap::new());
        let lt = "<";
        let block = format!(
            "{lt}invoke name=\"Bash\">{lt}parameter name=\"c\">x{lt}/parameter>{lt}/invoke>"
        );
        let events = ctx.process_assistant_response(&block);
        assert!(!has_tool_use_block(&events), "无声明工具时不重组");
        assert!(
            ctx.invoke_sniff_buffer.is_empty(),
            "不启用重组则不进 sniff 缓冲"
        );
    }

    // ===== 结构性 stray 熔断(治 course/课 打地鼠 + 修 thinking/无工具盲区,旧代码上会失败)=====

    #[test]
    fn test_structural_flood_single_line_cjk() {
        // 单行连写「课」×100(无换行)——旧独占行匹配漏,结构性检测应抓到并从游程起点截断。
        let s = format!("正常开头 {}", "课".repeat(100));
        let cut = detect_structural_flood(&s);
        assert!(cut.is_some(), "单行连写课刷屏应被结构性检测命中");
    }

    #[test]
    fn test_structural_flood_multichar_course() {
        // "coursecourse…" ×40(词表里根本没 course)——多字符 token 连写应被抓。
        let s = "course".repeat(40);
        assert!(
            detect_structural_flood(&s).is_some(),
            "course 连写应被结构性检测命中(不靠词表)"
        );
    }

    #[test]
    fn test_structural_flood_normal_text_safe() {
        // 正常文本(含少量重复词)不应误判。
        assert!(
            detect_structural_flood("这是一段正常的中文回复,讲解代码逻辑和实现细节。").is_none()
        );
        assert!(detect_structural_flood("the quick brown fox jumps over the lazy dog").is_none());
        assert!(
            detect_structural_flood("aaa bbb ccc").is_none(),
            "短重复但未达阈值不误判"
        );
    }

    #[test]
    fn test_observe_stray_leak_forms() {
        let mut sa = 0u32;
        let mut il = 0u32;
        // 独占行:course 单独一行 → standalone。
        observe_stray_leak_forms("正常\ncourse\n继续", &mut sa, &mut il);
        assert_eq!(sa, 1, "course 独占行应计 standalone");
        // 句中紧贴 CJK:`重读course了` 里 course 前后都贴 CJK → inline。
        let (mut sa2, mut il2) = (0u32, 0u32);
        observe_stray_leak_forms("重读course了", &mut sa2, &mut il2);
        assert_eq!(il2, 1, "句中紧贴 CJK 的 course 应计 inline");
        // 正常英文(有空格分隔)不误判:"the course is" 里 course 两侧是空格非 CJK。
        let (mut sa3, mut il3) = (0u32, 0u32);
        observe_stray_leak_forms("the course is good", &mut sa3, &mut il3);
        assert_eq!(sa3, 0, "正常英文散文不计 standalone");
        assert_eq!(il3, 0, "有空格分隔的正常 course 不计 inline");
        // 完全不含 stray 词:零开销快路径 + 零计数。
        let (mut sa4, mut il4) = (0u32, 0u32);
        observe_stray_leak_forms("一段完全正常的中文回复讲解逻辑", &mut sa4, &mut il4);
        assert_eq!(sa4 + il4, 0);
    }

    #[test]
    fn test_stray_guard_covers_thinking_path() {
        // 核心盲区修复:thinking 开着时,课刷屏也要被熔断(旧代码 thinking 提前 return 完全绕过)。
        let mut ctx = StreamContext::new_with_thinking("claude-opus-4.6", 10, true, HashMap::new());
        let flood = format!("{}", "课".repeat(200));
        let events = ctx.process_assistant_response(&flood);
        assert!(ctx.stray_guard_tripped, "thinking 路径的课刷屏也应触发熔断");
        // 熔断后本轮应几乎不吐正文(截断在游点起点)。
        let _ = events;
    }

    #[test]
    fn test_thinking_buffer_bounded_on_whitespace_flood() {
        // review Finding 5 修复:上游持续吐纯空白(无 <thinking>)时 thinking_buffer 不应无界增长。
        let mut ctx = StreamContext::new_with_thinking("claude-opus-4.6", 10, true, HashMap::new());
        // 每次喂一大块纯空白(不含 <thinking>),多轮累积。旧代码 buffer 只涨不裁。
        for _ in 0..20 {
            let _ = ctx.process_content_with_thinking(&" ".repeat(50_000));
        }
        assert!(
            ctx.thinking_buffer.len() <= MAX_THINKING_BUFFER_BYTES + 50_000,
            "纯空白洪水下 thinking_buffer 应被上限约束,实测 {} 字节",
            ctx.thinking_buffer.len()
        );
    }

    #[test]
    fn test_stray_guard_covers_no_tools_path() {
        // 核心盲区修复:无工具声明请求(known_tool_names 空)的课刷屏也要被处理。
        // 用单行连写(泄漏清洗器只剥行首独占,够不到单行连写)专门验证 guard 生效——
        // 逐行独占的课会被 clean_leaked_tokens 先剥掉(那也是有效清除路径),故此处用连写测 guard。
        let mut ctx =
            StreamContext::new_with_thinking("claude-opus-4.6", 10, false, HashMap::new());
        let flood = format!("正文{}", "课".repeat(200));
        let events = ctx.process_assistant_response(&flood);
        assert!(
            ctx.stray_guard_tripped,
            "无工具请求的单行课连写刷屏应触发 guard 熔断"
        );
        // 熔断后吐出的文本里课的数量应远少于 200(截断在游程起点)。
        let emitted: String = events
            .iter()
            .filter_map(|e| e.data["delta"]["text"].as_str())
            .collect();
        assert!(emitted.matches('课').count() < 200, "熔断应截断掉大部分课");
    }

    #[test]
    fn test_strip_dsml_flush_recovers_leftover() {
        // 末尾孤立 < 被 hold 后,若流结束(无下一帧),flush_dsml_tail 应把它作为普通文本补发,不吞字。
        let mut ctx = mk_ctx();
        let out = ctx.strip_dsml_markers("结尾是 a <");
        assert_eq!(out, "结尾是 a ");
        assert_eq!(ctx.dsml_tail_buffer, "<");
        // 模拟流结束:flush 应产出含 "<" 的 text_delta 事件,tail 清空。
        let flushed = ctx.flush_dsml_tail();
        assert!(!flushed.is_empty(), "flush 应补发残留 <,不静默吞字");
        assert!(ctx.dsml_tail_buffer.is_empty());
    }

    #[test]
    fn test_strip_dsml_marker_no_gt_discarded_on_flush() {
        // 实测漏的形态:<｜DSML｜function_calls 单帧到达且不以 > 收尾(DeepSeek 标记本就以后续为界)。
        // 应被识别为标记 hold 到 tail 且标记 is_marker,流结束 flush 时**丢弃不补发**,不泄漏。
        let mut ctx = mk_ctx(); // deepseek
        let out = ctx.strip_dsml_markers("我来看看目录。\n\n<｜DSML｜function_calls");
        assert_eq!(out, "我来看看目录。\n\n", "正文保留,标记 hold 不输出");
        assert!(ctx.dsml_tail_is_marker, "应标记为确认标记");
        let flushed = ctx.flush_dsml_tail();
        assert!(
            flushed.is_empty(),
            "确认标记的残留 flush 时丢弃,绝不当正文补发(否则泄漏)"
        );
        assert!(ctx.dsml_tail_buffer.is_empty());
    }

    #[test]
    fn test_strip_dsml_unclosed_marker_bounded_flush() {
        // 是关键字但超长不闭合 → 不无界囤积,放行为正文(防吞正文/防无界)。
        let mut ctx = mk_ctx();
        let long = format!("<｜tool{}", "x".repeat(60)); // >DSML_TAIL_MAX 且无 >
        let out = ctx.strip_dsml_markers(&long);
        assert!(out.contains("tool"), "超长未闭合应放行为正文,不吞");
    }

    #[test]
    fn test_dsml_flush_after_thinking_close_keeps_block_order() {
        // 回归(对抗 review #2):国产模型 + thinking 开启,流在 thinking 块内结束,且最后一帧
        // 内容以孤立 `<` 收尾(被 strip_dsml_markers hold 进 dsml_tail_buffer,不进 thinking_buffer)。
        // generate_final_events 必须先 stop thinking 块,再把 DSML 残留作为 text 块 start——
        // 否则会出现「text 块 start(大索引)→ thinking 块 stop(小索引)」交错,违反 Anthropic
        // 「先 stop 当前块再 start 下一块」契约,CC 解析报错。
        let mut ctx = StreamContext::new_with_thinking("deepseek", 10, true, HashMap::new());

        // 驱动进入 thinking 块并停在块内;末尾孤立 `<` 会被 DSML 逻辑 hold 到 tail。
        let _ = ctx.process_assistant_response("<thinking>我在想 <");
        assert!(ctx.in_thinking_block, "应仍处于 thinking 块内");
        assert_eq!(
            ctx.dsml_tail_buffer, "<",
            "末尾孤立 < 应被 hold 到 DSML tail,而非进 thinking_buffer"
        );

        let events = ctx.generate_final_events();

        // 收集块生命周期事件,校验:同一 index 的 text start 必须在 thinking stop 之后。
        // 找出 thinking 块 stop 的位置与任何 text 块 start 的位置。
        let mut thinking_stop_pos: Option<usize> = None;
        let mut text_start_pos: Option<usize> = None;
        for (pos, e) in events.iter().enumerate() {
            let idx = e.data.get("index").and_then(|v| v.as_i64());
            match e.event.as_str() {
                "content_block_stop" => {
                    // thinking 块索引来自 ctx.thinking_block_index
                    if idx == ctx.thinking_block_index.map(|i| i as i64) {
                        thinking_stop_pos = Some(pos);
                    }
                }
                "content_block_start" => {
                    let is_text = e
                        .data
                        .get("content_block")
                        .and_then(|cb| cb.get("type"))
                        .and_then(|t| t.as_str())
                        == Some("text");
                    if is_text && text_start_pos.is_none() {
                        text_start_pos = Some(pos);
                    }
                }
                _ => {}
            }
        }

        // 残留 `<` 会作为 text 块补发,thinking 块必然先 stop。
        let ts = thinking_stop_pos.expect("thinking 块应被 stop");
        if let Some(txs) = text_start_pos {
            assert!(
                ts < txs,
                "thinking 块 stop(pos={}) 必须早于 DSML 残留 text 块 start(pos={}),否则块顺序交错",
                ts,
                txs
            );
        }
        assert!(ctx.dsml_tail_buffer.is_empty(), "flush 后 tail 应清空");
    }

    #[test]
    fn test_sse_state_manager_message_start() {
        let mut manager = SseStateManager::new();

        // 第一次应该成功
        let event = manager.handle_message_start(json!({"type": "message_start"}));
        assert!(event.is_some());

        // 第二次应该被跳过
        let event = manager.handle_message_start(json!({"type": "message_start"}));
        assert!(event.is_none());
    }

    #[test]
    fn test_sse_state_manager_block_lifecycle() {
        let mut manager = SseStateManager::new();

        // 创建块
        let events = manager.handle_content_block_start(0, "text", json!({}));
        assert_eq!(events.len(), 1);

        // delta
        let event = manager.handle_content_block_delta(0, json!({}));
        assert!(event.is_some());

        // stop
        let event = manager.handle_content_block_stop(0);
        assert!(event.is_some());

        // 重复 stop 应该被跳过
        let event = manager.handle_content_block_stop(0);
        assert!(event.is_none());
    }

    #[test]
    fn test_tool_name_reverse_mapping_in_stream() {
        use crate::kiro::model::events::ToolUseEvent;

        let mut map = HashMap::new();
        map.insert(
            "short_abc12345".to_string(),
            "mcp__very_long_original_tool_name".to_string(),
        );

        let mut ctx = StreamContext::new_with_thinking("test-model", 1, false, map);
        let _ = ctx.generate_initial_events();

        // 模拟 Kiro 返回短名称的 tool_use
        let tool_event = Event::ToolUse(ToolUseEvent {
            name: "short_abc12345".to_string(),
            tool_use_id: "toolu_01".to_string(),
            input: r#"{"key":"value"}"#.to_string(),
            stop: true,
        });

        let events = ctx.process_kiro_event(&tool_event);

        // content_block_start 中的 name 应该是原始长名称
        let start_event = events
            .iter()
            .find(|e| e.event == "content_block_start")
            .unwrap();
        assert_eq!(
            start_event.data["content_block"]["name"], "mcp__very_long_original_tool_name",
            "应还原为原始工具名称"
        );
    }

    /// 跑一串 tool_use 帧，返回 (拼接出的 partial_json 全文, 发出的 input_json_delta 事件数)。
    /// 根治后应恒为「单个 delta 在 stop 时发出」，故 delta 数应为 0(空参)或 1(有参)。
    fn run_tool_frames(ctx: &mut StreamContext, frames: &[(&str, bool)]) -> (String, usize) {
        use crate::kiro::model::events::ToolUseEvent;
        let mut out = String::new();
        let mut delta_count = 0usize;
        for (input, stop) in frames {
            let ev = Event::ToolUse(ToolUseEvent {
                name: "t".to_string(),
                tool_use_id: "toolu_x".to_string(),
                input: input.to_string(),
                stop: *stop,
            });
            for e in ctx.process_kiro_event(&ev) {
                if e.event == "content_block_delta" && e.data["delta"]["type"] == "input_json_delta"
                {
                    out.push_str(e.data["delta"]["partial_json"].as_str().unwrap_or(""));
                    delta_count += 1;
                }
            }
        }
        (out, delta_count)
    }

    /// 兼容旧断言：只取拼接全文。
    fn collect_tool_partial_json(ctx: &mut StreamContext, frames: &[(&str, bool)]) -> String {
        run_tool_frames(ctx, frames).0
    }

    #[test]
    fn test_tool_input_cumulative_snapshots() {
        // 上游发累积快照：每帧是"到目前为止的完整 JSON"。转发拼接后不应重复,应恰为最终完整 JSON。
        let mut ctx = StreamContext::new_with_thinking("m", 1, false, HashMap::new());
        let _ = ctx.generate_initial_events();
        let joined = collect_tool_partial_json(
            &mut ctx,
            &[
                (r#"{"a""#, false),
                (r#"{"a":1"#, false),
                (r#"{"a":1,"b":2}"#, true),
            ],
        );
        assert_eq!(
            joined, r#"{"a":1,"b":2}"#,
            "累积模式:拼接后应为完整 JSON,无重复"
        );
    }

    #[test]
    fn test_tool_input_repeated_final_frame() {
        // 累积模式常见收尾：stop 帧重复带上完整 JSON（与上一帧相同）→ 不应重复发。
        let mut ctx = StreamContext::new_with_thinking("m", 1, false, HashMap::new());
        let _ = ctx.generate_initial_events();
        let joined = collect_tool_partial_json(
            &mut ctx,
            &[
                (r#"{"a":1}"#, false),
                (r#"{"a":1}"#, true), // 完全重复帧
            ],
        );
        assert_eq!(joined, r#"{"a":1}"#, "重复帧不应二次转发");
    }

    #[test]
    fn test_tool_input_pure_deltas() {
        // 上游发纯增量:每帧是不同片段。转发原样,拼接后仍为完整 JSON。
        let mut ctx = StreamContext::new_with_thinking("m", 1, false, HashMap::new());
        let _ = ctx.generate_initial_events();
        let joined = collect_tool_partial_json(
            &mut ctx,
            &[(r#"{"a""#, false), (r#":1,"#, false), (r#""b":2}"#, true)],
        );
        assert_eq!(
            joined, r#"{"a":1,"b":2}"#,
            "增量模式:原样转发,拼接后为完整 JSON"
        );
    }

    #[test]
    fn test_tool_input_single_full_snapshot() {
        // 单帧完整 JSON（最常见）:原样一次发出。
        let mut ctx = StreamContext::new_with_thinking("m", 1, false, HashMap::new());
        let _ = ctx.generate_initial_events();
        let joined = collect_tool_partial_json(&mut ctx, &[(r#"{"k":"v"}"#, true)]);
        assert_eq!(joined, r#"{"k":"v"}"#);
    }

    #[test]
    fn test_tool_input_single_delta_invariant() {
        // 根治不变式：无论上游发几帧，最终只在 stop 发**一个** input_json_delta（缓冲到 stop 再发）。
        let mut ctx = StreamContext::new_with_thinking("m", 1, false, HashMap::new());
        let _ = ctx.generate_initial_events();
        let (joined, n) = run_tool_frames(
            &mut ctx,
            &[
                (r#"{"a""#, false),
                (r#"{"a":1"#, false),
                (r#"{"a":1,"b":2}"#, true),
            ],
        );
        assert_eq!(joined, r#"{"a":1,"b":2}"#);
        assert_eq!(n, 1, "应只发一个 delta（缓冲到 stop 一次性发）");
    }

    #[test]
    fn test_tool_input_non_prefix_trap() {
        // 旧逐片启发式的致命陷阱:上游第二帧不以第一帧为前缀(非单调重写)。
        // 根治后（merge_tool_input 第 6 步）：两帧各自都是完整合法 JSON → 视为"重写",
        // 只保留最新完整对象,消灭 `}{` 粘连非法串(Invalid tool parameters 类型 C 根因)。
        let mut ctx = StreamContext::new_with_thinking("m", 1, false, HashMap::new());
        let _ = ctx.generate_initial_events();
        let (joined, n) = run_tool_frames(
            &mut ctx,
            &[(r#"{"path":"/a"}"#, false), (r#"{"path":"/b"}"#, true)],
        );
        // 只发一个 delta,且结果是合法 JSON(第二帧),不再是 `}{` 粘连串。
        assert_eq!(n, 1, "非前缀帧也只发一个 delta");
        assert_eq!(
            joined, r#"{"path":"/b"}"#,
            "非前缀双完整对象只留最新完整对象"
        );
        assert!(
            serde_json::from_str::<serde_json::Value>(&joined).is_ok(),
            "结果必须是合法 JSON"
        );
    }

    #[test]
    fn test_tool_input_illegal_json_at_stop_repaired_by_default() {
        // 修复层默认开：上游发本就非法的 JSON（\x 是 JSON 不支持的转义）→ 修复层介入修成合法后发出，
        // 客户端能正常 parse，不再报 Invalid tool parameters。`\xd7` 的 `\x` 非法 → 降级字面 `\\xd7`。
        let mut ctx = StreamContext::new_with_thinking("m", 1, false, HashMap::new());
        let _ = ctx.generate_initial_events();
        let (joined, n) = run_tool_frames(&mut ctx, &[(r#"{"a":"\xd7"}"#, true)]);
        assert_eq!(n, 1, "非法 JSON 也要发出(不静默空参)");
        assert!(
            serde_json::from_str::<serde_json::Value>(&joined).is_ok(),
            "修复层默认开：发给客户端的必须是合法 JSON，实际={}",
            joined
        );
        // 值语义：`\x` 非法转义降级为字面反斜杠，值为字面 `\xd7`。
        let v: serde_json::Value = serde_json::from_str(&joined).unwrap();
        assert_eq!(
            v["a"].as_str().unwrap(),
            r"\xd7",
            "非法 \\x 转义降级为字面反斜杠"
        );
    }

    // 注：修复层"关闭时原样透传"的行为不单独用 static 开关测——进程级 static 在并行测试下会互相
    // 污染（一个测试 set(false) 期间别的 ON 前提测试恰好在跑就会假失败）。透传分支的正确性由
    // flush_tool_input 里 `if tool_repair_json_enabled()` 的显式门控保证（关则完全不调 repair），
    // 修复函数本身的正确性由上面的纯函数注入测试独立覆盖，两者组合已充分且无并发风险。

    #[test]
    fn test_tool_input_truncated_stream_flushes_on_final() {
        // 截断:tool_use 帧永不带 stop,流结束。generate_final_events 应把残留缓冲发出并关闭块,
        // 客户端不会卡在未闭合 tool_use 块上。
        use crate::kiro::model::events::ToolUseEvent;
        let mut ctx = StreamContext::new_with_thinking("m", 1, false, HashMap::new());
        let _ = ctx.generate_initial_events();
        // 两帧累积,均无 stop
        for input in [r#"{"a""#, r#"{"a":1}"#] {
            let ev = Event::ToolUse(ToolUseEvent {
                name: "t".to_string(),
                tool_use_id: "toolu_x".to_string(),
                input: input.to_string(),
                stop: false,
            });
            let evs = ctx.process_kiro_event(&ev);
            // stop 前不应发任何 input_json_delta
            assert!(!evs.iter().any(|e| e.event == "content_block_delta"
                && e.data["delta"]["type"] == "input_json_delta"));
        }
        let finals = ctx.generate_final_events();
        let delta = finals.iter().find(|e| {
            e.event == "content_block_delta" && e.data["delta"]["type"] == "input_json_delta"
        });
        assert!(delta.is_some(), "截断收尾应 flush 残留 tool input");
        assert_eq!(delta.unwrap().data["delta"]["partial_json"], r#"{"a":1}"#);
        // 块应被关闭
        assert!(
            finals.iter().any(|e| e.event == "content_block_stop"),
            "截断应关闭 tool 块"
        );
    }

    #[test]
    fn test_text_delta_after_tool_use_restarts_text_block() {
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, false, HashMap::new());

        let initial_events = ctx.generate_initial_events();
        assert!(
            initial_events
                .iter()
                .any(|e| e.event == "content_block_start"
                    && e.data["content_block"]["type"] == "text")
        );

        let initial_text_index = ctx
            .text_block_index
            .expect("initial text block index should exist");

        // tool_use 开始会自动关闭现有 text block
        let tool_events = ctx.process_tool_use(&crate::kiro::model::events::ToolUseEvent {
            name: "test_tool".to_string(),
            tool_use_id: "tool_1".to_string(),
            input: "{}".to_string(),
            stop: false,
        });
        assert!(
            tool_events.iter().any(|e| {
                e.event == "content_block_stop"
                    && e.data["index"].as_i64() == Some(initial_text_index as i64)
            }),
            "tool_use should stop the previous text block"
        );

        // 之后再来文本增量，应自动创建新的 text block 而不是往已 stop 的块里写 delta
        let text_events = ctx.process_assistant_response("hello");
        let new_text_start_index = text_events.iter().find_map(|e| {
            if e.event == "content_block_start" && e.data["content_block"]["type"] == "text" {
                e.data["index"].as_i64()
            } else {
                None
            }
        });
        assert!(
            new_text_start_index.is_some(),
            "should start a new text block"
        );
        assert_ne!(
            new_text_start_index.unwrap(),
            initial_text_index as i64,
            "new text block index should differ from the stopped one"
        );
        assert!(
            text_events.iter().any(|e| {
                e.event == "content_block_delta"
                    && e.data["delta"]["type"] == "text_delta"
                    && e.data["delta"]["text"] == "hello"
            }),
            "should emit text_delta after restarting text block"
        );
    }

    #[test]
    fn test_tool_use_flushes_pending_thinking_buffer_text_before_tool_block() {
        // thinking 模式下，**可能是半个 `<thinking>` 的尾巴**会被暂存在 thinking_buffer 等
        // 跨 chunk 匹配。当紧接着出现 tool_use 时，应先 flush 这段文本，再开始 tool_use block。
        //
        // ⚠️ 输入形态已随扣留判据更新：原用两段中文（"有修" + "改："）作为"被暂存的短文本"，
        // 那是**旧判据的副作用**而非不变量 —— 旧代码无条件扣末尾 10 字节，中文又因
        // `find_char_boundary` 向前退到字符边界，于是 12 字节的纯中文恰好一个字都发不出。
        // 扣留改为「按标签语法判定」后，不可能是标签的散文立即下发（这正是
        // `partial_thinking_tag_suffix_len` 的设计目的：首字节少等一个 chunk），
        // 纯中文不再滞留。本测试要钉的是「**有**待发内容时先 flush 再开 tool_use」，
        // 故改喂真的会被扣住的半标签；下方所有块顺序断言一字未改。
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new());
        let _initial_events = ctx.generate_initial_events();

        // 两段拼起来是 `<think` —— `<thinking>` 的真前缀，必须被扣在缓冲里等下一个 chunk。
        let ev1 = ctx.process_assistant_response("<thi");
        assert!(
            ev1.iter().all(|e| e.event != "content_block_delta"),
            "half tag prefix should be buffered under thinking mode"
        );
        let ev2 = ctx.process_assistant_response("nk");
        assert!(
            ev2.iter().all(|e| e.event != "content_block_delta"),
            "half tag prefix should still be buffered under thinking mode"
        );
        assert_eq!(
            ctx.thinking_buffer, "<think",
            "半标签必须完整滞留（否则下面的 flush 场景根本没被触发，断言会变成恒真的纸面测试）"
        );

        let events = ctx.process_tool_use(&crate::kiro::model::events::ToolUseEvent {
            name: "Write".to_string(),
            tool_use_id: "tool_1".to_string(),
            input: "{}".to_string(),
            stop: false,
        });

        let text_start_index = events.iter().find_map(|e| {
            if e.event == "content_block_start" && e.data["content_block"]["type"] == "text" {
                e.data["index"].as_i64()
            } else {
                None
            }
        });
        let pos_text_delta = events.iter().position(|e| {
            e.event == "content_block_delta" && e.data["delta"]["type"] == "text_delta"
        });
        let pos_text_stop = text_start_index.and_then(|idx| {
            events.iter().position(|e| {
                e.event == "content_block_stop" && e.data["index"].as_i64() == Some(idx)
            })
        });
        let pos_tool_start = events.iter().position(|e| {
            e.event == "content_block_start" && e.data["content_block"]["type"] == "tool_use"
        });

        assert!(
            text_start_index.is_some(),
            "should start a text block to flush buffered text"
        );
        assert!(
            pos_text_delta.is_some(),
            "should flush buffered text as text_delta"
        );
        assert!(
            pos_text_stop.is_some(),
            "should stop text block before tool_use block starts"
        );
        assert!(pos_tool_start.is_some(), "should start tool_use block");

        let pos_text_delta = pos_text_delta.unwrap();
        let pos_text_stop = pos_text_stop.unwrap();
        let pos_tool_start = pos_tool_start.unwrap();

        assert!(
            pos_text_delta < pos_text_stop && pos_text_stop < pos_tool_start,
            "ordering should be: text_delta -> text_stop -> tool_use_start"
        );

        assert!(
            events.iter().any(|e| {
                e.event == "content_block_delta"
                    && e.data["delta"]["type"] == "text_delta"
                    && e.data["delta"]["text"] == "<think"
            }),
            "flushed text should equal the buffered prefix"
        );
    }

    #[test]
    fn test_estimate_tokens() {
        assert!(estimate_tokens("Hello") > 0);
        assert!(estimate_tokens("你好") > 0);
        assert!(estimate_tokens("Hello 你好") > 0);
    }

    #[test]
    fn test_find_real_thinking_start_tag_basic() {
        // 基本情况：正常的开始标签
        assert_eq!(
            tag_start(find_real_thinking_start_tag("<thinking>")),
            Some(0)
        );
        assert_eq!(
            tag_start(find_real_thinking_start_tag("prefix<thinking>")),
            Some(6)
        );
        // 名字后紧跟别的字母 ⇒ 是另一个标签名，不得误判
        assert_eq!(find_real_thinking_start_tag("<thinkingfoo>"), None);
    }

    #[test]
    fn test_find_real_thinking_start_tag_with_backticks() {
        // 被反引号包裹的应该被跳过
        assert_eq!(find_real_thinking_start_tag("`<thinking>`"), None);
        assert_eq!(find_real_thinking_start_tag("use `<thinking>` tag"), None);

        // 先有被包裹的，后有真正的开始标签
        assert_eq!(
            tag_start(find_real_thinking_start_tag(
                "about `<thinking>` tag<thinking>content"
            )),
            Some(22)
        );
    }

    #[test]
    fn test_find_real_thinking_start_tag_with_quotes() {
        // 被双引号包裹的应该被跳过
        assert_eq!(find_real_thinking_start_tag("\"<thinking>\""), None);
        assert_eq!(find_real_thinking_start_tag("the \"<thinking>\" tag"), None);

        // 被单引号包裹的应该被跳过
        assert_eq!(find_real_thinking_start_tag("'<thinking>'"), None);

        // 混合情况
        assert_eq!(
            tag_start(find_real_thinking_start_tag(
                "about \"<thinking>\" and '<thinking>' then<thinking>"
            )),
            Some(40)
        );
    }

    #[test]
    fn test_find_real_thinking_end_tag_basic() {
        // 基本情况：正常的结束标签后面有双换行符
        assert_eq!(
            tag_start(find_real_thinking_end_tag("</thinking>\n\n")),
            Some(0)
        );
        assert_eq!(
            tag_start(find_real_thinking_end_tag("content</thinking>\n\n")),
            Some(7)
        );
        assert_eq!(
            tag_start(find_real_thinking_end_tag(
                "some text</thinking>\n\nmore text"
            )),
            Some(9)
        );

        // 没有双换行符的情况
        assert_eq!(find_real_thinking_end_tag("</thinking>"), None);
        assert_eq!(find_real_thinking_end_tag("</thinking>\n"), None);
        assert_eq!(find_real_thinking_end_tag("</thinking> more"), None);
    }

    /// ⭐ 回归：思考正文最后一个字符是 **ASCII 标点** 时，结束标签必须仍被识别。
    ///
    /// 旧 `QUOTE_CHARS` 有 30 个字符（含 `.` `,` `:` `;` `!` `?` `)` `]` `}` `%` …），
    /// 而文档注释只承诺 3 个引号。于是「思考以句号结尾」这种**散文常态**被判成
    /// 「标签被引用包裹」→ 跳过 → 找不到结束标签。
    ///
    /// 后果不是标签泄漏，而是**答案整段消失**：thinking 开启时正文被当思考内容发进
    /// thinking 面板（`has_non_thinking_blocks()=false` 还会把 stop_reason 伪造成
    /// `max_tokens`、只吐一个空格 text 块）；thinking 关闭时整段被丢弃，用户看到空回答。
    ///
    /// 把 `QUOTE_CHARS` 改回 30 个字符 → 本测试必 FAILED。
    #[test]
    fn end_tag_recognized_when_thinking_ends_with_ascii_punctuation() {
        for c in [
            '.', ',', ':', ';', '!', '?', ')', ']', '}', '%', '=', '-', '*', '&', '#',
        ] {
            let body = format!("reason{c}</thinking>\n\nANSWER");
            assert_eq!(
                tag_start(find_real_thinking_end_tag(&body)),
                Some("reason".len() + c.len_utf8()),
                "思考正文以 {c:?} 结尾时结束标签必须仍被识别（旧 QUOTE_CHARS 会漏判 → 答案消失）"
            );
        }
        // 对照：成对引号包裹的**引用**仍必须被跳过（收窄不等于放弃引用判定）
        assert_eq!(find_real_thinking_end_tag("`</thinking>`\n\n"), None);
        assert_eq!(find_real_thinking_end_tag("\"</thinking>\"\n\n"), None);
        assert_eq!(find_real_thinking_end_tag("'</thinking>'\n\n"), None);
    }

    /// ⭐ 回归：结束标签后**不是恰好 `\n\n`** 的形态也必须被识别。
    ///
    /// 旧判据只接受 `after_content.starts_with("\n\n")`，于是单换行 / 紧跟 `<` 全部漏判。
    /// 其中 `</thinking><invoke ...>` 是 Claude Code 的真实生产形态 ——
    /// 漏判会让整个工具调用连同答案一起被吞（见
    /// [`thinking_end_adjacent_invoke_must_still_reclaim_tool`]）。
    ///
    /// 把判据改回 `starts_with("\n\n")` → 本测试必 FAILED。
    #[test]
    fn end_tag_recognized_with_non_double_newline_suffix() {
        // 单换行 + 正文
        assert_eq!(
            tag_start(find_real_thinking_end_tag("reason\n</thinking>\nANSWER")),
            Some(7)
        );
        // 紧跟下一个标签（零空白）
        assert_eq!(
            tag_start(find_real_thinking_end_tag(
                "reason\n</thinking><invoke name=\"R\">"
            )),
            Some(7)
        );
        // 行内空白后接换行
        assert_eq!(
            tag_start(find_real_thinking_end_tag("reason\n</thinking> \n\nANSWER")),
            Some(7)
        );
        // 对照：纯行内空白（无换行）仍不认 —— 更像正文顺口提到标签
        assert_eq!(find_real_thinking_end_tag("</thinking> more"), None);
        // 对照：空白顶到缓冲末尾且未攒够 `\n\n` → 等下一个 chunk（防吃掉段落分隔的后半个）
        assert_eq!(find_real_thinking_end_tag("</thinking>"), None);
        assert_eq!(find_real_thinking_end_tag("</thinking>\n"), None);
    }

    /// ⭐ 回归：结束标签消耗长度必须按**实际后缀形态**算，不能写死 13 字节。
    ///
    /// 判据放宽后只有 `\n\n` 才恰好 13。写死会在其余形态下多切 2 字节：
    /// `</thinking>\nAnswer` 切掉 `\nA` → 客户端看到 `nswer`；
    /// `</thinking><invoke` 切掉 `<i` → 文本化工具调用永远无法重组。
    ///
    /// 把三处 `thinking_end_tag_consumed_len` 换回 `"</thinking>\n\n".len()` → 本测试必 FAILED。
    ///
    /// 同时钉住**标签本身也不定长**：长度取自匹配结果而非写死 11。若把
    /// `thinking_end_tag_consumed_len` 改回按 `"</thinking>".len()` 算，
    /// 下面 `</THINKING >` 那条（12 字节）会少切 1 字节 → 残留 `>` 泄漏 → FAILED。
    #[test]
    fn end_tag_consumed_len_matches_actual_suffix() {
        // 用**匹配结果**驱动，覆盖「匹配 → 算长度」全链，而非只喂写死的 0 位置。
        let consumed = |buf: &str| {
            let m = scan_thinking_tag(buf, 0, true).expect("应能匹配到闭标签");
            (thinking_end_tag_consumed_len(buf, &m), m.len)
        };
        const TAG: usize = "</thinking>".len();
        assert_eq!(consumed("</thinking>\n\nA"), (TAG + 2, TAG));
        assert_eq!(consumed("</thinking>\nA"), (TAG + 1, TAG));
        assert_eq!(consumed("</thinking><invoke"), (TAG, TAG));
        // 三个以上换行只吃掉段落分隔的两个，其余留给正文（保持既有行为）
        assert_eq!(consumed("</thinking>\n\n\nA"), (TAG + 2, TAG));
        // 大写：长度恰好相同（侥幸），但必须真的匹配上
        assert_eq!(consumed("</THINKING>\n\nA"), (TAG + 2, TAG));
        // 带空白的闭标签：**12 字节**，写死 11 会残留一个 `>`
        assert_eq!(consumed("</thinking >\n\nA"), (TAG + 1 + 2, TAG + 1));
    }

    /// ⭐ 回归（**走真实流路径**，不是只测纯函数）：单换行后缀时正文首字符不得被吃掉。
    ///
    /// 上面那条 `end_tag_consumed_len_matches_actual_suffix` 只断言辅助函数自身，
    /// 把三处**调用点**换回写死 13 它依然全绿 —— 那是纸面测试，挡不住回退。
    /// 本条从 `process_assistant_response` 入口喂真实文本、断言客户端看到的可见正文，
    /// 任一调用点写死 13 → `\n` + 正文首字节被一起切掉 → `ANSWER` 变 `NSWER` → FAILED。
    #[test]
    fn single_newline_suffix_must_not_eat_first_answer_char() {
        for thinking_enabled in [true, false] {
            let mut ctx = StreamContext::new_with_thinking(
                "claude-sonnet-5",
                1,
                thinking_enabled,
                HashMap::new(),
            );
            let mut ev = ctx.generate_initial_events();
            ev.extend(ctx.process_assistant_response("<thinking>\nreason\n</thinking>\nANSWER"));
            ev.extend(ctx.generate_final_events());
            let text: String = ev
                .iter()
                .filter(|e| e.event == "content_block_delta")
                .filter_map(|e| e.data["delta"]["text"].as_str())
                .collect();
            assert_eq!(
                text.trim(),
                "ANSWER",
                "thinking={thinking_enabled}：单换行后缀时正文必须完整（写死 13 会吃掉首字符）"
            );
        }
    }

    /// ⭐ 回归（最严重的复合缺陷）：`</thinking>` 紧跟文本化 `<invoke>` 时
    /// **工具必须仍被重组执行**。
    ///
    /// 旧代码下 4 种真实形态里有 3 种让工具**静默不执行**：结束标签漏判 →
    /// 整段（含 invoke 块）滞留 thinking_buffer → 被当思考内容发走，
    /// invoke 从未进入 sniff 缓冲。thinking 关闭时更是连文本都没有，
    /// 工具调用**毫无痕迹地消失** —— 面板上看不出任何异常。
    #[test]
    fn thinking_end_adjacent_invoke_must_still_reclaim_tool() {
        let shapes = [
            (
                "紧跟无换行",
                "<thinking>\nread it\n</thinking><invoke name=\"Read\"><parameter name=\"path\">/a</parameter></invoke>",
            ),
            (
                "单换行",
                "<thinking>\nread it\n</thinking>\n<invoke name=\"Read\"><parameter name=\"path\">/a</parameter></invoke>",
            ),
            (
                "双换行",
                "<thinking>\nread it\n</thinking>\n\n<invoke name=\"Read\"><parameter name=\"path\">/a</parameter></invoke>",
            ),
            (
                "句号紧贴标签",
                "<thinking>\nread it.</thinking>\n\n<invoke name=\"Read\"><parameter name=\"path\">/a</parameter></invoke>",
            ),
        ];
        for (name, full) in shapes {
            for thinking_enabled in [true, false] {
                let known: std::collections::HashSet<String> =
                    ["Read".to_string()].into_iter().collect();
                let mut ctx = StreamContext::new_full(
                    "claude-sonnet-5",
                    1,
                    thinking_enabled,
                    HashMap::new(),
                    known,
                );
                ctx.reclaim_enabled = true;
                let mut ev = ctx.generate_initial_events();
                ev.extend(ctx.process_assistant_response(full));
                ev.extend(ctx.generate_final_events());

                let tools: Vec<&str> = ev
                    .iter()
                    .filter(|e| e.event == "content_block_start")
                    .filter_map(|e| e.data["content_block"]["name"].as_str())
                    .collect();
                assert_eq!(
                    tools,
                    ["Read"],
                    "形态「{name}」thinking={thinking_enabled} 时工具必须被重组执行，实际 {tools:?}"
                );
                assert_eq!(
                    ctx.reclaimed_invoke_count, 1,
                    "形态「{name}」应恰好重组 1 次"
                );
            }
        }
    }

    #[test]
    fn test_find_real_thinking_end_tag_with_backticks() {
        // 被反引号包裹的应该被跳过
        assert_eq!(find_real_thinking_end_tag("`</thinking>`\n\n"), None);
        assert_eq!(
            find_real_thinking_end_tag("mention `</thinking>` in code\n\n"),
            None
        );

        // 只有前面有反引号
        assert_eq!(find_real_thinking_end_tag("`</thinking>\n\n"), None);

        // 只有后面有反引号
        assert_eq!(find_real_thinking_end_tag("</thinking>`\n\n"), None);
    }

    #[test]
    fn test_find_real_thinking_end_tag_with_quotes() {
        // 被双引号包裹的应该被跳过
        assert_eq!(find_real_thinking_end_tag("\"</thinking>\"\n\n"), None);
        assert_eq!(
            find_real_thinking_end_tag("the string \"</thinking>\" is a tag\n\n"),
            None
        );

        // 被单引号包裹的应该被跳过
        assert_eq!(find_real_thinking_end_tag("'</thinking>'\n\n"), None);
        assert_eq!(
            find_real_thinking_end_tag("use '</thinking>' as marker\n\n"),
            None
        );

        // 混合情况：双引号包裹后有真正的标签
        assert_eq!(
            tag_start(find_real_thinking_end_tag(
                "about \"</thinking>\" tag</thinking>\n\n"
            )),
            Some(23)
        );

        // 混合情况：单引号包裹后有真正的标签
        assert_eq!(
            tag_start(find_real_thinking_end_tag(
                "about '</thinking>' tag</thinking>\n\n"
            )),
            Some(23)
        );
    }

    #[test]
    fn test_find_real_thinking_end_tag_mixed() {
        // 先有被包裹的，后有真正的结束标签
        assert_eq!(
            tag_start(find_real_thinking_end_tag(
                "discussing `</thinking>` tag</thinking>\n\n"
            )),
            Some(28)
        );

        // 多个被包裹的，最后一个是真正的
        assert_eq!(
            tag_start(find_real_thinking_end_tag(
                "`</thinking>` and `</thinking>` done</thinking>\n\n"
            )),
            Some(36)
        );

        // 多种引用字符混合
        assert_eq!(
            tag_start(find_real_thinking_end_tag(
                "`</thinking>` and \"</thinking>\" and '</thinking>' done</thinking>\n\n"
            )),
            Some(54)
        );
    }

    #[test]
    fn test_tool_use_immediately_after_thinking_filters_end_tag_and_closes_thinking_block() {
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new());
        let _initial_events = ctx.generate_initial_events();

        let mut all_events = Vec::new();

        // thinking 内容以 `</thinking>` 结尾，但后面没有 `\n\n`（模拟紧跟 tool_use 的场景）
        all_events.extend(ctx.process_assistant_response("<thinking>abc</thinking>"));

        let tool_events = ctx.process_tool_use(&crate::kiro::model::events::ToolUseEvent {
            name: "Write".to_string(),
            tool_use_id: "tool_1".to_string(),
            input: "{}".to_string(),
            stop: false,
        });
        all_events.extend(tool_events);

        all_events.extend(ctx.generate_final_events());

        // 不应把 `</thinking>` 当作 thinking 内容输出
        assert!(
            all_events.iter().all(|e| {
                !(e.event == "content_block_delta"
                    && e.data["delta"]["type"] == "thinking_delta"
                    && e.data["delta"]["thinking"] == "</thinking>")
            }),
            "`</thinking>` should be filtered from output"
        );

        // thinking block 必须在 tool_use block 之前关闭
        let thinking_index = ctx
            .thinking_block_index
            .expect("thinking block index should exist");
        let pos_thinking_stop = all_events.iter().position(|e| {
            e.event == "content_block_stop"
                && e.data["index"].as_i64() == Some(thinking_index as i64)
        });
        let pos_tool_start = all_events.iter().position(|e| {
            e.event == "content_block_start" && e.data["content_block"]["type"] == "tool_use"
        });
        assert!(
            pos_thinking_stop.is_some(),
            "thinking block should be stopped"
        );
        assert!(pos_tool_start.is_some(), "tool_use block should be started");
        assert!(
            pos_thinking_stop.unwrap() < pos_tool_start.unwrap(),
            "thinking block should stop before tool_use block starts"
        );
    }

    #[test]
    fn test_final_flush_filters_standalone_thinking_end_tag() {
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new());
        let _initial_events = ctx.generate_initial_events();

        let mut all_events = Vec::new();
        all_events.extend(ctx.process_assistant_response("<thinking>abc</thinking>"));
        all_events.extend(ctx.generate_final_events());

        assert!(
            all_events.iter().all(|e| {
                !(e.event == "content_block_delta"
                    && e.data["delta"]["type"] == "thinking_delta"
                    && e.data["delta"]["thinking"] == "</thinking>")
            }),
            "`</thinking>` should be filtered during final flush"
        );
    }

    #[test]
    fn test_thinking_strips_leading_newline_same_chunk() {
        // <thinking>\n 在同一个 chunk 中，\n 应被剥离
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new());
        let _initial_events = ctx.generate_initial_events();

        let events = ctx.process_assistant_response("<thinking>\nHello world");

        // 找到所有 thinking_delta 事件
        let thinking_deltas: Vec<_> = events
            .iter()
            .filter(|e| {
                e.event == "content_block_delta" && e.data["delta"]["type"] == "thinking_delta"
            })
            .collect();

        // 拼接所有 thinking 内容
        let full_thinking: String = thinking_deltas
            .iter()
            .map(|e| e.data["delta"]["thinking"].as_str().unwrap_or(""))
            .collect();

        assert!(
            !full_thinking.starts_with('\n'),
            "thinking content should not start with \\n, got: {:?}",
            full_thinking
        );
    }

    #[test]
    fn test_thinking_strips_leading_newline_cross_chunk() {
        // <thinking> 在第一个 chunk 末尾，\n 在第二个 chunk 开头
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new());
        let _initial_events = ctx.generate_initial_events();

        let events1 = ctx.process_assistant_response("<thinking>");
        let events2 = ctx.process_assistant_response("\nHello world");

        let mut all_events = Vec::new();
        all_events.extend(events1);
        all_events.extend(events2);

        let thinking_deltas: Vec<_> = all_events
            .iter()
            .filter(|e| {
                e.event == "content_block_delta" && e.data["delta"]["type"] == "thinking_delta"
            })
            .collect();

        let full_thinking: String = thinking_deltas
            .iter()
            .map(|e| e.data["delta"]["thinking"].as_str().unwrap_or(""))
            .collect();

        assert!(
            !full_thinking.starts_with('\n'),
            "thinking content should not start with \\n across chunks, got: {:?}",
            full_thinking
        );
    }

    #[test]
    fn test_thinking_no_strip_when_no_leading_newline() {
        // <thinking> 后直接跟内容（无 \n），内容应完整保留
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new());
        let _initial_events = ctx.generate_initial_events();

        let events = ctx.process_assistant_response("<thinking>abc</thinking>\n\ntext");

        let thinking_deltas: Vec<_> = events
            .iter()
            .filter(|e| {
                e.event == "content_block_delta" && e.data["delta"]["type"] == "thinking_delta"
            })
            .collect();

        let full_thinking: String = thinking_deltas
            .iter()
            .filter(|e| {
                !e.data["delta"]["thinking"]
                    .as_str()
                    .unwrap_or("")
                    .is_empty()
            })
            .map(|e| e.data["delta"]["thinking"].as_str().unwrap_or(""))
            .collect();

        assert_eq!(full_thinking, "abc", "thinking content should be 'abc'");
    }

    #[test]
    fn test_text_after_thinking_strips_leading_newlines() {
        // `</thinking>\n\n` 后的文本不应以 \n\n 开头
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new());
        let _initial_events = ctx.generate_initial_events();

        let events = ctx.process_assistant_response("<thinking>\nabc</thinking>\n\n你好");

        let text_deltas: Vec<_> = events
            .iter()
            .filter(|e| e.event == "content_block_delta" && e.data["delta"]["type"] == "text_delta")
            .collect();

        let full_text: String = text_deltas
            .iter()
            .map(|e| e.data["delta"]["text"].as_str().unwrap_or(""))
            .collect();

        assert!(
            !full_text.starts_with('\n'),
            "text after thinking should not start with \\n, got: {:?}",
            full_text
        );
        assert_eq!(full_text, "你好");
    }

    /// 辅助函数：从事件列表中提取所有 thinking_delta 的拼接内容
    fn collect_thinking_content(events: &[SseEvent]) -> String {
        events
            .iter()
            .filter(|e| {
                e.event == "content_block_delta" && e.data["delta"]["type"] == "thinking_delta"
            })
            .map(|e| e.data["delta"]["thinking"].as_str().unwrap_or(""))
            .filter(|s| !s.is_empty())
            .collect()
    }

    /// 辅助函数：从事件列表中提取所有 text_delta 的拼接内容
    fn collect_text_content(events: &[SseEvent]) -> String {
        events
            .iter()
            .filter(|e| e.event == "content_block_delta" && e.data["delta"]["type"] == "text_delta")
            .map(|e| e.data["delta"]["text"].as_str().unwrap_or(""))
            .collect()
    }

    #[test]
    fn test_end_tag_newlines_split_across_events() {
        // `</thinking>\n` 在 chunk 1，`\n` 在 chunk 2，`text` 在 chunk 3
        // 确保 `</thinking>` 不会被部分当作 thinking 内容发出
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new());
        let _initial_events = ctx.generate_initial_events();

        let mut all = Vec::new();
        all.extend(ctx.process_assistant_response("<thinking>\nabc</thinking>\n"));
        all.extend(ctx.process_assistant_response("\n"));
        all.extend(ctx.process_assistant_response("你好"));
        all.extend(ctx.generate_final_events());

        let thinking = collect_thinking_content(&all);
        assert_eq!(
            thinking, "abc",
            "thinking should be 'abc', got: {:?}",
            thinking
        );

        let text = collect_text_content(&all);
        assert_eq!(text, "你好", "text should be '你好', got: {:?}", text);
    }

    #[test]
    fn test_end_tag_alone_in_chunk_then_newlines_in_next() {
        // `</thinking>` 单独在一个 chunk，`\n\ntext` 在下一个 chunk
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new());
        let _initial_events = ctx.generate_initial_events();

        let mut all = Vec::new();
        all.extend(ctx.process_assistant_response("<thinking>\nabc</thinking>"));
        all.extend(ctx.process_assistant_response("\n\n你好"));
        all.extend(ctx.generate_final_events());

        let thinking = collect_thinking_content(&all);
        assert_eq!(
            thinking, "abc",
            "thinking should be 'abc', got: {:?}",
            thinking
        );

        let text = collect_text_content(&all);
        assert_eq!(text, "你好", "text should be '你好', got: {:?}", text);
    }

    #[test]
    fn test_start_tag_newline_split_across_events() {
        // `\n\n` 在 chunk 1，`<thinking>` 在 chunk 2，`\n` 在 chunk 3
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new());
        let _initial_events = ctx.generate_initial_events();

        let mut all = Vec::new();
        all.extend(ctx.process_assistant_response("\n\n"));
        all.extend(ctx.process_assistant_response("<thinking>"));
        all.extend(ctx.process_assistant_response("\n"));
        all.extend(ctx.process_assistant_response("abc</thinking>\n\ntext"));
        all.extend(ctx.generate_final_events());

        let thinking = collect_thinking_content(&all);
        assert_eq!(
            thinking, "abc",
            "thinking should be 'abc', got: {:?}",
            thinking
        );

        let text = collect_text_content(&all);
        assert_eq!(text, "text", "text should be 'text', got: {:?}", text);
    }

    #[test]
    fn test_full_flow_maximally_split() {
        // 极端拆分：每个关键边界都在不同 chunk
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new());
        let _initial_events = ctx.generate_initial_events();

        let mut all = Vec::new();
        // \n\n<thinking>\n 拆成多段
        all.extend(ctx.process_assistant_response("\n"));
        all.extend(ctx.process_assistant_response("\n"));
        all.extend(ctx.process_assistant_response("<thin"));
        all.extend(ctx.process_assistant_response("king>"));
        all.extend(ctx.process_assistant_response("\n"));
        all.extend(ctx.process_assistant_response("hello"));
        // </thinking>\n\n 拆成多段
        all.extend(ctx.process_assistant_response("</thi"));
        all.extend(ctx.process_assistant_response("nking>"));
        all.extend(ctx.process_assistant_response("\n"));
        all.extend(ctx.process_assistant_response("\n"));
        all.extend(ctx.process_assistant_response("world"));
        all.extend(ctx.generate_final_events());

        let thinking = collect_thinking_content(&all);
        assert_eq!(
            thinking, "hello",
            "thinking should be 'hello', got: {:?}",
            thinking
        );

        let text = collect_text_content(&all);
        assert_eq!(text, "world", "text should be 'world', got: {:?}", text);
    }

    #[test]
    fn test_thinking_only_sets_max_tokens_stop_reason() {
        // 整个流只有 thinking 块，没有 text 也没有 tool_use，stop_reason 应为 max_tokens
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new());
        let _initial_events = ctx.generate_initial_events();

        let mut all_events = Vec::new();
        all_events.extend(ctx.process_assistant_response("<thinking>\nabc</thinking>"));
        all_events.extend(ctx.generate_final_events());

        let message_delta = all_events
            .iter()
            .find(|e| e.event == "message_delta")
            .expect("should have message_delta event");

        assert_eq!(
            message_delta.data["delta"]["stop_reason"], "max_tokens",
            "stop_reason should be max_tokens when only thinking is produced"
        );

        // 应补发一套完整的 text 事件（content_block_start + delta 空格 + content_block_stop）
        assert!(
            all_events.iter().any(|e| {
                e.event == "content_block_start" && e.data["content_block"]["type"] == "text"
            }),
            "should emit text content_block_start"
        );
        assert!(
            all_events.iter().any(|e| {
                e.event == "content_block_delta"
                    && e.data["delta"]["type"] == "text_delta"
                    && e.data["delta"]["text"] == " "
            }),
            "should emit text_delta with a single space"
        );
        // text block 应被 generate_final_events 自动关闭
        let text_block_index = all_events
            .iter()
            .find_map(|e| {
                if e.event == "content_block_start" && e.data["content_block"]["type"] == "text" {
                    e.data["index"].as_i64()
                } else {
                    None
                }
            })
            .expect("text block should exist");
        assert!(
            all_events.iter().any(|e| {
                e.event == "content_block_stop"
                    && e.data["index"].as_i64() == Some(text_block_index)
            }),
            "text block should be stopped"
        );
    }

    #[test]
    fn test_thinking_with_text_keeps_end_turn_stop_reason() {
        // thinking + text + 上游 metadata 明确 end_turn → 成功收尾
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new());
        let _initial_events = ctx.generate_initial_events();

        let mut all_events = Vec::new();
        all_events.extend(ctx.process_assistant_response("<thinking>\nabc</thinking>\n\nHello"));
        all_events.extend(ctx.process_kiro_event(&Event::Metadata(
            crate::kiro::model::events::MetadataEvent {
                stop_reason: Some("end_turn".into()),
            },
        )));
        all_events.extend(ctx.generate_final_events());

        assert!(ctx.completion().is_ok(), "有终止信号的一轮必须是成功态");
        let message_delta = all_events
            .iter()
            .find(|e| e.event == "message_delta")
            .expect("should have message_delta event");

        assert_eq!(
            message_delta.data["delta"]["stop_reason"], "end_turn",
            "stop_reason should be end_turn when text is also produced"
        );
    }

    #[test]
    fn metadata_event_stop_reason_max_tokens_surfaces() {
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, false, HashMap::new());
        let _ = ctx.generate_initial_events();
        ctx.process_kiro_event(&Event::AssistantResponse(
            crate::kiro::model::events::AssistantResponseEvent {
                content: "partial answer that hit the cap".into(),
            },
        ));
        ctx.process_kiro_event(&Event::Metadata(
            crate::kiro::model::events::MetadataEvent {
                stop_reason: Some("max_tokens".into()),
            },
        ));
        let finals = ctx.generate_final_events();
        assert!(
            ctx.completion().is_ok(),
            "metadata 给出 max_tokens 是干净收尾，不是失败"
        );
        let message_delta = finals
            .iter()
            .find(|e| e.event == "message_delta")
            .expect("应有 message_delta");
        assert_eq!(
            message_delta.data["delta"]["stop_reason"], "max_tokens",
            "上游 metadata.stopReason=max_tokens 必须露出，不得推断成 end_turn"
        );
        assert!(
            !finals.iter().any(|e| e.event == "error"),
            "干净 max_tokens 不得发 SSE error"
        );
    }

    #[test]
    fn clean_eof_partial_text_without_stop_is_not_success_end_turn() {
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, false, HashMap::new());
        let _ = ctx.generate_initial_events();
        ctx.process_kiro_event(&Event::AssistantResponse(
            crate::kiro::model::events::AssistantResponseEvent {
                content: "this answer was cut off mid-".into(),
            },
        ));
        let finals = ctx.generate_final_events();
        assert!(
            !ctx.completion().is_ok(),
            "干净 EOF + 部分正文 + 无终止信号不得记成功"
        );
        assert!(
            !finals.iter().any(|e| {
                e.event == "message_delta" && e.data["delta"]["stop_reason"] == "end_turn"
            }),
            "不得下发成功 end_turn。实际: {:?}",
            finals.iter().map(|e| &e.event).collect::<Vec<_>>()
        );
        assert!(
            finals.iter().any(|e| e.event == "error"),
            "应发 SSE error 让客户端重试，而不是当成功 turn"
        );
        match ctx.completion() {
            CompletionStatus::Incomplete { .. } => {}
            other => panic!("期望 Incomplete，实际 {other:?}"),
        }
    }

    #[test]
    fn tool_stop_true_without_metadata_is_complete_tool_use() {
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, false, HashMap::new());
        let _ = ctx.generate_initial_events();
        ctx.process_kiro_event(&Event::ToolUse(
            crate::kiro::model::events::ToolUseEvent {
                name: "Read".into(),
                tool_use_id: "toolu_1".into(),
                input: r#"{"path":"/a"}"#.into(),
                stop: true,
            },
        ));
        let finals = ctx.generate_final_events();
        assert!(ctx.completion().is_ok(), "tool_use stop=true 即终止，无需 metadata");
        let message_delta = finals
            .iter()
            .find(|e| e.event == "message_delta")
            .expect("应有 message_delta");
        assert_eq!(
            message_delta.data["delta"]["stop_reason"], "tool_use",
            "有 stop:true 的 tool 帧即使没有 metadata 也是完整 tool_use"
        );
    }

    #[test]
    fn stream_must_consume_metadata_and_reject_clean_eof_without_stop() {
        let src = include_str!("stream.rs");
        let prod = src
            .split_once("\n#[cfg(test)]")
            .map(|(p, _)| p)
            .unwrap_or(src);
        let prod: String = prod
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        let meta_arm = format!("{}::{}", "Event", "Metadata");
        let incomplete = format!("{}::{}", "CompletionStatus", "Incomplete");
        let eof_fn = format!("{}{}", "is_clean_eof_without", "_terminal");
        assert!(
            prod.contains(&meta_arm),
            "process 路径必须消费 Metadata 帧（不能再 _ => 丢掉 stopReason）"
        );
        assert!(
            prod.contains(&incomplete),
            "干净 EOF 无终止必须落非 Ok 完成态"
        );
        assert!(
            prod.contains(&eof_fn),
            "必须有干净 EOF 终止信号检查"
        );
    }

    #[test]
    fn test_thinking_with_tool_use_keeps_tool_use_stop_reason() {
        // thinking + tool_use 的情况，stop_reason 应为 tool_use
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new());
        let _initial_events = ctx.generate_initial_events();

        let mut all_events = Vec::new();
        all_events.extend(ctx.process_assistant_response("<thinking>\nabc</thinking>"));
        all_events.extend(
            ctx.process_tool_use(&crate::kiro::model::events::ToolUseEvent {
                name: "test_tool".to_string(),
                tool_use_id: "tool_1".to_string(),
                input: "{}".to_string(),
                stop: true,
            }),
        );
        all_events.extend(ctx.generate_final_events());

        let message_delta = all_events
            .iter()
            .find(|e| e.event == "message_delta")
            .expect("should have message_delta event");

        assert_eq!(
            message_delta.data["delta"]["stop_reason"], "tool_use",
            "stop_reason should be tool_use when tool_use is present"
        );
    }

    /// B3 回归：流式 thinking 块结束前必须发一个非空 signature_delta，且排在
    /// content_block_stop 之前。否则客户端下一轮回传时本地校验失败报错
    /// "The content[].thinking in the thinking mode must be passed back"。
    #[test]
    fn test_thinking_block_emits_signature_delta_before_stop() {
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new());
        let _initial_events = ctx.generate_initial_events();

        let mut all_events = Vec::new();
        all_events.extend(ctx.process_assistant_response("<thinking>\nabc</thinking>\n\nHello"));
        all_events.extend(ctx.generate_final_events());

        let thinking_index = ctx
            .thinking_block_index
            .expect("thinking block index should exist");

        // 存在一个非空 signature_delta，index 指向 thinking 块
        let pos_sig = all_events.iter().position(|e| {
            e.event == "content_block_delta"
                && e.data["delta"]["type"] == "signature_delta"
                && e.data["delta"]["signature"]
                    .as_str()
                    .map(|s| !s.is_empty())
                    .unwrap_or(false)
                && e.data["index"].as_i64() == Some(thinking_index as i64)
        });
        assert!(pos_sig.is_some(), "应发出非空 signature_delta");

        // signature_delta 必须排在 thinking 块的 content_block_stop 之前
        let pos_stop = all_events.iter().position(|e| {
            e.event == "content_block_stop"
                && e.data["index"].as_i64() == Some(thinking_index as i64)
        });
        assert!(pos_stop.is_some(), "thinking 块应被关闭");
        assert!(
            pos_sig.unwrap() < pos_stop.unwrap(),
            "signature_delta 必须排在 content_block_stop 之前"
        );
    }

    /// ⭐ 回归（P3-1）：`reasoningContentEvent` 带真签名 → `signature_delta` 必须用**上游真签名**，
    /// 而不是占位符。Foxfishc 实测「伪造签名不被上游识别，cache_read 仍 0」——真签名是
    /// 多轮 cache 命中的关键。把 `create_signature_delta_event` 里的
    /// `pending_reasoning_signature.take()` 改回恒占位符 → 本测试 FAILED。
    #[test]
    fn reasoning_signature_is_forwarded_to_client_instead_of_placeholder() {
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new());
        let _initial_events = ctx.generate_initial_events();

        let ev = Event::ReasoningContent(crate::kiro::model::events::ReasoningContentEvent {
            text: "思考过程".to_string(),
            signature: Some("upstream-real-signature-abc123".to_string()),
            ..Default::default()
        });
        ctx.process_kiro_event(&ev);
        let all_events = ctx.generate_final_events();

        let sig = all_events
            .iter()
            .find(|e| e.data["delta"]["type"] == "signature_delta")
            .expect("应发出 signature_delta")
            .data["delta"]["signature"]
            .as_str()
            .expect("signature 应为字符串");
        assert_eq!(
            sig, "upstream-real-signature-abc123",
            "必须回传上游真签名（否则多轮 cache 永不命中）"
        );
    }

    /// 对照：上游 reasoningContentEvent 不带签名 → 回退占位符，行为与改动前逐字节一致。
    #[test]
    fn reasoning_without_signature_falls_back_to_placeholder() {
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new());
        let _initial_events = ctx.generate_initial_events();

        let ev = Event::ReasoningContent(crate::kiro::model::events::ReasoningContentEvent {
            text: "思考".to_string(),
            ..Default::default()
        });
        ctx.process_kiro_event(&ev);
        let all_events = ctx.generate_final_events();

        let sig = all_events
            .iter()
            .find(|e| e.data["delta"]["type"] == "signature_delta")
            .expect("应发出 signature_delta")
            .data["delta"]["signature"]
            .as_str()
            .expect("signature 应为字符串");
        assert_eq!(
            sig, THINKING_SIGNATURE_PLACEHOLDER,
            "无上游签名应回退占位符"
        );
    }

    /// ⭐ 回归（P3-2）：thinking 开启 + 结构化 reasoning 开块 + 直接 tool_use（无正文）——
    /// thinking 块必须**先 stop 再 start tool_use 块**（Anthropic SSE 块顺序契约）。
    ///
    /// 旧代码缺这条：`process_tool_use` 只按 sniff 路径（`thinking_buffer` 里有
    /// `</thinking>`）关块，而 reasoning 开的块内容直接以 thinking_delta 下发、buffer 恒空，
    /// 于是 tool_use 块 start 时 thinking 块仍未 stop → SSE 出现「新块 start → 旧块 stop」
    /// 交错（工具块 index 1 先于思考块 index 0 收尾），CC 解析报错。把
    /// `close_reasoning_thinking_block` 接进 `process_tool_use` 的 reasoning 分支 → 本测试 FAILED。
    #[test]
    fn reasoning_opened_thinking_block_closed_before_tool_use() {
        let mut ctx =
            StreamContext::new_with_thinking("claude-sonnet-4.5", 10, true, HashMap::new());
        let mut all = ctx.generate_initial_events();
        all.extend(ctx.process_kiro_event(&reasoning_ev("思考如何调用工具")));
        all.extend(ctx.process_kiro_event(&Event::ToolUse(
            crate::kiro::model::events::ToolUseEvent {
                name: "Bash".into(),
                tool_use_id: "toolu_1".into(),
                input: "{\"command\":\"ls\"}".into(),
                stop: true,
            },
        )));
        all.extend(ctx.generate_final_events());

        // 思考块 stop（index 0）必须出现在工具块 start（index 1）之前。
        let thinking_stop_pos = all
            .iter()
            .position(|e| e.event == "content_block_stop" && e.data["index"].as_i64() == Some(0))
            .expect("thinking 块必须收尾");
        let tool_start_pos = all
            .iter()
            .position(|e| e.event == "content_block_start" && e.data["index"].as_i64() == Some(1))
            .expect("tool_use 块必须存在");
        assert!(
            thinking_stop_pos < tool_start_pos,
            "thinking 块必须在 tool_use 块 start 之前 stop（SSE 块顺序契约）"
        );
    }

    // ============ 「截断即成功」修复：CompletionStatus 回归 ============

    #[test]
    fn test_completion_status_outcome_and_http_mapping() {
        // 上游限流类错误 → RateLimited / overloaded_error / 429
        let rl = CompletionStatus::UpstreamError {
            code: "ThrottlingException".to_string(),
            message: "rate exceeded".to_string(),
        };
        assert_eq!(rl.outcome(), RequestOutcome::RateLimited);
        assert_eq!(rl.sse_error_type(), "overloaded_error");
        assert_eq!(rl.http_status_u16(), 429);

        // 普通上游错误 → ServerError / api_error / 502
        let se = CompletionStatus::UpstreamError {
            code: "InternalServerException".to_string(),
            message: "boom".to_string(),
        };
        assert_eq!(se.outcome(), RequestOutcome::ServerError);
        assert_eq!(se.sse_error_type(), "api_error");
        assert_eq!(se.http_status_u16(), 502);

        // 传输中断 → NetworkError / 502
        let te = CompletionStatus::TransportError {
            message: "connection reset".to_string(),
        };
        assert_eq!(te.outcome(), RequestOutcome::NetworkError);
        assert_eq!(te.http_status_u16(), 502);

        // 解码器停止 → ServerError / 502
        let ds = CompletionStatus::DecoderStopped {
            message: "too many errors".to_string(),
        };
        assert_eq!(ds.outcome(), RequestOutcome::ServerError);
        assert_eq!(ds.http_status_u16(), 502);

        let inc = CompletionStatus::Incomplete {
            message: "no terminal".to_string(),
        };
        assert_eq!(inc.outcome(), RequestOutcome::ServerError);
        assert_eq!(inc.sse_error_type(), "api_error");
        assert_eq!(inc.http_status_u16(), 502);
        assert!(!inc.is_ok());

        // Ok → Success
        assert_eq!(CompletionStatus::Ok.outcome(), RequestOutcome::Success);
        assert!(CompletionStatus::Ok.is_ok());
    }

    /// 🔴 **Bug C**：工具参数 JSON 合法但缺 `required` 字段 → 判定缺失（2026-08-10 新增）。
    ///
    /// 现实症状：`Bash failed: The required parameter 'command' is missing` ——
    /// 模型吐的 input 是 `{"description":"..."}`，**JSON 完全合法**，只是漏了 `command`。
    /// 既有的 `tool_repair_json`（修 JSON 语法）与 `tool_truncation_recovery`（修截断）
    /// 都碰不到它，此前落在 Bug A/B 之间的盲区。
    ///
    /// 本测试直接测判据函数（`find_missing_required_fields`），覆盖「该判缺」与
    /// 四类「不该干预」的边界。
    #[test]
    fn bug_c_detects_missing_required_tool_fields() {
        let mut ctx =
            StreamContext::new_with_thinking("claude-sonnet-4-5", 10, false, HashMap::new());
        // 模拟：Bash 工具的 required = ["command"]，block 3 是它。
        let mut req = HashMap::new();
        req.insert("Bash".to_string(), vec!["command".to_string()]);
        ctx.set_tool_required_fields(req);
        ctx.tool_block_names.insert(3, "Bash".to_string());

        // ① 缺 command → 判缺（这是真实故障形态）
        assert_eq!(
            ctx.find_missing_required_fields(3, r#"{"description":"list files"}"#),
            Some(vec!["command".to_string()]),
            "缺必需字段必须被判出来，否则客户端会报 The required parameter is missing"
        );

        // ② 齐全 → 不干预
        assert_eq!(
            ctx.find_missing_required_fields(3, r#"{"command":"ls","description":"x"}"#),
            None,
            "必需字段齐全时不得干预"
        );

        // ③ 显式 null 视为**存在**（客户端把 null 当"给了但为空"，与"没给"语义不同）
        assert_eq!(
            ctx.find_missing_required_fields(3, r#"{"command":null}"#),
            None,
            "显式 null 是「给了」，不算缺失 —— 只判键存在性，不判值"
        );

        // ④ 非工具块（无 block→name 记录）→ 不干预
        assert_eq!(
            ctx.find_missing_required_fields(99, r#"{"description":"x"}"#),
            None,
            "非工具块没有参数可校验"
        );

        // ⑤ 顶层不是 object（数组/标量）→ 不干预：required 描述的是顶层属性
        assert_eq!(
            ctx.find_missing_required_fields(3, r#"["a","b"]"#),
            None,
            "顶层非 object 不在本判据范围"
        );

        // ⑥ 非法 JSON → 不干预（那是 Bug A 的地盘，已由 repair 层处理）
        assert_eq!(
            ctx.find_missing_required_fields(3, r#"{"command":"#),
            None,
            "非法 JSON 归 Bug A 修复层，本判据不插手"
        );

        // ⑦ 工具无必需参数（未入表）→ 不干预（含 WebSearch 类无 input_schema 的工具）
        ctx.tool_block_names.insert(4, "WebSearch".to_string());
        assert_eq!(
            ctx.find_missing_required_fields(4, r#"{}"#),
            None,
            "没有 required 声明的工具不校验"
        );
    }

    #[test]
    fn test_inband_error_event_sets_failure_and_emits_error_event() {
        // 回归 BUG①/③：in-band Event::Error 应内联发 SSE error 事件，并把 completion 置失败态，
        // 使收尾 outcome 不再是硬编码的 Success。
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, false, HashMap::new());
        let _ = ctx.generate_initial_events();

        let events = ctx.process_kiro_event(&Event::Error {
            error_code: "InternalServerException".to_string(),
            error_message: "upstream boom".to_string(),
        });

        // 内联发出了 error 事件
        assert!(
            events
                .iter()
                .any(|e| e.event == "error" && e.data["error"]["type"] == "api_error"),
            "in-band 错误应内联发出 SSE error 事件"
        );
        assert!(ctx.error_event_emitted(), "应标记已发 error 事件");
        // completion 置为失败态，outcome 不再是 Success
        assert!(!ctx.completion().is_ok());
        assert_eq!(ctx.completion_outcome(), RequestOutcome::ServerError);
    }

    #[test]
    fn test_content_length_exceeded_is_not_failure() {
        // 铁律：ContentLengthExceededException = max_tokens 干净收尾，绝不算失败。
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, false, HashMap::new());
        let _ = ctx.generate_initial_events();

        let events = ctx.process_kiro_event(&Event::Exception {
            exception_type: "ContentLengthExceededException".to_string(),
            message: "max tokens".to_string(),
        });

        assert!(events.is_empty(), "CL 异常不应发 error 事件");
        assert!(ctx.completion().is_ok(), "CL 异常不应置失败态");
        assert_eq!(ctx.completion_outcome(), RequestOutcome::Success);
        assert!(!ctx.error_event_emitted());
    }

    #[test]
    fn test_non_cl_exception_marks_failure_and_emits_error() {
        // 非 CL 异常是上游真实失败：置失败态 + 内联发 error 事件。
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, false, HashMap::new());
        let _ = ctx.generate_initial_events();

        let events = ctx.process_kiro_event(&Event::Exception {
            exception_type: "ThrottlingException".to_string(),
            message: "slow down".to_string(),
        });

        assert!(
            events
                .iter()
                .any(|e| e.event == "error" && e.data["error"]["type"] == "overloaded_error"),
            "限流类异常应发 overloaded_error"
        );
        assert_eq!(ctx.completion_outcome(), RequestOutcome::RateLimited);
    }

    #[test]
    fn test_mark_transport_and_decoder_stopped_are_idempotent() {
        // 传输中断 / 解码器停止的 setter 应置失败态，且幂等保留首因。
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, false, HashMap::new());
        let _ = ctx.generate_initial_events();

        ctx.mark_transport_error("reset");
        assert_eq!(ctx.completion_outcome(), RequestOutcome::NetworkError);
        // 首因已定，后续 mark 不覆盖
        ctx.mark_decoder_stopped("later error");
        assert_eq!(
            ctx.completion_outcome(),
            RequestOutcome::NetworkError,
            "幂等：应保留首个失败原因"
        );
    }

    #[test]
    fn test_interrupted_bytes_only_when_broken() {
        // 中断字节：正常收尾 None；断流（mark_transport_error / in-band 错误）时
        // 回报已累计的上游字节；断流前一个字节都没收到 → Some(0)。
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, false, HashMap::new());
        let _ = ctx.generate_initial_events();
        assert_eq!(ctx.interrupted_bytes(), None, "正常进行中不应有中断字节");

        ctx.note_received_bytes(100);
        ctx.note_received_bytes(50);
        ctx.mark_transport_error("connection reset");
        assert_eq!(
            ctx.interrupted_bytes(),
            Some(150),
            "断流时应返回已收字节之和"
        );

        // 另一实例：in-band 错误同样算中断，且首帧前断流为 Some(0)
        let mut ctx2 = StreamContext::new_with_thinking("test-model", 1, false, HashMap::new());
        let _ = ctx2.generate_initial_events();
        ctx2.process_kiro_event(&Event::Error {
            error_code: "InternalServerException".to_string(),
            error_message: "boom".to_string(),
        });
        assert_eq!(ctx2.interrupted_bytes(), Some(0), "零字节断流应记 0");
    }

    #[test]
    fn test_buffered_context_delegates_completion() {
        // BufferedStreamContext 应把完成状态透传给内部 StreamContext。
        let mut ctx = BufferedStreamContext::new(
            "test-model",
            1,
            false,
            HashMap::new(),
            std::collections::HashSet::new(),
        );
        ctx.process_and_buffer(&Event::Error {
            error_code: "InternalServerException".to_string(),
            error_message: "boom".to_string(),
        });
        assert!(!ctx.completion().is_ok());
        assert_eq!(ctx.completion_outcome(), RequestOutcome::ServerError);
        assert!(ctx.error_event_emitted());
    }

    #[test]
    fn test_buffered_context_bounds_memory_on_flood() {
        // C4 回归:上游持续推送超长文本时,缓冲事件累计字节超上限应触发截断守卫——
        // 停止继续缓冲(event_buffer 不再无界增长)并置失败态。旧实现无上限会一直吃内存 OOM。
        use crate::kiro::model::events::AssistantResponseEvent;
        let mut ctx = BufferedStreamContext::new(
            "test-model",
            1,
            false,
            HashMap::new(),
            std::collections::HashSet::new(),
        );

        // 每帧约 1MiB 文本;喂 200 帧(约 200MiB) 远超 64MiB 上限。
        // 用变化的自然文本(非单字符/单词连写),避免触发 stray 复读熔断(那会吞掉内容)。
        let sentence = "The quick brown fox jumps over the lazy dog while parsing tokens. ";
        let chunk = sentence.repeat((1024 * 1024) / sentence.len() + 1);
        for _ in 0..200 {
            ctx.process_and_buffer(&Event::AssistantResponse(AssistantResponseEvent {
                content: chunk.clone(),
            }));
            if ctx.buffer_overflowed() {
                break;
            }
        }

        assert!(ctx.buffer_overflowed(), "超长响应应触发缓冲上限守卫");
        // 缓冲不应无界增长:累计字节应止于略超上限(不是 200MiB 全吃进来)。
        assert!(
            ctx.buffered_bytes <= MAX_BUFFERED_EVENT_BYTES + 4 * 1024 * 1024,
            "超限后应停止缓冲,不再继续增长(实际 {} 字节)",
            ctx.buffered_bytes
        );
        // 截断按失败态处置(收尾会发 SSE error,不把半截当成功)。
        assert!(!ctx.completion().is_ok(), "缓冲溢出应置失败态");

        // 溢出后继续喂事件应被丢弃(early-return),字节数不再变。
        let before = ctx.buffered_bytes;
        ctx.process_and_buffer(&Event::AssistantResponse(AssistantResponseEvent {
            content: chunk.clone(),
        }));
        assert_eq!(ctx.buffered_bytes, before, "溢出后应丢弃后续事件,不再缓冲");
    }

    // ==================== merge_tool_input 决策表回归（Invalid tool parameters 类型 C 根治） ====================

    /// 累积快照三帧：每帧是"到目前为止的完整 JSON" → 最终取最后最全的一帧。
    #[test]
    fn test_merge_cumulative_snapshots() {
        let mut buf = String::new();
        buf = merge_tool_input(&buf, r#"{"path""#);
        buf = merge_tool_input(&buf, r#"{"path":"a.txt""#);
        buf = merge_tool_input(&buf, r#"{"path":"a.txt","content":"hi"}"#);
        assert_eq!(buf, r#"{"path":"a.txt","content":"hi"}"#);
        assert!(serde_json::from_str::<serde_json::Value>(&buf).is_ok());
    }

    /// 纯增量碎片三帧：每帧只带新片段 → 追加拼成完整 JSON。
    #[test]
    fn test_merge_pure_increments() {
        let mut buf = String::new();
        buf = merge_tool_input(&buf, r#"{"path":"#);
        buf = merge_tool_input(&buf, r#""a.txt","content""#);
        buf = merge_tool_input(&buf, r#":"hi"}"#);
        assert_eq!(buf, r#"{"path":"a.txt","content":"hi"}"#);
        assert!(serde_json::from_str::<serde_json::Value>(&buf).is_ok());
    }

    /// 重复终帧：同一完整快照来两次 → 不翻倍。
    #[test]
    fn test_merge_duplicate_final_frame() {
        let full = r#"{"a":1,"b":2}"#;
        let mut buf = String::new();
        buf = merge_tool_input(&buf, full);
        buf = merge_tool_input(&buf, full);
        assert_eq!(buf, full, "重复终帧不应翻倍");
    }

    /// 核心：两个各自完整、彼此非前缀的对象 → 结果是第二帧，而不是 `{"a":1}{"a":2}` 粘连串。
    #[test]
    fn test_merge_nonprefix_double_object_keeps_latest() {
        let buf = merge_tool_input(r#"{"a":1}"#, r#"{"a":2}"#);
        assert_eq!(
            buf, r#"{"a":2}"#,
            "非前缀双完整对象应只留最新，消灭 object 粘连"
        );
        assert!(
            serde_json::from_str::<serde_json::Value>(&buf).is_ok(),
            "结果必须是合法 JSON"
        );
    }

    /// 完整对象之后来一个更短的旧前缀快照 → 保持完整，不被旧短帧覆盖。
    #[test]
    fn test_merge_full_then_shorter_prefix_kept() {
        let full = r#"{"path":"a.txt","content":"hi"}"#;
        let buf = merge_tool_input(full, r#"{"path""#);
        assert_eq!(buf, full, "迟到的旧短前缀快照应被丢弃，保留更全缓冲");
    }

    /// 空帧不改变缓冲；空缓冲取本帧。
    #[test]
    fn test_merge_empty_edges() {
        assert_eq!(merge_tool_input("abc", ""), "abc", "空帧 → 缓冲不变");
        assert_eq!(merge_tool_input("", "abc"), "abc", "空缓冲 → 取本帧");
        assert_eq!(merge_tool_input("", ""), "", "双空 → 空");
    }

    /// 真增量碎片（各帧本身非法）→ append 后拼成合法整体，不被第 6 步误判。
    #[test]
    fn test_merge_illegal_fragments_append() {
        // 第一帧是未闭合的合法前缀，第二帧续上闭合：两者都不是完整 JSON → 走追加。
        let buf = merge_tool_input(r#"{"x":[1,2"#, r#",3]}"#);
        assert_eq!(buf, r#"{"x":[1,2,3]}"#);
        assert!(serde_json::from_str::<serde_json::Value>(&buf).is_ok());
    }

    /// 回归（🔴 第 5 步裸判据吞掉单独成帧的 `{`）：buf 尚不完整（`{"nested":`）时，
    /// 上游把嵌套对象的开括号单独发成一帧 → 旧代码用「buf.starts_with(frame)」纯形状判据，
    /// 而几乎所有 JSON 对象前缀都以 `{` 开头，于是把这个合法续帧误判成「迟到的旧短快照」而丢弃，
    /// 拼出的串永久缺这个 `{`。修复后必须追加，最终三帧拼出完整合法 JSON。
    #[test]
    fn test_merge_standalone_open_brace_not_dropped_when_buf_incomplete() {
        let mut buf = String::new();
        buf = merge_tool_input(&buf, r#"{"nested":"#);
        buf = merge_tool_input(&buf, "{");
        buf = merge_tool_input(&buf, r#""a":1}}"#);
        assert!(
            buf.starts_with('{'),
            "拼出的串必须以 `{{` 开头，实际: {}",
            buf
        );
        assert_eq!(buf, r#"{"nested":{"a":1}}"#, "实际: {}", buf);
        assert!(
            serde_json::from_str::<serde_json::Value>(&buf).is_ok(),
            "必须能解析成完整合法 JSON，实际: {}",
            buf
        );
    }

    /// 顺序钉死：同一个短前缀帧 `{`，buf 完整 vs 不完整，第 5 步的判定必须相反 ——
    /// 前置条件必须挂在第 5 步本身，而不是被更前面的步骤悄悄接管，也不能被更后面的
    /// 步骤（6/7）兜底出错误结果。两种 buf 都不触发第 1~4 步，唯一的分岔点就是第 5 步
    /// 新增的 `is_complete_json(buf)` 前置条件。
    #[test]
    fn test_merge_short_prefix_frame_gating_depends_on_buf_completeness() {
        // buf 不完整：单独成帧的 `{` 必须被追加（落到第 7 步），不能被第 5 步丢弃。
        let incomplete_result = merge_tool_input(r#"{"nested":"#, "{");
        assert_eq!(
            incomplete_result, r#"{"nested":{"#,
            "buf 不完整时，单独成帧的 `{{` 必须被追加，不能丢弃，实际: {}",
            incomplete_result
        );

        // buf 完整：更短的迟到快照仍应被丢弃（第 5 步应正常触发，不能被修复误伤）。
        let complete_result = merge_tool_input(r#"{"a":1}"#, "{");
        assert_eq!(
            complete_result, r#"{"a":1}"#,
            "buf 完整时，更短的迟到旧快照仍应被丢弃，实际: {}",
            complete_result
        );
    }

    // ============ JSON 修复层（缓解④，根治向）离线注入测试 ============
    // 数据源：Claude Code 官方 issue 坐实的真实坏帧成因——
    //   #20015 Windows 路径反斜杠（\U 非法转义）、#69522 长 unicode 转义、#29715 裸控制符/smart quote。
    // 契约：repair_tool_json 只在 from_str 已失败时被调用；返回 Some 必为合法 JSON，否则 None。

    /// 铁律①：合法 JSON 绝不进入修复函数——但即便误入，也应原样往返、语义不变（幂等安全网）。
    #[test]
    fn test_repair_noop_on_valid_json() {
        let valid = r#"{"path":"C:/Users/foo","content":"hello world"}"#;
        // repair_tool_json 内部先 char_level 再复验；合法 JSON 的 char_level 不改结构、复验即过。
        let repaired = repair_tool_json(valid).expect("合法 JSON 应能往返");
        let a: serde_json::Value = serde_json::from_str(valid).unwrap();
        let b: serde_json::Value = serde_json::from_str(&repaired).unwrap();
        assert_eq!(a, b, "合法 JSON 修复往返后语义必须完全一致");
    }

    /// #20015：Windows 路径反斜杠 `C:\Users`（模型直接吐字面 `\U`，JSON 非法转义）。
    /// 现状：客户端 JSON.parse 失败 → Invalid tool parameters。修复后合法，客户端不再报错。
    ///
    /// 诚实边界：`\U`、`\d` 是 JSON 非法转义 → 修复层把反斜杠降级成字面 `\\`，值正确还原。
    /// 但 `\t`（如 `\test.txt`）是 JSON **合法**转义（制表符）——修复层**不碰合法转义**（碰了会破坏
    /// 正常场景），故这里用 `program.exe` 这类**不含合法转义字符**的路径，锁死"非法转义被正确还原"。
    #[test]
    fn test_repair_windows_path_backslash() {
        // content 值里 C:\Users\dwgx\program.exe —— \U \d \p 都是 JSON 非法转义（无合法转义歧义）。
        let bad = r#"{"file_path":"C:\Users\dwgx\program.exe"}"#;
        assert!(
            serde_json::from_str::<serde_json::Value>(bad).is_err(),
            "前提：Windows 反斜杠路径确为非法 JSON"
        );
        let fixed = repair_tool_json(bad).expect("Windows 路径反斜杠应可修复");
        let v: serde_json::Value = serde_json::from_str(&fixed).expect("修复后必合法");
        assert_eq!(
            v["file_path"].as_str().unwrap(),
            r"C:\Users\dwgx\program.exe",
            "修复后路径值应还原成字面反斜杠（非法转义 \\U\\d\\p 降级为字面）"
        );
    }

    /// #29715：裸控制符（真实换行/制表符混进字符串值，未转义）→ JSON 非法。修复后转义成 \n/\t。
    #[test]
    fn test_repair_bare_control_chars() {
        // content 值里有真实换行和 tab（裸控制符），JSON 字符串内非法。
        let bad = "{\"content\":\"line1\nline2\tend\"}";
        assert!(
            serde_json::from_str::<serde_json::Value>(bad).is_err(),
            "前提：裸控制符确为非法 JSON"
        );
        let fixed = repair_tool_json(bad).expect("裸控制符应可修复");
        let v: serde_json::Value = serde_json::from_str(&fixed).expect("修复后必合法");
        assert_eq!(
            v["content"].as_str().unwrap(),
            "line1\nline2\tend",
            "修复后控制符应还原为真实换行/制表符（值语义不变）"
        );
    }

    /// 截断输出（流被中途切断，缺收尾 `"` 和 `}`）→ 结构层补全。
    #[test]
    fn test_repair_truncated_structure() {
        let bad = r#"{"path":"a.txt","content":"unfinished"#;
        assert!(
            serde_json::from_str::<serde_json::Value>(bad).is_err(),
            "前提：截断串确为非法 JSON"
        );
        let fixed = repair_tool_json(bad).expect("截断应可补全");
        let v: serde_json::Value = serde_json::from_str(&fixed).expect("补全后必合法");
        assert_eq!(v["path"].as_str().unwrap(), "a.txt");
        assert_eq!(v["content"].as_str().unwrap(), "unfinished");
    }

    /// #69522：截断的 `\u` 转义（`\uD83`——不足 4 位 hex）→ 降级字面，复验兜底。
    #[test]
    fn test_repair_truncated_unicode_escape() {
        let bad = r#"{"q":"emoji \uD83"}"#;
        assert!(
            serde_json::from_str::<serde_json::Value>(bad).is_err(),
            "前提：截断 \\u 转义确为非法 JSON"
        );
        // 能修成合法即达标（降级为字面 \uD83 文本），语义退化可接受、总比客户端整个报错强。
        let fixed = repair_tool_json(bad).expect("截断 unicode 转义应可修成合法");
        assert!(
            serde_json::from_str::<serde_json::Value>(&fixed).is_ok(),
            "修复后必为合法 JSON"
        );
    }

    /// 洞4:合法 UTF-16 代理对(😀 = 😀)必须**原样保留**,不被误降级。
    #[test]
    fn test_repair_keeps_valid_surrogate_pair() {
        // 构造一个整体非法(裸控制符触发 repair)、但含合法代理对的串,验证代理对不被破坏。
        let bad = "{\"emoji\":\"\\uD83D\\uDE00\nx\"}"; // 含真实换行=裸控制符→非法,触发 repair
        assert!(serde_json::from_str::<serde_json::Value>(bad).is_err());
        let fixed = repair_tool_json(bad).expect("应可修复");
        let v: serde_json::Value = serde_json::from_str(&fixed).expect("修复后合法");
        // 合法代理对应解码成 😀,不被降级为字面。
        assert!(
            v["emoji"].as_str().unwrap().contains('😀'),
            "合法代理对必须保留为 emoji"
        );
    }

    /// 洞4:孤立高代理(无低代理配对)→ 降级字面,修成合法 JSON。
    #[test]
    fn test_repair_isolated_high_surrogate() {
        let bad = r#"{"x":"\uD83Dnext"}"#; // 高代理后跟普通文本,孤立
        assert!(
            serde_json::from_str::<serde_json::Value>(bad).is_err(),
            "前提:孤立高代理非法"
        );
        let fixed = repair_tool_json(bad).expect("孤立高代理应可降级修复");
        assert!(
            serde_json::from_str::<serde_json::Value>(&fixed).is_ok(),
            "修复后必合法"
        );
    }

    /// 洞4:孤立低代理 → 降级字面,修成合法 JSON。
    #[test]
    fn test_repair_isolated_low_surrogate() {
        let bad = r#"{"x":"\uDE00abc"}"#;
        assert!(
            serde_json::from_str::<serde_json::Value>(bad).is_err(),
            "前提:孤立低代理非法"
        );
        let fixed = repair_tool_json(bad).expect("孤立低代理应可降级修复");
        assert!(
            serde_json::from_str::<serde_json::Value>(&fixed).is_ok(),
            "修复后必合法"
        );
    }

    /// glued 粘连修复：头部多余 `}` 剥除后得到合法 JSON。
    #[test]
    fn test_repair_glued_leading_brace() {
        let bad = r#"}{"path": "src/foo.rs"}"#;
        assert!(
            serde_json::from_str::<serde_json::Value>(bad).is_err(),
            "前提：glued 串非合法 JSON"
        );
        let fixed = repair_tool_json(bad).expect("glued 粘连应可修复");
        let v: serde_json::Value = serde_json::from_str(&fixed).expect("修复后必须是合法 JSON");
        assert_eq!(v["path"].as_str().unwrap(), "src/foo.rs");
    }

    /// glued 粘连修复（带空白分隔）：`} {` 变体同样可剥除。
    #[test]
    fn test_repair_glued_with_space() {
        let bad = r#"} {"key": "value"}"#;
        assert!(
            serde_json::from_str::<serde_json::Value>(bad).is_err(),
            "前提：非法"
        );
        let fixed = repair_tool_json(bad).expect("glued (带空白) 应可修复");
        let v: serde_json::Value = serde_json::from_str(&fixed).unwrap();
        assert_eq!(v["key"].as_str().unwrap(), "value");
    }

    /// glued 修复保守性：`}` 后无 `{` 时返回 None，不乱猜。
    #[test]
    fn test_repair_glued_no_opening_brace_returns_none() {
        let bad = r#"}not_json_at_all"#;
        assert!(
            repair_tool_json(bad).is_none(),
            "无 `{{` 时修复层应返回 None"
        );
    }

    /// 洞1:整包双重编码解包——顶层是被字符串编码的 object → 解一层还原。
    #[test]
    fn test_unwrap_double_encoded_object() {
        // 双重编码:整个 {"path":"a.txt"} 被再套一层字符串编码。
        let double = r#""{\"path\":\"a.txt\"}""#;
        // 顶层 from_str 成功但得到 String(漏过 repair 层)。
        assert!(
            serde_json::from_str::<serde_json::Value>(double)
                .unwrap()
                .is_string()
        );
        let unwrapped = unwrap_double_encoded(double).expect("应解一层");
        let v: serde_json::Value = serde_json::from_str(&unwrapped).unwrap();
        assert_eq!(v["path"].as_str().unwrap(), "a.txt");
    }

    /// review confirmed 回归(端到端):合法的双重编码串经 flush_tool_input 必须被 unwrap 成 object
    /// 再发出。这锁死"unwrap 在函数尾统一出口执行"——修复前 repair 成功分支 early-return 会绕过它,
    /// 修复后两条路径(原本合法 / 修复后)都经同一 unwrap 出口。此处走"原本合法"路径(from_str 成功
    /// → 跳过 repair → 命中尾部 unwrap),验证出口本身正确;repair 分支的 fall-through 由结构保证
    /// (assembled=repaired 后不再 return,与本路径汇合到同一 unwrap)。
    #[test]
    fn test_flush_unwraps_double_encoded_at_exit() {
        let mut ctx =
            StreamContext::new_with_thinking("claude-sonnet-4.6", 1, false, HashMap::new());
        let _ = ctx.generate_initial_events();
        // 合法的双重编码:整个 {"path":"a.txt"} 被再套一层字符串编码(from_str 成功但得 String)。
        let double = r#""{\"path\":\"a.txt\"}""#;
        assert!(
            serde_json::from_str::<serde_json::Value>(double)
                .unwrap()
                .is_string(),
            "前提:顶层 from_str 成功但是 String(双重编码,会漏过 repair)"
        );
        let evs = ctx.flush_tool_input(0, double.to_string());
        let delta = evs
            .iter()
            .find(|e| e.data["delta"]["type"] == "input_json_delta")
            .expect("应发出 input_json_delta");
        let partial = delta.data["delta"]["partial_json"].as_str().unwrap();
        let v: serde_json::Value = serde_json::from_str(partial).expect("发出的必是合法 JSON");
        assert!(
            v.is_object(),
            "双重编码必须在出口被 unwrap 成 object,实际={}",
            partial
        );
        assert_eq!(v["path"].as_str().unwrap(), "a.txt");
    }

    /// 洞1:双重编码 array 也解;标量/普通 object 不动。
    #[test]
    fn test_unwrap_double_encoded_boundaries() {
        // array 双重编码 → 解。
        let arr = r#""[1,2,3]""#;
        assert!(unwrap_double_encoded(arr).is_some());
        // 正常 object(非双重编码)→ 不动(顶层不是 String)。
        assert!(unwrap_double_encoded(r#"{"a":1}"#).is_none());
        // 顶层是字符串但内层是标量(不是 object/array)→ 不动(不臆测)。
        assert!(unwrap_double_encoded(r#""hello""#).is_none());
        assert!(unwrap_double_encoded(r#""42""#).is_none());
    }

    /// 端到端：flush_tool_input 收到非法 JSON（开关默认开）→ 修复成功 → 发出的 partial_json 必合法。
    /// 这是客户端真正消费的字节，锁死"客户端不再报 Invalid tool parameters"。
    #[test]
    fn test_flush_tool_input_repairs_before_sending() {
        let mut ctx =
            StreamContext::new_with_thinking("claude-sonnet-4.6", 1, false, HashMap::new());
        let _ = ctx.generate_initial_events();
        // Windows 路径非法转义（#20015 真实成因）。
        let bad = r#"{"file_path":"C:\Users\x\a.txt"}"#.to_string();
        let evs = ctx.flush_tool_input(0, bad);
        let delta = evs
            .iter()
            .find(|e| e.data["delta"]["type"] == "input_json_delta")
            .expect("应发出 input_json_delta");
        let partial = delta.data["delta"]["partial_json"].as_str().unwrap();
        assert!(
            serde_json::from_str::<serde_json::Value>(partial).is_ok(),
            "flush 发给客户端的 partial_json 必须是合法 JSON（修复层已介入）：{}",
            partial
        );
    }

    /// 真实上游模式回归（2026-07-13 本地网关 KIRO_TOOL_TRACE 抓包坐实）：Kiro toolUseEvent.input
    /// 是**纯增量碎片**——每帧只带新片段（`{"path": "` → `test.txt"` → `, "con` → …），buf 单调增长、
    /// 全程无 `}{` 粘连，最后一帧拼成完整合法 JSON。含反斜杠 / emoji / 多语言 / 引号亦无碍。
    /// 这是 Invalid tool parameters 类型 C 修复覆盖的主路径，此测试锁死其正确性防回归。
    #[test]
    fn test_merge_real_upstream_incremental_capture() {
        // 逐帧照抄一次真实抓包的碎片序列（含转义反斜杠与 emoji）。
        let frames = [
            r#"{"path": ""#,
            r#"test.txt""#,
            r#", "con"#,
            r#"tent": "H"#,
            "ello World! ",
            r#"🌍\n\nend"#,
            r#""}"#,
        ];
        let mut buf = String::new();
        let mut glued_ever = false;
        for f in frames {
            buf = merge_tool_input(&buf, f);
            if buf.contains("}{") {
                glued_ever = true;
            }
        }
        assert!(!glued_ever, "纯增量拼装全程不应出现 }}{{ 粘连");
        assert_eq!(
            buf,
            r#"{"path": "test.txt", "content": "Hello World! 🌍\n\nend"}"#
        );
        assert!(
            serde_json::from_str::<serde_json::Value>(&buf).is_ok(),
            "纯增量碎片最终应拼成合法 JSON（类型 C 主路径）"
        );
    }

    /// 曾经的"类型 A 只能透传"契约，已被修复层（缓解④，默认开）**升级为根治**：上游模型帧含 JSON
    /// 非法转义（`\x` —— JSON 只认 `\uXXXX`）时，`flush_tool_input` 先修成合法 JSON 再发，客户端能
    /// 正常 parse，不再报 Invalid tool parameters。此测试锁死"端到端非法 → 发出的必是合法 JSON"。
    /// （修复层关闭时的原样透传行为由纯函数契约 + repair_off 专测覆盖。）
    #[test]
    fn test_process_tool_use_type_a_illegal_escape_repaired() {
        use crate::kiro::model::events::ToolUseEvent;
        let mut ctx =
            StreamContext::new_with_thinking("claude-sonnet-4.6", 1, false, HashMap::new());
        let _ = ctx.generate_initial_events();
        // `\x41` 是 JSON 非法转义（合法应为 `A`）——模拟上游模型控制 token 抽风产出的非法串。
        let illegal = r#"{"path":"a.txt","content":"bad\x41escape"}"#;
        assert!(
            serde_json::from_str::<serde_json::Value>(illegal).is_err(),
            "前提：该串确为非法 JSON（\\x 转义）"
        );
        let evs = ctx.process_tool_use(&ToolUseEvent {
            name: "write_file".to_string(),
            tool_use_id: "toolu_typea".to_string(),
            input: illegal.to_string(),
            stop: true,
        });
        let delta = evs
            .iter()
            .find(|e| {
                e.event == "content_block_delta" && e.data["delta"]["type"] == "input_json_delta"
            })
            .expect("应发出 input_json_delta（修复后，不吞）");
        let assembled = delta.data["delta"]["partial_json"].as_str().unwrap();
        assert!(
            serde_json::from_str::<serde_json::Value>(assembled).is_ok(),
            "修复层默认开：发给客户端的必是合法 JSON，实际={}",
            assembled
        );
    }

    /// 泄漏 token 清洗（收严高信号）：行首泄漏词直贴 **CJK/全角** 粘连 → 剥离；正常英文用法（含
    /// ASCII 冒号/数字/大写）→ 绝不误删。court/課/课 独占整行 → 剥（高置信 #70544 泄漏）。
    #[test]
    fn test_strip_leaked_prefix() {
        // 辅助：只取清洗后文本（新签名返回 (String, StripHit)）。
        let s = |line: &str| StreamContext::strip_leaked_prefix(line).0;
        // CJK/全角粘连 → 剥离。
        assert_eq!(s("course重读文件"), "重读文件");
        assert_eq!(s("課我加的是"), "我加的是");
        assert_eq!(s("care：我把"), "：我把"); // 全角冒号
        assert_eq!(s("count你好"), "你好");
        assert_eq!(s("court重读"), "重读"); // 新增 court
        assert_eq!(s("card表格"), "表格"); // 新增 card
        assert_eq!(s("call调用"), "调用"); // 新增 call
        // court/課/课 独占整行 → 剥空(保留换行)。
        assert_eq!(s("court\n"), "\n");
        assert_eq!(s("court"), "");
        assert_eq!(s("課\n"), "\n");
        // 【收严关键】正常英文含 ASCII 冒号/数字/大写 → 绝不误删(旧逻辑会误剥)。
        assert_eq!(s("count: 42"), "count: 42"); // 半角冒号
        assert_eq!(s("countDown()"), "countDown()"); // 大写
        assert_eq!(s("care2share"), "care2share"); // 数字
        assert_eq!(s("courseCatalog"), "courseCatalog"); // 大写
        assert_eq!(s("card#1"), "card#1"); // ASCII 标点
        // 正常英文：词后空格 / 小写延续 → 原样保留。
        assert_eq!(s("count the items"), "count the items");
        assert_eq!(s("counter offer"), "counter offer");
        assert_eq!(s("careful now"), "careful now");
        assert_eq!(s("call me"), "call me");
        // call/card/count/care/course 独占整行 → **不**享特例(可能是正常内容),保守不剥。
        assert_eq!(s("count"), "count");
        assert_eq!(s("card"), "card");
        assert_eq!(s("call"), "call");
        // 非泄漏词开头 → 原样。
        assert_eq!(s("hello世界"), "hello世界");
    }

    /// 诊断计数：strip 命中信息（stripped / standalone）正确——供收尾泄漏诊断计数。
    #[test]
    fn test_strip_leaked_prefix_hit_flags() {
        let (_, hit) = StreamContext::strip_leaked_prefix("court\n"); // 独占整行
        assert!(
            hit.stripped && hit.standalone,
            "独占整行 court 应 stripped+standalone"
        );
        let (_, hit) = StreamContext::strip_leaked_prefix("course重读"); // 粘连非独占
        assert!(
            hit.stripped && !hit.standalone,
            "粘连剥离应 stripped 但非 standalone"
        );
        let (_, hit) = StreamContext::strip_leaked_prefix("count: 42"); // 正常英文不剥
        assert!(!hit.stripped && !hit.standalone, "正常英文不应命中");
    }

    /// clean_leaked_tokens：只在行首处理，多行时每行行首各判一次；并累加诊断计数。
    #[test]
    fn test_clean_leaked_tokens_multiline() {
        let mut ctx =
            StreamContext::new_with_thinking("claude-sonnet-4.6", 1, false, HashMap::new());
        let input = "course重读\nnormal line\ncount你好";
        assert_eq!(ctx.clean_leaked_tokens(input), "重读\nnormal line\n你好");
        assert_eq!(ctx.leaked_stripped, 2, "剥了 course / count 两个");
        assert_eq!(ctx.leaked_saturation_lines, 0, "无独占整行泄漏");
    }

    /// saturation 计数：满屏纯 court 独占行 → 每行计入 saturation。
    #[test]
    fn test_clean_leaked_tokens_saturation_count() {
        let mut ctx = StreamContext::new_with_thinking("claude-opus-4.8", 1, false, HashMap::new());
        let input = "court\ncourt\ncourt\n";
        ctx.clean_leaked_tokens(input);
        assert_eq!(
            ctx.leaked_saturation_lines, 3,
            "3 行纯 court 独占行=saturation 信号"
        );
        assert_eq!(ctx.leaked_stripped, 3);
    }

    /// process_tool_use 端到端：非前缀双对象场景，flush 出的 partial_json 必须是合法 JSON（第二帧）。
    #[test]
    fn test_process_tool_use_nonprefix_double_object_emits_legal_json() {
        use crate::kiro::model::events::ToolUseEvent;
        let mut ctx =
            StreamContext::new_with_thinking("claude-sonnet-4.6", 1, false, HashMap::new());
        let _ = ctx.generate_initial_events();
        let _ = ctx.process_tool_use(&ToolUseEvent {
            name: "AskUserQuestion".to_string(),
            tool_use_id: "toolu_np".to_string(),
            input: r#"{"a":1}"#.to_string(),
            stop: false,
        });
        let evs = ctx.process_tool_use(&ToolUseEvent {
            name: "AskUserQuestion".to_string(),
            tool_use_id: "toolu_np".to_string(),
            input: r#"{"a":2}"#.to_string(),
            stop: true,
        });
        let delta = evs
            .iter()
            .find(|e| {
                e.event == "content_block_delta" && e.data["delta"]["type"] == "input_json_delta"
            })
            .expect("应有 input_json_delta");
        let assembled = delta.data["delta"]["partial_json"].as_str().unwrap();
        assert_eq!(assembled, r#"{"a":2}"#, "非前缀双对象只发最新完整对象");
        assert!(
            serde_json::from_str::<serde_json::Value>(assembled).is_ok(),
            "流式 flush 的 partial_json 必须合法"
        );
    }

    /// 隔离回归：tool_use input 含小于号 + 全角竖线 DSML 起始标记（U+FF5C）时，
    /// 拼装完全不经过 strip_dsml_markers，原样保留（Claude 系）。
    #[test]
    fn test_tool_input_not_stripped_by_dsml_claude() {
        use crate::kiro::model::events::ToolUseEvent;
        let mut ctx =
            StreamContext::new_with_thinking("claude-sonnet-4.6", 1, false, HashMap::new());
        let _ = ctx.generate_initial_events();
        let payload = r#"{"code":"if a < b","note":"<｜DSML｜function_calls｜>","x":"a<｜tool"}"#;
        let evs = ctx.process_tool_use(&ToolUseEvent {
            name: "Write".to_string(),
            tool_use_id: "toolu_d".to_string(),
            input: payload.to_string(),
            stop: true,
        });
        let delta = evs
            .iter()
            .find(|e| {
                e.event == "content_block_delta" && e.data["delta"]["type"] == "input_json_delta"
            })
            .expect("delta");
        let assembled = delta.data["delta"]["partial_json"].as_str().unwrap();
        assert_eq!(assembled, payload, "tool input 应原样保留，DSML 未碰");
    }

    /// 隔离回归：国产模型（deepseek）下 DSML 门控放行，但 tool input 拼装路径同样不经过剥离。
    #[test]
    fn test_tool_input_not_stripped_by_dsml_deepseek() {
        use crate::kiro::model::events::ToolUseEvent;
        let mut ctx = StreamContext::new_with_thinking("deepseek-v3", 1, false, HashMap::new());
        let _ = ctx.generate_initial_events();
        let payload = r#"{"code":"if a < b","note":"<｜DSML｜function_calls｜>"}"#;
        let evs = ctx.process_tool_use(&ToolUseEvent {
            name: "Write".to_string(),
            tool_use_id: "toolu_d2".to_string(),
            input: payload.to_string(),
            stop: true,
        });
        let delta = evs
            .iter()
            .find(|e| {
                e.event == "content_block_delta" && e.data["delta"]["type"] == "input_json_delta"
            })
            .expect("delta");
        let assembled = delta.data["delta"]["partial_json"].as_str().unwrap();
        assert_eq!(
            assembled, payload,
            "国产模型下 tool input 也应原样，DSML 只作用于 text/thinking"
        );
    }

    // ============ 截断诊断归因标签（短板 2.5，纯可观测）离线测试 ============
    // classify_tool_json_defect 只在 from_str 已失败分支被调、只写日志，绝不进控制流。
    // 判据与 repair 层同源：truncated=结构未闭合/串未终结、illegal_chars=非法转义或裸控制符。

    /// 截断（缺收尾 `"` 和 `}`）→ 归因 Truncated。
    #[test]
    fn test_classify_defect_truncated() {
        let s = r#"{"path":"a.txt","content":"unfinished"#;
        assert_eq!(classify_tool_json_defect(s), ToolJsonDefect::Truncated);
    }

    /// 非法转义（`\x`）+ 结构完整闭合 → 归因 IllegalChars。
    #[test]
    fn test_classify_defect_illegal_chars() {
        let s = r#"{"path":"bad\x41escape"}"#;
        assert_eq!(classify_tool_json_defect(s), ToolJsonDefect::IllegalChars);
    }

    /// 裸控制符（真实换行未转义）+ 结构完整 → 归因 IllegalChars。
    #[test]
    fn test_classify_defect_bare_control() {
        let s = "{\"content\":\"line1\nline2\"}";
        assert_eq!(classify_tool_json_defect(s), ToolJsonDefect::IllegalChars);
    }

    /// 既含非法转义又结构截断 → 归因 TruncatedAndIllegal。
    #[test]
    fn test_classify_defect_truncated_and_illegal() {
        let s = r#"{"path":"C:\Users\x"#;
        assert_eq!(
            classify_tool_json_defect(s),
            ToolJsonDefect::TruncatedAndIllegal
        );
    }

    /// 结构闭合、字符合法，但 `}{` 粘连（非前缀双对象）→ 归因 Malformed。
    #[test]
    fn test_classify_defect_malformed_glued() {
        let s = r#"{"a":1}{"b":2}"#;
        let scan = scan_tool_json(s);
        assert!(scan.glued, "应识别出 }}{{ 粘连");
        assert_eq!(classify_tool_json_defect(s), ToolJsonDefect::Malformed);
    }

    // ============ malformed 子型细分（第一步:解开类型 A/C 的诊断钥匙）离线测试 ============
    // malformed_subkind 只在归因 Malformed 时调用,纯诊断不进控制流。判据源=serde 官方错误消息 + scan.glued。
    // 每条都先确认:①确实 parse 失败 ②归因确为 Malformed(结构闭合+字符合法)③子型标签正确。

    #[test]
    fn test_malformed_subkind_glued() {
        // `}{` 粘连:两个完整对象黏一起——偏类型 C(合并/上游重写),优先怀疑我们侧。
        let s = r#"{"a":1}{"b":2}"#;
        assert!(serde_json::from_str::<serde_json::Value>(s).is_err());
        assert_eq!(classify_tool_json_defect(s), ToolJsonDefect::Malformed);
        assert_eq!(malformed_subkind(s), "glued");
    }

    #[test]
    fn test_malformed_subkind_trailing_comma() {
        // 尾逗号:偏模型侧多吐一个逗号。
        let s = r#"{"a":1,"b":2,}"#;
        assert!(serde_json::from_str::<serde_json::Value>(s).is_err());
        assert_eq!(classify_tool_json_defect(s), ToolJsonDefect::Malformed);
        assert_eq!(malformed_subkind(s), "trailing_comma");
    }

    #[test]
    fn test_malformed_subkind_missing_comma() {
        // 两个键值之间缺分隔逗号:偏模型侧漏吐/上游丢了逗号帧。
        let s = r#"{"a":1"b":2}"#;
        assert!(serde_json::from_str::<serde_json::Value>(s).is_err());
        assert_eq!(classify_tool_json_defect(s), ToolJsonDefect::Malformed);
        assert_eq!(malformed_subkind(s), "missing_comma");
    }

    #[test]
    fn test_malformed_subkind_expected_value() {
        // 键后缺值:偏模型侧生成中断在值前。
        let s = r#"{"a":}"#;
        assert!(serde_json::from_str::<serde_json::Value>(s).is_err());
        assert_eq!(classify_tool_json_defect(s), ToolJsonDefect::Malformed);
        assert_eq!(malformed_subkind(s), "expected_value");
    }

    #[test]
    fn test_malformed_subkind_key_not_string() {
        // 键不是字符串(裸键):偏模型侧吐了非法键。
        let s = r#"{a:1}"#;
        assert!(serde_json::from_str::<serde_json::Value>(s).is_err());
        assert_eq!(classify_tool_json_defect(s), ToolJsonDefect::Malformed);
        assert_eq!(malformed_subkind(s), "key_not_string");
    }

    #[test]
    fn test_malformed_subkind_trailing_chars() {
        // 首个完整值后有非 `}{` 的多余字符(不触发 glued 那条)——偏上游多发尾随。
        let s = r#"{"a":1} garbage"#;
        assert!(serde_json::from_str::<serde_json::Value>(s).is_err());
        assert_eq!(classify_tool_json_defect(s), ToolJsonDefect::Malformed);
        // 非 `}{`,glued=false,应落到 trailing_chars。
        assert_eq!(malformed_subkind(s), "trailing_chars");
    }

    /// 短板2：截断跨轮恢复纯决策——恢复开关开 + 修复层开 + 归因真截断,三条件全满足才触发。
    #[test]
    fn test_contains_textified_tool_call_detector() {
        // 文本化 invoke 标记(不论是否带 antml: 前缀)应命中。
        assert!(contains_textified_tool_call(r#"<invoke name="Bash">"#));
        assert!(contains_textified_tool_call(r#"<invoke name="Bash">"#));
        assert!(contains_textified_tool_call("</invoke>"));
        assert!(contains_textified_tool_call(
            r#"<parameter name="command">"#
        ));
        assert!(contains_textified_tool_call("</function_calls>"));
        // 正常文本不误命中。
        assert!(!contains_textified_tool_call(
            "这是一段正常的助手回复,讲 invoke 概念但无标签"
        ));
        assert!(!contains_textified_tool_call(
            "函数调用 function calls 讨论"
        ));
        assert!(!contains_textified_tool_call(""));
    }

    #[test]
    fn test_should_recover_truncation_decision() {
        use ToolJsonDefect::*;
        // 恢复开关关 → 任何情况都不触发（默认行为不变）。
        assert!(!should_recover_truncation(Truncated, false, true));
        assert!(!should_recover_truncation(TruncatedAndIllegal, false, true));
        // 修复层关 → 不触发（无法断言"修复也补不回",退回②/③原语义）。
        assert!(!should_recover_truncation(Truncated, true, false));
        assert!(!should_recover_truncation(TruncatedAndIllegal, true, false));
        // 恢复开 + 修复开 + 真截断 → 触发。
        assert!(should_recover_truncation(Truncated, true, true));
        assert!(should_recover_truncation(TruncatedAndIllegal, true, true));
        // 恢复开 + 修复开但非截断畸形 → 不归本开关管（仍走②/③）。
        assert!(!should_recover_truncation(IllegalChars, true, true));
        assert!(!should_recover_truncation(Malformed, true, true));
    }

    /// 稳定短标签：日志字段值不随重构漂移。
    #[test]
    fn test_defect_as_str_labels() {
        assert_eq!(ToolJsonDefect::Truncated.as_str(), "truncated");
        assert_eq!(ToolJsonDefect::IllegalChars.as_str(), "illegal_chars");
        assert_eq!(
            ToolJsonDefect::TruncatedAndIllegal.as_str(),
            "truncated_and_illegal"
        );
        assert_eq!(ToolJsonDefect::Malformed.as_str(), "malformed");
    }
