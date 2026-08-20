    //! 从 k2cc 移植的「token 显示缩放 + 空/近空响应检测」回归测试。
    //!
    //! 缩放只改**回给客户端**的 usage 展示，不碰真实记账；空响应判据要求
    //! 输入占比 + 输出过短 + 无工具调用三者同时，避免误伤正常短回答。
    use super::*;

    fn ctx(model: &str, input_tokens: i32) -> StreamContext {
        StreamContext::new_with_thinking(model, input_tokens, false, HashMap::new())
    }

    #[test]
    fn test_scale_for_client_basic() {
        assert_eq!(scale_for_client(100_000), 66_570);
        assert_eq!(scale_for_client(85_000), 56_585);
        assert_eq!(scale_for_client(0), 0);
        assert_eq!(scale_for_client(1), 1);
        assert_eq!(scale_for_client(-100), 0);
        assert_eq!(scale_for_client(11), 8); // ceil(11 × 0.6657) = 8
    }

    #[test]
    fn test_empty_response_fully_empty() {
        // output=0 且无工具调用 → 完全空响应。
        let c = ctx("claude-sonnet-5", 10_000);
        assert_eq!(c.output_tokens, 0);
        assert!(c.is_empty_response());
    }

    #[test]
    fn test_empty_response_near_empty_oversized_context_flagged() {
        // output 极少 + 无工具调用 + input 超 28% 窗口 → 上下文压力退化响应。
        let model = "claude-sonnet-5";
        let threshold = empty_response_oversized_threshold(model);
        let mut c = ctx(model, threshold);
        c.output_tokens = 5;
        assert!(c.is_empty_response());
        assert!(c.empty_response_is_oversized_context());
    }

    #[test]
    fn test_empty_response_near_empty_small_context_not_flagged() {
        // 同样短的输出，但输入远未超阈值 → 正常短回答，不误伤。
        let mut c = ctx("claude-sonnet-5", 10_000);
        c.output_tokens = 5;
        assert!(!c.is_empty_response());
        assert!(!c.empty_response_is_oversized_context());
    }

    #[test]
    fn test_empty_response_with_tool_use_not_flagged() {
        // 有工具调用 → 客户端会去执行工具，不是空响应。
        let mut c = ctx("claude-sonnet-5", 10_000);
        c.output_tokens = 5;
        c.state_manager.set_has_tool_use(true);
        assert!(!c.is_empty_response());
    }

    #[test]
    fn test_empty_response_short_answer_not_flagged() {
        // output=15（<30）但输入小 → 正常短回答。
        let mut c = ctx("claude-sonnet-5", 10_000);
        c.output_tokens = 15;
        assert!(!c.is_empty_response());
    }

    #[test]
    fn near_empty_judgment_shared_by_both_paths() {
        // 流式（is_empty_response）与非流式（handlers.rs 收尾兜底）必须调用同一个
        // 共用判据函数，不得各写一份实现 —— 两条路径分界漂移的后果是「流式判空、
        // 非流式放行 → 200 空 body」。本守卫钉调用点，不钉行为。
        //
        // needle 运行时拼接：include_str! 会把本测试模块自己的字面量也算进匹配，
        // 固定串会让「删掉调用、守卫自己仍含字面量」的纸面测试变绿。
        let call = format!("{}near_empty_response{}", "stream::", "(");
        let impl_marker = format!("{}near_empty_response", "fn ");
        let self_src = include_str!("stream.rs");
        let handlers_src = include_str!("handlers.rs");
        let prod = |src: &str| -> String {
            // 只看生产段（切掉测试模块），且剔掉注释行 —— 否则守卫会匹配到
            // 注释里的说明文字，变成「把调用注释掉守卫仍绿」的纸面测试。
            let cut = src.find("#[cfg(test)]").unwrap_or(src.len());
            src[..cut]
                .lines()
                .filter(|l| !l.trim_start().starts_with("//"))
                .collect::<Vec<_>>()
                .join("\n")
        };
        let handlers_prod = prod(handlers_src);
        assert!(
            handlers_prod.contains(&call),
            "非流式收尾兜底必须调用共用判据（handlers.rs 出现独立实现即分界漂移）"
        );
        assert!(
            !handlers_prod.contains(&impl_marker),
            "handlers.rs 不得出现共用判据的独立实现（只能调用，不能定义）"
        );
        let self_prod = prod(self_src);
        let cut = self_prod
            .find("pub fn is_empty_response")
            .expect("is_empty_response 定义不应被删改");
        let body = &self_prod[cut..];
        let body = &body[..body
            .find("pub fn empty_response_is_oversized_context")
            .expect("is_empty_response 应后随 empty_response_is_oversized_context")];
        assert!(
            body.contains("near_empty_response("),
            "流式 is_empty_response 必须收敛到共用判据（残留自实现即分界漂移）"
        );
    }
