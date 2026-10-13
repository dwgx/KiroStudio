// Tests for anthropic handlers. Loaded via #[path] from handlers.rs (parent stays a file).
use super::*;

#[cfg(test)]
mod non_stream_cache_accounting_tests {
    //! 非流式路径的 cache 记账：埋点必须与返回客户端的 usage.cache_* 同源。
    //! 历史缺陷：埋点块漏写 cache_read_tokens/cache_creation_tokens，
    //! 客户端拿到 cache_read=12000 而落库恒 0。
    use super::*;

    fn new_record() -> crate::usage::RequestRecord {
        crate::usage::RequestRecord::new("req-1", "claude-sonnet-5")
    }

    #[test]
    fn should_write_cache_read_and_creation_from_breakdown() {
        let mut record = new_record();
        // 契约：先设 gross input_tokens，再写 cache（apply 内部按 gross 收敛上限）
        record.input_tokens = 20000;
        apply_cache_breakdown(
            &mut record,
            Some(CacheUsageBreakdown {
                cache_creation_input_tokens: 300,
                cache_read_input_tokens: 12000,
                cache_creation_5m_input_tokens: 300,
                cache_creation_1h_input_tokens: 0,
            }),
        );
        assert_eq!(record.cache_read_tokens, 12000, "cache_read 必须落库");
        assert_eq!(record.cache_creation_tokens, 300, "cache_creation 必须落库");
    }

    #[test]
    fn should_clamp_cache_read_to_gross_input_when_context_estimate_is_lower() {
        // cache_read 由本地前缀估算并按本地 count_all_tokens clamp（=12000），
        // 而落库 input_tokens 取 contextUsageEvent 百分比反推值（=5000）。
        // 两者不同源，反推值偏小时会产出 cache_read > input_tokens 的矛盾记录。
        let mut record = new_record();
        record.input_tokens = 5000;
        apply_cache_breakdown(
            &mut record,
            Some(CacheUsageBreakdown {
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 12000,
                cache_creation_5m_input_tokens: 0,
                cache_creation_1h_input_tokens: 0,
            }),
        );
        assert_eq!(
            record.cache_read_tokens, 5000,
            "cache_read 不得超过 gross input_tokens"
        );
        assert_eq!(record.billed_input_tokens(), 0, "billed 不得为负");
    }

    /// `promptCacheEnabled=false` 必须让记账**整体缺失**（None），不是 Some(全 0)。
    ///
    /// 这个区别对客户端是实质性的：`cache_read_input_tokens: 0` 表示"确实一次都没命中"，
    /// 字段缺失表示"本网关不做该记账"。注入 0 会把"未记账"误报成"缓存全未命中"。
    #[test]
    fn should_omit_cache_breakdown_entirely_when_disabled() {
        assert!(
            estimate_cache_breakdown(false, 12_000, 20_000).is_none(),
            "关闭时必须返回 None（字段缺失），不能是 Some(0)"
        );
        // 开启且有前缀 → 正常记账
        let on = estimate_cache_breakdown(true, 12_000, 20_000).expect("开启时应有记账");
        assert_eq!(on.cache_read_input_tokens, 12_000);
    }

    /// 首轮请求（无历史前缀）在开启时也应为 None —— 没有可复用前缀就不该声称命中。
    #[test]
    fn should_omit_cache_breakdown_when_no_prefix_tokens() {
        assert!(estimate_cache_breakdown(true, 0, 20_000).is_none());
        assert!(estimate_cache_breakdown(true, -1, 20_000).is_none());
    }

    /// 标注头只在**真的下发了** cache 字段时出现，否则头与响应体自相矛盾。
    ///
    /// 三条响应路径（非流式 / 流式 SSE / buffered SSE）都用同一个判据
    /// `cache_breakdown.is_some()` —— 与 estimate_cache_breakdown 的返回一致。
    /// 这条测试守的是「判据同源」：只要下发条件变了，标注条件必须跟着变。
    #[test]
    fn should_mark_estimated_only_when_cache_fields_are_sent() {
        // 开启且有前缀 → 下发字段 → 应标注
        let sent = estimate_cache_breakdown(true, 12_000, 20_000);
        assert!(sent.is_some(), "应下发 cache 字段");

        // 开关关闭 → 不下发 → 不应标注
        assert!(
            estimate_cache_breakdown(false, 12_000, 20_000).is_none(),
            "关闭时不下发，故不应加标注头"
        );
        // 首轮无前缀 → 不下发 → 不应标注
        assert!(
            estimate_cache_breakdown(true, 0, 20_000).is_none(),
            "无前缀命中时不下发，故不应加标注头"
        );
    }

    /// 头名与值必须是合法 HTTP 头（大小写、非法字符会在运行时 panic 而非编译期报错）。
    #[test]
    fn should_use_valid_lowercase_header_name_and_value() {
        assert_eq!(
            CACHE_ESTIMATED_HEADER,
            CACHE_ESTIMATED_HEADER.to_ascii_lowercase(),
            "HTTP/2 要求头名小写，写成大写会在某些客户端上出问题"
        );
        // from_static 对非法值会 panic —— 这里显式构造一次，把 panic 暴露在测试而非生产
        let v = cache_estimated_header_value();
        assert_eq!(v.to_str().unwrap(), "true");
        assert!(
            axum::http::HeaderName::try_from(CACHE_ESTIMATED_HEADER).is_ok(),
            "头名必须是合法 HeaderName"
        );
    }

    /// 缩放头名必须是合法 HTTP/2 小写；值锁定 `0.6657`（禁止 `format!("{}", f64)`）。
    #[test]
    fn should_use_valid_lowercase_header_name_and_input_token_scale_value() {
        assert_eq!(
            INPUT_TOKEN_SCALE_HEADER,
            INPUT_TOKEN_SCALE_HEADER.to_ascii_lowercase(),
            "HTTP/2 要求头名小写，写成大写会在某些客户端上出问题"
        );
        assert_eq!(
            CLIENT_TOKEN_DISPLAY_SCALE_HEADER, "0.6657",
            "缩放头值必须与 CLIENT_TOKEN_DISPLAY_SCALE 同一十进制字面量"
        );
        assert!(
            axum::http::HeaderName::try_from(INPUT_TOKEN_SCALE_HEADER).is_ok(),
            "头名必须是合法 HeaderName"
        );
        let v = axum::http::HeaderValue::from_static(CLIENT_TOKEN_DISPLAY_SCALE_HEADER);
        assert_eq!(v.to_str().unwrap(), "0.6657");
    }

    /// 三条 SSE 路径必须走 `sse_event_stream_builder`；生产段 helper 外不得再手写
    /// `text/event-stream` Content-Type。
    #[test]
    fn input_token_scale_sse_sites_must_use_shared_builder() {
        let src = include_str!("handlers.rs");
        let cut = src.find("#[cfg(test)]").unwrap_or(src.len());
        let prod = &src[..cut];

        let helper_fn = format!("{}{}", "fn sse_event_stream_builder", "()");
        let start = prod
            .find(helper_fn.as_str())
            .expect("sse_event_stream_builder 必须存在");
        let rest = &prod[start..];
        let end = rest.find("\n}\n").expect("helper 必须有结尾");
        let helper_src = &rest[..end];
        let without_helper = format!("{}{}", &prod[..start], &rest[end..]);

        let sse_ct = format!(
            ".header({}::CONTENT_TYPE, \"text/event-stream\")",
            "header"
        );
        assert!(
            helper_src.contains(sse_ct.as_str()),
            "helper 必须设置 content-type: text/event-stream"
        );
        assert!(
            !without_helper.contains(sse_ct.as_str()),
            "生产段 helper 之外不得再手写 text/event-stream Content-Type"
        );

        let helper_call = format!("{}{}", "sse_event_stream_builder", "()");
        assert_eq!(
            prod.matches(helper_call.as_str()).count(),
            4,
            "helper 定义 1 处 + 三条 SSE 路径各 1 处调用"
        );
    }

    /// 前缀估算超过总输入时必须收敛到总输入（两个数字不同源，见 clamp_cache_to_input）。
    #[test]
    fn should_clamp_estimated_prefix_to_input_tokens() {
        let c = estimate_cache_breakdown(true, 99_000, 4_000).expect("应有记账");
        assert_eq!(
            c.cache_read_input_tokens, 4_000,
            "cache_read 不得超过本次输入总量"
        );
    }

    #[test]
    fn should_write_zero_when_no_cache_breakdown() {
        let mut record = new_record();
        record.cache_read_tokens = 999;
        record.cache_creation_tokens = 999;
        apply_cache_breakdown(&mut record, None);
        assert_eq!(record.cache_read_tokens, 0, "首轮无前缀缓存应记 0");
        assert_eq!(record.cache_creation_tokens, 0, "首轮无前缀缓存应记 0");
    }

    /// Layer 1：上游 metering 真值优先于一切本地估算，且不标注「估算」。
    #[test]
    fn should_use_metering_truth_over_estimate() {
        let (bd, estimated) = resolve_cache_chain(true, 1000, None, None, Some(600), Some(200));
        let bd = bd.expect("metering 真值应产出记账");
        assert_eq!(bd.cache_read_input_tokens, 600);
        assert_eq!(bd.cache_creation_input_tokens, 200);
        assert_eq!(bd.cache_creation_5m_input_tokens, 200);
        assert_eq!(bd.cache_creation_1h_input_tokens, 0);
        assert!(!estimated, "真值不应标「估算」头");
    }

    /// Layer 1 真值 > total 时按 clamp_to_total 收敛（优先保留 read）。
    #[test]
    fn should_clamp_metering_truth_to_total() {
        let (bd, _) = resolve_cache_chain(true, 100, None, None, Some(80), Some(50));
        let bd = bd.expect("应有记账");
        assert_eq!(bd.cache_read_input_tokens, 80);
        assert_eq!(bd.cache_creation_input_tokens, 20);
    }

    /// Layer 1 真值不受 `promptCacheEnabled=false` 约束（真值不是估算）。
    #[test]
    fn should_record_metering_truth_even_when_disabled() {
        let (bd, estimated) = resolve_cache_chain(false, 1000, None, None, Some(400), Some(100));
        assert!(bd.is_some(), "真值应照记，即使开关关");
        assert!(!estimated);
    }

    /// 开关关且无 metering 真值 → 整体缺失（None），不凭空造 cache 命中。
    #[test]
    fn should_omit_entirely_when_disabled_and_no_metering() {
        let (bd, estimated) = resolve_cache_chain(false, 1000, Some(estimate_cache_breakdown(true, 500, 1000).unwrap()), None, None, None);
        assert!(bd.is_none(), "关闭时不得下发 cache 记账");
        assert!(!estimated);
    }

    /// Layer 2：无 metering 时回落 prefix 估算（既有行为）。
    #[test]
    fn should_fall_back_to_prefix_estimate() {
        let (bd, estimated) = resolve_cache_chain(true, 1000, Some(estimate_cache_breakdown(true, 400, 1000).unwrap()), None, None, None);
        let bd = bd.expect("prefix 估算应产出记账");
        assert_eq!(bd.cache_read_input_tokens, 400);
        assert_eq!(bd.cache_creation_input_tokens, 0);
        assert!(estimated, "估算应标「估算」头");
    }

    /// ⭐ Layer 3 回归（对抗审查 MAJOR 1，2026-08-11）：fingerprint 与 prefix 估算
    /// **同时存在**时必须走 Layer 3 —— fingerprint 的 creation 绝不能被 Layer 2 分支
    /// （只读 read、creation 硬置 0）吞掉。回退即 FAIL：把 resolve_cache_chain 里的
    /// `if fingerprint_usage.is_some() { None } else { ... }` 改回直接 map。
    #[test]
    fn fingerprint_wins_over_prefix_and_keeps_creation() {
        // 构造：prefix 估算 read=400（Layer 2 若赢：creation=0）；
        // fingerprint（Layer 3）：read=250、creation=120。
        let fp = crate::anthropic::cache::PromptCacheUsage {
            cache_creation_input_tokens: 120,
            cache_read_input_tokens: 250,
            cache_creation_5m_input_tokens: 120,
            cache_creation_1h_input_tokens: 0,
        };
        let (bd, estimated) = resolve_cache_chain(
            true,
            1000,
            Some(estimate_cache_breakdown(true, 400, 1000).unwrap()),
            Some(fp),
            None,
            None,
        );
        let bd = bd.expect("fingerprint 应产出记账");
        assert_eq!(
            bd.cache_creation_input_tokens, 120,
            "Layer 3 的 creation 不得被 Layer 2 吞成 0（非流式路径曾恒 0）"
        );
        assert_eq!(bd.cache_read_input_tokens, 250, "Layer 3 的 read 优先于 Layer 2 的 400");
        assert!(estimated);
    }

    /// Layer 4：无 metering、无 prefix 时 ratio 兜底（50% cache / 30% creation）。
    #[test]
    fn should_fall_back_to_ratio_when_no_estimate() {
        let (bd, estimated) = resolve_cache_chain(true, 1000, None, None, None, None);
        let bd = bd.expect("ratio 兜底应产出记账");
        // 50% × 1000 = 500 cache，creation = 150，read = 350。
        assert_eq!(bd.cache_read_input_tokens, 350);
        assert_eq!(bd.cache_creation_input_tokens, 150);
        assert!(estimated, "ratio 也是估算，应标「估算」头");
    }

    /// 🔴 源码级守卫：`contextUsageEvent` 的判据必须**与流式路径共用同一个函数**，
    /// 不得在本文件里重新算一遍。
    ///
    /// # 为什么需要这条
    ///
    /// 这个判据有两个调用点（流式 `StreamContext` / 非流式本文件的缓冲聚合循环）。
    /// 它们曾是两份独立实现，**只有流式那份有下界守卫** ⇒ 同一个上游异常在两条路径上
    /// 表现不同：流式忽略脏值，非流式把计费口径的 `input_tokens` 写成 0
    /// （或 NaN 经 `as i32` 饱和成的 `i32::MAX`）。
    ///
    /// 本仓已多次踩「同一判据两份实现、只修了其中一份」这个形态（`endpoint_for` 与
    /// `for_credentials`、`restart_fields` 与 reload restore 表、`cleanup_verdict` 与
    /// `batch-delete`）。给第二处也加个守卫治不了根 —— 两份实现仍会各自演化。
    /// 本守卫钉的是**物理共用**：本文件不得出现自己的百分比乘算。
    #[test]
    fn context_usage_predicate_must_be_shared() {
        let src = include_str!("handlers.rs");
        let cut = src.find("#[cfg(test)]").unwrap_or(src.len());
        // 只看生产段，且剔掉注释行（否则本守卫会匹配到注释里的说明文字或被注释掉的实现，
        // 变成「把实现注释掉守卫仍绿」的纸面测试 —— 该形态本轮实测踩过一次）。
        // ⚠️ 还要把连续空白归一成单空格。否则 rustfmt 在表达式中间插一个换行就能让
        // 下面的反向断言失配 ⇒ 守卫静默失效（本轮实测踩到：手工回退时那句乘算被格式化成
        // 三行，含换行的 needle 匹配不上，守卫报绿）。归一后断言与排版无关。
        let prod: String = src[..cut]
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");

        // needle 运行时拼接，避免 include_str! 把本测试自己的字面量算进匹配。
        let shared_call = format!("stream::context_input_tokens_from_pct{}", "(");
        assert!(
            prod.contains(&shared_call),
            "非流式路径必须调用共享判据 `context_input_tokens_from_pct`，\
             不得自行判定 —— 否则两条路径会再次分叉（只有一侧有下界守卫）"
        );

        // 反向断言：不得再出现自己的乘算。历史实现是
        // `context_usage.context_usage_percentage * (window_size as f64) / 100.0`。
        let own_math = format!("context_usage_percentage {}", "* (window_size as f64)");
        assert!(
            !prod.contains(&own_math),
            "本文件不得自行用百分比乘算 input_tokens（发现历史实现的形状）：\
             那正是下界守卫缺失的那一份。改为调用 `context_input_tokens_from_pct`。"
        );
    }

    /// 源码级守卫：非流式成功埋点块必须调用 [`apply_cache_breakdown`]。
    /// 纯单测覆盖不到 `handle_non_stream_request`（需真实上游 + `CallMeta`/`InflightGuard`），
    /// 故用本文件源码断言把"埋点块漏写 cache 字段"这一具体回归钉死。
    #[test]
    fn should_call_apply_cache_breakdown_in_non_stream_emit_block() {
        let src = include_str!("handlers.rs");
        // needle 运行时拼接：生产注释含「或空响应」（P1-2），不得写完整字面量
        // 否则 include_str 命中本测试自己 → 切片落到测试段、守卫假红。
        let marker = format!(
            "{}{}",
            "// 用量埋点：非流式成功",
            "或空响应（completion 保持 Ok，不进 report_failure）"
        );
        let block = src
            .split(marker.as_str())
            .nth(1)
            .expect("非流式成功埋点块的定位注释不应被删改");
        let block = block
            .split("crate::usage::emit_record(record);")
            .next()
            .expect("埋点块应以 emit_record 收尾");
        assert!(
            block.contains("apply_cache_breakdown(&mut record, final_cache_breakdown)"),
            "非流式成功埋点块必须写入 cache 字段(四层降级链收敛后的 final_cache_breakdown),否则落库与客户端 usage 矛盾"
        );
    }
}

/// 测试串行锁:IP/机器码黑名单是进程级全局静态(ArcSwap 镜像),多个测试并行读写会互相污染
/// (一个测试清空黑名单会让另一个测试的命中断言失败)。凡改这些全局态的测试都先取此锁,串行执行。
#[cfg(test)]
static BLOCKLIST_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod ip_blocklist_tests {
    //! 业务层 IP 黑名单:按真实客户端 IP(XFF 首段)封禁,反代后也生效。
    use super::*;

    #[test]
    fn test_ip_blocklist_business_layer() {
        let _guard = BLOCKLIST_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // 空黑名单:任何 IP 都不拦。
        set_ip_blocklist(&[]);
        assert!(!ip_is_blocked("223.73.32.14"));
        // 设单 IP + 子网。
        set_ip_blocklist(&["223.73.32.14/32".to_string(), "10.0.0.0/8".to_string()]);
        assert!(ip_is_blocked("223.73.32.14"), "命中单 IP 应拦");
        assert!(ip_is_blocked("10.1.2.3"), "命中子网应拦");
        assert!(!ip_is_blocked("8.8.8.8"), "不在黑名单应放行");
        assert!(!ip_is_blocked("not-an-ip"), "非法 IP 字符串不拦(不 panic)");
        // 清空恢复(避免污染其它测试的全局镜像)。
        set_ip_blocklist(&[]);
        assert!(!ip_is_blocked("223.73.32.14"));
    }
}

#[cfg(test)]
mod machine_code_blocklist_tests {
    //! 业务层机器码黑名单:按当前请求真实客户端 IP 重算机器码,命中即拒(消息 sbsbsb！)。
    use super::*;

    #[test]
    fn test_machine_code_blocklist_business_layer() {
        let _guard = BLOCKLIST_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // 空黑名单:任何机器码都不拦。
        set_machine_code_blocklist(&[]);
        let code = crate::usage::machine_code_of(Some("223.73.32.14"), Some("claude-code"));
        assert!(!machine_code_is_blocked(&code));

        // 拉黑该机器码后命中。
        set_machine_code_blocklist(&[code.clone()]);
        assert!(machine_code_is_blocked(&code), "命中机器码应拦");
        // 大小写不敏感。
        assert!(
            machine_code_is_blocked(&code.to_uppercase()),
            "大写形式也应命中"
        );
        // 另一台机器(不同 IP → 不同码)不受影响。
        let other = crate::usage::machine_code_of(Some("8.8.8.8"), Some("claude-code"));
        assert!(!machine_code_is_blocked(&other), "未拉黑的机器码应放行");

        // 有 IP 时 device 不影响判定(machine_key = IP)。
        let same_ip_diff_dev = crate::usage::machine_code_of(Some("223.73.32.14"), Some("vscode"));
        assert!(
            machine_code_is_blocked(&same_ip_diff_dev),
            "同 IP 不同 device 仍应命中"
        );

        // 清空恢复(避免污染其它测试的全局镜像)。
        set_machine_code_blocklist(&[]);
        assert!(!machine_code_is_blocked(&code));
    }

    // F2 回归:安全封禁网关独立于 collect_client_fingerprint 隐私开关。
    // 网关直接从请求头解析真实 IP(不走 ClientInfo,后者关指纹时返回空 IP 会让黑名单失效)。
    #[test]
    fn test_security_gate_independent_of_fingerprint_flag() {
        use axum::http::HeaderMap;
        use std::net::SocketAddr;
        use std::sync::atomic::Ordering;

        let _guard = BLOCKLIST_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());

        // 反代场景:对端=本机 openresty(127.0.0.1),XFF 最右=反代追加的真实客户端 IP。
        // (A1:最右不可伪造;此处 223.73.32.14 是反代追加的真实 IP。)
        let proxy_peer: Option<SocketAddr> = Some("127.0.0.1:9999".parse().unwrap());
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", "10.9.9.9, 223.73.32.14".parse().unwrap());

        // 记录并强制关闭指纹采集(模拟 collect_client_fingerprint=false)。
        let saved = COLLECT_CLIENT_FINGERPRINT.load(Ordering::Relaxed);
        COLLECT_CLIENT_FINGERPRINT.store(false, Ordering::Relaxed);

        // 场景 A:IP 黑名单命中——即便关指纹,网关仍按 XFF 最右真实 IP 拦截(403)。
        set_ip_blocklist(&["223.73.32.14/32".to_string()]);
        set_machine_code_blocklist(&[]);
        let resp = security_block_response(&headers, proxy_peer);
        assert!(resp.is_some(), "关指纹时 IP 黑名单仍应生效(F2)");
        assert_eq!(resp.unwrap().status(), StatusCode::FORBIDDEN);

        // 场景 B:机器码黑名单命中——按真实 IP 重算的码,关指纹也拦。
        set_ip_blocklist(&[]);
        let code = crate::usage::machine_code_of(Some("223.73.32.14"), None);
        set_machine_code_blocklist(&[code.clone()]);
        let resp = security_block_response(&headers, proxy_peer);
        assert!(resp.is_some(), "关指纹时机器码黑名单仍应生效(F2)");
        assert_eq!(resp.unwrap().status(), StatusCode::FORBIDDEN);

        // 场景 C:都不命中→放行(None)。
        set_machine_code_blocklist(&[]);
        assert!(
            security_block_response(&headers, proxy_peer).is_none(),
            "未命中应放行"
        );

        // 恢复全局状态,避免污染其它测试。
        set_ip_blocklist(&[]);
        set_machine_code_blocklist(&[]);
        COLLECT_CLIENT_FINGERPRINT.store(saved, Ordering::Relaxed);
    }

    /// 回归（已知问题 #6）：handler 层必须遵守 `trust_forwarded_header`。
    ///
    /// **旧代码为何 FAIL**：`trusted_client_ip` 自己实现了一份判定，只看
    /// `is_trusted_proxy_peer(peer)`（对端是否私网/环回），**根本没有读**
    /// `config.trust_forwarded_header` —— 该 flag 在 `main.rs` 里只喂给了 `SecurityState`。
    /// 于是对端是**公网**反代时，无论开关开没开，handler 都退回 `peer`，
    /// 本测试第二段断言（应取 XFF 最右段）必然 FAIL。
    ///
    /// 生产后果：反代在公网 IP（CDN 直连 / 跨网段 LB）且管理员开了 `trustForwardedHeader=true` 时，
    /// security 中间件按 XFF 最右段判真实客户端，而业务层退回反代公网 IP →
    /// **IP 黑名单实际封的是反代自己**，一封就封掉全部用户；且所有客户端共享同一个机器码，
    /// 机器码黑名单同样一封封全部。
    #[test]
    fn test_trusted_client_ip_respects_trust_forwarded_header_config() {
        use axum::http::HeaderMap;
        use std::net::SocketAddr;

        let _guard = BLOCKLIST_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());

        // 反代在**公网** IP —— 这是本缺陷唯一的受害场景（私网对端两种实现结果相同，测不出差异）。
        let public_proxy: Option<SocketAddr> = Some("203.0.113.99:443".parse().unwrap());
        let mut h = HeaderMap::new();
        h.insert("x-forwarded-for", "1.2.3.4, 198.51.100.7".parse().unwrap());

        // 开关关（默认）：忽略 XFF，用对端 —— 直连客户端伪造 XFF 时这是正确行为。
        set_trust_forwarded_header(false);
        assert_eq!(
            trusted_client_ip(&h, public_proxy).as_deref(),
            Some("203.0.113.99"),
            "开关关闭时应忽略公网对端的 XFF（防伪造）"
        );

        // 开关开：应采信 XFF **最右**段（反代追加的、不可伪造的那段），与 security 中间件同口径。
        set_trust_forwarded_header(true);
        assert_eq!(
            trusted_client_ip(&h, public_proxy).as_deref(),
            Some("198.51.100.7"),
            "开关开启时必须采信 XFF 最右段（旧代码无视该配置，恒返回反代 IP → 黑名单封掉反代自己）"
        );

        // 复位，避免污染同进程内其它测试（进程级 atomic 是全局状态）。
        set_trust_forwarded_header(false);
    }

    // A1 回归:业务层客户端 IP 取 XFF **最右**(不可伪造),客户端伪造的最左前缀不改变封禁。
    // A2 回归:对端是可信反代(私网)才采信 XFF;公网直连忽略伪造 XFF 用对端。
    #[test]
    fn test_trusted_client_ip_a1_a2_forgery_resistance() {
        use axum::http::HeaderMap;
        use std::net::SocketAddr;

        let _guard = BLOCKLIST_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());

        let proxy_peer: Option<SocketAddr> = Some("127.0.0.1:8990".parse().unwrap());

        // A1:反代后,XFF = "<客户端伪造>, <反代追加的真实IP>",取最右=真实 IP。
        let mut h = HeaderMap::new();
        h.insert("x-forwarded-for", "8.8.8.8, 203.0.113.7".parse().unwrap());
        assert_eq!(
            trusted_client_ip(&h, proxy_peer).as_deref(),
            Some("203.0.113.7"),
            "反代后应取 XFF 最右真实 IP,不受最左伪造影响"
        );

        // A1 核心:攻击者把自己真实流量伪装成被封 IP——无论前缀怎么伪造,判定结果不变。
        set_ip_blocklist(&["203.0.113.7/32".to_string()]);
        let mut forged = HeaderMap::new();
        // 攻击者(真实 203.0.113.7)想改前缀嫁祸/绕过:仍被反代把真实 IP 追加到最右。
        forged.insert("x-forwarded-for", "1.2.3.4, 203.0.113.7".parse().unwrap());
        assert!(
            security_block_response(&forged, proxy_peer).is_some(),
            "伪造前缀不能绕过对真实最右 IP 的封禁"
        );
        set_ip_blocklist(&[]);

        // A2:对端是公网(客户端直连,非反代)→ 忽略可伪造的 XFF,用对端 IP。
        let public_peer: Option<SocketAddr> = Some("198.51.100.22:5000".parse().unwrap());
        let mut spoof = HeaderMap::new();
        spoof.insert("x-forwarded-for", "10.0.0.1, 203.0.113.7".parse().unwrap());
        assert_eq!(
            trusted_client_ip(&spoof, public_peer).as_deref(),
            Some("198.51.100.22"),
            "公网直连应忽略 XFF,用对端 IP(防直连客户端伪造 XFF)"
        );

        // 直连无 XFF → 回退对端。
        let empty = HeaderMap::new();
        assert_eq!(
            trusted_client_ip(&empty, public_peer).as_deref(),
            Some("198.51.100.22"),
            "无 XFF 应回退对端 IP"
        );
    }
}

/// `pub(crate)`：websearch.rs 的 f3 测试也改 ERROR_MESSAGES 全局镜像（跨模块共享，
/// 2026-08-15 并发审计），必须能取到本 mod 的锁——mod 私有会让外部访问
/// `handlers::error_translation_tests::ERROR_MESSAGES_TEST_LOCK` 报 module is private。
#[cfg(test)]
pub(crate) mod error_translation_tests {
    //! 错误翻译层：已确证含义的上游错误 → 带排障步骤的可读错误；未知错误诚实透传（None）。
    use super::*;

