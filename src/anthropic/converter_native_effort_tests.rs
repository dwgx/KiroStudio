//! native effort 路径测试。
//!
//! 镜像开关是进程级全局，测试并行会相互污染。用一把静态锁串行所有触碰该开关的
//! 用例，并在守卫里恢复原值（与 ENV_NOISE_TEST_LOCK 同款）。

    use super::*;
    use super::super::types::Message as AnthropicMessage;

    static NATIVE_EFFORT_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct NativeEffortGuard {
        prev: bool,
        _lock: std::sync::MutexGuard<'static, ()>,
    }
    impl NativeEffortGuard {
        fn with(enabled: bool) -> Self {
            let lock = NATIVE_EFFORT_TEST_LOCK
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let prev = native_thinking_effort_enabled();
            set_native_thinking_effort_enabled(enabled);
            NativeEffortGuard { prev, _lock: lock }
        }
    }
    impl Drop for NativeEffortGuard {
        fn drop(&mut self) {
            set_native_thinking_effort_enabled(self.prev);
        }
    }

    /// 构造只有一条 user 消息的最小请求，thinking/output_config 可控。
    fn mk_req(
        thinking: Option<super::super::types::Thinking>,
        output_config: Option<super::super::types::OutputConfig>,
    ) -> MessagesRequest {
        MessagesRequest {
            model: "claude-opus-4-8".to_string(),
            max_tokens: 1024,
            messages: vec![AnthropicMessage {
                role: "user".to_string(),
                content: serde_json::json!("hi"),
            }],
            stream: false,
            system: None,
            tools: None,
            tool_choice: None,
            thinking,
            output_config,
            metadata: None,
        }
    }

    fn enabled_thinking(budget: i32) -> super::super::types::Thinking {
        super::super::types::Thinking {
            thinking_type: "enabled".to_string(),
            budget_tokens: budget,
        }
    }

    /// budget_tokens → effort 档位表边界（参考仓同款映射）。
    #[test]
    fn budget_tokens_map_to_effort_tiers() {
        assert_eq!(effort_from_budget_tokens(0), "low");
        assert_eq!(effort_from_budget_tokens(4_000), "low");
        assert_eq!(effort_from_budget_tokens(4_001), "medium");
        assert_eq!(effort_from_budget_tokens(16_000), "medium");
        assert_eq!(effort_from_budget_tokens(16_001), "high");
        assert_eq!(effort_from_budget_tokens(64_000), "high");
        assert_eq!(effort_from_budget_tokens(64_001), "xhigh");
        assert_eq!(effort_from_budget_tokens(i32::MAX), "xhigh");
        assert_eq!(effort_from_budget_tokens(i32::MIN), "low");
    }

    /// 归一化：trim + 小写；未知值回退 "high"。
    #[test]
    fn normalize_effort_is_case_insensitive_with_fallback() {
        assert_eq!(normalize_thinking_effort("low"), "low");
        assert_eq!(normalize_thinking_effort("  HIGH "), "high");
        assert_eq!(normalize_thinking_effort("XHigh"), "xhigh");
        assert_eq!(normalize_thinking_effort("max"), "max");
        assert_eq!(normalize_thinking_effort(""), "high");
        assert_eq!(normalize_thinking_effort("ultra"), "high");
        assert_eq!(normalize_thinking_effort("enabled"), "high");
    }

    /// 白名单：实测过的 4 个模型命中，其余一律不命中（保守）。
    #[test]
    fn whitelist_hits_verified_models_only() {
        assert_eq!(native_reasoning_efforts("claude-opus-4.8"), Some(EFFORTS_WITH_XHIGH));
        assert_eq!(native_reasoning_efforts("claude-opus-4.7"), Some(EFFORTS_WITH_XHIGH));
        assert_eq!(
            native_reasoning_efforts("claude-opus-4.6"),
            Some(EFFORTS_WITHOUT_XHIGH)
        );
        assert_eq!(
            native_reasoning_efforts("claude-sonnet-4.6"),
            Some(EFFORTS_WITHOUT_XHIGH)
        );
        for miss in [
            "claude-opus-5",
            "claude-sonnet-5",
            "claude-opus-4.5",
            "claude-sonnet-4.5",
            "claude-sonnet-4.0",
            "claude-haiku-4.5",
            "deepseek-v4-flash",
            "claude-3-5-sonnet",
            "",
        ] {
            assert_eq!(
                native_reasoning_efforts(miss),
                None,
                "未实测的模型 {miss} 不得进白名单（保守回退 XML 注入）"
            );
        }
    }

    /// ⭐ 白名单与 model_catalog 校准守卫：白名单每个 kiro_id 必须真实存在于目录，
    /// 目录里删模型 → 本测试红，提示同步白名单（防止硬编码白名单脱离 catalog 漂移）。
    #[test]
    fn native_effort_whitelist_models_exist_in_catalog() {
        let catalog = super::super::model_catalog::CATALOG;
        let ids: Vec<&str> = catalog.iter().map(|s| s.kiro_id).collect();
        for model in ["claude-opus-4.8", "claude-opus-4.7", "claude-opus-4.6", "claude-sonnet-4.6"] {
            assert!(
                ids.contains(&model),
                "白名单模型 {model} 不在 model_catalog.CATALOG 中 —— 白名单与目录已漂移"
            );
        }
    }

    /// ⭐ 镜像初值与 config 默认一致（都 false）：改任一处默认都必须同步另一处，
    /// 否则绕过 main 播种的测试/旁路会读到与 config 矛盾的默认值。
    ///
    /// ⚠️ 必须持锁：同模块 12 个测试会把镜像临时置 true（NativeEffortGuard），
    /// 本测试不持锁就会在那些窗口内随机读到 true 而误红。
    #[test]
    fn native_effort_mirror_matches_config_default() {
        let _lock = NATIVE_EFFORT_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        assert_eq!(
            native_thinking_effort_enabled(),
            crate::model::config::Config::default().native_thinking_effort_enabled,
            "NATIVE_THINKING_EFFORT_ENABLED static 初值必须与 config 默认一致（默认关）"
        );
    }

    /// 开关关闭（默认）：白名单模型 + thinking 启用也不走 native（行为逐字节不变）。
    #[test]
    fn toggle_off_keeps_legacy_behavior() {
        let _g = NativeEffortGuard::with(false);
        let req = mk_req(Some(enabled_thinking(32_000)), None);
        assert_eq!(native_thinking_effort(&req, "claude-opus-4.8"), None);
        assert_eq!(build_additional_model_request_fields(&req, "claude-opus-4.8"), None);
        // XML 前缀照旧注入。
        assert_eq!(
            generate_thinking_prefix_for_model(&req, "claude-opus-4.8"),
            generate_thinking_prefix(&req)
        );
    }

    /// 开关开启 + 白名单 + thinking 启用：budget_tokens 映射选档。
    #[test]
    fn native_effort_selected_from_budget_tokens() {
        let _g = NativeEffortGuard::with(true);
        // 32000 → high
        let req = mk_req(Some(enabled_thinking(32_000)), None);
        assert_eq!(native_thinking_effort(&req, "claude-opus-4.8"), Some("high"));
        // 1000 → low
        let req = mk_req(Some(enabled_thinking(1_000)), None);
        assert_eq!(native_thinking_effort(&req, "claude-opus-4.8"), Some("low"));
        // 100000 → xhigh（5 档表允许）
        let req = mk_req(Some(enabled_thinking(100_000)), None);
        assert_eq!(native_thinking_effort(&req, "claude-opus-4.8"), Some("xhigh"));
    }

    /// 显式 output_config.effort 优先于 budget_tokens 映射。
    #[test]
    fn explicit_output_config_effort_wins() {
        let _g = NativeEffortGuard::with(true);
        // budget 会映射成 high，但显式 effort=low 优先。
        let req = mk_req(
            Some(enabled_thinking(32_000)),
            Some(super::super::types::OutputConfig {
                effort: "low".to_string(),
            }),
        );
        assert_eq!(native_thinking_effort(&req, "claude-opus-4.8"), Some("low"));
    }

    /// adaptive 分支：无 output_config 时默认 high；显式 effort 优先；
    /// 无 xhigh 档模型收到 xhigh 回退 max。
    #[test]
    fn adaptive_thinking_defaults_to_high() {
        let _g = NativeEffortGuard::with(true);
        let adaptive = super::super::types::Thinking {
            thinking_type: "adaptive".to_string(),
            budget_tokens: 0,
        };
        // 无显式 effort → high。
        let req = mk_req(Some(adaptive.clone()), None);
        assert_eq!(native_thinking_effort(&req, "claude-opus-4.8"), Some("high"));
        // 显式 xhigh + 5 档表 → xhigh。
        let req = mk_req(
            Some(adaptive.clone()),
            Some(super::super::types::OutputConfig {
                effort: "xhigh".to_string(),
            }),
        );
        assert_eq!(native_thinking_effort(&req, "claude-opus-4.8"), Some("xhigh"));
        // 显式 xhigh + 无 xhigh 档的 sonnet-4.6 → 回退 max。
        let req = mk_req(
            Some(adaptive.clone()),
            Some(super::super::types::OutputConfig {
                effort: "xhigh".to_string(),
            }),
        );
        assert_eq!(
            native_thinking_effort(&req, "claude-sonnet-4.6"),
            Some("max"),
            "adaptive + xhigh 超出白名单档位应回退 max"
        );
    }

    /// 空 effort 视同未给：enabled thinking + 大 budget 应按 budget 映射（xhigh），
    /// 不被空串归一化出的 high 覆盖（与 requested_native_reasoning 判空口径一致）。
    #[test]
    fn empty_effort_falls_through_to_budget_mapping() {
        let _g = NativeEffortGuard::with(true);
        let req = mk_req(
            Some(enabled_thinking(100_000)),
            Some(super::super::types::OutputConfig {
                effort: "".to_string(),
            }),
        );
        assert_eq!(
            native_thinking_effort(&req, "claude-opus-4.8"),
            Some("xhigh"),
            "空 effort 不应覆盖 budget 映射出的档位"
        );
    }

    /// 白名单档位外 → 回退档位表最后一项（max）。
    #[test]
    fn effort_outside_whitelist_falls_back() {
        let _g = NativeEffortGuard::with(true);
        // sonnet-4.6 无 xhigh 档：budget 映射出 xhigh → 回退 max。
        let req = mk_req(Some(enabled_thinking(100_000)), None);
        assert_eq!(
            native_thinking_effort(&req, "claude-sonnet-4.6"),
            Some("max"),
            "无 xhigh 档的模型收到 xhigh 请求应回退到允许表最后档"
        );
        // 未知 effort 字符串 → normalize 成 high（在表内，直接用）。
        let req = mk_req(
            Some(enabled_thinking(32_000)),
            Some(super::super::types::OutputConfig {
                effort: "ultra".to_string(),
            }),
        );
        assert_eq!(native_thinking_effort(&req, "claude-sonnet-4.6"), Some("high"));
    }

    /// 开关开启但模型不在白名单 → 无 native 字段，XML 照旧（非 native 路径逐字节不变）。
    #[test]
    fn non_whitelist_model_keeps_xml_injection() {
        let _g = NativeEffortGuard::with(true);
        let req = mk_req(Some(enabled_thinking(32_000)), None);
        assert_eq!(native_thinking_effort(&req, "claude-opus-5"), None);
        assert_eq!(native_thinking_effort(&req, "claude-sonnet-4.5"), None);
        assert_eq!(
            generate_thinking_prefix_for_model(&req, "claude-opus-5"),
            generate_thinking_prefix(&req),
            "非白名单模型的 XML 注入必须保持原样"
        );
    }

    /// thinking 显式 disabled → 不出 native 字段（即使给了 output_config.effort）。
    #[test]
    fn disabled_thinking_suppresses_native_path() {
        let _g = NativeEffortGuard::with(true);
        let req = mk_req(
            Some(super::super::types::Thinking {
                thinking_type: "disabled".to_string(),
                budget_tokens: 0,
            }),
            Some(super::super::types::OutputConfig {
                effort: "high".to_string(),
            }),
        );
        assert_eq!(native_thinking_effort(&req, "claude-opus-4.8"), None);
    }

    /// 无 thinking 也无 output_config → 无 native 字段（也没有 XML，两端一致 None）。
    #[test]
    fn no_reasoning_request_yields_nothing() {
        let _g = NativeEffortGuard::with(true);
        let req = mk_req(None, None);
        assert_eq!(native_thinking_effort(&req, "claude-opus-4.8"), None);
        assert_eq!(build_additional_model_request_fields(&req, "claude-opus-4.8"), None);
        assert_eq!(generate_thinking_prefix_for_model(&req, "claude-opus-4.8"), None);
    }

    /// 只给 output_config.effort（无 thinking 块）也走 native：实测的
    /// `/effort xhigh` 最小形态就是 `{output_config:{effort:xhigh}}`。
    #[test]
    fn bare_output_config_effort_triggers_native() {
        let _g = NativeEffortGuard::with(true);
        let req = mk_req(
            None,
            Some(super::super::types::OutputConfig {
                effort: "XHIGH".to_string(),
            }),
        );
        let fields = build_additional_model_request_fields(&req, "claude-opus-4.8")
            .expect("白名单 + 显式 effort 应产出 native 字段");
        assert_eq!(
            fields.output_config.expect("effort 字段应存在").effort,
            "xhigh",
            "显式 effort 应归一化后写入"
        );
    }

    /// 端到端：convert_request 产出的字段能序列化进 KiroRequest 顶层 JSON（wire 形状）。
    #[test]
    fn convert_request_carries_native_fields_into_wire_json() {
        let _g = NativeEffortGuard::with(true);
        let req = mk_req(Some(enabled_thinking(100_000)), None);
        let conversion = convert_request(&req).expect("转换应成功");
        let fields = conversion
            .additional_model_request_fields
            .expect("白名单 + thinking 应产出 native 字段");
        let kiro_request = crate::kiro::model::requests::kiro::KiroRequest {
            conversation_state: conversion.conversation_state,
            profile_arn: None,
            additional_model_request_fields: Some(fields),
        };
        let v = serde_json::to_value(&kiro_request).unwrap();
        assert_eq!(
            v["additionalModelRequestFields"]["output_config"]["effort"],
            "xhigh",
            "wire 形状必须为顶层 additionalModelRequestFields.output_config.effort"
        );
    }

    /// 开关关（默认）：同一请求零 native 字段（与旧版逐字节一致）。
    #[test]
    fn toggle_off_produces_no_native_fields_in_wire() {
        let _g = NativeEffortGuard::with(false);
        let req = mk_req(Some(enabled_thinking(100_000)), None);
        let conversion = convert_request(&req).expect("转换应成功");
        assert!(
            conversion.additional_model_request_fields.is_none(),
            "开关关时不得产出 native 字段"
        );
    }
