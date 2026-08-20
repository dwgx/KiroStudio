    use super::*;
    use super::schema_normalize::{
        extract_schema_defs, resolve_schema_refs, SchemaRefBudget, MAX_SCHEMA_NODES,
        normalize_json_schema_with_node_budget,
    };
    use super::tool_compat::{shorten_tool_name, TOOL_NAME_MAX_LEN};

    #[test]
    fn test_map_model_sonnet() {
        assert!(
            map_model("claude-sonnet-4-20250514")
                .unwrap()
                .contains("sonnet")
        );
        assert!(
            map_model("claude-3-5-sonnet-20241022")
                .unwrap()
                .contains("sonnet")
        );
    }

    /// 短板3：按字符边界截断，多字节不被切坏；max==0 = 不截断。
    #[test]
    fn test_truncate_chars_boundary_safe() {
        // 5 个中文（各 3 字节）→ 截到 3 字符应得前 3 个字，且是合法 UTF-8（未切多字节）。
        let s = "你好世界啊";
        let out = truncate_chars(s, 3);
        assert_eq!(out, "你好世");
        assert_eq!(out.chars().count(), 3);
        // 短于上限 → 原样。
        assert_eq!(truncate_chars("abc", 10), "abc");
        // max==0 → 不截断（原样返回）。
        assert_eq!(truncate_chars(s, 0), s);
    }

    /// 短板3：schema 内嵌上限恒为顶层的 1/5（保持既有 10000→2000 比例），0 时同样为 0。
    #[test]
    fn test_schema_desc_ratio_derives_from_top() {
        // 不改全局镜像（并行测试污染风险），只验证默认镜像值下的派生比例。
        let top = tool_description_max_chars();
        let schema = schema_description_max_chars();
        if top == 0 {
            assert_eq!(schema, 0);
        } else {
            assert_eq!(schema, (top / 5).max(1));
        }
        // 默认镜像应为 10000（与 config 默认一致）→ schema 2000。
        assert_eq!(top, 10000);
        assert_eq!(schema, 2000);
    }

    /// T1 回归：带每请求漂移 cc_version/cch 的归因头，归一化后 system 转发字节应相同。
    #[test]
    fn test_billing_header_canonicalized_in_forwarded_system() {
        use super::super::types::{Message as AnthropicMessage, SystemMessage};

        let mk_req = |header: &str| MessagesRequest {
            model: "claude-sonnet-4".to_string(),
            max_tokens: 1024,
            messages: vec![AnthropicMessage {
                role: "user".to_string(),
                content: serde_json::json!("hello"),
            }],
            stream: false,
            system: Some(vec![
                SystemMessage {
                    text: header.to_string(),
                    block_type: Some("text".to_string()),
                    cache_control: None,
                },
                SystemMessage {
                    text: "You are a helpful assistant.".to_string(),
                    block_type: Some("text".to_string()),
                    cache_control: None,
                },
            ]),
            tools: None,
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: None,
        };

        // 两个请求的归因头 cc_version / cch 不同（每请求漂移）
        let req_a = mk_req("x-anthropic-billing-header: cc_version=1.0.0;cch=aaaaaaaa");
        let req_b = mk_req("x-anthropic-billing-header: cc_version=2.5.9;cch=zzzzzzzz");

        // build_history 的第一条 user 消息即拼接后的 system 内容
        let extract_system = |req: &MessagesRequest| -> String {
            let history =
                build_history(req, &req.messages, "claude-sonnet-4.5", &mut HashMap::new())
                    .unwrap();
            match &history[0] {
                Message::User(u) => u.user_input_message.content.clone(),
                _ => panic!("首条历史应为 system 对应的 user 消息"),
            }
        };

        let sys_a = extract_system(&req_a);
        let sys_b = extract_system(&req_b);

        // 归一化后前缀稳定：两个请求转发给上游的 system 字节完全相同
        assert_eq!(sys_a, sys_b, "归因头归一化后 system 转发字节应一致");
        // 占位符出现在最前端，漂移字段不再泄漏到转发字节里
        assert!(sys_a.starts_with(BILLING_HEADER_PLACEHOLDER));
        assert!(!sys_a.contains("cc_version"));
        assert!(sys_a.contains("You are a helpful assistant."));
    }

    #[test]
    fn test_billing_header_non_matching_untouched() {
        // 保守性：非归因头开头的 system 内容不应被改动
        assert_eq!(
            canonicalize_billing_header("You are a helpful assistant."),
            "You are a helpful assistant."
        );
        assert_eq!(
            canonicalize_billing_header("x-anthropic-billing-header: cc_version=1;cch=x"),
            BILLING_HEADER_PLACEHOLDER
        );
    }

    // ===== 环境噪音剥离 prompt_filter =====

    /// 环境噪音开关是进程级全局，测试并行会相互污染。用一把静态锁串行所有触碰该开关的
    /// 用例，并在守卫里恢复原值，既消除竞态又不影响其它测试。
    static ENV_NOISE_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct EnvNoiseGuard {
        prev: bool,
        _lock: std::sync::MutexGuard<'static, ()>,
    }
    impl EnvNoiseGuard {
        fn with(enabled: bool) -> Self {
            let lock = ENV_NOISE_TEST_LOCK
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let prev = strip_env_noise_enabled();
            set_strip_env_noise(enabled);
            EnvNoiseGuard { prev, _lock: lock }
        }
        fn enable() -> Self {
            Self::with(true)
        }
    }
    impl Drop for EnvNoiseGuard {
        fn drop(&mut self) {
            set_strip_env_noise(self.prev);
        }
    }

    #[test]
    fn test_strip_env_noise_removes_env_block() {
        let _g = EnvNoiseGuard::enable();
        // <env> 块整块剥离，稳定正文保留
        let text = "You are a helpful assistant.\n<env>\nWorking directory: /home/a\nPlatform: linux\nToday's date: 2026-07-09\n</env>\nFollow the task.";
        let out = canonicalize_system_text(text);
        assert!(!out.contains("<env>"), "env 起始标签应被剥离");
        assert!(!out.contains("Working directory"), "cwd 行应被剥离");
        assert!(!out.contains("Today's date"), "日期行应被剥离");
        assert!(
            out.contains("You are a helpful assistant."),
            "稳定正文应保留"
        );
        assert!(out.contains("Follow the task."), "env 后正文应保留");
    }

    #[test]
    fn test_strip_env_noise_removes_git_and_model_lines() {
        let _g = EnvNoiseGuard::enable();
        let text = "System prompt body.\ngitStatus: main clean\nRecent commits: abc123 fix\nYou are powered by the model named Claude.\nKeep going.";
        let out = canonicalize_system_text(text);
        assert!(!out.contains("gitStatus:"));
        assert!(!out.contains("Recent commits:"));
        assert!(!out.contains("powered by the model named"));
        assert!(out.contains("System prompt body."));
        assert!(out.contains("Keep going."));
    }

    #[test]
    fn test_strip_env_noise_removes_environment_section() {
        let _g = EnvNoiseGuard::enable();
        // # Environment 段剥到下一个 # 标题为止，后续标题及正文保留
        let text =
            "# Task\nDo the work.\n# Environment\nfoo\nbar\ngitStatus: x\n# Rules\nBe concise.";
        let out = canonicalize_system_text(text);
        assert!(out.contains("# Task"));
        assert!(out.contains("Do the work."));
        assert!(!out.contains("# Environment"));
        assert!(!out.contains("foo"));
        assert!(!out.contains("bar"));
        assert!(out.contains("# Rules"), "环境段后的新标题应保留");
        assert!(out.contains("Be concise."));
    }

    #[test]
    fn test_strip_env_noise_stable_content_untouched() {
        let _g = EnvNoiseGuard::enable();
        // 纯稳定正文：无任何噪音标记 → 原样借用不改写
        let text =
            "You are an expert engineer.\nWrite clean, tested code.\nExplain your reasoning.";
        let out = canonicalize_system_text(text);
        assert_eq!(out.as_ref(), text, "稳定正文一字节不改");
        assert!(
            matches!(out, std::borrow::Cow::Borrowed(_)),
            "未改写应零分配借用"
        );
    }

    #[test]
    fn test_strip_env_noise_disabled_keeps_noise() {
        // 开关关闭时不剥离环境噪音（但归因头折叠仍无条件生效）
        let _g = EnvNoiseGuard::with(false);
        let text = "Body.\ngitStatus: main\n<env>\ncwd\n</env>";
        let out = canonicalize_system_text(text);
        assert_eq!(out.as_ref(), text, "关闭时环境噪音应原样保留");
    }

    /// 转发字节路径（canonicalize_system_text）正确剥离环境噪音。
    /// （原先还与影子指纹路径 cache_tracker 做一致性比对；影子缓存记账已整体移除，
    ///   此处只保留转发路径本身的归一化回归。）
    #[test]
    fn test_forward_canonicalization_strips_env_noise() {
        let _g = EnvNoiseGuard::enable();
        let raw = "You are a helpful assistant.\n<env>\nWorking directory: /x\nPlatform: linux\n</env>\ngitStatus: clean\nDo the task.";

        let forwarded = canonicalize_system_text(raw).into_owned();

        assert!(!forwarded.contains("Working directory"));
        assert!(!forwarded.contains("gitStatus:"));
        assert!(forwarded.contains("You are a helpful assistant."));
        assert!(forwarded.contains("Do the task."));
    }

    #[test]
    fn test_env_noise_drift_produces_identical_forwarded_system() {
        use super::super::types::{Message as AnthropicMessage, SystemMessage};
        let _g = EnvNoiseGuard::enable();

        // 两次请求：env 块里的 cwd/日期漂移，稳定正文相同
        let mk = |env_line: &str| MessagesRequest {
            model: "claude-sonnet-4".to_string(),
            max_tokens: 1024,
            messages: vec![AnthropicMessage {
                role: "user".to_string(),
                content: serde_json::json!("hi"),
            }],
            stream: false,
            system: Some(vec![SystemMessage {
                text: format!(
                    "You are Claude Code.\n<env>\n{}\nPlatform: win32\n</env>\nHelp the user.",
                    env_line
                ),
                block_type: Some("text".to_string()),
                cache_control: None,
            }]),
            tools: None,
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: None,
        };
        let req_a = mk("Working directory: /home/a  (2026-07-08)");
        let req_b = mk("Working directory: /home/b  (2026-07-09)");

        let extract = |req: &MessagesRequest| -> String {
            let history =
                build_history(req, &req.messages, "claude-sonnet-4.5", &mut HashMap::new())
                    .unwrap();
            match &history[0] {
                Message::User(u) => u.user_input_message.content.clone(),
                _ => panic!("首条历史应为 system 对应的 user 消息"),
            }
        };
        let sys_a = extract(&req_a);
        let sys_b = extract(&req_b);

        assert_eq!(sys_a, sys_b, "env 漂移剥离后转发字节应一致");
        assert!(
            !sys_a.contains("Working directory"),
            "漂移的 cwd 不应泄漏到转发字节"
        );
        assert!(sys_a.contains("Help the user."), "稳定正文应保留");
    }

    /// 构造只有一条 user 消息的最小请求，system/thinking 可控。
    fn mk_thinking_req(
        system: Option<Vec<super::super::types::SystemMessage>>,
        thinking: Option<super::super::types::Thinking>,
    ) -> MessagesRequest {
        use super::super::types::Message as AnthropicMessage;
        MessagesRequest {
            model: "claude-sonnet-4".to_string(),
            max_tokens: 1024,
            messages: vec![AnthropicMessage {
                role: "user".to_string(),
                content: serde_json::json!("hi"),
            }],
            stream: false,
            system,
            tools: None,
            tool_choice: None,
            thinking,
            output_config: None,
            metadata: None,
        }
    }

    /// 构造只控制「工作上下文」（system 文本 + 工具名）的最小请求，供 L0-5 派生用例使用。
    /// 默认带一条 `user`/`"hi"`，调用方可改 `messages` 测首条文本对派生键的影响。
    fn req_with_context(system: Option<&str>, tool_names: &[&str]) -> MessagesRequest {
        use super::super::types::Message as AnthropicMessage;
        use super::super::types::{SystemMessage, Tool};
        MessagesRequest {
            model: "claude-sonnet-4".to_string(),
            max_tokens: 1024,
            messages: vec![AnthropicMessage {
                role: "user".to_string(),
                content: serde_json::json!("hi"),
            }],
            stream: false,
            system: system.map(|t| {
                vec![SystemMessage {
                    text: t.to_string(),
                    block_type: Some("text".to_string()),
                    cache_control: None,
                }]
            }),
            tools: if tool_names.is_empty() {
                None
            } else {
                Some(
                    tool_names
                        .iter()
                        .map(|n| Tool {
                            tool_type: None,
                            name: n.to_string(),
                            description: String::new(),
                            input_schema: HashMap::new(),
                            cache_control: None,
                            max_uses: None,
                        })
                        .collect(),
                )
            },
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: None,
        }
    }

    fn first_history_user_content(req: &MessagesRequest) -> Option<String> {
        let history =
            build_history(req, &req.messages, "claude-sonnet-4.5", &mut HashMap::new()).unwrap();
        match history.first() {
            Some(Message::User(u)) => Some(u.user_input_message.content.clone()),
            _ => None,
        }
    }

    /// 回归：`"system": ""` 经 types.rs 的 visit_str 变成 `Some(vec![{text:""}])`，
    /// 归一化后为空。旧代码外层 `if let Some(system)` 已匹配、内层 is_empty 跳过，
    /// 控制流到不了 else 分支 → thinking 前缀被静默丢弃。修复后必须仍注入。
    #[test]
    fn should_inject_thinking_prefix_when_system_is_empty_string() {
        use super::super::types::SystemMessage;

        let req = mk_thinking_req(
            Some(vec![SystemMessage {
                text: String::new(),
                block_type: Some("text".to_string()),
                cache_control: None,
            }]),
            Some(super::super::types::Thinking {
                thinking_type: "enabled".to_string(),
                budget_tokens: 8192,
            }),
        );

        let content = first_history_user_content(&req)
            .expect("system 空但 thinking 开启时，首条历史应为注入 thinking 前缀的 user 消息");
        assert!(
            has_thinking_tags(&content),
            "thinking 前缀必须注入，实际内容：{content}"
        );
        assert!(content.contains("<max_thinking_length>8192</max_thinking_length>"));
        // 无有效 system 文本时不应附带分块策略（保持与 system=None 路径一致）
        assert!(!content.contains(SYSTEM_CHUNKED_POLICY));
    }

    /// 回归：system 整块是环境噪音，剥离后为空 —— 同一条控制流缺陷的第二条触发路径。
    #[test]
    fn should_inject_thinking_prefix_when_system_stripped_to_empty_by_env_noise() {
        use super::super::types::SystemMessage;
        let _g = EnvNoiseGuard::enable();

        let req = mk_thinking_req(
            Some(vec![SystemMessage {
                text: "<env>\nWorking directory: /home/a\nPlatform: linux\n</env>".to_string(),
                block_type: Some("text".to_string()),
                cache_control: None,
            }]),
            Some(super::super::types::Thinking {
                thinking_type: "adaptive".to_string(),
                budget_tokens: 0,
            }),
        );

        let content = first_history_user_content(&req)
            .expect("system 被剥空但 thinking 开启时，首条历史应为 thinking 前缀");
        assert!(has_thinking_tags(&content), "实际内容：{content}");
        assert!(content.contains("<thinking_effort>high</thinking_effort>"));
    }

    /// 正常路径不变：有有效 system + thinking → 前缀在最前，system 正文与分块策略都在。
    #[test]
    fn derived_conversation_id_is_stable_across_requests() {
        // 同一工作上下文（system + tools + 默认首条 "hi"）必须派生出同一个键。
        let a = req_with_context(Some("you are a helpful agent"), &["read", "write"]);
        let b = req_with_context(Some("you are a helpful agent"), &["read", "write"]);
        let ka = derive_conversation_id_from_context(&a).expect("应能派生");
        let kb = derive_conversation_id_from_context(&b).expect("应能派生");
        assert_eq!(ka, kb, "同上下文必须稳定派生同一键，否则等于没修");
        assert!(is_valid_uuid(&ka), "必须是 UUID 形状：下游与上游都按此消费");
    }

    #[test]
    fn derived_conversation_id_ignores_tool_order() {
        // 官方自认造过「工具排序非确定」的事故；不排序会让同上下文分裂成多个键。
        let a = req_with_context(Some("sys"), &["alpha", "beta", "gamma"]);
        let b = req_with_context(Some("sys"), &["gamma", "alpha", "beta"]);
        assert_eq!(
            derive_conversation_id_from_context(&a),
            derive_conversation_id_from_context(&b),
            "工具名顺序抖动不得改变派生键"
        );
    }

    #[test]
    fn derived_conversation_id_separates_distinct_contexts() {
        let a = req_with_context(Some("agent A"), &["read"]);
        let b = req_with_context(Some("agent B"), &["read"]);
        assert_ne!(
            derive_conversation_id_from_context(&a),
            derive_conversation_id_from_context(&b),
            "不同工作上下文必须隔离，否则无关请求会互相污染上游会话"
        );
    }

    #[test]
    fn derived_conversation_id_resists_concat_ambiguity() {
        // 无分隔符时 ["ab","c"] 与 ["a","bc"] 会哈希成同一串。
        let a = req_with_context(None, &["ab", "c"]);
        let b = req_with_context(None, &["a", "bc"]);
        assert_ne!(
            derive_conversation_id_from_context(&a),
            derive_conversation_id_from_context(&b),
            "拼接歧义必须由分隔符消除"
        );
    }

    #[test]
    fn derived_conversation_id_is_none_without_material() {
        // system 与 tools 双空：没有可稳定的前缀，应回落随机而非归到同一个键。
        // 夹具自带首条 "hi"；双空仍必须 None（不得凭第一行文本绑死裸 curl）。
        let empty = req_with_context(None, &[]);
        assert!(
            derive_conversation_id_from_context(&empty).is_none(),
            "无材料时必须返回 None，让调用方回落随机 UUID"
        );
        let blank = req_with_context(Some("   "), &[]);
        assert!(
            derive_conversation_id_from_context(&blank).is_none(),
            "纯空白 system 不算材料"
        );
    }

    #[test]
    fn derived_conversation_id_survives_env_noise_drift() {
        // 关键回归：工作目录/日期漂移不得打散键。不复用 canonicalize_system_text
        // 就会在这里失败，而那等于 L0-5 没修。
        let a = req_with_context(
            Some("stable instructions\n<env>cwd: /home/a\ntoday: 2026-08-04</env>"),
            &["read"],
        );
        let b = req_with_context(
            Some("stable instructions\n<env>cwd: /home/b\ntoday: 2026-08-05</env>"),
            &["read"],
        );
        assert_eq!(
            derive_conversation_id_from_context(&a),
            derive_conversation_id_from_context(&b),
            "环境噪音漂移必须被归一化吸收"
        );
    }

    #[test]
    fn derive_conversation_id_separates_distinct_first_user_text() {
        // 同 system/tools、不同首条可见文本 → 必须是两个键（折叠会把流量钉在同一号）。
        let mut a = req_with_context(Some("sys"), &["read"]);
        let mut b = req_with_context(Some("sys"), &["read"]);
        a.messages[0].content = serde_json::json!("session alpha first line");
        b.messages[0].content = serde_json::json!("session beta first line");
        assert_ne!(
            derive_conversation_id_from_context(&a),
            derive_conversation_id_from_context(&b),
            "不同首条文本必须隔离"
        );
    }

    #[test]
    fn derive_conversation_id_ignores_later_messages() {
        // 同会话后续轮只追加历史，下标 0 不变 → 键必须稳定。
        use super::super::types::Message as AnthropicMessage;
        let mut a = req_with_context(Some("sys"), &["read"]);
        a.messages[0].content = serde_json::json!("stable first");
        let mut b = a.clone();
        b.messages.push(AnthropicMessage {
            role: "assistant".to_string(),
            content: serde_json::json!("ack"),
        });
        b.messages.push(AnthropicMessage {
            role: "user".to_string(),
            content: serde_json::json!("follow up that must not change the key"),
        });
        assert_eq!(
            derive_conversation_id_from_context(&a),
            derive_conversation_id_from_context(&b),
            "后续消息不得改变派生键"
        );
    }

    #[test]
    fn derive_conversation_id_hashes_text_blocks_not_image_payload() {
        // 数组 content：只吃顶层 text；image/document 的 base64 不得进 hasher。
        let mut with_image = req_with_context(Some("sys"), &["read"]);
        with_image.messages[0].content = serde_json::json!([
            {
                "type": "image",
                "source": {
                    "type": "base64",
                    "media_type": "image/png",
                    "data": "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg=="
                }
            },
            { "type": "text", "text": "same visible prompt" },
            {
                "type": "document",
                "source": {
                    "type": "base64",
                    "media_type": "application/pdf",
                    "data": "JVBERi0xLjAKAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
                }
            }
        ]);
        let mut other_image = req_with_context(Some("sys"), &["read"]);
        other_image.messages[0].content = serde_json::json!([
            {
                "type": "image",
                "source": {
                    "type": "base64",
                    "media_type": "image/png",
                    "data": "QUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUE="
                }
            },
            { "type": "text", "text": "same visible prompt" }
        ]);
        let mut text_only = req_with_context(Some("sys"), &["read"]);
        text_only.messages[0].content = serde_json::json!("same visible prompt");
        assert_eq!(
            derive_conversation_id_from_context(&with_image),
            derive_conversation_id_from_context(&other_image),
            "不同附件不得改变派生键"
        );
        assert_eq!(
            derive_conversation_id_from_context(&with_image),
            derive_conversation_id_from_context(&text_only),
            "纯文本与 image+text 在可见文本相同时必须同键"
        );
    }

    #[test]
    fn derive_conversation_id_none_if_empty_context_even_with_first_text() {
        // 双空时先 return，不得因首条有字就派生（裸 curl 不绑号）。
        let mut req = req_with_context(None, &[]);
        req.messages[0].content =
            serde_json::json!("a unique first line that must not bind a credential");
        assert!(
            derive_conversation_id_from_context(&req).is_none(),
            "system+tools 双空时即使首条有文本也必须 None"
        );
    }

    #[test]
    fn derive_conversation_id_first_message_hashed_only_after_material_gate() {
        // 源码守卫：生产区必须先因无材料退出，再碰下标 0 的文本。
        // needle 运行时拼接，避免 include_str 把本测试字面量算进匹配。
        let full = include_str!("converter.rs");
        let production = full
            .split("#[cfg(test)]")
            .next()
            .expect("生产区应在测试模块之前");
        // 切片覆盖取文本 helper + 派生函数（Display 若藏在 helper 里也要红）。
        let helper_needle = ["fn first_message_text", "_for_hash"].concat();
        let start = production
            .find(&helper_needle)
            .expect("缺少首条文本 helper");
        let chunk = &production[start..];
        let next_fn = chunk
            .find("\nfn collect_history_tool_names")
            .expect("派生函数后应是 collect_history_tool_names");
        let body = &chunk[..next_fn];

        let none_needle = ["return ", "None"].concat();
        let first_needle = ["messages", ".first()"].concat();
        let none_pos = body.find(&none_needle).expect("必须有无材料早退");
        let first_pos = body.find(&first_needle).expect("有材料后必须哈希首条");
        assert!(
            none_pos < first_pos,
            "无材料早退必须发生在哈希首条之前，否则裸 curl 会按第一行绑号"
        );

        let prefix_needle = ["derived", "-conversation:"].concat();
        assert!(
            body.contains(&prefix_needle),
            "前缀不得改成参考仓的 fallback-conversation"
        );
        let display_needle = ["first.content", ".to_string()"].concat();
        assert!(
            !body.contains(&display_needle),
            "不得把整段 content Display 进 hasher"
        );
        let json_needle = ["to_string(", "&first.content"].concat();
        assert!(
            !body.contains(&json_needle),
            "不得序列化整段 content 进 hasher"
        );
    }

    #[test]
    fn explicit_session_id_wins_over_derivation() {
        // 回落顺序不能反：Claude Code 给了 session_id 就必须用它。
        let mut req = req_with_context(Some("sys"), &["read"]);
        let sid = "11111111-2222-3333-4444-555555555555";
        req.metadata = Some(super::super::types::Metadata {
            user_id: Some(format!("user_x_session_{sid}")),
        });
        let result = convert_request(&req).expect("转换应成功");
        let derived = derive_conversation_id_from_context(&req).expect("应能派生");
        assert_ne!(
            result.conversation_state.conversation_id, derived,
            "显式 session_id 优先于上下文派生"
        );
        assert_eq!(result.conversation_state.conversation_id, sid);
    }

    #[test]
    fn should_keep_thinking_prefix_ahead_of_non_empty_system() {
        use super::super::types::SystemMessage;

        let req = mk_thinking_req(
            Some(vec![SystemMessage {
                text: "You are a helpful assistant.".to_string(),
                block_type: Some("text".to_string()),
                cache_control: None,
            }]),
            Some(super::super::types::Thinking {
                thinking_type: "enabled".to_string(),
                budget_tokens: 1024,
            }),
        );

        let content = first_history_user_content(&req).expect("首条历史应为 system 对应的 user");
        assert!(content.starts_with("<thinking_mode>enabled</thinking_mode>"));
        assert!(content.contains("You are a helpful assistant."));
        assert!(content.contains(SYSTEM_CHUNKED_POLICY));
    }

    /// 两者都无时不插入 system 配对（首条历史不再是 system 伪装的 user）。
    #[test]
    fn should_not_inject_system_pair_when_system_empty_and_thinking_off() {
        use super::super::types::SystemMessage;

        let req = mk_thinking_req(
            Some(vec![SystemMessage {
                text: String::new(),
                block_type: Some("text".to_string()),
                cache_control: None,
            }]),
            None,
        );

        let history = build_history(
            &req,
            &req.messages,
            "claude-sonnet-4.5",
            &mut HashMap::new(),
        )
        .unwrap();
        // 只有一条 user 消息 → 作为 currentMessage 不入历史，历史应为空
        assert!(
            history.is_empty(),
            "无 system 无 thinking 时不应插入任何历史"
        );
    }

    #[test]
    fn test_map_model_sonnet_variants() {
        assert!(
            map_model("claude-3-5-sonnet-20241022")
                .unwrap()
                .contains("sonnet")
        );
    }

    #[test]
    fn test_map_model_opus() {
        assert!(
            map_model("claude-opus-4-20250514")
                .unwrap()
                .contains("opus")
        );
    }

    #[test]
    fn test_map_model_haiku() {
        assert!(
            map_model("claude-haiku-4-20250514")
                .unwrap()
                .contains("haiku")
        );
    }

    #[test]
    fn test_map_model_unsupported() {
        assert!(map_model("gpt-4").is_none());
        // 仍不支持的：gemini / 未知
        assert!(map_model("gemini-2.0").is_none());
    }

    #[test]
    fn test_map_model_national() {
        // 模糊名 → 规范 kiro modelId
        assert_eq!(map_model("deepseek"), Some("deepseek-3.2".to_string()));
        assert_eq!(map_model("glm"), Some("glm-5".to_string()));
        assert_eq!(map_model("qwen"), Some("qwen3-coder-next".to_string()));
        assert_eq!(map_model("minimax"), Some("minimax-m2.5".to_string()));
        // 完整原生 id 直透（含子串，映射回自身）
        assert_eq!(map_model("deepseek-3.2"), Some("deepseek-3.2".to_string()));
        assert_eq!(map_model("glm-5"), Some("glm-5".to_string()));
        assert_eq!(
            map_model("qwen3-coder-next"),
            Some("qwen3-coder-next".to_string())
        );
        assert_eq!(map_model("minimax-m2.5"), Some("minimax-m2.5".to_string()));
        // minimax 版本细分
        assert_eq!(map_model("minimax-m2.1"), Some("minimax-m2.1".to_string()));
        // 大小写不敏感
        assert_eq!(map_model("DeepSeek"), Some("deepseek-3.2".to_string()));
        // 国产模型窗口 = 200k（非 1M）
        assert_eq!(get_context_window_size("deepseek-3.2"), 128_000); // 官方 128K
        assert_eq!(get_context_window_size("glm-5"), 200_000);
    }

    #[test]
    fn test_map_model_thinking_suffix_sonnet() {
        // thinking 后缀不应影响 sonnet 模型映射
        let result = map_model("claude-sonnet-4-5-20250929-thinking");
        assert_eq!(result, Some("claude-sonnet-4.5".to_string()));
    }

    #[test]
    fn test_map_model_thinking_suffix_opus_4_5() {
        // thinking 后缀不应影响 opus 4.5 模型映射
        let result = map_model("claude-opus-4-5-20251101-thinking");
        assert_eq!(result, Some("claude-opus-4.5".to_string()));
    }

    #[test]
    fn test_map_model_thinking_suffix_opus_4_6() {
        // thinking 后缀不应影响 opus 4.6 模型映射
        let result = map_model("claude-opus-4-6-thinking");
        assert_eq!(result, Some("claude-opus-4.6".to_string()));
    }

    #[test]
    fn test_map_model_opus_4_8() {
        assert_eq!(
            map_model("claude-opus-4-8"),
            Some("claude-opus-4.8".to_string())
        );
        assert_eq!(
            map_model("claude-opus-4-8-thinking"),
            Some("claude-opus-4.8".to_string())
        );
        assert_eq!(get_context_window_size("claude-opus-4-8"), 1_000_000);
    }

    #[test]
    fn test_map_model_thinking_suffix_haiku() {
        // thinking 后缀不应影响 haiku 模型映射
        let result = map_model("claude-haiku-4-5-20251001-thinking");
        assert_eq!(result, Some("claude-haiku-4.5".to_string()));
    }

    #[test]
    fn test_determine_chat_trigger_type() {
        // 无工具时返回 MANUAL
        let req = MessagesRequest {
            model: "claude-sonnet-4".to_string(),
            max_tokens: 1024,
            messages: vec![],
            stream: false,
            system: None,
            tools: None,
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: None,
        };
        assert_eq!(determine_chat_trigger_type(&req), "MANUAL");
    }

    #[test]
    fn test_collect_history_tool_names() {
        use crate::kiro::model::requests::tool::ToolUseEntry;

        // 创建包含工具使用的历史消息
        let mut assistant_msg = AssistantMessage::new("I'll read the file.");
        assistant_msg = assistant_msg.with_tool_uses(vec![
            ToolUseEntry::new("tool-1", "read")
                .with_input(serde_json::json!({"path": "/test.txt"})),
            ToolUseEntry::new("tool-2", "write")
                .with_input(serde_json::json!({"path": "/out.txt"})),
        ]);

        let history = vec![
            Message::User(HistoryUserMessage::new(
                "Read the file",
                "claude-sonnet-4.5",
            )),
            Message::Assistant(HistoryAssistantMessage {
                assistant_response_message: assistant_msg,
            }),
        ];

        let tool_names = collect_history_tool_names(&history);
        assert_eq!(tool_names.len(), 2);
        assert!(tool_names.contains(&"read".to_string()));
        assert!(tool_names.contains(&"write".to_string()));
    }

    #[test]
    fn test_create_placeholder_tool() {
        let tool = create_placeholder_tool("my_custom_tool");

        assert_eq!(tool.tool_specification.name, "my_custom_tool");
        assert!(!tool.tool_specification.description.is_empty());

        // 验证 JSON 序列化正确
        let json = serde_json::to_string(&tool).unwrap();
        assert!(json.contains("\"name\":\"my_custom_tool\""));
    }

    #[test]
    fn test_shorten_tool_name_deterministic() {
        let long_name =
            "mcp__some_very_long_server_name__some_very_long_tool_name_that_exceeds_limit";
        assert!(long_name.len() > TOOL_NAME_MAX_LEN);

        let short1 = shorten_tool_name(long_name);
        let short2 = shorten_tool_name(long_name);
        assert_eq!(short1, short2, "相同输入应产生相同的短名称");
        assert!(
            short1.len() <= TOOL_NAME_MAX_LEN,
            "短名称长度应 <= 63，实际 {}",
            short1.len()
        );
    }

    #[test]
    fn test_map_tool_name_cjk_never_exceeds_limit() {
        // ⭐回归(旧代码必失败):超限判断用字节数、前缀截取用字符数,两者单位不一致。
        // 30 个汉字 = 90 字节 > 63 → 触发缩短;但 char_indices().nth(54) 在只有 30 字符时
        // 返回 None → prefix 取整个名字 → 结果 90+1+8 = 99 字节,**比原名更长且仍超上限**,
        // 上游 Kiro 会回 400 Improperly formed request。
        // 修复后前缀按 chars().take(54) 截取,短名恒为 ASCII 且 ≤63 字节。
        let mut map = HashMap::new();
        for n in [20usize, 22, 30, 40, 60, 100, 200] {
            let cjk_name: String = "工".repeat(n);
            let short = map_tool_name(&cjk_name, &mut map);
            assert!(
                short.len() <= TOOL_NAME_MAX_LEN,
                "{n} 个汉字({} 字节)的工具名缩短后为 {} 字节(>{}上限): {:?}",
                cjk_name.len(),
                short.len(),
                TOOL_NAME_MAX_LEN,
                short
            );
            if cjk_name.len() > TOOL_NAME_MAX_LEN {
                assert!(
                    short.len() < cjk_name.len(),
                    "缩短后必须比原名更短,否则毫无意义(原 {} 字节 → 短 {} 字节)",
                    cjk_name.len(),
                    short.len()
                );
                assert_eq!(
                    map.get(&short).map(String::as_str),
                    Some(cjk_name.as_str()),
                    "必须登记 short→original 映射,否则 stream 层无法还原成客户端原名"
                );
            }
        }
    }

    #[test]
    fn test_map_tool_name_mixed_width_boundary() {
        // 混合宽度(ASCII + CJK)在 63 字节边界附近:凡触发缩短的,结果都必须 ≤63 字节。
        let mut map = HashMap::new();
        for ascii_len in 0..8usize {
            for cjk_len in 18..26usize {
                let name = format!("{}{}", "a".repeat(ascii_len), "文".repeat(cjk_len));
                let short = map_tool_name(&name, &mut map);
                assert!(
                    short.len() <= TOOL_NAME_MAX_LEN,
                    "name({} 字节) → short({} 字节) 超限",
                    name.len(),
                    short.len()
                );
                // 未超限的名字必须原样返回(不该被无谓改写)。
                if name.len() <= TOOL_NAME_MAX_LEN {
                    assert_eq!(short, name, "未超限的工具名不应被改写");
                }
            }
        }
    }

    #[test]
    fn test_shorten_tool_name_uniqueness() {
        let name_a = "mcp__server_alpha__tool_name_that_is_very_long_and_exceeds_the_limit_a";
        let name_b = "mcp__server_alpha__tool_name_that_is_very_long_and_exceeds_the_limit_b";
        let short_a = shorten_tool_name(name_a);
        let short_b = shorten_tool_name(name_b);
        assert_ne!(short_a, short_b, "不同输入应产生不同的短名称");
    }

    #[test]
    fn test_map_tool_name_short_passthrough() {
        let mut map = HashMap::new();
        let result = map_tool_name("short_name", &mut map);
        assert_eq!(result, "short_name");
        assert!(map.is_empty(), "短名称不应产生映射");
    }

    #[test]
    fn test_map_tool_name_long_creates_mapping() {
        let mut map = HashMap::new();
        let long_name = "mcp__plugin_very_long_server_name__extremely_long_tool_name_exceeds_63";
        let result = map_tool_name(long_name, &mut map);
        assert!(result.len() <= TOOL_NAME_MAX_LEN);
        assert_eq!(map.get(&result), Some(&long_name.to_string()));
    }

    #[test]
    fn test_tool_name_mapping_in_convert_request() {
        use super::super::types::{Message as AnthropicMessage, Tool as AnthropicTool};

        let long_tool_name =
            "mcp__plugin_very_long_server_name__extremely_long_tool_name_exceeds_63";
        assert!(long_tool_name.len() > TOOL_NAME_MAX_LEN);

        let mut schema = std::collections::HashMap::new();
        schema.insert("type".to_string(), serde_json::json!("object"));
        schema.insert("properties".to_string(), serde_json::json!({}));

        let req = MessagesRequest {
            model: "claude-sonnet-4".to_string(),
            max_tokens: 1024,
            messages: vec![AnthropicMessage {
                role: "user".to_string(),
                content: serde_json::json!("test"),
            }],
            system: None,
            stream: false,
            tools: Some(vec![AnthropicTool {
                name: long_tool_name.to_string(),
                description: "A test tool".to_string(),
                input_schema: schema,
                tool_type: None,
                max_uses: None,
                cache_control: None,
            }]),
            thinking: None,
            tool_choice: None,
            output_config: None,
            metadata: None,
        };

        let result = convert_request(&req).unwrap();

        // 应该有映射
        assert_eq!(result.tool_name_map.len(), 1);

        // 映射中的值应该是原始名称
        let (short, original) = result.tool_name_map.iter().next().unwrap();
        assert_eq!(original, long_tool_name);
        assert!(short.len() <= TOOL_NAME_MAX_LEN);

        // Kiro 请求中的工具名应该是短名称
        let tools = &result
            .conversation_state
            .current_message
            .user_input_message
            .user_input_message_context
            .tools;
        assert_eq!(tools[0].tool_specification.name, *short);
    }

    /// Claude Code 2.1.215+ 的 ToolSearch 延迟加载产生 `type=tool_reference` 块
    /// （只有 tool_name，没有 text）。system 数组里混入 → 反序列化必须容忍
    /// （旧代码 text 必填，整请求 400）；content 里混入 → 转换静默跳过，
    /// 不报错、从转发内容移除。
    #[test]
    fn test_tool_reference_blocks_tolerated_and_skipped() {
        let json = serde_json::json!({
            "model": "claude-sonnet-4",
            "max_tokens": 1024,
            "stream": false,
            "system": [
                {"type": "text", "text": "you are helpful"},
                {"type": "tool_reference", "tool_name": "mcp__server__tool"}
            ],
            "messages": [
                {"role": "user", "content": [
                    {"type": "text", "text": "hi"},
                    {"type": "tool_reference", "tool_name": "mcp__server__tool2"}
                ]}
            ]
        });
        let req: MessagesRequest = serde_json::from_value(json)
            .expect("system 数组含 tool_reference 块不得反序列化失败（旧代码在此 400）");

        // tool_reference 块被容忍：text 缺省为空串，text 块原样保留。
        let system = req.system.as_deref().expect("system 应反序列化成功");
        assert_eq!(system.len(), 2, "容忍而非丢弃：tool_reference 块仍在数组里");
        assert_eq!(system[0].text, "you are helpful");
        assert_eq!(system[1].text, "", "tool_reference 块无 text，容忍为空串");

        let result = convert_request(&req).expect("含 tool_reference 块的请求转换不得报错");

        // 转发内容：文本块保留，tool_reference 静默跳过（无残留）。
        let content = &result
            .conversation_state
            .current_message
            .user_input_message
            .content;
        assert!(
            content.contains("hi"),
            "text 块文本必须保留: {content:?}"
        );
        assert!(
            !content.contains("tool_reference") && !content.contains("mcp__server"),
            "tool_reference 不得泄漏进转发内容: {content:?}"
        );
    }

    #[test]
    fn test_convert_tools_strips_web_search_in_mixed_list() {
        let _g = ToolCompatGuard::with(true);
        use super::super::types::Tool as AnthropicTool;

        let mk = |name: &str, ty: Option<&str>| AnthropicTool {
            name: name.to_string(),
            description: String::new(),
            input_schema: std::collections::HashMap::new(),
            tool_type: ty.map(|s| s.to_string()),
            max_uses: None,
            cache_control: None,
        };

        // 🔴 2026-08-09 行为变更：web_search（带 type）在混合列表里**不再剥离**，
        // 而是归一化成 Kiro 认的函数工具形态（`name: web_search` + 内置 schema）。
        // 改前 assert 它被剥离 —— 那是导致 CC WebSearch 静默失效的行为。
        let tools = Some(vec![
            mk("web_search", Some("web_search_20250305")),
            mk("Read", None),
            mk("Write", None),
        ]);
        let mut map = HashMap::new();
        let converted = convert_tools(&tools, &mut map);

        let names: Vec<&str> = converted
            .iter()
            .map(|t| t.tool_specification.name.as_str())
            .collect();
        // 现在应是 3 个：web_search（归一化后）+ read_file + fs_write。
        assert_eq!(names.len(), 3, "web_search 不应被剥离，应归一化保留: {names:?}");
        assert!(names.contains(&"read_file"));
        assert!(names.contains(&"fs_write"));
        assert!(
            names.contains(&"web_search"),
            "web_search 必须归一化为 name=web_search 的函数工具（模型需要能看到搜索能力）"
        );
    }

    /// 🔴 MAJOR-1 守卫（对抗审查，2026-08-15）：type-only 形态（name 缺失、仅
    /// `type: web_search_*`，非官方 server tool 形态）必须被归一化分支改写为
    /// `name: web_search` + 清掉 Anthropic 服务端 type —— 搜索能力保留在 converter
    /// 归一化层（网关入站判定不代答，见 websearch.rs 守卫）。该分支此前零覆盖，
    /// 删掉不会红，本条补上：删分支（name 落空串）即 FAIL。
    #[test]
    fn test_convert_tools_normalizes_type_only_web_search() {
        use super::super::types::Tool as AnthropicTool;

        // is_builtin 分支依赖 tool_compat_mapping 开关（默认开，可经 toolCompatMapping
        // 配置关闭）；测试显式开启以验证「归一化后命中内置 web_search schema」的完整链。
        // 串行锁：与 test_convert_tools_passthrough_when_mapping_disabled（翻转同一原子）
        // 互斥，防止并行执行时读到对方写入的开关值（ENV_NOISE_TEST_LOCK 同款）。
        // std::sync::Mutex 非重入：本测试只能拿一次 ToolCompatGuard（双 with 会自死锁）。
        let _g = ToolCompatGuard::with(true);

        let mk = |name: &str, ty: Option<&str>| AnthropicTool {
            name: name.to_string(),
            description: String::new(),
            input_schema: std::collections::HashMap::new(),
            tool_type: ty.map(|s| s.to_string()),
            max_uses: None,
            cache_control: None,
        };

        let tools = Some(vec![mk("", Some("web_search_20250305"))]);
        let mut map = HashMap::new();
        let converted = convert_tools(&tools, &mut map);

        assert_eq!(converted.len(), 1, "type-only web_search 必须归一化保留，不得剥除");
        let spec = &converted[0].tool_specification;
        assert_eq!(
            spec.name, "web_search",
            "归一化补 WebSearch（内置表 key）后经映射必须落到 Kiro 原生名 web_search"
        );
        // Kiro 工具结构无 tool_type 字段（归一化即丢弃 Anthropic 服务端 type），
        // 开关开启时 schema 应命中内置 web_search schema（{query}）—— 搜索能力完整保留。
        let schema: serde_json::Value = spec.input_schema.json.clone();
        assert!(
            schema["properties"].get("query").is_some(),
            "归一化后的 schema 必须是内置 web_search schema（含 query）"
        );
        assert!(
            schema["properties"].get("type").is_none() && schema["properties"].get("name").is_none(),
            "Anthropic 服务端 type/name 字段不得泄漏进 schema"
        );
    }

    /// 工具映射开关是进程级全局原子，测试并行会相互污染。用一把静态锁串行所有触碰
    /// 该开关的用例，并在守卫里恢复原值（ENV_NOISE_TEST_LOCK 同款，见上文）。
    static TOOL_COMPAT_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct ToolCompatGuard {
        prev: bool,
        _lock: std::sync::MutexGuard<'static, ()>,
    }
    impl ToolCompatGuard {
        fn with(enabled: bool) -> Self {
            let lock = TOOL_COMPAT_TEST_LOCK
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let prev = tool_compat_mapping_enabled();
            set_tool_compat_mapping(enabled);
            ToolCompatGuard { prev, _lock: lock }
        }
    }
    impl Drop for ToolCompatGuard {
        fn drop(&mut self) {
            set_tool_compat_mapping(self.prev);
        }
    }

    /// 开关关闭时映射**不生效**：Write 不映射成 fs_write、schema 不换成 Kiro 原生形态、
    /// 反向映射表不记录 Kiro 名；type-only web_search 归一化分支仍补名（分支不看开关），
    /// 但名字保持客户端原形（WebSearch）而不是 Kiro 原生 web_search。
    #[test]
    fn test_convert_tools_passthrough_when_mapping_disabled() {
        use super::super::types::Tool as AnthropicTool;

        let _g = ToolCompatGuard::with(false);

        let mk = |name: &str, ty: Option<&str>| AnthropicTool {
            name: name.to_string(),
            description: "d".to_string(),
            input_schema: {
                let mut m = std::collections::HashMap::new();
                m.insert("type".to_string(), serde_json::json!("object"));
                m.insert(
                    "properties".to_string(),
                    serde_json::json!({"file_path": {"type": "string"}}),
                );
                m
            },
            tool_type: ty.map(|s| s.to_string()),
            max_uses: None,
            cache_control: None,
        };

        let tools = Some(vec![mk("Write", None), mk("", Some("web_search_20250305"))]);
        let mut map = HashMap::new();
        let converted = convert_tools(&tools, &mut map);

        assert_eq!(converted.len(), 2, "关闭开关只影响映射，不影响工具保留/归一化");
        let names: Vec<&str> = converted
            .iter()
            .map(|t| t.tool_specification.name.as_str())
            .collect();
        assert!(
            names.contains(&"Write"),
            "关闭后 Write 必须原样透传（仅超长缩短），不得映射成 fs_write: {names:?}"
        );
        assert!(
            names.contains(&"WebSearch"),
            "关闭后 web_search 只补名不换 Kiro 原生名: {names:?}"
        );
        // 反向映射表不得记录 Kiro 原生名（fs_write）→ 客户端名。
        assert!(
            !map.contains_key("fs_write"),
            "关闭后反向映射表不得出现 Kiro 原生名 fs_write"
        );
        // Write 的 schema 必须是客户端原样（file_path），不是 Kiro 原生合成 schema（path）。
        // 真实入站 input_schema 是 JSON Schema 对象（type/properties），normalize 白名单
        // 只留 schema 字段；参数在 properties 下，不在根上。
        let write = converted
            .iter()
            .find(|t| t.tool_specification.name == "Write")
            .expect("Write 必须在");
        let schema: serde_json::Value = write.tool_specification.input_schema.json.clone();
        assert!(
            schema["properties"].get("file_path").is_some(),
            "关闭后 schema.properties 必须保留客户端 file_path 参数"
        );
        assert!(
            schema["properties"].get("path").is_none(),
            "关闭后不得注入 Kiro 原生 path 参数"
        );
    }

    #[test]
    fn test_convert_tools_regular_tool_unaffected() {
        let _g = ToolCompatGuard::with(true);
        use super::super::types::Tool as AnthropicTool;
        let tools = Some(vec![AnthropicTool {
            name: "Read".to_string(),
            description: String::new(),
            input_schema: std::collections::HashMap::new(),
            tool_type: None,
            max_uses: None,
            cache_control: None,
        }]);
        let mut map = HashMap::new();
        let converted = convert_tools(&tools, &mut map);
        assert_eq!(converted.len(), 1);
        // Read → read_file（CC↔Kiro 映射层）；tool_name_map 记录反向映射供出站还原。
        assert_eq!(converted[0].tool_specification.name, "read_file");
        assert_eq!(
            map.get("read_file").map(|s| s.as_str()),
            Some("Read"),
            "应记录 Kiro名→Claude Code 名的反向映射"
        );
    }

    /// fs_append（Claude Code 2.1.215+ 新增）在兼容模式下被隐藏（Kiro 上游不支持，
    /// 参考仓 kiro-rs-admin 同款处置）；Write/Read 等既有映射不受影响；
    /// 开关关闭（raw 透传）时原样保留。
    #[test]
    fn test_convert_tools_hides_fs_append_in_compat_mode() {
        use super::super::types::Tool as AnthropicTool;

        let mk = |name: &str| AnthropicTool {
            name: name.to_string(),
            description: String::new(),
            input_schema: std::collections::HashMap::new(),
            tool_type: None,
            max_uses: None,
            cache_control: None,
        };

        let tools = Some(vec![mk("fs_append"), mk("Write"), mk("Read")]);

        // 开关开启（Claude Code 兼容模式）：fs_append 隐藏，Write/Read 映射不受影响。
        let _g = ToolCompatGuard::with(true);
        let mut map = HashMap::new();
        let converted = convert_tools(&tools, &mut map);
        let names: Vec<&str> = converted
            .iter()
            .map(|t| t.tool_specification.name.as_str())
            .collect();
        assert!(
            !names.contains(&"fs_append"),
            "fs_append 应被隐藏，不得转发上游: {names:?}"
        );
        assert!(names.contains(&"fs_write"), "Write 映射不受影响: {names:?}");
        assert!(names.contains(&"read_file"), "Read 映射不受影响: {names:?}");
        assert!(
            !map.contains_key("fs_append"),
            "隐藏的工具不得进入 tool_name_map: {map:?}"
        );

        // 开关关闭（raw 透传）：fs_append 原样保留。
        drop(_g);
        let _g2 = ToolCompatGuard::with(false);
        let mut map = HashMap::new();
        let converted = convert_tools(&tools, &mut map);
        let names: Vec<&str> = converted
            .iter()
            .map(|t| t.tool_specification.name.as_str())
            .collect();
        assert!(
            names.contains(&"fs_append"),
            "关闭开关后 fs_append 必须透传（非 Claude Code 客户端不受影响）: {names:?}"
        );
    }

    /// CC↔Kiro 映射：Write → fs_write，且 schema 换成 Kiro 原生参数形态（path/text）。
    #[test]
    fn test_convert_tools_maps_builtin_to_kiro_schema() {
        let _g = ToolCompatGuard::with(true);
        use super::super::types::Tool as AnthropicTool;

        let tools = Some(vec![AnthropicTool {
            name: "Write".to_string(),
            description: String::new(),
            input_schema: std::collections::HashMap::new(),
            tool_type: None,
            max_uses: None,
            cache_control: None,
        }]);
        let mut map = HashMap::new();
        let converted = convert_tools(&tools, &mut map);
        assert_eq!(converted.len(), 1);
        let spec = &converted[0].tool_specification;
        assert_eq!(spec.name, "fs_write");
        // schema 应为合成 schema：参数名已是 Kiro 形态（path/text），不是客户端 file_path/content。
        let schema: serde_json::Value = spec.input_schema.json.clone();
        let props = &schema["properties"];
        assert!(props.get("path").is_some(), "合成 schema 应有 path");
        assert!(props.get("text").is_some(), "合成 schema 应有 text");
        assert!(
            props.get("file_path").is_none(),
            "合成 schema 不应残留客户端 file_path"
        );
        // 反向映射已记录
        assert_eq!(map.get("fs_write").map(|s| s.as_str()), Some("Write"));
    }

    /// 入站参数转换：Claude Code 参数 → Kiro 参数（file_path→path、content→text、
    /// old_string→oldStr、offset/limit→start_line/end_line）。
    /// 🔴 `Read.pages` 必须**降级而非报错**（2026-08-10 修，线上实测缺陷）。
    ///
    /// 改前：带 `pages` 的 Read 直接 `Err(UnsupportedToolMapping)` ⇒ handlers 渲染成
    /// **400 `工具参数无法映射: Read — ...`** 并终结整个请求 ⇒ Claude Code 整轮对话失败。
    ///
    /// 为什么处置过重：`pages` 只是「读哪几页」的范围提示，丢掉它的后果是「整读」——
    /// 信息更多而非更少，模型能自己定位。拿它否决整轮请求，代价远大于收益。
    #[test]
    fn read_pages_degrades_instead_of_failing() {
        // ① 字符串页范围：不再 Err，且意图进了 explanation
        let out = map_tool_input_to_kiro(
            "Read",
            serde_json::json!({"file_path": "/a.pdf", "pages": "1-5"}),
        )
        .expect("带 pages 的 Read 不该再报错（旧代码在此 panic）");
        assert_eq!(out["path"], "/a.pdf", "路径必须照常映射");
        let expl = out["explanation"].as_str().unwrap_or_default();
        assert!(
            expl.contains("1-5"),
            "页范围意图必须落进 explanation，否则降级就是静默丢信息: {expl}"
        );
        assert!(
            !out.as_object().unwrap().contains_key("pages"),
            "pages 不该原样透传给 Kiro（它不认这个参数）"
        );

        // ② 数组形式（部分客户端版本）
        let out = map_tool_input_to_kiro(
            "Read",
            serde_json::json!({"file_path": "/b.pdf", "pages": [2, 3]}),
        )
        .expect("数组 pages 同样不该报错");
        assert!(out["explanation"].as_str().unwrap_or_default().contains("2,3"));

        // ③ pages 为 null / 缺失：行为与改前完全一致（不追加任何提示）
        for v in [
            serde_json::json!({"file_path": "/c.txt", "pages": null}),
            serde_json::json!({"file_path": "/c.txt"}),
        ] {
            let out = map_tool_input_to_kiro("Read", v).expect("无 pages 必须正常");
            assert!(
                !out["explanation"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("只关心第"),
                "没有 pages 时不该凭空追加页提示"
            );
        }

        // ④ 与既有 offset/limit 映射共存（pages 提示不能挤掉行范围）
        let out = map_tool_input_to_kiro(
            "Read",
            serde_json::json!({"file_path": "/d.txt", "pages": "7", "offset": 10, "limit": 5}),
        )
        .expect("共存不该报错");
        assert_eq!(out["start_line"], 10);
        assert_eq!(out["end_line"], 14);
        assert!(out["explanation"].as_str().unwrap_or_default().contains("7"));
    }

    #[test]
    fn test_map_tool_input_to_kiro_converts_params() {
        // Write：file_path→path, content→text
        let write_in = serde_json::json!({"file_path": "/a.txt", "content": "hi"});
        let write_out = map_tool_input_to_kiro("Write", write_in).unwrap();
        assert_eq!(
            write_out,
            serde_json::json!({"path": "/a.txt", "text": "hi"})
        );

        // Edit：old_string→oldStr, new_string→newStr
        let edit_in = serde_json::json!({"file_path": "/a.txt", "old_string": "x", "new_string": "y"});
        let edit_out = map_tool_input_to_kiro("Edit", edit_in).unwrap();
        assert_eq!(
            edit_out,
            serde_json::json!({"path": "/a.txt", "oldStr": "x", "newStr": "y"})
        );

        // Read：offset/limit→start_line/end_line
        let read_in = serde_json::json!({"file_path": "/a.txt", "offset": 10, "limit": 5});
        let read_out = map_tool_input_to_kiro("Read", read_in).unwrap();
        assert_eq!(
            read_out,
            serde_json::json!({"path": "/a.txt", "start_line": 10, "end_line": 14, "explanation": "Mapped from Claude Code Read tool."})
        );

        // 非内置工具原样
        let custom = serde_json::json!({"x": 1});
        assert_eq!(map_tool_input_to_kiro("my_tool", custom.clone()).unwrap(), custom);
    }

    /// 出站参数还原：Kiro 参数 → Claude Code 参数（path→file_path、oldStr→old_string、
    /// start_line/end_line→offset/limit）。
    #[test]
    fn test_map_tool_input_from_kiro_restores_params() {
        let kiro_in = serde_json::json!({"path": "/a.txt", "text": "hi"});
        let restored = map_tool_input_from_kiro("Write", kiro_in);
        assert_eq!(
            restored,
            serde_json::json!({"file_path": "/a.txt", "content": "hi"})
        );

        let kiro_edit = serde_json::json!({"path": "/a.txt", "oldStr": "x", "newStr": "y"});
        assert_eq!(
            map_tool_input_from_kiro("Edit", kiro_edit),
            serde_json::json!({"file_path": "/a.txt", "old_string": "x", "new_string": "y"})
        );

        let kiro_read = serde_json::json!({"path": "/a.txt", "start_line": 10, "end_line": 14});
        assert_eq!(
            map_tool_input_from_kiro("Read", kiro_read),
            serde_json::json!({"file_path": "/a.txt", "offset": 10, "limit": 5})
        );
    }

    /// 🔴 回归：Write write_mode 透传、Glob includeIgnoredFiles 出站还原 bool、
    /// Grep excludePattern→exclude 出站还原、注入的 explanation 出站剥离。
    #[test]
    fn test_tool_mapping_write_mode_and_glob_grep_roundtrip() {
        // Write write_mode 入站透传 + 出站保留（之前被静默丢弃 → 覆盖写数据丢失）
        let write_in = serde_json::json!({"file_path": "/a.txt", "content": "hi", "write_mode": "append"});
        let write_out = map_tool_input_to_kiro("Write", write_in).unwrap();
        assert_eq!(write_out["write_mode"], "append", "write_mode 必须透传（防退化成覆盖写）");
        let restored = map_tool_input_from_kiro("Write", write_out);
        assert_eq!(restored["write_mode"], "append", "write_mode 出站保留");
        assert_eq!(restored["file_path"], "/a.txt");

        // Glob includeIgnoredFiles：入站 bool→"yes"/"no"，出站必须还原回 bool
        let glob_in = serde_json::json!({"pattern": "*.ts", "includeIgnoredFiles": true});
        let glob_out = map_tool_input_to_kiro("Glob", glob_in).unwrap();
        assert_eq!(glob_out["includeIgnoredFiles"], "yes");
        let glob_restored = map_tool_input_from_kiro("Glob", glob_out);
        assert_eq!(glob_restored["includeIgnoredFiles"], true, "includeIgnoredFiles 必须还原回 bool");
        assert!(
            !glob_restored.as_object().unwrap().contains_key("explanation"),
            "入站注入的 explanation 出站必须剥离（幻影参数）"
        );

        // Grep excludePattern→exclude 出站还原
        let grep_in = serde_json::json!({"pattern": "foo", "exclude": "vendor"});
        let grep_out = map_tool_input_to_kiro("Grep", grep_in).unwrap();
        assert_eq!(grep_out["excludePattern"], "vendor");
        let grep_restored = map_tool_input_from_kiro("Grep", grep_out);
        assert_eq!(grep_restored["exclude"], "vendor", "excludePattern 必须还原成 exclude");
        assert!(
            !grep_restored.as_object().unwrap().contains_key("excludePattern"),
            "还原后不应残留 excludePattern"
        );

        // Read 出站剥离注入的 explanation
        let read_restored = map_tool_input_from_kiro(
            "Read",
            serde_json::json!({"path": "/a.txt", "start_line": 1, "end_line": 2}),
        );
        assert!(
            !read_restored.as_object().unwrap().contains_key("explanation"),
            "Read 出站剥离 explanation"
        );
    }

    /// 出站完整还原：Kiro 名 + 参数 → Claude Code 名 + 参数（fs_write + path → Write + file_path）。
    #[test]
    fn test_restore_tool_use_for_client_roundtrip() {
        let mut map = HashMap::new();
        map.insert("fs_write".to_string(), "Write".to_string());
        let (name, input) = restore_tool_use_for_client(
            "fs_write",
            serde_json::json!({"path": "/a.txt", "text": "hi"}),
            &map,
        );
        assert_eq!(name, "Write");
        assert_eq!(
            input,
            serde_json::json!({"file_path": "/a.txt", "content": "hi"})
        );
    }

    #[test]
    fn test_tool_name_mapping_in_history() {
        use super::super::types::{Message as AnthropicMessage, Tool as AnthropicTool};

        let long_tool_name =
            "mcp__plugin_very_long_server_name__extremely_long_tool_name_exceeds_63";

        let mut schema = std::collections::HashMap::new();
        schema.insert("type".to_string(), serde_json::json!("object"));
        schema.insert("properties".to_string(), serde_json::json!({}));

        let req = MessagesRequest {
            model: "claude-sonnet-4".to_string(),
            max_tokens: 1024,
            messages: vec![
                AnthropicMessage {
                    role: "user".to_string(),
                    content: serde_json::json!("use the tool"),
                },
                AnthropicMessage {
                    role: "assistant".to_string(),
                    content: serde_json::json!([
                        {"type": "text", "text": "calling tool"},
                        {"type": "tool_use", "id": "toolu_01", "name": long_tool_name, "input": {}}
                    ]),
                },
                AnthropicMessage {
                    role: "user".to_string(),
                    content: serde_json::json!([
                        {"type": "tool_result", "tool_use_id": "toolu_01", "content": "done"}
                    ]),
                },
            ],
            system: None,
            stream: false,
            tools: Some(vec![AnthropicTool {
                name: long_tool_name.to_string(),
                description: "A test tool".to_string(),
                input_schema: schema,
                tool_type: None,
                max_uses: None,
                cache_control: None,
            }]),
            thinking: None,
            tool_choice: None,
            output_config: None,
            metadata: None,
        };

        let result = convert_request(&req).unwrap();
        let short_name = result.tool_name_map.iter().next().unwrap().0.clone();

        // 历史中 assistant 消息的 tool_use name 也应该被映射
        let history = &result.conversation_state.history;
        let mut found = false;
        for msg in history {
            if let Message::Assistant(a) = msg {
                if let Some(ref tool_uses) = a.assistant_response_message.tool_uses {
                    for tu in tool_uses {
                        if tu.tool_use_id == "toolu_01" {
                            assert_eq!(tu.name, short_name, "历史中的 tool_use name 应该是短名称");
                            found = true;
                        }
                    }
                }
            }
        }
        assert!(found, "应该在历史中找到 tool_use");
    }

    // ===== JSON Schema $ref 展开 + 规范化 =====

    #[test]
    fn test_normalize_schema_expands_ref_from_defs() {
        // MCP/pydantic 风格：属性用 $ref 指向 $defs 的子 schema
        let schema = serde_json::json!({
            "type": "object",
            "properties": { "filter": { "$ref": "#/$defs/Filter" } },
            "$defs": {
                "Filter": {
                    "type": "object",
                    "properties": { "field": { "type": "string" } },
                    "required": ["field"]
                }
            }
        });
        let out = normalize_json_schema(schema);
        let filter = &out["properties"]["filter"];
        // $ref 应展开为真实子 schema，而非退化为空对象
        assert_eq!(filter["type"], "object");
        assert_eq!(filter["properties"]["field"]["type"], "string");
        // $defs / $ref 不应残留（Kiro 不认）
        assert!(out.get("$defs").is_none(), "$defs 不应残留");
        assert!(filter.get("$ref").is_none(), "$ref 不应残留");
    }

    #[test]
    fn test_normalize_schema_ref_cycle_safe() {
        // 自引用循环：node 指向自身，必须靠深度上限兜底不栈溢出
        let schema = serde_json::json!({
            "type": "object",
            "properties": { "node": { "$ref": "#/$defs/Node" } },
            "$defs": {
                "Node": {
                    "type": "object",
                    "properties": { "child": { "$ref": "#/$defs/Node" } }
                }
            }
        });
        let out = normalize_json_schema(schema);
        // 不 panic 即通过；顶层结构正常
        assert_eq!(out["type"], "object");
        assert!(out["properties"].get("node").is_some());
    }

    /// 构造一个自引用扇出（fan-out）schema：`$defs.T` 的 `properties` 里放 `b` 个
    /// 都指回 `T` 自身的属性（`c0..c{b-1}`），根节点的 `node` 属性再引用 `T`。
    ///
    /// 这是节点预算文档注释里描述的攻击最小形态：链长闸门（`MAX_REF_DEPTH=16`）只限
    /// "跳了多少次 `$ref`"，同级的 b 个属性复用同一个 `depth`，于是不设总量预算时
    /// 节点数是 b^16 量级（实测 b=2 时六百万+，`resolve_schema_refs` 内联文档写的
    /// "800 万+" 与之量级一致）。b=1（唯一安全的分叉因子）不会触发这条路径，因为
    /// 链长闸门本身就先拦住了它——这正是本测试要补的缺口。
    fn build_fanout_ref_schema(b: usize) -> serde_json::Value {
        let mut props = serde_json::Map::new();
        for i in 0..b {
            props.insert(format!("c{i}"), serde_json::json!({ "$ref": "#/$defs/T" }));
        }
        serde_json::json!({
            "type": "object",
            "properties": { "node": { "$ref": "#/$defs/T" } },
            "$defs": {
                "T": { "type": "object", "properties": props }
            }
        })
    }

    /// 构造一条**有限**的分叉引用链：`T0 -> T1 -> ... -> T{depth-1} -> leaf`，每层都
    /// fan-out `b` way（`c0..c{b-1}` 都指向下一层，而不是指回自己）。
    ///
    /// 与 `build_fanout_ref_schema` 的关键区别：这不是自引用循环，链长本身就有限
    /// （`depth` 层后落到叶子 `"type": "string"`），无论有没有节点预算 / 深度闸门都会
    /// 自然终止 —— 这正是"预算充足时应正常展开"这条对照要验证的场景：一个真实存在
    /// （只是层数多、扇出大）的合法 schema，不该被节点预算误杀成宽松 object。
    fn build_bounded_fanout_chain_schema(b: usize, depth: usize) -> serde_json::Value {
        let mut defs = serde_json::Map::new();
        defs.insert(format!("T{depth}"), serde_json::json!({ "type": "string" }));
        for lvl in (0..depth).rev() {
            let mut props = serde_json::Map::new();
            for i in 0..b {
                props.insert(
                    format!("c{i}"),
                    serde_json::json!({ "$ref": format!("#/$defs/T{}", lvl + 1) }),
                );
            }
            defs.insert(
                format!("T{lvl}"),
                serde_json::json!({ "type": "object", "properties": props }),
            );
        }
        serde_json::json!({
            "type": "object",
            "properties": { "node": { "$ref": "#/$defs/T0" } },
            "$defs": serde_json::Value::Object(defs)
        })
    }

    /// 递归统计 JSON 节点总数（object/array 记 1 再加子节点，标量记 1），用来断言
    /// 展开结果确实被节点预算钉住了上界，而不是靠"没崩就算过"这种弱断言。
    fn count_json_nodes(v: &serde_json::Value) -> usize {
        match v {
            serde_json::Value::Object(obj) => 1 + obj.values().map(count_json_nodes).sum::<usize>(),
            serde_json::Value::Array(arr) => 1 + arr.iter().map(count_json_nodes).sum::<usize>(),
            _ => 1,
        }
    }

    #[test]
    fn test_normalize_schema_fanout_b2_bounded_by_small_budget() {
        // b=2：最小的真实扇出因子。小预算下必须返回（不挂死）且被截断（预算生效）。
        let schema = build_fanout_ref_schema(2);
        let out = normalize_json_schema_with_node_budget(schema, 100);
        // 上界：展开结果的节点数不能超过注入的预算（+ 少量白名单/顶层字段的常数开销）。
        // 100 节点的预算下，实测输出稳定在个位数到十几个节点，远小于无预算时的
        // 6,488,067（b=2, 深度 16）——这就是本测试要守住的差异。
        assert!(
            count_json_nodes(&out) <= 100,
            "b=2 小预算展开结果应被节点预算钉住上界，实际 {} 个节点",
            count_json_nodes(&out)
        );
        // 结构仍是合法 schema（顶层未被整体判 malformed）。
        assert_eq!(out["type"], "object");
    }

    #[test]
    fn test_normalize_schema_fanout_b3_bounded_by_small_budget() {
        // b=3：分叉因子更大，无预算时展开量比 b=2 大得多（指数级），链长闸门
        // （depth 只在 $ref 跳转时 +1，不算同级扇出）对此完全无效，必须靠节点预算拦。
        let schema = build_fanout_ref_schema(3);
        let out = normalize_json_schema_with_node_budget(schema, 100);
        assert!(
            count_json_nodes(&out) <= 100,
            "b=3 小预算展开结果应被节点预算钉住上界，实际 {} 个节点",
            count_json_nodes(&out)
        );
        assert_eq!(out["type"], "object");
    }

    #[test]
    fn test_normalize_schema_fanout_b3_expands_fully_when_budget_sufficient() {
        // 对照组：证明节点预算**不是无脑截断** —— 一个合法的大 schema 在**生产预算**下
        // 必须完整展开、零截断。对应生产注释里"合法请求不可能被截断"的担忧。
        //
        // ⚠️ 参数是**实测**定的，不是估的（本测试上一版就是估错才长期为红）：
        //   b=3 深度 3 → visited   825 / 输出  110
        //   b=3 深度 4 → visited 3,168 / 输出  326
        //   b=3 深度 5 → visited 11,613 / 输出  974   ← 本测试用这档
        //   b=3 深度 6 → visited 41,199 / 输出 2,918   （旧版注释写"18,774"，错了一倍多）
        // 深度 6 在 20,000 预算下**必然**被截断（41,199 > 20,000），而旧版断言它不该截断，
        // 于是实现正确、测试为红。深度 5 的 11,613 在生产预算 50,000 下余量约 4 倍。
        //
        // 刻意用生产常量 `MAX_SCHEMA_NODES` 而非注入一个更大的数：注入 60,000 也能让深度 6
        // 通过，但那证明的是"给足够大的预算就不截断"（同义反复），而这里要证明的是
        // **真实生产配置下合法 schema 不受影响**。
        let schema = build_bounded_fanout_chain_schema(3, 5);
        let defs = extract_schema_defs(&schema);
        let mut budget = SchemaRefBudget::new(MAX_SCHEMA_NODES);
        let resolved = resolve_schema_refs(schema.clone(), &defs, 0, &mut budget);

        // 🔴 承重断言：**零截断**。这比"输出节点数 > N"强得多 ——
        // 后者在部分截断时仍可能成立（截断只降级末梢子树，总数照样很大）。
        assert_eq!(
            budget.truncated_nodes, 0,
            "生产预算下合法 schema 不得被截断，实际截断 {} 次（visited={}）",
            budget.truncated_nodes, budget.visited
        );
        // 预算确实被消耗了（防"夹具没触发展开"这种恒真断言：若 $defs 拼错导致
        // 一个 $ref 都没展开，visited 会是个位数而零截断照样成立）。
        assert!(
            budget.visited > 10_000,
            "夹具自检：深度 5 应访问约 11,613 个节点，实际 {} —— 太少说明 $ref 没被展开",
            budget.visited
        );
        assert!(
            resolved.get("properties").is_some(),
            "展开结果应保留 properties"
        );

        let out = normalize_json_schema_with_node_budget(schema, MAX_SCHEMA_NODES);
        let node = &out["properties"]["node"];
        // 未被截断：$ref 已展开为真实子 schema，属性里应能看到 c0/c1/c2，
        // 而不是降级后的 { "type": "object", "additionalProperties": true } 空壳。
        assert_eq!(node["type"], "object");
        assert!(
            node["properties"].get("c0").is_some(),
            "预算充足时应展开出真实子属性 c0，而非降级空壳: {node:?}"
        );
        // 再深一层同样展开（证明是整棵树展开，不只是第一层）。
        assert_eq!(node["properties"]["c0"]["type"], "object");
        assert!(
            node["properties"]["c0"]["properties"].get("c0").is_some(),
            "第二层也应展开出 c0（整棵树而非仅首层）"
        );
    }

    /// 🔴 预算耗尽时**数组必须仍是数组**，不得被换成 object 占位。
    ///
    /// 缺陷形态：`degraded_object_schema()` 是个 object，若预算在一个 `Value::Array`
    /// 节点上耗尽，整个数组被替换成对象。而 `anyOf` / `oneOf` / `allOf` / 元组式
    /// `items` **必须是数组** ⇒ 产出结构非法的 JSON Schema ⇒ 上游 400。
    /// 而节点预算存在的全部目的就是避免上游报错，那就自相矛盾了。
    ///
    /// 回退即 FAILED：把函数开头那个数组提前返回删掉（让数组重新落到预算闸门之后），
    /// 本测试必红。
    #[test]
    fn test_budget_exhaustion_keeps_arrays_as_arrays() {
        // ⚠️ 数组必须放在**根的直接键**上。实测过一版把它们埋在 `properties.pick` /
        // `properties.tuple` 之下：预算在那两个**对象**上就耗尽 ⇒ 对象被换成 object 占位
        // ⇒ `anyOf`/`items` 这两个键**整个消失**，断言拿到 `Null` 而不是「数组变对象」。
        // 那测的是父节点降级，不是本缺陷。放根上才让断言只关心数组本身。
        let big = serde_json::json!({ "type": "object", "properties": {
            "a": {"type":"string"}, "b": {"type":"string"}, "c": {"type":"string"},
            "d": {"type":"string"}, "e": {"type":"string"}, "f": {"type":"string"}
        }});

        // 每个 case 一份独立夹具：`anyOf` 与元组式 `items` 各自在根上。
        for (label, schema) in [
            (
                "anyOf",
                serde_json::json!({
                    "anyOf": [
                        { "$ref": "#/$defs/Big" },
                        { "$ref": "#/$defs/Big" },
                        { "$ref": "#/$defs/Big" }
                    ],
                    "$defs": { "Big": big.clone() }
                }),
            ),
            (
                "items",
                serde_json::json!({
                    "items": [ { "$ref": "#/$defs/Big" }, { "$ref": "#/$defs/Big" } ],
                    "$defs": { "Big": big.clone() }
                }),
            ),
        ] {
            let defs = extract_schema_defs(&schema);
            // 预算 3：足够访问根对象，但展开第一个 $ref 就耗尽。
            let mut budget = SchemaRefBudget::new(3);
            let resolved = resolve_schema_refs(schema.clone(), &defs, 0, &mut budget);

            // 夹具自检：预算必须真的被耗尽，否则本断言恒真（本仓「纸面测试」形态之一）。
            assert!(
                budget.truncated_nodes > 0,
                "[{label}] 夹具自检失败：预算未被耗尽（truncated=0, visited={}）⇒ 本测试恒真",
                budget.visited
            );

            // 🔴 承重断言：耗尽后该位置仍是数组。
            let arr = &resolved[label];
            assert!(
                arr.is_array(),
                "[{label}] 预算耗尽后必须仍是数组（否则 schema 结构非法、上游 400），实际: {arr:?}"
            );
            // 元素数不变（元素可退化成 object 占位，但不能少 —— 少了元组语义就变了）。
            let expected = if label == "anyOf" { 3 } else { 2 };
            assert_eq!(
                arr.as_array().map(|a| a.len()),
                Some(expected),
                "[{label}] 元素数不得改变"
            );
        }
    }

    #[test]
    fn test_normalize_schema_unresolvable_ref_degrades() {
        // OpenAPI 风格 #/components 无法展开 → 降级为宽松 object 而非空壳
        let schema = serde_json::json!({
            "type": "object",
            "properties": { "x": { "$ref": "#/components/schemas/Foo" } }
        });
        let out = normalize_json_schema(schema);
        assert_eq!(out["properties"]["x"]["type"], "object");
        assert!(out["properties"]["x"].get("$ref").is_none());
    }

    #[test]
    fn test_normalize_schema_drops_combinators_and_nonwhitelist() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "opts": { "anyOf": [{"type": "object"}, {"type": "null"}] }
            },
            "title": "should be stripped",
            "$schema": "http://json-schema.org/draft-07/schema#"
        });
        let out = normalize_json_schema(schema);
        // anyOf 被丢弃；非白名单顶层字段被清
        assert!(out["properties"]["opts"].get("anyOf").is_none());
        assert!(out.get("title").is_none());
        assert!(out.get("$schema").is_none());
    }

    #[test]
    fn test_derive_agent_continuation_id_deterministic_and_isolated() {
        let a1 = derive_agent_continuation_id("conv-abc");
        let a2 = derive_agent_continuation_id("conv-abc");
        let b = derive_agent_continuation_id("conv-xyz");
        // 同会话恒定
        assert_eq!(a1, a2, "同一 conversationId 必须派生相同 continuationId");
        // 跨会话隔离
        assert_ne!(a1, b, "不同 conversationId 必须不同");
        // UUID 形状（36 字符,含 4 个连字符）
        assert_eq!(a1.len(), 36);
        assert_eq!(a1.matches('-').count(), 4);
    }

    #[test]
    fn test_extract_pdf_text_from_literal_streams() {
        // 构造一个最小 PDF 内容流片段：两个 (文本) 后接 Tj
        let fake_pdf = b"%PDF-1.4\nBT /F1 12 Tf (Hello World) Tj 0 -14 Td (Second line) Tj ET\n";
        let out = extract_pdf_text_from_bytes(fake_pdf);
        assert!(out.is_some(), "应能抽取到文本");
        let text = out.unwrap();
        assert!(text.contains("Hello World"), "应含第一段: {text}");
        assert!(text.contains("Second line"), "应含第二段: {text}");
    }

    #[test]
    fn test_extract_pdf_text_none_when_no_text() {
        // 没有文本绘制操作符的字面量不应被当作文本
        let no_text = b"%PDF-1.4\n(random data without Tj)\n";
        // 后面无 Tj/TJ/' → 不算文本
        let out = extract_pdf_text_from_bytes(no_text);
        assert!(out.is_none(), "无 Tj 操作符不应抽出文本");
    }

    #[test]
    fn test_history_tools_added_to_tools_list() {
        use super::super::types::Message as AnthropicMessage;

        // 创建一个请求，历史中有工具使用，但 tools 列表为空
        let req = MessagesRequest {
            model: "claude-sonnet-4".to_string(),
            max_tokens: 1024,
            messages: vec![
                AnthropicMessage {
                    role: "user".to_string(),
                    content: serde_json::json!("Read the file"),
                },
                AnthropicMessage {
                    role: "assistant".to_string(),
                    content: serde_json::json!([
                        {"type": "text", "text": "I'll read the file."},
                        {"type": "tool_use", "id": "tool-1", "name": "read", "input": {"path": "/test.txt"}}
                    ]),
                },
                AnthropicMessage {
                    role: "user".to_string(),
                    content: serde_json::json!([
                        {"type": "tool_result", "tool_use_id": "tool-1", "content": "file content"}
                    ]),
                },
            ],
            stream: false,
            system: None,
            tools: None, // 没有提供工具定义
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: None,
        };

        let result = convert_request(&req).unwrap();

        // 验证 tools 列表中包含了历史中使用的工具的占位符定义
        let tools = &result
            .conversation_state
            .current_message
            .user_input_message
            .user_input_message_context
            .tools;

        assert!(!tools.is_empty(), "tools 列表不应为空");
        assert!(
            tools.iter().any(|t| t.tool_specification.name == "read"),
            "tools 列表应包含 'read' 工具的占位符定义"
        );
    }

    #[test]
    fn test_extract_session_id_valid() {
        // 测试有效的 user_id 格式
        let user_id = "user_0dede55c6dcc4a11a30bbb5e7f22e6fdf86cdeba3820019cc27612af4e1243cd_account__session_8bb5523b-ec7c-4540-a9ca-beb6d79f1552";
        let session_id = extract_session_id(user_id);
        assert_eq!(
            session_id,
            Some("8bb5523b-ec7c-4540-a9ca-beb6d79f1552".to_string())
        );
    }

    #[test]
    fn test_extract_session_id_json_format() {
        // 测试 JSON 格式的 user_id
        let user_id = r#"{"device_id":"0dede55c6dcc4a11a30bbb5e7f22e6fdf86cdeba3820019cc27612af4e1243cd","account_uuid":"","session_id":"8bb5523b-ec7c-4540-a9ca-beb6d79f1552"}"#;
        let session_id = extract_session_id(user_id);
        assert_eq!(
            session_id,
            Some("8bb5523b-ec7c-4540-a9ca-beb6d79f1552".to_string())
        );
    }

    #[test]
    fn test_extract_session_id_json_invalid_session() {
        // 测试 JSON 格式但 session_id 不是有效 UUID
        let user_id = r#"{"device_id":"abc","session_id":"not-a-uuid"}"#;
        let session_id = extract_session_id(user_id);
        assert_eq!(session_id, None);
    }

    #[test]
    fn test_extract_session_id_no_session() {
        // 测试没有 session 的 user_id
        let user_id = "user_0dede55c6dcc4a11a30bbb5e7f22e6fdf86cdeba3820019cc27612af4e1243cd";
        let session_id = extract_session_id(user_id);
        assert_eq!(session_id, None);
    }

    #[test]
    fn test_extract_session_id_invalid_uuid() {
        // 测试无效的 UUID 格式
        let user_id = "user_xxx_session_invalid-uuid";
        let session_id = extract_session_id(user_id);
        assert_eq!(session_id, None);
    }

    #[test]
    fn test_convert_request_with_session_metadata() {
        use super::super::types::{Message as AnthropicMessage, Metadata};

        // 测试带有 metadata 的请求，应该使用 session UUID 作为 conversationId
        let req = MessagesRequest {
            model: "claude-sonnet-4".to_string(),
            max_tokens: 1024,
            messages: vec![AnthropicMessage {
                role: "user".to_string(),
                content: serde_json::json!("Hello"),
            }],
            stream: false,
            system: None,
            tools: None,
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: Some(Metadata {
                user_id: Some(
                    "user_0dede55c6dcc4a11a30bbb5e7f22e6fdf86cdeba3820019cc27612af4e1243cd_account__session_a0662283-7fd3-4399-a7eb-52b9a717ae88".to_string(),
                ),
            }),
        };

        let result = convert_request(&req).unwrap();
        assert_eq!(
            result.conversation_state.conversation_id,
            "a0662283-7fd3-4399-a7eb-52b9a717ae88"
        );
    }

    #[test]
    fn test_convert_request_without_metadata() {
        use super::super::types::Message as AnthropicMessage;

        // 无 metadata **且** system/tools 双空 —— 三级回落链的最后一级（随机 UUID）。
        // 有 system 或 tools 时走上下文派生，见 `derived_conversation_id_*` 系列用例。
        let req = MessagesRequest {
            model: "claude-sonnet-4".to_string(),
            max_tokens: 1024,
            messages: vec![AnthropicMessage {
                role: "user".to_string(),
                content: serde_json::json!("Hello"),
            }],
            stream: false,
            system: None,
            tools: None,
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: None,
        };

        let result = convert_request(&req).unwrap();
        // 验证生成的是有效的 UUID 格式
        assert_eq!(result.conversation_state.conversation_id.len(), 36);
        assert_eq!(
            result
                .conversation_state
                .conversation_id
                .chars()
                .filter(|c| *c == '-')
                .count(),
            4
        );
    }

    #[test]
    fn test_validate_tool_pairing_orphaned_result() {
        // 测试孤立的 tool_result 被过滤
        // 历史中没有 tool_use，但 tool_results 中有 tool_result
        let history = vec![
            Message::User(HistoryUserMessage::new("Hello", "claude-sonnet-4.5")),
            Message::Assistant(HistoryAssistantMessage::new("Hi there!")),
        ];

        let tool_results = vec![ToolResult::success("orphan-123", "some result")];

        let (filtered, _) = validate_tool_pairing(&history, &tool_results);

        // 孤立的 tool_result 应该被过滤掉
        assert!(filtered.is_empty(), "孤立的 tool_result 应该被过滤");
    }

    #[test]
    fn test_validate_tool_pairing_orphaned_use() {
        use crate::kiro::model::requests::tool::ToolUseEntry;

        // 测试孤立的 tool_use（有 tool_use 但没有对应的 tool_result）
        let mut assistant_msg = AssistantMessage::new("I'll read the file.");
        assistant_msg = assistant_msg.with_tool_uses(vec![
            ToolUseEntry::new("tool-orphan", "read")
                .with_input(serde_json::json!({"path": "/test.txt"})),
        ]);

        let history = vec![
            Message::User(HistoryUserMessage::new(
                "Read the file",
                "claude-sonnet-4.5",
            )),
            Message::Assistant(HistoryAssistantMessage {
                assistant_response_message: assistant_msg,
            }),
        ];

        // 没有 tool_result
        let tool_results: Vec<ToolResult> = vec![];

        let (filtered, orphaned) = validate_tool_pairing(&history, &tool_results);

        // 结果应该为空（因为没有 tool_result）
        // 同时应该返回孤立的 tool_use_id
        assert!(filtered.is_empty());
        assert!(orphaned.contains("tool-orphan"));
    }

    #[test]
    fn test_validate_tool_pairing_valid() {
        use crate::kiro::model::requests::tool::ToolUseEntry;

        // 测试正常配对的情况
        let mut assistant_msg = AssistantMessage::new("I'll read the file.");
        assistant_msg = assistant_msg.with_tool_uses(vec![
            ToolUseEntry::new("tool-1", "read")
                .with_input(serde_json::json!({"path": "/test.txt"})),
        ]);

        let history = vec![
            Message::User(HistoryUserMessage::new(
                "Read the file",
                "claude-sonnet-4.5",
            )),
            Message::Assistant(HistoryAssistantMessage {
                assistant_response_message: assistant_msg,
            }),
        ];

        let tool_results = vec![ToolResult::success("tool-1", "file content")];

        let (filtered, orphaned) = validate_tool_pairing(&history, &tool_results);

        // 配对成功，应该保留，无孤立
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].tool_use_id, "tool-1");
        assert!(orphaned.is_empty());
    }

    #[test]
    fn test_validate_tool_pairing_mixed() {
        use crate::kiro::model::requests::tool::ToolUseEntry;

        // 测试混合情况：部分配对成功，部分孤立
        let mut assistant_msg = AssistantMessage::new("I'll use two tools.");
        assistant_msg = assistant_msg.with_tool_uses(vec![
            ToolUseEntry::new("tool-1", "read").with_input(serde_json::json!({})),
            ToolUseEntry::new("tool-2", "write").with_input(serde_json::json!({})),
        ]);

        let history = vec![
            Message::User(HistoryUserMessage::new("Do something", "claude-sonnet-4.5")),
            Message::Assistant(HistoryAssistantMessage {
                assistant_response_message: assistant_msg,
            }),
        ];

        // tool_results: tool-1 配对，tool-3 孤立
        let tool_results = vec![
            ToolResult::success("tool-1", "result 1"),
            ToolResult::success("tool-3", "orphan result"), // 孤立
        ];

        let (filtered, orphaned) = validate_tool_pairing(&history, &tool_results);

        // 只有 tool-1 应该保留
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].tool_use_id, "tool-1");
        // tool-2 是孤立的 tool_use（无 result），tool-3 是孤立的 tool_result
        assert!(orphaned.contains("tool-2"));
    }

    #[test]
    fn test_validate_tool_pairing_history_already_paired() {
        use crate::kiro::model::requests::tool::ToolUseEntry;

        // 测试历史中已配对的 tool_use 不应该被报告为孤立
        // 场景：多轮对话中，之前的 tool_use 已经在历史中有对应的 tool_result
        let mut assistant_msg1 = AssistantMessage::new("I'll read the file.");
        assistant_msg1 = assistant_msg1.with_tool_uses(vec![
            ToolUseEntry::new("tool-1", "read")
                .with_input(serde_json::json!({"path": "/test.txt"})),
        ]);

        // 构建历史中的 user 消息，包含 tool_result
        let mut user_msg_with_result = UserMessage::new("", "claude-sonnet-4.5");
        let mut ctx = UserInputMessageContext::new();
        ctx = ctx.with_tool_results(vec![ToolResult::success("tool-1", "file content")]);
        user_msg_with_result = user_msg_with_result.with_context(ctx);

        let history = vec![
            // 第一轮：用户请求
            Message::User(HistoryUserMessage::new(
                "Read the file",
                "claude-sonnet-4.5",
            )),
            // 第一轮：assistant 使用工具
            Message::Assistant(HistoryAssistantMessage {
                assistant_response_message: assistant_msg1,
            }),
            // 第二轮：用户返回工具结果（历史中已配对）
            Message::User(HistoryUserMessage {
                user_input_message: user_msg_with_result,
            }),
            // 第二轮：assistant 响应
            Message::Assistant(HistoryAssistantMessage::new("The file contains...")),
        ];

        // 当前消息没有 tool_results（用户只是继续对话）
        let tool_results: Vec<ToolResult> = vec![];

        let (filtered, orphaned) = validate_tool_pairing(&history, &tool_results);

        // 结果应该为空，且不应该有孤立 tool_use
        // 因为 tool-1 已经在历史中配对了
        assert!(filtered.is_empty());
        assert!(orphaned.is_empty());
    }

    #[test]
    fn test_validate_tool_pairing_duplicate_result() {
        use crate::kiro::model::requests::tool::ToolUseEntry;

        // 测试重复的 tool_result（历史中已配对，当前消息又发送了相同的 tool_result）
        let mut assistant_msg = AssistantMessage::new("I'll read the file.");
        assistant_msg = assistant_msg.with_tool_uses(vec![
            ToolUseEntry::new("tool-1", "read")
                .with_input(serde_json::json!({"path": "/test.txt"})),
        ]);

        // 历史中已有 tool_result
        let mut user_msg_with_result = UserMessage::new("", "claude-sonnet-4.5");
        let mut ctx = UserInputMessageContext::new();
        ctx = ctx.with_tool_results(vec![ToolResult::success("tool-1", "file content")]);
        user_msg_with_result = user_msg_with_result.with_context(ctx);

        let history = vec![
            Message::User(HistoryUserMessage::new(
                "Read the file",
                "claude-sonnet-4.5",
            )),
            Message::Assistant(HistoryAssistantMessage {
                assistant_response_message: assistant_msg,
            }),
            Message::User(HistoryUserMessage {
                user_input_message: user_msg_with_result,
            }),
            Message::Assistant(HistoryAssistantMessage::new("Done")),
        ];

        // 当前消息又发送了相同的 tool_result（重复）
        let tool_results = vec![ToolResult::success("tool-1", "file content again")];

        let (filtered, _) = validate_tool_pairing(&history, &tool_results);

        // 重复的 tool_result 应该被过滤掉
        assert!(filtered.is_empty(), "重复的 tool_result 应该被过滤");
    }

    #[test]
    fn test_convert_assistant_message_tool_use_only() {
        use super::super::types::Message as AnthropicMessage;

        // 测试仅包含 tool_use 的 assistant 消息（无 text 块）
        // Kiro API 要求 content 字段不能为空
        let msg = AnthropicMessage {
            role: "assistant".to_string(),
            content: serde_json::json!([
                {"type": "tool_use", "id": "toolu_01ABC", "name": "read_file", "input": {"path": "/test.txt"}}
            ]),
        };

        let result = convert_assistant_message(&msg, &mut HashMap::new()).expect("应该成功转换");

        // 验证 content 不为空（使用占位符）
        assert!(
            !result.assistant_response_message.content.is_empty(),
            "content 不应为空"
        );
        assert_eq!(
            result.assistant_response_message.content, " ",
            "仅 tool_use 时应使用 ' ' 占位符"
        );

        // 验证 tool_uses 被正确保留
        let tool_uses = result
            .assistant_response_message
            .tool_uses
            .expect("应该有 tool_uses");
        assert_eq!(tool_uses.len(), 1);
        assert_eq!(tool_uses[0].tool_use_id, "toolu_01ABC");
        assert_eq!(tool_uses[0].name, "read_file");
    }

    #[test]
    fn test_convert_assistant_message_with_text_and_tool_use() {
        use super::super::types::Message as AnthropicMessage;

        // 测试同时包含 text 和 tool_use 的 assistant 消息
        let msg = AnthropicMessage {
            role: "assistant".to_string(),
            content: serde_json::json!([
                {"type": "text", "text": "Let me read that file for you."},
                {"type": "tool_use", "id": "toolu_02XYZ", "name": "read_file", "input": {"path": "/data.json"}}
            ]),
        };

        let result = convert_assistant_message(&msg, &mut HashMap::new()).expect("应该成功转换");

        // 验证 content 使用原始文本（不是占位符）
        assert_eq!(
            result.assistant_response_message.content,
            "Let me read that file for you."
        );

        // 验证 tool_uses 被正确保留
        let tool_uses = result
            .assistant_response_message
            .tool_uses
            .expect("应该有 tool_uses");
        assert_eq!(tool_uses.len(), 1);
        assert_eq!(tool_uses[0].tool_use_id, "toolu_02XYZ");
    }

    #[test]
    fn test_remove_orphaned_tool_uses() {
        use crate::kiro::model::requests::tool::ToolUseEntry;

        // 测试从历史中移除孤立的 tool_use
        let mut assistant_msg = AssistantMessage::new("I'll use multiple tools.");
        assistant_msg = assistant_msg.with_tool_uses(vec![
            ToolUseEntry::new("tool-1", "read").with_input(serde_json::json!({})),
            ToolUseEntry::new("tool-2", "write").with_input(serde_json::json!({})),
            ToolUseEntry::new("tool-3", "delete").with_input(serde_json::json!({})),
        ]);

        let mut history = vec![
            Message::User(HistoryUserMessage::new("Do something", "claude-sonnet-4.5")),
            Message::Assistant(HistoryAssistantMessage {
                assistant_response_message: assistant_msg,
            }),
        ];

        // 移除 tool-1 和 tool-3
        let mut orphaned = std::collections::HashSet::new();
        orphaned.insert("tool-1".to_string());
        orphaned.insert("tool-3".to_string());

        remove_orphaned_tool_uses(&mut history, &orphaned);

        // 验证只剩下 tool-2
        if let Message::Assistant(ref assistant_msg) = history[1] {
            let tool_uses = assistant_msg
                .assistant_response_message
                .tool_uses
                .as_ref()
                .expect("应该还有 tool_uses");
            assert_eq!(tool_uses.len(), 1);
            assert_eq!(tool_uses[0].tool_use_id, "tool-2");
        } else {
            panic!("应该是 Assistant 消息");
        }
    }

    #[test]
    fn test_remove_orphaned_tool_uses_all_removed() {
        use crate::kiro::model::requests::tool::ToolUseEntry;

        // 测试移除所有 tool_use 后，tool_uses 变为 None
        let mut assistant_msg = AssistantMessage::new("I'll use a tool.");
        assistant_msg = assistant_msg.with_tool_uses(vec![
            ToolUseEntry::new("tool-1", "read").with_input(serde_json::json!({})),
        ]);

        let mut history = vec![
            Message::User(HistoryUserMessage::new("Do something", "claude-sonnet-4.5")),
            Message::Assistant(HistoryAssistantMessage {
                assistant_response_message: assistant_msg,
            }),
        ];

        let mut orphaned = std::collections::HashSet::new();
        orphaned.insert("tool-1".to_string());

        remove_orphaned_tool_uses(&mut history, &orphaned);

        // 验证 tool_uses 变为 None
        if let Message::Assistant(ref assistant_msg) = history[1] {
            assert!(
                assistant_msg.assistant_response_message.tool_uses.is_none(),
                "移除所有 tool_use 后应为 None"
            );
        } else {
            panic!("应该是 Assistant 消息");
        }
    }

    #[test]
    fn test_merge_consecutive_assistant_messages() {
        // 测试连续 assistant 消息被正确合并（Issue #79）
        use super::super::types::Message as AnthropicMessage;

        let msg1 = AnthropicMessage {
            role: "assistant".to_string(),
            content: serde_json::json!([
                {"type": "thinking", "thinking": "Let me think about this..."},
                {"type": "text", "text": " "}
            ]),
        };

        let msg2 = AnthropicMessage {
            role: "assistant".to_string(),
            content: serde_json::json!([
                {"type": "thinking", "thinking": "I should read the file."},
                {"type": "text", "text": "Let me read that file."},
                {"type": "tool_use", "id": "toolu_01ABC", "name": "read_file", "input": {"path": "/test.txt"}}
            ]),
        };

        let messages: Vec<&AnthropicMessage> = vec![&msg1, &msg2];
        let result = merge_assistant_messages(&messages, &mut HashMap::new()).expect("合并应成功");

        let content = &result.assistant_response_message.content;
        assert!(content.contains("<thinking>"), "应包含 thinking 标签");
        assert!(
            content.contains("Let me read that file"),
            "应包含第二条消息的 text 内容"
        );

        let tool_uses = result
            .assistant_response_message
            .tool_uses
            .expect("应有 tool_uses");
        assert_eq!(tool_uses.len(), 1);
        assert_eq!(tool_uses[0].tool_use_id, "toolu_01ABC");
    }

    #[test]
    fn test_consecutive_assistant_with_tool_use_result_pairing() {
        // 测试 Issue #79 的完整场景
        use super::super::types::Message as AnthropicMessage;

        let req = MessagesRequest {
            model: "claude-sonnet-4".to_string(),
            max_tokens: 1024,
            messages: vec![
                AnthropicMessage {
                    role: "user".to_string(),
                    content: serde_json::json!("Read the config file"),
                },
                AnthropicMessage {
                    role: "assistant".to_string(),
                    content: serde_json::json!([
                        {"type": "thinking", "thinking": "I need to read the file..."},
                        {"type": "text", "text": " "}
                    ]),
                },
                AnthropicMessage {
                    role: "assistant".to_string(),
                    content: serde_json::json!([
                        {"type": "thinking", "thinking": "Let me read the config."},
                        {"type": "text", "text": "I'll read the config file for you."},
                        {"type": "tool_use", "id": "toolu_01XYZ", "name": "read_file", "input": {"path": "/config.json"}}
                    ]),
                },
                AnthropicMessage {
                    role: "user".to_string(),
                    content: serde_json::json!([
                        {"type": "tool_result", "tool_use_id": "toolu_01XYZ", "content": "{\"key\": \"value\"}"}
                    ]),
                },
            ],
            stream: false,
            system: None,
            tools: None,
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: None,
        };

        let result = convert_request(&req);
        assert!(
            result.is_ok(),
            "连续 assistant 消息场景不应报错: {:?}",
            result.err()
        );

        let state = result.unwrap().conversation_state;
        let mut found_tool_use = false;
        for msg in &state.history {
            if let Message::Assistant(assistant_msg) = msg {
                if let Some(ref tool_uses) = assistant_msg.assistant_response_message.tool_uses {
                    if tool_uses.iter().any(|t| t.tool_use_id == "toolu_01XYZ") {
                        found_tool_use = true;
                        break;
                    }
                }
            }
        }
        assert!(found_tool_use, "合并后的 assistant 消息应包含 tool_use");
    }

    // === B1 回归：tool_result 内的图片上浮到顶层 images ===

    /// 1x1 PNG 的 base64（测试用）
    const TINY_PNG_B64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+M8AAAMBAQDJ/pLvAAAAAElFTkSuQmCC";

    // 图片路径会走 block_in_place（见 extract_kiro_image），测试需多线程 runtime
    #[tokio::test(flavor = "multi_thread")]
    async fn test_tool_result_image_lifts_to_top_level() {
        use super::super::types::Message as AnthropicMessage;

        // user 提问 -> assistant tool_use -> user tool_result（含 image + text）
        let req = MessagesRequest {
            model: "claude-sonnet-4.5".to_string(),
            max_tokens: 1024,
            messages: vec![
                AnthropicMessage {
                    role: "user".to_string(),
                    content: serde_json::json!("take a screenshot"),
                },
                AnthropicMessage {
                    role: "assistant".to_string(),
                    content: serde_json::json!([
                        {"type": "tool_use", "id": "tool-1", "name": "screenshot", "input": {}}
                    ]),
                },
                AnthropicMessage {
                    role: "user".to_string(),
                    content: serde_json::json!([
                        {"type": "tool_result", "tool_use_id": "tool-1", "content": [
                            {"type": "text", "text": "here is the screen"},
                            {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": TINY_PNG_B64}}
                        ]}
                    ]),
                },
                // 追加一轮当前 user 消息，让上一轮 tool_result 进入历史（走去重/上浮路径）
                AnthropicMessage {
                    role: "user".to_string(),
                    content: serde_json::json!("what do you see?"),
                },
            ],
            stream: false,
            system: None,
            tools: None,
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: None,
        };

        let result = convert_request(&req).unwrap();

        // 图片应从历史 tool_result 上浮到某条历史 user 消息的顶层 images
        let mut found_image = false;
        let mut tool_result_text_ok = false;
        for msg in &result.conversation_state.history {
            if let Message::User(u) = msg {
                for img in &u.user_input_message.images {
                    if img.format == "png" && img.source.bytes == TINY_PNG_B64 {
                        found_image = true;
                    }
                }
                // tool_result 只保留文本，base64 不应出现在 tool_result content 里
                for tr in &u.user_input_message.user_input_message_context.tool_results {
                    if tr.tool_use_id == "tool-1" {
                        let text = tr.content[0]
                            .get("text")
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        assert_eq!(text, "here is the screen");
                        assert!(!text.contains(TINY_PNG_B64), "tool_result 不应含 base64");
                        tool_result_text_ok = true;
                    }
                }
            }
        }
        assert!(found_image, "tool_result 内的图片应上浮到顶层 images");
        assert!(tool_result_text_ok, "应找到保留文本的 tool_result");
    }

    #[test]
    fn test_tool_result_text_only_unchanged() {
        use super::super::types::Message as AnthropicMessage;

        // 纯文本 tool_result：回归不变，不应产生任何顶层图片
        let req = MessagesRequest {
            model: "claude-sonnet-4.5".to_string(),
            max_tokens: 1024,
            messages: vec![
                AnthropicMessage {
                    role: "user".to_string(),
                    content: serde_json::json!("read the file"),
                },
                AnthropicMessage {
                    role: "assistant".to_string(),
                    content: serde_json::json!([
                        {"type": "tool_use", "id": "tool-1", "name": "read", "input": {"path": "/a.txt"}}
                    ]),
                },
                AnthropicMessage {
                    role: "user".to_string(),
                    content: serde_json::json!([
                        {"type": "tool_result", "tool_use_id": "tool-1", "content": "file content"}
                    ]),
                },
                AnthropicMessage {
                    role: "user".to_string(),
                    content: serde_json::json!("thanks"),
                },
            ],
            stream: false,
            system: None,
            tools: None,
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: None,
        };

        let result = convert_request(&req).unwrap();
        for msg in &result.conversation_state.history {
            if let Message::User(u) = msg {
                assert!(
                    u.user_input_message.images.is_empty(),
                    "纯文本 tool_result 不应产生顶层图片"
                );
            }
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_current_message_image_always_kept() {
        // 当前轮消息（非历史）图片永远保留，不去重
        let content = serde_json::json!([
            {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": TINY_PNG_B64}},
            {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": TINY_PNG_B64}}
        ]);
        let (_text, images, _tr) = process_message_content(&content).unwrap();
        // 当前轮 dedup 为 None，两张相同图片都保留
        assert_eq!(images.len(), 2, "当前轮相同图片应全部保留");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_history_image_dedup() {
        // 历史路径：同一张图跨消息重复出现，只保留首次
        let mut dedup = std::collections::HashSet::new();
        let content = serde_json::json!([
            {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": TINY_PNG_B64}}
        ]);

        let (_t1, imgs1, _) = process_message_content_dedup(&content, Some(&mut dedup)).unwrap();
        assert_eq!(imgs1.len(), 1, "首次出现应保留图片");

        let (text2, imgs2, _) = process_message_content_dedup(&content, Some(&mut dedup)).unwrap();
        assert!(imgs2.is_empty(), "重复图片不应再次上浮");
        assert!(
            text2.contains("identical to an earlier screenshot"),
            "重复图片应替换为去重占位符"
        );
    }

    // === H3 回归：图片格式按 magic bytes 校正（客户端声明值不可信）===

    /// 造一张以 `magic` 开头、填充到 24 字节的假图，返回其 base64。
    ///
    /// 只需头部字节能被嗅探到，后续内容与判类型无关，故不必用真图。
    fn fake_image_b64(magic: &[u8]) -> String {
        use base64::Engine;
        let mut bytes = magic.to_vec();
        bytes.resize(24, 0x00);
        base64::engine::general_purpose::STANDARD.encode(&bytes)
    }

    /// 走真实调用点（`process_message_content` → `extract_kiro_image`）取下发格式。
    ///
    /// 直接测 `resolve_image_format` 会变成纸面测试：函数本身对了但调用点没接上一样是 400。
    fn format_via_real_path(media_type: &str, data: &str) -> Option<String> {
        let content = serde_json::json!([
            {"type": "image", "source": {"type": "base64", "media_type": media_type, "data": data}}
        ]);
        let (_text, images, _tr) = process_message_content(&content).unwrap();
        images.first().map(|img| img.format.clone())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_image_format_corrected_to_jpeg_by_magic_bytes() {
        let data = fake_image_b64(&[0xFF, 0xD8, 0xFF, 0xE0]);
        assert_eq!(
            format_via_real_path("image/png", &data).as_deref(),
            Some("jpeg"),
            "声明 png 而字节是 jpeg，应按 magic bytes 纠正为 jpeg"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_image_format_corrected_to_png_by_magic_bytes() {
        let data = fake_image_b64(&[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]);
        assert_eq!(
            format_via_real_path("image/jpeg", &data).as_deref(),
            Some("png"),
            "声明 jpeg 而字节是 png，应按 magic bytes 纠正为 png"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_image_format_corrected_to_gif_by_magic_bytes() {
        let data = fake_image_b64(b"GIF89a");
        assert_eq!(
            format_via_real_path("image/webp", &data).as_deref(),
            Some("gif"),
            "声明 webp 而字节是 gif，应按 magic bytes 纠正为 gif"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_image_format_corrected_to_webp_by_magic_bytes() {
        // RIFF + 4 字节长度占位 + WEBP：偏移 8 处的 WEBP 必须一起验，否则 wav/avi 也会命中
        let mut magic = b"RIFF".to_vec();
        magic.extend_from_slice(&[0x10, 0x00, 0x00, 0x00]);
        magic.extend_from_slice(b"WEBP");
        let data = fake_image_b64(&magic);
        assert_eq!(
            format_via_real_path("image/png", &data).as_deref(),
            Some("webp"),
            "声明 png 而字节是 webp，应按 magic bytes 纠正为 webp"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_image_format_keeps_declared_when_magic_unknown() {
        // 不匹配任何 magic：保留声明值，不猜——瞎猜会把上游本来能接受的格式改坏
        let data = fake_image_b64(&[0x00, 0x01, 0x02, 0x03]);
        assert_eq!(
            format_via_real_path("image/png", &data).as_deref(),
            Some("png"),
            "magic 认不出时应保留客户端声明的 png"
        );
        // RIFF 但偏移 8 不是 WEBP（wav 容器）同样算认不出
        let mut wav = b"RIFF".to_vec();
        wav.extend_from_slice(&[0x10, 0x00, 0x00, 0x00]);
        wav.extend_from_slice(b"WAVE");
        assert_eq!(
            format_via_real_path("image/gif", &fake_image_b64(&wav)).as_deref(),
            Some("gif"),
            "RIFF/WAVE 不是 webp，应保留声明的 gif"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_image_format_unchanged_when_declaration_matches_magic() {
        // 真 1x1 PNG：声明与 magic 一致，格式不变
        assert_eq!(
            format_via_real_path("image/png", TINY_PNG_B64).as_deref(),
            Some("png"),
            "声明与 magic 一致时格式应保持 png"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_image_format_unsupported_declaration_rescued_by_magic() {
        // 声明是不支持的 media_type 但字节认得出：旧行为整张图无声丢弃，现在按 magic 下发
        let data = fake_image_b64(&[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]);
        assert_eq!(
            format_via_real_path("image/bmp", &data).as_deref(),
            Some("png"),
            "声明 image/bmp 而字节是 png，应按 magic 下发 png 而非丢图"
        );
        // 真 BMP（magic `BM` 不在判据内）仍认不出 → 声明值也不支持 → 维持旧的丢弃行为
        assert!(
            format_via_real_path("image/bmp", &fake_image_b64(b"BM")).is_none(),
            "magic 与声明都定不出格式时应维持旧的无声跳过"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_image_format_sniff_tolerates_data_url_prefix_and_newlines() {
        // 客户端偶发 data: 前缀 / 带换行的 base64，剥不掉就退化成"认不出"、纠正失效
        let jpeg = fake_image_b64(&[0xFF, 0xD8, 0xFF, 0xDB]);
        let with_prefix = format!("data:image/png;base64,{}", jpeg);
        assert_eq!(
            format_via_real_path("image/png", &with_prefix).as_deref(),
            Some("jpeg"),
            "带 data: 前缀时仍应按 magic bytes 纠正"
        );

        let wrapped = format!("{}\n{}", &jpeg[..8], &jpeg[8..]);
        assert_eq!(
            format_via_real_path("image/png", &wrapped).as_deref(),
            Some("jpeg"),
            "base64 带换行时仍应按 magic bytes 纠正"
        );
    }