    /// ⭐ 致命缺陷回归（旧代码必失败）：上游账户级 429 曾被映射成 502 且无 Retry-After。
    ///
    /// 旧代码路径：该错误串匹配不上任何 translate_* 分支（translate_network 有
    /// is_transport_error 闸门挡住）→ translate_upstream_error 返 None → map_provider_error
    /// 落到兜底 → 502 BAD_GATEWAY。客户端（Claude Code）把 502 当服务故障、退避逻辑不启动、
    /// 立刻重发 → 撞进上游惩罚窗口（实测窗口内命中率 47.2%）→ 单次拒绝被放大成
    /// 最长 52min/431 次的持续发作（当天 3 个长发作占全部 429 的 84%）。
    /// ⭐ 回归（走真实 `map_provider_error` 出口）：400 `INSUFFICIENT_MODEL_CAPACITY`
    /// 必须映射成 **503 `overloaded_error`**，而不是兜底的 502。
    ///
    /// 这是与上面那条 429 缺陷**完全同型**的第二例，只是形态不同：
    /// 上游发的是 HTTP 400 + `ThrottlingException` + `reason:INSUFFICIENT_MODEL_CAPACITY`
    /// （实测 24h **272 次**）。它逐条落空所有分支 → 落末尾兜底 →
    /// **502 Bad Gateway 且无 Retry-After** → 客户端按永久性服务端故障处理 →
    /// 不退避、原样重发 → 在上游容量本就不足时继续加压。
    ///
    /// 断言落在**客户端实际看到的状态码**上，不是只断言谓词函数 ——
    /// 后者是纸面测试（本仓已踩过两次）：把 `translate_upstream_error` 里那条
    /// `|| err_str.contains("INSUFFICIENT_MODEL_CAPACITY")` 删掉，谓词测试仍会全绿。
    ///
    /// 删掉那条 → 本测试必 FAILED（实得 502）。
    #[test]
    fn insufficient_model_capacity_maps_to_503_not_bad_gateway() {
        let raw = r#"流式 API 请求失败: 400 Bad Request {"__type":"com.amazon.aws.codewhisperer#ThrottlingException","message":"I am experiencing high traffic, please try again shortly.","reason":"INSUFFICIENT_MODEL_CAPACITY"}"#;
        let resp = map_provider_error(anyhow::Error::msg(raw));
        assert_eq!(
            resp.status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "容量不足必须返 503（客户端会退避重试），而非兜底的 502（客户端当永久故障不退避）"
        );
        // 对照：既有的 503 形态必须仍然同样映射，不得因本次改动漂移。
        let legacy = r#"流式 API 请求失败: 503 Service Unavailable {"reason":"MODEL_TEMPORARILY_UNAVAILABLE"}"#;
        assert_eq!(
            map_provider_error(anyhow::Error::msg(legacy)).status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "既有 MODEL_TEMPORARILY_UNAVAILABLE 形态不得回退"
        );
    }

    #[test]
    fn test_upstream_429_maps_to_429_with_retry_after() {
        // provider 实际组装的错误串原文（含 HTTP 状态码 + 上游 body）。
        let raw = r#"流式 API 请求失败: 429 Too Many Requests {"message":"Too many requests, please wait before trying again.","reason":"USER_REQUEST_RATE_EXCEEDED"}"#;
        let err = anyhow::Error::msg(raw);
        let resp = map_provider_error(err);
        assert_eq!(
            resp.status(),
            StatusCode::TOO_MANY_REQUESTS,
            "上游速率限流必须映射成 429（旧代码返 502 → 客户端不退避）"
        );
        let hv = resp
            .headers()
            .get(header::RETRY_AFTER)
            .expect("上游 429 必须带 Retry-After 头，否则客户端退避逻辑不启动");
        assert_eq!(hv.to_str().unwrap(), "8");
    }

    /// ⭐ 致命缺陷回归（去掉 `retry_after_secs=` 标记即必失败）：**号池真耗尽**曾落 502 无 Retry-After。
    ///
    /// 与上面那条是同一类缺陷的不同实例。0.7.45 只修了情形②（模型硬门，加
    /// `model_unsupported_by_pool=1` 标记），情形①「available == 0 真耗尽」当时未处理，
    /// 而它才是量最大的那个：
    ///
    /// 线上 2026-08-03 01:55–02:10 号池被烧空的 15 分钟窗口里，`所有凭据均已禁用（0/0）`
    /// 产生 2082 次，单个 5 分钟桶峰值 937 次 —— 且该窗口内**未识别兜底 502 全部是这一种**。
    ///
    /// 旧路径：该串既无 `retry_after_secs=`、也无 `model_unsupported_by_pool=1`、不含
    /// QUOTA 等上游关键词、`is_transport_error` 也不认 → 逐条穿过所有分支 → 落
    /// `map_provider_error` 末尾兜底 → 502 且无 Retry-After → 客户端不退避、原样重发。
    ///
    /// 为什么"真耗尽"该给退避而不是当永久故障：它**会自愈**（全池自愈实测 41 分钟触发 36 次），
    /// 403 `TEMPORARILY_SUSPENDED` 本身也是限时态。
    /// ⭐ 上游 5xx / 传输层失败 → **503 + Retry-After**，不落未识别兜底的 502。
    ///
    /// 回退即 FAIL：删掉 `map_provider_error` 里那条 `is_upstream_transient_5xx` 分支 ——
    /// 这两类会逐条穿过所有已识别分支、落末尾兜底 → **502 且无 Retry-After** →
    /// 客户端（Claude Code）把 502 当服务端故障，退避逻辑压根不启动、原样重发。
    ///
    /// 实测量级（24h）：上游 `InternalServerException` 160 条 + 传输层失败 148 条，
    /// 其中 296 条 `retries=0` —— 因为 `compute_max_retries` 按池子大小算，
    /// 只剩 1 个可用号时算出的是 1（日志那句 `尝试 1/1`），所以上游一次 500
    /// **一次都没重试**就吐给客户端。网关侧重试预算要单独修（碰选号热路径），
    /// 但至少要让客户端知道该退避。
    #[test]
    fn upstream_5xx_and_transport_errors_map_to_503_with_retry_after() {
        for err_str in [
            // 线上原文（provider 格式化后的形态）
            r#"非流式 API 请求失败: 500 Internal Server Error {"__type":"com.amazon.aws.codewhisperer#InternalServerException","message":"Encountered an unexpected error when processing the request, please try again."}"#,
            r#"流式 API 请求失败: 502 Bad Gateway {"message":"upstream"}"#,
            r#"流式 API 请求失败: 503 Service Unavailable {"message":"x"}"#,
            r#"流式 API 请求失败: 504 Gateway Timeout {"message":"x"}"#,
            "error sending request for url (https://runtime.us-east-1.kiro.dev/generateAssistantResponse)",
        ] {
            let resp = map_provider_error(anyhow::Error::msg(err_str.to_string()));
            assert_eq!(
                resp.status(),
                StatusCode::SERVICE_UNAVAILABLE,
                "上游 5xx/传输层必须映射成 503（旧代码落兜底 502 无 Retry-After）: {err_str}"
            );
            assert!(
                resp.headers().get(header::RETRY_AFTER).is_some(),
                "必须带 Retry-After，否则客户端不退避: {err_str}"
            );
        }
    }

    /// ⭐ 顺序守卫：5xx 判据**绝不能**抢走已识别的分支，也不能误判 4xx。
    ///
    /// 回退即 FAIL：把 `is_upstream_transient_5xx` 那条 `if` 移到 `map_provider_error`
    /// 靠前的位置（例如 429/403/model-unsupported 之前）——那些本该拿 429/404 的错误
    /// 会被当成 5xx 返 503，客户端的退避语义整体错位。
    #[test]
    fn transient_5xx_branch_must_not_shadow_more_specific_ones() {
        // 429 仍必须是 429
        // 线上真实 429 原文（19855/19855 条都含小写 "Too many requests" 这句 message；
        // 判据故意不认 HTTP reason phrase 的大写 "Too Many Requests"，实测零漏判）。
        let r = map_provider_error(anyhow::Error::msg(
            r#"流式 API 请求失败: 429 Too Many Requests {"__type":"com.amazon.kiro.runtimeservice#ThrottlingException","message":"Too many requests, please wait before trying again.","reason":"USER_REQUEST_RATE_EXCEEDED"}"#.to_string(),
        ));
        assert_eq!(
            r.status(),
            StatusCode::TOO_MANY_REQUESTS,
            "429 不得被 5xx 分支抢走"
        );

        // 全池冷却（带 retry_after_secs=）仍必须是 429
        let r = map_provider_error(anyhow::Error::msg(
            "所有凭据均已禁用（0/2）retry_after_secs=10".to_string(),
        ));
        assert_eq!(r.status(), StatusCode::TOO_MANY_REQUESTS);

        // 模型永久不可用仍必须是 404 且无 Retry-After
        let r = map_provider_error(anyhow::Error::msg(
            "模型不被本号池支持 model_unsupported_by_pool=1".to_string(),
        ));
        assert_eq!(r.status(), StatusCode::NOT_FOUND);
        assert!(r.headers().get(header::RETRY_AFTER).is_none());

        // ⭐ 4xx 绝不能被误判成瞬态 5xx（判据只认确切的 5xx 字样，不裸匹配数字）
        for s in [
            r#"400 Bad Request {"requestId":"abc-500-def"}"#,
            r#"403 Forbidden {"message":"quota exceeded, 500 requests used"}"#,
        ] {
            assert!(
                !is_upstream_transient_5xx(s),
                "4xx 不得被当成可重试的 5xx（响应体里含 500 之类的数字很常见）: {s}"
            );
        }
    }

    /// ⭐ 524（Cloudflare 边缘网关超时）必须识别为瞬态 5xx：换号全灭后终态落
    /// 503 + Retry-After，而不是 502 兜底（客户端不退避、原样重发）。
    #[test]
    fn upstream_524_is_recognized_as_transient_5xx() {
        for err_str in [
            // Cloudflare 错误页形态：状态行只有裸 "524"，正文 HTML 含 "A timeout occurred"。
            r#"流式 API 请求失败: 524 <html><title>Error 524</title><body>A timeout occurred</body></html>"#,
            // 完整 reason phrase 形态（部分网关自定义 reason 为 "Gateway Timeout"）。
            r#"非流式 API 请求失败: 524 Gateway Timeout {"message":"x"}"#,
            // 仅连续 reason phrase 形态（无 `: 524` 状态行前缀）：必须命中第二判据。
            r#"上游返回 524 a timeout occurred"#,
        ] {
            assert!(
                is_upstream_transient_5xx(err_str),
                "524 必须识别为瞬态 5xx: {err_str}"
            );
            let resp = map_provider_error(anyhow::Error::msg(err_str.to_string()));
            assert_eq!(
                resp.status(),
                StatusCode::SERVICE_UNAVAILABLE,
                "524 终态必须映射成 503（旧代码落兜底 502 无 Retry-After）: {err_str}"
            );
            assert!(
                resp.headers().get(header::RETRY_AFTER).is_some(),
                "524 必须带 Retry-After，否则客户端不退避: {err_str}"
            );
        }
    }

    /// 普通 body 里含数字 "524"（如 token 数 / 请求计数）不得被误判成瞬态 5xx。
    #[test]
    fn plain_524_number_in_body_is_not_mistaken_for_5xx() {
        for s in [
            r#"400 Bad Request {"message":"context length 524 exceeds the limit"}"#,
            r#"403 Forbidden {"message":"quota exceeded, 524 requests used"}"#,
        ] {
            assert!(
                !is_upstream_transient_5xx(s),
                "含 524 数字的 4xx 不得被当成可重试的 5xx: {s}"
            );
        }
    }

    /// 🔴 对抗审查反例（MINOR-5，2026-08-15）：524 数字 + timeout/gateway 干扰词的
    /// 4xx body 不得被误判成瞬态 5xx —— 旧宽组合判据「contains(524) && (timeout ||
    /// gateway)」会命中下面两条（如 Cloudflare 限流页 "gateway abuse detected, 524
    /// requests blocked"），客户端对永久错误无限退避重试。
    #[test]
    fn gateway_interference_words_with_524_are_not_5xx() {
        for s in [
            r#"403 Forbidden {"message":"gateway abuse detected, 524 requests blocked"}"#,
            r#"400 Bad Request {"message":"524 tokens too long for this request, timeout after 30s"}"#,
        ] {
            assert!(
                !is_upstream_transient_5xx(s),
                "524 数字 + gateway/timeout 干扰词的 4xx 不得被当成可重试的 5xx: {s}"
            );
            let resp = map_provider_error(anyhow::Error::msg(s.to_string()));
            assert_ne!(
                resp.status(),
                StatusCode::SERVICE_UNAVAILABLE,
                "干扰词 4xx 不得落 503（Retry-After 诱导退避）: {s}"
            );
        }
    }

    /// 🔴 裸 524 状态行形态（MINOR-5 补判据，2026-08-15）：错误串含 `: 524`
    /// （provider 组装的「{api_type} API 请求失败: {status}」状态行）即命中，
    /// 不依赖正文里的 timeout/gateway 特征词 —— 换号全灭后终态仍落 503 + Retry-After。
    #[test]
    fn bare_524_status_line_is_recognized_as_transient_5xx() {
        for err_str in [
            "流式 API 请求失败: 524",
            r#"流式 API 请求失败: 524 <html><title>Error 524</title></html>"#,
            r#"非流式 API 请求失败: 524 Gateway Timeout {"message":"x"}"#,
        ] {
            assert!(
                is_upstream_transient_5xx(err_str),
                "裸 524 状态行必须识别为瞬态 5xx: {err_str}"
            );
            let resp = map_provider_error(anyhow::Error::msg(err_str.to_string()));
            assert_eq!(
                resp.status(),
                StatusCode::SERVICE_UNAVAILABLE,
                "524 状态行终态必须映射成 503: {err_str}"
            );
            assert!(
                resp.headers().get(header::RETRY_AFTER).is_some(),
                "524 状态行必须带 Retry-After: {err_str}"
            );
        }
    }

    #[test]
    fn test_pool_truly_exhausted_maps_to_429_with_retry_after_not_502() {
        // token_manager 的两个 bail 点实际组装的错误串原文。
        let err = anyhow::Error::msg("所有凭据均已禁用（0/0）retry_after_secs=10");
        let resp = map_provider_error(err);
        assert_eq!(
            resp.status(),
            StatusCode::TOO_MANY_REQUESTS,
            "号池真耗尽必须映射成可重试的 429（旧代码落兜底 → 502 且无 Retry-After → \
             客户端把它当服务故障、退避不启动、原样重发；实测 15 分钟内 2082 次）"
        );
        let hv = resp
            .headers()
            .get(header::RETRY_AFTER)
            .expect("号池耗尽必须带 Retry-After，否则客户端退避逻辑不启动");
        assert_eq!(hv.to_str().unwrap(), "10");
    }

    /// ⭐ 致命缺陷回归（去掉分类分支即 FAIL）：**403 账户级临时风控**曾落 502 无 Retry-After。
    ///
    /// 与「上游 429」「号池真耗尽」是同一类缺陷的第三个实例，也是**量最大**的一个：
    /// 线上近 2 小时 `auth_failed` 占 **22.3%**（1485/6662），全部是这一种，
    /// 且呈突发形态（13:50 一次 928 条、14:50 一次 516 条，中间为 0）= 风控窗口开合。
    ///
    /// 旧路径：该串不含任何已知关键词 → 逐条穿过所有分支 → 末尾兜底 502 无 Retry-After
    /// → 客户端把限时风控当服务端故障、不退避、原样重发 → 加深上游风控判定。
    #[test]
    fn test_upstream_temporarily_suspended_maps_to_429_with_retry_after() {
        // provider 实际组装的错误串原文（线上 traces.db 取出，账号 id 已改）。
        let raw = r#"流式 API 请求失败: 403 Forbidden {"__type":"com.amazon.aws.codewhisperer#AccessDeniedException","message":"Your User ID (450334904897) temporarily is suspended. We've locked your account as a security precaution. To restore access, please contact our support team to verify your identity: https://aws.amazon.com/contact-us/"}"#;
        let resp = map_provider_error(anyhow::Error::msg(raw));
        assert_eq!(
            resp.status(),
            StatusCode::TOO_MANY_REQUESTS,
            "403 临时风控必须映射成可重试的 429（旧代码落兜底 → 502 无 Retry-After → \
             客户端不退避、原样重发；实测占 22.3% 流量）"
        );
        let hv = resp
            .headers()
            .get(header::RETRY_AFTER)
            .expect("403 临时风控必须带 Retry-After，否则客户端退避逻辑不启动");
        assert_eq!(hv.to_str().unwrap(), "20");
    }

    /// 边界：判据必须**窄** —— 不带 `temporarily` 的 403 不得被吞成可重试。
    ///
    /// 若泛匹配 `AccessDeniedException` 或裸 403，账号**真被永久封禁**时也会返回
    /// 429 + Retry-After，客户端会对一个永远不会恢复的号无限退避重试，
    /// 同时把真实故障藏起来。与 `translate_quota_subscription` 刻意不吞配额类同理。
    #[test]
    fn test_permanent_access_denied_is_not_absorbed_as_retryable() {
        let raw = r#"流式 API 请求失败: 403 Forbidden {"__type":"com.amazon.aws.codewhisperer#AccessDeniedException","message":"Your account has been permanently disabled for violating the terms of service."}"#;
        let resp = map_provider_error(anyhow::Error::msg(raw));
        assert_ne!(
            resp.status(),
            StatusCode::TOO_MANY_REQUESTS,
            "永久封禁不得被判成可重试的 429（会让客户端对死号无限重试并掩盖真实故障）"
        );
        assert!(
            resp.headers().get(header::RETRY_AFTER).is_none(),
            "永久封禁不该带 Retry-After"
        );
    }

    /// ⭐ 致命缺陷回归（删掉分支即 FAIL）：**403 region 错配**曾落 502 兜底，实测 397 次。
    ///
    /// 旧路径：该串不带 `retry_after_secs=`、不含 `USER_REQUEST_RATE_EXCEEDED` /
    /// `Too many requests` / `temporarily is suspended`，也不含 `translate_quota_subscription`
    /// 认的首字母大写 `Invalid token`（上游写的是句末 `is invalid.`）→ 穿过所有分支 →
    /// 末尾兜底 **502 无 Retry-After** → 外挂 `kiro_shield.py`
    /// （`RETRYABLE={429,500,502,503,504}`）与客户端都按 5xx 盲退避重打，
    /// 而 `ksk_` token 按 region 授权、打错区恒 403，重打多少次都不会变。
    #[test]
    fn test_region_mismatch_403_maps_to_permission_error_not_502() {
        // provider 实际组装的错误串原文（`{api_type} API 请求失败: {status} {body}`）。
        for raw in [
            r#"流式 API 请求失败: 403 Forbidden {"__type":"com.amazon.aws.codewhisperer#AccessDeniedException","message":"The bearer token included in the request is invalid."}"#,
            r#"非流式 API 请求失败（所有凭据已用尽）: 403 Forbidden {"__type":"com.amazon.kiro.runtimeservice#AccessDeniedException","message":"The bearer token included in the request is invalid."}"#,
        ] {
            let resp = map_provider_error(anyhow::Error::msg(raw.to_string()));
            assert_eq!(
                resp.status(),
                StatusCode::FORBIDDEN,
                "region 错配型 403 必须映射成 403 permission_error（旧代码落兜底 502 → \
                 外挂按 5xx 盲退避重打一个永远不会变的授权错误）: {raw}"
            );
            assert!(
                resp.headers().get(header::RETRY_AFTER).is_none(),
                "region 错配不该带 Retry-After —— 给了就等于宣称「等一会儿会好」: {raw}"
            );
        }
    }

    /// 边界：判据必须**窄** —— 永久封禁串不得命中 region 错配分支。
    ///
    /// 若为了接住那 397 次而泛匹配 `AccessDeniedException` 或裸 403，账号真被永久封禁时
    /// 会被告知「改 region」，给出完全错误的排障动作，同时与
    /// `is_upstream_temporarily_suspended` 的窄判据互相拆台。
    #[test]
    fn test_region_mismatch_judgement_is_narrow() {
        // ① 永久封禁：同为 403 + AccessDeniedException，但不含 bearer-invalid 那句。
        let banned = r#"流式 API 请求失败: 403 Forbidden {"__type":"com.amazon.aws.codewhisperer#AccessDeniedException","message":"Your account has been permanently disabled for violating the terms of service."}"#;
        assert!(
            !is_upstream_region_mismatch_403(banned),
            "永久封禁不得被判成 region 错配（否则排障动作完全错，且掩盖真实故障）"
        );
        // ② 临时风控：同为 403 + AccessDeniedException，也不含那句。
        let suspended = r#"流式 API 请求失败: 403 Forbidden {"__type":"com.amazon.aws.codewhisperer#AccessDeniedException","message":"Your User ID (450334904897) temporarily is suspended."}"#;
        assert!(
            !is_upstream_region_mismatch_403(suspended),
            "临时风控不得被 region 分支抢走（它必须拿 429 + Retry-After）"
        );
        // ③ 401：token 本身死了 ≠ region 错了。处置是刷新/换号，不是改 region。
        //    与 `region_probe.rs::classify_probe_result` 的「401 排在 403 之前」同源。
        let dead_token = r#"流式 API 请求失败: 401 Unauthorized {"message":"The bearer token included in the request is invalid.","requestId":"403-ish-id"}"#;
        assert!(
            !is_upstream_region_mismatch_403(dead_token),
            "401 必须让路：token 死了要刷新/换号，不是改 region"
        );
        // ④ 裸 403 无任何 message：不得命中（判据要求那句确切文案）。
        assert!(!is_upstream_region_mismatch_403("403 Forbidden"));
    }

    /// provider 组装 bearer-invalid 型 403 时的**真实**错误串。
    ///
    /// 形状逐字取自 `provider.rs`：`"{api_type} API 请求失败: {status} {body}"`
    /// （`api_type` = `流式` / `非流式`，`status` 是 `StatusCode` 的 Display ⇒ `403 Forbidden`）。
    const REAL_BEARER_INVALID_403: &str = r#"流式 API 请求失败: 403 Forbidden {"__type":"com.amazon.aws.codewhisperer#AccessDeniedException","message":"The bearer token included in the request is invalid."}"#;

    /// ⭐ 顺序守卫：region 错配分支**绝不能**抢走 429 / 全池冷却 / 临时风控 / 模型不支持。
    ///
    /// 回退即 FAIL 的形态是「把 `is_upstream_region_mismatch_403` 那条 `if` 上移到
    /// `is_upstream_rate_limited` / `is_upstream_temporarily_suspended` 之前」——
    /// 那些本该拿 429 + Retry-After 的错误会变成不带退避的 403，客户端退避逻辑整体失效。
    ///
    /// 断言的是**分支顺序**而非分支内容：每个用例都先钉死「region 判据确实命中它」，
    /// 再断言 `map_provider_error` 仍返回那条更优先的分支的结果。少了前半句，
    /// 测试就退化成「本来也不会命中」的纸面断言。
    ///
    /// # 夹具的诚实说明（上一版是自己编的串，2026-08-06 重写）
    ///
    /// 每个用例的**两个半段各自都是真串**，逐字取自生产链路的 `format!`：
    /// - 上游响应型：`provider.rs` 的 `"{api_type} API 请求失败: {status} {body}"`；
    /// - 号池 bail 型：`token_manager.rs` 的 `"所有凭据均在冷却（{}/{}）retry_after_secs={}"`
    ///   与 `"模型 {:?} 不被本号池支持（{}/{} …）model_unsupported_by_pool=1"`。
    ///
    /// 但**拼接本身是测试构造的**，真实链路不会产出这种双命中串：`last_error` 每次只装
    /// 一个错误（号池 bail 与上游 body 是两条互斥来源，中间没有"；最后错误:"这种拼接）。
    /// 上一版夹具凭空造了那个拼接词和 `"detail"` 字段，这里改掉 —— 编的串与真串差一个
    /// 字段就可能让判据形同虚设，那时守卫看着绿实则没在守。
    ///
    /// 保留这条守卫的理由：承重点是**分支顺序**，而顺序在「某天上游 body 里同时提两件事」
    /// 或「某天有人把两个错误拼起来」时才暴露。用真半段拼出来的串是能触到该顺序的最小输入，
    /// 也是唯一能触到的 —— 所以拼接是刻意的，并在此写明它是构造的而非采集的。
    #[test]
    fn region_mismatch_branch_must_not_shadow_rate_limit_or_suspended() {
        // 与 `test_pool_cooling_retry_after_still_takes_precedence` 同款：
        // 错误消息表是进程级 ArcSwap，并行测试可能把 `rate_limited_pool` 配成 503。
        let _em_guard = ERROR_MESSAGES_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        set_error_messages(ErrorMessagesTable::new());

        // 先钉死：单独的真 bearer-invalid 串确实走 region 分支（403）。
        // 否则下面每条"不得被抢走"都可能只是因为 region 分支本来就不参与竞争。
        assert_eq!(
            map_provider_error(anyhow::Error::msg(REAL_BEARER_INVALID_403)).status(),
            StatusCode::FORBIDDEN,
            "前提：真 bearer-invalid 串单独出现时确实落 region 分支，下面的竞争才成立"
        );

        // ① 上游 429 真串（`token_manager` 之外，provider 把上游 body 原样带出）
        //    + bearer-invalid 真串：必须仍是 429 + Retry-After。
        let real_429 = r#"流式 API 请求失败: 429 Too Many Requests {"message":"Too many requests, please wait before trying again.","reason":"USER_REQUEST_RATE_EXCEEDED"}"#;
        let both_429 = format!("{real_429} / {REAL_BEARER_INVALID_403}");
        assert!(
            is_upstream_region_mismatch_403(&both_429),
            "前提：region 判据确实命中该串（否则下面的顺序断言是空的）"
        );
        let r = map_provider_error(anyhow::Error::msg(both_429));
        assert_eq!(
            r.status(),
            StatusCode::TOO_MANY_REQUESTS,
            "429 不得被 region 分支抢走：限流是可重试态，必须拿 429 + Retry-After"
        );
        assert!(r.headers().get(header::RETRY_AFTER).is_some());

        // ② 全池冷却真 bail 串（`token_manager.rs` 的 `所有凭据均在冷却（{}/{}）
        //    retry_after_secs={}`，全角括号、无任何后缀）+ bearer-invalid 真串：
        //    必须仍是 429 且用号池算出的精确秒数。
        let real_cooldown = "所有凭据均在冷却（0/3）retry_after_secs=14";
        let both_cooldown = format!("{real_cooldown} / {REAL_BEARER_INVALID_403}");
        assert!(
            is_upstream_region_mismatch_403(&both_cooldown),
            "前提：region 判据确实命中该串"
        );
        let r = map_provider_error(anyhow::Error::msg(both_cooldown));
        assert_eq!(
            r.status(),
            StatusCode::TOO_MANY_REQUESTS,
            "全池冷却不得被 region 分支抢走"
        );
        assert_eq!(
            r.headers()
                .get(header::RETRY_AFTER)
                .unwrap()
                .to_str()
                .unwrap(),
            "14",
            "全池冷却的精确 retry_after 不该被 region 分支吃掉"
        );

        // ③ 临时风控真串（线上原文，`temporarily is suspended` + `security precaution`）
        //    + bearer-invalid 真串：必须仍是 429 + Retry-After: 20。
        let real_suspended = r#"非流式 API 请求失败: 403 Forbidden {"__type":"com.amazon.aws.codewhisperer#AccessDeniedException","message":"Your User ID (186648603162) temporarily is suspended. We've locked your account as a security precaution. To restore access, please contact our support team to verify your identity: https://aws.amazon.com/contact-us/"}"#;
        let both_suspended = format!("{real_suspended} / {REAL_BEARER_INVALID_403}");
        assert!(
            is_upstream_region_mismatch_403(&both_suspended),
            "前提：region 判据确实命中该串"
        );
        let r = map_provider_error(anyhow::Error::msg(both_suspended));
        assert_eq!(
            r.status(),
            StatusCode::TOO_MANY_REQUESTS,
            "临时风控不得被 region 分支抢走：它自称 temporarily，是可恢复限时态"
        );
        assert_eq!(
            r.headers()
                .get(header::RETRY_AFTER)
                .unwrap()
                .to_str()
                .unwrap(),
            "20"
        );

        // ④ 模型永久不可用真 bail 串（`token_manager.rs` 那条，`{:?}` 让模型名带引号）
        //    + bearer-invalid 真串：必须仍是 404，不得被 region 分支抢走。
        let real_model = r#"模型 "claude-opus-5" 不被本号池支持（2/2 个号均因订阅档位或成本白名单不含该模型而被过滤，非号池耗尽，重试无效）model_unsupported_by_pool=1"#;
        let both_model = format!("{real_model} / {REAL_BEARER_INVALID_403}");
        assert!(
            is_upstream_region_mismatch_403(&both_model),
            "前提：region 判据确实命中该串"
        );
        let r = map_provider_error(anyhow::Error::msg(both_model));
        assert_eq!(
            r.status(),
            StatusCode::NOT_FOUND,
            "model_unsupported 不得被 region 分支抢走"
        );
    }

