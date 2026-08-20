    //! `input_tokens` 的**两个口径**：
    //! - 发给客户端的 `usage.input_tokens` = billed（已剔除 cache，与 cache_read 互斥）
    //! - 落进 `RequestRecord` 的 `input_tokens` = gross（含 cache，是 cache_read 的超集）
    //!
    //! 同名不同义，零注释时极易被下游当同一口径而把 cache 计两次。此处把契约钉死：
    //! 谁改动其中一侧的口径，本模块必失败。
    use super::*;

    fn ctx_with_cache(input_tokens: i32, cache_read: i32, cache_creation: i32) -> StreamContext {
        let mut ctx = StreamContext::new_with_thinking(
            "claude-sonnet-5",
            input_tokens,
            false,
            HashMap::new(),
        );
        ctx.set_cache_usage(Some(CacheUsageBreakdown {
            cache_creation_input_tokens: cache_creation,
            cache_read_input_tokens: cache_read,
            cache_creation_5m_input_tokens: cache_creation,
            cache_creation_1h_input_tokens: 0,
        }));
        ctx
    }

    #[test]
    fn should_report_gross_input_to_record_and_billed_to_client() {
        let ctx = ctx_with_cache(12_500, 12_000, 300);

        // 客户端侧：billed 口径（12500 - 300 - 12000 = 200）
        let msg_start = ctx.create_message_start_event();
        assert_eq!(msg_start["message"]["usage"]["input_tokens"], 200);
        assert_eq!(
            msg_start["message"]["usage"]["cache_read_input_tokens"],
            12_000
        );

        // 统计侧：gross 口径（未剔除 cache）
        let usage = ctx.resolved_usage();
        assert_eq!(
            usage.input_tokens, 12_500,
            "ResolvedUsage.input_tokens 必须是 gross，改成 billed 会让历史统计数据断裂"
        );
        assert_eq!(usage.cache_read_tokens, 12_000);
        assert_eq!(usage.cache_creation_tokens, 300);
    }

    #[test]
    fn should_keep_gross_input_as_superset_of_cache_in_record() {
        // 落库后：cache 是 input 的子集，消费方直接用 input_tokens 即为总输入
        let ctx = ctx_with_cache(12_500, 12_000, 300);
        let usage = ctx.resolved_usage();
        let mut record = crate::usage::RequestRecord::new("req-caliber", "claude-sonnet-5");
        record.input_tokens = usage.input_tokens;
        record.cache_read_tokens = usage.cache_read_tokens;
        record.cache_creation_tokens = usage.cache_creation_tokens;
        record.clamp_cache_to_input();

        assert!(
            record.cache_read_tokens + record.cache_creation_tokens <= record.input_tokens,
            "cache 必须是 gross input 的子集"
        );
        // 还原客户端口径应与 message_start 一致
        assert_eq!(
            record.billed_input_tokens(),
            ctx.create_message_start_event()["message"]["usage"]["input_tokens"]
                .as_i64()
                .unwrap() as i32
        );
    }

    #[test]
    fn should_prefer_context_usage_over_estimate_for_gross_input() {
        // resolved_usage 优先上游反推值；这正是它与 cache（本地估算）不同源的根因
        let mut ctx = ctx_with_cache(10_000, 9_000, 0);
        ctx.context_input_tokens = Some(4_000);
        assert_eq!(ctx.resolved_usage().input_tokens, 4_000);
        // 此时 cache_read(9000) > input(4000) → 落库前必须收敛，否则面板出现矛盾数字
        let usage = ctx.resolved_usage();
        let mut record = crate::usage::RequestRecord::new("req-mismatch", "claude-sonnet-5");
        record.input_tokens = usage.input_tokens;
        record.cache_read_tokens = usage.cache_read_tokens;
        record.clamp_cache_to_input();
        assert_eq!(record.cache_read_tokens, 4_000);
    }