    /// ⭐ 收窄回归（删掉那条排除即 FAIL）：provider 已判为**瞬态抖动**的 bearer-invalid
    /// 不得被判成 region 错配。
    ///
    /// 依据（`provider.rs` 的 `bearer_invalid_but_proven`，判据 `has_ever_succeeded`）：
    /// - 从未成功过的号 → 真 region 错配（实测 3 个号共吃 17 次）；
    /// - 已成功过的号 → 抖动（实测 4 个号累计 3393 次成功、共吃 42 次）。
    /// 即这个串的**多数出现不是 region 错配**，而两者的上游文案逐字节相同 ——
    /// 只有 provider 分得出来，所以它把结论写成机器可读标记带出来。
    ///
    /// 收窄前的两个后果：
    /// ① 排障文案让管理员去查 region，而那个号的 region 是对的；
    /// ② 状态码 502 → 403。502 在外挂 `kiro_shield.py` 的
    ///    `RETRYABLE={429,500,502,503,504}` 内会被重试，403 是 4xx 不重试；
    ///    而这一类下一次重试大概率落到别的号上成功（实测 #481 成功率 93.9%）⇒
    ///    收窄等于把本该有的重试机会还回去。
    ///
    /// 夹具是 `provider.rs:1661` 的**真串**（`{api_type} API 请求失败（token 瞬态失效，
    /// 已冷却换号）bearer_invalid_transient=1: {status} {body}`），不是编的。
    #[test]
    fn provider_marked_transient_bearer_invalid_is_not_region_mismatch() {
        // provider 真串：流式与非流式两种 api_type 都要覆盖（只有前缀不同）。
        for raw in [
            r#"流式 API 请求失败（token 瞬态失效，已冷却换号）bearer_invalid_transient=1: 403 Forbidden {"__type":"com.amazon.aws.codewhisperer#AccessDeniedException","message":"The bearer token included in the request is invalid."}"#,
            r#"非流式 API 请求失败（token 瞬态失效，已冷却换号）bearer_invalid_transient=1: 403 Forbidden {"__type":"com.amazon.kiro.runtimeservice#AccessDeniedException","message":"The bearer token included in the request is invalid."}"#,
        ] {
            assert!(
                !is_upstream_region_mismatch_403(raw),
                "provider 已判瞬态，不得再判成 region 错配（排障方向会错，且 403 让外挂不再重试）: {raw}"
            );
            let resp = map_provider_error(anyhow::Error::msg(raw.to_string()));
            assert_ne!(
                resp.status(),
                StatusCode::FORBIDDEN,
                "瞬态抖动不得拿 403 —— 4xx 不在外挂 RETRYABLE 集内，一次抖动会固化成硬失败: {raw}"
            );
            // 落回兜底 502：它在 `RETRYABLE={429,500,502,503,504}` 内 ⇒ 会被重试，
            // 而下一跳大概率是另一个号 ⇒ 成功。这正是收窄要恢复的行为。
            assert_eq!(
                resp.status(),
                StatusCode::BAD_GATEWAY,
                "瞬态抖动应退回可重试路径（502 在外挂 RETRYABLE 集内）: {raw}"
            );
        }

        // 对照组（承重）：**不带**标记的同款上游文案仍必须判 region 错配 →
        // 证明收窄只切掉了带标记的那一类，没有把整条修复关掉。
        assert!(
            is_upstream_region_mismatch_403(REAL_BEARER_INVALID_403),
            "不带标记的 bearer-invalid（从未成功过的号）仍须判 region 错配"
        );
        assert_eq!(
            map_provider_error(anyhow::Error::msg(REAL_BEARER_INVALID_403)).status(),
            StatusCode::FORBIDDEN
        );
    }

    /// ⭐ 源码级守卫：`bearer_invalid_transient=1` 这个字面量必须在 provider 侧真的存在。
    ///
    /// 上面那条测试只证明「handlers 侧看见标记会排除」。若 provider 改名/改大小写/加空格，
    /// 排除会**静默失效**（回到误判）且编译不报错 —— 因为两侧靠字符串约定，没有类型联系。
    /// 本仓已因「判据在一层、承重点在另一层」踩过多次（见 endpoint 侧那条状态门守卫）。
    ///
    /// 锚点切掉注释行：本仓踩过五次「needle 命中注释里的散文」——
    /// provider 那处的注释里就写了这个字面量。
    #[test]
    fn provider_must_still_emit_the_transient_marker() {
        let src = include_str!("../kiro/provider.rs");
        let prod: String = src
            .split("#[cfg(test)]")
            .next()
            .expect("生产段应存在")
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            prod.contains(BEARER_INVALID_TRANSIENT_MARKER),
            "provider 必须仍在瞬态 bail 串里带 `{BEARER_INVALID_TRANSIENT_MARKER}` —— \
             改掉它 handlers 侧的排除会静默失效，回到「把健康号判成 region 错配」"
        );
        // 且必须与 `has_ever_succeeded` 那个二分在同一处：标记若被挪到别的分支，
        // 语义就从「已证明有效的号」变成别的东西，而排除逻辑不会察觉。
        let mi = prod
            .find(BEARER_INVALID_TRANSIENT_MARKER)
            .expect("上一条断言已保证存在");
        let window = &prod[mi.saturating_sub(1200)..mi];
        assert!(
            window.contains(&["has_ever_", "succeeded(ctx.id)"].concat()),
            "标记必须仍打在 `has_ever_succeeded` 那个二分的分支里 —— \
             否则它标的不再是「已证明有效的号」，而 handlers 侧照旧排除"
        );
    }

    /// 边界：region 错配**不可吸收**。
    ///
    /// 吸收层的对象是「等一会儿真的会好」的态。region 错配在单请求的 45s 预算内
    /// 等多久都不会变（要改配置或等探测器重选），吸收它只是占着客户端连接空转满预算。
    /// 这条同时防止将来有人顺手把它加进 `absorb_class_of`。
    #[test]
    fn region_mismatch_403_is_never_absorbable() {
        let raw = r#"流式 API 请求失败: 403 Forbidden {"__type":"com.amazon.aws.codewhisperer#AccessDeniedException","message":"The bearer token included in the request is invalid."}"#;
        assert!(
            is_upstream_region_mismatch_403(raw),
            "前提：region 判据确实命中该串"
        );
        assert!(
            absorb_class_of(raw).is_none(),
            "region 错配不可吸收：45s 预算内等多久都不会变（要改 region 或等探测重选）"
        );
    }

    /// 边界：坐实上面那条测的是**标记**而非中文文案 —— 不带标记的同款文案仍落 502 兜底。
    ///
    /// 这条的作用是防止将来有人"顺手"改成按 `所有凭据均已禁用` 文案匹配：那正是本类缺陷
    /// 反复出现的成因（文案一改分类就失效）。它同时证明修复的承重点在 token_manager
    /// 那两个 bail 串上，而不在本函数里。
    #[test]
    fn test_pool_exhausted_without_marker_still_falls_through_to_502() {
        let err = anyhow::Error::msg("所有凭据均已禁用（0/0）");
        let resp = map_provider_error(err);
        assert_eq!(
            resp.status(),
            StatusCode::BAD_GATEWAY,
            "不带 retry_after_secs 标记时应仍落兜底 —— 说明分类判据是标记而非中文文案"
        );
    }

    /// 用户线上实测原文（逐字，未改一个字符）：图片声明 `image/png` 而字节是 jpeg。
    const REAL_IMAGE_MIME_MISMATCH: &str = r#"非流式 API 请求失败: 400 Bad Request {"__type":"com.amazon.aws.codewhisperer#ValidationException","message":"messages.2.content.1.image.source.base64: The image was specified using the image/png media type, but the image appears to be a image/jpeg image","reason":"IMAGE_MIME_MISMATCH"}"#;

    /// ⭐ 新增判据回归（删掉 `translate_context_input` 里那条即 FAIL）：
    /// `IMAGE_MIME_MISMATCH` 必须映射成 400 `invalid_request_error` 且带图片专属排障文案。
    ///
    /// 这个 reason 码此前**全仓零判据**，落通用兜底 → 客户端拿 502 `api_error`
    /// 「上游 API 调用失败（未识别错误）」：既说错了性质（这是客户端请求构造问题，
    /// 不是上游故障），又让它进了外挂 `kiro_shield.py` 的 `RETRYABLE` 集
    /// （`{429,500,502,503,504}`）⇒ 一个**重试永远不会变**的请求被重打到预算耗尽。
    ///
    /// 而它的主要价值是**度量**：`converter.rs` 的 `resolve_image_format` 已按 magic bytes
    /// 校正声明的 media_type，但若仍有边缘情况漏掉（magic 认不出而回退声明值等），
    /// 那些 400 会混进通用 `bad_request` 桶 ⇒ 无法回答「那条修干净了没有」。
    /// 这条判据是那条修复唯一的效果度量。
    #[test]
    fn image_mime_mismatch_maps_to_400_invalid_request_not_502() {
        // 判据本身（收口在 endpoint 侧，与 `default_is_*` 系列同处）。
        assert!(
            crate::kiro::endpoint::default_is_image_mime_mismatch(REAL_IMAGE_MIME_MISMATCH),
            "用户线上原文必须命中判据"
        );
        let resp = map_provider_error(anyhow::Error::msg(REAL_IMAGE_MIME_MISMATCH));
        assert_eq!(
            resp.status(),
            StatusCode::BAD_REQUEST,
            "IMAGE_MIME_MISMATCH 是请求构造问题，必须 400（旧路径落兜底 502 → \
             性质说错，且进外挂 RETRYABLE 集被反复重打）"
        );
        assert!(
            resp.headers().get(header::RETRY_AFTER).is_none(),
            "重试无效的错误绝不带 Retry-After"
        );
    }

    /// ⭐ 顺序守卫（承重）：`IMAGE_MIME_MISMATCH` **不得**抢走同为 400 的
    /// `INSUFFICIENT_MODEL_CAPACITY`。
    ///
    /// 两者都是 HTTP 400，但处置相反：容量不足必须拿 **503 `overloaded_error`**
    /// （可退避重试，实测 24h 272 次），图片格式错必须拿 **400**（重试无效）。
    /// 若把图片判据放到 `translate_quota_subscription` **之前**（或放宽成认
    /// `ValidationException` / 认 message 里的 `media type` 散文），容量不足会被说成
    /// 「你的图片格式错」：既误导用户，又让客户端不再退避 —— 而那正是本仓
    /// `INSUFFICIENT_MODEL_CAPACITY` 那批修复要解决的问题，等于把它退回去。
    ///
    /// 回退即 FAIL 的形态：把 `translate_upstream_error` 的 `.or_else` 链改成
    /// `translate_context_input(...).or_else(|| translate_quota_subscription(...))`。
    /// 断言的是**分支顺序**，不是判据内部形状 —— 后者对顺序缺陷完全不可见
    /// （本仓已因此让一条"三处都改对、四测全绿"的修复无效上线过）。
    #[test]
    fn image_mime_mismatch_must_not_shadow_capacity_400() {
        // 真容量串（线上原文）单独出现：必须仍是 503。
        let real_capacity = r#"流式 API 请求失败: 400 Bad Request {"__type":"com.amazon.aws.codewhisperer#ThrottlingException","message":"I am experiencing high traffic, please try again shortly.","reason":"INSUFFICIENT_MODEL_CAPACITY"}"#;
        assert_eq!(
            map_provider_error(anyhow::Error::msg(real_capacity)).status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "前提：容量 400 单独出现时确实拿 503，下面的竞争才成立"
        );

        // 双命中串（两个半段各自都是真串，拼接是测试构造的 —— 真实链路一次只带一个错误，
        // 但顺序缺陷只有这种输入触得到）：必须仍按容量处置返 503。
        let both = format!("{real_capacity} / {REAL_IMAGE_MIME_MISMATCH}");
        assert!(
            crate::kiro::endpoint::default_is_image_mime_mismatch(&both),
            "前提：图片判据确实命中该串（否则顺序断言是空的）"
        );
        assert_eq!(
            map_provider_error(anyhow::Error::msg(both)).status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "容量 400 不得被图片判据抢走：它必须拿 503 才会被客户端退避重试"
        );

        // 反向边界：图片判据只认 reason 字面量，不得因 `ValidationException` 泛匹配而
        // 吞掉别的校验错误（`TOOL_USE_RESULT_MISMATCH` 也是这个 `__type`）。
        let tool_mismatch = r#"非流式 API 请求失败: 400 Bad Request {"__type":"com.amazon.aws.codewhisperer#ValidationException","message":"...","reason":"TOOL_USE_RESULT_MISMATCH"}"#;
        assert!(
            !crate::kiro::endpoint::default_is_image_mime_mismatch(tool_mismatch),
            "同 __type 的其它校验错误不得被图片判据吞掉（处置不同，混判会给错排障方向）"
        );
    }

    /// 线上实测原文（passthrough.rs 注释里的样本，逐字）：同一 body 里 message 与
    /// reason 两个信号并存 —— 判据必须同时认，认其一漏其二。
    const REAL_REQUEST_BODY_INVALID: &str = r#"非流式 API 请求失败: 400 Bad Request {"__type":"com.amazon.aws.codewhisperer#ValidationException","message":"Invalid tool use format.","reason":"REQUEST_BODY_INVALID"}"#;

    #[test]
    fn request_body_invalid_maps_to_400_invalid_request_not_502() {
        use axum::body::to_bytes;
        // 判据本身（收口在 endpoint 侧，与 `default_is_*` 系列同处）。
        assert!(
            crate::kiro::endpoint::default_is_request_body_invalid(REAL_REQUEST_BODY_INVALID),
            "线上原文必须命中判据"
        );
        let resp = map_provider_error(anyhow::Error::msg(REAL_REQUEST_BODY_INVALID));
        assert_eq!(
            resp.status(),
            StatusCode::BAD_REQUEST,
            "请求体校验失败是请求构造问题，必须 400（旧路径落兜底 502 → 性质说错，\
             且 502 在外挂 RETRYABLE 集里会被反复重打同一个必败的请求）"
        );
        assert!(
            resp.headers().get(header::RETRY_AFTER).is_none(),
            "重试无效的错误绝不带 Retry-After"
        );
        let body = futures::executor::block_on(to_bytes(resp.into_body(), usize::MAX)).unwrap();
        let text = String::from_utf8_lossy(&body);
        assert!(
            text.contains("invalid_request_error"),
            "错误类型必须是 invalid_request_error（客户端按类型决定是否重试）。实际: {text}"
        );
    }

    /// 判据边界：`ValidationException` 是上游多种校验共用（TOOL_USE_RESULT_MISMATCH 等），
    /// 泛匹配会把处置不同的错误混成一类 —— 判据只认两个专用信号，不得泛认。
    #[test]
    fn request_body_invalid_predicate_never_matches_bare_validation_exception() {
        let bare = r#"非流式 API 请求失败: 400 Bad Request {"__type":"com.amazon.aws.codewhisperer#ValidationException","message":"anything","reason":"SOME_UNMAPPED_REASON"}"#;
        assert!(
            !crate::kiro::endpoint::default_is_request_body_invalid(bare),
            "不带两个专用信号的 body 不得命中（否则把处置不同的校验错误混成一类）"
        );
        assert_eq!(
            map_provider_error(anyhow::Error::msg(bare)).status(),
            StatusCode::BAD_GATEWAY,
            "未映射错误仍落 502 兜底 —— 证明本判据没把别的 400 抢走"
        );
    }

    /// M1 形态（对抗审查）：`Improperly formed request` + `reason=REQUEST_BODY_INVALID`
    /// 是**用户请求体**格式校验失败的常见形态（converter.rs/websearch.rs 实测：工具 schema
    /// 属性、工具名超限、web_search 直发）。改前它被凭据分类分支（`Improperly formed` 子串）
    /// 截胡成 502「上游拒绝凭据」——排障方向全错。必须 400 invalid_request_error。
    #[test]
    fn improperly_formed_body_invalid_maps_to_400_not_502_credential_rejection() {
        let raw = r#"流式 API 请求失败: 400 Bad Request {"__type":"com.amazon.aws.codewhisperer#ValidationException","message":"Improperly formed request.","reason":"REQUEST_BODY_INVALID"}"#;
        assert!(
            crate::kiro::endpoint::default_is_request_body_invalid(raw),
            "判据必须命中该形态"
        );
        let resp = map_provider_error(anyhow::Error::msg(raw));
        assert_eq!(
            resp.status(),
            StatusCode::BAD_REQUEST,
            "用户请求体的格式校验失败必须 400，不得被说成凭据/订阅问题（502）"
        );
    }

    /// 顺序守卫（承重，m2）：请求体校验分支**不得**抢走 `INSUFFICIENT_MODEL_CAPACITY`
    /// 的 503。与 `image_mime_mismatch_must_not_shadow_capacity_400` 同款——容量 400
    /// 必须拿 503 `overloaded_error` 客户端才会退避重试；顺序靠 `.or_else` 链保证
    /// （quota 链先执行），回退即 FAIL。
    #[test]
    fn request_body_invalid_must_not_steal_capacity_400_503() {
        let capacity = r#"400 Bad Request {"__type":"com.amazon.aws.codewhisperer#ThrottlingException","message":"I am experiencing high traffic, please try again shortly.","reason":"INSUFFICIENT_MODEL_CAPACITY"}"#;
        assert_eq!(
            map_provider_error(anyhow::Error::msg(capacity)).status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "前提：容量 400 单独出现时确实拿 503"
        );
        let both = format!("{capacity} / {REAL_REQUEST_BODY_INVALID}");
        assert!(
            crate::kiro::endpoint::default_is_request_body_invalid(&both),
            "前提：请求体判据确实命中该串（否则顺序断言是空的）"
        );
        assert_eq!(
            map_provider_error(anyhow::Error::msg(both)).status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "容量 400 不得被请求体校验分支抢走：它必须拿 503 才会被客户端退避重试"
        );
    }

    #[test]
    fn test_insufficient_throughput_also_maps_to_429() {
        // 另一种上游限流文案（实测 8 条）：high traffic / INSUFFICIENT_THROUGHPUT。
        let raw = r#"流式 API 请求失败: 429 Too Many Requests {"message":"I am experiencing high traffic, please try again shortly.","reason":"INSUFFICIENT_THROUGHPUT"}"#;
        let err = anyhow::Error::msg(raw);
        let resp = map_provider_error(err);
        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
        assert!(resp.headers().get(header::RETRY_AFTER).is_some());
    }

    #[test]
    fn test_quota_exhausted_stays_429_without_retry_after() {
        // 边界：配额耗尽同为 429 但**不可重试**（要等下个计费周期）→ 绝不能带 Retry-After，
        // 否则客户端会做无意义的 8s 退避后反复砸一个本月已无额度的号。
        // 同时验证限流判据没有误吞它（is_upstream_rate_limited 不匹配 MONTHLY_REQUEST_COUNT）。
        let err = anyhow::Error::msg("upstream: MONTHLY_REQUEST_COUNT limit reached");
        let resp = map_provider_error(err);
        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
        assert!(
            resp.headers().get(header::RETRY_AFTER).is_none(),
            "配额耗尽不该带 Retry-After（不可重试）"
        );
    }

    #[test]
    fn test_rate_limit_judgement_does_not_swallow_quota() {
        // 判据单测：速率类命中、配额类不命中。防止后续有人放宽判据把配额也吞进来。
        assert!(is_upstream_rate_limited(
            r#"{"reason":"USER_REQUEST_RATE_EXCEEDED"}"#
        ));
        assert!(is_upstream_rate_limited(
            r#"{"reason":"INSUFFICIENT_THROUGHPUT"}"#
        ));
        assert!(is_upstream_rate_limited("429 Too many requests"));
        assert!(!is_upstream_rate_limited(
            "upstream: MONTHLY_REQUEST_COUNT limit reached"
        ));
        assert!(!is_upstream_rate_limited("403 FEATURE_NOT_SUPPORTED"));
        assert!(!is_upstream_rate_limited(
            "CONTENT_LENGTH_EXCEEDS_THRESHOLD"
        ));
    }

    #[test]
    fn test_pool_cooling_retry_after_still_takes_precedence() {
        // 错误消息表是进程级 ArcSwap：`configured_status_and_type_override_render`
        // 会把 `rate_limited_pool` 配成 503。本夹具不持锁就会在全量并行里读到那张表，
        // 冷却分支仍命中但 status 变成 503（T5 left 503 / right 429），与分支顺序无关。
        let _guard = ERROR_MESSAGES_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        set_error_messages(ErrorMessagesTable::new());

        // 零回归：全池冷却分支（带 retry_after_secs=N 标记）在限流判据 / 通用 503 之前，
        // 其上游给定的精确秒数不该被固定 8s 或 5xx 503 覆盖。
        let err = anyhow::Error::msg("所有凭据均在冷却（1/5）retry_after_secs=14");
        let resp = map_provider_error(err);
        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(
            resp.headers()
                .get(header::RETRY_AFTER)
                .unwrap()
                .to_str()
                .unwrap(),
            "14",
            "全池冷却的精确 retry_after 不该被固定 8s 覆盖"
        );

        // 源码顺序：冷却 if-let 必须在通用 5xx 503 之前（针拼接，测试段不写完整生产字面量）。
        let full = include_str!("handlers.rs");
        let start = full
            .find("fn map_provider_error(err: Error) -> Response {")
            .expect("函数签名必须存在");
        let body = &full[start..];
        let cooling = ["if let Some(secs) = ", "parse_retry_after_secs(&err_str)"].concat();
        let five_xx = ["is_upstream_transient_5xx", "(&err_str)"].concat();
        let cooling_at = body
            .find(&cooling)
            .expect("全池冷却 if-let 分支必须存在");
        let five_xx_at = body.find(&five_xx).expect("通用 5xx 分支必须存在");
        assert!(
            cooling_at < five_xx_at,
            "全池冷却必须排在通用 5xx 503 之前，否则本夹具会被抢走返 503"
        );
    }

    /// ⭐ BLOCKER 2 守卫：入站准入超时**绝不可吸收**。
    ///
    /// 回退即 FAIL：删掉 `absorb_class_of` 里第一条 `inbound_admission_timeout=1 → None`，
    /// 该串会落到下面的 `retry_after_secs=` 分支被判成 `PoolCooldown` → 吸收层去重试
    /// **网关自己的背压信号**：把同一个请求塞回同一个已经满的桶，队列更长、客户端等更久，
    /// 且拿不到任何额外成功概率（实测 2 轮 × 30s = 客户端等 60s 才拿到 429，正确是 <2s）。
    #[test]
    fn admission_timeout_is_never_absorbable() {
        // provider.rs:820 那条 bail 的原文形态（同时带两个标记）。
        let s = "入站限速排队超时(网关目标 300 RPM 保护上游)inbound_admission_timeout=1 retry_after_secs=3";
        assert!(
            absorb_class_of(s).is_none(),
            "准入超时必须不可吸收；它与全池冷却共用 retry_after_secs= 标记，\
             靠 inbound_admission_timeout=1 这道显式判据区分"
        );
        // 对照组：同样带 retry_after_secs= 但**不带**准入标记的全池冷却，必须可吸收。
        assert_eq!(
            absorb_class_of("所有凭据均在冷却（0/1）retry_after_secs=3"),
            Some(AbsorbClass::PoolCooldown(3)),
            "全池冷却是「上游稍后真的会好」，必须可吸收（否则吸收层没有任何作用对象）"
        );
    }

    /// ⭐ 顺序守卫：`model_unsupported_by_pool=1` 永久不可吸收，且判据必须排在
    /// `retry_after_secs=` **之前**。
    ///
    /// 回退即 FAIL：删掉那条 `None`，或把它移到 `retry_after_secs=` 之后 —— 后者更隐蔽：
    /// 「模型级过滤但可恢复」那条 bail **带** `retry_after_secs=`，顺序反了就会把
    /// **永久**不可用当成可恢复态反复吸收，等于把 404 死循环搬进网关。
    #[test]
    fn model_unsupported_by_pool_is_never_absorbable() {
        // token_manager 那条 bail 的原文形态（不带 retry_after_secs）。
        let permanent = "模型 \"claude-opus-5\" 不被本号池支持（0/1 个号均因订阅档位或成本白名单不含该模型而被过滤，非号池耗尽，重试无效）model_unsupported_by_pool=1";
        assert!(
            absorb_class_of(permanent).is_none(),
            "模型对号池永久不可用时重试无效，必须不可吸收"
        );
        // ⭐ 承重：两个标记同时出现时，永久态必须赢（顺序守卫）。
        let both = "模型不被本号池支持 model_unsupported_by_pool=1 retry_after_secs=30";
        assert!(
            absorb_class_of(both).is_none(),
            "同时带 model_unsupported_by_pool=1 与 retry_after_secs= 时必须判不可吸收 —— \
             说明永久态判据排在 retry_after_secs 之前"
        );
    }

    /// 🔴 **跨系统契约守卫**：shield（kiro_shield.py，仓外）的 `COOLING_MARKERS` 只有
    /// **3 个英文判据串**（2026-08-15 线上实读核对，`ssh skiapi 'grep -A12
    /// COOLING_MARKERS /opt/skiapi/services/kiro_shield.py'`）：
    ///   · `temporarily cooling down`
    ///   · `All credentials are temporarily`
    ///   · `inbound rate shaping`
    /// 它们与我们的响应文案是硬契约 —— shield 的 `classify()` **按 body 文案分类
    /// 而非状态码**，且只有 `verdict ∈ {cool,auth}` 才读我们的 `Retry-After`：
    /// ```text
    /// if verdict in ("cool","auth"): delay = cool_delay(attempt, Retry-After)   # 听真值
    /// else:                          delay = swap_delay(attempt)                # 本地阶梯
    /// ```
    /// 判据词被替换 ⇒ 落 `retry` 兜底 ⇒ **精心算出的 Retry-After 被整个丢弃**，
    /// 改走 20→60s 阶梯 ⇒ 等真实恢复时间的 2~6 倍（CLAUDE.md 记录：当晚 1753 次失败）。
    ///
    /// ⚠️ **2026-08-15 认知纠错**：本守卫此前钉的是「等容量」——它只出现在 shield 的
    /// **注释**（337 行）里，不是 COOLING_MARKERS 判据；A1/A2 的 503 文案因此
    /// **不承载**任何判据词，可自由改。旧守卫放行真判据词被替换、却拦住无害的
    /// 「等容量」，方向反了。真实承载点只有三处生产文案：
    ///   · A5（全池冷却 429）= `All credentials are temporarily cooling down.`（命中前两个 marker）；
    ///   · A3（入站准入超时 429）+ 入站闸门 = `Gateway inbound rate shaping is at capacity...`（命中第三个 marker）。
    ///
    /// 回退即 FAIL：替换上述任一判据子串 → 本测试红。
    /// ⚠️ 若 shield 的 `COOLING_MARKERS` 变更，需同步本测试。
    #[test]
    fn shield_cooling_markers_stay_in_production_text() {
        let src = include_str!("handlers.rs");
        let prod = src.split("\n#[cfg(test)]").next().unwrap_or(src);

        // 判据串运行时拼接，避免本测试段自身的字面量让 `find` 命中测试代码而非生产代码
        // —— 本仓已发生过两次这类「守卫静默变绿」事故。
        let a5_head: String = ["All credentials are temporarily"].concat();
        let cooling: String = ["temporarily cooling", " down"].concat();
        let shaping: String = ["inbound rate", " shaping"].concat();

        // A5 全池冷却文案必须恰好 1 处（map_provider_error 的 rate_limited_pool 分支），
        // 且同一文案内两个 COOLING_MARKERS 齐备。
        let n_a5 = prod.matches(a5_head.as_str()).count();
        assert_eq!(
            n_a5, 1,
            "A5 全池冷却文案必须恰好 1 处（map_provider_error rate_limited_pool 分支），实际 {n_a5} 处。\n\
             若你新增/删除了全池冷却文案，请同步本守卫。"
        );
        let pos = prod
            .find(a5_head.as_str())
            .expect("count==1 已保证存在（unwrap 仅用于取偏移）");
        // ⚠️ 用字符窗口而非字节窗口：源码含大量中文，字节偏移 + 固定长度会切进
        // UTF-8 多字节字符内部（本仓同类守卫初版就是这么挂的）。`chars().take(N)`
        // 按字符取，安全且与直觉一致。
        let window: String = prod[pos..].chars().take(120).collect();
        assert!(
            window.contains(cooling.as_str()),
            "A5 文案必须携带 COOLING_MARKERS 判据词。\n\
             后果：shield 的 classify() 判它为 `retry` 而非 `cool` ⇒ 丢弃我们的 Retry-After，\
             改走 20→60s 本地阶梯 ⇒ 1753 次失败事故形态。"
        );

        // `inbound rate shaping` 的承载点：A3 分支（map_provider_error）+ 入站闸门
        // （try_inbound_admission_gate），恰好 2 处生产文案。
        let n_shaping = prod.matches(shaping.as_str()).count();
        assert_eq!(
            n_shaping, 2,
            "「inbound rate shaping」必须恰好 2 处生产文案（A3 分支 + 入站闸门），实际 {n_shaping} 处。\n\
             若你新增/删除了网关背压文案，请同步本守卫。"
        );
    }

    /// ⭐ 池**永久**耗尽不可吸收，且判据必须排在 `retry_after_secs=` **之前**。
    ///
    /// 回退即 FAIL：删掉 `absorb_class_of` 里 `pool_permanently_exhausted=1 → None`
    /// （或把它移到 `retry_after_secs=` 之后），该串会被判成 `PoolCooldown(10)` →
    /// 吸收层对一个**一个可自愈的号都没有**的池（全 QuotaExhausted /
    /// RefreshTokenInvalid / AccountSuspended）拿满 45s 预算空转，客户端从 <2s
    /// 拿到 429 变成 45s 才拿到，且这 45s 内它一直占着连接。
    #[test]
    fn permanently_exhausted_pool_is_never_absorbable() {
        // token_manager 两处 bail 的原文形态（**带** retry_after_secs=，因为对客户端
        // 而言 429 + Retry-After 仍是对的：人工补号后确实会好）。
        let dead = "所有凭据均已禁用（0/2）pool_permanently_exhausted=1 retry_after_secs=10";
        assert!(
            absorb_class_of(dead).is_none(),
            "池里没有任何可自愈的号时，单请求预算内等多久都不会变，必须不可吸收"
        );
        // 对照组：同样是「全禁用」，但有可自愈的号 ⇒ 不带该标记 ⇒ 必须可吸收。
        // 这是 pool-empty 类占 24h 流量 16.5% 的那一大类，吸收层的主要作用对象。
        assert_eq!(
            absorb_class_of("所有凭据均已禁用（0/2）retry_after_secs=10"),
            Some(AbsorbClass::PoolCooldown(10)),
            "可自愈的全禁用必须可吸收，否则吸收层对最大的一类失败没有作用"
        );
    }

    /// 永久耗尽对**客户端**仍必须是 429 + Retry-After —— 只对「单请求内重试」说不。
    ///
    /// 回退即 FAIL：若有人把该标记也加进 `map_provider_error` 的早退分支、
    /// 或把它渲染成 404/502，这条断言会失败。人工补号后池子确实会恢复，
    /// 所以客户端该退避重试；不可吸收针对的只是**同一条请求内**的重试。
    #[test]
    fn permanently_exhausted_pool_still_renders_429_to_client() {
        let resp = map_provider_error(anyhow::anyhow!(
            "所有凭据均已禁用（0/2）pool_permanently_exhausted=1 retry_after_secs=10"
        ));
        assert_eq!(
            resp.status(),
            StatusCode::TOO_MANY_REQUESTS,
            "对客户端必须仍是 429（人工补号后会恢复，客户端该退避）"
        );
        assert_eq!(
            resp.headers()
                .get(header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok()),
            Some("10"),
            "必须带 Retry-After，否则客户端把它当服务端故障、退避逻辑不启动"
        );
    }

    /// 不可重试类一律不吸收：月度配额耗尽 / 传输层故障 / 普通 4xx / 未知。
    ///
    /// 回退即 FAIL：把 `is_upstream_temporarily_suspended` 放宽成裸 403 或
    /// `AccessDeniedException`，配额串与永久封禁串会被判成可吸收 → 对一个永远不会恢复的号
    /// 反复重试，同时把真实故障藏起来。
    #[test]
    fn non_retryable_errors_are_not_absorbable() {
        for s in [
            "流式 API 请求失败: 429 Too Many Requests {\"reason\":\"MONTHLY_REQUEST_COUNT\"}",
            "error sending request for url (https://runtime.eu-central-1.kiro.dev)",
            "流式 API 请求失败: 400 Bad Request {\"message\":\"Improperly formed request\"}",
            "某个谁也没见过的错误",
        ] {
            assert!(
                absorb_class_of(s).is_none(),
                "不可重试类必须不吸收，但 {s:?} 被判成了 {:?}",
                absorb_class_of(s)
            );
        }
    }

    /// 可吸收的两类正例 + 403 临时风控被单独归类（是否真吸收由配置决定，不在分类器里判）。
    #[test]
    fn retryable_upstream_errors_are_classified() {
        assert_eq!(
            absorb_class_of("流式 API 请求失败: 429 {\"reason\":\"USER_REQUEST_RATE_EXCEEDED\"}"),
            Some(AbsorbClass::UpstreamRateLimit)
        );
        assert_eq!(
            absorb_class_of(
                "403 Forbidden {\"message\":\"Your User ID (450334904897) temporarily is suspended.\"}"
            ),
            Some(AbsorbClass::SwapWindow),
            "403 临时风控（换号空窗）要能被识别出来（默认不吸收，但必须可分类，否则配置开了也没用）"
        );
    }

    /// ⭐ 合并外挂缺口 1：**上游 5xx 可分类**（此前对所有 5xx 返 None ⇒ 吸收层完全不覆盖）。
    ///
    /// 依据：外挂 `RETRYABLE={429,500,502,503,504}`，注释原文「500/502/503/504 = 网关/上游抖动，
    /// 也含『凭据全禁用』这类换号空窗」，且线上 shield 日志实见
    /// `502 -> wait 1.0s, attempt 1/60`。
    ///
    /// 回退即 FAIL：删掉 `absorb_class_of` 里那条 `is_upstream_transient_5xx` 分支。
    ///
    /// 夹具是 provider 真实组装的串：`"{api_type} API 请求失败: {status} {body}"`
    /// （provider.rs 通用 5xx 分支，`api_type` = 流式/非流式，`status` 是 `StatusCode` 的 Display）。
    #[test]
    fn transient_5xx_is_absorbable_but_transport_failure_is_not() {
        for raw in [
            "流式 API 请求失败: 502 Bad Gateway <html>502 Bad Gateway</html>",
            r#"非流式 API 请求失败: 500 Internal Server Error {"__type":"com.amazon.aws.codewhisperer#InternalServerException","message":"Internal server error"}"#,
            "流式 API 请求失败: 504 Gateway Timeout upstream timed out",
        ] {
            assert_eq!(
                absorb_class_of(raw),
                Some(AbsorbClass::TransientServerError),
                "上游 5xx 必须可分类（外挂把它们放进 RETRYABLE 且线上实见 502）: {raw}"
            );
        }
        // ⭐ 边界（承重）：**传输层**失败仍必须不可吸收。provider 内部换号已把每个号各试过
        // 一遍，吸收层再套一层只是把同一个网络故障重打 N 遍。
        // 回退即 FAIL：把分类器里那条 `&& !is_transport_error(...)` 删掉。
        for raw in [
            "error sending request for url (https://runtime.eu-central-1.kiro.dev)",
            "error trying to connect: dns error: failed to lookup address information",
        ] {
            assert!(
                absorb_class_of(raw).is_none(),
                "传输层故障不可吸收（provider 内部换号已覆盖）: {raw}"
            );
        }
    }

    /// ⭐ 合并外挂缺口 2：**带瞬态标记的 400 可分类**，其余 400 一律不吸收。
    ///
    /// 依据（外挂注释原文，带实测）：「Kiro 会把一部分瞬态故障塞进 400，跟『请求写错了』同一个
    /// 状态码。实测 6 小时样本里 400 共 165 次，其中容量类 101 次、格式错 80 次。只认这些明确的
    /// 瞬态标记，其余 400 一律透传，避免把真正的格式错误重试 60 次。」
    ///
    /// 判据**复用既有谓词** `endpoint::default_is_model_temporarily_unavailable`
    /// （认 `MODEL_TEMPORARILY_UNAVAILABLE` / `INSUFFICIENT_MODEL_CAPACITY`），不新写匹配。
    ///
    /// 回退即 FAIL：删掉分类器里那条容量分支 → 400 容量类落 None、503 容量类被 5xx 抢走。
    #[test]
    fn transient_capacity_400_is_absorbable_but_real_bad_request_is_not() {
        // provider.rs 容量分支的真实串（上游原文逐字，见 endpoint/mod.rs 该谓词处的实测记录）。
        let capacity_400 = r#"流式 API 请求失败（模型暂时不可用，建议稍后重试）: 400 Bad Request {"__type":"com.amazon.aws.codewhisperer#ThrottlingException","message":"I am experiencing high traffic, please try again shortly.","reason":"INSUFFICIENT_MODEL_CAPACITY"}"#;
        assert_eq!(
            absorb_class_of(capacity_400),
            Some(AbsorbClass::TransientCapacity400),
            "400 + INSUFFICIENT_MODEL_CAPACITY 是**瞬态**容量问题，必须可分类"
        );
        // ⭐ 顺序守卫：容量类的另一种上游形态是 **503**，必须仍判容量类而**不是** 5xx。
        // 回退即 FAIL：把容量分支移到 5xx 分支之后 —— 那条 5xx 判据认
        // `503 service unavailable` 字样，会把这一串抢走并套上 1s 起的短曲线。
        let capacity_503 = r#"非流式 API 请求失败（模型暂时不可用，建议稍后重试）: 503 Service Unavailable {"reason":"MODEL_TEMPORARILY_UNAVAILABLE"}"#;
        assert_eq!(
            absorb_class_of(capacity_503),
            Some(AbsorbClass::TransientCapacity400),
            "503 形态的容量类必须仍归容量（它与 400 形态处置相同）——说明容量判据排在 5xx 之前"
        );
        // 真格式错的 400（实测 6h 内 80 次）必须仍不可吸收：重试 60 次也永远不会成功。
        for raw in [
            r#"流式 API 请求失败: 400 Bad Request {"__type":"com.amazon.aws.codewhisperer#ValidationException","message":"Improperly formed request"}"#,
            r#"非流式 API 请求失败: 400 Bad Request {"reason":"IMAGE_MIME_MISMATCH"}"#,
        ] {
            assert!(
                absorb_class_of(raw).is_none(),
                "真格式错的 400 必须不可吸收（重试永远不会成功）: {raw}"
            );
        }
        // 裸 `ThrottlingException`（无 reason）**刻意不认**：那个 __type 被真限流共用。
        assert!(
            absorb_class_of(
                r#"流式 API 请求失败: 400 Bad Request {"__type":"ThrottlingException"}"#
            )
            .is_none(),
            "裸 ThrottlingException 不得被判成容量类（外挂白名单认它，本仓刻意不认：\
             `USER_REQUEST_RATE_EXCEEDED` 真限流共用同一个 __type）"
        );
    }

    /// ⭐ 顺序守卫（承重，外挂 2026-08-04 实测踩过的坑）：
    /// **`PoolCooldown` 判据必须排在 `SwapWindow` 之前**。
    ///
    /// 外挂原文：「全池不可用时返回 429 + `Retry-After: 10`，body 是
    /// "All credentials are temporarily cooling down..."，而 `"All credentials"` 原先挂在
    /// SWAP_WINDOW_MARKERS 里 → 判 swap → 套了长阶梯 → 本该等 10 秒的等了几十秒。」
    ///
    /// 即：号池冷却必须**听网关算出的真值**，换号空窗才用 20~60s 长阶梯。两者混判的代价是
    /// 客户端白等几十秒。
    ///
    /// 本测试有两道断言，第二道是源码级顺序守卫 —— 因为第一道只能证明「当前判据不冲突」，
    /// 证明不了「顺序对」（这正是本仓第 8 种纸面测试形态：测了分支内部，没测分支顺序）。
    #[test]
    fn pool_cooldown_wins_over_swap_window_ordering() {
        // 夹具：同时带 `retry_after_secs=`（号池真值）与 suspend 字样的串。
        // 真实来源：吸收轮之间 `last_error` 刻意不重置，某一轮拿到 403 风控、下一轮拿到全池
        // 冷却 bail 时，两种特征会先后出现在同一条请求的错误链上；而 KiroStudio 作为上游被
        // 串联时（custom_api 代挂），它自己渲染的 429 body 就带 "temporarily cooling down"。
        let both = "所有凭据均在冷却（0/4）retry_after_secs=10 \
                    上游原文: 403 Forbidden {\"message\":\"Your User ID temporarily is suspended.\"}";
        assert_eq!(
            absorb_class_of(both),
            Some(AbsorbClass::PoolCooldown(10)),
            "同时带号池真值与 suspend 字样时，**必须**判 PoolCooldown 并用真值 10s —— \
             判成 SwapWindow 会套 20~60s 长阶梯，本该等 10 秒的等几十秒（外挂实测踩过）"
        );

        // ⭐ 源码级顺序守卫：`parse_retry_after_secs`（PoolCooldown）必须出现在
        // `is_upstream_temporarily_suspended`（SwapWindow）之前。
        // 回退即 FAIL：把 SwapWindow 那条分支上移到 `retry_after_secs=` 之前。
        let body = absorb_class_of_source();
        let pool_at = body
            .find("parse_retry_after_secs")
            .expect("分类器必须仍用 parse_retry_after_secs 判 PoolCooldown");
        let swap_at = body
            .find("is_upstream_temporarily_suspended")
            .expect("分类器必须仍用既有谓词判 SwapWindow（不新写字符串匹配）");
        assert!(
            pool_at < swap_at,
            "PoolCooldown 判据必须排在 SwapWindow 之前（听网关真值 vs 套长阶梯，混判即白等）"
        );
    }

    /// ⭐ 顺序守卫（承重）：新增的三条判据必须全部排在**三条 `None`** 之后。
    ///
    /// 那三条是「网关自己的背压」（`inbound_admission_timeout=1`）与两种「永久态」
    /// （`model_unsupported_by_pool=1` / `pool_permanently_exhausted=1`）。任何通用判据排到
    /// 它们前面，都会把不该重试的东西吸收掉。
    ///
    /// 回退即 FAIL：把任一条新判据上移到那三条 `None` 之前。
    #[test]
    fn new_absorb_predicates_come_after_the_three_none_gates() {
        let body = absorb_class_of_source();
        // 三道 None 各自的位置（按机器可读标记定位，不依赖注释文案）。
        let gates = [
            "inbound_admission_timeout=1",
            "model_unsupported_by_pool=1",
            "pool_permanently_exhausted=1",
        ]
        .map(|m| {
            body.find(m)
                .unwrap_or_else(|| panic!("分类器必须仍有 {m} 这道 None 闸门"))
        });
        let last_gate = *gates.iter().max().expect("三道闸门非空");
        for needle in [
            "default_is_model_temporarily_unavailable",
            "is_upstream_transient_5xx",
            "is_upstream_region_mismatch_403",
        ] {
            let at = body
                .find(needle)
                .unwrap_or_else(|| panic!("分类器必须仍调 {needle}（复用既有谓词）"));
            assert!(
                at > last_gate,
                "{needle} 必须排在三条 None 闸门之后 —— 排前面会把网关背压/永久态当可吸收"
            );
        }

        // ⭐ 容量类必须排在 5xx 之前（容量类的一种形态是 503，会被 5xx 判据抢走）。
        assert!(
            body.find("default_is_model_temporarily_unavailable")
                .unwrap()
                < body.find("is_upstream_transient_5xx").unwrap(),
            "容量判据必须排在 5xx 之前，否则 503 形态的容量类被 5xx 抢走、套错退避曲线"
        );
    }

    /// 取 `absorb_class_of` 的**函数体**源码（供顺序守卫用）。
    ///
    /// 为什么要切片而不是直接用整个文件：`include_str!` 会把本测试模块自己的字面量也读进来，
    /// 按全文件找位置会命中测试里的字符串（本仓 `absorb_stop_reasons_are_distinguishable_in_logs`
    /// 就吃过这个坑：短名在注释里也出现，实测 left=3 right=2）。
    fn absorb_class_of_source() -> &'static str {
        let full = include_str!("handlers.rs");
        let start = full
            .find("pub(crate) fn absorb_class_of")
            .expect("函数签名必须存在");
        let rest = &full[start..];
        // 函数体结束于下一个顶层 `\n}\n`（本函数内所有 `}` 都有缩进）。
        let end = rest.find("\n}\n").expect("函数必须有结尾");
        &rest[..end]
    }

    /// ⭐ 合并外挂缺口 4：预算耗尽后按配置回 **503**（而非透传 429）。
    ///
    /// 依据（外挂注释原文）：「有总时间预算，超预算返回 **503（不是 429）** —— Cursor 对 503
    /// 不会像对 429 那样立刻停止会话。」这是产品级行为差异：同一个「网关已尽力但没成」的事实，
    /// 用 429 表达会让 Cursor 直接掐会话，用 503 表达会让它自己再退避重试。
    ///
    /// 回退即 FAIL：删掉 `map_provider_error` 的第一条分支（标记分支），该串会落到下面的
    /// 全池冷却分支返回 429 —— 即这个开关静默失效。
    #[test]
    fn absorb_exhausted_marker_renders_503_with_retry_after() {
        // provider 在 `absorb_gave_up_after_rounds && exhausted_as_503` 时组装的串形态：
        // 原错误 + 空格 + 标记。
        let raw = format!(
            "所有凭据均在冷却（0/4）retry_after_secs=10 {}",
            ABSORB_BUDGET_EXHAUSTED_MARKER
        );
        let resp = map_provider_error(anyhow::Error::msg(raw));
        assert_eq!(
            resp.status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "带耗尽标记时必须回 503（Cursor 见 429 会掐会话，见 503 会自行退避）"
        );
        assert_eq!(
            resp.headers()
                .get(header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok()),
            Some("10"),
            "必须优先用号池真值 10s 而不是常数兜底 —— 真值比任何常数都准"
        );

        // ⭐ 边界（承重）：**不带**标记的同一条串必须仍是 429。
        // 这坐实了状态码只对「吸收层真的跑过并放弃」的请求变化，没进过吸收层的 429 照旧。
        let untouched = map_provider_error(anyhow::Error::msg(
            "所有凭据均在冷却（0/4）retry_after_secs=10",
        ));
        assert_eq!(
            untouched.status(),
            StatusCode::TOO_MANY_REQUESTS,
            "没有标记就必须仍是 429（默认配置下 provider 不打标记 ⇒ 渲染路径逐字节不变）"
        );

        // 无号池真值时按类别兜底：403 风控用 20s（与 cooldown.rs 的 SuspiciousActivity 同源）。
        let swap = map_provider_error(anyhow::Error::msg(format!(
            "403 Forbidden {{\"message\":\"Your User ID temporarily is suspended.\"}} {}",
            ABSORB_BUDGET_EXHAUSTED_MARKER
        )));
        assert_eq!(swap.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            swap.headers()
                .get(header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok()),
            Some("20")
        );
    }

    /// ⭐ 承重：标记分支必须是 `map_provider_error` 的**第一条**。
    ///
    /// 这个标记只可能打在**已被判为可吸收**的错误串上，而那些串必然还带着各自的原始特征
    /// （`retry_after_secs=` / `USER_REQUEST_RATE_EXCEEDED` / `temporarily is suspended` /
    /// 5xx 字样）—— 后面任何一条分支都会先把它们接走并返回 429。排在后面等于开关静默失效。
    ///
    /// 回退即 FAIL：把标记分支下移到准入超时分支之后 —— 上面那条 `retry_after_secs=10`
    /// 的夹具会被全池冷却分支抢走返 429。这里再加一道源码级顺序断言，
    /// 因为运行时断言只能证明「当前夹具通过」，证明不了顺序本身。
    #[test]
    fn absorb_exhausted_branch_is_first_in_map_provider_error() {
        let full = include_str!("handlers.rs");
        let start = full
            .find("fn map_provider_error(err: Error) -> Response {")
            .expect("函数签名必须存在");
        let body = &full[start..];
        let marker_at = body
            .find("ABSORB_BUDGET_EXHAUSTED_MARKER")
            .expect("必须有耗尽标记分支");
        for later in [
            "inbound_admission_timeout=1",
            "parse_retry_after_secs(&err_str)",
            "is_upstream_rate_limited(&err_str)",
            "is_upstream_temporarily_suspended(&err_str)",
        ] {
            let at = body
                .find(later)
                .unwrap_or_else(|| panic!("{later} 分支必须仍存在"));
            assert!(
                marker_at < at,
                "耗尽标记分支必须排在 {later} 之前，否则那条分支会先把串接走返 429（开关静默失效）"
            );
        }
    }

    /// 全池配额 402 必须排在 `parse_retry_after_secs` / 速率 429 / 临时风控之前
    /// （形态 b 串同时带 retry_after_secs，顺序反了会被 429 抢走）。
    #[test]
    fn quota_exhausted_all_branch_precedes_retry_after_and_rate_limit() {
        let full = include_str!("handlers.rs");
        let start = full
            .find("fn map_provider_error(err: Error) -> Response {")
            .expect("函数签名必须存在");
        let body = &full[start..];
        let quota_at = body
            .find(&format!("{}{}", "quota_exhausted_all", "=1"))
            .expect("map_provider_error 必须有 quota_exhausted_all 分支");
        for later in [
            "inbound_admission_timeout=1",
            "if let Some(secs) = parse_retry_after_secs(&err_str)",
            "if is_upstream_rate_limited(&err_str)",
            "if is_upstream_temporarily_suspended(&err_str)",
        ] {
            let at = body
                .find(later)
                .unwrap_or_else(|| panic!("{later} 分支必须仍存在"));
            assert!(
                quota_at < at,
                "quota_exhausted_all 402 必须排在 {later} 之前"
            );
        }
    }

    #[test]
    fn map_provider_error_quota_marker_returns_402_without_retry_after() {
        let err = anyhow::Error::msg(
            "流式 API 请求失败（所有凭据已用尽）quota_exhausted_all=1: 400 MONTHLY_REQUEST_COUNT",
        );
        let resp = map_provider_error(err);
        assert_eq!(resp.status(), StatusCode::PAYMENT_REQUIRED);
        assert!(
            resp.headers().get(header::RETRY_AFTER).is_none(),
            "402 不得带 Retry-After"
        );
    }

    #[test]
    fn map_provider_error_mixed_429_and_quota_marker_prefers_402() {
        let err = anyhow::Error::msg(
            "429 Too Many Requests quota_exhausted_all=1 pool_permanently_exhausted=1 retry_after_secs=10",
        );
        let resp = map_provider_error(err);
        assert_eq!(resp.status(), StatusCode::PAYMENT_REQUIRED);
        assert!(resp.headers().get(header::RETRY_AFTER).is_none());
    }

    #[test]
    fn map_provider_error_bare_monthly_request_count_stays_429() {
        let err = anyhow::Error::msg("upstream: MONTHLY_REQUEST_COUNT limit reached");
        let resp = map_provider_error(err);
        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
    }

    #[test]
    fn dispatch_websearch_paths_skips_mcp_without_kiro_pool() {
        let src = include_str!("handlers.rs");
        let start = src
            .find("async fn dispatch_websearch_paths")
            .expect("dispatch_websearch_paths 必须存在");
        let body = src[start..].split("async fn dispatch_kiro_attempt").next().unwrap();
        let compact: String = body.chars().filter(|c| !c.is_whitespace()).collect();
        let needle = format!("{}{}", "has_enabled_kiro", "_credential");
        assert!(
            compact.contains(&needle),
            "纯透传池必须用 has_enabled_kiro_credential 跳过 MCP 快路径/回灌"
        );
    }

    /// 抽出的 `parse_retry_after_secs` 与它替换掉的两份内联拷贝行为一致。
    ///
    /// 回退即 FAIL：若有人把解析逻辑改回各写一份并写歪一处，这里的边界断言会失败。
    #[test]
    fn parse_retry_after_secs_handles_boundaries() {
        assert_eq!(parse_retry_after_secs("x retry_after_secs=14"), Some(14));
        assert_eq!(parse_retry_after_secs("x retry_after_secs=7 y"), Some(7));
        assert_eq!(parse_retry_after_secs("x retry_after_secs=0"), Some(0));
        assert_eq!(parse_retry_after_secs("没有这个标记"), None);
        assert_eq!(parse_retry_after_secs("retry_after_secs=abc"), None);
    }

    #[test]
    fn parse_upstream_retry_after_handles_boundaries() {
        assert_eq!(
            parse_upstream_retry_after(&format!("x {}14", UPSTREAM_RETRY_AFTER_MARKER_PREFIX)),
            Some(14)
        );
        assert_eq!(
            parse_upstream_retry_after(&format!("x {}7 y", UPSTREAM_RETRY_AFTER_MARKER_PREFIX)),
            Some(7)
        );
        assert_eq!(
            parse_upstream_retry_after(&format!("x {}0", UPSTREAM_RETRY_AFTER_MARKER_PREFIX)),
            Some(0)
        );
        assert_eq!(parse_upstream_retry_after("没有这个标记"), None);
        assert_eq!(
            parse_upstream_retry_after(&format!("x {}abc", UPSTREAM_RETRY_AFTER_MARKER_PREFIX)),
            None
        );
        // 网关恒把 marker 追加在错误串**末尾**（body 噪声只可能在前）→ 取最后一次出现。
        assert_eq!(
            parse_upstream_retry_after(&format!(
                "body 里意外出现 {}999 的噪声 {}30",
                UPSTREAM_RETRY_AFTER_MARKER_PREFIX, UPSTREAM_RETRY_AFTER_MARKER_PREFIX
            )),
            Some(30),
            "必须取最后一次出现（网关追加的才是真值，body 噪声在前）"
        );
    }

    /// Review3 m1：A7 判据必须锚定串尾（trim 后），裸 contains 会把 body 噪声
    /// 误判成网关真值。锚定判据与 `parse_upstream_retry_after` 的解析天然一致
    /// （marker 之后直到串尾全是数字才算）。
    #[test]
    fn upstream_retry_after_anchored_requires_marker_at_tail() {
        let marker = UPSTREAM_RETRY_AFTER_MARKER_PREFIX;
        assert!(
            upstream_retry_after_anchored(&format!("x {}30", marker)),
            "网关追加在末尾（后跟数字）必须锚定"
        );
        assert!(
            upstream_retry_after_anchored(&format!("x {}30  ", marker)),
            "尾随空白可容忍（trim_end 后锚定）"
        );
        assert!(
            !upstream_retry_after_anchored(&format!("x {}7 y", marker)),
            "marker 后还有字符 = body 噪声，不得锚定"
        );
        assert!(
            !upstream_retry_after_anchored(&format!("body 噪声 {}999 后还有字", marker)),
            "marker 不在串尾的噪声不得锚定"
        );
        assert!(!upstream_retry_after_anchored("没有这个标记"));
        assert!(
            !upstream_retry_after_anchored(&format!("x {}abc", marker)),
            "marker 后非数字不得锚定"
        );
    }

    /// Review3 m1 行为级：marker 不在串尾的噪声串绝不落 A7（5xx 终态不被错误
    /// 映射回 429+RA——否则客户端拿上游真值做短退避白打）。
    #[test]
    fn upstream_retry_after_noise_not_at_tail_never_maps_to_429() {
        let marker = UPSTREAM_RETRY_AFTER_MARKER_PREFIX;
        // 网关真实形态是追加在末尾；这里 marker 在串中间（body 噪声形态）。
        let noise = format!(
            "流式 API 请求失败: 502 Bad Gateway {{\"message\":\"upstream\"}} {}30 然后结束",
            marker
        );
        let resp = map_provider_error(anyhow::Error::msg(noise));
        assert_ne!(
            resp.status(),
            StatusCode::TOO_MANY_REQUESTS,
            "marker 不在串尾的噪声不得触发 A7（裸 contains 会误判）"
        );
    }

    /// ⭐ S2：上游显式 Retry-After 透传 —— 上游真值 > 配置 > 默认 8s。
    ///
    /// provider 在 429 分支把上游 RA 打进错误串（`upstream_retry_after=N`），
    /// A7 分支必须优先读它：上游说 30s，客户端就该等 30s，而不是 8s 就重打。
    #[test]
    fn upstream_retry_after_value_wins_over_config_and_default() {
        let _guard = ERROR_MESSAGES_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let real_429 = r#"流式 API 请求失败: 429 Too Many Requests {"message":"Too many requests, please wait before trying again.","reason":"USER_REQUEST_RATE_EXCEEDED"}"#;

        // ① 无 marker（上游没给 RA）→ 默认 8s。
        let resp = map_provider_error(anyhow::Error::msg(real_429.to_string()));
        assert_eq!(
            resp.headers().get(header::RETRY_AFTER).and_then(|v| v.to_str().ok()),
            Some("8"),
            "无上游 RA、无配置时维持默认 8s（既有行为逐字不变）"
        );

        // ② 上游 RA=30（S2 目标场景）→ 必须 30。
        let with_marker = format!("{} {}30", real_429, UPSTREAM_RETRY_AFTER_MARKER_PREFIX);
        let resp = map_provider_error(anyhow::Error::msg(with_marker));
        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(
            resp.headers().get(header::RETRY_AFTER).and_then(|v| v.to_str().ok()),
            Some("30"),
            "上游显式 RA=30 必须透传给客户端（此前 A7 恒 8s，客户端 8s 就重打白打一轮）"
        );

        // ③ 配置 rate_limited_credential.retryAfterSecs=99 → 上游真值仍优先（99 被覆盖）。
        let mut table = test_table();
        table.insert(
            "rate_limited_credential".to_string(),
            crate::model::config::ErrorMessageOverride {
                status: None,
                r#type: None,
                message: None,
                retry_after_secs: Some(99),
            },
        );
        set_error_messages(table);
        let resp = map_provider_error(anyhow::Error::msg(format!(
            "{} {}30",
            real_429, UPSTREAM_RETRY_AFTER_MARKER_PREFIX
        )));
        assert_eq!(
            resp.headers().get(header::RETRY_AFTER).and_then(|v| v.to_str().ok()),
            Some("30"),
            "上游真值必须优先于配置（上游真值 > 配置 > 默认）"
        );

        // ④ 无 marker + 配置 99 → 配置生效。
        let resp = map_provider_error(anyhow::Error::msg(real_429.to_string()));
        assert_eq!(
            resp.headers().get(header::RETRY_AFTER).and_then(|v| v.to_str().ok()),
            Some("99"),
            "无上游真值时配置必须生效（上游真值 > 配置 > 默认）"
        );
        // 复位（防污染其它测试的全局镜像）。
        set_error_messages(test_table());
    }

    /// ⭐ S3 端到端：最早 429 的 RA 被并入 generic 5xx 终态后，A7 的 marker 判据
    /// 必须把它映射回 429 + 上游 RA（而不是 503+3s 或 502）。
    #[test]
    fn upstream_retry_after_marker_maps_5xx_final_to_429() {
        // provider 的 assemble_final_error 并入 marker 后的 5xx 终态形态。
        let preserved = format!(
            "流式 API 请求失败: 502 Bad Gateway {{\"message\":\"upstream\"}} {}30",
            UPSTREAM_RETRY_AFTER_MARKER_PREFIX
        );
        let resp = map_provider_error(anyhow::Error::msg(preserved));
        assert_eq!(
            resp.status(),
            StatusCode::TOO_MANY_REQUESTS,
            "最早类型化 429 的 RA 必须把 generic 5xx 终态映射回 429（客户端按上游节奏退避）"
        );
        assert_eq!(
            resp.headers().get(header::RETRY_AFTER).and_then(|v| v.to_str().ok()),
            Some("30")
        );
    }

    /// ⭐ H2 契约：配额字样的串即使带 marker 也绝不能落 A7 带 RA（月度配额要等下个
    /// 计费周期，给秒数会诱导客户端做无意义的短退避反复砸死号）。
    #[test]
    fn upstream_retry_after_marker_never_breaks_quota_semantics() {
        let quota_with_marker = format!(
            "流式 API 请求失败: 429 Too Many Requests {{\"reason\":\"MONTHLY_REQUEST_COUNT\"}} {}30",
            UPSTREAM_RETRY_AFTER_MARKER_PREFIX
        );
        let resp = map_provider_error(anyhow::Error::msg(quota_with_marker));
        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
        assert!(
            resp.headers().get(header::RETRY_AFTER).is_none(),
            "配额 429 必须维持「无 Retry-After」（H2 契约）：marker 判据不得吞配额类"
        );
    }

    #[test]
    fn test_inbound_admission_timeout_is_distinguishable_from_pool_cooling() {
        // ⭐ 回归（把 `inbound_admission_timeout=1` 那条分支删掉即必失败）：
        // 准入超时（网关自己的背压）与全池冷却（上游没准备好）**语义相反**，
        // 但两者都带 `retry_after_secs=`。若响应体上不可区分，任何按 body 判定的
        // 重试层（内置吸收层 / 外挂 kiro_shield）都会去重试网关自己的背压信号
        // —— 实测形态：2 轮 × 30s = 客户端等 60s 才拿到 429，而正确是 <2s。
        let admission = map_provider_error(anyhow::Error::msg(
            "入站限速排队超时(网关目标 300 RPM 保护上游)inbound_admission_timeout=1 retry_after_secs=3",
        ));
        let cooling = map_provider_error(anyhow::Error::msg(
            "所有凭据均在冷却（0/1）retry_after_secs=3",
        ));

        // 对客户端而言两者都该是 429 + Retry-After（它就该退避）。
        assert_eq!(admission.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(cooling.status(), StatusCode::TOO_MANY_REQUESTS);
        for (name, resp) in [("准入超时", &admission), ("全池冷却", &cooling)] {
            assert_eq!(
                resp.headers()
                    .get(header::RETRY_AFTER)
                    .unwrap_or_else(|| panic!("{name} 应带 Retry-After"))
                    .to_str()
                    .unwrap(),
                "3",
                "{name} 的 Retry-After 应透传上游/网关给的精确秒数"
            );
        }

        // 但对**重试层**必须可区分：响应体文案不同。
        let dump = |resp: axum::response::Response| async move {
            let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024)
                .await
                .expect("读取响应体");
            String::from_utf8_lossy(&bytes).to_string()
        };
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (a_body, c_body) = rt.block_on(async { (dump(admission).await, dump(cooling).await) });

        assert_ne!(
            a_body, c_body,
            "准入超时与全池冷却的响应体必须不同，否则重试层无法分辨（这正是缺陷本体）"
        );
        assert!(
            a_body.contains("backpressure"),
            "准入超时的文案应自述为网关背压，实际: {a_body}"
        );
        assert!(
            !a_body.contains("cooling down"),
            "准入超时绝不能复用全池冷却的 `cooling down` 文案 —— \
             kiro_shield 的 COOLING_MARKERS 命中它就会重试网关自己的背压。实际: {a_body}"
        );
        assert!(
            c_body.contains("cooling down"),
            "全池冷却的文案应保持不变（零回归），实际: {c_body}"
        );
    }

    /// 上游并发闸满（`upstream_gate_full=1`）必须：① 对客户端 429 + Retry-After（而非 502，
    /// 502 让客户端立即重发重新灌满闸门）；② 对吸收层判为**不可吸收**（不能被 `retry_after_secs=`
    /// 抢成 PoolCooldown 睡 2s 重打整链 +6s 延迟）。
    #[test]
    fn upstream_gate_full_is_429_not_absorbable() {
        // 吸收层分类：带 retry_after_secs=2 但必须返 None（网关自己的背压）。
        assert!(
            absorb_class_of("上游并发闸已满，停止本轮重试以免放大 upstream_gate_full=1 retry_after_secs=2")
                .is_none(),
            "gate-full 不得被当 PoolCooldown 吸收"
        );

        // map_provider_error：429 + Retry-After:2。
        let resp = map_provider_error(anyhow::Error::msg(
            "上游并发闸已满，停止本轮重试以免放大 upstream_gate_full=1 retry_after_secs=2",
        ));
        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(
            resp.headers().get(header::RETRY_AFTER).unwrap().to_str().unwrap(),
            "2",
            "gate-full 的 Retry-After 应为网关给的退避秒数"
        );
    }

    /// 取 `try_inbound_admission_gate` 的**函数体**源码（供准入闸门守卫用）。
    ///
    /// 为什么要切片而不是直接用整个文件：`include_str!` 会把本测试模块自己的
    /// 字面量也读进来（历史事故：同文件测试夹具里有两份带标记的完整消息副本，
    /// 全文件 `contains` 会在生产格式串丢掉标记时照样绿——本仓自述的
    /// 「源码级守卫自匹配」陷阱在迁移中被重新引入过一次）。
    fn inbound_admission_gate_source() -> &'static str {
        let full = include_str!("handlers.rs");
        let start = full
            .find("fn try_inbound_admission_gate(")
            .expect("准入闸门函数必须存在");
        let rest = &full[start..];
        // 函数体结束于下一个顶层 `\n}\n`（本函数内所有 `}` 都有缩进）。
        let end = rest.find("\n}\n").expect("准入闸门函数必须有结尾");
        &rest[..end]
    }

    #[test]
    fn admission_timeout_bail_must_carry_its_own_marker() {
        // 源码级守卫：准入闸门分支必须带标记、必须 bump 计数、必须 emit_record。
        // 只靠行为测试不够 —— 它喂的是手写字符串，而真正的风险是生产分支
        // **改了文案却没带标记 / 丢了观测**，那样行为测试照样绿、线上照样不可区分。
        // 2026-08-11：从全文件 contains 改为切片函数体，杜绝测试夹具自匹配。
        let body = inbound_admission_gate_source();
        let needle = format!("{}{}{}", "保护上游)", "inbound_admission_timeout", "=1");
        assert!(
            body.contains(&needle),
            "准入闸门格式串必须紧接 `保护上游)` 带上 inbound_admission_timeout=1 标记，\
             否则分类器无法把它与全池冷却区分开（两者都带 retry_after_secs=）"
        );
        assert!(
            body.contains("bump_inbound_admission_timeout"),
            "准入超时分支必须 bump 背压计数（面板可观测性，2026-08-11 恢复的断言）"
        );
        assert!(
            body.contains("emit_record"),
            "准入超时分支必须 emit_record（usage 统计可观测性，2026-08-11 恢复的断言）"
        );
    }

    #[test]
    fn admission_gate_placement_and_cc_coverage() {
        // 闸门必须位于 post_messages 的透传分叉**之前**，且 /cc/v1 入口也必须过闸。
        // 回退即 FAIL：把闸门移到透传块之后（透传再次 100% 绕闸，即本轮修复的缺陷
        // 本体）或删掉 post_messages_cc 的调用，本守卫都会红。
        let full = include_str!("handlers.rs");
        let pm = {
            let start = full
                .find("pub async fn post_messages(")
                .expect("post_messages 必须存在");
            let rest = &full[start..];
            let end = rest.find("\n}\n").expect("post_messages 必须有结尾");
            &rest[..end]
        };
        let gate_at = pm
            .find("try_inbound_admission_gate(")
            .expect("post_messages 必须调用入站闸门");
        let passthrough_at = pm
            .find("try_custom_api_passthrough(")
            .expect("post_messages 必须包含透传分叉");
        assert!(
            gate_at < passthrough_at,
            "准入闸门必须位于透传分叉之前，否则透传 100% 绕闸且现有守卫全部失明"
        );
        let cc = {
            let start = full
                .find("pub async fn post_messages_cc(")
                .expect("post_messages_cc 必须存在");
            let rest = &full[start..];
            let end = rest.find("\n}\n").expect("post_messages_cc 必须有结尾");
            &rest[..end]
        };
        assert!(
            cc.contains("try_inbound_admission_gate("),
            "/cc/v1 入口必须过入站闸门（闸门移到 handler 层后曾漏掉该入口，属回归）"
        );
    }

    #[test]
    fn v1_and_cc_call_shared_decode_frames_into() {
        // 双入口流式/缓冲/非流式都必须走同一 decode helper（3→1）。
        // needle 运行时拼接，避免 include_str 把本测试自己算进匹配。
        let handlers = include_str!("handlers.rs");
        let sibling = include_str!("handlers_dispatch.rs");
        let cut = handlers.find("#[cfg(test)]").unwrap_or(handlers.len());
        let prod = &handlers[..cut];
        let call = format!("{}{}", "decode_frames", "_into(");
        let def = format!("{}{}", "fn decode_frames", "_into");

        fn slice_fn<'a>(src: &'a str, sig: &str) -> &'a str {
            let start = src.find(sig).unwrap_or_else(|| panic!("{sig} 必须存在"));
            let rest = &src[start..];
            let end = rest.find("\n}\n").unwrap_or(rest.len());
            &rest[..end]
        }

        let sse = slice_fn(prod, "fn create_sse_stream(");
        let buffered = slice_fn(prod, "fn create_buffered_sse_stream(");
        let nonstream = slice_fn(prod, "fn handle_non_stream_request(");
        assert!(
            sse.contains(call.as_str()),
            "/v1 真流式（create_sse_stream，/cc 非 buffered 也走这里）必须调用共享 decode helper"
        );
        assert!(
            buffered.contains(call.as_str()),
            "/cc/v1 buffered（create_buffered_sse_stream，/v1 Claude Code 也走这里）必须调用共享 decode helper"
        );
        assert!(
            nonstream.contains(call.as_str()),
            "非流式路径必须调用共享 decode helper"
        );
        assert_eq!(
            prod.matches(call.as_str()).count(),
            3,
            "生产段恰好 3 处调用（stream / buffered / non-stream），不得再复制 decode_iter 循环"
        );
        assert!(
            !prod.contains("decode_iter()"),
            "handlers.rs 生产段不得再内联 decode_iter（应只在 sibling helper）"
        );
        assert!(
            sibling.contains(def.as_str()),
            "共享 decode helper 必须定义在 handlers_dispatch.rs"
        );
    }

    #[test]
    fn nonstream_must_run_bug_c_before_outbound_map() {
        let handlers = include_str!("handlers.rs");
        let cut = handlers.find("#[cfg(test)]").unwrap_or(handlers.len());
        let prod = &handlers[..cut];
        fn slice_fn<'a>(src: &'a str, sig: &str) -> &'a str {
            let start = src.find(sig).unwrap_or_else(|| panic!("{sig} 必须存在"));
            let rest = &src[start..];
            let end = rest.find("\n}\n").unwrap_or(rest.len());
            &rest[..end]
        }
        let nonstream = slice_fn(prod, "fn handle_non_stream_request(");
        let bug_c = format!("{}{}", "missing_required", "_keys(");
        let map_fn = format!("{}{}", "map_tool_input_from", "_kiro(");
        let bug_c_at = nonstream
            .find(&bug_c)
            .expect("非流式必须调用 missing_required_keys（Bug C）");
        let map_at = nonstream
            .find(&map_fn)
            .expect("非流式仍须出站 map_tool_input_from_kiro");
        assert!(
            bug_c_at < map_at,
            "Bug C 必须在出站还原之前，否则 Write 的 file_path 会对 path/text 假阳性"
        );
        assert!(
            nonstream.contains("tool_required_fields"),
            "非流式签名必须接收 converter 抽出的 required 表，不得再丢弃"
        );
    }

    #[test]
    fn post_messages_and_cc_have_request_span() {
        // 请求级 root span：双入口必须挂 tracing span，供跨凭据重试链聚合。
        // needle 运行时拼接；切片签名前的 attr 窗口，避免测试段自匹配。
        let handlers = include_str!("handlers.rs");
        let cut = handlers.find("#[cfg(test)]").unwrap_or(handlers.len());
        let prod = &handlers[..cut];
        let instrument = format!("{}{}", "tracing::", "instrument");
        let info_span = format!("{}{}", "info_span", "!");
        let skip = format!("{}{}", "skip", "_all");
        let cred_field = format!("{}{}", "credential_id = tracing::field::", "Empty");

        fn attr_window<'a>(prod: &'a str, sig: &str) -> &'a str {
            let at = prod.find(sig).unwrap_or_else(|| panic!("{sig} 必须存在"));
            let start = prod[..at].rfind("\n}\n").map(|i| i + 3).unwrap_or(0);
            &prod[start..at]
        }

        for sig in [
            "pub async fn post_messages(",
            "pub async fn post_messages_cc(",
        ] {
            let window = attr_window(prod, sig);
            let body_start = prod.find(sig).unwrap_or_else(|| panic!("{sig} 必须存在"));
            let rest = &prod[body_start..];
            let body_end = rest.find("\n}\n").unwrap_or(rest.len());
            let body = &rest[..body_end];
            let has_attr = window.contains(instrument.as_str());
            let has_info_span = body.contains(info_span.as_str());
            assert!(
                has_attr || has_info_span,
                "{sig} 必须有 tracing instrument 属性或 info_span 宏作为请求级 span"
            );
            if has_attr {
                assert!(
                    window.contains(skip.as_str()),
                    "{sig} 的 instrument 必须 skip_all，避免把 State/HeaderMap/body 打进日志"
                );
                assert!(
                    window.contains(cred_field.as_str()),
                    "{sig} 的 span 必须预留 credential_id 字段（选号后 record）"
                );
            }
        }
        let helper = format!("{}{}", "record_request_span_credential", "_id(");
        assert!(
            prod.contains(helper.as_str()),
            "选号后必须 record 到请求 span（passthrough / CallMeta）"
        );
        assert!(
            prod.matches(helper.as_str()).count() >= 4,
            "透传 + 流式 + 缓冲流式 + 非流式 四处 CallMeta/PassthroughMeta 都必须 record"
        );
    }

    #[test]
    fn compress_retry_loop_uses_extracted_target_fn() {
        // 压缩重试的目标必须走 compress_retry_target（防有人把公式内联回来再次写反向）。
        // ⚠️ needle 运行时拼接 + 切片锚定到循环结束（4 空格缩进的 `}`）：
        // 完整字面量若出现在源码里，include_str! 会把测试段/注释也读进来，生产被删后
        // `.find` 命中它们 → 守卫静默变绿（本仓踩过同型坑）。拼接后源码不存在完整
        // needle；循环级切片保证「移出循环但仍在函数内」的回退也会红。
        // 2026-08-16 W20：target 计算随 rebuild_body_for_compress_retry 提取进 helper
        // （F3 字节兜底），循环内调用 helper——断言改为「循环调用 helper + 全段
        // compress_retry_target 存在」，防 helper 内联回循环（目标公式反向回归）。
        let full = include_str!("handlers.rs");
        let needle = format!("{}: loop {}", "'compress_retry", "{");
        let start = full
            .find(needle.as_str())
            .expect("压缩重试循环必须存在");
        let end = full[start..]
            .find("\n    }\n")
            .map(|i| start + i)
            .unwrap_or(full.len());
        let body = &full[start..end];
        assert!(
            body.contains("rebuild_body_for_compress_retry("),
            "压缩重试循环必须调用 rebuild_body_for_compress_retry（字节兜底 + target 计算）"
        );
        let cut = full.find("#[cfg(test)]").unwrap_or(full.len());
        let prod = &full[..cut];
        assert!(
            prod.contains("compress_retry_target("),
            "compress_retry_target 必须存在（helper 内计算目标字节数）"
        );
    }

    #[test]
    fn compress_retry_loop_cc_coverage() {
        // /cc/v1 必须与 /v1 同款压缩重试循环（2026-08-11 审计缺口补齐）：
        // 循环标签、目标公式、轮末 strip 三者都必须落在 cc 函数体内。
        // ⚠️ needle 运行时拼接（同仓教训：完整字面量出现在测试/注释里会让守卫静默变绿）：
        // 循环标签拆成三段拼、strip 的头部名拆两段拼；切片锚定 cc 函数体
        // （"pub async fn post_messages_cc(" 到函数收尾 `}`），保证「循环挪进别的函数」
        // 的回退也红。
        let full = include_str!("handlers.rs");
        let cc = {
            let start = full
                .find("pub async fn post_messages_cc(")
                .expect("post_messages_cc 必须存在");
            let rest = &full[start..];
            let end = rest.find("\n}\n").expect("post_messages_cc 必须有结尾");
            &rest[..end]
        };
        let loop_needle = format!("{}: loop {}", "'compress_retry", "{");
        let start = cc
            .find(loop_needle.as_str())
            .expect("/cc/v1 必须与 /v1 同款压缩重试循环");
        // 循环收尾锚定：轮末 return 之后紧跟循环结束的 4 空格 `}`（本仓惯例循环收尾
        // 与函数收尾紧贴、无空行；cc 函数切片止于函数收尾 `}` 前，故用 return 行定位）。
        let return_at = cc[start..]
            .find("return final_response;")
            .map(|i| start + i)
            .expect("/cc/v1 压缩重试循环轮末必须 return");
        let end = cc[return_at..]
            .find("\n    }")
            .map(|i| return_at + i)
            .unwrap_or(cc.len());
        let loop_body = &cc[start..end];
        // W20：target 公式在 rebuild_body_for_compress_retry 内；循环必须调 helper，
        // 不得把 compress_retry_target 再内联回 /cc/v1 循环（与 /v1 守卫同口径）。
        assert!(
            loop_body.contains("rebuild_body_for_compress_retry("),
            "/cc/v1 压缩重试循环必须调用 rebuild_body_for_compress_retry"
        );
        let cut = full.find("#[cfg(test)]").unwrap_or(full.len());
        let prod = &full[..cut];
        assert!(
            prod.contains("compress_retry_target("),
            "compress_retry_target 必须存在（helper 内计算目标字节数）"
        );
        let strip_needle = format!("remove(\"x-kirostudio-{}\")", "compress-retry");
        assert!(
            loop_body.contains(strip_needle.as_str()),
            "/cc/v1 循环轮末必须 strip 内部标记头（2026-08-11 F1b 同款防泄漏，不得移出循环）"
        );
        // ⚠️ 强化（2026-08-11 对抗审查 m1）：锁「strip 在循环收尾之前」。
        // 盲区：把轮末改成 `break` 出循环、在循环外 strip 再 return（行为等价但结构
        // 迁移）时，上面的 loop_body 切片扩张到函数末尾、断言照样绿。两段锁死：
        // ① 循环收尾（return 之后的 4 空格 `}`）必须存在 —— break 写法下 return 在
        //    循环外，其后没有 4 空格 `}`（函数收尾是 0 空格），此处 expect 直接红；
        // ② strip 必须位于循环收尾之前。
        let loop_end_at = cc[return_at..]
            .find("\n    }")
            .expect("循环收尾（4 空格 `}`）必须紧跟轮末 return 之后 —— 若 break 出循环\
                    再 return，此处必红");
        let strip_at = cc[start..]
            .find(strip_needle.as_str())
            .map(|i| start + i)
            .unwrap_or(usize::MAX);
        assert!(
            strip_at < return_at + loop_end_at,
            "strip 必须位于循环收尾之前（不得 break 出循环后再 strip）"
        );
    }

    #[test]
    fn compress_retry_target_strictly_decreasing_with_floor() {
        let trigger = 4 * 1024 * 1024; // 4 MiB（默认 trigger_bytes）
        let a1 = compress_retry_target(trigger, 1);
        let a2 = compress_retry_target(trigger, 2);
        let a3 = compress_retry_target(trigger, 3);
        // 0.75 → 0.5625 → 0.421875：逐轮更紧，绝不能反弹（历史 bug：序列反向）。
        assert!(
            a1 < trigger && a2 < a1 && a3 < a2,
            "target 必须逐轮递减，a1={a1} a2={a2} a3={a3}"
        );
        assert_eq!(a1, trigger * 3 / 4);
        assert_eq!(a2, trigger * 9 / 16);
        assert_eq!(a3, trigger * 27 / 64);
        // 下限 64 KiB；attempt=0 恒等（文档语义：初试用配置值，本函数只用于重试）。
        assert_eq!(compress_retry_target(1024, 3), 65536);
        assert_eq!(compress_retry_target(trigger, 0), trigger);
    }

    /// 压缩重试重建 body 的 native effort 字段携带（deep 审计补测，2026-08-11）：
    /// `build_kiro_request_body` 带 `additionalModelRequestFields` 时，初试与重试
    /// （更小 target_bytes）两次序列化都必须含该字段——P1 移植与 P0-2 压缩重试的
    /// 交叉点，丢字段 = 重试请求的 extended thinking 静默失效。
    #[test]
    fn compress_retry_rebuild_keeps_additional_model_request_fields() {
        use crate::kiro::model::requests::kiro::{AdditionalModelRequestFields, KiroOutputConfig};
        use crate::model::config::CompressionConfig;

        let state = crate::kiro::model::requests::conversation::ConversationState::new("conv-1");
        let fields = Some(AdditionalModelRequestFields {
            output_config: Some(KiroOutputConfig {
                effort: "xhigh".to_string(),
            }),
            reasoning: None,
        });
        let mut cfg = CompressionConfig::default();
        cfg.enabled = true;
        cfg.trigger_bytes = 1024;

        let body_initial = build_kiro_request_body(state.clone(), fields.clone(), &cfg, None)
            .expect("初试序列化应成功");
        assert!(
            body_initial.contains("additionalModelRequestFields")
                && body_initial.contains("output_config"),
            "初试 body 必须含 native effort 字段"
        );

        let body_retry = build_kiro_request_body(state, fields, &cfg, Some(256))
            .expect("重试序列化应成功");
        assert!(
            body_retry.contains("additionalModelRequestFields")
                && body_retry.contains("output_config"),
            "压缩重试重建 body 不得丢 native effort 字段（键与 effort 都要在，\
             只保键不保 effort 同样等于静默失效）"
        );
    }

    /// 守卫：压缩重试循环重建 body 时**必须**把捕获的 native fields 传进去。
    /// 行为测试（上面那条）只证明函数本身不丢字段；这条钉的是循环调用点——
    /// 若有人把调用改回不传 fields（或注释掉捕获行），行为测试照样绿（函数能力
    /// 没变），只有这条会红。
    #[test]
    fn compress_retry_rebuild_passes_native_fields_through() {
        let full = include_str!("handlers.rs");
        let needle = format!("{}: loop {}", "'compress_retry", "{");
        let start = full
            .find(needle.as_str())
            .expect("压缩重试循环必须存在");
        let end = full[start..]
            .find("\n    }\n")
            .map(|i| start + i)
            .unwrap_or(full.len());
        let body = &full[start..end];
        let field = format!("native_fields_for_{}", "compress_retry");
        assert!(
            body.contains(field.as_str()),
            "压缩重试循环重建 body 时必须传入捕获的 native effort 字段 \
             （丢了 = 重试请求的 extended thinking 静默失效）"
        );
    }

    /// 行为测试：CONTENT_LENGTH_EXCEEDS 重试路径重建 body 时**先走字节兜底**。
    ///
    /// 上游按字节拒绝（400 CONTENT_LENGTH_EXCEEDS_THRESHOLD），token 压缩压不到目标
    /// 时（长会话字节先撞线），重试重建必须先截断历史（量纲对齐）再走 token 压缩。
    /// 断言的是传入的 state 被就地字节兜底（历史变短 + 首条占位）：token 压缩的
    /// L1-L5 层做不到"插入占位说明"，占位出现即证明截断路径真正执行了。
    #[test]
    fn compress_retry_rebuild_applies_byte_overflow_guard() {
        use crate::model::config::CompressionConfig;

        let big = "p".repeat(200 * 1024);
        let mut hist = Vec::new();
        for _ in 0..6 {
            hist.push(crate::kiro::model::requests::conversation::Message::User(
                crate::kiro::model::requests::conversation::HistoryUserMessage::new(
                    &big,
                    "claude-sonnet-4.5",
                ),
            ));
            hist.push(crate::kiro::model::requests::conversation::Message::Assistant(
                crate::kiro::model::requests::conversation::HistoryAssistantMessage::new(&big),
            ));
        }
        let mut state = crate::kiro::model::requests::conversation::ConversationState::new("conv-1")
            .with_history(hist);

        let mut cfg = CompressionConfig::default();
        cfg.enabled = true;
        cfg.trigger_bytes = 4 * 1024 * 1024;

        let body = rebuild_body_for_compress_retry(&mut state, &None, &cfg, 1)
            .expect("重试重建应成功");

        // 字节兜底执行证据：历史被截断 + 首条为占位说明（token 压缩不做这两件事）。
        assert!(
            state.history.len() < 12,
            "重试路径必须先按字节截断历史，实际保留 {} 条",
            state.history.len()
        );
        match state.history.first() {
            Some(crate::kiro::model::requests::conversation::Message::User(m)) => assert_eq!(
                m.user_input_message.content,
                crate::anthropic::converter::TRUNCATION_PLACEHOLDER,
                "截断处必须插入占位说明"
            ),
            other => panic!("首条应为占位 user 消息，实际: {other:?}"),
        }
        // 截断后 ≤ 900KB，必低于 attempt=1 的 target（4MiB × 3/4 = 3MiB）。
        assert!(
            body.len() <= 3 * 1024 * 1024,
            "重试 body 应被压到 target 以内，实际 {}",
            body.len()
        );
    }

    /// 守卫：双入口压缩重试循环都走 `rebuild_body_for_compress_retry`，且该函数
    /// 内部必须调用字节兜底（`apply_byte_overflow_guard`）。行为测试只能证明函数
    /// 能力本身，钉的是接线——若有人把某入口的调用改回旧的 `build_kiro_request_body`
    /// 直连（跳过字节兜底），行为测试照样绿，只有这条会红。
    #[test]
    fn compress_retry_loops_wired_to_byte_overflow_guard() {
        let full = include_str!("handlers.rs");
        let prod = full.split("\n#[cfg(test)]").next().unwrap_or(full);
        // 函数定义 1 处 + 双入口循环（/v1、/cc/v1）各 1 处调用 = 3。
        let calls = prod.matches("rebuild_body_for_compress_retry(").count();
        assert!(
            calls >= 3,
            "重试重建必须由双入口共用 helper 且 helper 被调用，实际 {calls} 处"
        );
        let helper_start = prod
            .find("fn rebuild_body_for_compress_retry(")
            .expect("rebuild_body_for_compress_retry 必须存在");
        let helper_end = prod[helper_start..]
            .find("\n}\n")
            .map(|i| helper_start + i)
            .unwrap_or(prod.len());
        let helper = &prod[helper_start..helper_end];
        assert!(
            helper.contains("apply_byte_overflow_guard(conv_state)"),
            "重试重建必须调用字节兜底（sanitize + 字节截断）；带实参形态匹配，\
             防 doc 注释里的裸词冒充真实调用"
        );
    }

    #[test]
    fn test_translate_quota_exhausted() {
        let t = translate_upstream_error("upstream: MONTHLY_REQUEST_COUNT limit reached").unwrap();
        assert_eq!(t.status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(t.error_type, "rate_limit_error");
        assert!(t.message.contains("配额") && t.message.contains("排障"));
    }

    /// 🔴 回归（2026-08-10）：**全池配额耗尽只由显式标记断言，裸串不得冒充**。
    ///
    /// 缺陷：`translate_quota_subscription` 原先用裸串 `MONTHLY_REQUEST_COUNT` / `QUOTA`
    /// 判「月度配额耗尽」，而那两个串来自**上游 body**。单号耗尽时 provider 走「换号
    /// continue」分支，其 `last_error` 同样带这两个串，且 `last_error` **刻意不重置**
    /// ⇒ 池里其余号健康时，最终错误仍被判成"全部凭据配额耗尽"，归因口径被污染。
    ///
    /// 现在：带 `quota_exhausted_all=1`（provider 确认 `has_available == false` 后才打）
    /// 的才断言"号池内所有凭据"；裸串降级为不断言范围的通用配额文案。
    /// 带标记走 402 billing_error（停手）；裸串保持 429（可退避）。删掉裸串会让
    /// MCP/透传等路径的配额错误落 502 兜底 → 客户端当永久故障不退避。
    #[test]
    fn quota_exhausted_all_marker_distinguishes_pool_wide_from_single_credential() {
        // ① 带标记 → 明确断言"所有凭据"
        let all = translate_upstream_error(
            "流式 API 请求失败（所有凭据已用尽）quota_exhausted_all=1: 402 {\"reason\":\"MONTHLY_REQUEST_COUNT\"}",
        )
        .expect("带标记应命中配额分支");
        assert_eq!(all.status, StatusCode::PAYMENT_REQUIRED);
        assert_eq!(all.error_type, "billing_error");
        assert!(
            all.retry_after_override.is_none(),
            "402 不得带 Retry-After（否则诱导当月反复砸）"
        );
        assert!(
            all.message.contains("所有凭据"),
            "带 quota_exhausted_all=1 时必须断言范围是整个号池，实际: {}",
            all.message
        );

        // ② 只有裸串（单号耗尽后换号，链上残留的上游 body）→ **不得**断言"所有凭据"
        let single = translate_upstream_error(
            "流式 API 请求失败: 402 {\"reason\":\"MONTHLY_REQUEST_COUNT\"}",
        )
        .expect("裸串仍须命中配额分支（不能落 502 兜底）");
        assert_eq!(
            single.status,
            StatusCode::TOO_MANY_REQUESTS,
            "裸串必须仍返 429（可退避）——落 502 会让客户端当永久故障不退避"
        );
        assert!(
            !single.message.contains("所有凭据"),
            "只有裸串时**不能**断言\"所有凭据\"（那是标记分支才能确认的事实）。\
             池里其余号可能仍健康，错误归因不该扩大范围。实际: {}",
            single.message
        );
        assert!(
            single.message.contains("配额") && single.message.contains("排障"),
            "裸串分支仍须给出可操作的排障提示，实际: {}",
            single.message
        );
    }

    #[test]
    fn test_translate_region_not_activated() {
        let t = translate_upstream_error("403 FEATURE_NOT_SUPPORTED for this region").unwrap();
        assert_eq!(t.error_type, "api_error");
        assert!(t.message.contains("region") && t.message.contains("Profile ARN"));
    }

    #[test]
    fn test_translate_subscription_invalid() {
        let t = translate_upstream_error("Invalid token: subscription expired").unwrap();
        assert!(t.message.contains("刷新 Token") && t.message.contains("排障"));
    }

    #[test]
    fn test_translate_context_full() {
        let t = translate_upstream_error("CONTENT_LENGTH_EXCEEDS_THRESHOLD").unwrap();
        assert_eq!(t.status, StatusCode::BAD_REQUEST);
        assert!(t.message.contains("上下文") && t.message.contains("精简"));
        // 英文哨兵与中文文案**同时**存在（前缀不是替换）。子串契约本身由
        // `overflow_errors_must_match_claude_code_compact_retry_predicate` 钉死。
        assert!(t.message.starts_with(OVERFLOW_COMPACT_HINT));
    }

    #[test]
    fn test_translate_input_too_long() {
        let t = translate_upstream_error("Input is too long for the model").unwrap();
        assert_eq!(t.status, StatusCode::BAD_REQUEST);
        assert!(t.message.contains("输入过长") && t.message.contains("拆分"));
        assert!(t.message.starts_with(OVERFLOW_COMPACT_HINT));
    }

    /// ⭐ 外部契约守卫（承重）：「装不下」类错误的 message **必须**命中 Claude Code 的
    /// compact-and-retry 判据，否则用户的自动压缩静默失效。
    ///
    /// # 这条断言在保护什么
    ///
    /// Claude Code（本机 2.1.220 实测）判「该压缩后重试」的方式是对错误 message 做
    /// **小写化子串匹配**，形如：
    ///
    /// ```text
    /// msg.toLowerCase().includes("prompt is too long")
    ///   || msg.toLowerCase().includes("input is too long for requested model")
    /// ```
    ///
    /// 命中后它会压缩上下文并**自动重试**；不命中就只是把错误打给用户。而它的另一条
    /// 「按水位主动压缩」的路径在网关模式下结构性不可用（见
    /// `docs/auto-compact-fix-2026-08-06.md`）⇒ 这个子串是网关唯一能给用户的自动压缩。
    ///
    /// # 为什么必须单列一条，而不是靠上面两条整串比对
    ///
    /// 上面两条测的是**中文文案还在不在**。有人润色文案时顺手去掉英文前缀，那两条照样绿
    /// （它们断言的是"上下文"/"输入过长"这些中文词），而保险丝已经烧了。本条直接断言
    /// **外部消费者的判据能命中**，是唯一一处「改文案会立刻变红」的地方。
    ///
    /// ⚠️ 这里刻意写**字面量**而不是引用 `OVERFLOW_COMPACT_HINT`：引用了就是同义反复
    /// （把常量改成空串，断言依然成立），钉不住任何东西。契约的对面是别人的二进制，
    /// 本仓这侧只能用字面量表达。
    ///
    /// ⚠️ 大小写：判据先 `toLowerCase()`，故本断言也先 `to_lowercase()` —— 文案首字母
    /// 将来若改成大写，契约仍然成立，测试不该因此误红。
    #[test]
    fn overflow_errors_must_match_claude_code_compact_retry_predicate() {
        use axum::body::to_bytes;

        // Claude Code 侧的两个判据字面量（小写形态）。任一命中即触发 compact-and-retry。
        const CC_SENTINELS: [&str; 2] = [
            "prompt is too long",
            "input is too long for requested model",
        ];

        // 两类上游「装不下」原文（实测形态）。
        for upstream in [
            "非流式 API 请求失败: 400 Bad Request {\"reason\":\"CONTENT_LENGTH_EXCEEDS_THRESHOLD\"}",
            "流式 API 请求失败: 400 Bad Request {\"message\":\"Input is too long for the model\"}",
        ] {
            // ⭐ 走 `map_provider_error` **全路径**读真实响应体，不是只调 `translate_*`。
            //
            // 理由（本仓「纸面测试」的第 8 种形态）：只测分支内部，对**分支顺序**完全不可见。
            // 客户端真正拿到的是本函数的输出，而它前面还排着若干条 return（吸收层耗尽的 503
            // 覆盖、准入超时、全池冷却、限流、临时风控、region 错配…）。将来任何一条被放宽到
            // 能匹配这两个 400 串，客户端拿到的 message 里就不再有哨兵 —— 而只调
            // `translate_upstream_error` 的断言那时**依然全绿**。
            let resp = map_provider_error(anyhow::Error::msg(upstream));
            assert_eq!(
                resp.status(),
                StatusCode::BAD_REQUEST,
                "「装不下」必须仍是 400（重试原请求无意义，客户端要做的是压缩后重试）：{upstream}"
            );
            let body = futures::executor::block_on(to_bytes(resp.into_body(), usize::MAX)).unwrap();
            let text = String::from_utf8_lossy(&body);
            let low = text.to_lowercase();
            assert!(
                CC_SENTINELS.iter().any(|s| low.contains(s)),
                "响应体未命中 Claude Code 的 compact-and-retry 判据 ⇒ 用户撞满上下文后不会自动\
                 压缩重试，只会看到报错。改文案时必须保留英文哨兵子串。实际响应体: {text}"
            );
            // 中文排障文案不得因为加哨兵而丢失（两个受众各拿到自己那份）。
            assert!(
                text.contains("排障"),
                "英文哨兵是**前缀**不是替换，中文排障步骤必须仍在: {text}"
            );
        }
    }

    #[test]
    fn test_translate_network_dns() {
        let t = translate_upstream_error("error trying to connect: dns error: failed to resolve")
            .unwrap();
        assert_eq!(t.status, StatusCode::BAD_GATEWAY);
        assert!(t.message.contains("DNS") && t.message.contains("排障"));
    }

    #[test]
    fn test_translate_network_timeout() {
        // 纯 reqwest 超时(无 HTTP 状态码语境)。
        let t = translate_upstream_error("operation timed out").unwrap();
        assert_eq!(t.status, StatusCode::GATEWAY_TIMEOUT);
        assert!(t.message.contains("超时"));
    }

    #[test]
    fn test_translate_tls() {
        // 真实 reqwest TLS 错误在建连阶段,Display 带 "error trying to connect" 传输标志。
        let t = translate_upstream_error(
            "error trying to connect: invalid certificate: SSL handshake failed",
        )
        .unwrap();
        assert!(t.message.contains("TLS") || t.message.contains("证书"));
    }

    #[test]
    fn test_translate_proxy() {
        // 真实 reqwest 代理错误同样在建连阶段包裹。
        let t = translate_upstream_error("error trying to connect: proxy CONNECT failed").unwrap();
        assert!(t.message.contains("代理"));
    }

    #[test]
    fn test_translate_unknown_returns_none() {
        // 未知错误必须返回 None（调用方诚实透传原文，不臆造排障步骤）。
        assert!(translate_upstream_error("some totally unrecognized upstream gibberish").is_none());
    }

    /// review 泄露回归:未知错误的 map_provider_error 响应体**绝不含**原始错误链里的敏感信息
    /// (profileArn / AWS 账号号 / region / 内部 URL)。只给通用提示 + 引导查日志。
    #[test]
    fn test_unknown_error_response_body_no_sensitive_leak() {
        use axum::body::to_bytes;
        // 构造一个含敏感信息的未知错误(模拟上游响应体泄露 ARN/账号)。
        let leaky = anyhow::anyhow!(
            "API 请求失败: 500 {{\"detail\":\"profile arn:aws:codewhisperer:eu-central-1:123456789012:profile/SECRET failed\"}}"
        );
        let resp = map_provider_error(leaky);
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
        let body = futures::executor::block_on(to_bytes(resp.into_body(), usize::MAX)).unwrap();
        let text = String::from_utf8_lossy(&body);
        // 客户端拿到的响应体绝不含任何敏感片段。
        assert!(!text.contains("arn:aws"), "响应体泄露了 ARN: {}", text);
        assert!(
            !text.contains("123456789012"),
            "响应体泄露了 AWS 账号号: {}",
            text
        );
        assert!(
            !text.contains("SECRET"),
            "响应体泄露了 profile id: {}",
            text
        );
        assert!(
            !text.contains("eu-central-1"),
            "响应体泄露了 region: {}",
            text
        );
        // 仍给出通用引导。
        assert!(text.contains("未识别错误") && text.contains("网关日志"));
    }

    /// review high 回归:上游 HTTP 错误**响应体**里恰好含 timeout/tls/proxy/resolve 字样时,
    /// **绝不**被误判成网络故障(它不是传输层错误,无 "error sending request" 等标志)。
    #[test]
    fn test_translate_network_no_false_positive_on_upstream_body() {
        // 模拟 provider 格式化的上游错误串(含 HTTP 状态码 + body,body 里有 "timeout"/"proxy" 字样)。
        let upstream_body = "流式 API 请求失败: 400 {\"message\":\"your request proxy timeout config is invalid, tls off\"}";
        // is_transport_error 应判 false → translate_network 返回 None → 整体不误翻译。
        assert!(!is_transport_error(&upstream_body.to_lowercase()));
        assert!(
            translate_network(upstream_body).is_none(),
            "上游 body 含 timeout/proxy/tls 字样不应被误判成网络故障"
        );
    }

    /// ⭐ M3 回归：`subscription_unsupported=1`（provider 打的**永久**条件标记）必须
    /// 映射成 404 `not_found_error`，绝不能落进下方 `contains("subscription")` 宽匹配
    /// 被译成 502 `api_error` +「刷新 Token」误导文案。
    ///
    /// 旧路径（必失败）：该标记串含 "subscription" 子串 → 宽匹配先行命中 →
    /// 502 可重试语义 → 客户端按 5xx 盲退避重打一个**重试永远不会变**的请求；
    /// 排障文案还让管理员去「刷新 Token」——订阅档位缺失刷新多少次都没用。
    #[test]
    fn subscription_unsupported_maps_to_404_not_found_not_502() {
        // provider.rs:3094 那条 bail 的原文形态（上游 body 原样拼接）。
        let permanent = "Kiro API 请求失败（订阅不支持该应用/模型，换区与重试均无效）: 403 {\"message\":\"profile does not support this model\"} subscription_unsupported=1";
        let translated = translate_upstream_error(permanent)
            .expect("subscription_unsupported=1 必须被翻译，不能落 502 兜底");
        assert_eq!(
            translated.status,
            StatusCode::NOT_FOUND,
            "订阅档位不支持是永久条件：404 不可重试，502 会诱导客户端盲退避重打"
        );
        assert_eq!(translated.error_type, "not_found_error");
        assert!(
            !translated.message.contains("刷新 Token"),
            "订阅档位缺失与 token 无关，绝不给「刷新 Token」误导排障动作"
        );
        // 走真实出口验证 HTTP 码（防谓词测试纸面化）。
        let resp = map_provider_error(anyhow::Error::msg(permanent));
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        assert!(
            resp.headers().get(header::RETRY_AFTER).is_none(),
            "永久条件绝不带 Retry-After（带了等于宣称「等一会儿会好」）"
        );
    }

    /// M3 对照组：不带标记的裸 `subscription` 文案**仍**走原 502 兜底分支
    /// （收口只切掉了带标记的那一类，没有把整条宽匹配关掉）。
    #[test]
    fn bare_subscription_without_marker_still_maps_502() {
        let bare = "上游返回: subscription has expired for this profile";
        let translated = translate_upstream_error(bare)
            .expect("裸 subscription 文案仍应被翻译（兜底）");
        assert_eq!(
            translated.status,
            StatusCode::BAD_GATEWAY,
            "裸 subscription 分支必须保留（MCP/透传路径可能不带标记冒泡上来）"
        );
    }

    /// 🔴 配额兜底分支判据收口（2026-08-15）：只认 endpoint 词表里的 reason 码
    /// （MONTHLY_REQUEST_COUNT / OVERAGE_REQUEST_LIMIT_EXCEEDED），
    /// 裸 `QUOTA` 字样**不再**命中配额分支（宽判据误伤：无关文案含 QUOTA 会被
    /// 429 配额文案误导退避）。
    ///
    /// 回退即 FAIL：把判据改回 `contains("QUOTA")`，反例组全部误判为配额耗尽。
    #[test]
    fn quota_fallback_uses_exact_reason_codes_not_bare_quota() {
        // 正例：Kiro 实测 reason 码（月度 + overage，endpoint QUOTA_EXHAUSTED_REASONS 同款）。
        for body in [
            r#"流式 API 请求失败: 402 {"reason":"MONTHLY_REQUEST_COUNT"}"#,
            r#"流式 API 请求失败: 402 {"message":"You have reached the limit for overages.","reason":"OVERAGE_REQUEST_LIMIT_EXCEEDED"}"#,
        ] {
            let t = translate_upstream_error(body)
                .expect("配额 reason 码必须命中配额分支（不能落 502 兜底）: {body}");
            assert_eq!(
                t.status,
                StatusCode::TOO_MANY_REQUESTS,
                "配额 reason 码必须仍返 429（可退避）: {body}"
            );
        }
        // 反例（误伤场景）：含大写 QUOTA 但与配额耗尽无关 —— 不得被判配额分支。
        for body in [
            "上游返回: QUOTA tier not available for this model",
            r#"{"error":{"code":"QUOTA","message":"configuration error"}}"#,
        ] {
            assert!(
                translate_upstream_error(body).is_none(),
                "非配额耗尽的 QUOTA 字样不得命中配额分支（宽判据误伤）: {body}"
            );
        }
    }

    /// 🔴 凭据分支 `subscription` 判据收窄（2026-08-15）：只认**失效**连续形态。
    /// 「subscription does not support」（订阅档位类）与「subscription rate limit」
    /// （限流类）不得命中 invalid_credential —— 否则给「刷新 Token」误导排障。
    ///
    /// 回退即 FAIL：把判据改回裸 `subscription`，反例组全部误判为凭据失效。
    #[test]
    fn subscription_judgement_is_credential_invalidity_phrases_only() {
        // 正例：订阅失效形态仍命中（502 invalid_credential + 刷新 Token 排障）。
        for body in [
            "上游返回: subscription has expired for this profile",
            "Invalid token: subscription expired",
            "上游返回: no active subscription for this profile",
            "上游返回: your subscription is invalid",
            "上游返回: subscription is no longer active",
        ] {
            let t = translate_upstream_error(body)
                .expect("订阅失效文案必须命中 invalid_credential 分支: {body}");
            assert_eq!(t.status, StatusCode::BAD_GATEWAY, "订阅失效仍应 502: {body}");
            assert!(
                t.message.contains("刷新 Token"),
                "订阅失效的排障应指向刷新 Token: {body}"
            );
        }
        // 反例（误伤场景）：非失效的 subscription 字样不得命中（不再给「刷新 Token」误导）。
        for body in [
            "上游返回: your subscription does not support this application",
            "上游返回: the request exceeds your subscription rate limit",
            r#"上游返回: {"message":"subscription tier requires a higher plan"}"#,
        ] {
            assert!(
                translate_upstream_error(body).is_none(),
                "非订阅失效的 subscription 字样不得命中 invalid_credential（宽判据误伤）: {body}"
            );
        }
    }

    /// M4 辅助函数回归：空响应的错误形态（文案/类型）由流式与非流式共用。
    #[test]
    fn empty_response_shape_matches_streaming_contract() {
        let (err_type, message, _ra) = empty_response_error_shape(true);
        assert_eq!(err_type, "invalid_request_error");
        assert!(message.contains("/compact"), "大输入必须提示压缩上下文");
        let (err_type, message, _ra) = empty_response_error_shape(false);
        assert_eq!(err_type, "overloaded_error");
        assert!(message.contains("重试"), "偶发空响应必须可重试");
    }

    /// M4 回归：非流式空响应的 HTTP 码 —— 大输入 400（重试还是同样的大请求）、
    /// 偶发 429（客户端可重试）。该判据与流式共用同一阈值函数。
    #[test]
    fn non_stream_empty_response_status_codes() {
        // 直接断言收尾用的状态码映射逻辑（handle_non_stream_request 需真实上游，
        // 这里把「阈值判定 → 状态码」的对应关系钉住）。
        let model = "claude-sonnet-5";
        let threshold = crate::anthropic::stream::empty_response_oversized_threshold(model);
        // 大输入（≥ 阈值）→ 400 invalid_request_error
        {
            let oversized = 1_000_000_000 >= threshold;
            assert!(oversized);
            let (err_type, _, _) = empty_response_error_shape(oversized);
            assert_eq!(err_type, "invalid_request_error");
            let status = if oversized {
                StatusCode::BAD_REQUEST
            } else {
                StatusCode::TOO_MANY_REQUESTS
            };
            assert_eq!(status, StatusCode::BAD_REQUEST);
        }
        // 小输入（< 阈值）→ 429 overloaded_error
        {
            let oversized = 100 >= threshold;
            assert!(!oversized);
            let (err_type, _, _) = empty_response_error_shape(oversized);
            assert_eq!(err_type, "overloaded_error");
            let status = if oversized {
                StatusCode::BAD_REQUEST
            } else {
                StatusCode::TOO_MANY_REQUESTS
            };
            assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
        }
    }

    /// 清单 11：非流式「只有 thinking 块」判定 —— 有非 thinking 内容即 false，
    /// 全 thinking 才 true（与流式 has_non_thinking_blocks 口径对齐）。
    #[test]
    fn content_is_thinking_only_matches_streaming_semantics() {
        use serde_json::json;
        // 只有 thinking → true（需要补空格 text 兜底）
        assert!(content_is_thinking_only(&[json!({"type": "thinking", "thinking": "x"})]));
        // 空数组 → false（那是 M4 的完全空响应，不归 thinking-only 管）
        assert!(!content_is_thinking_only(&[]));
        // thinking + text → false
        assert!(!content_is_thinking_only(&[
            json!({"type": "thinking", "thinking": "x"}),
            json!({"type": "text", "text": "answer"}),
        ]));
        // thinking + tool_use → false
        assert!(!content_is_thinking_only(&[
            json!({"type": "thinking", "thinking": "x"}),
            json!({"type": "tool_use", "name": "Bash"}),
        ]));
        // 无 type 字段的块按非 thinking 处理（保守）
        assert!(!content_is_thinking_only(&[json!({"text": "x"})]));
        // 仅 server_tool_use（websearch 场景）→ false
        assert!(!content_is_thinking_only(&[json!({"type": "server_tool_use"})]));
    }

    /// 测试串行锁：错误消息配置表是进程级全局静态（ArcSwap 镜像，与 IP 黑名单同款），
    /// 多个测试并行读写会互相污染（一个测试 set 配置会让另一个测试的默认文案断言失败）。
    /// 凡改 ERROR_MESSAGES 镜像的测试都先取此锁，串行执行（范式同 BLOCKLIST_TEST_LOCK）。
    ///
    /// `pub(crate)`：websearch.rs 的 f3 测试也改这个全局镜像（跨模块共享），
    /// 必须持同一把锁，否则与 handlers 锁内测试双向污染（单跑绿全量随机红，2026-08-15 审计）。
    pub(crate) static ERROR_MESSAGES_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn test_table() -> ErrorMessagesTable {
        ErrorMessagesTable::new()
    }

    /// 读响应体为字符串（同步测试里用 current_thread runtime，范式同
    /// `test_inbound_admission_timeout_is_distinguishable_from_pool_cooling`）。
    fn block_on_body(resp: axum::response::Response) -> String {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024)
                    .await
                    .expect("读取响应体");
                String::from_utf8_lossy(&bytes).to_string()
            })
    }

    /// 错误消息可配置化：①配置的 message 生效（map_provider_error 渲染配置值）；
    /// ②marker 分支判断不受配置影响（分支仍按 marker 命中，marker 在 err_str 里不在
    /// message 里——message 换配置值后标记逻辑照旧）；③**号池真值永远优先于配置**
    /// （`retry_after_secs=10` 时配置的 99 被覆盖）；④未配置 → 现状文案逐字不变（回归）。
    #[test]
    fn configured_message_renders_marker_stays_pool_truth_wins() {
        let _guard = ERROR_MESSAGES_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // ① 未配置 → 现状文案不变（先回归后配置，顺序无所谓，set 恒先于断言）。
        let resp_default = map_provider_error(anyhow::Error::msg(format!(
            "所有凭据均在冷却（0/4）retry_after_secs=10 {}",
            ABSORB_BUDGET_EXHAUSTED_MARKER
        )));
        let default_body = block_on_body(resp_default);
        assert!(
            default_body.contains("等容量"),
            "未配置时必须仍是现状文案（含承重词「等容量」）"
        );

        // ② 配置 message + retryAfterSecs=99。
        let mut table = test_table();
        table.insert(
            "absorb_exhausted".to_string(),
            crate::model::config::ErrorMessageOverride {
                status: None,
                r#type: None,
                message: Some("配置的吸收层耗尽文案".to_string()),
                retry_after_secs: Some(99),
            },
        );
        set_error_messages(table);
        let raw = format!(
            "所有凭据均在冷却（0/4）retry_after_secs=10 {}",
            ABSORB_BUDGET_EXHAUSTED_MARKER
        );
        let resp = map_provider_error(anyhow::Error::msg(raw));
        assert_eq!(
            resp.status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "带耗尽标记仍必须命中吸收层分支（marker 判定不受配置影响）"
        );
        // 头必须在 move body 之前读（block_on_body 消费 resp）。
        assert_eq!(
            resp.headers().get(header::RETRY_AFTER).and_then(|v| v.to_str().ok()),
            Some("10"),
            "号池真值 retry_after_secs=10 必须优先于配置的 99（真值比任何配置都准）"
        );
        let body = block_on_body(resp);
        assert!(
            body.contains("配置的吸收层耗尽文案"),
            "配置 message 必须生效，实际: {body}"
        );
        assert!(
            !body.contains("等容量"),
            "配置 message 替换默认文案（含承重词；配置侧自行保证保留）"
        );

        // ③ 复位（防污染其它测试的全局镜像）。
        set_error_messages(test_table());
    }

    /// 配置的 status/type 同样生效（message 之外的渲染字段），且 marker 判据不变。
    #[test]
    fn configured_status_and_type_override_render() {
        let _guard = ERROR_MESSAGES_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let mut table = test_table();
        table.insert(
            "rate_limited_pool".to_string(),
            crate::model::config::ErrorMessageOverride {
                status: Some(503),
                r#type: Some("overloaded_error".to_string()),
                message: None,
                retry_after_secs: None,
            },
        );
        set_error_messages(table);
        let resp = map_provider_error(anyhow::Error::msg(
            "所有凭据均在冷却（0/2）retry_after_secs=10",
        ));
        assert_eq!(
            resp.status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "配置的 status 必须生效"
        );
        // type 在 body 里。
        let body = block_on_body(resp);
        assert!(body.contains("\"overloaded_error\""), "配置的 type 必须生效");
        assert!(
            body.contains("temporarily cooling down"),
            "未配置的 message 字段仍用默认文案"
        );
        // 复位。
        set_error_messages(test_table());
    }

    /// B4 矛盾修复（设计 §五 1）：容量 503 现状无 Retry-After（客户端不退避），
    /// 补默认 3s（key `overloaded_capacity` 的 retryAfterSecs 默认值）。
    #[test]
    fn b4_capacity_503_carries_default_retry_after() {
        // 400 形态（INSUFFICIENT_MODEL_CAPACITY）与 503 形态（MODEL_TEMPORARILY_UNAVAILABLE）
        // 都必须带默认 Retry-After: 3。
        for raw in [
            r#"流式 API 请求失败: 400 Bad Request {"reason":"INSUFFICIENT_MODEL_CAPACITY"}"#,
            r#"流式 API 请求失败: 503 Service Unavailable {"reason":"MODEL_TEMPORARILY_UNAVAILABLE"}"#,
        ] {
            let resp = map_provider_error(anyhow::Error::msg(raw));
            assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
            assert_eq!(
                resp.headers()
                    .get(header::RETRY_AFTER)
                    .and_then(|v| v.to_str().ok()),
                Some("3"),
                "容量 503 必须带默认 Retry-After（B4 修复），实际串: {raw}"
            );
        }
    }

    /// D10 矛盾修复（设计 §五 2）：空响应 429（小输入偶发）补默认 Retry-After 3s；
    /// D9（大输入 400）不带（400 重试无意义，配置给了也不应用）。
    #[test]
    fn d10_empty_response_429_carries_retry_after() {
        let (_t, _m, ra) = empty_response_error_shape(false);
        assert_eq!(ra, Some(3), "偶发空响应（429）必须带默认 Retry-After（D10 修复）");
        let (_t, _m, ra) = empty_response_error_shape(true);
        assert_eq!(ra, None, "大输入空响应（400）不带 Retry-After（重试原请求无意义）");
    }

    /// 双入口同 key：/v1 与 /cc/v1 的 D5（KiroProvider 未配置）读同一配置
    /// （provider_not_configured）——两份复制不再漂移。
    #[tokio::test]
    async fn both_entries_read_same_provider_not_configured_key() {
        let _guard = ERROR_MESSAGES_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        use axum::extract::ConnectInfo;
        let mut table = test_table();
        table.insert(
            "provider_not_configured".to_string(),
            crate::model::config::ErrorMessageOverride {
                status: None,
                r#type: None,
                message: Some("双入口同配置文案".to_string()),
                retry_after_secs: None,
            },
        );
        set_error_messages(table);
        let state = AppState::new("test-key");
        let peer = ConnectInfo(std::net::SocketAddr::from(([127, 0, 0, 1], 12345)));
        let body_json = br#"{"model":"claude-sonnet-5","max_tokens":1024,"messages":[{"role":"user","content":"hi"}]}"#;

        // /v1 入口：raw_body 字节解析 → provider 未配置分支。
        let resp = post_messages(
            State(state.clone()),
            peer.clone(),
            axum::http::HeaderMap::new(),
            bytes::Bytes::from_static(body_json),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        let v: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(resp.into_body(), 64 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            v["error"]["message"],
            "双入口同配置文案",
            "/v1 入口必须读到配置 message"
        );

        // /cc/v1 入口：JsonExtractor 解析 → provider 未配置分支。
        let payload: MessagesRequest = serde_json::from_slice(body_json).unwrap();
        let resp2 = post_messages_cc(
            State(state),
            peer,
            axum::http::HeaderMap::new(),
            JsonExtractor(payload),
        )
        .await;
        assert_eq!(resp2.status(), StatusCode::SERVICE_UNAVAILABLE);
        let v2: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(resp2.into_body(), 64 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            v2["error"]["message"],
            "双入口同配置文案",
            "/cc/v1 入口必须读到同一配置（双入口同 key）"
        );

        // 复位。
        set_error_messages(test_table());
    }

    /// 双入口共享处理（#15 提取）纯函数的行为形态：上限校验边界、转换错误三形态、
    /// 序列化失败渲染。单一实现 = 两入口天然一致，此处钉住渲染契约本身。
    #[test]
    fn shared_render_functions_produce_expected_shapes() {
        let _guard = ERROR_MESSAGES_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // 上限边界：393216（上游实测上限）放行、393217 拒绝。
        assert!(check_max_tokens_limit(393216).is_none(), "上限内必须放行");
        let resp = check_max_tokens_limit(393217).expect("超限必须拒绝");
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = block_on_body(resp);
        assert!(body.contains("\"invalid_request_error\""));
        // 转换错误三形态（默认文案契约）。
        let (s, t, m) =
            render_conversion_error(&ConversionError::UnsupportedModel("gpt-x".to_string()));
        assert_eq!((s.as_u16(), t.as_str()), (400, "invalid_request_error"));
        assert!(m.ends_with("gpt-x"), "模型名必须拼进 message");
        let (s, t, _) = render_conversion_error(&ConversionError::EmptyMessages);
        assert_eq!((s.as_u16(), t.as_str()), (400, "invalid_request_error"));
        let (s, t, m) = render_conversion_error(&ConversionError::UnsupportedToolMapping {
            tool_name: "Bash".to_string(),
            reason: "缺 command".to_string(),
        });
        assert_eq!((s.as_u16(), t.as_str()), (400, "invalid_request_error"));
        assert!(
            m.contains("Bash") && m.contains("缺 command"),
            "工具名与原因必须拼进 message"
        );
        // 序列化失败：500 + internal_error + 动态详情保留（排障必需）。
        let parse_err = serde_json::from_str::<serde_json::Value>("{").unwrap_err();
        let resp = render_serialization_failed(&parse_err);
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let body = block_on_body(resp);
        assert!(
            body.contains("EOF while parsing") || body.contains("unexpected end of JSON input"),
            "动态详情必须保留在 message 里"
        );
        set_error_messages(test_table());
    }

    /// 双入口同行为（#15 提取后对照）：同一输入（max_tokens 超限）在两入口 →
    /// 同一 status + 同一 error.type + 同一 message。带真实 provider 走完
    /// provider 检查/入站闸门/预算段（全不触网），在 max_tokens 校验处汇合。
    #[tokio::test]
    async fn both_entries_reject_oversized_max_tokens_identically() {
        let _guard = ERROR_MESSAGES_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        use axum::extract::ConnectInfo;
        use std::collections::HashMap;
        use std::sync::Arc;
        use crate::kiro::endpoint::KiroEndpoint;
        use crate::kiro::endpoint::ide::{IdeEndpoint, IDE_ENDPOINT_NAME};
        use crate::kiro::provider::KiroProvider;
        use crate::kiro::token_manager::MultiTokenManager;

        let mut config = crate::model::config::Config::default();
        // 关入站闸门：测试不打令牌桶，确定性直通 max_tokens 校验。
        config.inbound_throttle_enabled = false;
        let manager =
            MultiTokenManager::new(config, vec![], None, None, false).expect("构造 manager");
        let mut endpoints: HashMap<String, Arc<dyn KiroEndpoint>> = HashMap::new();
        endpoints.insert(IDE_ENDPOINT_NAME.to_string(), Arc::new(IdeEndpoint::new()));
        let provider = KiroProvider::with_proxy(
            Arc::new(manager),
            None,
            endpoints,
            IDE_ENDPOINT_NAME.to_string(),
        );
        let state = AppState::new("test-key").with_kiro_provider(provider);
        let peer = ConnectInfo(std::net::SocketAddr::from(([127, 0, 0, 1], 12345)));
        let body_json =
            br#"{"model":"claude-sonnet-5","max_tokens":400000,"messages":[{"role":"user","content":"hi"}]}"#;

        let resp = post_messages(
            State(state.clone()),
            peer.clone(),
            axum::http::HeaderMap::new(),
            bytes::Bytes::from_static(body_json),
        )
        .await;
        let payload: MessagesRequest = serde_json::from_slice(body_json).unwrap();
        let resp2 = post_messages_cc(
            State(state),
            peer,
            axum::http::HeaderMap::new(),
            JsonExtractor(payload),
        )
        .await;

        assert_eq!(resp.status(), resp2.status(), "两入口 status 必须一致");
        let v: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(resp.into_body(), 64 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        let v2: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(resp2.into_body(), 64 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            v["error"]["type"], v2["error"]["type"],
            "两入口 error.type 必须一致"
        );
        assert_eq!(
            v["error"]["message"], v2["error"]["message"],
            "两入口 message 必须一致"
        );
        assert_eq!(v["error"]["type"], "invalid_request_error");
        set_error_messages(test_table());
    }

    /// 双入口同行为（#15 提取后对照）：同一输入（空 messages 触发转换错误）在
    /// 两入口 → 同一 status/type/message。与上一条互补：走的是 websearch 判定 +
    /// convert_request 错误映射段（同样全不触网）。
    #[tokio::test]
    async fn both_entries_reject_empty_messages_identically() {
        let _guard = ERROR_MESSAGES_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        use axum::extract::ConnectInfo;
        use std::collections::HashMap;
        use std::sync::Arc;
        use crate::kiro::endpoint::KiroEndpoint;
        use crate::kiro::endpoint::ide::{IdeEndpoint, IDE_ENDPOINT_NAME};
        use crate::kiro::provider::KiroProvider;
        use crate::kiro::token_manager::MultiTokenManager;

        let mut config = crate::model::config::Config::default();
        config.inbound_throttle_enabled = false;
        let manager =
            MultiTokenManager::new(config, vec![], None, None, false).expect("构造 manager");
        let mut endpoints: HashMap<String, Arc<dyn KiroEndpoint>> = HashMap::new();
        endpoints.insert(IDE_ENDPOINT_NAME.to_string(), Arc::new(IdeEndpoint::new()));
        let provider = KiroProvider::with_proxy(
            Arc::new(manager),
            None,
            endpoints,
            IDE_ENDPOINT_NAME.to_string(),
        );
        let state = AppState::new("test-key").with_kiro_provider(provider);
        let peer = ConnectInfo(std::net::SocketAddr::from(([127, 0, 0, 1], 12345)));
        let body_json = br#"{"model":"claude-sonnet-5","max_tokens":1024,"messages":[]}"#;

        let resp = post_messages(
            State(state.clone()),
            peer.clone(),
            axum::http::HeaderMap::new(),
            bytes::Bytes::from_static(body_json),
        )
        .await;
        let payload: MessagesRequest = serde_json::from_slice(body_json).unwrap();
        let resp2 = post_messages_cc(
            State(state),
            peer,
            axum::http::HeaderMap::new(),
            JsonExtractor(payload),
        )
        .await;

        assert_eq!(resp.status(), resp2.status(), "两入口 status 必须一致");
        let v: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(resp.into_body(), 64 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        let v2: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(resp2.into_body(), 64 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            v["error"]["type"], v2["error"]["type"],
            "两入口 error.type 必须一致"
        );
        assert_eq!(
            v["error"]["message"], v2["error"]["message"],
            "两入口 message 必须一致"
        );
        assert_eq!(v["error"]["type"], "invalid_request_error");
        set_error_messages(test_table());
    }
}

#[cfg(test)]
mod websearch_usage_accounting_tests {
    //! WebSearch 用量埋点的真实形态：is_streaming 必须来自请求的 stream 标志。
    use super::*;

    /// 清单 8 源码守卫：回灌埋点必须把 `wants_stream` 传入（旧代码恒写死 false，
    /// 面板按流式/非流式过滤回灌流量时全部显示非流式）。
    #[test]
    fn websearch_loop_usage_carries_real_stream_flag() {
        let src = include_str!("handlers.rs");
        let prod = src.split("\n#[cfg(test)]").next().unwrap_or(src);
        let needle = [
            "emit_websearch_loop_usage(provider, &success, client, ",
            "wants_stream",
            ")",
        ]
        .concat();
        assert!(
            prod.contains(&needle),
            "非流式回灌必须把 wants_stream 传入埋点（旧代码恒 false）"
        );
        let live = [
            "emit_websearch_loop_usage(&p_ok, success, &c_ok, ",
            "true)",
        ]
        .concat();
        assert!(
            prod.contains(&live),
            "直播 SSE 成功臂必须 emit_websearch_loop_usage(..., true)"
        );
    }

    /// 缺口 C 埋点守卫：emit_websearch_loop_usage 必须把末轮映射名写入
    /// `record.upstream_model`（与 credential_id 同源，见 websearch.rs 的
    /// `loop_success_carries_last_round_mapped_model`）。回退即 FAIL：把该行删掉或
    /// 改回恒 None，断言失败。
    #[test]
    fn websearch_loop_usage_carries_upstream_model() {
        let src = include_str!("handlers.rs");
        let prod = src.split("\n#[cfg(test)]").next().unwrap_or(src);
        let emit_fn = prod
            .split("fn emit_websearch_loop_usage")
            .nth(1)
            .expect("emit_websearch_loop_usage 不应被改名")
            .split("\npub(super) fn ")
            .next()
            .unwrap_or(prod);
        let needle = [
            "record.upstream_model = success.mapped_model",
            ".clone()",
        ]
        .concat();
        assert!(
            emit_fn.contains(&needle),
            "回灌埋点必须写入 record.upstream_model（末轮映射名），否则 by_model 聚合失真"
        );
    }

    /// 失败路径必须埋一条非 Success 记录。历史：Err 臂只回 Response、零 emit，
    /// 面板上看不到失败的混合搜索。源码守卫 + 分类函数行为测试。
    #[test]
    fn emit_websearch_loop_error_path_records_failure() {
        let src = include_str!("handlers.rs");
        let prod = src.split("\n#[cfg(test)]").next().unwrap_or(src);
        let dispatch = prod
            .split("async fn dispatch_web_search_loop")
            .nth(1)
            .expect("dispatch_web_search_loop 不应被改名")
            .split("\nfn render_provider_not_configured")
            .next()
            .expect("dispatch 体应在 render_provider_not_configured 之前结束");
        let call = [
            "emit_websearch_loop_error_usage(",
        ]
        .concat();
        assert!(
            dispatch.contains(&call),
            "回灌失败必须埋点（直播 SSE 走 on_err，非流式走 Err 臂）"
        );
        let emit_fn = prod
            .split("fn emit_websearch_loop_error_usage")
            .nth(1)
            .expect("emit_websearch_loop_error_usage 不应被改名")
            .split("\nfn websearch_loop_error_outcome")
            .next()
            .expect("失败埋点之后应是 outcome 分类函数");
        let rec = ["crate::usage::emit", "_record(record)"].concat();
        assert!(
            emit_fn.contains(&rec),
            "失败埋点必须 emit_record，否则用量管道收不到失败混合搜索"
        );
        assert!(
            !emit_fn.contains("RequestOutcome::Success"),
            "失败埋点不得写成 Success"
        );
    }

    #[test]
    fn websearch_loop_error_outcome_matches_provider_failures() {
        use crate::usage::RequestOutcome;
        assert_eq!(
            websearch_loop_error_outcome(StatusCode::TOO_MANY_REQUESTS),
            RequestOutcome::RateLimited
        );
        assert_eq!(
            websearch_loop_error_outcome(StatusCode::BAD_GATEWAY),
            RequestOutcome::ServerError
        );
        assert_eq!(
            websearch_loop_error_outcome(StatusCode::INTERNAL_SERVER_ERROR),
            RequestOutcome::ServerError
        );
        assert_eq!(
            websearch_loop_error_outcome(StatusCode::SERVICE_UNAVAILABLE),
            RequestOutcome::ServerError
        );
        assert_eq!(
            websearch_loop_error_outcome(StatusCode::BAD_REQUEST),
            RequestOutcome::BadRequest
        );
        assert_eq!(
            websearch_loop_error_outcome(StatusCode::FORBIDDEN),
            RequestOutcome::AuthFailed
        );
        assert_eq!(
            websearch_loop_error_outcome(StatusCode::UNAUTHORIZED),
            RequestOutcome::AuthFailed
        );
        assert_eq!(
            websearch_loop_error_outcome(StatusCode::NOT_FOUND),
            RequestOutcome::OtherError
        );
        assert_ne!(
            websearch_loop_error_outcome(StatusCode::BAD_GATEWAY),
            RequestOutcome::Success
        );
    }
}

#[cfg(test)]
mod tier3_hotreload_tests {
    //! TIER3 配置热重载回归：AppState 曾固化的热路径开关改用进程级镜像后，
    //! setter 写入应被对应 getter（handler 热路径读点）立即读到，证明改配置即时生效。
    //!
    //! 注意：镜像是进程级 static，测试间共享同一份。这些测试各自操作**不同的**镜像，
    //! 且末尾恢复默认，避免串扰；不并发断言同一镜像的中间态。
    use super::*;

    #[test]
    fn cc_auto_buffer_static_matches_config_default() {
        // ccAutoBuffer 的默认值散落三处，历史上长期不一致（config 默认 false，而本文件的
        // static 初值与 admin 快照 Default 都是 true）。运行时 static 会被 main 启动播种覆盖，
        // 所以不一致不会立刻出错——但会让单元测试、以及任何绕过 create_router_with_provider
        // 的代码路径读到错的默认值，排障时极易误判。此处把两者钉死。
        //
        // ⚠️ 本测试必须在任何 set_cc_auto_buffer 之前读取，故不与其它 TIER3 测试共用镜像。
        assert_eq!(
            cc_auto_buffer_enabled(),
            crate::model::config::Config::default().cc_auto_buffer,
            "CC_AUTO_BUFFER static 初值与 config 默认不一致：改任一处都必须同步另一处\
             （src/anthropic/handlers.rs 的 static、src/model/config.rs 的 default_cc_auto_buffer）"
        );
    }

    #[test]
    fn test_extract_thinking_mirror_roundtrip() {
        set_extract_thinking(true);
        assert!(extract_thinking_enabled(), "set true 后热路径应读到 true");
        set_extract_thinking(false);
        assert!(
            !extract_thinking_enabled(),
            "set false 后热路径应读到 false"
        );
    }

    #[test]
    fn test_compression_mirror_roundtrip() {
        use crate::model::config::CompressionConfig;
        let mut c = CompressionConfig::default();
        // 翻转 enabled 以可观测地区分（不依赖具体默认值，只验证 setter→getter 传递）
        c.enabled = !c.enabled;
        let flipped = c.enabled;
        set_compression(c);
        assert_eq!(
            current_compression().enabled,
            flipped,
            "set_compression 后热路径应读到新的 compression 快照"
        );
        // 复位默认，避免影响其它测试
        set_compression(CompressionConfig::default());
    }

    /// mock ratio 清洗：>1 / <0 / NaN / ±inf 全部归位到 [0.0, 1.0]，合法值不动。
    #[test]
    fn sanitize_mock_ratio_clamps_and_recovers_non_finite() {
        assert_eq!(sanitize_mock_cache_ratio(1.5), 1.0, ">1 必须 clamp 到 1.0");
        assert_eq!(sanitize_mock_cache_ratio(-0.2), 0.0, "<0 必须 clamp 到 0.0");
        assert_eq!(sanitize_mock_cache_ratio(7.0), 1.0, ">1 必须 clamp 到 1.0");
        assert_eq!(
            sanitize_mock_cache_ratio(f64::NAN),
            0.7,
            "NaN 必须归默认 0.7（clamp 对 NaN 返回 NaN，会污染下游）"
        );
        assert_eq!(
            sanitize_mock_cache_ratio(f64::INFINITY),
            0.7,
            "+inf 必须归默认 0.7"
        );
        assert_eq!(
            sanitize_mock_cache_ratio(f64::NEG_INFINITY),
            0.7,
            "-inf 必须归默认 0.7"
        );
        assert_eq!(sanitize_mock_cache_ratio(0.7), 0.7, "合法值原样");
        assert_eq!(sanitize_mock_cache_ratio(0.0), 0.0, "边界 0 原样");
        assert_eq!(sanitize_mock_cache_ratio(1.0), 1.0, "边界 1 原样");
    }

    /// mock cache 镜像：与 config 默认的一致性 + setter→getter 往返，**合并在一个测试里
    /// 顺序执行**——本镜像只有一个（MOCK_CACHE_*），拆成多个测试会在并行执行下互相
    /// 覆盖状态（违反本模块「各测试操作不同镜像」约定）。任何顺序都绿：set 恒先于断言。
    #[test]
    fn mock_cache_mirror_seed_roundtrip_and_clamp() {
        let cfg_default = crate::model::config::Config::default().mock_cache_read_ratio;
        // ① 与 config 默认一致（main 启动按 config 播种，改任一处默认值都必须同步另一处）
        set_mock_cache_config(false, cfg_default);
        let (enabled, ratio) = mock_cache_config();
        assert!(!enabled, "镜像关闭态与 config 默认一致");
        assert_eq!(ratio, cfg_default, "镜像 ratio 必须与 config 默认一致");

        // ② setter→getter 往返
        set_mock_cache_config(true, 0.5);
        assert_eq!(
            mock_cache_config(),
            (true, 0.5),
            "set true/0.5 后热路径应读到同一组值"
        );
        // ③ 非法值经 setter 清洗后落盘，getter 读到的必须是合法值
        set_mock_cache_config(true, 2.0);
        assert_eq!(mock_cache_config().1, 1.0, ">1 必须被 setter clamp");
        // ④ 复位默认（不影响其它测试）
        set_mock_cache_config(false, cfg_default);
        assert_eq!(mock_cache_config(), (false, cfg_default));
    }
}

#[cfg(test)]
mod truncation_completion_tests {
    //! 「截断即成功」修复回归：验证非流式收尾逻辑依赖的
    //! 解码 → CompletionStatus → HTTP 状态码 链路。
    //!
    //! 非流式 handler 与实盘 provider 强耦合，无法在单测里跑完整请求；
    //! 这里用**真实构造的 event-stream 帧**驱动 handler 内部同一套解码 + 事件分类逻辑，
    //! 断言 in-band error 帧会被识别为失败态，且映射到非 200。
    use super::*;
    use crate::kiro::parser::crc::crc32;

    /// 构造一个带指定 message-type / 头部 / payload 的 event-stream 帧。
    ///
    /// 头部编码：name_len(1) + name + type(7=String) + value_len(2) + value。
    fn build_frame(headers: &[(&str, &str)], payload: &[u8]) -> Vec<u8> {
        let mut header_bytes = Vec::new();
        for (name, value) in headers {
            header_bytes.push(name.len() as u8);
            header_bytes.extend_from_slice(name.as_bytes());
            header_bytes.push(7u8); // String
            header_bytes.extend_from_slice(&(value.len() as u16).to_be_bytes());
            header_bytes.extend_from_slice(value.as_bytes());
        }
        let header_length = header_bytes.len() as u32;
        let total_length = (PRELUDE_SIZE + header_bytes.len() + payload.len() + 4) as u32;

        let mut buf = Vec::new();
        buf.extend_from_slice(&total_length.to_be_bytes());
        buf.extend_from_slice(&header_length.to_be_bytes());
        let prelude_crc = crc32(&buf[..8]);
        buf.extend_from_slice(&prelude_crc.to_be_bytes());
        buf.extend_from_slice(&header_bytes);
        buf.extend_from_slice(payload);
        let msg_crc = crc32(&buf);
        buf.extend_from_slice(&msg_crc.to_be_bytes());
        buf
    }

    // 引入 PRELUDE_SIZE
    use crate::kiro::parser::frame::PRELUDE_SIZE;

    /// 复刻非流式 handler 的解码收尾：drain 全部帧，映射 metadata.stopReason，
    /// 遇 in-band error/非 CL 异常/解码器停止/不完整 EOF 置失败态。
    struct NonStreamTerminal {
        completion: CompletionStatus,
        stop_reason: String,
    }

    fn collect_nonstream_terminal(data: &[u8]) -> NonStreamTerminal {
        let mut decoder = EventStreamDecoder::new();
        let mut completion = CompletionStatus::Ok;
        let mut stop_reason: Option<String> = None;
        let mut saw_upstream_stop = false;
        let mut saw_tool_stop = false;
        let mut has_tool_use = false;
        let mut text_content = String::new();
        let mut decoded_events = Vec::new();
        {
            let mut sink = NonStreamDecodeSink {
                events: &mut decoded_events,
                completion: &mut completion,
            };
            let _ = decode_frames_into(&mut decoder, data, &mut sink);
        }
        for event in decoded_events {
            match event {
                            Event::AssistantResponse(resp) => {
                                text_content.push_str(&resp.content);
                            }
                            Event::ToolUse(tool_use) => {
                                has_tool_use = true;
                                if tool_use.stop {
                                    saw_tool_stop = true;
                                }
                            }
                            Event::Error {
                                error_code,
                                error_message,
                            } => {
                                if completion.is_ok() {
                                    completion = CompletionStatus::UpstreamError {
                                        code: error_code,
                                        message: error_message,
                                    };
                                }
                            }
                            Event::Exception {
                                exception_type,
                                message,
                            } => {
                                if exception_type == "ContentLengthExceededException" {
                                    stop_reason = Some("max_tokens".to_string());
                                } else if completion.is_ok() {
                                    completion = CompletionStatus::UpstreamError {
                                        code: exception_type,
                                        message,
                                    };
                                }
                            }
                            Event::Metadata(meta) => {
                                if let Some(mapped) =
                                    crate::kiro::model::events::map_metadata_stop_reason(
                                        meta.stop_reason.as_deref(),
                                    )
                                {
                                    saw_upstream_stop = true;
                                    if !has_tool_use {
                                        stop_reason = Some(mapped);
                                    }
                                }
                            }
                            _ => {}
            }
        }
        let has_visible_body = !text_content.trim().is_empty();
        if nonstream_clean_eof_without_terminal(
            completion.is_ok(),
            stop_reason.as_deref(),
            saw_upstream_stop,
            saw_tool_stop,
            has_visible_body,
            has_tool_use,
        ) {
            completion = CompletionStatus::Incomplete {
                message: "传输层干净结束，但缺少 stopReason 或 tool_use 终止信号".to_string(),
            };
        }
        NonStreamTerminal {
            completion,
            stop_reason: resolve_nonstream_stop_reason(stop_reason, has_tool_use),
        }
    }

    fn decode_to_completion(data: &[u8]) -> CompletionStatus {
        collect_nonstream_terminal(data).completion
    }

    #[test]
    fn test_inband_error_frame_maps_to_non_200() {
        // 回归 BUG①：in-band error 帧过去落入 `_ => {}` 被忽略、照返 200。
        // 现在应被识别为 UpstreamError，映射非 200。
        let frame = build_frame(
            &[
                (":message-type", "error"),
                (":error-code", "InternalServerException"),
            ],
            b"upstream exploded",
        );
        let completion = decode_to_completion(&frame);

        assert!(!completion.is_ok(), "in-band error 帧应被识别为失败");
        assert_ne!(completion.http_status_u16(), 200, "失败必须返回非 200");
        assert_eq!(completion.http_status_u16(), 502);
        assert_eq!(
            completion.outcome(),
            crate::usage::RequestOutcome::ServerError
        );
    }

    #[test]
    fn test_inband_throttling_error_frame_maps_to_429() {
        let frame = build_frame(
            &[
                (":message-type", "error"),
                (":error-code", "ThrottlingException"),
            ],
            b"slow down",
        );
        let completion = decode_to_completion(&frame);
        assert_eq!(completion.http_status_u16(), 429);
        assert_eq!(
            completion.outcome(),
            crate::usage::RequestOutcome::RateLimited
        );
    }

    #[test]
    fn test_content_length_exception_frame_stays_ok() {
        // 铁律：ContentLengthExceededException 干净收尾，不算失败，仍走 200。
        let frame = build_frame(
            &[
                (":message-type", "exception"),
                (":exception-type", "ContentLengthExceededException"),
            ],
            b"max tokens reached",
        );
        let completion = decode_to_completion(&frame);
        assert!(completion.is_ok(), "CL 异常不应被判为失败");
        assert_eq!(completion.outcome(), crate::usage::RequestOutcome::Success);
    }

    #[test]
    fn test_toolusevent_parse_failure_maps_to_502() {
        // 回归：toolUseEvent 帧解析失败过去被静默丢弃 → 客户端按 end_turn 当成功不重试。
        // 现在应置 DecoderStopped 失败态，映射 502/ServerError，供收尾补发 error 触发重试。
        // 帧 CRC/framing 合法（decoder 不 is_stopped），仅 ToolUseEvent::from_frame 因非法 JSON 返 Err。
        let frame = build_frame(
            &[(":message-type", "event"), (":event-type", "toolUseEvent")],
            b"not valid json",
        );
        let completion = decode_to_completion(&frame);
        assert!(!completion.is_ok(), "toolUseEvent 解析失败应判失败态");
        assert_eq!(completion.http_status_u16(), 502);
        assert_eq!(
            completion.outcome(),
            crate::usage::RequestOutcome::ServerError
        );
    }

    #[test]
    fn test_non_tool_parse_failure_stays_ok() {
        // 零倒退承诺：非 tool 帧解析失败只应告警、不置失败态。
        // 注意 AssistantResponseEvent.content 有 serde(default)，故须用非法 JSON 而非 `{}` 才能触发反序列化失败。
        let frame = build_frame(
            &[
                (":message-type", "event"),
                (":event-type", "assistantResponseEvent"),
            ],
            b"not valid json",
        );
        let completion = decode_to_completion(&frame);
        assert!(completion.is_ok(), "非 tool 帧解析失败只应告警,不置失败态");
        assert_eq!(completion.outcome(), crate::usage::RequestOutcome::Success);
    }

    #[test]
    fn test_from_frame_toolusevent_malformed_errs() {
        // 防呆：锁死「frame 层成功、Event 层失败、event_type 在 move 前可取」三条前提，
        // 防未来 payload 结构变动悄悄使该帧变成 Ok。
        let raw = build_frame(
            &[(":message-type", "event"), (":event-type", "toolUseEvent")],
            b"not valid json",
        );
        let mut d = EventStreamDecoder::new();
        d.feed(&raw).unwrap();
        let frame = d.decode_iter().next().unwrap().unwrap();
        assert_eq!(frame.event_type(), Some("toolUseEvent"));
        assert!(Event::from_frame(frame).is_err());
    }

    #[test]
    fn nonstream_metadata_event_stop_reason_max_tokens_surfaces() {
        let mut data = build_frame(
            &[
                (":message-type", "event"),
                (":event-type", "assistantResponseEvent"),
            ],
            br#"{"content":"partial answer that hit the cap"}"#,
        );
        data.extend(build_frame(
            &[
                (":message-type", "event"),
                (":event-type", "metadataEvent"),
            ],
            br#"{"stopReason":"max_tokens"}"#,
        ));
        let terminal = collect_nonstream_terminal(&data);
        assert!(
            terminal.completion.is_ok(),
            "metadata 给出 max_tokens 是干净收尾，不是失败"
        );
        assert_eq!(
            terminal.stop_reason, "max_tokens",
            "上游 metadata.stopReason=max_tokens 必须露出，不得推断成 end_turn"
        );
    }

    #[test]
    fn nonstream_clean_eof_partial_text_without_stop_is_not_success_end_turn() {
        let data = build_frame(
            &[
                (":message-type", "event"),
                (":event-type", "assistantResponseEvent"),
            ],
            br#"{"content":"this answer was cut off mid-"}"#,
        );
        let terminal = collect_nonstream_terminal(&data);
        assert!(
            !terminal.completion.is_ok(),
            "干净 EOF + 部分正文 + 无终止信号不得记成功"
        );
        assert_ne!(
            terminal.completion.http_status_u16(),
            200,
            "不完整必须非 200"
        );
        match terminal.completion {
            CompletionStatus::Incomplete { .. } => {}
            other => panic!("期望 Incomplete，实际 {other:?}"),
        }
    }

    #[test]
    fn nonstream_tool_stop_true_without_metadata_is_complete_tool_use() {
        let data = build_frame(
            &[(":message-type", "event"), (":event-type", "toolUseEvent")],
            br#"{"name":"Read","toolUseId":"toolu_1","input":"{\"path\":\"/a\"}","stop":true}"#,
        );
        let terminal = collect_nonstream_terminal(&data);
        assert!(
            terminal.completion.is_ok(),
            "tool_use stop=true 即终止，无需 metadata"
        );
        assert_eq!(
            terminal.stop_reason, "tool_use",
            "有 stop:true 的 tool 帧即使没有 metadata 也是完整 tool_use"
        );
    }

    #[test]
    fn nonstream_empty_body_without_metadata_is_not_incomplete() {
        // 完全空留给 near_empty_response（completion 保持 Ok），不是 Incomplete。
        let terminal = collect_nonstream_terminal(&[]);
        assert!(terminal.completion.is_ok());
        assert_eq!(terminal.stop_reason, "end_turn");
    }

    #[test]
    fn resolve_nonstream_stop_reason_matches_stream_get_stop_reason() {
        assert_eq!(
            resolve_nonstream_stop_reason(Some("max_tokens".into()), false),
            "max_tokens"
        );
        assert_eq!(
            resolve_nonstream_stop_reason(None, true),
            "tool_use"
        );
        assert_eq!(
            resolve_nonstream_stop_reason(None, false),
            "end_turn"
        );
        assert_eq!(
            resolve_nonstream_stop_reason(Some("end_turn".into()), true),
            "end_turn",
            "显式 end_turn 优先于 has_tool_use（与流式 get_stop_reason 一致）"
        );
    }

    #[test]
    fn nonstream_content_has_visible_body_ignores_thinking_only_space() {
        use serde_json::json;
        assert!(nonstream_content_has_visible_body(&[json!({
            "type": "text",
            "text": "hello"
        })]));
        assert!(!nonstream_content_has_visible_body(&[json!({
            "type": "thinking",
            "thinking": "x"
        })]));
        assert!(
            !nonstream_content_has_visible_body(&[json!({"type": "text", "text": " "})]),
            "thinking-only 补的空格不算可见正文"
        );
        assert!(nonstream_content_has_visible_body(&[json!({
            "type": "tool_use",
            "name": "Read"
        })]));
    }

    #[test]
    fn nonstream_must_consume_metadata_event_and_reject_clean_eof() {
        let src = include_str!("handlers.rs");
        let start = src
            .find("async fn handle_non_stream_request(")
            .expect("handle_non_stream_request 必须存在");
        let rest = &src[start..];
        let end = rest
            .find("\nfn override_thinking_from_model_name")
            .expect("下一函数定位不应被改名");
        let prod: String = rest[..end]
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        let meta_arm = format!("{}::{}", "Event", "Metadata");
        let mapper = ["map_metadata", "_stop_reason"].concat();
        let incomplete = format!("{}::{}", "CompletionStatus", "Incomplete");
        assert!(
            prod.contains(&meta_arm),
            "非流式 match 必须消费 Metadata 帧（不能再 _ => 丢掉 stopReason）"
        );
        assert!(
            prod.contains(&mapper),
            "必须复用 map_metadata_stop_reason，禁止第二张映射表"
        );
        assert!(
            prod.contains(&incomplete),
            "干净 EOF 无终止必须落非 Ok 完成态"
        );
    }
}

#[cfg(test)]
mod ported_k2cc_empty_response_event_tests {
    //! 从 k2cc 移植的「空响应 SSE error 事件」测试：上下文过大 → invalid_request_error
    //! 且带 /compact 提示；偶发 → overloaded_error 可重试。
    use super::*;

    #[test]
    fn empty_response_error_event_oversized_hints_compact() {
        let ev = empty_response_error_event(true);
        assert_eq!(ev.event, "error");
        assert_eq!(ev.data["error"]["type"], "invalid_request_error");
        let msg = ev.data["error"]["message"].as_str().unwrap();
        assert!(msg.contains("/compact"), "提示文案必须含 /compact: {msg}");
    }

    #[test]
    fn empty_response_error_event_transient_is_retryable() {
        let ev = empty_response_error_event(false);
        assert_eq!(ev.event, "error");
        assert_eq!(ev.data["error"]["type"], "overloaded_error");
        assert!(ev.data["error"]["message"].as_str().unwrap().contains("重试"));
    }
}

#[cfg(test)]
mod stream_usage_outcome_tests {
    //! P1-1 / P1-2：空响应记 EmptyResponse；Drop 未 emit 补 Interrupted。
    use super::*;
    use crate::usage::RequestOutcome;
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::sync::atomic::AtomicU32;

    fn empty_ctx() -> StreamContext {
        StreamContext::new_with_thinking("claude-sonnet-5", 100, false, HashMap::new())
    }

    fn test_provider() -> Arc<crate::kiro::provider::KiroProvider> {
        let cfg = crate::model::config::Config::default();
        let default_ep = cfg.default_endpoint.clone();
        let tm = Arc::new(
            crate::kiro::token_manager::MultiTokenManager::new(cfg, vec![], None, None, false)
                .expect("test token manager"),
        );
        Arc::new(crate::kiro::provider::KiroProvider::with_proxy(
            tm,
            None,
            crate::kiro::endpoint::registry(),
            default_ep,
        ))
    }

    fn test_meta() -> crate::kiro::provider::CallMeta {
        crate::kiro::provider::CallMeta {
            credential_id: 1,
            model: Some("claude-sonnet-5".into()),
            mapped_model: None,
            session_id: None,
            is_streaming: true,
            retries: 0,
            latency_ms: 7,
            started_at: std::time::Instant::now(),
            inflight: crate::kiro::scheduling::InflightGuard::acquire(Arc::new(AtomicU32::new(0))),
        }
    }

    #[test]
    fn apply_usage_outcome_empty_stream_is_empty_response_not_success() {
        let mut rec = crate::usage::RequestRecord::new("req-empty", "claude-sonnet-5");
        apply_usage_outcome(
            &mut rec,
            true,
            RequestOutcome::Success,
            None,
            true,
            false,
            false,
        );
        assert_eq!(rec.outcome, RequestOutcome::EmptyResponse);
        assert!(!rec.outcome.is_success());
        let msg = rec.error_message.as_deref().unwrap_or("");
        assert!(msg.contains("重试"), "偶发空响应文案: {msg}");
    }

    #[test]
    fn apply_usage_outcome_empty_nonstream_oversized_uses_compact_text() {
        let mut rec = crate::usage::RequestRecord::new("req-oversize", "claude-sonnet-5");
        apply_usage_outcome(
            &mut rec,
            true,
            RequestOutcome::Success,
            None,
            true,
            true,
            false,
        );
        assert_eq!(rec.outcome, RequestOutcome::EmptyResponse);
        let msg = rec.error_message.as_deref().unwrap_or("");
        assert!(msg.contains("/compact"), "大输入空响应文案: {msg}");
    }

    #[test]
    fn apply_usage_outcome_disconnected_wins_over_empty() {
        let mut rec = crate::usage::RequestRecord::new("req-disc", "claude-sonnet-5");
        apply_usage_outcome(
            &mut rec,
            true,
            RequestOutcome::Success,
            None,
            true,
            false,
            true,
        );
        assert_eq!(rec.outcome, RequestOutcome::Interrupted);
        assert_eq!(
            rec.error_message.as_deref(),
            Some(CLIENT_DISCONNECTED_MESSAGE)
        );
    }

    #[test]
    fn apply_usage_outcome_keeps_success_when_not_empty() {
        let mut rec = crate::usage::RequestRecord::new("req-ok", "claude-sonnet-5");
        apply_usage_outcome(
            &mut rec,
            true,
            RequestOutcome::Success,
            None,
            false,
            false,
            false,
        );
        assert_eq!(rec.outcome, RequestOutcome::Success);
        assert!(rec.error_message.is_none());
    }

    #[test]
    fn empty_stream_emit_records_empty_response_not_success() {
        let ctx = empty_ctx();
        assert!(ctx.completion().is_ok());
        assert!(ctx.is_empty_response());
        let provider = test_provider();
        let meta = test_meta();
        let client = ClientInfo::default();
        let ((), recs) = crate::usage::pipeline::with_captured_records(|| {
            emit_stream_usage(&provider, &ctx, &meta, &client, false);
        });
        assert_eq!(recs.len(), 1, "应落一条 usage: {recs:?}");
        assert_eq!(recs[0].outcome, RequestOutcome::EmptyResponse);
        assert!(!recs[0].outcome.is_success());
        assert!(recs[0].error_message.is_some());
    }

    #[test]
    fn usage_emit_guard_drop_without_emit_records_interrupted() {
        let mut ctx = empty_ctx();
        ctx.credits_used = Some(1.25);
        let provider = test_provider();
        let meta = test_meta();
        let client = ClientInfo::default();
        let ((), recs) = crate::usage::pipeline::with_captured_records(|| {
            let guard = UsageEmitGuard::new(ctx, meta, client, provider, emit_stream_usage);
            drop(guard);
        });
        assert_eq!(recs.len(), 1, "Drop 未 emit 应补一条: {:?}", recs.len());
        assert_eq!(recs[0].outcome, RequestOutcome::Interrupted);
        assert_eq!(
            recs[0].error_message.as_deref(),
            Some(CLIENT_DISCONNECTED_MESSAGE)
        );
        assert_eq!(recs[0].credits_used, Some(1.25));
    }

    #[test]
    fn usage_emit_guard_drop_after_emit_does_not_double_record() {
        let ctx = empty_ctx();
        let provider = test_provider();
        let meta = test_meta();
        let client = ClientInfo::default();
        let ((), recs) = crate::usage::pipeline::with_captured_records(|| {
            let mut guard = UsageEmitGuard::new(ctx, meta, client, provider, emit_stream_usage);
            guard.emit();
            drop(guard);
        });
        assert_eq!(recs.len(), 1, "emit 后再 Drop 不得第二条: {}", recs.len());
        assert_eq!(
            recs[0].outcome,
            RequestOutcome::EmptyResponse,
            "自然 emit 的空流是 EmptyResponse，不是 Interrupted"
        );
    }
}

#[cfg(test)]
mod adaptive_compress_loop_tests {
    //! 自适应二次压缩循环：压一次仍超限 → 迭代降级直至进阈值；以及 fail-safe 行为。
    //!
    //! ⚠️ 本机无法 `cargo build`（8GB 内存 + 编译不过的历史问题），只能静态自检：
    //! 断言围绕「最终序列化字节数必须小于阈值」与「不再调用即返回当前结果」，
    //! 逻辑自洽但未在真实编译器上验证过类型/借用。
    use super::*;
    use crate::kiro::model::requests::conversation::*;
    use crate::kiro::model::requests::tool::ToolResult;
    use crate::model::config::CompressionConfig;

    fn config(trigger_bytes: usize, tool_result_max_chars: usize) -> CompressionConfig {
        CompressionConfig {
            enabled: true,
            trigger_bytes,
            whitespace_compression: false,
            tool_result_max_chars,
            tool_result_head_lines: 3,
            tool_result_tail_lines: 3,
        }
    }

    fn run(
        conversation_state: ConversationState,
        cfg: &CompressionConfig,
    ) -> (String, ConversationState) {
        let kiro_request = KiroRequest {
            conversation_state,
            profile_arn: None,
            additional_model_request_fields: None,
        };
        let before = serde_json::to_string(&kiro_request).unwrap();
        assert!(before.len() > cfg.trigger_bytes, "前置：初始已超阈值");
        // 造一个可变的 KiroRequest 供循环使用
        let mut kiro_request = kiro_request;
        let mut body = before;
        adaptive_compress_loop(&mut kiro_request, cfg, &mut body, None).unwrap();
        (body, kiro_request.conversation_state)
    }

    #[test]
    fn converge_to_below_threshold_via_tool_result() {
        // 单一超大 tool_result：压一次（8000）仍超限，但压到 4500 就能进阈值
        let long_text = (0..400).map(|i| format!("row {}", i)).collect::<Vec<_>>().join("\n");
        let state = ConversationState::new("conv")
            .with_current_message(CurrentMessage::new(
                UserInputMessage::new("msg", "claude-sonnet-4.5").with_context(
                    UserInputMessageContext::new()
                        .with_tool_results(vec![ToolResult::success("t1", &long_text)]),
                ),
            ))
            .with_history(Vec::new());

        let cfg = config(1800, 8000);
        let (body, _state) = run(state, &cfg);
        assert!(body.len() < cfg.trigger_bytes, "最终字节 {} 仍超阈值 {}", body.len(), cfg.trigger_bytes);
    }

    #[test]
    fn converge_to_below_threshold_via_history_drop() {
        // 多轮小历史：没有 tool_result 可压，删掉若干最老轮次后进阈值
        let mut history = Vec::new();
        for i in 0..80 {
            history.push(Message::User(HistoryUserMessage::new(
                format!("long user message number {}", i),
                "claude-sonnet-4.5",
            )));
            history.push(Message::Assistant(HistoryAssistantMessage::new(
                format!("assistant answer {}", i),
            )));
        }
        let state = ConversationState::new("conv")
            .with_current_message(CurrentMessage::new(
                UserInputMessage::new("hi", "claude-sonnet-4.5"),
            ))
            .with_history(history);

        let cfg = config(2000, 0); // 关掉 tool_result 层，逼循环走历史删除
        let (body, state) = run(state, &cfg);
        assert!(body.len() < cfg.trigger_bytes, "最终字节 {} 仍超阈值 {}", body.len(), cfg.trigger_bytes);
        assert!(state.history.len() >= 4, "保留对不能低于 2 对，实际 {}", state.history.len());
    }

    #[test]
    fn single_message_huge_triggers_message_truncation() {
        // 单条 user content 本身就远超阈值：删历史救不回来 → 走正文截断层
        let huge = "x".repeat(50_000);
        let state = ConversationState::new("conv")
            .with_current_message(CurrentMessage::new(
                UserInputMessage::new(huge, "claude-sonnet-4.5"),
            ))
            .with_history(Vec::new());

        // 阈值取 20000：正文截断的下限是 ADAPTIVE_MIN_MESSAGE_CONTENT_MAX_CHARS=8192
        // （+ 省略标记约 40 字节），阈值若定得比这个地板还小，循环永远压不进去
        // ——那是「压到底仍超限，照发交上游」的预期路径，不该用它断言收敛。
        let cfg = config(20_000, 0);
        let (body, state) = run(state, &cfg);
        assert!(body.len() < cfg.trigger_bytes, "最终字节 {} 仍超阈值 {}", body.len(), cfg.trigger_bytes);
        let final_chars = state.current_message.user_input_message.content.chars().count();
        assert!(final_chars < 50_000, "正文应被截短，实际 {final_chars}");
    }

    #[test]
    fn floor_reached_still_oversized_gives_up_without_hanging() {
        // 压到地板（8192 字符）仍超阈值：必须在 32 轮内退出并照发，不挂死、不 panic
        let huge = "x".repeat(50_000);
        let state = ConversationState::new("conv")
            .with_current_message(CurrentMessage::new(
                UserInputMessage::new(huge, "claude-sonnet-4.5"),
            ))
            .with_history(Vec::new());

        let cfg = config(4000, 0); // 低于 8192 地板，永远压不进
        let (body, _state) = run(state, &cfg);
        // 仍超阈值是预期结果（交上游判死），关键是函数返回了
        assert!(body.len() > cfg.trigger_bytes);
    }

    #[test]
    fn history_images_removed_when_no_tool_result() {
        // 有历史图片、无 tool_result：应触发图片降级而不是删历史
        let img = KiroImage::from_base64("png", "a".repeat(8000));
        // HistoryUserMessage 没有 with_images，图片挂在内层 UserMessage 上
        let mut hu = HistoryUserMessage::new("u", "claude-sonnet-4.5");
        hu.user_input_message = hu.user_input_message.with_images(vec![img]);
        let history = vec![
            Message::User(hu),
            Message::Assistant(HistoryAssistantMessage::new("a")),
        ];
        let state = ConversationState::new("conv")
            .with_current_message(CurrentMessage::new(
                UserInputMessage::new("hi", "claude-sonnet-4.5"),
            ))
            .with_history(history);

        let cfg = config(3000, 0);
        let (body, state) = run(state, &cfg);
        assert!(body.len() < cfg.trigger_bytes, "最终字节 {} 仍超阈值 {}", body.len(), cfg.trigger_bytes);
        // 历史消息应保留（删的是图片不是轮次）
        assert_eq!(state.history.len(), 2);
        if let Message::User(u) = &state.history[0] {
            assert!(u.user_input_message.images.is_empty(), "历史图片应被清除");
        }
    }

    #[test]
    fn disabled_config_returns_original_body() {
        // compression.enabled = false 时循环必须原样返回（不触发任何压缩）。
        // 守卫在 `adaptive_compress_loop` 内部（对齐参考仓 ref-mjy/handlers.rs:251），
        // 因此即使 state 里存在可压缩的大 tool_result，也不得改动。
        let long_text = (0..300).map(|i| format!("row {}", i)).collect::<Vec<_>>().join("\n");
        let state = ConversationState::new("conv")
            .with_current_message(CurrentMessage::new(
                UserInputMessage::new("msg", "claude-sonnet-4.5").with_context(
                    UserInputMessageContext::new()
                        .with_tool_results(vec![ToolResult::success("t1", &long_text)]),
                ),
            ))
            .with_history(Vec::new());

        let cfg = CompressionConfig {
            enabled: false,
            trigger_bytes: 1,
            ..Default::default()
        };
        let kiro_request = KiroRequest {
            conversation_state: state,
            profile_arn: None,
            additional_model_request_fields: None,
        };
        let before = serde_json::to_string(&kiro_request).unwrap();
        let mut kiro_request = kiro_request;
        let mut body = before.clone();
        adaptive_compress_loop(&mut kiro_request, &cfg, &mut body, None).unwrap();
        assert_eq!(body, before, "禁用时不应改动请求体");
    }

    #[test]
    fn zero_trigger_returns_original_body() {
        // trigger_bytes = 0 表示不限制，循环必须原样返回
        let state = ConversationState::new("conv")
            .with_current_message(CurrentMessage::new(
                UserInputMessage::new("hello", "claude-sonnet-4.5"),
            ))
            .with_history(Vec::new());

        let cfg = config(0, 0);
        let kiro_request = KiroRequest {
            conversation_state: state,
            profile_arn: None,
            additional_model_request_fields: None,
        };
        let before = serde_json::to_string(&kiro_request).unwrap();
        let mut kiro_request = kiro_request;
        let mut body = before.clone();
        adaptive_compress_loop(&mut kiro_request, &cfg, &mut body, None).unwrap();
        assert_eq!(body, before);
    }

    #[test]
    fn max_iters_are_bounded() {
        // 即使永远压不进阈值，循环也必须在 32 轮内退出（不挂死）
        let state = ConversationState::new("conv")
            .with_current_message(CurrentMessage::new(
                UserInputMessage::new("hello", "claude-sonnet-4.5"),
            ))
            .with_history(Vec::new());

        let cfg = config(1, 0); // 极小阈值，永远达不到
        let kiro_request = KiroRequest {
            conversation_state: state,
            profile_arn: None,
            additional_model_request_fields: None,
        };
        let before = serde_json::to_string(&kiro_request).unwrap();
        let mut kiro_request = kiro_request;
        let mut body = before;
        adaptive_compress_loop(&mut kiro_request, &cfg, &mut body, None).unwrap();
        assert!(!body.is_empty());
    }
}

// ==================== B7 启动播种自检：源码守卫 ====================
#[cfg(test)]
mod mirror_wiring_guard {
    // 守卫纪律（CLAUDE.md 教训 #9）：本模块注释里不得出现带引号括号的完整调用字面量
    // （needle 是拼接出来的，注释命中会静默变绿）。

    /// 每个镜像 setter 内部必须登记接线标记：遍历名字表，断言生产区存在对应调用。
    /// 删掉任一 setter 的登记行 / 改名不同步 → 红。与 token_manager 的 reload 守卫同范式。
    #[test]
    fn mirror_wiring_source_guard() {
        let full = include_str!("handlers.rs");
        let prod = full.split("\n#[cfg(test)]").next().unwrap_or(full);
        for name in super::MIRROR_WIRED_NAMES {
            let needle = format!("mark_mirror_wired(\"{}\"{}", name, ")");
            assert!(
                prod.contains(&needle),
                "setter 登记缺失: {needle} 不存在于生产代码（MIRROR_WIRED_NAMES 与 setter 必须同步）"
            );
        }
        // 名字表本身不得重复（重复会让位图错位）
        let mut sorted: Vec<&str> = super::MIRROR_WIRED_NAMES.to_vec();
        sorted.sort_unstable();
        let deduped = sorted.clone();
        sorted.dedup();
        assert_eq!(sorted.len(), deduped.len(), "MIRROR_WIRED_NAMES 含重复名字");
    }

    /// 位图框架自检：全部置位后 unwired 为空；mark 未登记名字必须 panic。
    #[test]
    fn bitmap_unwired_and_unknown_name() {
        let total = super::MIRROR_WIRED_NAMES.len();
        // 全部名字逐一 mark（幂等）
        for name in super::MIRROR_WIRED_NAMES {
            super::mark_mirror_wired(name);
        }
        assert!(super::unwired_mirrors().is_empty());
        // 未登记名字 → panic
        let r = std::panic::catch_unwind(|| super::mark_mirror_wired("no_such_mirror"));
        assert!(r.is_err(), "未登记名字必须 panic");
        assert_eq!(super::unwired_mirrors().len(), 0);
        // 位图没有溢出
        assert!(total <= 64);
    }
}
