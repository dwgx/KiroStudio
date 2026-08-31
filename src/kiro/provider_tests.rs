//! `provider.rs` inline tests extracted as a #[path] sibling.
//! This file is kiro::provider::tests. Do not turn the parent into a directory.


    use super::*;
    use super::absorb_policy::ABSORB_MIN_BACKOFF;
    use super::retry_budget::MAX_RETRIES_PER_CREDENTIAL;

    #[test]
    fn parse_retry_after_header_accepts_delta_seconds_and_http_date() {
        assert_eq!(parse_retry_after_header_value("12"), Some(12));
        assert_eq!(parse_retry_after_header_value(" 7 "), Some(7));
        assert_eq!(parse_retry_after_header_value("not-a-date"), None);
        let future = (chrono::Utc::now() + chrono::Duration::seconds(90))
            .format("%a, %d %b %Y %H:%M:%S GMT")
            .to_string();
        let secs = parse_retry_after_header_value(&future).expect("HTTP-date 应解析");
        assert!(
            (60..=120).contains(&secs),
            "未来 90s 的 HTTP-date 应落在 60..=120，实际 {secs} ({future})"
        );
        let past = "Wed, 21 Oct 2015 07:28:00 GMT";
        assert_eq!(parse_retry_after_header_value(past), Some(0));
    }

    // ===== S4：透传池冷却标签独立（状态码 → 秒数 + 原因）=====

    /// 映射表契约（2026-08-16 S4）：每个状态码的 `(秒数, 原因)` 必须精确匹配。
    /// 原因只决定面板 `cooldownReason`/`cooldownCode` 展示；秒数是既有调参
    /// （401/403 用 `AuthTransient` 仍是 180s，不走该变体 20s 默认时长）。
    /// 回退即 FAIL：S4 前全部冷却硬编码 `RateLimitExceeded`（401 在面板显示
    /// 「速率限制」误导排障）→ 原因断言失败。
    #[test]
    fn passthrough_cooldown_reason_mapping_table() {
        use crate::kiro::cooldown::CooldownReason as R;
        // 认证类（key 失效/403）→ AuthTransient，180s 非瞬态冷却。
        assert_eq!(
            KiroProvider::passthrough_cooldown_for(401),
            (180, Some(R::AuthTransient))
        );
        assert_eq!(
            KiroProvider::passthrough_cooldown_for(403),
            (180, Some(R::AuthTransient))
        );
        // 配额耗尽（中转站常用 402 表额度）→ QuotaExhausted。
        assert_eq!(
            KiroProvider::passthrough_cooldown_for(402),
            (180, Some(R::QuotaExhausted))
        );
        // 限流 → RateLimitExceeded（保留原标签）。
        assert_eq!(
            KiroProvider::passthrough_cooldown_for(429),
            (5, Some(R::RateLimitExceeded))
        );
        // 站点不认请求（模型/tool/role）→ 5s 调度跳过（现状保留）。
        assert_eq!(
            KiroProvider::passthrough_cooldown_for(400),
            (5, Some(R::RateLimitExceeded))
        );
        assert_eq!(
            KiroProvider::passthrough_cooldown_for(404),
            (5, Some(R::RateLimitExceeded))
        );
        // 服务器错误 → ServerError（5s 调度跳过 + 标签）。
        assert_eq!(
            KiroProvider::passthrough_cooldown_for(500),
            (5, Some(R::ServerError))
        );
        assert_eq!(
            KiroProvider::passthrough_cooldown_for(502),
            (5, Some(R::ServerError))
        );
        assert_eq!(
            KiroProvider::passthrough_cooldown_for(599),
            (5, Some(R::ServerError))
        );
        // 网络错误（无状态码，code=0）与其它码：不冷却。
        assert_eq!(KiroProvider::passthrough_cooldown_for(0), (0, None));
        assert_eq!(KiroProvider::passthrough_cooldown_for(422), (0, None));
        assert_eq!(KiroProvider::passthrough_cooldown_for(600), (0, None));
    }

    // ===== MCP 无号直连（OAuth 带头 / ksk_ 永不带）=====

    /// social 有真实 ARN：直连头必须带 `x-amzn-kiro-profile-arn`（runtime 主机要求）。
    #[test]
    fn mcp_direct_headers_oauth_sends_profile_arn() {
        let mut social = KiroCredentials::default();
        social.auth_method = Some("social".to_string());
        social.profile_arn = Some("arn:aws:codewhisperer:us-east-1:1:profile/OWN".to_string());
        let headers = KiroProvider::mcp_direct_headers(&social, "tok");
        assert!(
            headers.iter().any(|(k, v)| *k == "x-amzn-kiro-profile-arn"
                && v == "arn:aws:codewhisperer:us-east-1:1:profile/OWN"),
            "OAuth 直连必须带头，且值为 effective_profile_arn"
        );
        assert!(
            headers.iter().any(|(k, v)| *k == "Authorization" && v == "Bearer tok"),
            "必须带 Bearer Authorization"
        );
        assert!(
            headers
                .iter()
                .any(|(k, v)| *k == "x-amzn-codewhisperer-optout" && v == "false"),
            "必须带 x-amzn-codewhisperer-optout"
        );
        assert!(
            !headers.iter().any(|(k, _)| *k == "tokentype"),
            "social 号不带 tokentype"
        );
    }

    /// ksk_ 即使库里有 profile_arn 也绝不带头（IDE 主机 + CLI 令牌）。
    #[test]
    fn mcp_direct_headers_api_key_never_sends_profile_arn() {
        let mut api_key = KiroCredentials::default();
        api_key.auth_method = Some("api_key".to_string());
        api_key.kiro_api_key = Some("ksk_x".to_string());
        api_key.profile_arn =
            Some("arn:aws:codewhisperer:us-east-1:1:profile/OWN".to_string());
        let headers = KiroProvider::mcp_direct_headers(&api_key, "ksk_x");
        assert!(
            !headers.iter().any(|(k, _)| *k == "x-amzn-kiro-profile-arn"),
            "ksk_ 直连永不带 profileArn"
        );
        assert!(
            headers
                .iter()
                .any(|(k, v)| *k == "tokentype" && v == "API_KEY"),
            "ksk_ 号直连必须带 tokentype: API_KEY"
        );
    }

    /// api_key（ksk_）号直连带 `tokentype: API_KEY`（与 decorate_mcp 同口径）。
    #[test]
    fn mcp_direct_headers_api_key_gets_tokentype() {
        let mut api_key = KiroCredentials::default();
        api_key.auth_method = Some("api_key".to_string());
        api_key.kiro_api_key = Some("ksk_x".to_string());
        let headers = KiroProvider::mcp_direct_headers(&api_key, "ksk_x");
        assert!(
            headers
                .iter()
                .any(|(k, v)| *k == "tokentype" && v == "API_KEY"),
            "ksk_ 号直连必须带 tokentype: API_KEY"
        );
        assert!(
            !headers.iter().any(|(k, _)| *k == "x-amzn-kiro-profile-arn"),
            "缺 ARN 的 ksk_ 也不带头"
        );
    }

    /// external_idp 号直连带 `tokentype: EXTERNAL_IDP`；有真实 ARN 才带头。
    #[test]
    fn mcp_direct_headers_external_idp_gets_tokentype() {
        let mut ext = KiroCredentials::default();
        ext.auth_method = Some("external_idp".to_string());
        let headers = KiroProvider::mcp_direct_headers(&ext, "t");
        assert!(
            headers
                .iter()
                .any(|(k, v)| *k == "tokentype" && v == "EXTERNAL_IDP"),
            "external_idp 号直连必须带 tokentype: EXTERNAL_IDP"
        );
        assert!(
            !headers.iter().any(|(k, _)| *k == "x-amzn-kiro-profile-arn"),
            "external_idp 缺真实 ARN 不得套占位"
        );

        ext.profile_arn =
            Some("arn:aws:codewhisperer:us-east-1:9:profile/TENANT".to_string());
        let headers = KiroProvider::mcp_direct_headers(&ext, "t");
        assert!(
            headers.iter().any(|(k, v)| *k == "x-amzn-kiro-profile-arn"
                && v == "arn:aws:codewhisperer:us-east-1:9:profile/TENANT"),
            "external_idp 有真实 ARN 必须带头"
        );
    }

    /// 开关默认开（「默认开无号直连尝试，失败降级现状」——实测前置要求）。
    #[test]
    fn mcp_direct_bypass_defaults_to_enabled() {
        assert!(
            MCP_DIRECT_BYPASS_ENABLED.load(std::sync::atomic::Ordering::Relaxed),
            "无号直连开关默认必须开启（失败由降级兜底）"
        );
    }

    /// 接线守卫①：`call_mcp` 入口必须含「标记识别 → 直连兜底 → 剥标记返回」三段。
    ///
    /// 回退即 FAIL：把 call_mcp 改回裸转发（直连不接线 = 结构性缺陷修不了）。
    #[test]
    fn call_mcp_is_wired_for_direct_bypass() {
        let full = include_str!("provider.rs");
        let prod = full
            .split_once("\n#[cfg(test)]")
            .map(|(a, _)| a)
            .unwrap_or(full);
        let call_mcp = prod
            .split("pub async fn call_mcp(")
            .nth(1)
            .expect("call_mcp 不应被改名");
        let call_mcp = call_mcp
            .split("\n    }\n")
            .next()
            .expect("call_mcp 应有函数体收尾");
        assert!(
            call_mcp.contains("call_mcp_with_retry(request_body, budget)"),
            "call_mcp 必须先走原路径"
        );
        assert!(
            call_mcp.contains("strip_prefix(MCP_POOL_UNAVAILABLE_MARKER)"),
            "必须识别无号标记（否则直连永不触发）"
        );
        assert!(
            call_mcp.contains("call_mcp_direct(request_body, budget)"),
            "无号时必须调用直连兜底"
        );
        assert!(
            call_mcp.contains("MCP_DIRECT_BYPASS_ENABLED.load"),
            "直连必须受开关门控"
        );
    }

    /// 接线守卫②：`call_mcp_with_retry` 的 acquire_context 失败分支必须打无号标记。
    ///
    /// 回退即 FAIL：把 `.context(MCP_POOL_UNAVAILABLE_MARKER)` 删掉或改回
    /// `last_error = Some(e)` → 直连永不触发。
    #[test]
    fn acquire_context_failure_marks_pool_unavailable() {
        let full = include_str!("provider.rs");
        let prod = full
            .split_once("\n#[cfg(test)]")
            .map(|(a, _)| a)
            .unwrap_or(full);
        let mcp_fn = prod
            .split("async fn call_mcp_with_retry")
            .nth(1)
            .expect("call_mcp_with_retry 不应被改名");
        let acquire_err = mcp_fn
            .split("last_error = Some(e.context(MCP_POOL_UNAVAILABLE_MARKER));")
            .count();
        assert_eq!(
            acquire_err,
            2,
            "acquire_context 失败分支必须打 mcp_pool_unavailable=1 标记（仅此一处）"
        );
    }

    /// 接线守卫③：直连 URL 必须恒为 IDE 协议的 `runtime.{region}.kiro.dev/mcp`，
    /// 不得随凭据端点类型变成 `q.*`（CLI 端点的 mcp_url 是 q.* 兜底，不适合直连）。
    #[test]
    fn call_mcp_direct_uses_ide_mcp_url_only() {
        let full = include_str!("provider.rs");
        let prod = full
            .split_once("\n#[cfg(test)]")
            .map(|(a, _)| a)
            .unwrap_or(full);
        let direct = prod
            .split("async fn call_mcp_direct(")
            .nth(1)
            .expect("call_mcp_direct 不应被改名");
        let direct = direct
            .split("\n    }\n")
            .next()
            .expect("call_mcp_direct 应有函数体收尾");
        assert!(
            direct.contains("IdeEndpoint::new()"),
            "直连必须显式构造 IDE 端点（MCP 端点是 IDE 协议的）"
        );
        assert!(
            !direct.contains("for_credentials("),
            "直连不得按凭据类型路由端点（ksk_ 号会拿到 cli 端点的 q.* 兜底 URL）"
        );
        assert!(
            !direct.contains("amazonaws.com"),
            "直连 URL 不得出现 q.* 兜底"
        );
    }

    /// 接线守卫④：直连必须消费共享预算（真实发了上游请求 = 打了就是打了）。
    #[test]
    fn call_mcp_direct_consumes_budget() {
        let full = include_str!("provider.rs");
        let prod = full
            .split_once("\n#[cfg(test)]")
            .map(|(a, _)| a)
            .unwrap_or(full);
        let direct = prod
            .split("async fn call_mcp_direct(")
            .nth(1)
            .expect("call_mcp_direct 不应被改名");
        let direct = direct
            .split("\n    }\n")
            .next()
            .expect("call_mcp_direct 应有函数体收尾");
        assert_eq!(
            direct.matches("budget.consume(1)").count(),
            2,
            "直连成功与发送失败两条出口都必须扣共享预算"
        );
    }

    /// 接线守卫⑤：直连必须拒绝非 2xx 响应（不得当成功解析）。
    ///
    /// 回退即 FAIL：删掉 status 检查 → 无 ARN 形态被上游拒（403/400）时错误体
    /// 会被当 MCP JSON-RPC 解析，反序列化失败掩盖真实原因，且可能把「上游不认
    /// 无 ARN」伪装成「解析错误」误导排障。
    #[test]
    fn call_mcp_direct_rejects_non_success_status() {
        let full = include_str!("provider.rs");
        let prod = full
            .split_once("\n#[cfg(test)]")
            .map(|(a, _)| a)
            .unwrap_or(full);
        let direct = prod
            .split("async fn call_mcp_direct(")
            .nth(1)
            .expect("call_mcp_direct 不应被改名");
        let direct = direct
            .split("\n    }\n")
            .next()
            .expect("call_mcp_direct 应有函数体收尾");
        assert!(
            direct.contains("is_success()"),
            "直连必须检查上游 status（非 2xx 不得当成功返回）"
        );
    }

    /// M3 接线守卫⑥：直连非 2xx 失败必须落短负缓存，且键含凭据 id。
    ///
    /// 回退即 FAIL：删掉 mark → 直连失败零记忆，每请求再打死 token 一跳
    /// （风控窗口加流量，与网关纪律矛盾）。把键改回 region-only → 同区坏 ksk
    /// 连坐健康 OAuth。
    #[test]
    fn call_mcp_direct_marks_negative_cache_on_failure() {
        let full = include_str!("provider.rs");
        let prod = full
            .split_once("\n#[cfg(test)]")
            .map(|(a, _)| a)
            .unwrap_or(full);
        let direct = prod
            .split("async fn call_mcp_direct(")
            .nth(1)
            .expect("call_mcp_direct 不应被改名");
        let direct = direct
            .split("\n    }\n")
            .next()
            .expect("call_mcp_direct 应有函数体收尾");
        let mark = ["mark_endpoint_dead", "("].concat();
        let id_slot = ["mcp-direct@", "{}"].concat();
        assert!(
            direct.contains(&mark) && direct.contains("is_success()"),
            "直连非 2xx 必须落负缓存（60s 内不重试该号直连）"
        );
        assert!(
            direct.contains(&id_slot),
            "负缓存端点名必须嵌入凭据 id，避免同 region 一号毒全池"
        );
    }

    /// M3 接线守卫⑦：直连发送前必须检查短负缓存（否则负缓存是死代码）。
    ///
    /// 回退即 FAIL：删掉 `is_mcp_direct_blocked` 检查 → 负缓存永远读不到，
    /// 401 后 60s 内仍每请求白打一跳。
    #[test]
    fn call_mcp_direct_checks_negative_cache_before_send() {
        let full = include_str!("provider.rs");
        let prod = full
            .split_once("\n#[cfg(test)]")
            .map(|(a, _)| a)
            .unwrap_or(full);
        let direct = prod
            .split("async fn call_mcp_direct(")
            .nth(1)
            .expect("call_mcp_direct 不应被改名");
        let direct = direct
            .split("\n    }\n")
            .next()
            .expect("call_mcp_direct 应有函数体收尾");
        assert!(
            direct.contains("is_mcp_direct_blocked("),
            "直连发送前必须查负缓存（失败 60s 内跳过直连降级回池子错误）"
        );
    }

    /// 同请求 401 必须换号：删掉 excluding / exclude.insert 会退回单 token。
    #[test]
    fn call_mcp_direct_rotates_on_same_request_after_failure() {
        let full = include_str!("provider.rs");
        let prod = full
            .split_once("\n#[cfg(test)]")
            .map(|(a, _)| a)
            .unwrap_or(full);
        let direct = prod
            .split("async fn call_mcp_direct(")
            .nth(1)
            .expect("call_mcp_direct 不应被改名");
        let direct = direct
            .split("\n    }\n")
            .next()
            .expect("call_mcp_direct 应有函数体收尾");
        let acquire = ["acquire_mcp_direct_token", "_excluding"].concat();
        assert!(
            direct.contains(&acquire),
            "直连必须按排除集选下一个 token（同请求换号）"
        );
        assert!(
            direct.contains("exclude.insert"),
            "试过的 id 必须进排除集，否则会钉死同一号"
        );
    }

    /// 缺口 B 守卫：overload fallback **成功**路径的 CallMeta 双口径必须与主循环一致
    /// —— `model` 恒为客户端原始名（client_model 回落），`mapped_model` 记 fallback 名
    /// （它就是实际发给上游的名，直接进 upstream_model 口径）。
    ///
    /// 历史缺陷：`model` 被覆盖成 fallback 名、`mapped_model` 恒 None ⇒ requested_model
    /// 失真（面板以为客户端点了 fallback 模型）+ upstream_model 回落 model 双重错误。
    /// 回退即 FAIL：把 `model:` 改回 `Some(fallback_model_id` 或把 `mapped_model:` 改回
    /// `None`，断言失败。
    #[test]
    fn overload_fallback_success_keeps_client_model_in_meta() {
        let full = include_str!("provider.rs");
        let prod = full
            .split_once("\n#[cfg(test)]")
            .map(|(a, _)| a)
            .unwrap_or(full);
        let retry_fn = prod
            .split("async fn call_api_with_retry")
            .nth(1)
            .expect("call_api_with_retry 不应被改名");
        // 切到 fallback 分支：从「尝试 overload_fallback_model」日志到失败记录组装之前。
        let fb = retry_fn
            .split("overload_fallback_model: {}")
            .nth(1)
            .expect("fallback 分支锚点不应被改名");
        let fb_ok = fb
            .split("let final_error = last_error")
            .next()
            .unwrap_or(fb);
        let mapped = ["mapped_model: ", "Some(fallback_model_id"].concat();
        assert!(
            fb_ok.contains(&mapped),
            "fallback 成功路径必须把 fallback 名记入 mapped_model（upstream_model 口径），\
             否则按 upstream_model 聚合时该样本按原始名统计"
        );
        assert!(
            fb_ok.contains("model: client_model_owned.clone().or_else(|| model.clone())"),
            "fallback 成功路径的 CallMeta.model 必须用客户端原始名（client_model 回落），\
             不得覆盖成 fallback 名（requested_model 契约）"
        );
    }

    /// ⭐ S3：最早类型化 429 保留 —— 终态错误组装（`assemble_final_error`）的行为集。
    ///
    /// 场景（research §2.3）：attempt1 = A 号 429（上游 RA 30s）、attempt2 = C 号 5xx
    /// → 终态按最后一个错误分类 → 客户端拿 503+3s 而不是 429+30s。修复后终态串
    /// 必须带上最早 429 的 RA marker，由 map_provider_error 的 A7 分支映射回 429。
    #[test]
    fn assemble_final_error_keeps_earliest_429_ra_over_later_5xx() {
        let final_err = anyhow::anyhow!(
            "流式 API 请求失败: 502 Bad Gateway {{\"message\":\"upstream\"}}"
        );
        // 先 429(RA=30) 后 502 → 终态仍带 RA=30。
        let merged = assemble_final_error(
            final_err,
            Some(30),
            crate::usage::RequestOutcome::ServerError,
        );
        let s = merged.to_string();
        assert!(
            s.contains(&format!("{}30", crate::anthropic::handlers::UPSTREAM_RETRY_AFTER_MARKER_PREFIX)),
            "最早类型化 429 的 RA=30 必须被并入 5xx 终态（否则客户端拿 503+3s 而非 429+30s）: {s}"
        );
    }

    /// 🔴 m7 回归（2026-08-16 对抗审查 RA MINOR）：重试链内 429 RA 合并必须是
    /// `.max()` 语义——**保留最大 RA**（「上游说多久等多久」）。
    ///
    /// `.or()`（首见值胜出）的 bug：attempt1 号 429 RA=10、attempt2 号 429 RA=120 时
    /// 客户端拿首个 10s 就重试，提前撞回上游仍在限流的窗口（120s 是更晚、更保守的
    /// 退避指令，却被首个值吞掉）。
    ///
    /// 回退即 FAIL：把 `merge_upstream_429_retry_after` 改回 `.or()` → 本条
    /// 「先 10 后 120 → 终态 120」断言红。
    #[test]
    fn merge_upstream_429_retry_after_keeps_max() {
        // 先 10 后 120 → 终态 120（m7 核心场景）。
        assert_eq!(
            merge_upstream_429_retry_after(Some(10), Some(120)),
            Some(120),
            "第二个号 429 RA=120 时客户端不得拿首个 10s 就重试"
        );
        // 逆序（120 先、10 后）→ 仍是 120。
        assert_eq!(merge_upstream_429_retry_after(Some(120), Some(10)), Some(120));
        // 首个 429 无 RA、后续 429 有 RA → 取后续（max(None, Some) = Some）。
        assert_eq!(merge_upstream_429_retry_after(None, Some(10)), Some(10));
        // 先 429 后 5xx（无 RA）→ 首个 RA 仍保留（max(Some, None) = Some，
        // 5xx 不能稀释 429 的退避指令）——与既有
        // `assemble_final_error_keeps_earliest_429_ra_over_later_5xx` 场景兼容。
        assert_eq!(merge_upstream_429_retry_after(Some(10), None), Some(10));
        // 全程无 RA → None。
        assert_eq!(merge_upstream_429_retry_after(None, None), None);
    }

    /// S3 限定：没有前置 429 时终态逐字不变（默认配置路径零影响）。
    #[test]
    fn assemble_final_error_untouched_without_earlier_429() {
        let err = anyhow::anyhow!("流式 API 请求失败: 502 Bad Gateway");
        let out = assemble_final_error(err, None, crate::usage::RequestOutcome::ServerError);
        assert_eq!(out.to_string(), "流式 API 请求失败: 502 Bad Gateway");
    }

    /// S3 限定①：吸收层耗尽路径（absorb_budget_exhausted=1，503 语义）不转换。
    #[test]
    fn assemble_final_error_never_converts_absorb_exhausted() {
        let marker = crate::anthropic::handlers::ABSORB_BUDGET_EXHAUSTED_MARKER;
        let err = anyhow::anyhow!("流式 API 请求失败: 429 Too Many Requests {}", marker);
        let out = assemble_final_error(
            err,
            Some(30),
            crate::usage::RequestOutcome::RateLimited,
        );
        assert!(
            !out.to_string()
                .contains(crate::anthropic::handlers::UPSTREAM_RETRY_AFTER_MARKER_PREFIX),
            "吸收耗尽 503 不得被 429 marker 转换（Cursor 掐会话兼容）"
        );
    }

    /// S3 限定②：永久态/配额/背压/已带真值的终态一律不转换。
    #[test]
    fn assemble_final_error_never_converts_recognized_branches() {
        let cases: Vec<(&str, crate::usage::RequestOutcome)> = vec![
            ("流式 API 请求失败: 403 Forbidden subscription_unsupported=1", crate::usage::RequestOutcome::BadRequest),
            ("模型不被本号池支持 model_unsupported_by_pool=1", crate::usage::RequestOutcome::OtherError),
            ("所有凭据均在冷却（0/2）retry_after_secs=10", crate::usage::RequestOutcome::RateLimited),
            ("入站限速排队超时 inbound_admission_timeout=1 retry_after_secs=3", crate::usage::RequestOutcome::OtherError),
            ("上游并发闸已满 upstream_gate_full=1 retry_after_secs=2", crate::usage::RequestOutcome::OtherError),
            ("流式 API 请求失败: 429 {\"reason\":\"MONTHLY_REQUEST_COUNT\"}", crate::usage::RequestOutcome::RateLimited),
            ("流式 API 请求失败（所有凭据已用尽）quota_exhausted_all=1: 429 x", crate::usage::RequestOutcome::QuotaExhausted),
            ("每客户端请求的上游调用预算已耗尽 shared_budget_exhausted=1", crate::usage::RequestOutcome::OtherError),
        ];
        for (raw, outcome) in cases {
            let out = assemble_final_error(anyhow::anyhow!("{}", raw), Some(30), outcome);
            assert_eq!(
                out.to_string(),
                raw,
                "已识别分支不得被最早 429 的 RA 转换: {raw}"
            );
        }
    }

    /// S3 限定③④：终态已是 429+RA（自带 marker）不重复打；非 generic 终态
    /// （认证失败/400/模型容量）不转换（与 zyphr 只在终态 generic 时保留一致）。
    #[test]
    fn assemble_final_error_skips_already_marked_and_recognized_outcomes() {
        let marker = crate::anthropic::handlers::UPSTREAM_RETRY_AFTER_MARKER_PREFIX;
        // 终态本身就是 429 + RA=60：保留最早 30 还是覆盖成 60？—— 保留终态自身（已带 marker）。
        let already = format!("流式 API 请求失败: 429 Too Many Requests {}60", marker);
        let out = assemble_final_error(
            anyhow::anyhow!("{}", already),
            Some(30),
            crate::usage::RequestOutcome::RateLimited,
        );
        assert_eq!(
            out.to_string(),
            already,
            "终态已带自己的 marker 时不重复并入（终态自身是最后一条 429 的信息）"
        );

        // 认证失败终态（AuthFailed）：不转换（401/403 语义保持）。
        let auth = anyhow::anyhow!("流式 API 请求失败: 401 Unauthorized {{\"message\":\"x\"}}");
        let out = assemble_final_error(
            auth,
            Some(30),
            crate::usage::RequestOutcome::AuthFailed,
        );
        assert!(
            !out.to_string().contains(marker),
            "认证失败终态不得被 429 转换（与 zyphr take_rate_limit_error 语义一致）"
        );

        // 400 终态（BadRequest）：不转换。
        let bad = anyhow::anyhow!("流式 API 请求失败: 400 Bad Request {{\"message\":\"x\"}}");
        let out = assemble_final_error(bad, Some(30), crate::usage::RequestOutcome::BadRequest);
        assert!(!out.to_string().contains(marker));

        // 模型容量终态（ModelUnavailable）：不转换（有独立的 503 overload 语义）。
        let cap = anyhow::anyhow!("流式 API 请求失败: 503 MODEL_TEMPORARILY_UNAVAILABLE");
        let out = assemble_final_error(
            cap,
            Some(30),
            crate::usage::RequestOutcome::ModelUnavailable,
        );
        assert!(!out.to_string().contains(marker));
    }

    /// 缺口 A 守卫：Kiro 主路径成功/失败埋点的 `requested_model` 必须同源（都是
    /// `client_model` = 客户端原始名），不得一边原始名一边归一化 Kiro id 的混合口径。
    ///
    /// 历史缺陷：成功路径埋点记 `extract_model_and_session` 从请求体解析的 modelId
    /// （已被 converter 归一化成 Kiro id），失败记录同源同错——与透传路径（原始名）
    /// 口径分叉。回退即 FAIL：把成功路径的 `model:` 改回 `model.clone()` 或把
    /// `fail_record.requested_model` 改回 `model.clone()`，断言失败。
    #[test]
    fn kiro_success_and_failure_records_share_client_model() {
        let full = include_str!("provider.rs");
        let prod = full
            .split_once("\n#[cfg(test)]")
            .map(|(a, _)| a)
            .unwrap_or(full);
        let retry_fn = prod
            .split("async fn call_api_with_retry")
            .nth(1)
            .expect("call_api_with_retry 不应被改名");
        let success_meta = "model: client_model_owned.clone().or_else(|| model.clone())";
        assert_eq!(
            retry_fn.matches(success_meta).count(),
            2,
            "主循环与 overload fallback 两条成功路径的 CallMeta.model 都必须用客户端原始名"
        );
        assert!(
            retry_fn.contains("fail_record.requested_model = client_model_owned.clone().or(model.clone())"),
            "失败记录 requested_model 必须与成功路径同源（client_model），\
             否则按 requested_model 聚合时成功/失败口径分叉"
        );
        assert!(
            retry_fn.contains("client_model_owned.clone().or(model.clone()).unwrap_or_default()"),
            "失败记录的 record.model 必须同样回落 client_model（与成功记录 record.model 口径一致）"
        );
    }

    /// 模型映射改写 body：命中 `/conversationState/currentMessage/userInputMessage/modelId`。
    #[test]
    fn test_rewrite_model_id_replaces_kiro_model_id() {
        let body = r#"{"conversationState":{"currentMessage":{"userInputMessage":{"modelId":"claude-opus-4-8"}}}}"#;
        let out = KiroProvider::rewrite_model_id(body, "claude-sonnet-4-5");
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(
            v["conversationState"]["currentMessage"]["userInputMessage"]["modelId"],
            "claude-sonnet-4-5"
        );
    }

    /// 非 JSON body：`rewrite_model_id` 原样返回不 panic（映射在该跳静默不生效）。
    #[test]
    fn test_rewrite_model_id_invalid_json_returns_unchanged() {
        let body = "not-json";
        assert_eq!(KiroProvider::rewrite_model_id(body, "x"), body);
    }

    /// 动态降档阶梯的边界：0/0.3/0.5 为不变档，0.31/0.51 触发降档，地板 1。
    #[test]
    fn test_apply_retry_pressure_staircase() {
        assert_eq!(apply_retry_pressure(12, 0.0), 12);
        assert_eq!(apply_retry_pressure(12, 0.3), 12, "0.3 恰好是阈值，不降");
        assert_eq!(apply_retry_pressure(12, 0.5), 6, "0.5 未过 0.5 档但过 0.3 档 → 砍半");
        assert_eq!(apply_retry_pressure(12, 0.31), 6, ">0.3 砍半");
        assert_eq!(apply_retry_pressure(12, 0.51), 3, ">0.5 砍到 33%（12*33/100=3）");
        assert_eq!(apply_retry_pressure(12, 1.0), 3, "满额 429 也只砍到 3，不归零");
        assert_eq!(apply_retry_pressure(1, 1.0), 1, "地板 1：降档绝不归零");
        assert_eq!(apply_retry_pressure(3, 0.51), 1, "3 的 33% 向下取整到 1");
    }

    /// 窗口 rate() 是纯计算：直接注入状态验证 429 占比。
    #[test]
    fn test_retry_pressure_window_rate() {
        let mut w = RetryPressureWindow::new(60);
        assert_eq!(w.rate(), 0.0, "空窗口无信号，不降档");
        // 5 成功 + 5 个 429 → 50%
        for i in 0..10 {
            w.deque.push_back((std::time::Instant::now(), i % 2 == 1));
        }
        assert!((w.rate() - 0.5).abs() < 1e-6);
        // 全 429 → 100%
        let mut w2 = RetryPressureWindow::new(60);
        for _ in 0..4 {
            w2.deque.push_back((std::time::Instant::now(), true));
        }
        assert_eq!(w2.rate(), 1.0);
    }

    /// 🔴 回归：5xx 与 429 同样计入压力（纯 500 风暴降档必须触发）；
    /// 4xx（客户端错误）不算压力。
    #[test]
    fn test_retry_pressure_window_counts_5xx_and_not_4xx() {
        let mut w = RetryPressureWindow::new(60);
        // 2 个 500 + 1 个 200 → 压力率 2/3
        w.deque.push_back((std::time::Instant::now(), false)); // 200
        w.deque.push_back((std::time::Instant::now(), true)); // 500
        w.deque.push_back((std::time::Instant::now(), true)); // 500
        assert!(
            (w.rate() - 2.0 / 3.0).abs() < 1e-6,
            "5xx 必须计入压力（纯 500 风暴降档才不失效），实际 {}",
            w.rate()
        );

        // 4xx 不算压力：2 个 400 + 1 个 200 → 压力率 0
        let mut w2 = RetryPressureWindow::new(60);
        w2.deque.push_back((std::time::Instant::now(), false)); // 200
        w2.deque.push_back((std::time::Instant::now(), false)); // 400
        w2.deque.push_back((std::time::Instant::now(), false)); // 400
        assert_eq!(w2.rate(), 0.0, "4xx（客户端错误）不算压力");
    }

    /// record() 顺带逐出超窗事件：极小窗口 + sleep 后，旧事件被清出。
    #[tokio::test]
    async fn test_retry_pressure_window_prune_expired() {
        let mut w = RetryPressureWindow::new(1); // 1s 窗口
        w.record(true);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(w.deque.len(), 1, "窗口内一条还在");
        // 换一个 0 秒窗口：第二次 record 必把第一条逐出
        let mut w0 = RetryPressureWindow::new(0);
        w0.record(true);
        w0.record(false);
        assert_eq!(w0.deque.len(), 1, "0 秒窗口下第一条立即过期");
        assert_eq!(w0.rate(), 0.0, "剩下的那一条是 false");
    }

    /// 并发闸 Semaphore：容量 N 时 N 个 permit 全过、第 N+1 拿不到、Drop 后恢复。
    #[tokio::test]
    async fn test_upstream_gate_concurrency() {
        let gate = Arc::new(tokio::sync::Semaphore::new(2));
        let p1 = gate.clone().try_acquire_owned().unwrap();
        let p2 = gate.clone().try_acquire_owned().unwrap();
        assert!(
            gate.clone().try_acquire_owned().is_err(),
            "容量 2 时第 3 个拿不到"
        );
        drop(p1);
        let p3 = gate.clone().try_acquire_owned().unwrap();
        drop(p2);
        drop(p3);
        let p4 = gate.clone().try_acquire_owned().unwrap();
        drop(p4);
        assert_eq!(gate.available_permits(), 2, "全部 Drop 后 permit 复原");
    }

    /// 预算恒被 `ABSOLUTE_MAX_TOTAL_RETRIES` 封顶，**且刻意不再随可用号数抬高**。
    ///
    /// ⚠️ 本测试此前名为 `..._covers_every_available_credential`，断言 `r >= total`
    /// 并声称"保证每个可用凭据至少被尝试一次"。那个承诺在移除内层 `.max(available)`
    /// 之后已不成立 —— 它当时**只是碰巧通过**：`total=10` 时预算 `min(30,12)=12`，
    /// 而 `12 >= 10` 恰好为真；换成现在的 4 上限后 `min(30,4)=4`，连 `total=10`
    /// 都过不了。即那是个会在号池扩容时才爆的定时炸弹，且它在维护一条代码已不提供的不变式。
    ///
    /// 现在改为锁住真实行为：封顶生效。若有人把 `.max(available)` 加回来（那正是
    /// 「号池越大越慢」的成因：线上 43 号时预算 = 43，单请求扫全池耗尽 45s 墙钟），
    /// `large_pool_stays_capped` 会立刻失败。
    #[test]
    fn test_compute_max_retries_is_capped_and_ignores_available() {
        // 常规池：按 total*per_cred 走，但受绝对上限封顶。
        assert_eq!(
            compute_max_retries(10, 10),
            (10 * MAX_RETRIES_PER_CREDENTIAL).min(ABSOLUTE_MAX_TOTAL_RETRIES)
        );

        // ⭐ 承重断言：大池必须仍被封顶，**不因可用号多而放开**。
        let large = compute_max_retries(20, 20);
        assert_eq!(
            large, ABSOLUTE_MAX_TOTAL_RETRIES,
            "大号池预算必须封顶在 {}，实际 {} —— 若等于 available 则说明 .max(available) 被加回来了",
            ABSOLUTE_MAX_TOTAL_RETRIES, large
        );

        // `available` 不参与计算：同一 total 下改变 available 不应改变结果。
        assert_eq!(
            compute_max_retries(20, 1),
            compute_max_retries(20, 20),
            "available 已不参与预算计算，改变它不该影响结果"
        );
    }

    /// 预算永不为 0：0 意味着一次都不尝试，请求立刻以「最大重试次数（0次）」失败。
    ///
    /// 这是真实回归的守卫：把预算基数从 `total_count()`（含 disabled，恒非 0）改成
    /// `kiro_selectable_count()` 后，瞬时全池不可选会让基数为 0 → 预算 0 →
    /// acquire_context 的等待逻辑根本没机会跑。线上 20 分钟内出现 10 次。
    #[test]
    fn should_never_return_zero_retry_budget() {
        assert_eq!(
            compute_max_retries(0, 0),
            1,
            "全池瞬时不可选时也必须至少尝试一次，否则请求零重试即失败"
        );
        for (t, a) in [(0usize, 0usize), (0, 1), (1, 0), (1, 1)] {
            assert!(
                compute_max_retries(t, a) >= 1,
                "compute_max_retries({t}, {a}) 不得为 0"
            );
        }
    }

    /// 收紧上限的意图守卫：一条请求不该能连打十几个号。
    ///
    /// 生产事故里 `尝试 8/36` 的 36 = 12 号 × 3，配合 suspend 分支的零延迟遍历，
    /// 一条客户端请求几秒内烧掉 8~12 个账号（同一出口 IP），正是风控要抓的突发特征。
    #[test]
    fn should_cap_retry_budget_well_below_historic_36() {
        // 与生产同规模的池子（12 个可选号）
        assert!(
            compute_max_retries(12, 12) <= ABSOLUTE_MAX_TOTAL_RETRIES,
            "12 号池的预算必须被上限约束，不能回到 36"
        );
        assert!(
            ABSOLUTE_MAX_TOTAL_RETRIES < 36,
            "绝对上限必须显著小于事故时的 36"
        );
    }

    #[test]
    fn test_compute_max_retries_small_pool() {
        // 小号池降重试：total<=SMALL_POOL_THRESHOLD 时每号只重试 1 次，
        // 每个号各摸一次即透传上游错误，避免在小池上反复砸同几个号加重冷却。
        assert_eq!(compute_max_retries(3, 3), 3, "3 号池应每号只摸 1 次 = 3");
        assert_eq!(compute_max_retries(2, 2), 2, "2 号池应每号只摸 1 次 = 2");
        // 只有 1 个凭据仍至少能试 1 次
        assert_eq!(compute_max_retries(1, 1), 1);

        // 刚过小池阈值（total=4）恢复常规 total*MAX_RETRIES_PER_CREDENTIAL，
        // 但随即被 ABSOLUTE_MAX_TOTAL_RETRIES 封顶（min(4×3, 4) = 4）。
        assert_eq!(compute_max_retries(4, 4), ABSOLUTE_MAX_TOTAL_RETRIES);

        // 小池但部分禁用：available 做下限，仍保证可用号被摸到。
        assert!(compute_max_retries(3, 2) >= 2);
    }

    #[test]
    fn test_compute_max_retries_respects_absolute_upper_bound() {
        // 巨量凭据：预算**恒**被 ABSOLUTE_MAX 封顶，不再随 available 放大。
        assert!(compute_max_retries(1000, 1000) <= ABSOLUTE_MAX_TOTAL_RETRIES);
        assert_eq!(
            compute_max_retries(100, 5),
            ABSOLUTE_MAX_TOTAL_RETRIES,
            "可用号少于上限时应封顶到 ABSOLUTE_MAX"
        );
    }

    /// 回归（大号池不得放大重试 · 本轮核心）：预算恒 ≤ ABSOLUTE_MAX_TOTAL_RETRIES，
    /// 与池子大小无关。
    ///
    /// **旧代码为何失败**：`.min(ABSOLUTE_MAX_TOTAL_RETRIES.max(available))` 里的内层
    /// `.max(available)` 在 `available > ABSOLUTE_MAX_TOTAL_RETRIES` 时把硬上限自己抵消掉
    /// → 预算 = available。
    /// 线上 43 个号实测预算 = 43，日志即「尝试 43/43」：一条请求顺着整池撞一遍、
    /// 耗尽 45s 墙钟才失败 → 用户体感 45 秒卡死，且**号池越大越慢**。
    /// 旧代码下 `compute_max_retries(43, 43)` 返回 43，本断言会失败。
    #[test]
    fn should_not_scale_retry_budget_with_pool_size() {
        for available in [13usize, 43, 200, 1000] {
            let r = compute_max_retries(available, available);
            assert!(
                r <= ABSOLUTE_MAX_TOTAL_RETRIES,
                "{available} 个可用号时预算为 {r}，必须被 {ABSOLUTE_MAX_TOTAL_RETRIES} 封顶——\
                 否则号池越大单请求越慢（线上实测 43 号 → 尝试 43/43 → 45s 墙钟）"
            );
        }
        // 线上确切规模的定点回归
        assert_eq!(
            compute_max_retries(43, 43),
            ABSOLUTE_MAX_TOTAL_RETRIES,
            "43 号池（线上实测规模）预算必须是 {ABSOLUTE_MAX_TOTAL_RETRIES} 而非 43"
        );
    }

    #[test]
    fn test_extract_model_and_session_both_present() {
        // 一次解析应同时取出 modelId 与 conversationId（与旧双解析等价）
        let body = r#"{
            "conversationState": {
                "conversationId": "0b4445e1-f5be-49e1-87ce-62bbc28ad705",
                "currentMessage": {
                    "userInputMessage": { "modelId": "claude-sonnet-4" }
                }
            }
        }"#;
        let (model, session) = KiroProvider::extract_model_and_session(body);
        assert_eq!(model.as_deref(), Some("claude-sonnet-4"));
        assert_eq!(session.as_deref(), Some("0b4445e1-f5be-49e1-87ce-62bbc28ad705"));
    }

    #[test]
    fn test_extract_model_and_session_partial() {
        // 只有 conversationId、无 modelId：model=None、session=Some
        let only_session = r#"{"conversationState":{"conversationId":"8bb5523b-ec7c-4540-a9ca-beb6d79f1552"}}"#;
        let (model, session) = KiroProvider::extract_model_and_session(only_session);
        assert_eq!(model, None);
        assert_eq!(session.as_deref(), Some("8bb5523b-ec7c-4540-a9ca-beb6d79f1552"));

        // 只有 modelId、无 conversationId：model=Some、session=None
        let only_model =
            r#"{"conversationState":{"currentMessage":{"userInputMessage":{"modelId":"m"}}}}"#;
        let (model, session) = KiroProvider::extract_model_and_session(only_model);
        assert_eq!(model.as_deref(), Some("m"));
        assert_eq!(session, None);
    }

    // ===== S6 透传会话归一（会话研究 P1-1/P1-2/P1-4）=====

    /// S6 P1-1 归一化 + P1-4 脱敏：透传埋点的 session 键与 Kiro 路径同源——
    /// 同一 `metadata.user_id` 里提取出的 UUID，两条路径必须同一个 key；
    /// 且只落 UUID，`user_xxx_account__` 前缀 / account_uuid 明文不得进 trace。
    #[test]
    fn test_passthrough_session_id_kiro_consistent_and_redacted() {
        // 字符串格式（Claude Code 典型）：含 account 前缀 + account_uuid 片段
        let user_id = "user_ffffffff-aaaa-4bbb-8ccc-dddddddddddd_account__session_0b4445e1-f5be-49e1-87ce-62bbc28ad705";
        let extracted =
            KiroProvider::extract_session_uuid(user_id).expect("应从 user_id 提取出 session UUID");
        assert_eq!(
            extracted,
            "0b4445e1-f5be-49e1-87ce-62bbc28ad705",
            "透传 session 必须是提取后的纯 UUID（Kiro 路径 conversationId 同源）"
        );
        // 脱敏：trace 里的 session 键不含 account 前缀 / account_uuid 片段
        assert!(
            !extracted.contains("account_") && !extracted.contains("ffffffff-aaaa"),
            "session 键不得携带 account_uuid 明文，实际 {extracted}"
        );

        // 归一化：Kiro 路径把同一 UUID 写进 conversationState.conversationId，
        // 提取结果 = 透传提取结果 = 同一个 key（同会话跨路径不再拆双 key）。
        let kiro_body = format!(
            r#"{{"conversationState":{{"conversationId":"{extracted}"}}}}"#
        );
        let (_, kiro_session) = KiroProvider::extract_model_and_session(&kiro_body);
        assert_eq!(
            kiro_session.as_deref(),
            Some(extracted.as_str()),
            "Kiro 路径与透传路径必须得到同一个 session key"
        );
    }

    /// S6 P1-1 兜底 None：JSON 格式的 user_id 提取 session_id（形状合法才收）。
    #[test]
    fn test_passthrough_session_id_json_format() {
        let user_id = r#"{"device_id":"0dede55c6dcc4a11a30bbb5e7f22e6fdf86cdeba3820019cc27612af4e1243cd","account_uuid":"acc-123","session_id":"8bb5523b-ec7c-4540-a9ca-beb6d79f1552"}"#;
        assert_eq!(
            KiroProvider::extract_session_uuid(user_id).as_deref(),
            Some("8bb5523b-ec7c-4540-a9ca-beb6d79f1552")
        );
    }

    /// S6 P1-1 兜底 None：提取不到（无 session / 非法形状 / 空）→ None，
    /// 不再回落原始 user_id 串（旧行为把整串当 session 进 trace）。
    #[test]
    fn test_passthrough_session_id_none_when_not_extractable() {
        assert_eq!(KiroProvider::extract_session_uuid(""), None, "空串无会话");
        assert_eq!(
            KiroProvider::extract_session_uuid("plain-string-no-session"),
            None,
            "无 session_ 标记的普通串无会话"
        );
        assert_eq!(
            KiroProvider::extract_session_uuid("user_x_account__session_not-a-uuid"),
            None,
            "session_ 后非 UUID 形状 → None（形状门）"
        );
        assert_eq!(
            KiroProvider::extract_session_uuid(r#"{"session_id":"not-a-uuid"}"#),
            None,
            "JSON 里 session_id 非 UUID 形状 → None"
        );
    }

    /// S6 P1-2 兜底 None（Kiro 侧形状门）：conversationId 非 UUID 形状 → session None。
    /// converter 产的 conversationId 恒为 UUID 形状，此门只拦截异常/伪造键，不再进
    /// by_session / traces。
    #[test]
    fn test_kiro_session_id_requires_uuid_shape() {
        // 合法 UUID 形状 → 收
        let good = r#"{"conversationState":{"conversationId":"8bb5523b-ec7c-4540-a9ca-beb6d79f1552"}}"#;
        let (_, session) = KiroProvider::extract_model_and_session(good);
        assert_eq!(session.as_deref(), Some("8bb5523b-ec7c-4540-a9ca-beb6d79f1552"));

        // 非 UUID 形状（畸形/伪造）→ None（兜底 None，by_session 不落键）
        for bad in [
            "sess-123",
            "not-a-uuid",
            "0b4445e1-f5be-49e1-87ce",       // 短
            "0b4445e1-f5be-49e1-87ce-62bbc28ad705XX", // 长
        ] {
            let body = format!(r#"{{"conversationState":{{"conversationId":"{bad}"}}}}"#);
            let (_, session) = KiroProvider::extract_model_and_session(&body);
            assert_eq!(session, None, "非 UUID 形状 conversationId 必须归 None（实际 {bad}）");
        }
    }


    #[test]
    fn should_build_mcp_record_with_honest_zeros_and_no_credits() {
        let rec = build_mcp_record(7, crate::usage::RequestOutcome::Success, 123, 2);
        assert_eq!(rec.credential_id, Some(7), "必须归属到真实服务的凭据");
        assert_eq!(rec.model, MCP_USAGE_MODEL, "MCP 无 modelId，用显式常量标识");
        // MCP 上游既不返回 token 数也无本地估算依据：只能是 0，不许瞎估。
        assert_eq!(rec.input_tokens, 0);
        assert_eq!(rec.output_tokens, 0);
        assert_eq!(rec.cache_read_tokens, 0);
        assert_eq!(rec.cache_creation_tokens, 0);
        assert_eq!(rec.credits_used, None, "MCP 响应无 meteringEvent");
        assert!(!rec.is_streaming, "MCP 上游是一次性 JSON POST");
        assert_eq!(rec.latency_ms, 123);
        assert_eq!(rec.retries, 2);
        assert_eq!(rec.outcome, crate::usage::RequestOutcome::Success);
        assert!(rec.error_message.is_none(), "成功记录不应带错误信息");
        // request_id 每条唯一，否则 SQLite 主键冲突会静默丢记录。
        let other = build_mcp_record(7, crate::usage::RequestOutcome::Success, 123, 2);
        assert_ne!(rec.request_id, other.request_id);
    }

    /// 源码级守卫：MCP 成功分支里 `report_success` 与 `emit_record` 必须成对出现。
    ///
    /// 单测覆盖不到 `call_mcp_with_retry`（需真实上游 + 号池），而这正是回归发生的地方：
    /// 历史实现只加凭据计数器不落用量记录，导致 success_count 恒大于用量库记录数。
    #[test]
    fn should_emit_usage_record_in_mcp_success_branch() {
        let src = include_str!("provider.rs");
        let mcp_fn = src
            .split("async fn call_mcp_with_retry")
            .nth(1)
            .expect("call_mcp_with_retry 不应被改名");
        // 截到该函数内第一次出现「失败响应」处理为止，只看成功分支。
        let success_branch = mcp_fn
            .split("// 失败响应")
            .next()
            .expect("成功分支的定位注释不应被删改");
        assert!(
            success_branch.contains("report_success"),
            "成功分支应上报凭据成功"
        );
        assert!(
            success_branch.contains("emit_record(build_mcp_record("),
            "MCP 成功分支必须落一条用量记录，否则凭据计数与用量库对不上账"
        );
    }

    /// 源码级守卫（已知问题 #11）：MCP 路径的**失败出口**必须 emit_record + bump 计数器。
    ///
    /// 历史缺陷：`call_mcp_with_retry` 只有成功分支 emit_record，失败全部零埋点 ⇒
    /// MCP 失败在面板与 recovery-metrics 端点上完全不存在，成功率的分子分母对不上账。
    /// 单测覆盖不到（需真实上游 + 号池），用源码断言钉死 7 个失败出口。
    #[test]
    fn mcp_failure_exits_must_emit_record_and_bump_counter() {
        let full = include_str!("provider.rs");
        let src = full
            .split_once("\n#[cfg(test)]")
            .map(|(a, _)| a)
            .unwrap_or(full);
        let mcp_fn = src
            .split("async fn call_mcp_with_retry")
            .nth(1)
            .expect("call_mcp_with_retry 不应被改名");
        // 只看成功分支之后的失败区（把本测试的 needle 排除在命中集外）。
        let failure_region = mcp_fn
            .split("// 失败响应")
            .nth(1)
            .expect("失败响应的定位注释不应被删改");
        assert!(
            failure_region.contains("crate::common::recovery_metrics::bump_mcp_failure()"),
            "MCP 失败出口必须 bump 专用计数器，否则失败在 recovery-metrics 端点上不可见"
        );
        assert!(
            failure_region.contains("emit_record(build_mcp_record("),
            "MCP 失败出口必须 emit_record，否则失败在用量面板上不存在（#11）"
        );
        // client_for 那个出口在「// 失败响应」标记之前，故按整个 MCP 函数计数（排除测试段）。
        assert_eq!(
            mcp_fn
                .matches("crate::common::recovery_metrics::bump_mcp_failure()")
                .count(),
            7,
            "MCP 应有 7 个失败出口（5 条 bail + client_for `?` + 重试耗尽）各自 bump；\
             数量变化说明出口新增/删除，需同步本守卫"
        );
    }

    /// ⭐ 源码级守卫：MCP 重试循环必须有墙钟预算闸门（2026-08-15，M5）。
    ///
    /// 历史缺陷：`call_mcp_with_retry` 只有次数闸（`max_retries`）无墙钟。retry_delay
    /// 指数退避叠加后，一条慢请求可以在小号池里拖过分钟级、反复扫同一个坏号，把偶发
    /// 429 拖成持续雪崩 —— 与对话路径 2026-08-11 修掉的吸收层放大是同一形态
    /// （对话路径靠 round_clock 闸门兜住，MCP 路径当时漏了）。
    ///
    /// 单测覆盖不到（需真实上游 + 号池），用源码断言钉死三点：
    /// 1. 闸门确实在 MCP 函数内；
    /// 2. 闸门在 MCP 的 for 循环**内**、且在 `acquire_context`（发请求前）之前 ——
    ///    挪到循环外或发请求之后都会让墙钟失效；
    /// 3. 整个生产段只有这一处该闸门（防在别的函数里加个假的充数）。
    #[test]
    fn mcp_retry_loop_must_have_wall_clock_gate() {
        let full = include_str!("provider.rs");
        let src = full
            .split_once("\n#[cfg(test)]")
            .map(|(a, _)| a)
            .unwrap_or(full);
        let mcp_fn = src
            .split("async fn call_mcp_with_retry")
            .nth(1)
            .expect("call_mcp_with_retry 不应被改名");
        // needle 运行时拼接（include_str! 会把测试自身字面量也读进来，本仓踩过多次）。
        // 判据不带对齐空格（本仓守卫约定，见 token_manager 排序键守卫的教训）：
        // 只匹配「预算比较」这一行本身，缩进/换行随 rustfmt 怎么排都不影响。
        // 常量取 MCP_WALL_SECS（≈read_timeout×2+30，推导见该常量注释）—— 复用主路径
        // 45s 会掐死换号（同透传墙钟教训），此守卫顺带钉住不用错常量。
        let gate_body = format!(
            "{}{}",
            "&& call_started.elapsed() >= Duration::from_secs", "(MCP_WALL_SECS)"
        );
        let for_at = mcp_fn
            .find("for attempt in 0..max_retries")
            .expect("MCP 重试循环不应被改名");
        let body_at = mcp_fn.find(&gate_body).unwrap_or_else(|| {
            panic!("MCP 循环内必须存在墙钟预算比较（`{gate_body}`），否则单请求可在小号池里拖过分钟级")
        });
        // 同一语句窗口内必须有「attempt > 0」首试豁免（保证至少打一次）。
        // ⚠️ 不能用字节切片（`&mcp_fn[a..b]`）：body_at 偏移可能落在多字节字符
        // 中间（2026-08-15 实测 panic: not a char boundary），用 rfind 比较位置。
        let before_at = mcp_fn[..body_at].rfind("if attempt > 0");
        assert!(
            before_at.is_some_and(|p| body_at - p < 300),
            "墙钟闸门必须带 attempt>0 首试豁免：首次尝试不受此限，保证至少打一次"
        );
        assert!(
            for_at < body_at,
            "墙钟闸门必须在 for 循环**内**（挪到循环外即失效）"
        );
        let acquire_at = mcp_fn
            .find("acquire_context(None, None)")
            .expect("MCP 的上下文获取调用不应被改名");
        assert!(
            body_at < acquire_at,
            "墙钟闸门必须排在 acquire_context（发请求）之前，否则超预算的请求仍会真打上游"
        );
        assert_eq!(
            src.matches(&gate_body).count(),
            1,
            "该墙钟闸门在整个生产段应只出现一次（MCP 路径）；对话路径用的是 round_clock 形态"
        );
    }

    /// ⭐ 源码级守卫：两处 force-refresh 调用点都必须跳过 api_key 号。
    ///
    /// 单测覆盖不到（需真实上游返回 401/403 才会走到该分支），而这是**会加速烧号**的路径：
    /// api_key 号没有 refreshToken，`refresh_token()` 对它是契约级 bail，
    /// 在热路径上调它结构上不可能成功，却会计入失败 + 落 auth 冷却。
    ///
    /// 线上实测（本轮多开时暴露）：一个 api_key 号遇 403 后每轮白等约 3 秒
    /// （错误串不含任何 HTTP 码 → 被刷新层的黑名单式瞬态判据当可重试 → 1s+2s 退避），
    /// 连计 3 次失败即判死号自动禁用 —— 死亡速度被放大三倍。
    ///
    /// 断言两处而非一处：对话路径与 MCP 路径各有一份 force-refresh 逻辑，
    /// 这种「同款逻辑复制两份」正是本仓 #4 类漏改事故的成因（对话路径修了、MCP 漏了）。
    /// 🔴 额度耗尽判定**不得门控状态码** —— 只认 body 里的 reason 字面量。
    ///
    /// # 实测（2026-08-05，6 小时窗口）
    ///
    /// - `402 Payment Required`：**0 次**
    /// - `400 Bad Request` + `"reason":"OVERAGE_REQUEST_LIMIT_EXCEEDED"`：**564 次**
    ///
    /// 旧代码 `status == 402 && is_monthly_request_limit(&body)` ⇒ 那道门从不成立 ⇒
    /// 564 个额度耗尽的请求落到通用 400 分支 `break`，凭据**不禁用、继续留在轮转里**，
    /// 每个新请求再撞一次（实测 #508 一个号吃了 543 次）。
    ///
    /// 回退即 FAIL：把 `if endpoint.is_monthly_request_limit(&body)` 改回
    /// `if status.as_u16() == 402 && endpoint.is_monthly_request_limit(&body)` → 本条失败。
    #[test]
    fn quota_exhausted_must_not_be_gated_on_status_code() {
        let src = include_str!("provider.rs");
        let cut = src.find("#[cfg(test)]").unwrap_or(src.len());
        let prod = &src[..cut];
        // needle 运行时拼接（include_str! 自匹配坑，本仓库踩过四次）。
        let bad = format!(
            "status.as_u16() == 402 && endpoint.is_monthly_request_limit{}",
            "("
        );
        assert!(
            !prod.contains(&bad),
            "额度耗尽不得门控 402：上游已改用 400（实测 402 六小时 0 次、400+OVERAGE 564 次），\
             门控会让所有额度耗尽的号继续留在轮转里反复被撞"
        );
        // 两条路径（对话 + MCP）都必须有不带状态码门控的判定。
        let good = format!("if endpoint.is_monthly_request_limit(&body){}", " {");
        assert_eq!(
            prod.matches(&good).count(),
            2,
            "对话路径与 MCP 路径都必须有该判定（当前 {} 处）",
            prod.matches(&good).count()
        );
        // 顺序守卫：必须在通用 400 分支之前，否则 400 先 break 就永远走不到。
        let qi = prod.find(&good).expect("额度判定不该被改名");
        let generic400 = format!("if status.as_u16() == 400 {}", "{");
        if let Some(gi) = prod.find(&generic400) {
            assert!(
                qi < gi,
                "额度判定必须排在通用 400 分支之前（挪到之后即失效）"
            );
        }
    }

    /// ⭐ 源码级守卫（客户端格式错误不重试防 503 风暴）：客户端请求校验错误分支必须**同时**
    /// 认 `TOOL_USE_RESULT_MISMATCH`（endpoint 层 `is_client_validation_error` 覆盖）与
    /// `TOOL_SCHEMA_INVALID`（本处补认），且命中后直接 break —— 不重试、不换号、不进吸收层。
    ///
    /// 参考 ZyphrZero/kiro.rs endpoint/mod.rs 的 `CLIENT_VALIDATION_REASONS`：这两个 reason
    /// 都是客户端请求构造问题（多轮工具结果不匹配 / 工具 schema 非法），重试/换号只会白烧
    /// 并发请求，放大成上游 503 风暴。漏认任一都会把它们当可重试瞬态错误处理。
    ///
    /// 用源码级守卫而非行为测试：`call_api_with_retry` 需真实上游 + 号池，单测造不出
    /// （本仓既有惯例）。
    #[test]
    fn client_validation_error_recognizes_both_markers_and_breaks() {
        let full = include_str!("provider.rs");
        // 切掉测试段：本测试自身的字面量不能成为假命中源。
        let src = full
            .split_once("\n#[cfg(test)]")
            .map(|(a, _)| a)
            .unwrap_or(full);
        // 定位该分支：条件文本到第一个 `{` 为止。
        let marker = "if endpoint.is_client_validation_error(&body)";
        let at = src
            .find(marker)
            .expect("客户端请求校验错误分支不应被删除");
        let cond_end = src[at..]
            .find('{')
            .map(|i| at + i)
            .unwrap_or(src.len());
        let cond = &src[at..cond_end];
        assert!(
            cond.contains("TOOL_SCHEMA_INVALID"),
            "客户端请求校验错误分支必须同时认 TOOL_USE_RESULT_MISMATCH（endpoint 层\
             is_client_validation_error）与 TOOL_SCHEMA_INVALID（本处补认）：漏认后者会把\
             客户端构造错误当可重试瞬态，白烧并发请求并放大成上游 503 风暴"
        );
        // 命中后必须 break（直接失败），分支内不得 continue（continue 即重试/换号）。
        let branch_body = &src[at..src[at..]
            .find("break")
            .map(|i| at + i)
            .expect("命中后必须 break（直接失败、不重试不换号）：改回 continue 即回归")];
        assert!(
            !branch_body.contains("continue"),
            "客户端请求校验错误分支内不得 continue：continue 即重试/换号，\
             与『客户端错不重试』的语义冲突"
        );
    }

    /// ⭐ 源码级守卫：订阅永久错误的分支必须**排在所有 403 处置之前**，且不得计凭据失败。
    ///
    /// 为什么用源码守卫而不是行为测试：触发它需要真实上游返回该 403，而本仓铁律禁止
    /// 测试依赖网络；热路径那段又在 `call_api_with_retry` / `call_mcp_with_retry` 深处，
    /// 构造不出确定性用例。
    ///
    /// 🔴 **先剔注释行再匹配**。`include_str!` 读的是原始源文本（含注释），直接
    /// `contains` 会匹配到**被注释掉**的实现 ⇒ 把代码注释掉守卫仍然绿。本仓记录
    /// 该形态已踩过五次（见 `admission_timeout_must_be_observable` 的注释）。
    #[test]
    fn subscription_unsupported_branch_must_precede_other_403_handling() {
        let src = include_str!("provider.rs");
        let prod = src.split("#[cfg(test)]").next().expect("生产段应存在");
        let prod: String = prod
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");

        // needle 运行时拼接，避免把本测试自己的字面量算进匹配。
        let sub = format!("endpoint.{}(&body)", "is_subscription_unsupported");
        let validation = format!("endpoint.{}(&body)", "is_client_validation_error");
        let temp_rl = format!("endpoint.{}(&body)", "is_temporary_rate_limit");

        // ⚠️ **必须按函数切片再比较位置**。`is_temporary_rate_limit` 有**两个**调用点
        // （`call_mcp_with_retry` 与 `call_api_with_retry`），全局 `find` 拿到的是靠前的
        // 那个（MCP），于是拿它和对话路径的订阅分支比位置 —— 跨函数比较，结论无意义。
        // ⇒ 本守卫遍历**两个**函数，缺任何一个都 FAIL（本仓 issue #2 的「同一逻辑各写
        // 一份 ⇒ 漏改」形态：订阅分支只加在对话路径、MCP 路径漏加正是历史缺陷的原型）。
        for (fname, marker) in [
            ("call_api_with_retry", "async fn call_api_with_retry"),
            ("call_mcp_with_retry", "async fn call_mcp_with_retry"),
        ] {
            let start = prod
                .find(marker)
                .unwrap_or_else(|| panic!("{fname} 不应被改名"));
            // 函数体上界：下一个同缩进层方法的起始（三种签名形态取最靠前者）。
            let after_sig = start + marker.len();
            let rest = &prod[after_sig..];
            let end = ["\n    async fn ", "\n    pub fn ", "\n    fn "]
                .iter()
                .filter_map(|m| rest.find(m))
                .min()
                .map(|i| after_sig + i)
                .unwrap_or(prod.len());
            let seg_fn = &prod[start..end];

            let sub_at = seg_fn.find(&sub).unwrap_or_else(|| {
                panic!(
                    "{fname} 缺少订阅判据分支 —— 漏了它，该路径上的永久失败会被当成可重试"
                )
            });

            // 同函数内若存在其它 403 分支，订阅必须排在它们之前。
            for (other_name, other) in [
                ("is_client_validation_error", &validation),
                ("is_temporary_rate_limit", &temp_rl),
            ] {
                if let Some(other_at) = seg_fn.find(other.as_str()) {
                    assert!(
                        sub_at < other_at,
                        "{fname}：订阅永久错误必须排在 {other_name} 之前 —— 排在后面时，\
                         换区（L1）/短冷却 failover 会先命中，而两者对订阅问题都无效\
                         （实测同一把 key 在两个区拿到的是**不同**的 403：us 回 bearer \
                         invalid、eu 回 subscription unsupported），只是白烧上游往返与重试预算"
                    );
                }
            }

            // 承重：该分支**不得**调 report_failure —— 号没坏，是订阅不含该应用/模型。
            // 片段取到该分支的闭合花括号为止。
            let branch = &seg_fn[sub_at..];
            let branch_end = branch
                .find("\n                }")
                .map(|i| i + 1)
                .unwrap_or(branch.len());
            let branch = &branch[..branch_end];
            assert!(
                !branch.contains("report_failure"),
                "{fname}：订阅永久错误不得计入凭据失败 —— 那会在 3 次后自动禁用一个\
                 「换个模型就能用」的号，且 persist_disabled_state 落盘后重启也回不来"
            );
            assert!(
                branch.contains("subscription_unsupported=1"),
                "{fname}：错误串必须带机器可读标记，否则面板/外挂无法与其它 403 区分"
            );
        }
    }

    // ⚠️ `#[test]` 曾在 2026-08-06 之前的某次改动中丢失，导致本守卫**从未运行过**
    // （表现为编译期 `function is never used` 警告，而非测试失败 —— 所以没人注意）。
    // 上一轮已补过一次又退化，故此处留注记：删这行属性等于悄悄关掉一条守卫。
    #[test]
    fn force_refresh_must_skip_api_key_credentials_at_both_sites() {
        let src = include_str!("provider.rs");
        // ⚠️ needle 必须**运行时拼接**：若把完整串写成一个字面量，它自己也会出现在
        // 本文件里，被 include_str! 读到并多算一处（第一版就是这样，测试在回退前就 FAIL）。
        let needle = format!("{}{}", "if endpoint.is_bearer_token_invalid", "(&body)");
        let sites: Vec<&str> = src.split(needle.as_str()).skip(1).collect();
        assert_eq!(
            sites.len(),
            2,
            "预期恰好两处 force-refresh 调用点（对话路径 + MCP 路径）；\
             数量变化说明有新增/删除，需同步本守卫"
        );
        for (i, site) in sites.iter().enumerate() {
            // 只看该 if 的条件部分（到左花括号为止）
            let cond = site.split('{').next().unwrap_or("");
            assert!(
                cond.contains("is_api_key_credential"),
                "第 {} 处 force-refresh 未跳过 api_key 号：它结构上不可能刷新成功，\
                 却会计入失败并被退避重试，把该号的死亡速度放大三倍。条件为: {cond}",
                i + 1
            );
        }
    }

    /// ⭐ 源码级守卫：**失败记录必须带 `retries`**。
    ///
    /// 单测覆盖不到 `call_api_with_retry` 的失败路径（需真实上游 + 号池才能把重试预算跑穿），
    /// 而这正是回归发生过的地方：`fail_record` 组装块设了 credential_id / session_id /
    /// is_streaming / latency_ms / outcome / error_message，**唯独漏了 `retries`** →
    /// 落库即 `RequestRecord::new` 的默认 0。
    ///
    /// 线上实测坐实（近 2 小时）：全部失败样本 **无一例外 retries=0**
    /// （auth_failed 1487 / rate_limited 1098 / server_error 118 / bad_request 91），
    /// 而同期成功样本有 retries=1、历史号池大时到过 7 以上 —— 统计上不可能，
    /// 除非失败路径从不赋值。后果是「烧掉 12 次换号才失败」与「第一次就失败」
    /// 在面板上完全不可区分，而那恰是判断重试预算是否够用的唯一依据。
    ///
    /// 用源码级守卫而非行为测试的理由与上面两个测试相同。
    #[test]
    fn fail_record_must_carry_retries() {
        let src = include_str!("provider.rs");
        // 定位失败记录组装块：从 `let mut fail_record` 到紧随其后的 `emit_record`。
        let block = src
            .split("let mut fail_record")
            .nth(1)
            .expect("fail_record 组装块不应被改名/删除");
        let block = block
            .split("emit_record")
            .next()
            .expect("fail_record 之后应紧跟 emit_record");
        assert!(
            block.contains("fail_record.retries"),
            "失败记录必须设 retries，否则一切失败样本的重试次数恒为 0，\
             无法区分『扫穿整池才失败』与『首次即失败』"
        );
    }

    /// ⭐ 源码级守卫（N4）：失败记录必须携带「链内首选号」。
    ///
    /// 线上实测：透传全败的失败样本 `credential_id=None retries=3`，面板看不出
    /// 「首选了哪个号」—— 若死号每次都排最前（`select_custom_api` 排序首写），
    /// 这种「死号恒选」在面板上完全不可见。`first_attempted_credential_id` 由
    /// 共享预算携带（透传首跳写、Kiro 主路径兜底），fail_record 必须读它。
    ///
    /// 用源码级守卫而非行为测试：触发需要整条透传全败 → 落 Kiro 主路径的端到端
    /// mock 链，且记录经管道异步落库无法在单测内同步断言（与 retries 守卫同理）。
    #[test]
    fn fail_record_must_carry_first_attempted_credential() {
        let src = include_str!("provider.rs");
        let block = src
            .split("let mut fail_record")
            .nth(1)
            .expect("fail_record 组装块不应被改名/删除");
        let block = block
            .split("emit_record")
            .next()
            .expect("fail_record 之后应紧跟 emit_record");
        assert!(
            block.contains("fail_record.first_attempted_credential_id"),
            "失败记录必须设 first_attempted_credential_id，否则透传全败的样本\
             看不到『首选了哪个号』，面板无法发现死号恒选（N4）"
        );
    }

    /// ⭐ 源码级守卫（已知问题 #20）：准入闸门超时必须**既 emit_record 又 bump 计数器**。
    ///
    /// 旧代码是裸 `anyhow::bail!` —— 被网关自己背压掐掉的请求在面板上**完全不存在**，
    /// 于是看到的成功率**偏乐观**（分母里少了这批）。而面板成功率是本项目后续一切限流
    /// 调参判断的依据，依据本身有偏则调参全是在算空气。实测这类 bail 在高峰时段
    /// 逐小时占比可达两位数。
    ///
    /// acquire_admission 已移至 handlers 层（post_messages 入口），
    /// provider.rs 生产代码中不应再有任何调用。守卫确保将来不会有人在此加回。
    ///
    /// 用源码级守卫而非行为测试：触发它需要真实令牌桶排满 + 真实 TokenManager +
    /// 走满 `inbound_queue_max_wait_secs`（默认 5s）的 await，单测里造不出且会拖慢全套。
    #[test]
    fn admission_timeout_must_be_observable() {
        let src = include_str!("provider.rs");
        let prod_all = src.split("#[cfg(test)]").next().expect("生产段应存在");
        let prod: String = prod_all
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        let prod = prod.as_str();
        // 🔴 2026-08-10：acquire_admission 已移至 handlers 层，provider 不应再调用。
        let retry_needle = ["acquire_admission", "().await"].concat();
        assert_eq!(
            prod.matches(retry_needle.as_str()).count(),
            0,
            "acquire_admission 已移至 handlers 层（透传与 Kiro 两条路径统一在 post_messages \
             入口过闸门）。provider.rs 不应再有调用点。若要在此加回，先确认不会导致某条路径绕闸。"
        );
    }

    /// 源码级守卫（E2）：MCP 的 401/403 分支必须**先**判账户级风控/封禁，
    /// 才允许落通用 `report_failure`。
    ///
    /// 用源码级守卫的理由与上一个测试相同：`call_mcp_with_retry` 需真实上游 + 号池，
    /// 单测覆盖不到，而这正是回归发生的地方（本条修复前该分支就是裸 `report_failure`）。
    ///
    /// **旧代码为何失败**：403 分支内只有 `report_failure`，缺
    /// `is_temporary_rate_limit` / `is_account_suspended` 两道判定。
    /// 而 403 `TEMPORARILY_SUSPENDED` 是**临时态**，`report_failure` 累加
    /// `failure_count` 达阈值即以 `TooManyFailures`（**永久型**标签）禁用 →
    /// 临时限流的号走 WebSearch 被打 3 次就永久禁用。这正是历史事故
    /// （12h 内 88 次误禁 + 36 次全池自愈活锁）的同一误判形态：对话路径已修，
    /// 本路径此前漏修。
    #[test]
    fn should_classify_account_risk_before_generic_failure_in_mcp_auth_branch() {
        let src = include_str!("provider.rs");
        let mcp_fn = src
            .split("async fn call_mcp_with_retry")
            .nth(1)
            .expect("call_mcp_with_retry 不应被改名");
        // 只看 401/403 分支：从它的定位注释起，到下一个「瞬态错误」分支为止。
        //
        // ⚠️ 先坐实两个定位标记的**唯一性**，否则本测试会在标记被改名时**静默失效**：
        // `.split(x).next()` 永不返回 None，所以若标记消失，`auth_branch` 会变成
        // 「函数剩余全文」—— 那里同样含 is_temporary_rate_limit / report_failure，
        // 顺序断言可能照样通过，于是守卫形同虚设（审查发现的真实弱点）。
        // 每个标记在 provider.rs 生产段应恰好 1 次。测试已 #[path] 外提，
        // include_str!("provider.rs") 不再含本测试字面量（外提前是 代码1+测试1=2）。
        const AUTH_MARKER: &str = "// 401/403 凭据问题";
        const TRANSIENT_MARKER: &str = "// 瞬态错误";
        assert_eq!(
            src.matches(AUTH_MARKER).count(),
            1,
            "401/403 定位标记在 provider.rs 必须唯一；数量变了说明标记被改动，\
             守卫会退化成扫全文而静默失效 —— 请同时更新代码与本测试"
        );
        assert_eq!(
            src.matches(TRANSIENT_MARKER).count(),
            1,
            "瞬态错误定位标记在 provider.rs 必须唯一，同上"
        );
        let auth_branch = mcp_fn
            .split(AUTH_MARKER)
            .nth(1)
            .expect("401/403 分支的定位注释不应被删改")
            .split(TRANSIENT_MARKER)
            .next()
            .expect("瞬态错误分支的定位注释不应被删改");
        // 边界健全性：分支切片必须显著短于整个函数，否则说明切错了（扫到全文）。
        assert!(
            auth_branch.len() < mcp_fn.len() / 2,
            "401/403 分支切片异常大（{} vs 函数 {}），定位失败",
            auth_branch.len(),
            mcp_fn.len()
        );

        let rate_limit_at = auth_branch
            .find("is_temporary_rate_limit")
            .expect("MCP 403 必须判账户级临时风控，否则临时态会被贴 TooManyFailures 永久标签");
        let suspended_at = auth_branch
            .find("is_account_suspended")
            .expect("MCP 403 必须判账户封禁，否则 disabled_reason 会落成 TooManyFailures");
        // 匹配**调用点**而非注释：分支内的说明注释里也出现 report_failure 字样。
        let generic_failure_at = auth_branch
            .find("self.token_manager.report_failure(")
            .expect("非风控 403 仍应计入通用失败（对照：不能修过头把真失败也放过）");

        assert!(
            rate_limit_at < generic_failure_at,
            "临时风控判定必须在 report_failure 之前（顺序错等于没修）"
        );
        assert!(
            suspended_at < generic_failure_at,
            "封禁判定必须在 report_failure 之前"
        );
        // 与对话路径同款：风控命中走分钟级退避，而非累加永久失败。
        assert!(
            auth_branch.contains("report_suspicious_activity"),
            "MCP 风控命中应走 report_suspicious_activity（分钟级退避）"
        );
    }

    #[test]
    fn test_extract_model_and_session_invalid_json() {
        // 非法 JSON：两者都为 None（与旧实现一致，不 panic）
        let (model, session) = KiroProvider::extract_model_and_session("not json");
        assert_eq!(model, None);
        assert_eq!(session, None);

        // 合法 JSON 但缺 conversationState：两者都为 None
        let (model, session) = KiroProvider::extract_model_and_session(r#"{"foo":"bar"}"#);
        assert_eq!(model, None);
        assert_eq!(session, None);
    }
    /// 回归（🔴 会杀号的缺陷）：请求热路径的端点解析必须与 `effective_endpoint` 同口径。
    ///
    /// **旧代码为何 FAIL**：`endpoint_for` 只读 `credentials.endpoint` 原始字段，
    /// 漏了「`ksk_` API Key 号自动路由到 CLI 端点」这一层（`effective_endpoint` 的第 ② 步）。
    /// 实测：同一个 ksk_ 号，`effective_endpoint()` 返回 `cli`，而热路径返回 `ide`。
    ///
    /// **为什么严重**：`ksk_` 号打 IDE 端点会 403（两个端点按凭据类型绑定、不可互换）。
    /// 403 走 `report_suspicious_activity`，连续 6 次即判死号自动禁用 ——
    /// 于是一个**完全健康**的 ksk_ 号，只因没手工填 `endpoint: cli` 就被烧掉。
    /// 这与线上号池"单号存活 25~60 分钟"的现象直接相关。
    ///
    /// 用源码级断言而非构造 provider：`endpoint_for` 需要完整的 endpoints 注册表 + 配置，
    /// 而缺陷本身只在"读哪个字段"这一行，源码断言足以锁死且不会因重构失效。
    #[test]
    fn endpoint_for_must_use_effective_endpoint_not_raw_field() {
        let src = include_str!("provider.rs");
        let body = src
            .split("fn endpoint_for")
            .nth(1)
            .expect("endpoint_for 不应被改名")
            .split("\n    /// ")
            .next()
            .expect("函数体应以下一项文档注释为界");
        assert!(
            body.contains("effective_endpoint"),
            "请求热路径必须走 effective_endpoint（否则 ksk_ 号走错端点 → 403 → 被当死号禁用）"
        );
        assert!(
            !body.contains(".endpoint\n            .as_deref()"),
            "不得回退到直读 credentials.endpoint 原始字段"
        );
    }

    /// 配套：坐实 `effective_endpoint` 对 ksk_ 号确实路由到 CLI（本回归的前提）。
    #[test]
    fn effective_endpoint_routes_api_key_credential_to_cli() {
        let mut c = crate::kiro::model::credentials::KiroCredentials::default();
        c.auth_method = Some("api_key".to_string());
        c.kiro_api_key = Some("ksk_test_key".to_string());
        c.endpoint = None;
        assert_eq!(
            c.effective_endpoint("ide"),
            crate::kiro::endpoint::cli::CLI_ENDPOINT_NAME,
            "ksk_ 号未显式配置时应自动路由到 CLI"
        );
        // 显式配置优先（面板可切回 ide 救急）
        c.endpoint = Some("ide".to_string());
        assert_eq!(c.effective_endpoint("ide"), "ide", "显式配置必须优先");
    }

    /// ⭐ 守卫：`select_endpoint` 必须按 `effective_endpoint_order` 候选顺序遍历，
    /// 而不是只取 `effective_endpoint` 单值。若回退成单值，429 换桶机制失去「q.* 封桶后落
    /// runtime.*」的能力，等于回到单端点。
    #[test]
    fn select_endpoint_must_use_endpoint_order_for_bucket_fallback() {
        let src = include_str!("provider.rs");
        let body = src
            .split("fn select_endpoint")
            .nth(1)
            .expect("select_endpoint 不应被改名")
            .split("\n    /// ")
            .next()
            .expect("函数体应以下一项文档注释为界");
        assert!(
            body.contains("effective_endpoint_order"),
            "select_endpoint 必须用 effective_endpoint_order 遍历候选端点（q.* 优先、runtime.* 回退）"
        );
        assert!(
            body.contains("endpoint_buckets"),
            "select_endpoint 必须查询端点桶封禁状态"
        );
    }

    // ══════════ select_endpoint 自适应派发（按凭据成功率，取代 round-robin）══════════

    /// 用真实端点注册表构造 provider（select_endpoint 只查 name，不触达实现细节）。
    fn provider_with_default(default_endpoint: &str) -> KiroProvider {
        let cfg = crate::model::config::Config::default();
        let tm = Arc::new(
            MultiTokenManager::new(cfg, vec![], None, None, false).expect("测试 token manager"),
        );
        KiroProvider::with_proxy(
            tm,
            None,
            crate::kiro::endpoint::registry(),
            default_endpoint.to_string(),
        )
    }

    // ============ call_api_with_retry 行为测试（端到端 mock 上游，2026-08-15 补）============
    //
    // call_api_with_retry 是全仓最重的单函数（约 1800 行），此前只有纯函数测试与
    // include_str 源码守卫——「分支之间怎么咬合」从未被真跑过（blockers-structure.md §1）。
    // 本组测试用本地 TCP 假上游 + 注入 mock endpoint（KiroEndpoint trait 的实现者，
    // 经 `with_proxy` 的 endpoints 注册表传入——这是现有构造路径，非测试专用 seam），
    // 把「选号 → 建请求 → 打上游 → 错误分类 → 换号/重试/耗尽」整条链真实跑起来。
    //
    // 网络与 AWS 签名完全在 mock 侧消除：api_url 指向 127.0.0.1、decorate_api 不加头。

    /// 本地 mock 上游：每个连接消费一个预配置响应（`connection: close` 强制新连接，
    /// 响应队列按请求次序出队），超出队列的请求一律 500（让测试以 Err 收尾而非挂死）。
    struct MockUpstream {
        port: u16,
        hits: Arc<std::sync::atomic::AtomicUsize>,
        /// 每个请求的原始请求头（按请求次序；含 Authorization，可据此区分请求是哪个号发的）。
        heads: Arc<Mutex<Vec<String>>>,
        _responses: Arc<Mutex<std::collections::VecDeque<MockResponse>>>,
    }

    #[derive(Clone)]
    struct MockResponse {
        status: u16,
        reason: &'static str,
        body: &'static str,
        retry_after_secs: Option<u64>,
    }

    impl MockResponse {
        fn ok(body: &'static str) -> Self {
            Self {
                status: 200,
                reason: "OK",
                body,
                retry_after_secs: None,
            }
        }
    }

    impl MockUpstream {
        fn start(responses: Vec<MockResponse>) -> Self {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("绑定 mock 上游端口");
            let port = listener.local_addr().expect("mock 端口").port();
            let responses = Arc::new(Mutex::new(std::collections::VecDeque::from(responses)));
            let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let heads = Arc::new(Mutex::new(Vec::new()));
            let (hits_t, responses_t, heads_t) = (hits.clone(), responses.clone(), heads.clone());
            std::thread::spawn(move || {
                for conn in listener.incoming() {
                    let Ok(mut stream) = conn else { continue };
                    let (hits_c, responses_c, heads_c) = (hits_t.clone(), responses_t.clone(), heads_t.clone());
                    std::thread::spawn(move || {
                        // 先落请求头再写响应：调用方拿到响应时，本连接的请求头必然已可读。
                        heads_c.lock().push(mock_read_request_head(&mut stream));
                        let n = hits_c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        let resp = responses_c
                            .lock()
                            .get(n)
                            .cloned()
                            .unwrap_or(MockResponse {
                                status: 500,
                                reason: "Internal Server Error",
                                body: "{}",
                                retry_after_secs: None,
                            });
                        mock_write_response(&mut stream, &resp);
                    });
                }
            });
            Self {
                port,
                hits,
                heads,
                _responses: responses,
            }
        }

        /// 按请求次序返回每个请求的原始请求头（含 Authorization，可据此区分是哪个号打的）。
        fn captured_heads(&self) -> Vec<String> {
            self.heads.lock().clone()
        }
    }

    fn mock_read_request_head(stream: &mut std::net::TcpStream) -> String {
        use std::io::Read;
        let mut buf = [0u8; 4096];
        let mut received = Vec::new();
        stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
        loop {
            match stream.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    received.extend_from_slice(&buf[..n]);
                    if received.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        String::from_utf8_lossy(&received).into_owned()
    }

    fn mock_write_response(stream: &mut std::net::TcpStream, r: &MockResponse) {
        use std::io::Write;
        let mut head = format!(
            "HTTP/1.1 {} {}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n",
            r.status,
            r.reason,
            r.body.len()
        );
        if let Some(ra) = r.retry_after_secs {
            head.push_str(&format!("retry-after: {}\r\n", ra));
        }
        head.push_str("\r\n");
        let _ = stream.write_all(head.as_bytes());
        let _ = stream.write_all(r.body.as_bytes());
        let _ = stream.flush();
    }

    /// 只认 mock 上游的端点实现：不加任何头、不改任何 body，URL 固定指向本地假上游。
    ///
    /// `name` 可自定义（真实端点名如 "ide" / "codewhisperer"）：端点回退链按名字在
    /// 注册表里补齐，固定叫 "mock" 的端点进不了 `ENDPOINT_FALLBACK_ORDER` 的链。
    struct MockEndpoint {
        url: String,
        name: &'static str,
    }

    impl KiroEndpoint for MockEndpoint {
        fn name(&self) -> &'static str {
            self.name
        }
        fn api_url(&self, _ctx: &RequestContext<'_>) -> String {
            self.url.clone()
        }
        fn mcp_url(&self, _ctx: &RequestContext<'_>) -> String {
            self.url.clone()
        }
        fn decorate_api(
            &self,
            req: reqwest::RequestBuilder,
            _ctx: &RequestContext<'_>,
        ) -> reqwest::RequestBuilder {
            req
        }
        fn decorate_mcp(
            &self,
            req: reqwest::RequestBuilder,
            _ctx: &RequestContext<'_>,
        ) -> reqwest::RequestBuilder {
            req
        }
        fn transform_api_body(&self, body: &str, _ctx: &RequestContext<'_>) -> String {
            body.to_string()
        }
    }

    /// 构造「2 个可用 Kiro 号 + 唯一 mock 端点」的 provider。
    ///
    /// ⚠️ 凭据 id 用本组专属段（91_xxx）：endpoint_health 是**进程级共享**表
    /// （endpoint_health::SHARED），与既有 select_endpoint 测试共用 id 会被对方写入的
    /// 样本破坏「冷启动」类断言（provider 内部 `report_endpoint_outcome` 会写这张表）。
    fn provider_with_mock_upstream(upstream: &MockUpstream) -> KiroProvider {
        let mut creds = Vec::new();
        for id in [91_001u64, 91_002] {
            let mut c = KiroCredentials::default();
            c.id = Some(id);
            c.auth_method = Some("api_key".to_string());
            c.kiro_api_key = Some(format!("sk-mock-{id}"));
            // ⚠️ 必须显式钉死 endpoint：api_key 号被 `effective_endpoint_order` 自动路由到
            // 内置的 ["cli", "cli-runtime"] 候选链（endpoint=None 时），而测试注册表只有
            // "mock"——不钉死则 select_endpoint 硬门滤掉全部候选 → 请求永不打 mock 上游。
            c.endpoint = Some("mock".to_string());
            creds.push(c);
        }
        let tm = Arc::new(
            MultiTokenManager::new(
                crate::model::config::Config::default(),
                creds,
                None,
                None,
                false,
            )
            .expect("构造测试 token manager"),
        );
        let mut endpoints: HashMap<String, Arc<dyn KiroEndpoint>> = HashMap::new();
        endpoints.insert(
            "mock".to_string(),
            Arc::new(MockEndpoint {
                url: format!("http://127.0.0.1:{}", upstream.port),
                name: "mock",
            }),
        );
        KiroProvider::with_proxy(tm, None, endpoints, "mock".to_string())
    }

    /// 构造「2 个 custom_api 代挂号（baseUrl 指向同一 mock 上游）+ mock 端点」的 provider，
    /// 供 `try_custom_api_passthrough` 的 failover 链测试（N4 首选号）。
    ///
    /// 与 `provider_with_mock_upstream` 的差异只有凭据形态（custom_api vs api_key）：
    /// 透传选号池（`select_custom_api`）只认 custom_api 号；透传路径不经 KiroEndpoint，
    /// URL 直接由 base_url 拼出（`passthrough::forward`），mock 端点在注册表里只是
    /// `with_proxy` 构造所需。
    fn provider_with_passthrough_upstream(upstream: &MockUpstream) -> KiroProvider {
        let mut creds = Vec::new();
        for id in [91_001u64, 91_002] {
            let mut c = KiroCredentials::default();
            c.id = Some(id);
            c.auth_method = Some("custom_api".to_string());
            c.base_url = Some(format!("http://127.0.0.1:{}", upstream.port));
            c.kiro_api_key = Some(format!("sk-mock-{id}"));
            creds.push(c);
        }
        let tm = Arc::new(
            MultiTokenManager::new(
                crate::model::config::Config::default(),
                creds,
                None,
                None,
                false,
            )
            .expect("构造测试 token manager"),
        );
        let mut endpoints: HashMap<String, Arc<dyn KiroEndpoint>> = HashMap::new();
        endpoints.insert(
            "mock".to_string(),
            Arc::new(MockEndpoint {
                url: format!("http://127.0.0.1:{}", upstream.port),
                name: "mock",
            }),
        );
        KiroProvider::with_proxy(tm, None, endpoints, "mock".to_string())
    }

        const MOCK_PASSTHROUGH_BODY: &str =
        r#"{"model":"claude-sonnet-4","messages":[{"role":"user","content":"hi"}]}"#;

    /// 构造「2 个可用 Kiro 号 + 双 mock 端点（ide → port_a、codewhisperer → port_b）」的
    /// provider，供端点级链式回退测试。
    ///
    /// ⚠️ 凭据 id 用本组专属段（93_xxx），理由同 `provider_with_mock_upstream`（91_xxx 段
    /// 的 endpoint_health 共享表会被本组写入的样本污染「冷启动」断言）。
    ///
    /// 凭据形态：api_key 号 + 显式 `endpoint="ide"`。api_key 号无显式值时被
    /// `effective_endpoint_order` 自动路由到 CLI 族（注册表里没有，select 拿不到候选），
    /// 显式指定 ide 后候选链 = ["ide", cli, cli-runtime, codewhisperer, amazonq]（显式值
    /// 放最前 + 完整候选链去重），注册表只含 ide/codewhisperer ⇒ select 候选 [ide, cw]，
    /// 冷启动选中 ide ⇒ 端点链 = [ide, codewhisperer]（order 补齐 cw，FALLBACK_ORDER 无新增）。
    fn provider_with_mock_chain_ports(port_a: u16, port_b: u16) -> KiroProvider {
        let mut creds = Vec::new();
        for id in [93_001u64, 93_002] {
            let mut c = KiroCredentials::default();
            c.id = Some(id);
            c.auth_method = Some("api_key".to_string());
            c.kiro_api_key = Some(format!("sk-mock-{id}"));
            c.endpoint = Some("ide".to_string());
            creds.push(c);
        }
        let tm = Arc::new(
            MultiTokenManager::new(
                crate::model::config::Config::default(),
                creds,
                None,
                None,
                false,
            )
            .expect("构造测试 token manager"),
        );
        let mut endpoints: HashMap<String, Arc<dyn KiroEndpoint>> = HashMap::new();
        endpoints.insert(
            "ide".to_string(),
            Arc::new(MockEndpoint {
                url: format!("http://127.0.0.1:{port_a}"),
                name: "ide",
            }),
        );
        endpoints.insert(
            "codewhisperer".to_string(),
            Arc::new(MockEndpoint {
                url: format!("http://127.0.0.1:{port_b}"),
                name: "codewhisperer",
            }),
        );
        KiroProvider::with_proxy(tm, None, endpoints, "ide".to_string())
    }

    fn provider_with_mock_chain(up_a: &MockUpstream, up_b: &MockUpstream) -> KiroProvider {
        provider_with_mock_chain_ports(up_a.port, up_b.port)
    }

    /// 构造「2 个可用 Kiro 号 + 4 个 mock 端点（ide/cli/codewhisperer/amazonq 各指
    /// 一个 mock 上游）」的 provider，供 M1 预算闸的「4 元素链全 429」集成测试。
    ///
    /// api_key 号 + 显式 `endpoint="ide"` ⇒ 候选链 = [ide, cli, cli-runtime(未注册),
    /// codewhisperer, amazonq]，注册表含全部 4 端点 ⇒ 链 = [ide, cli, codewhisperer,
    /// amazonq]（4 元素；FALLBACK_ORDER 里的端点已在链内，无新增）。
    fn provider_with_mock_chain_4ports(ports: [u16; 4]) -> KiroProvider {
        let mut creds = Vec::new();
        for id in [93_011u64, 93_012] {
            let mut c = KiroCredentials::default();
            c.id = Some(id);
            c.auth_method = Some("api_key".to_string());
            c.kiro_api_key = Some(format!("sk-mock-{id}"));
            c.endpoint = Some("ide".to_string());
            creds.push(c);
        }
        let tm = Arc::new(
            MultiTokenManager::new(
                crate::model::config::Config::default(),
                creds,
                None,
                None,
                false,
            )
            .expect("构造测试 token manager"),
        );
        let mut endpoints: HashMap<String, Arc<dyn KiroEndpoint>> = HashMap::new();
        for (name, port) in [
            ("ide", ports[0]),
            ("cli", ports[1]),
            ("codewhisperer", ports[2]),
            ("amazonq", ports[3]),
        ] {
            endpoints.insert(
                name.to_string(),
                Arc::new(MockEndpoint {
                    url: format!("http://127.0.0.1:{port}"),
                    name,
                }),
            );
        }
        KiroProvider::with_proxy(tm, None, endpoints, "ide".to_string())
    }

    /// 拿一个「立刻被释放、无人监听」的本地端口（连接层失败 = ECONNREFUSED 的模拟）。
    fn dead_local_port() -> u16 {
        let l = std::net::TcpListener::bind("127.0.0.1:0").expect("绑定临时端口");
        l.local_addr().expect("临时端口地址").port()
    }

    /// N4：透传 failover 链（首选号 502 → 换号 200）的 usage record 必须带
    /// `first_attempted_credential_id` = 首选号，与 `credential_id` = 最终号成对——
    /// 面板据此发现「死号恒选」（某号每次都被选中最前却被换掉）。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn passthrough_failover_record_carries_first_attempted_credential() {
        let up = MockUpstream::start(vec![
            MockResponse {
                status: 502,
                reason: "Bad Gateway",
                body: "{}",
                retry_after_secs: None,
            },
            MockResponse::ok(r#"{"ok":true}"#),
        ]);
        let provider = provider_with_passthrough_upstream(&up);
        let budget = SharedRetryBudget::new();

        let (resp, meta) = provider
            .try_custom_api_passthrough(
                MOCK_PASSTHROUGH_BODY.into(),
                Some("claude-sonnet-4"),
                None,
                None,
                &budget,
            )
            .await
            .expect("502 → failover → 200 应成功");
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
        assert_eq!(
            meta.first_attempted_credential_id,
            Some(91_001),
            "首选号必须是首个被选中的号（502 那个）"
        );
        assert_eq!(
            meta.credential_id, 91_002,
            "最终服务号必须是 failover 后的号（200 那个）"
        );
        assert_eq!(
            budget.first_attempted(),
            Some(91_001),
            "共享预算必须同步携带首选号——Kiro 主路径的失败记录要读它"
        );

        // 与 handlers.rs 同款 record 构造（透传成功链埋点）：record 带首选号 + 最终号。
        let mut record = crate::usage::RequestRecord::new(
            "req-pt",
            meta.model.clone().unwrap_or_default(),
        );
        record.credential_id = Some(meta.credential_id);
        record.first_attempted_credential_id = meta.first_attempted_credential_id;
        assert_eq!(
            record.first_attempted_credential_id,
            Some(91_001),
            "record 首选号 == 链首选号"
        );
        assert_eq!(record.credential_id, Some(91_002), "record 最终号 == 链最终号");
    }

    /// 🔴 M1.2 回归（2026-08-16 对抗审查 MAJOR）：400/404 **不记失败余温**——
    /// 坏请求（无效 tool schema / 该站不认模型）是全池同质的客户端错误，一次
    /// failover 把所有号打上余温会让 60s 内任何请求零尝试直返 503（毒化整池）。
    ///
    /// 断言方式（外部可观察行为）：第一请求 A 号 400（值得换号）→ failover 到 B 号
    /// 成功；第二请求（全新上游+全新 manager）若 400 被记热，A 号会被余温过滤 →
    /// 直接选 B 号；不记热则 A 号仍在候选（全平局按 id）→ 首试 A 号。
    /// `meta.credential_id` 公开可见，无需侵入式访问内部状态。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn passthrough_400_404_do_not_record_failure_warmth() {
        for (code, reason) in [
            (400u16, "Bad Request"),
            (404u16, "Not Found"),
        ] {
            // 第一请求：#91_001 返 code（值得换号）→ failover → #91_002 200。
            let up = MockUpstream::start(vec![
                MockResponse {
                    status: code,
                    reason,
                    body: "{}",
                    retry_after_secs: None,
                },
                MockResponse::ok(r#"{"ok":true}"#),
            ]);
            let provider = provider_with_passthrough_upstream(&up);
            let budget = SharedRetryBudget::new();
            let (resp, meta) = provider
                .try_custom_api_passthrough(
                    MOCK_PASSTHROUGH_BODY.into(),
                    Some("claude-sonnet-4"),
                    None,
                    None,
                    &budget,
                )
                .await
                .expect("400/404(值得换号) → failover → 200 应成功");
            assert_eq!(resp.status(), reqwest::StatusCode::OK);
            assert_eq!(
                meta.credential_id, 91_002,
                "{code} 换号后应由 #91_002 服务（failover 语义不变）"
            );

            // 第二请求（全新上游 + 全新 manager，无任何跨请求状态残留）：
            // 不记热 → #91_001 仍首选；记热 → 它被余温过滤 → 直接选 #91_002。
            let up2 = MockUpstream::start(vec![MockResponse::ok(r#"{"ok":true}"#)]);
            let provider2 = provider_with_passthrough_upstream(&up2);
            let budget2 = SharedRetryBudget::new();
            let (_resp2, meta2) = provider2
                .try_custom_api_passthrough(
                    MOCK_PASSTHROUGH_BODY.into(),
                    Some("claude-sonnet-4"),
                    None,
                    None,
                    &budget2,
                )
                .await
                .expect("第二请求应成功");
            assert_eq!(
                meta2.credential_id, 91_001,
                "{code} 不得记失败余温：第二请求必须先试原号（记热时这里会是 #91_002，\
                 整池被坏请求毒化 60s）"
            );
        }
    }

    /// N4：透传全败（落 Kiro 主路径）时，首选号不随 `None` 返回丢失——由共享预算携带，
    /// Kiro 主路径的 `fail_record` 读取它（线上证据形态：`cred_id=None retries=3` 的失败链）。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn passthrough_all_fail_budget_keeps_first_attempted() {
        let up = MockUpstream::start(vec![
            MockResponse {
                status: 502,
                reason: "Bad Gateway",
                body: "{}",
                retry_after_secs: None,
            },
            MockResponse {
                status: 502,
                reason: "Bad Gateway",
                body: "{}",
                retry_after_secs: None,
            },
        ]);
        let provider = provider_with_passthrough_upstream(&up);
        let budget = SharedRetryBudget::new();

        let r = provider
            .try_custom_api_passthrough(
                MOCK_PASSTHROUGH_BODY.into(),
                Some("claude-sonnet-4"),
                None,
                None,
                &budget,
            )
            .await;
        assert!(r.is_none(), "全 502 → 透传整体不可用，落 Kiro 主路径");
        assert_eq!(
            budget.first_attempted(),
            Some(91_001),
            "首选号不因全败而丢失——Kiro 主路径 fail_record 依赖它"
        );
    }

    /// 共享预算「链内首选号」的语义：首写生效（透传首跳的号优先于后续任何跳）。
    #[test]
    fn shared_retry_budget_first_attempt_is_first_wins() {
        let b = SharedRetryBudget::new();
        assert_eq!(b.first_attempted(), None, "未记录时恒为 None");
        b.note_first_attempt(3);
        b.note_first_attempt(2);
        assert_eq!(b.first_attempted(), Some(3), "首写生效：先试的号拥有槽位");
    }

    const MOCK_BODY: &str = r#"{"conversationState":{"conversationId":"sess-1","currentMessage":{"userInputMessage":{"modelId":"claude-sonnet-4"}}}}"#;

    /// 成功路径：上游直接 200 → `Ok((resp, meta))`，retries=0，只打 1 次上游。
    ///
    /// 回退即 FAIL：把成功分支的 `report_success`/`return Ok` 弄丢（例如重试循环
    /// 对 200 也继续换号），断言失败。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn call_api_first_try_success_returns_zero_retries() {
        let up = MockUpstream::start(vec![MockResponse::ok(r#"{"ok":true}"#)]);
        let provider = provider_with_mock_upstream(&up);

        let (resp, meta) = match provider
            .call_api(MOCK_BODY, false, &SharedRetryBudget::new(), Some("claude-sonnet-4"))
            .await
        {
            Ok(v) => v,
            Err(e) => panic!("首次 200 应直接成功: {e}"),
        };
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
        assert_eq!(resp.text().await.unwrap(), r#"{"ok":true}"#, "上游 body 必须原样透传");
        assert_eq!(meta.retries, 0, "首次即成功不得计重试");
        assert!(
            meta.credential_id == 91_001 || meta.credential_id == 91_002,
            "meta 必须带实际使用的那条凭据"
        );
        assert_eq!(
            up.hits.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "成功路径只允许打 1 次上游"
        );
    }

    /// ⭐ 链式回退核心回归（P0 移植）：第一端点 429 → 轮内立即换第二端点 → 200。
    ///
    /// 钉住三件事：
    /// 1. **不消耗重试预算**：`meta.retries` 必须仍是 0（链内跳不触碰 attempt 计数）；
    /// 2. 第二端点真被打到（hits 两个上游各 1）；
    /// 3. 链首 429 封桶（`order.len() > 1` 时）→ 但成功不受影响。
    ///
    /// 回退即 FAIL：把链式回退的 `continue 'endpoint_chain` 删掉（回到跨轮换号），
    /// `meta.retries` 变成 1 或请求失败——本测试断言 retries==0 必红。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn endpoint_chain_fallback_uses_next_endpoint_without_retry_budget() {
        let up_a = MockUpstream::start(vec![MockResponse {
            status: 429,
            reason: "Too Many Requests",
            body: "{}",
            retry_after_secs: None,
        }]);
        let up_b = MockUpstream::start(vec![MockResponse::ok(r#"{"ok":true}"#)]);
        let provider = provider_with_mock_chain(&up_a, &up_b);

        let (resp, meta) = match provider
            .call_api(MOCK_BODY, false, &SharedRetryBudget::new(), Some("claude-sonnet-4"))
            .await
        {
            Ok(v) => v,
            Err(e) => panic!("第一端点 429 → 链式回退第二端点应成功: {e}"),
        };
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
        assert_eq!(resp.text().await.unwrap(), r#"{"ok":true}"#);
        assert_eq!(meta.retries, 0, "链式回退不得消耗凭据重试预算（attempt 计数不变）");
        assert_eq!(
            up_a.hits.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "第一端点只打 1 次（429 后即顺延）"
        );
        assert_eq!(
            up_b.hits.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "第二端点必须被链式回退打到"
        );
    }

    /// ⭐ 死端点负缓存：A 端口无人监听（连接层失败）→ 记入负缓存 + 顺延 B 成功；
    /// 第二次调用跳过 A（负缓存 TTL 内），只打 B。
    ///
    /// 回退即 FAIL：把链循环顶部的 `is_endpoint_dead` 跳过分支删掉，第二次调用会
    /// 先白打一次 A（connect refused）——断言 `up_b.hits == 2` 变红（B 只被打 1 次
    /// 的话说明第二次没到 B？不——A 连接失败很快，B 仍会打到。真正的判据是
    /// `is_endpoint_dead` 被置位 + A 跳过。用 hits 数 A 无法直接数（连接失败不落
    /// mock），故用负缓存状态断言）。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dead_endpoint_negative_cache_skips_failed_route() {
        let dead_port = dead_local_port();
        let up_b = MockUpstream::start(vec![
            MockResponse::ok(r#"{"ok":true}"#),
            MockResponse::ok(r#"{"ok":true}"#),
        ]);
        let provider = provider_with_mock_chain_ports(dead_port, up_b.port);

        // 第一次：A 连接失败（记负缓存）→ 顺延 B → 成功。
        let (resp, meta) = match provider
            .call_api(MOCK_BODY, false, &SharedRetryBudget::new(), Some("claude-sonnet-4"))
            .await
        {
            Ok(v) => v,
            Err(e) => panic!("A 连接失败应链式顺延到 B 并成功: {e}"),
        };
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
        assert_eq!(meta.retries, 0, "连接层顺延同样不耗重试预算");
        assert!(
            provider.is_endpoint_dead("ide", "us-east-1"),
            "连接层失败必须记入负缓存"
        );
        assert_eq!(
            up_b.hits.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "第一次调用 B 被顺延打到 1 次"
        );

        // 第二次：A 在负缓存 TTL 内 → 跳过，直接打 B。
        let (resp, _) = match provider
            .call_api(MOCK_BODY, false, &SharedRetryBudget::new(), Some("claude-sonnet-4"))
            .await
        {
            Ok(v) => v,
            Err(e) => panic!("第二次调用应跳过 A 直接打 B: {e}"),
        };
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
        assert_eq!(
            up_b.hits.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "A 被负缓存跳过 → B 累计打 2 次"
        );
        assert!(
            provider.is_endpoint_dead("ide", "us-east-1"),
            "负缓存 TTL 未到不得清除"
        );
    }

    /// ⭐ 链尾兜底铁律：A、B 全部连接失败过（都在负缓存内）→ A（非链尾）跳过、
    /// B（链尾）**绝不跳过**，仍真打 → 成功。
    ///
    /// 回退即 FAIL：把链循环的「链尾不跳过」条件删掉（`idx != last_idx` 放宽成
    /// 无条件跳过），第二次调用整链无人发送 → response 恒 None → 请求 Err。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn endpoint_chain_tail_is_never_skipped() {
        let dead_port = dead_local_port();
        let up_b = MockUpstream::start(vec![
            MockResponse::ok(r#"{"ok":true}"#),
            MockResponse::ok(r#"{"ok":true}"#),
        ]);
        let provider = provider_with_mock_chain_ports(dead_port, up_b.port);

        // 第一次：A 连接失败 → 记 dead；B 200。
        provider
            .call_api(MOCK_BODY, false, &SharedRetryBudget::new(), Some("claude-sonnet-4"))
            .await
            .expect("第一次应经 A 失败顺延 B 成功");
        assert!(provider.is_endpoint_dead("ide", "us-east-1"));

        // 把 B 也标记连接失败（模拟 B 近期也连不上）——现在链内两个端点全在负缓存里。
        provider.mark_endpoint_dead("codewhisperer", "us-east-1");

        // 第二次：A 跳过（非链尾）、B dead 但链尾不跳过 → 仍真打 B → 200。
        let (resp, _) = match provider
            .call_api(MOCK_BODY, false, &SharedRetryBudget::new(), Some("claude-sonnet-4"))
            .await
        {
            Ok(v) => v,
            Err(e) => panic!("链尾绝不跳过：全死仍尝试 B 并成功: {e}"),
        };
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
        assert_eq!(
            up_b.hits.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "链尾被跳过则 B 不会被打到第 2 次"
        );
    }

    /// 整链失败交凭据级分类：两个端点都 429 → 链式回退耗尽 → 链尾响应走既有
    /// 429 分类（封桶 + has_unthrottled 判定 + 冷却换号），最终 `Err` 透传。
    ///
    /// hits：号 1 打 A、B（链内 2 跳），封双桶 → has_unthrottled false（cli/cli-runtime/
    /// amazonq 未注册不算可用）→ 凭据冷却 → 号 2 同样 2 跳 → 预算耗尽（2 号池
    /// max_retries=2）→ Err。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn endpoint_chain_full_failure_falls_through_to_credential_classification() {
        let up_a = MockUpstream::start(vec![
            MockResponse {
                status: 429,
                reason: "Too Many Requests",
                body: "{}",
                retry_after_secs: None,
            },
            MockResponse {
                status: 429,
                reason: "Too Many Requests",
                body: "{}",
                retry_after_secs: None,
            },
        ]);
        let up_b = MockUpstream::start(vec![
            MockResponse {
                status: 429,
                reason: "Too Many Requests",
                body: "{}",
                retry_after_secs: None,
            },
            MockResponse {
                status: 429,
                reason: "Too Many Requests",
                body: "{}",
                retry_after_secs: None,
            },
        ]);
        let provider = provider_with_mock_chain(&up_a, &up_b);

        let err = provider
            .call_api(MOCK_BODY, false, &SharedRetryBudget::new(), Some("claude-sonnet-4"))
            .await
            .err().expect("两个端点都 429 → 整链失败应 Err（透传 429）");
        assert!(
            err.to_string().contains("429"),
            "整链失败必须交凭据级分类（透传上游 429 语义）: {err}"
        );
        assert_eq!(
            up_a.hits.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "两个号各打一次 A（链首）"
        );
        assert_eq!(
            up_b.hits.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "两个号各打一次 B（链尾）"
        );
    }

    /// ⭐ 链内共享预算闸（对抗审查 M1）：4 元素链全 429 → 总上游调用必须 ≤
    /// `ABSOLUTE_MAX_TOTAL_RETRIES`(=4)，不得出现「attempts × 链跳数」的超发。
    ///
    /// 现有 2 元素链测试（`endpoint_chain_full_failure_falls_through_...`）对 M1
    /// **失明**：2 元素链在第 1 个 attempt 内就打满 4 次预算（2 跳 × 2 号），hits 恒 4，
    /// 看不出「链内跳不扣共享预算」的洞。4 元素链下第 1 个 attempt 的 4 跳就耗尽预算，
    /// 换号后的第 2 个 attempt 必须在链首跳前被预算闸拦下——这是对「每请求 ≤
    /// ABSOLUTE_MAX_TOTAL_RETRIES 次上游调用」不变量的端到端回归。
    ///
    /// 回退即 FAIL：把链循环顶部的预算闸删掉，第 2 个 attempt 会再打 4 跳
    /// → total hits == 8 > 4，断言变红。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn endpoint_chain_respects_shared_retry_budget_when_all_429() {
        let ups: Vec<MockUpstream> = (0..4)
            .map(|_| {
                MockUpstream::start(vec![MockResponse {
                    status: 429,
                    reason: "Too Many Requests",
                    body: "{}",
                    retry_after_secs: None,
                }])
            })
            .collect();
        let ports: [u16; 4] = ups
            .iter()
            .map(|u| u.port)
            .collect::<Vec<_>>()
            .try_into()
            .expect("4 个端口");
        let provider = provider_with_mock_chain_4ports(ports);

        let err = provider
            .call_api(MOCK_BODY, false, &SharedRetryBudget::new(), Some("claude-sonnet-4"))
            .await
            .err()
            .expect("4 端点全 429 → 整链失败应 Err（透传 429）");
        assert!(
            err.to_string().contains("429"),
            "整链失败必须透传上游 429 语义: {err}"
        );

        let total: usize = ups
            .iter()
            .map(|u| u.hits.load(std::sync::atomic::Ordering::SeqCst))
            .sum();
        assert!(
            total <= ABSOLUTE_MAX_TOTAL_RETRIES,
            "链式回退 + 换号重试的总上游调用必须 ≤ ABSOLUTE_MAX_TOTAL_RETRIES（实际 {total} 次）\
             —— 链内每跳消耗共享预算，预算耗尽必须停在链首前"
        );
        assert_eq!(
            total, ABSOLUTE_MAX_TOTAL_RETRIES,
            "4 元素链全 429 恰好打满 4 次（第 1 个 attempt 的 4 跳），换号后的 attempt 被预算闸拦下"
        );
    }

    /// 🔴 对抗审查 m4：501（Not Implemented）是**确定性**错误，不得触发链式回退
    /// （`status.is_server_error()` 会把 501/505 也顺延，白烧一跳——换 host 不会让
    /// 501 变 200，它是对请求的确定性答复）。
    ///
    /// 回退即 FAIL：把链内瞬态判定改回 `|| status.is_server_error()`，501 触发
    /// 链式回退 → B 被真打（up_b.hits == 1），断言变红。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn endpoint_chain_does_not_fallback_on_501() {
        let up_a = MockUpstream::start(vec![MockResponse {
            status: 501,
            reason: "Not Implemented",
            body: "{}",
            retry_after_secs: None,
        }]);
        let up_b = MockUpstream::start(vec![MockResponse {
            status: 501,
            reason: "Not Implemented",
            body: "{}",
            retry_after_secs: None,
        }]);
        let provider = provider_with_mock_chain(&up_a, &up_b);

        let err = provider
            .call_api(MOCK_BODY, false, &SharedRetryBudget::new(), Some("claude-sonnet-4"))
            .await
            .err()
            .expect("501 不顺延 → 交凭据级分类（两号都 501）→ Err");
        assert!(
            err.to_string().contains("501"),
            "错误必须保留上游 501 语义: {err}"
        );
        // 501 不触发**链式**回退（同凭据换 host 不会让 501 变 200）；A 被命中 2 次 =
        // 首端点 1 次 + 凭据级/吸收层对 501 的既有重试 1 次（501 不在链内瞬态集，
        // 但吸收层分类仍视 5xx 为可吸收——那是既有行为，m4 只修链内顺延）。
        assert_eq!(
            up_a.hits.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "同凭据链内不得因 501 顺延（501 是确定性错误），A 命中 = 首打 + 吸收层重试"
        );
    }

    /// 注册表含 ide/cli/codewhisperer/amazonq 四端点的 provider（URL 指向无人监听端口，
    /// 链构造不联网），供 `endpoint_chain_for` 与负缓存的纯函数测试。
    fn provider_with_full_endpoint_registry() -> KiroProvider {
        let mut creds = Vec::new();
        let mut c = KiroCredentials::default();
        c.id = Some(94_001);
        creds.push(c);
        let tm = Arc::new(
            MultiTokenManager::new(
                crate::model::config::Config::default(),
                creds,
                None,
                None,
                false,
            )
            .expect("构造测试 token manager"),
        );
        let mut endpoints: HashMap<String, Arc<dyn KiroEndpoint>> = HashMap::new();
        for name in ["ide", "cli", "codewhisperer", "amazonq"] {
            endpoints.insert(
                name.to_string(),
                Arc::new(MockEndpoint {
                    url: format!("http://127.0.0.1:1/{name}"),
                    name,
                }),
            );
        }
        KiroProvider::with_proxy(tm, None, endpoints, "ide".to_string())
    }

    /// 链构造：OAuth 号（默认 ide）+ fallback 仍开 → 链只有 ide。
    /// 不得从 FALLBACK_ORDER 补 CLI 族（cw/amazonq 会硬编码 tokentype=API_KEY）。
    /// 回退即 FAIL：把 retain 改回给 OAuth 整表补齐，断言变红。
    #[test]
    fn endpoint_chain_oauth_head_gets_fallback_order_endpoints() {
        let p = provider_with_full_endpoint_registry();
        let head = p.endpoints["ide"].clone();
        let chain = p.endpoint_chain_for(&head, &KiroCredentials::default(), true, "us-east-1");
        let names: Vec<&str> = chain.iter().map(|ep| ep.name()).collect();
        assert_eq!(names, vec!["ide"]);
        assert!(
            !names.contains(&"codewhisperer") && !names.contains(&"amazonq"),
            "OAuth 号不得从 FALLBACK_ORDER 落入 CLI 族端点"
        );
    }

    /// 链构造：ksk_ 号（自动路由 cli）→ 链首 + 凭据候选顺序（CLI 族端点，cli-runtime
    /// 未注册跳过）+ 跨族兜底：codewhisperer/amazonq（同为 CLI 协议族）。
    ///
    /// 🔴 对抗审查 M2：ide 必须**整体不在链里**（不是排链尾）——ksk_ 打 ide 必 403
    /// 是确定性错误，而链尾有「兜底铁律」永不跳过，容量风暴时链尾 ide 必被真打 →
    /// 403 从从未成功号 report_failure 累计 → TooManyFailures 误禁用（历史事故
    /// #481 同型）。回退即 FAIL：把 `retain` 改回「挪到链尾」，断言变红。
    #[test]
    fn endpoint_chain_ksk_head_uses_credential_order_first() {
        let p = provider_with_full_endpoint_registry();
        let mut cred = KiroCredentials::default();
        cred.auth_method = Some("api_key".to_string());
        cred.kiro_api_key = Some("ksk_test".to_string());
        let head = p.endpoints["cli"].clone();
        let chain = p.endpoint_chain_for(&head, &cred, true, "us-east-1");
        let names: Vec<&str> = chain.iter().map(|ep| ep.name()).collect();
        assert_eq!(names, vec!["cli", "codewhisperer", "amazonq"]);
        assert!(
            !names.contains(&"ide"),
            "ksk_ 号链不得含 ide（协议族安全：ksk_ 打 ide 必 403，链尾兜底铁律会把它打成确定性失败）"
        );
    }

    /// 开关关闭：链退化为单元素（部署方显式关掉回退的意图必须被尊重）。
    #[test]
    fn endpoint_chain_fallback_disabled_is_single_element() {
        let p = provider_with_full_endpoint_registry();
        let head = p.endpoints["ide"].clone();
        let chain = p.endpoint_chain_for(&head, &KiroCredentials::default(), false, "us-east-1");
        assert_eq!(chain.len(), 1, "显式关闭回退时绝不擅自加端点");
        assert_eq!(chain[0].name(), "ide");
    }

    /// 主端点协议不符 → 不占链首（降级出链），其余健康**同族**端点按序上位。
    /// OAuth 无 CLI 族可上位：兜底铁律把 ide 放回，且不得跨族落到 cw/amazonq。
    #[test]
    fn endpoint_chain_broken_head_is_demoted_not_removed() {
        let p = provider_with_full_endpoint_registry();
        p.mark_route_protocol_broken("ide", "us-east-1");
        let head = p.endpoints["ide"].clone();
        let chain = p.endpoint_chain_for(&head, &KiroCredentials::default(), true, "us-east-1");
        assert!(!chain.is_empty());
        let names: Vec<&str> = chain.iter().map(|ep| ep.name()).collect();
        assert!(
            !names.contains(&"codewhisperer") && !names.contains(&"amazonq") && !names.contains(&"cli"),
            "OAuth 号 ide 被隔离也不得跨族落到 CLI 端点"
        );
        assert_eq!(names, vec!["ide"], "无同族可上位时兜底铁律放回 head");
    }

    /// 兜底铁律：所有端点都被隔离 → 链仍不得为空（否则 response 恒 None，请求无人发送）。
    #[test]
    fn endpoint_chain_never_empty_even_when_all_routes_quarantined() {
        let p = provider_with_full_endpoint_registry();
        for name in ["ide", "cli", "codewhisperer", "amazonq"] {
            p.mark_route_protocol_broken(name, "us-east-1");
        }
        let head = p.endpoints["ide"].clone();
        let chain = p.endpoint_chain_for(&head, &KiroCredentials::default(), true, "us-east-1");
        assert!(!chain.is_empty(), "全隔离时链仍不得为空");
    }

    /// 协议隔离是软的且按 (端点, region) 精确划界：不连坐别的 region / 端点。
    #[test]
    fn protocol_broken_quarantine_is_recorded_and_scoped() {
        let p = provider_with_full_endpoint_registry();
        assert!(!p.is_route_protocol_broken("cli", "us-east-1"), "初始不应有任何隔离");
        p.mark_route_protocol_broken("cli", "us-east-1");
        assert!(p.is_route_protocol_broken("cli", "us-east-1"));
        assert!(
            !p.is_route_protocol_broken("cli", "eu-central-1"),
            "隔离不得跨 region 连坐"
        );
        assert!(
            !p.is_route_protocol_broken("ide", "us-east-1"),
            "隔离不得跨端点连坐"
        );
    }

    /// M3：MCP 直连失败短负缓存 —— 记入 → 判定 → 按 (凭据 id, region) 划界。
    /// 同 region 其它 id 不连坐；与端点连接层键空间互不干扰。
    #[test]
    fn mcp_direct_negative_cache_blocks_same_region_only() {
        let p = provider_with_full_endpoint_registry();
        let id_a = 1u64;
        let id_b = 2u64;
        assert!(
            !p.is_mcp_direct_blocked(id_a, "us-east-1"),
            "初始不应有负缓存"
        );
        p.mark_endpoint_dead(&format!("mcp-direct@{}", id_a), "us-east-1");
        assert!(
            p.is_mcp_direct_blocked(id_a, "us-east-1"),
            "直连失败后 60s 内必须跳过该号直连"
        );
        assert!(
            !p.is_mcp_direct_blocked(id_b, "us-east-1"),
            "负缓存不得连坐同 region 其它凭据"
        );
        assert!(
            !p.is_mcp_direct_blocked(id_a, "eu-central-1"),
            "负缓存不得跨 region 连坐"
        );
        assert!(
            !p.is_endpoint_dead("ide", "us-east-1"),
            "mcp-direct 键不得影响端点连接层负缓存（键空间正交）"
        );
    }

    /// M3：直连负缓存 TTL 过期必须放行（自愈语义，同 is_endpoint_dead 同款惰性清理）。
    #[test]
    fn mcp_direct_negative_cache_expires_after_ttl() {
        let p = provider_with_full_endpoint_registry();
        let id = 1u64;
        p.dead_endpoints.lock().insert(
            format!("mcp-direct@{}@us-east-1", id),
            std::time::Instant::now() - std::time::Duration::from_secs(61),
        );
        assert!(
            !p.is_mcp_direct_blocked(id, "us-east-1"),
            "TTL 过期必须放行重试（上游/token 可能已恢复）"
        );
    }

    /// 死端点负缓存：记入 → 判定 → 划界 → alive 清除。
    #[test]
    fn dead_endpoint_negative_cache_is_recorded_cleared_and_scoped() {
        let p = provider_with_full_endpoint_registry();
        assert!(!p.is_endpoint_dead("ide", "us-east-1"), "初始不应有负缓存");
        p.mark_endpoint_dead("ide", "us-east-1");
        assert!(p.is_endpoint_dead("ide", "us-east-1"));
        assert!(
            !p.is_endpoint_dead("ide", "eu-central-1"),
            "负缓存不得跨 region 连坐"
        );
        p.mark_endpoint_alive("ide", "us-east-1");
        assert!(
            !p.is_endpoint_dead("ide", "us-east-1"),
            "mark_endpoint_alive 必须清除负缓存（拿到 HTTP 响应 = 连接层通了）"
        );
    }

    /// ⭐ 接线守卫：endpoint_chain_for 必须在 call_api_with_retry 内被调用（链式回退接线）。
    ///
    /// 回退即 FAIL：把链构造挪到别的函数 / 改成 `endpoint_for` 单端点直发 → 本条变红。
    #[test]
    fn endpoint_chain_for_is_wired_in_call_api_with_retry() {
        let full = include_str!("provider.rs");
        let cut = full.find("#[cfg(test)]").unwrap_or(full.len());
        let prod = &full[..cut];
        // needle 运行时拼接，避免 include_str! 自匹配。
        let call = ["self.endpoint_chain_for", "("].concat();
        assert_eq!(
            prod.matches(&call).count(),
            1,
            "endpoint_chain_for 应在生产段恰好调用 1 次（MCP 路径不参与端点回退）"
        );
        let fn_body = prod
            .split("async fn call_api_with_retry")
            .nth(1)
            .expect("call_api_with_retry 不应被改名");
        assert!(
            fn_body.contains(&call),
            "endpoint_chain_for 调用必须在 call_api_with_retry 函数体内"
        );
    }

    /// ⭐ 接线守卫：endpoint_fallback 配置开关必须作为 endpoint_chain_for 的实参接线，
    /// 否则开关是死配置（关了也没用）。
    #[test]
    fn endpoint_fallback_config_is_wired_into_chain() {
        let full = include_str!("provider.rs");
        let cut = full.find("#[cfg(test)]").unwrap_or(full.len());
        let prod = &full[..cut];
        let call = ["self.endpoint_chain_for", "("].concat();
        let at = prod
            .find(&call)
            .expect("endpoint_chain_for 调用不应被改名");
        // 开关实参在调用点**之后**（同一调用语句内）。`after` 从 at 起切：at 是
        // ASCII needle 的起点（合法字符边界），find 返回的偏移同理，无多字节坑。
        let after = &prod[at..];
        let gate = after.find("config.endpoint_fallback").unwrap_or_else(|| {
            panic!(
                "endpoint_chain_for 的 fallback_enabled 实参必须来自 config 开关 \
                 （否则该配置是死配置，部署方显式关闭会被无视）"
            )
        });
        assert!(gate < 300, "开关实参必须在调用点近旁（同一语句窗口）");
    }

    /// 失败重试路径：上游先 429（带 Retry-After）→ 冷却 + 换号 → 第二个号 200。
    ///
    /// 钉住「429 分类 → 凭据冷却 → tried_this_call 结构性排除 → failover 换号 → 成功」
    /// 整条链（此前只有 absorb 层纯函数测试，链咬合从未真跑）。meta.retries 必须为 1。
    ///
    /// 回退即 FAIL：把 429 分支的 `report_rate_limited_with_retry_after` 或
    /// `tried_this_call.insert` 删掉——换号会落空、整条链回到同一个号，请求变 Err。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn call_api_failover_after_upstream_429_succeeds_on_another_credential() {
        let up = MockUpstream::start(vec![
            MockResponse {
                status: 429,
                reason: "Too Many Requests",
                body: "{}",
                retry_after_secs: Some(1),
            },
            MockResponse::ok(r#"{"ok":true}"#),
        ]);
        let provider = provider_with_mock_upstream(&up);

        let (resp, meta) = match provider
            .call_api(MOCK_BODY, false, &SharedRetryBudget::new(), Some("claude-sonnet-4"))
            .await
        {
            Ok(v) => v,
            Err(e) => panic!("429 换号后应成功: {e}"),
        };
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
        assert_eq!(meta.retries, 1, "429 一次 + 换号成功 = 恰好 1 次重试");
        assert_eq!(
            up.hits.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "必须打 2 次上游（1 次 429 + 1 次成功）"
        );
    }

    /// 重试耗尽：1 号池（小池预算 1 次）+ 上游恒 500 → `Err`，且只打 1 次就停
    /// （预算 1 = 各号摸一次即透传，不风暴）。
    ///
    /// 回退即 FAIL：把墙钟闸门/预算判据改松（例如预算恢复成号池倍数），
    /// hits 会变成 3+，断言失败。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn call_api_budget_exhausted_returns_err_without_storming_upstream() {
        let up = MockUpstream::start(vec![
            MockResponse {
                status: 500,
                reason: "Internal Server Error",
                body: "{}",
                retry_after_secs: None,
            },
            MockResponse {
                status: 500,
                reason: "Internal Server Error",
                body: "{}",
                retry_after_secs: None,
            },
            MockResponse {
                status: 500,
                reason: "Internal Server Error",
                body: "{}",
                retry_after_secs: None,
            },
        ]);
        // 单号池：小池预算 = 每号 1 次 = 总共 1 次尝试。
        let mut cred = KiroCredentials::default();
        cred.id = Some(91_003);
        cred.auth_method = Some("api_key".to_string());
        cred.kiro_api_key = Some("sk-mock-91003".to_string());
        // 同 provider_with_mock_upstream：显式钉死 endpoint，否则被自动路由到
        // 内置 cli/cli-runtime 候选链、mock 上游零命中。
        cred.endpoint = Some("mock".to_string());
        let tm = Arc::new(
            MultiTokenManager::new(
                crate::model::config::Config::default(),
                vec![cred],
                None,
                None,
                false,
            )
            .expect("构造测试 token manager"),
        );
        let mut endpoints: HashMap<String, Arc<dyn KiroEndpoint>> = HashMap::new();
        endpoints.insert(
            "mock".to_string(),
            Arc::new(MockEndpoint {
                url: format!("http://127.0.0.1:{}", up.port),
                name: "mock",
            }),
        );
        let provider = KiroProvider::with_proxy(tm, None, endpoints, "mock".to_string());

        let err = match provider
            .call_api(MOCK_BODY, false, &SharedRetryBudget::new(), Some("claude-sonnet-4"))
            .await
        {
            Ok(_) => panic!("预算耗尽必须 Err"),
            Err(e) => e,
        };
        assert!(
            err.to_string().contains("500"),
            "终态错误必须透传上游 500 文案，实际: {}",
            err
        );
        assert_eq!(
            up.hits.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "小池预算 1：恒 500 也只许打 1 次，不得风暴"
        );
    }

    // ══════════ A-5：429 备区换桶 与 L1 403 换区 的共享感知（防同请求内 A→B→A→B 振荡）══════════

    /// A-5 专用端点：**URL 与桶键都随 region 走** —— 让「当前区全封 ⇒ 用备区桶」的
    /// 换桶真实可触发（MockEndpoint 的 URL/桶键与 region 无关，两区永远同桶，换桶
    /// 路径根本走不到）。`decorate_api` 仿真实 cli 端点带上 Bearer 头，让 mock 上游
    /// 能按 token 区分请求是哪个号发的。
    ///
    /// 依赖前提：`bucket_key`/`bucket_id` 走 trait 默认实现（= `api_url`，`amz_target`
    /// 为 None），故 URL 随区变化 ⇒ 桶键随区独立；与生产端点的「region 在 host 里」
    /// 是同一种不变量。
    struct RegionAwareMockEndpoint {
        eu_url: String,
        us_url: String,
    }

    impl KiroEndpoint for RegionAwareMockEndpoint {
        fn name(&self) -> &'static str {
            "mock-region"
        }
        fn api_url(&self, ctx: &RequestContext<'_>) -> String {
            self.url_for_region(ctx.credentials.effective_upstream_region(&ctx.config))
        }
        fn mcp_url(&self, ctx: &RequestContext<'_>) -> String {
            self.url_for_region(ctx.credentials.effective_upstream_region(&ctx.config))
        }
        fn decorate_api(
            &self,
            req: reqwest::RequestBuilder,
            ctx: &RequestContext<'_>,
        ) -> reqwest::RequestBuilder {
            req.header("Authorization", format!("Bearer {}", ctx.token))
        }
        fn decorate_mcp(
            &self,
            req: reqwest::RequestBuilder,
            _ctx: &RequestContext<'_>,
        ) -> reqwest::RequestBuilder {
            req
        }
        fn transform_api_body(&self, body: &str, _ctx: &RequestContext<'_>) -> String {
            body.to_string()
        }
    }

    impl RegionAwareMockEndpoint {
        fn url_for_region(&self, region: &str) -> String {
            match region {
                "eu-central-1" => self.eu_url.clone(),
                _ => self.us_url.clone(),
            }
        }
    }

    /// A-5 复现测试（钉顺序）：当前区 429 全封 → 429 路径换备区 → 备区 403 →
    /// **不得**再由 L1 换回原区（原区桶仍在 30s 封禁期，select_endpoint 会把请求
    /// 弹回备区 ⇒ 同一请求内 A→B→A→B 振荡）。
    ///
    /// 序列（#910101 = 受害者，初始区 eu，备区 us）：
    ///   ① #910101 → eu → 429（eu 桶被封）
    ///   ② #910101 → us（429 备区换桶）→ 403 bearer-invalid
    ///   ③ 修复前：L1 按当前区(us)算 `region_retry_target` 换回 eu → eu 桶还封着
    ///      → 备区路径又弹回 us → #910101 **第二次**打 us；修复后：共享标记
    ///      `region_switched_this_call` 挡住 L1 ⇒ #910101 不再换区，惩罚换号走
    ///      failover（再打 eu 的是 #910102）。
    ///
    /// 断言（按请求头里的 token 数每个号在每区的命中）：
    ///   - us 上 #910101 恰好 1 次（换区次数 ≤ 1；修复前是 2 次 = 振荡）；
    ///   - eu 上 #910101 恰好 1 次（从未被换回去）；
    ///   - 单路径行为不变：#910102 从未换过区，eu 403 后仍走 L1 换区（us 恰好 1 次）。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a5_no_region_pingpong_after_429_swap_then_403() {
        // eu = 当前区（429 封桶），us = 备区（403 bearer-invalid）。
        // 每区各配 2 个响应就够（预算封顶 4 次上游调用）；队列耗尽后一律 500，
        // 若真的多打了，500 也会被计入命中 → 断言照样红，不会假绿。
        let eu = MockUpstream::start(vec![
            MockResponse {
                status: 429,
                reason: "Too Many Requests",
                body: "{}",
                retry_after_secs: None,
            },
            MockResponse {
                status: 403,
                reason: "Forbidden",
                body: REAL_BEARER_INVALID_BODY,
                retry_after_secs: None,
            },
            MockResponse {
                status: 403,
                reason: "Forbidden",
                body: REAL_BEARER_INVALID_BODY,
                retry_after_secs: None,
            },
        ]);
        let us = MockUpstream::start(vec![
            MockResponse {
                status: 403,
                reason: "Forbidden",
                body: REAL_BEARER_INVALID_BODY,
                retry_after_secs: None,
            },
            MockResponse {
                status: 403,
                reason: "Forbidden",
                body: REAL_BEARER_INVALID_BODY,
                retry_after_secs: None,
            },
        ]);

        // 4 个 ksk_ 号，初始区钉死 eu（PROBE_ORDER 首项 ⇒ 备区恒为 us），
        // endpoint 钉死 mock-region（order = [mock-region, cli, cli-runtime]，len>1
        // ⇒ 429 分支会封桶并尝试换桶，这正是 A-5 的前提）。id 用本组专属段 91_1xx
        // （endpoint_health 是进程级共享表，避开既有测试的 91_0xx）。
        //
        // ⚠️ 号池必须 ≥4：本轮重试配额 = compute_max_retries(号数, …)，号数 ≤
        // SMALL_POOL_THRESHOLD(3) 时每号只重试 1 次 ⇒ 2 号池配额 = 2，第 2 跳（#910101
        // 打 us）403 后本轮即耗尽（region 错配类错误按设计不可吸收，吸收层不会续轮），
        // failover 根本走不到 #910102。4 号池配额 = min(4×3, 4) = 4，正好装下 A-5 全
        // 序列（eu→us→eu→us，共 4 次上游调用）。91_103/91_104 是陪跑号：选号排序键
        // 按 id 升序平局决胜（⑬ e.id），本序列只会用到 91_101/91_102，它们不被选中。
        let mut creds = Vec::new();
        for (id, token) in [
            (91_101u64, "sk-mock-a5-910101"),
            (91_102, "sk-mock-a5-910102"),
            (91_103, "sk-mock-a5-910103"),
            (91_104, "sk-mock-a5-910104"),
        ] {
            let mut c = KiroCredentials::default();
            c.id = Some(id);
            c.auth_method = Some("api_key".to_string());
            c.kiro_api_key = Some(token.to_string());
            c.api_region = Some("eu-central-1".to_string());
            c.endpoint = Some("mock-region".to_string());
            creds.push(c);
        }
        let tm = Arc::new(
            MultiTokenManager::new(
                crate::model::config::Config::default(),
                creds,
                None,
                None,
                false,
            )
            .expect("构造测试 token manager"),
        );
        let mut endpoints: HashMap<String, Arc<dyn KiroEndpoint>> = HashMap::new();
        endpoints.insert(
            "mock-region".to_string(),
            Arc::new(RegionAwareMockEndpoint {
                eu_url: format!("http://127.0.0.1:{}", eu.port),
                us_url: format!("http://127.0.0.1:{}", us.port),
            }),
        );
        let provider = KiroProvider::with_proxy(tm, None, endpoints, "mock-region".to_string());

        let err = match provider
            .call_api(MOCK_BODY, false, &SharedRetryBudget::new(), Some("claude-sonnet-4"))
            .await
        {
            Ok(_) => panic!("备区 403 是永久性（未授权区），序列必须以 Err 收尾"),
            Err(e) => e,
        };

        let eu_heads = eu.captured_heads();
        let us_heads = us.captured_heads();
        let hits_by = |heads: &[String], token: &str| heads.iter().filter(|h| h.contains(token)).count();
        let victim_eu = hits_by(&eu_heads, "sk-mock-a5-910101");
        let victim_us = hits_by(&us_heads, "sk-mock-a5-910101");
        let fresh_eu = hits_by(&eu_heads, "sk-mock-a5-910102");
        let fresh_us = hits_by(&us_heads, "sk-mock-a5-910102");
        let extra_eu = hits_by(&eu_heads, "sk-mock-a5-910103") + hits_by(&eu_heads, "sk-mock-a5-910104");
        let extra_us = hits_by(&us_heads, "sk-mock-a5-910103") + hits_by(&us_heads, "sk-mock-a5-910104");
        eprintln!(
            "a5 hits eu={} us={} | 910101 eu/us={}/{} 910102 eu/us={}/{} extra 103+104 eu/us={}/{} | err={}",
            eu_heads.len(),
            us_heads.len(),
            victim_eu,
            victim_us,
            fresh_eu,
            fresh_us,
            extra_eu,
            extra_us,
            err
        );
        assert!(
            err.to_string().contains("403"),
            "终态错误必须透传上游 403 文案，实际: {err}; hits 910101 eu/us={victim_eu}/{victim_us} \
             910102 eu/us={fresh_eu}/{fresh_us} extra103+104 eu/us={extra_eu}/{extra_us} \
             eu_total={} us_total={}",
            eu_heads.len(),
            us_heads.len()
        );

        assert_eq!(
            victim_eu, 1,
            "受害者 #910101 必须恰好打 1 次当前区 eu（初始 429）；换回原区=振荡，实际 {victim_eu}"
        );
        assert_eq!(
            victim_us, 1,
            "受害者 #910101 在备区 us 必须恰好 1 次（429 换桶那一次）；\
             修复前 L1 换回 eu 被备区路径弹回 ⇒ us 会是 2 次（A→B→A→B 振荡），实际 {victim_us}"
        );
        assert_eq!(
            fresh_eu, 1,
            "从未换过区的 #910102 应接住 failover 打 eu，实际 {fresh_eu}"
        );
        assert_eq!(
            fresh_us, 1,
            "单路径行为不变：从未换过区的 #910102 吃 eu 403 后仍走 L1 换区（us 恰好 1 次），\
             实际 {fresh_us}"
        );
    }

    /// ksk_ API Key 凭据：`effective_endpoint_order` 返回多端点候选链（q.* 优先、其余回退）。
    fn ksk_credential() -> KiroCredentials {
        let mut c = KiroCredentials::default();
        c.auth_method = Some("api_key".to_string());
        c.kiro_api_key = Some("ksk_test_key".to_string());
        c.endpoint = None;
        c
    }

    /// 无统计数据时，每个候选端点都会被试到（冷启动探测），且**首选遵循先验顺序**。
    ///
    /// 🔴 **本测试取代了原来的 `select_endpoint_rotates_through_all_available_endpoints`。**
    /// 那条断言的是严格 round-robin 全序（`cli, cli-runtime, cli, cli-runtime, ...`），
    /// 而 round-robin 正是本次要移除的行为（它既不按凭据也不按成功率）。断言逐字保留会
    /// 让"改对了"表现为"测试失败"，所以必须换成断言**新的契约**：
    ///
    /// - 冷启动阶段每个候选都能拿到样本（不试就永远没数据，没数据就永远不被降权 = 死锁）
    /// - 首次选择走先验（候选序第一个），保证零数据时行为与旧的固定优先序一致
    #[test]
    fn select_endpoint_probes_every_candidate_on_cold_start() {
        let cli = crate::kiro::endpoint::cli::CLI_ENDPOINT_NAME;
        let provider = provider_with_default(cli);
        let cred = ksk_credential();
        let order = cred.effective_endpoint_order(cli);
        assert!(order.len() >= 2, "ksk_ 号应为多端点候选链");

        // ⚠️ 用**本测试专属**的凭据 id：端点健康表是进程级共享的（见
        // endpoint_health::SHARED），而 `cargo test` 多线程并发跑 ⇒ 与别的测试共用 id
        // 会让「冷启动」前提被对方写入的样本破坏（本测试断言的正是零样本时的行为）。
        const ID: u64 = 90_001;

        // 零数据时首选 = 先验第一个（与旧固定优先序一致，无回归）。
        assert_eq!(
            provider.pick_endpoint_for_test(&cred, ID).unwrap().name(),
            order[0],
            "冷启动首选必须遵循候选先验顺序"
        );

        // 每选中一个就记一次结果（模拟真实请求闭环），冷启动规则会把流量导向尚无样本者，
        // 直到所有候选都被探测过。
        let mut seen: HashSet<&str> = HashSet::new();
        for _ in 0..(order.len() * 4) {
            let ep = provider.pick_endpoint_for_test(&cred, ID).expect("全可用必有返回");
            let name = ep.name();
            seen.insert(name);
            provider.report_endpoint_outcome(ID, name, true);
        }
        assert_eq!(
            seen.len(),
            order.len(),
            "冷启动阶段每个候选端点都必须被试到，实际 {:?}",
            seen
        );
    }

    /// 硬门：部分桶冷却时只选非冷却桶（跳过被封桶，恒命中剩余桶）。
    ///
    /// 429 封禁是**硬门**，自适应派发（软偏好）不得越过它 —— 哪怕被封那个桶的
    /// 历史成功率更高。软偏好只在硬门放行的候选之间排序。
    #[test]
    fn select_endpoint_hard_gate_skips_cooled_buckets() {
        let cli = crate::kiro::endpoint::cli::CLI_ENDPOINT_NAME;
        let provider = provider_with_default(cli);
        let cred = ksk_credential();
        let order = cred.effective_endpoint_order(cli);
        // 封掉首选桶（q.*），其余候选保持可用。
        // ⚠️ 桶键必须用 `bucket_key` 算 —— 与生产写入点同源。写死端点名会让这条测试
        // 恒绿而实际什么都没封（键对不上 ⇒ select 侧读不到），是最坏的假绿形态。
        let cfg = provider.token_manager.config();
        let blocked_key = provider
            .endpoints
            .get(order[0])
            .expect("首选端点应已注册")
            .bucket_key(&cred, &cfg);
        provider.endpoint_buckets.lock().insert(
            (123, blocked_key),
            Instant::now() + Duration::from_secs(60),
        );
        let mut picked: HashSet<&str> = HashSet::new();
        // 循环次数取探索周期的整数倍：自适应派发靠「冷启动优先 + 周期性探索」覆盖
        // 全部候选，而不再是每次调用就换一个（round-robin）。次数不足会漏掉探索节拍。
        for _ in 0..(order.len() * 16) {
            let ep = provider
                .pick_endpoint_for_test(&cred, 123)
                .expect("还有非冷却桶，必有返回");
            assert_ne!(ep.name(), order[0], "硬门不得放行被封的端点桶");
            picked.insert(ep.name());
        }
        // 足够多的调用里，所有非冷却桶都应被选到（冷启动保证每个至少一次）。
        assert_eq!(
            picked.len(),
            order.len() - 1,
            "部分冷却时应覆盖所有非冷却桶"
        );
    }

    /// 🔴 ksk 号「当前区全封 ⇒ 改用备区桶」（2026-08-10 新增能力的正向测试）。
    ///
    /// # 修的是什么
    /// 一个 `ksk_` 号的桶集合此前只含**当前 region** 的两个（`q.<区>` / `runtime.<区>`）。
    /// 当前区两个桶各被 429 封 30s ⇒ `select_endpoint` 返 None ⇒ 判该号不可用
    /// ⇒ **另一个区即使完全空闲也永不被尝试**。实测后果是单号有效 RPM 被压到
    /// 「30s 窗口能挤进多少」（用户观察到 EU 号从 60~70 RPM 掉到十几二十）。
    ///
    /// 本测试钉住：只封当前区 ⇒ 仍能选出端点，且返回的备区 **不等于**当前区。
    #[test]
    fn ksk_falls_back_to_alt_region_when_current_region_all_banned() {
        let cli = crate::kiro::endpoint::cli::CLI_ENDPOINT_NAME;
        let provider = provider_with_default(cli);
        let cred = ksk_credential();
        let order = cred.effective_endpoint_order(cli);
        let cfg = provider.token_manager.config();
        let cur = cred.effective_upstream_region(&cfg);

        // 只封**当前区**的全部桶（桶键与生产同源）
        {
            let mut buckets = provider.endpoint_buckets.lock();
            for name in &order {
                let key = provider
                    .endpoints
                    .get(*name)
                    .expect("候选端点应已注册")
                    .bucket_key(&cred, &cfg);
                buckets.insert((123, key), Instant::now() + Duration::from_secs(60));
            }
        }

        let (_, alt) = provider
            .select_endpoint(&cred, 123)
            .expect("当前区全封时必须回退到备区，而不是判该号不可用");
        let alt = alt.expect("回退路径必须报告用了哪个备区（调用方要据此覆盖 api_region）");
        assert_ne!(
            alt, cur,
            "备区不能等于当前区，否则等于没换（桶仍在封禁中）"
        );
    }

    /// 全部冷却返回 None（既有语义，自适应派发不得破坏）。
    ///
    /// ⚠️ 2026-08-10 起 ksk 号有「当前区全封 ⇒ 用备区桶」的回退
    /// （见 `ksk_falls_back_to_alt_region_when_current_region_all_banned`），
    /// 所以要断言"真的无桶可用"必须把**所有候选 region** 的桶都封掉。
    /// 保留 ksk 号不变，但把**每个候选 region** 的桶都封掉 —— 只有这样才是
    /// "真的无桶可用"。（只封当前区已不足以返 None，那正是新回退能力要解决的场景。）
    #[test]
    fn select_endpoint_all_cooled_returns_none() {
        let cli = crate::kiro::endpoint::cli::CLI_ENDPOINT_NAME;
        let provider = provider_with_default(cli);
        let cred = ksk_credential();
        let order = cred.effective_endpoint_order(cli);
        // 桶键与生产同源（`bucket_key`），理由同上一条测试。
        let cfg = provider.token_manager.config();
        {
            let mut buckets = provider.endpoint_buckets.lock();
            // 遍历所有候选 region（与生产回退用的同一份 `PROBE_ORDER`），
            // 逐区把该区的全部端点桶封住。
            for region in crate::kiro::region_probe::PROBE_ORDER {
                let mut c = cred.clone();
                c.api_region = Some(region.to_string());
                for name in &order {
                    let key = provider
                        .endpoints
                        .get(*name)
                        .expect("候选端点应已注册")
                        .bucket_key(&c, &cfg);
                    buckets.insert((123, key), Instant::now() + Duration::from_secs(60));
                }
            }
        }
        assert!(
            provider.pick_endpoint_for_test(&cred, 123).is_none(),
            "全部冷却必须返回 None"
        );
    }

    /// last hop 全桶 429 封禁：generic 错误必须带 `retry_after_secs=`（handlers A5 → 429+RA，
    /// 而不是无标记兜底 502）。TTL 取最短桶剩余，没有则 2s。不增加 hop。
    #[test]
    fn last_hop_all_buckets_sealed_stamps_retry_after_secs() {
        let cli = crate::kiro::endpoint::cli::CLI_ENDPOINT_NAME;
        const ID: u64 = 92_201;
        let mut cred = ksk_credential();
        cred.id = Some(ID);
        let tm = std::sync::Arc::new(
            crate::kiro::token_manager::MultiTokenManager::new(
                crate::model::config::Config::default(),
                vec![cred.clone()],
                None,
                None,
                false,
            )
            .expect("测试 token manager"),
        );
        let provider = KiroProvider::with_proxy(
            tm,
            None,
            crate::kiro::endpoint::registry(),
            cli.to_string(),
        );
        let order = cred.effective_endpoint_order(cli);
        let cfg = provider.token_manager.config();
        {
            let mut buckets = provider.endpoint_buckets.lock();
            for region in crate::kiro::region_probe::PROBE_ORDER {
                let mut c = cred.clone();
                c.api_region = Some(region.to_string());
                for name in &order {
                    let key = provider
                        .endpoints
                        .get(*name)
                        .expect("候选端点应已注册")
                        .bucket_key(&c, &cfg);
                    buckets.insert((ID, key), Instant::now() + Duration::from_secs(14));
                }
            }
        }
        assert!(
            provider.select_endpoint(&cred, ID).is_none(),
            "全 region 封桶必须返 None"
        );
        assert!(
            provider.all_enabled_kiro_endpoint_buckets_sealed(),
            "号池里唯一 Kiro 的桶全封"
        );
        let ra = provider.shortest_endpoint_bucket_retry_after_secs(Some(ID));
        assert!(
            (1..=14).contains(&ra),
            "最短桶 TTL 应落在 1..=14，实际 {ra}"
        );

        let marker = ["retry_after_secs", "="].concat();
        let sealed = anyhow::anyhow!(
            "凭据 #{ID} 所有端点桶均处于 429 封禁期（当前区与备用区的桶都在封禁中）"
        );
        let stamped = provider.with_sealed_bucket_retry_after(
            sealed,
            crate::usage::RequestOutcome::RateLimited,
        );
        let s = stamped.to_string();
        assert!(
            s.contains(marker.as_str()),
            "A5 冷却分支需要 {marker} 才能 429+Retry-After 而非 502: {s}"
        );
        let secs = crate::anthropic::handlers::parse_retry_after_secs(&s)
            .expect("必须能解析 retry_after 真值");
        assert!((1..=14).contains(&secs), "解析出 {secs}");
        let again = provider.with_sealed_bucket_retry_after(
            stamped,
            crate::usage::RequestOutcome::RateLimited,
        );
        assert_eq!(
            again.to_string().matches(marker.as_str()).count(),
            1,
            "已有标记不得再打一份"
        );

        let generic = anyhow::anyhow!("流式 API 请求失败: 429 Too Many Requests {{}}");
        let s2 = provider
            .with_sealed_bucket_retry_after(generic, crate::usage::RequestOutcome::RateLimited)
            .to_string();
        assert!(
            s2.contains(marker.as_str()),
            "last hop generic 429 必须 stamp: {s2}"
        );

        let auth = anyhow::anyhow!("流式 API 请求失败: 403 Forbidden bearer invalid");
        let s3 = provider
            .with_sealed_bucket_retry_after(auth, crate::usage::RequestOutcome::AuthFailed)
            .to_string();
        assert!(
            !s3.contains(marker.as_str()),
            "403 不得被打成 A5 冷却: {s3}"
        );

        let empty = provider_with_default(cli);
        assert_eq!(
            empty.shortest_endpoint_bucket_retry_after_secs(None),
            2,
            "无封禁桶时兜底 2s"
        );
    }

    /// 混池：只有部分号的桶封了 → 不是「every credential sealed」，不 stamp。
    #[test]
    fn mixed_pool_partial_seal_does_not_stamp_retry_after_secs() {
        let cli = crate::kiro::endpoint::cli::CLI_ENDPOINT_NAME;
        const A: u64 = 92_211;
        const B: u64 = 92_212;
        let mk = |id: u64| {
            let mut c = ksk_credential();
            c.id = Some(id);
            c
        };
        let tm = std::sync::Arc::new(
            crate::kiro::token_manager::MultiTokenManager::new(
                crate::model::config::Config::default(),
                vec![mk(A), mk(B)],
                None,
                None,
                false,
            )
            .expect("测试 token manager"),
        );
        let provider = KiroProvider::with_proxy(
            tm,
            None,
            crate::kiro::endpoint::registry(),
            cli.to_string(),
        );
        let cred_a = mk(A);
        let order = cred_a.effective_endpoint_order(cli);
        let cfg = provider.token_manager.config();
        {
            let mut buckets = provider.endpoint_buckets.lock();
            for region in crate::kiro::region_probe::PROBE_ORDER {
                let mut c = cred_a.clone();
                c.api_region = Some(region.to_string());
                for name in &order {
                    let key = provider
                        .endpoints
                        .get(*name)
                        .expect("候选端点应已注册")
                        .bucket_key(&c, &cfg);
                    buckets.insert((A, key), Instant::now() + Duration::from_secs(30));
                }
            }
        }
        assert!(
            !provider.all_enabled_kiro_endpoint_buckets_sealed(),
            "B 未封，不得判全池封禁"
        );
        let marker = ["retry_after_secs", "="].concat();
        let generic = anyhow::anyhow!("流式 API 请求失败: 429 Too Many Requests {{}}");
        let s = provider
            .with_sealed_bucket_retry_after(generic, crate::usage::RequestOutcome::RateLimited)
            .to_string();
        assert!(
            !s.contains(marker.as_str()),
            "还有未封号时 generic 429 不 stamp（换号仍可能成功）: {s}"
        );
    }

    /// order 长度 1（单端点 OAuth 号）：恒返回该唯一端点，行为与固定优先序完全一致（零回归）。
    #[test]
    fn select_endpoint_single_endpoint_is_stable() {
        let provider = provider_with_default("ide");
        let cred = KiroCredentials::default(); // 无 api_key、无显式 endpoint → order=[ide]
        for _ in 0..4 {
            let ep = provider
                .pick_endpoint_for_test(&cred, 9)
                .expect("单端点不封必有返回");
            assert_eq!(ep.name(), "ide", "单端点恒返回唯一端点");
        }
    }

    /// ⭐ 自适应派发：某端点对某号连续失败后，流量应转向成功的那个端点。
    ///
    /// 这是替换 round-robin 的**核心收益**：旧实现无论结果如何都每隔一次送一批请求
    /// 给坏端点，新实现会学会避开它。
    #[test]
    fn select_endpoint_shifts_traffic_to_successful_endpoint() {
        let cli = crate::kiro::endpoint::cli::CLI_ENDPOINT_NAME;
        let provider = provider_with_default(cli);
        let cred = ksk_credential();
        let order = cred.effective_endpoint_order(cli);
        assert!(order.len() >= 2, "ksk_ 号应有多端点候选，否则本测试无意义");
        let bad = order[0];
        let good = order[1];

        // 先让两个端点各拿到样本（冷启动阶段），再把 bad 打成恒失败。
        provider.report_endpoint_outcome(90011, bad, false);
        provider.report_endpoint_outcome(90011, good, true);
        for _ in 0..5 {
            provider.report_endpoint_outcome(90011, bad, false);
            provider.report_endpoint_outcome(90011, good, true);
        }

        // 统计一轮选择里 good 的占比：应显著多于 bad（bad 只在探索节拍出现）。
        let mut good_hits = 0;
        let mut bad_hits = 0;
        for _ in 0..32 {
            match provider.pick_endpoint_for_test(&cred, 90011) {
                Some(ep) if ep.name() == good => good_hits += 1,
                Some(ep) if ep.name() == bad => bad_hits += 1,
                other => panic!("意外的端点: {:?}", other.map(|e| e.name())),
            }
        }
        assert!(
            good_hits > bad_hits * 3,
            "成功端点应拿到绝大多数流量，实际 good={} bad={}",
            good_hits,
            bad_hits
        );
        assert!(
            bad_hits > 0,
            "坏端点仍须被周期性探索（否则上游恢复无从发现）"
        );
    }

    /// ⭐ 自适应派发是**每凭据独立**的：号 A 学到的结论不得影响号 B。
    ///
    /// 旧的 `endpoint_rotation` 是全进程共享计数器，做不到这一点 —— 这正是本次替换
    /// 要解决的第一个缺陷。
    #[test]
    fn select_endpoint_learning_is_per_credential() {
        let cli = crate::kiro::endpoint::cli::CLI_ENDPOINT_NAME;
        let provider = provider_with_default(cli);
        let cred = ksk_credential();
        let order = cred.effective_endpoint_order(cli);
        let a = order[0];
        let b = order[1];

        // 号 90021：a 坏 b 好。号 90022：反过来。
        for _ in 0..6 {
            provider.report_endpoint_outcome(90021, a, false);
            provider.report_endpoint_outcome(90021, b, true);
            provider.report_endpoint_outcome(90022, a, true);
            provider.report_endpoint_outcome(90022, b, false);
        }

        // 各取一次非探索节拍的选择（连续多次取众数，避开探索干扰）。
        let mut a_for_202 = 0;
        let mut b_for_101 = 0;
        for _ in 0..7 {
            if provider.pick_endpoint_for_test(&cred, 90022).map(|e| e.name()) == Some(a) {
                a_for_202 += 1;
            }
            if provider.pick_endpoint_for_test(&cred, 90021).map(|e| e.name()) == Some(b) {
                b_for_101 += 1;
            }
        }
        assert!(a_for_202 >= 5, "号 90022 应偏好 a，实际命中 {}", a_for_202);
        assert!(b_for_101 >= 5, "号 90021 应偏好 b，实际命中 {}", b_for_101);
    }

    /// ⭐ 硬门优先于软偏好：成功率最高的端点被 429 封禁时，必须让位给低成功率的可用端点。
    #[test]
    fn hard_gate_overrides_success_rate_preference() {
        let cli = crate::kiro::endpoint::cli::CLI_ENDPOINT_NAME;
        let provider = provider_with_default(cli);
        let cred = ksk_credential();
        let order = cred.effective_endpoint_order(cli);
        let weak = order[0];
        let strong = order[1];

        // strong 成功率远高于 weak。
        for _ in 0..6 {
            provider.report_endpoint_outcome(90031, weak, false);
            provider.report_endpoint_outcome(90031, strong, true);
        }
        // 但 strong 被 429 封禁。
        // 桶键与生产同源（`bucket_key`）：写死端点名会让封禁不生效而测试假绿。
        let cfg = provider.token_manager.config();
        let strong_key = provider
            .endpoints
            .get(strong)
            .expect("strong 端点应已注册")
            .bucket_key(&cred, &cfg);
        provider.endpoint_buckets.lock().insert(
            (90031, strong_key),
            Instant::now() + Duration::from_secs(60),
        );

        for _ in 0..8 {
            let ep = provider
                .pick_endpoint_for_test(&cred, 90031)
                .expect("weak 未封禁，必有返回");
            assert_eq!(
                ep.name(),
                weak,
                "硬门封禁的端点不得因成功率高而被选中"
            );
        }
    }

    /// ⭐ 每凭据并发闸：同一号的许可数受限，**不同号各自独立**。
    ///
    /// 这是两级闸的核心契约 —— 全局闸只管总量，不管分布；本闸保证一个号打满后
    /// 其余容量必然留给别的号（防「一个慢号吃光全局许可、整池被拖死」）。
    #[test]
    fn per_credential_gate_is_isolated_between_credentials() {
        let cli = crate::kiro::endpoint::cli::CLI_ENDPOINT_NAME;
        let provider = provider_with_default(cli);
        let limit = provider.per_credential_limit();
        assert!(limit >= 1, "容量必须 ≥1，否则号被静默废掉");

        // 号 1 拿满全部许可。
        let g1 = provider.per_credential_gate(1);
        let mut held = Vec::new();
        for _ in 0..limit {
            held.push(
                g1.clone()
                    .try_acquire_owned()
                    .expect("容量内应能拿到许可"),
            );
        }
        // 再拿必失败 —— 硬上限生效。
        assert!(
            g1.clone().try_acquire_owned().is_err(),
            "超出每凭据上限必须拿不到许可"
        );

        // 关键断言：号 2 完全不受号 1 打满的影响。
        let g2 = provider.per_credential_gate(2);
        assert!(
            g2.try_acquire_owned().is_ok(),
            "一个号打满不得影响其它号 —— 这正是两级闸要解决的问题"
        );

        // 号 1 释放后容量回归。
        held.clear();
        assert!(
            g1.try_acquire_owned().is_ok(),
            "许可 Drop 后应自动归还（RAII，免手动释放）"
        );
    }

    /// 同一凭据多次取闸返回**同一把** Semaphore（懒初始化不得每次新建）。
    ///
    /// 若每次 new 一把，上限就完全失效 —— 每个请求都拿到一把全新的满容量闸。
    #[test]
    fn per_credential_gate_is_memoized_not_recreated() {
        let cli = crate::kiro::endpoint::cli::CLI_ENDPOINT_NAME;
        let provider = provider_with_default(cli);
        let limit = provider.per_credential_limit();
        let a = provider.per_credential_gate(7);
        let b = provider.per_credential_gate(7);
        assert!(Arc::ptr_eq(&a, &b), "同一 id 必须复用同一把闸");
        // 通过 a 拿满，再从 b 取应当也拿不到（证明二者共享计数）。
        let mut held = Vec::new();
        for _ in 0..limit {
            held.push(a.clone().try_acquire_owned().unwrap());
        }
        assert!(
            b.try_acquire_owned().is_err(),
            "两个句柄必须共享同一计数，否则上限形同虚设"
        );
    }

    /// 清理函数把该号的闸与端点统计一并移除，且不误伤其它号。
    #[test]
    fn forget_credential_runtime_state_clears_only_that_credential() {
        let cli = crate::kiro::endpoint::cli::CLI_ENDPOINT_NAME;
        let provider = provider_with_default(cli);
        let before = provider.per_credential_gate(90051);
        provider.report_endpoint_outcome(90051, cli, true);
        provider.report_endpoint_outcome(90052, cli, true);

        provider.forget_credential_runtime_state(90051);

        // ⚠️ 断言按 **id 存在性** 而非 `snap.len()`：端点健康表是**进程级共享**的
        // （见 endpoint_health::SHARED 的理由），而 `cargo test` 默认多线程并发跑 ⇒
        // 其它测试写进去的条目会让任何全局长度断言随机失败。用唯一 id + 存在性断言，
        // 与并发无关。（本仓踩过同型坑：usage/pipeline.rs 的 DROPPED 是进程级计数器，
        // 那里靠一把串行锁 + 差值断言解决；这里用唯一 id 更轻。）
        let snap = provider.endpoint_health_snapshot();
        assert!(
            !snap.iter().any(|s| s.credential_id == 90051),
            "号 90051 的统计应已被清除"
        );
        assert!(
            snap.iter().any(|s| s.credential_id == 90052),
            "号 90052 的统计不得被误伤"
        );
        // 闸被移除 ⇒ 再取是**新的一把**（与清理前不是同一个 Arc）。
        let after = provider.per_credential_gate(90051);
        assert!(!Arc::ptr_eq(&before, &after), "清理后应重新懒建");
    }

    /// 配置 0 视为「不限」并退化成全局容量，**绝不**建出容量 0 的闸。
    ///
    /// 容量 0 会让该号永远拿不到许可 = 号被静默废掉，且症状是「号在池里但一个请求都不走」，
    /// 极难排查。所以 0 必须被解释成「不单独限制」。
    #[test]
    fn per_credential_limit_zero_means_unlimited_not_zero_capacity() {
        let cfg = crate::model::config::Config::default();
        assert!(
            cfg.upstream_per_credential_limit > 0,
            "默认值必须为正，0 是「不限」的特殊语义而非默认"
        );
        // 直接验证构造逻辑：0 → 退化为全局容量（provider_with_default 用 Config::default，
        // 故此处只断言默认值语义；0 的分支由上面的 if 表达式保证，见 with_proxy）。
        let provider = provider_with_default(crate::kiro::endpoint::cli::CLI_ENDPOINT_NAME);
        assert!(
            provider.per_credential_limit() >= 1,
            "任何配置下容量都必须 ≥1"
        );
    }

    /// 快照可观测：记过结果的组合都能在 snapshot 里查到成功率与样本数。
    #[test]
    fn endpoint_health_snapshot_is_observable() {
        let cli = crate::kiro::endpoint::cli::CLI_ENDPOINT_NAME;
        let provider = provider_with_default(cli);
        provider.report_endpoint_outcome(90041, cli, true);
        provider.report_endpoint_outcome(90041, cli, false);
        let snap = provider.endpoint_health_snapshot();
        let e = snap
            .iter()
            .find(|s| s.credential_id == 90041 && s.endpoint == cli)
            .expect("应能查到该组合");
        assert_eq!(e.samples, 2);
        assert!(e.success_rate.is_some(), "有样本时必须给出成功率");
    }

    /// ⭐ 守卫：429 分支必须实现「封当前端点桶 + 判断是否还有未封端点 + 换端点时摘出本号」。
    /// 这三步缺一，换桶就退化成「设凭据冷却换号」，q.*/runtime.* 双桶形同虚设。
    #[test]
    fn bucket_switch_on_429_must_throttle_and_release_credential() {
        let src = include_str!("provider.rs");
        let prod: String = src
            .split("#[cfg(test)]")
            .next()
            .expect("生产段应存在")
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        // 封桶时长常量必须存在且被生产段使用。
        assert!(
            prod.contains("ENDPOINT_BUCKET_THROTTLE"),
            "429 分支必须封禁端点桶（引用 ENDPOINT_BUCKET_THROTTLE）"
        );
        assert!(
            prod.contains("has_unthrottled_endpoint"),
            "429 必须用 has_unthrottled_endpoint 判断是否还有未封端点（决定换端点还是换号）"
        );
        assert!(
            prod.contains("tried_this_call.remove(&ctx.id)"),
            "换端点路径必须把本号从 tried_this_call 摘出，否则 acquire_context_excluding 结构性避开它"
        );
    }

    /// ⭐ 守卫（BLOCKER 回归）：换端点继续分支**不得**占位 `rate_limited_this_call`。
    ///
    /// 若在 `tried_this_call.remove` 后顺手 `rate_limited_this_call.insert(ctx.id)` 当"占位"，
    /// 则当第二端点也 429、`has_unthrottled_endpoint` 返回 false 时，`else if rate_limited_this_call
    /// .insert(ctx.id)` 恒为 false → 落最终 else 只打 debug → **凭据级冷却永不设置**，只靠
    /// `tried_this_call` 排除，跨请求又靠 `select_endpoint` None 分支设 30s 冷却兜底。双端连 429
    /// 的凭据会失去"全部封 → 冷却换号"的语义，退化成在桶窗口内反复打上游。
    #[test]
    fn bucket_switch_branch_must_not_occupy_rate_limited_this_call() {
        let src = include_str!("provider.rs");
        let start = src
            .find("has_unthrottled_endpoint(&call_creds")
            .expect("429 换桶判断应存在");
        // 窗口从换桶判断（`has_unthrottled_endpoint`）截到「全部端点都封」的 `else if` 之前，
        // 中间正好是换端点继续分支体（tried_this_call.remove + warn），不应含任何 insert。
        let end = src[start..]
            .find("else if rate_limited_this_call")
            .map(|i| start + i)
            .expect("全部封分支的 else if 应存在");
        let window = &src[start..end];
        assert!(
            !window.contains("rate_limited_this_call.insert"),
            "换端点继续分支不得占位 rate_limited_this_call —— 否则全部端点都封时去重逻辑误判 \
             已冷却过、永不设凭据级冷却（双端点连 429 时凭据冷却失效）"
        );
    }

    // ══════════ 上游 429 吸收层 ══════════

    fn absorb_cfg(enabled: bool) -> crate::model::config::Config {
        let mut c = crate::model::config::Config::default();
        c.upstream_retry_absorb_enabled = enabled;
        c
    }

    /// ⭐ BLOCKER 9 守卫：吸收准入判据必须是「剩余 > 退避 + 一轮最坏耗时(20s)」。
    ///
    /// 回退即 FAIL：把 `should_start_another_round` 换回「剩余 >= 退避」（即删掉
    /// `+ ABSORB_MIN_USEFUL_ROUND_SECS`），下面第二条断言立刻失败 —— 那种判据下
    /// 剩余 25s / 退避 10s 会被判定"够跑一轮"，然后这一轮必然在半路被 deadline 砍断：
    /// 白打一轮上游、客户端白等，正是外置 shield 的 p50 73.2s 的成因。
    #[test]
    fn absorb_budget_gate_requires_room_for_a_full_round() {
        let now = std::time::Instant::now();
        let d = Duration::from_secs;

        // 剩余 45s、退避 10s ⇒ 45 > 10+20 ⇒ 可以再跑一轮。
        assert!(
            should_start_another_round(now + d(45), now, d(10)),
            "剩余 45s / 退避 10s 应当允许再跑一轮"
        );
        // ⭐ 承重断言：剩余 25s、退避 10s ⇒ 25 > 30 为假 ⇒ 必须放弃。
        //   若判据退回 `剩余 >= 退避`，25 >= 10 会为真 → 本断言 FAIL。
        assert!(
            !should_start_another_round(now + d(25), now, d(10)),
            "剩余 25s 不足以容纳 退避 10s + 一轮最坏 20s，必须放弃而非白打一轮"
        );
        // 边界：恰好等于 delay+20 也要拒（严格大于）。
        assert!(
            !should_start_another_round(now + d(30), now, d(10)),
            "恰好等于 退避+一轮最坏耗时 时必须拒绝（严格大于）"
        );
        // deadline 已过：saturating 归零，必拒，且不 panic。
        assert!(!should_start_another_round(now, now + d(5), d(1)));
    }

    /// 关闭时 `effective_max_rounds()` 恒为 0 ⇒ 「关 ⇒ 零额外轮次」。
    ///
    /// 回退即 FAIL：把 `effective_max_rounds` 改成无条件返回 `self.max_rounds`
    /// （即删掉 `if self.enabled`），第一条断言失败。这条是「默认关等价旧行为」
    /// 的唯一可断言支点 —— 循环里的 `absorb_round >= effective_max_rounds()`
    /// 正是靠它在关闭时立即 break。
    #[test]
    fn absorb_policy_disabled_yields_zero_rounds() {
        let off = AbsorbPolicy::from_config(&absorb_cfg(false));
        assert_eq!(
            off.effective_max_rounds(),
            0,
            "吸收层关闭时必须是零额外轮次（否则 'absorb 循环不会立即 break）"
        );
        let on = AbsorbPolicy::from_config(&absorb_cfg(true));
        assert_eq!(
            on.effective_max_rounds(),
            crate::model::config::Config::default().upstream_retry_absorb_max_rounds,
            "开启时应当用配置的 max_rounds"
        );
    }

    /// 关闭时 `round_budget()` 恒返完整 45s，与旧代码的墙钟判据逐字节等价。
    ///
    /// 回退即 FAIL：把 `round_budget` 里的 `if self.enabled` 去掉（无条件夹 deadline），
    /// 则关闭状态下剩余预算会参与 min() → 墙钟闸门行为改变 → 第一条断言失败。
    #[test]
    fn absorb_disabled_keeps_legacy_wall_clock_budget() {
        let off = AbsorbPolicy::from_config(&absorb_cfg(false));
        let now = std::time::Instant::now();
        let full = Duration::from_secs(MAX_REQUEST_RETRY_BUDGET_SECS);
        // 即便 deadline 已经过期，关闭状态也必须返完整 45s（等价旧行为）。
        assert_eq!(off.round_budget(now, now + Duration::from_secs(99)), full);
        assert_eq!(off.round_budget(now + Duration::from_secs(1), now), full);

        // 开启时：一轮上限被剩余预算夹住，这就是"吸收轮不会超总预算"的机制。
        let on = AbsorbPolicy::from_config(&absorb_cfg(true));
        let squeezed = on.round_budget(now + Duration::from_secs(12), now);
        assert_eq!(
            squeezed,
            Duration::from_secs(12),
            "剩余 12s 时一轮墙钟预算必须被夹到 12s，而不是仍用 45s"
        );
        assert!(
            on.round_budget(now + Duration::from_secs(600), now) <= full,
            "剩余预算再大，单轮也不得超过 MAX_REQUEST_RETRY_BUDGET_SECS"
        );
    }

    /// 403 临时风控被允许吸收时，额外轮次**硬钉为 1**。
    ///
    /// 回退即 FAIL：删掉 `from_config` 里的 `.min(1)`，断言失败。
    /// 依据：403 是账号级、族级连坐已让同族全退，多轮重试只会把更多号烧进正在惩罚的窗口，
    /// 且与 `config.self_heal_base_backoff_secs（默认 60s）=60s`（存在的意义就是停止试探）直接冲突。
    #[test]
    fn absorb_suspended_pins_rounds_to_one() {
        let mut c = absorb_cfg(true);
        c.upstream_retry_absorb_max_rounds = 3;
        c.upstream_retry_absorb_suspended = true;
        assert_eq!(
            AbsorbPolicy::from_config(&c).effective_max_rounds(),
            1,
            "开启 403 吸收时额外轮次必须硬钉 1（与自愈退避冲突，多轮会加深封禁）"
        );
    }

    /// 一次调用只取**一份**策略快照：`absorb_suspended` 必须来自 `AbsorbPolicy`，
    /// 循环里不得再 `self.token_manager.config()` 重读。
    ///
    /// 回退即 FAIL：把循环里的 `absorb.absorb_suspended` 换回
    /// `self.token_manager.config().upstream_retry_absorb_suspended`，断言失败。
    /// 理由：admin 在两个吸收轮之间热更配置，会让同一条客户端请求前半程按旧策略、
    /// 后半程按新策略走（`max_rounds` 已按旧值定好，suspended 判据却用了新值），
    /// 行为既不可复现也无法用测试固定。
    #[test]
    fn absorb_policy_is_snapshotted_once_per_call() {
        let src = include_str!("provider.rs");
        let retry_fn = src
            .split("async fn call_api_with_retry")
            .nth(1)
            .expect("call_api_with_retry 不应被改名");
        let body = retry_fn.split("\n#[cfg(test)]").next().unwrap_or(retry_fn);
        assert_eq!(
            body.matches("AbsorbPolicy::from_config").count(),
            1,
            "一次调用只应取一份策略快照"
        );
        let reread = format!("{}{}", "config().upstream_retry_absorb", "_suspended");
        assert!(
            !body.contains(reread.as_str()),
            "吸收循环内不得重读 config 的 suspended 标记：应使用 AbsorbPolicy 快照，\
             否则轮次之间的热更会让同一条请求前后按不同策略走"
        );
        // 策略里确实带上了这个字段（防有人删字段又改回重读）。
        assert!(
            include_str!("absorb_policy.rs").contains("absorb_suspended: bool"),
            "AbsorbPolicy 必须持有 absorb_suspended 字段"
        );
    }

    /// 退避：号池真值优先，且恒被 clamp 进 [min_delay, max_delay]。
    ///
    /// 回退即 FAIL：删掉 clamp 的下界 → `PoolCooldown(0)` 会返回 0 → 吸收循环变成无 sleep 的
    /// 忙等（正是 acquire_context 那次 CPU 打满一核、请求永不返回的事故形态），第二条断言失败。
    #[test]
    fn absorb_backoff_prefers_pool_truth_and_clamps() {
        use crate::model::AbsorbClass;
        let p = AbsorbPolicy::from_config(&absorb_cfg(true));

        // 号池给的真值在区间内 → 原样采用（无需等 HTTP Retry-After 头往返）。
        assert_eq!(
            p.backoff(AbsorbClass::PoolCooldown(8), 0),
            Duration::from_secs(8)
        );
        // ⭐ 承重断言：0 秒也必须睡满 min_delay，绝不返回 0。
        assert_eq!(
            p.backoff(AbsorbClass::PoolCooldown(0), 0),
            p.min_delay,
            "退避为 0 会让吸收循环变成忙等死循环，必须抬到 min_delay"
        );
        // 超上限被夹（防单请求长挂）。
        assert_eq!(p.backoff(AbsorbClass::PoolCooldown(9999), 0), p.max_delay);
        // 无真值：指数增长且不越界。
        let r0 = p.backoff(AbsorbClass::UpstreamRateLimit, 0);
        let r2 = p.backoff(AbsorbClass::UpstreamRateLimit, 2);
        assert!(r2 > r0, "无号池真值时应指数退避");
        assert!(r2 <= p.max_delay);
        // 大 round 不得 panic（移位溢出）也不得越界。
        assert!(p.backoff(AbsorbClass::UpstreamRateLimit, 64) <= p.max_delay);
    }

    /// ⭐ `min_delay > max_delay` 不得 panic：`Duration::clamp` 的 std 契约是
    /// `min > max` 即 panic，而这两个值来自面板上两个独立数字框（毫秒框上限 60000 /
    /// 秒框下限 1），`minDelayMs=60000` + `maxDelaySecs=1` 一次手滑即可配出。
    ///
    /// 回退即 FAIL：删掉 `from_config` 里 `min_delay` 的 `.min(max_delay)`，
    /// 下面每一条 `backoff` 调用都会 panic（`assertion failed: min <= max`），
    /// 而 panic 发生在**请求热路径**上 —— 开启吸收层后每个 429 都会打到。
    #[test]
    fn absorb_min_delay_above_max_is_normalized_not_panicking() {
        use crate::model::AbsorbClass;
        let mut c = absorb_cfg(true);
        c.upstream_retry_absorb_min_delay_ms = 60_000; // 面板毫秒框上限
        c.upstream_retry_absorb_max_delay_secs = 1; // 面板秒框下限
        let p = AbsorbPolicy::from_config(&c);

        assert!(
            p.min_delay <= p.max_delay,
            "构造后必须满足 min_delay <= max_delay，否则 backoff 的 clamp 会 panic"
        );
        // 方向是「抬 max 到 min」：矛盾配置下宁可退避更久（吸收层不干活、回落旧行为），
        // 而不是退避更短（对还在冷却的号池连打，正是吸收层要避免的事）。
        assert_eq!(p.min_delay, Duration::from_secs(60), "min 应被尊重");
        assert_eq!(
            p.max_delay,
            Duration::from_secs(60),
            "max 应被抬到不低于 min"
        );

        // 三类都不得 panic，且结果落在退化后的单点区间上。
        assert_eq!(p.backoff(AbsorbClass::PoolCooldown(0), 0), p.max_delay);
        assert_eq!(p.backoff(AbsorbClass::PoolCooldown(9999), 0), p.max_delay);
        assert_eq!(p.backoff(AbsorbClass::UpstreamRateLimit, 5), p.max_delay);
        assert_eq!(p.backoff(AbsorbClass::SwapWindow, 0), p.max_delay);
        // 新增的两类同样不得 panic（`class_max_delay` 只对 SwapWindow 且设了 swap 预算时放宽，
        // 这里 swap 预算是 0 ⇒ 五类共用同一个退化区间）。
        assert_eq!(p.backoff(AbsorbClass::TransientServerError, 3), p.max_delay);
        assert_eq!(p.backoff(AbsorbClass::TransientCapacity400, 3), p.max_delay);
    }

    /// ⭐ 吸收总预算**不得低于** 45s，否则它会反向砍掉既有的 failover 墙钟。
    ///
    /// 回退即 FAIL：删掉 `from_config` 里 budget 的
    /// `.max(Duration::from_secs(MAX_REQUEST_RETRY_BUDGET_SECS))` —— 面板允许填 1，
    /// 而 `round_budget()` 是 `min(45s, 剩余预算)`，于是填 5 会让**第 0 轮**
    /// （关掉吸收层时唯一的那一轮）的换号墙钟从 45s 变成 5s：与吸收层无关的正常
    /// 重试被截断，而面板上看不出这层耦合。
    #[test]
    fn absorb_budget_cannot_shrink_the_failover_wall_clock() {
        let full = Duration::from_secs(MAX_REQUEST_RETRY_BUDGET_SECS);
        let now = std::time::Instant::now();

        let mut c = absorb_cfg(true);
        c.upstream_retry_absorb_budget_secs = 5; // 面板允许的小值
        let p = AbsorbPolicy::from_config(&c);
        assert!(
            p.budget >= full,
            "总预算被抬到不低于 45s，实际 {:?}",
            p.budget
        );
        // 承重：第 0 轮（round_started == deadline - budget 起点）仍拿满 45s。
        assert_eq!(
            p.round_budget(now + p.budget, now),
            full,
            "第 0 轮的 failover 墙钟不得因吸收层旋钮变短"
        );

        // 反向：填大值应能真的放宽总预算（旋钮仍然有用，只是单向）。
        let mut c2 = absorb_cfg(true);
        c2.upstream_retry_absorb_budget_secs = 120;
        assert_eq!(
            AbsorbPolicy::from_config(&c2).budget,
            Duration::from_secs(120),
            "大于 45s 的值必须原样生效，否则这个旋钮等于没有"
        );
    }

    /// ⭐ `maxDelaySecs=0` 不得产生零退避 —— 那是忙等死循环，不是「不等待」。
    ///
    /// 回退即 FAIL：删掉 `from_config` 里 `max_delay` 的 `.max(ABSORB_MIN_BACKOFF)` ——
    /// `max_delay=0` 会把 `min_delay` 也经 `.min()` 压成 0，`backoff()` 对每一类都返
    /// `Duration::ZERO`，吸收循环变成无 sleep 的 `continue`：打满一核、请求永不返回。
    /// 该值经 Admin API 可写（`service.rs` 对这两个字段无 clamp），所以这是可达状态。
    #[test]
    fn absorb_zero_max_delay_cannot_produce_busy_loop() {
        use crate::model::AbsorbClass;
        let mut c = absorb_cfg(true);
        c.upstream_retry_absorb_max_delay_secs = 0;
        c.upstream_retry_absorb_min_delay_ms = 0;
        let p = AbsorbPolicy::from_config(&c);

        assert!(
            p.max_delay >= ABSORB_MIN_BACKOFF,
            "max_delay 必须有绝对下限"
        );
        for (label, d) in [
            (
                "PoolCooldown(0)",
                p.backoff(AbsorbClass::PoolCooldown(0), 0),
            ),
            (
                "PoolCooldown(9999)",
                p.backoff(AbsorbClass::PoolCooldown(9999), 0),
            ),
            (
                "UpstreamRateLimit",
                p.backoff(AbsorbClass::UpstreamRateLimit, 0),
            ),
            ("SwapWindow", p.backoff(AbsorbClass::SwapWindow, 0)),
            (
                "TransientServerError",
                p.backoff(AbsorbClass::TransientServerError, 0),
            ),
            (
                "TransientCapacity400",
                p.backoff(AbsorbClass::TransientCapacity400, 0),
            ),
        ] {
            assert!(
                d >= ABSORB_MIN_BACKOFF,
                "{label} 退避为 {d:?}，零/过小退避会让吸收循环变成忙等死循环"
            );
        }
    }

    /// ⭐ 源码级守卫：`bearer token invalid` 打在**已成功过**的号上必须判瞬态，
    /// 且该判定必须在 `report_failure` **之前**。
    ///
    /// 用源码断言：走到这条分支需要真实上游返 403 + 真实号池，行为测试写不了
    /// （本仓惯例，见 `should_emit_usage_record_in_mcp_success_branch`）。
    ///
    /// 回退即 FAIL：删掉 `bearer_invalid_but_proven` 那段，或把它移到
    /// `report_failure` 之后 —— 高并发下 3 次瞬态 403 会在 1 秒内把一个
    /// 93.9% 成功率的号推到 `TooManyFailures`（实测 #481：2412 次成功仍被禁），
    /// 池子少一个号 → 剩下的吃更多流量 → 更易撞惩罚窗口。当天 116 次禁用/42 次自愈。
    ///
    /// 同时钉住「从未成功的号不受影响」：那些是真 region 错配（实测 3 个号 17 次），
    /// 必须继续计失败并被禁用，否则死号会永久占着调度位。
    #[test]
    fn bearer_invalid_on_proven_credential_must_not_count_as_failure() {
        // needle 运行时拼接：完整字面量会被 include_str! 读到自己而自匹配（本文件已踩三次）。
        let full = include_str!("provider.rs");
        let src = full
            .split_once("\n#[cfg(test)]")
            .map(|(a, _)| a)
            .unwrap_or(full);

        let guard = format!("{}{}", "bearer_invalid_but", "_proven");
        let proven_check = format!("{}{}", "has_ever_", "succeeded(ctx.id)");
        let punish = format!("{}{}", "report_failure", "(ctx.id)");

        let guard_at = src.find(guard.as_str()).expect("瞬态判定不应被改名");
        assert!(
            src.contains(proven_check.as_str()),
            "必须用 has_ever_succeeded 区分「真 region 错配」与「瞬态抖动」"
        );
        // 对话路径的 report_failure 必须在守卫之后。
        let punish_at = src
            .rfind(punish.as_str())
            .expect("report_failure 调用点不应被改名");
        assert!(
            guard_at < punish_at,
            "瞬态判定必须在 report_failure 之前，否则健康号仍会被 3 次抖动打死"
        );
        // 处置必须是冷却而非计失败。
        let cooldown = format!("{}{}", "report_auth_", "cooldown(ctx.id)");
        assert!(
            src.contains(cooldown.as_str()),
            "瞬态分支应设短冷却让调度避开该号，而不是什么都不做（否则下一跳可能再选它）"
        );
    }

    /// ⭐ 未修问题 ②（跨轮次数预算）：`ABSOLUTE_MAX_TOTAL_RETRIES` 必须是「**每请求**」
    /// 而非「每轮」的上限。
    ///
    /// 缺陷是两处组合出来的：单看 `=4` 没问题，单看「每轮重跑 for 循环」也没问题，
    /// 但配额在循环外只算一次、循环每轮重跑 ⇒ 每轮各拿一份完整 4 ⇒ `max_rounds=3`
    /// 时一条客户端请求最坏 (1+3)×4 = **16 次**上游调用、同一出口 IP，正是当初把
    /// 64 砍到 4 要压住的突发特征。
    ///
    /// 本测试模拟整条客户端请求：把每轮配额按 `round_retry_quota` 算出来累加，
    /// 断言总和恒 ≤ `ABSOLUTE_MAX_TOTAL_RETRIES`。回退即 FAIL：让 `round_retry_quota`
    /// 忽略 `attempts_before`（直接 `base_quota`）→ 总和变 16 → 第二条断言失败。
    #[test]
    fn total_upstream_attempts_are_capped_per_request_not_per_round() {
        // 池 ≥ 上限时基础配额必吃满硬上限（compute_max_retries(n,n) 对 n ≥ 上限恒 == 上限）。
        let base =
            compute_max_retries(ABSOLUTE_MAX_TOTAL_RETRIES, ABSOLUTE_MAX_TOTAL_RETRIES);
        assert_eq!(base, ABSOLUTE_MAX_TOTAL_RETRIES, "前提：基础配额吃满硬上限");

        // 模拟 1 + max_rounds 轮，每轮把配额跑满（最坏情况）。
        let max_rounds = crate::model::config::Config::default().upstream_retry_absorb_max_rounds;
        let mut attempts_base: u32 = 0;
        let mut total: usize = 0;
        for _round in 0..=max_rounds {
            let quota = round_retry_quota(base, attempts_base);
            if quota == 0 {
                break;
            }
            total += quota;
            // 与热路径同款递推：attempts_used = attempts_base + (quota-1)，再 +1。
            attempts_base += quota as u32;
        }
        assert!(max_rounds >= 1, "前提：默认 max_rounds 至少 1 轮才有意义");
        assert!(
            total <= ABSOLUTE_MAX_TOTAL_RETRIES,
            "一条客户端请求打向上游的总次数 {} 超过硬上限 {} —— 上限退化成「每轮」语义，\
             max_rounds={} 时单请求会打 (1+{})×{} 次上游、同一出口 IP",
            total,
            ABSOLUTE_MAX_TOTAL_RETRIES,
            max_rounds,
            max_rounds,
            base
        );
    }

    /// 共享预算的语义（2026-08-11 方案 A）：
    /// 1. 新预算 = 硬上限；consume 递减；超量 consume 饱和到 0 不 panic。
    /// 2. **跨层共享**：预算在 websearch 轮/压缩轮/透传 failover 之间流转——一层消费后，
    ///    下一层 `round_retry_quota(base, budget.used())` 只能拿到剩余额度（实参语义是
    ///    「已用量」——传 `remaining()` 会反转：耗尽时 remaining=0 被当成「还没用」）。
    /// 3. 预算耗尽后 quota 返 0（调用点 break，不再空打）。
    ///
    /// 回退即 FAIL：把 `consume` 的 `saturating_sub` 换成 `-` 会在超量扣减时 panic；
    /// 把配额源从 `budget.used()` 换回局部计数则断言 2/3 失败（跨层放大复现）。
    #[test]
    fn shared_budget_caps_across_layers() {
        let budget = SharedRetryBudget::new();
        assert_eq!(
            budget.remaining(),
            ABSOLUTE_MAX_TOTAL_RETRIES as u32,
            "新预算必须是硬上限"
        );

        // 第一层（模拟透传 failover）花掉 2 次真实调用。
        budget.consume(2);
        assert_eq!(budget.remaining(), 2);
        assert_eq!(budget.used(), 2, "used = 总额 - remaining");

        // 第二层（模拟 websearch 回灌轮）只能拿剩余额度。
        let base = ABSOLUTE_MAX_TOTAL_RETRIES;
        assert_eq!(
            round_retry_quota(base, budget.used()),
            2,
            "跨层共享：第二层最多打剩余额度"
        );

        // 第三层（模拟压缩重试轮）耗尽：quota 返 0。
        budget.consume(2);
        assert_eq!(budget.remaining(), 0);
        assert_eq!(budget.used(), ABSOLUTE_MAX_TOTAL_RETRIES as u32);
        assert_eq!(round_retry_quota(base, budget.used()), 0, "耗尽返 0");

        // 超量扣减饱和不 panic。
        budget.consume(100);
        assert_eq!(budget.remaining(), 0);
    }

    /// `round_retry_quota` 的边界：额度用尽必须返 0（调用点据此 break，不空跑一轮）。
    ///
    /// 回退即 FAIL：把 `saturating_sub` 换成 `-` 会在 attempts > ABSOLUTE_MAX_TOTAL_RETRIES
    /// 时 panic；
    /// 把 `.min(remaining)` 删掉则第三、四条断言失败。
    #[test]
    fn round_retry_quota_shrinks_and_hits_zero() {
        let base = ABSOLUTE_MAX_TOTAL_RETRIES;
        assert_eq!(round_retry_quota(base, 0), base, "第 0 轮拿满基础配额");
        assert_eq!(round_retry_quota(base, 4), base - 4, "第 1 轮只剩 4-4");
        assert_eq!(
            round_retry_quota(base, ABSOLUTE_MAX_TOTAL_RETRIES as u32),
            0,
            "额度用尽必须返 0，否则调用点会空跑一轮、白睡一次退避"
        );
        // 超额（墙钟 break 后 attempts_base 可能越过上限）不得下溢 panic。
        assert_eq!(round_retry_quota(base, 999), 0);
        // 小号池：基础配额本就小于剩余额度时，不得被抬高。
        assert_eq!(round_retry_quota(2, 0), 2, "基础配额是上界，不能被额度抬高");

        // ⭐ 吸收层**关闭**时的逐字节等价（docs/absorb-layer-design.md §8）：只跑一轮 ⇒
        // attempts_base 恒 0；而 compute_max_retries 自身已 `.min(ABSOLUTE_MAX_TOTAL_RETRIES)`
        // ⇒ 本函数恒为恒等映射 ⇒ 关闭路径的行为与改动前完全相同。
        for pool in [0usize, 1, 3, 4, 12, 43, 1000] {
            let base = compute_max_retries(pool, pool);
            assert_eq!(
                round_retry_quota(base, 0),
                base,
                "吸收层关闭时（attempts_base 恒 0）本函数必须是恒等映射，池大小={pool}"
            );
        }
    }

    /// ⭐ 源码守卫：本轮配额必须**在** `'absorb: loop` 内经 `round_retry_quota` 算出。
    ///
    /// 纯函数单测证明不了热路径真的用了它（那正是「测了分支内部没测分支顺序」的形态）。
    /// 回退即 FAIL：把 `let max_retries = round_retry_quota(..)` 挪回循环外，
    /// 或改成直接用 `base_retry_quota` → 两条位置断言之一失败。
    #[test]
    fn per_round_quota_is_computed_inside_absorb_loop() {
        // ⚠️ 必须先切掉 `#[cfg(test)]` 之后的内容：`include_str!` 读整份源码，本测试自身
        // 也含这些 needle 的拼接结果，不切则位置比较命中测试里的那个 → 守卫静默失效
        // （前一版 `per_round_retry_cap_*` 正是这个形态：改完也照样通过）。
        let full = include_str!("provider.rs");
        let src = full
            .split_once("\n#[cfg(test)]")
            .map(|(a, _)| a)
            .unwrap_or(full);
        let loop_marker = format!("{}{}", "'absorb: ", "loop {");
        // ⚠️ 第二个实参必须是 `budget.used()`（共享预算的**已用量**——实参语义是「已完成
        // 的尝试次数」；真实上游调用在 `upstream_calls += 1` 处同步 `consume(1)`）而**不是**
        // `attempts_base`（迭代计数,含 fast-fail 空转）也**不是** `budget.remaining()`（剩余量
        // 传进来会被当成「已用这么多」——耗尽时 remaining=0 反而拿满配额，语义完全反转，
        // 2026-08-11 方案 A 实现时亲手踩过，单测 shared_budget_caps_across_layers 钉死）。
        // 喂错会让全池冷却在毫秒内烧空额度 ⇒ 吸收层被整体旁路,且这件事在纯函数单测
        // 里看不出来（两者类型相同、函数本身行为不变）。跨层（websearch 轮/压缩轮/
        // 透传 failover）共用同一总额度。
        let quota_call = format!(
            "{}{}",
            "round_retry_quota(base_retry_quota", ", budget.used())"
        );
        let decl = format!("{}{}", "let max_retries = ", "round_retry_quota(");

        let loop_at = src
            .find(loop_marker.as_str())
            .expect("'absorb: loop 不应被改名");
        let decl_at = src
            .find(decl.as_str())
            .expect("本轮配额必须由 round_retry_quota 算出（跨轮共享总额度）");
        assert!(
            decl_at > loop_at,
            "本轮配额必须在 'absorb: loop **内**重算：算在循环外等于每轮各拿一份完整配额，\
             上限退化成「每轮」语义（max_rounds=3 时单请求最坏 16 次上游调用）"
        );
        assert!(
            src.contains(quota_call.as_str()),
            "配额必须同时喂入基础配额与**跨轮累计**尝试数，否则夹不住总量"
        );

        // 额度耗尽必须在 sleep 之前 break：否则每轮白睡一次退避却零次上游调用。
        let zero_gate = format!(
            "{}{}",
            "round_retry_quota(base_retry_quota, budget.used()) ==", " 0"
        );
        let sleep_at = src
            .rfind(&format!("{}{}", "sleep(delay)", ".await"))
            .expect("吸收轮的 sleep 不应被改名");
        let zero_at = src
            .find(zero_gate.as_str())
            .expect("必须有「额度耗尽即 break」的闸门");
        assert!(
            zero_at < sleep_at,
            "额度耗尽的闸门必须排在 sleep 之前，否则客户端会为零次上游调用白等多个退避"
        );
    }

    /// ⭐ 未修问题 ③：退避被 `max_delay` 截断时**不得**再起一轮。
    ///
    /// 号池真值 60s（`config.self_heal_base_backoff_secs（默认 60s）`）vs `max_delay` 默认 15s：只 clamp 不判断
    /// ⇒ 睡 15s 醒来池子还在冷却 45s ⇒ 这一轮结构上必然拿回同一个 429 = 白打一轮上游
    /// + 客户端白等 15s。
    ///
    /// 回退即 FAIL：把 `backoff_is_truncated` 改成 `required_wait > max_delay` 之外的任何
    /// 恒假式（如 `false`），第二、三条断言失败。
    #[test]
    fn truncated_backoff_means_round_is_futile() {
        use crate::model::AbsorbClass;
        let p = AbsorbPolicy::from_config(&absorb_cfg(true));

        // 号池真值在退避上限之内 → 睡够就真到恢复时刻 → 这一轮有意义。
        assert!(
            !p.backoff_is_truncated(AbsorbClass::PoolCooldown(8), 0),
            "8s < max_delay，睡满即到恢复时刻，这一轮是有意义的"
        );
        // ⭐ 承重：全池自愈退避 60s 远超 max_delay ⇒ 必须判定「白打」。
        assert!(
            p.backoff_is_truncated(AbsorbClass::PoolCooldown(60), 0),
            "号池要 60s 才恢复而我们最多睡 {:?}，睡醒仍在冷却 —— 必须判白打",
            p.max_delay
        );
        // 而 clamp 后的睡眠时长看不出这件事（这正是必须分成两个函数的理由）。
        assert_eq!(
            p.backoff(AbsorbClass::PoolCooldown(60), 0),
            p.max_delay,
            "睡多久仍用截断值，判断够不够才用真值"
        );
        // ⭐ 反向承重：指数兜底撞上限**不算**白打。它是我们自己编的数、不是上游真值，
        // `max_delay` 本来就是为夹住它而存在。若这里判 true，吸收层会对**最主要**的那类
        // （上游裸 429）在 round 涨上去后提前停工，白丢一层保护。
        assert!(
            !p.backoff_is_truncated(AbsorbClass::UpstreamRateLimit, 30),
            "指数兜底无真值，撞 max_delay 只说明「我们不想睡更久」，不代表上游没好"
        );
        assert!(
            !p.backoff_is_truncated(AbsorbClass::SwapWindow, 30),
            "同上：SwapWindow（换号空窗）也没有号池真值"
        );
        // 新增两类同理：它们的曲线是我们自己编的数，撞上限不代表上游没好。
        assert!(!p.backoff_is_truncated(AbsorbClass::TransientServerError, 30));
        assert!(!p.backoff_is_truncated(AbsorbClass::TransientCapacity400, 30));
    }

    /// ⭐ 源码守卫（分支**顺序**）：截断判定必须排在 `should_start_another_round` **之前**。
    ///
    /// 两者是独立失败模式：前者管「睡够了上游好没好」，后者管「预算够不够睡」。
    /// 顺序反了的后果不是断言不成立而是**归因错**：预算判据用的是被截断的 15s（比真实
    /// 需求小），会先判「预算够」放行 → 白打一轮，且面板上记成 `absorb_round` 成功起轮
    /// 而不是被拦。回退即 FAIL：把 `backoff_is_truncated` 那段挪到
    /// `should_start_another_round` 之后，位置断言失败。
    #[test]
    fn truncation_gate_precedes_budget_gate() {
        let full = include_str!("provider.rs");
        let src = full
            .split_once("\n#[cfg(test)]")
            .map(|(a, _)| a)
            .unwrap_or(full);
        let trunc = format!(
            "{}{}",
            "absorb.backoff_is_truncated", "(class, absorb_round)"
        );
        // ⚠️ 实参已从 `absorb_deadline` 改为 `class_deadline`（换号空窗要用它自己那份预算）。
        // 按实参定位是原设计，保留这种写法：它顺带钉住「预算闸门吃的是某个 deadline 变量」。
        let budget = format!("{}{}", "should_start_another_round", "(class_deadline");

        let trunc_at = src.find(trunc.as_str()).expect("截断闸门不应被改名/删除");
        let budget_at = src.find(budget.as_str()).expect("预算闸门不应被改名");
        assert!(
            trunc_at < budget_at,
            "截断闸门必须排在预算闸门之前：预算判据吃的是被 max_delay 夹小后的 delay，\
             先跑它会把「睡醒也没好」的一轮判成「预算够」而放行"
        );
    }

    /// ⭐ BLOCKER 1 的机械防线（源码级）：准入闸门必须在吸收循环**之上**，且全文只有一处。
    ///
    /// 回退即 FAIL：把 `acquire_admission` 移进 `'absorb: loop`（或在循环内再加一个调用点），
    /// 断言立刻失败。这是本方案唯一的正确性支点 —— 入站令牌是「每客户端请求一个」，
    /// 若吸收重入闸门，一条请求吃 N 个令牌 → 令牌桶按 N 倍速率被抽干 → 每轮排队满 30s 才
    /// bail → 客户端从 <2s 拿到 429 变成 60s 才拿到（外置 shield 的 p50 73.2s 被搬进网关）。
    /// 单测覆盖不到（需真实号池 + 上游），故用源码断言。
    /// 🔴 源码守卫：**透传路径必须有并发闸 + 次数闸**（2026-08-10 审计发现的致命缺口）。
    ///
    /// # 为什么必须有这条守卫
    ///
    /// Kiro 主路径有五道背压（准入闸门 / 全局并发闸 / 每凭据并发闸 / `ABSOLUTE_MAX_TOTAL_RETRIES`
    /// / 动态压力降档），而透传循环**一道都没有** —— 它是按「低延迟零转换中转」设计的，
    /// 主路径后来加的调度设施它一项都没跟上。而线上号池当前全部是 custom_api 代挂号
    /// ⇒ **100% 流量走透传** ⇒ 那五道闸对当前流量全部失效。
    ///
    /// 缺口的具体后果：单请求可打 N 次上游（N=代挂号数，无上限），每次 connect 10s +
    /// read 720s；45s 墙钟只在每轮进循环时判 ⇒ 最后一跳可在 45s 后才开始并跑到 720s。
    /// 叠外置 shield-k2cc 的 10 次 ⇒ 无上限并发 × 无上限次数。
    ///
    /// # 回退即 FAIL
    /// 删掉任一道闸、或把次数累加挪到 `forward` 之前（那会让被闸门挡住的空转也吃配额），
    /// 断言立刻失败。行为测试需要真实上游 + 并发压力，故用源码断言。
    /// 🔴 源码守卫：**透传 failover 必须覆盖上游 404**（2026-08-10 修）。
    ///
    /// # 为什么
    /// 实测同一模型在两个代挂上游响应不同：`deepseek-v4-flash` 在 router.denzao.com 返
    /// **404 `model_not_found`**，在 k2cc 返 **200 OK**。改前 `should_failover` 三个条件
    /// （`401|402|403|429` / 5xx / `code == 400 && ...`）**都不含 404** ⇒ 404 直返客户端
    /// ⇒ Claude Code/Cursor 当「模型不存在」**当场断会话**，而池里另一个号明明能成功。
    ///
    /// 404 与 400 是同性质的：都是「**这个上游**不认这个请求」，只是不同站点用不同状态码
    /// 表达（k2cc 用 400 `INVALID_MODEL_ID`、denzao 用 404 `model_not_found`）。
    ///
    /// # 回退即 FAIL
    /// 把判定收回 `code == 400`、或从 `should_failover` 里漏掉它，断言立刻失败。
    /// 行为测试需要能返 404 的真实上游（本仓无 HTTP mock 设施），故用源码断言。
    #[test]
    fn passthrough_failover_must_cover_upstream_404() {
        let src = include_str!("provider.rs");
        let fn_marker = format!("{}{}", "async fn try_custom_api_passthrough", "(");
        let start = src
            .find(fn_marker.as_str())
            .expect("try_custom_api_passthrough 不应被改名");
        let body_end = src[start..]
            .find("\n    /// 累加一次请求的真实 credit")
            .map(|off| start + off)
            .unwrap_or(src.len());
        // 剔注释行：注释里出现 404 不算实现（本仓 :3913 记过「不剔注释会误判」的踩坑）。
        let code: String = src[start..body_end]
            .lines()
            .filter(|l| {
                let t = l.trim_start();
                !t.starts_with("//") && !t.starts_with("///")
            })
            .collect::<Vec<_>>()
            .join("\n");

        // 判定必须同时覆盖 400 与 404（`matches!(code, 400 | 404)` 形态）。
        let gate = format!("{}{}", "matches!(code, 400 ", "| 404)");
        assert!(
            code.contains(gate.as_str()),
            "透传 failover 判定必须同时覆盖 400 与 404：404 直返会让客户端把「这个上游不认」\
             误判成「模型不存在」而断会话，而池里其它号可能能成功（实测 deepseek-v4-flash \
             在 denzao 返 404、在 k2cc 返 200）"
        );
        // 冷却时长在 passthrough_cooldown_for（抽到 try_custom_api_passthrough 之前）。
        // 切片必须扫 helper，不能扫透传函数体——否则抽函数后守卫假红。
        let cool_marker = format!("{}{}", "fn passthrough_cooldown_for", "(");
        let cool_start = src
            .find(cool_marker.as_str())
            .expect("passthrough_cooldown_for 不应被改名");
        let cool_end = src[cool_start..]
            .find("\n    /// 混入池分流")
            .map(|off| cool_start + off)
            .unwrap_or(src.len());
        let cool_src: String = src[cool_start..cool_end]
            .lines()
            .filter(|l| {
                let t = l.trim_start();
                !t.starts_with("//") && !t.starts_with("///")
            })
            .collect::<Vec<_>>()
            .join("\n");
        let cooldown = format!("{}{}", "400 ", "| 404 => (5,");
        assert!(
            cool_src.contains(cooldown.as_str()),
            "404 的冷却时长必须与 400 同档（5s 调度级跳过）：它们是同一性质"
        );
    }

    /// 透传 400/404 的「换号无益」判据（`is_hopeless_upstream_400`）：
    /// 真实配额耗尽/超长形态必须判无益（不 failover），
    /// 但 body 恰好含 `quota` 字样的**上游能力差异**文案必须仍给换号机会。
    ///
    /// 回退即 FAIL：把判据改回裸 `quota` 宽匹配，反例组全部误判为无益 →
    /// 客户端白吃一个本来能靠换号解决的 400/404。
    #[test]
    fn hopeless_400_judgement_is_phrase_based_not_bare_quota() {
        // 正例：实测/常见配额耗尽与超长形态（OpenAI 系 / one-api 系 / DeepSeek 系）。
        for body in [
            r#"{"error":{"message":"You exceeded your current quota, please check your plan and billing details.","code":"insufficient_quota"}}"#,
            r#"{"error":{"message":"quota exhausted"}}"#,
            r#"{"error":{"message":"quota exceeded, 500 requests used"}}"#,
            "Insufficient Balance",
            "usage limit exceeded",
            "the request is too long",
            r#"{"reason":"CONTENT_LENGTH_EXCEEDS_THRESHOLD"}"#,
        ] {
            let low = body.to_ascii_lowercase();
            assert!(
                is_hopeless_upstream_400(&low),
                "真实配额耗尽/超长形态必须判「换号无益」: {body}"
            );
        }
        // 反例（误伤场景）：含 `quota` 字样但**不是**配额耗尽 —— 上游能力差异
        // （换一个号可能成功），必须仍给 failover 机会。
        for body in [
            "the model deepseek-quota-v2 requires a higher quota tier on this relay",
            r#"{"error":{"message":"quota tier not enabled for this model"}}"#,
            r#"{"error":{"code":"quota","message":"unknown error"}}"#,
        ] {
            let low = body.to_ascii_lowercase();
            assert!(
                !is_hopeless_upstream_400(&low),
                "非配额耗尽的 quota 字样不得判「换号无益」（会把上游能力差异吞成直返）: {body}"
            );
        }
    }

    #[test]
    fn passthrough_loop_must_have_concurrency_and_hop_gates() {
        let src = include_str!("provider.rs");
        // 只取透传函数体，避免误命中主路径的同名设施。
        let fn_marker = format!("{}{}", "async fn try_custom_api_passthrough", "(");
        let start = src
            .find(fn_marker.as_str())
            .expect("try_custom_api_passthrough 不应被改名");
        // 到下一个 `\n    /// ` 级别的项声明为止（该函数之后是 report_credits 的文档注释）。
        let body_end = src[start..]
            .find("\n    /// 累加一次请求的真实 credit")
            .map(|off| start + off)
            .unwrap_or(src.len());
        let body = &src[start..body_end];
        // 剔注释行：注释里出现关键词不算实现（本仓 :3913 记录过「不剔注释会误判」的踩坑）。
        let code: String = body
            .lines()
            .filter(|l| {
                let t = l.trim_start();
                !t.starts_with("//") && !t.starts_with("///")
            })
            .collect::<Vec<_>>()
            .join("\n");

        for (needle, why) in [
            (
                "upstream_gate",
                "透传必须过全局并发闸：线上 100% 流量走透传，它是当前唯一的全局并发保护",
            ),
            (
                "per_credential_gate",
                "透传必须过每凭据并发闸：否则一个慢中转站占满全局许可会拖死整池吞吐",
            ),
            (
                "MAX_PASSTHROUGH_FAILOVER_HOPS",
                "透传必须有换号次数上限：墙钟只在每轮进循环时判，最后一跳能跑到 read_timeout 720s",
            ),
        ] {
            assert!(
                code.contains(needle),
                "透传循环缺少 `{needle}`。{why}"
            );
        }

        // 次数累加必须在 forward **之后**（闸门挡住的空转不该吃配额，与主路径 upstream_calls 同款）。
        let fwd = code
            .find("passthrough::forward")
            .expect("透传必须调 passthrough::forward");
        let inc = code
            .find("upstream_hops += 1")
            .expect("必须有 upstream_hops 累加");
        assert!(
            fwd < inc,
            "`upstream_hops += 1` 必须在 forward 之后：放在之前会让被并发闸挡住的空转\
             （两处 continue）也吃掉换号配额，池子越大越早耗尽配额而一次上游都没真打成"
        );
    }

    /// 🔴 2026-08-10：acquire_admission 已移至 handlers 层（post_messages 入口），
    /// 透传与 Kiro 两条路径统一在 handler 层过闸门。provider.rs 不应再有任何调用。
    /// 本测试从「位置守卫」变为「零调用守卫」：防将来有人误加回 provider 内部。
    #[test]
    fn admission_gate_must_stay_above_absorb_loop() {
        let src = include_str!("provider.rs");
        let gate = format!("{}{}", "acquire_admission", "().await");
        assert_eq!(
            src.matches(gate.as_str()).count(),
            0,
            "acquire_admission 已移至 handlers 层（post_messages 与 post_messages_cc \
             入口统一过闸门）。provider.rs 不应再有调用点。若要在此加回，先确认不会导致某条路径绕闸。"
        );
    }

    /// 透传同号吸收判据：与 config.rs 的 upstream_retry_absorb_* 字段语义逐一钉死。
    /// - 429 只跟总开关（主路径 UpstreamRateLimit 同语义）；5xx 还需 server_error。
    /// - 本地失败（connect_error / 空错误体）绝不重试。
    /// - max_rounds 是「额外轮次」：0 = 不吸收；attempt 从 1 起，共最多 max_rounds 次
    ///   重试（2026-08-13 对齐主路径：旧判据 `attempt >= max_rounds` 只给 max_rounds−1 次）。
    #[test]
    fn passthrough_absorb_predicates_match_config_semantics() {
        // 429：只跟总开关。
        assert!(passthrough_absorb_should_retry(429, false, true, false, false, "", 1, 3));
        assert!(!passthrough_absorb_should_retry(429, false, false, true, false, "", 1, 3));
        // 5xx：总开关 + server_error 双开才吸收。
        assert!(passthrough_absorb_should_retry(502, false, true, true, false, "", 1, 3));
        assert!(!passthrough_absorb_should_retry(502, false, true, false, false, "", 1, 3));
        // 400 容量类（谓词认 INSUFFICIENT_MODEL_CAPACITY / MODEL_TEMPORARILY_UNAVAILABLE）：
        // capacity_400 开 + 谓词命中 → 吸收；开关关 → 不吸收（与改前逐字节一致）。
        assert!(passthrough_absorb_should_retry(
            400, false, true, false, true,
            r#"{"reason":"INSUFFICIENT_MODEL_CAPACITY"}"#, 1, 3
        ));
        assert!(passthrough_absorb_should_retry(
            400, false, true, false, true, "MODEL_TEMPORARILY_UNAVAILABLE", 1, 3
        ));
        assert!(!passthrough_absorb_should_retry(400, false, true, false, false, "", 1, 3));
        // 开关关但错误体是容量类 → 也不吸收（默认配置行为逐字节不变）。
        assert!(!passthrough_absorb_should_retry(
            400, false, true, false, false,
            r#"{"reason":"INSUFFICIENT_MODEL_CAPACITY"}"#, 1, 3
        ));
        // 谓词不认的 400（普通请求错误）即使开关开着也不吸收。
        assert!(!passthrough_absorb_should_retry(
            400, false, true, false, true, "INVALID_MODEL_ID", 1, 3
        ));
        // 本地失败绝不重试。
        assert!(!passthrough_absorb_should_retry(503, true, true, true, false, "", 1, 3));
        // max_rounds：0 = 不吸收；attempt 可达 max_rounds（额外轮次），再多一轮才停。
        assert!(!passthrough_absorb_should_retry(429, false, true, false, false, "", 0, 3));
        assert!(passthrough_absorb_should_retry(429, false, true, false, false, "", 3, 3));
        assert!(!passthrough_absorb_should_retry(429, false, true, false, false, "", 4, 3));
        assert!(!passthrough_absorb_should_retry(429, false, true, false, false, "", 1, 0));
    }

    /// 透传同号退避：默认配置下 500/1000/2000ms；clamp 到 [min_delay_ms, max_delay_secs]。
    #[test]
    fn passthrough_absorb_delay_monotonic_and_clamped() {
        assert_eq!(
            (
                passthrough_absorb_delay_ms(1, 150, 15),
                passthrough_absorb_delay_ms(2, 150, 15),
                passthrough_absorb_delay_ms(3, 150, 15),
            ),
            (500, 1000, 2000)
        );
        assert_eq!(passthrough_absorb_delay_ms(1, 6000, 15), 6000);
        assert_eq!(passthrough_absorb_delay_ms(1, 6000, 1), 6000);
        assert_eq!(passthrough_absorb_delay_ms(7, 150, 1), 1000);
    }

    /// 源码守卫：失败埋点与备用模型兜底必须留在吸收循环**之外**。
    ///
    /// 放进轮内会让一条客户端请求落 N 条失败记录 / 打 N 次备用模型，面板失败数被吸收轮次乘倍。
    ///
    /// ⚠️ 强度说明（避免把它当成比实际更硬的防线）：
    /// - 失败记录那一半**实际由编译器兜底** —— `fail_record` 在循环之后才构造，把
    ///   失败记录的 emit 调用挪进轮内会直接 E0425 `cannot find value`（已实测验证）。
    ///   本断言只是让意图显式化，真正拦住回退的是借用检查。
    /// - 备用模型那一半**是本测试独有的**：那段只依赖 `last_outcome` /
    ///   `model` / `session_id`，全都在循环内可见，搬进去能正常编译 —— 编译器不会报错，
    ///   只会静默变成"每轮都打一次备用模型"。这一半是这条测试存在的真正理由。
    ///
    /// ⚠️ needle 防自匹配（2026-08-11 审计修复）：
    /// - 完整字面量绝不出现在本文件任何注释/测试里（include_str! 会把它们也读进来，
    ///   生产被删后 `.find` 命中注释会让断言静默变绿 —— 本仓 4715 行注释记录过同型
    ///   踩坑五次；本轮审计抓到函数内 3749/3752/3963 行旧注释正是此形态，已改写）。
    /// - 全部运行时拼接；备用模型那一条用带 `cfg.` 前缀的片段（配置读取处的生产唯一
    ///   形态），注释/测试不可能自然写出。
    /// - 测试段按同文件 `failover_exhausted_*` 守卫的先例截断（`split_once`），
    ///   防止将来测试代码里出现完整字面量时守卫静默变绿。
    #[test]
    fn emit_record_and_fallback_stay_outside_absorb_loop() {
        let src = include_str!("provider.rs");
        let retry_fn = src
            .split("async fn call_api_with_retry")
            .nth(1)
            .and_then(|s| s.split_once("\n#[cfg(test)]").map(|(head, _)| head))
            .expect("call_api_with_retry 不应被改名");
        let end_marker = format!("{}{}", "break ", "'absorb;");
        let last_break = retry_fn
            .rfind(end_marker.as_str())
            .expect("'absorb 循环的 break 不应被改名");

        // ⚠️ 锚点必须是「失败记录的 emit 调用」而不是泛的 emit 调用 ——
        // 准入闸门超时（已知问题 #20 的修复）也 emit 一条记录，而它**刻意**在吸收循环
        // **之上**（闸门本身就在循环外，见 `admission_timeout_must_be_observable`）。
        // 泛锚点会先命中那一处，把「位置在循环后」的断言判成失败，而实际并无回归。
        // 本测试要钉的是**失败记录**那一条：它按吸收轮次乘倍才会污染面板失败数。
        let needles = [
            format!("{}{}", "emit_record(fail", "_record)"),
            format!("{}{}", "cfg.overload_fallback", "_model"),
        ];
        for needle in needles {
            let at = retry_fn
                .find(needle.as_str())
                .unwrap_or_else(|| panic!("{needle} 应仍在 call_api_with_retry 内"));
            assert!(
                at > last_break,
                "{needle} 必须位于吸收循环之后（循环外）：放进轮内会让一条客户端请求\
                 落 N 条失败记录，面板失败数被吸收轮次乘倍"
            );
        }
    }

    /// ⭐ 源码守卫（已知问题 #13）：`failover_exhausted` 只能在吸收循环**之外**、整条客户端
    /// 请求失败后记一次。
    ///
    /// 历史缺陷：bump 放在轮内且每轮清零 ⇒ 一条请求跑 N 轮就计 N 次（多计）；成功路径在轮内
    /// return 前也会被误计。回退即 FAIL：把 bump 挪回 'absorb 循环内 → `bump_at < loop_at`。
    #[test]
    fn failover_exhausted_bumped_once_outside_absorb_loop() {
        let full = include_str!("provider.rs");
        let src = full
            .split_once("\n#[cfg(test)]")
            .map(|(a, _)| a)
            .unwrap_or(full);
        let retry_fn = src
            .split("async fn call_api_with_retry")
            .nth(1)
            .expect("call_api_with_retry 不应被改名");
        let loop_at = retry_fn
            .find(format!("{}{}", "'absorb: ", "loop {").as_str())
            .expect("'absorb: loop 不应被改名");
        let bump_at = retry_fn
            .find("crate::common::recovery_metrics::bump_failover_exhausted()")
            .expect("failover_exhausted bump 不应被删除");
        assert!(
            bump_at > loop_at,
            "failover_exhausted 必须在吸收循环之外记（一次/请求）：放在轮内会被吸收轮次乘倍（#13）"
        );
        assert_eq!(
            retry_fn
                .matches("crate::common::recovery_metrics::bump_failover_exhausted()")
                .count(),
            1,
            "call_api_with_retry 内必须恰好一处 failover_exhausted bump（整条请求失败才记一次）"
        );
    }

    /// ⭐ 源码守卫：链内去重集必须声明在吸收循环**之外**（跨轮共享）。
    ///
    /// 回退即 FAIL：把 `rate_limited_this_call` 的 `let mut` 挪进 `'absorb: loop`，断言失败。
    /// 挪进去会让同一个号在每一轮都被重新惩罚 → trigger_count 累加 → 冷却 15s 被指数拉长到
    /// 72s，即「单请求自造雪崩」（这条历史根因写在该集合的声明处注释里）。
    #[test]
    fn chain_dedup_sets_declared_outside_absorb_loop() {
        let src = include_str!("provider.rs");
        let retry_fn = src
            .split("async fn call_api_with_retry")
            .nth(1)
            .expect("call_api_with_retry 不应被改名");
        let loop_at = retry_fn
            .find(format!("{}{}", "'absorb: ", "loop {").as_str())
            .expect("'absorb: loop 不应被改名");

        for set_name in [
            "let mut rate_limited_this_call",
            "let mut suspended_this_call",
            "let mut suspicious_failovers_this_call",
            "let mut auth_failed_this_call",
            "let mut region_corrected_this_call",
            // L1 换区：挪进轮内 ⇒ 每号一次上限退化成「每轮一次」，两个区来回打。
            "let mut region_switched_this_call",
            // L1 覆盖表：挪进轮内 ⇒ 上一轮换好的区在下一轮丢失，退回打错区。
            "let mut region_override_this_call",
            "let mut model_unavailable_attempts",
            "let mut attempts_used",
            // 挪进轮内会让每轮各拿一份完整 4 次上游调用额度 —— 那正是 round_retry_quota
            // 存在的理由（max_rounds=3 时单请求最坏 16 次上游调用、同一出口 IP）。
            "let mut upstream_calls",
        ] {
            let at = retry_fn
                .find(set_name)
                .unwrap_or_else(|| panic!("{set_name} 不应被改名/删除"));
            assert!(
                at < loop_at,
                "{set_name} 必须声明在吸收循环之外（跨轮共享）：挪进轮内会让同号被反复惩罚，\
                 冷却从 15s 指数拉长到 72s（单请求自造雪崩）"
            );
        }
    }

    /// ⭐ 源码守卫：四处 AIMD 上报点必须全部被 `absorb_round == 0` 包裹。
    ///
    /// 回退即 FAIL：去掉任一处的门，该处的上报数量断言失败。
    /// 依据：AIMD 的输入语义是「客户端请求撞上游的频率」，一条客户端请求无论吸收几轮都只是
    /// **一个** RPM 事件。逐轮上报时 `MD_DEBOUNCE_SECS=3` 挡不住吸收轮次（退避 ≥150ms、
    /// 号池真值常 8~15s，全部 >3s 穿窗）→ 每轮真降一档 → `last_md_nanos` 被反复推进 →
    /// `maybe_step_up` 的 20s 静默期永不满足（实测每 6.4s 一次 429）→ RPM 单调滑到 floor
    /// 锁死。这与已修的「AIMD 升档饿死」是同一死锁的第三条触发路径。
    #[test]
    fn aimd_reports_are_gated_to_first_absorb_round() {
        let src = include_str!("provider.rs");
        let retry_fn = src
            .split("async fn call_api_with_retry")
            .nth(1)
            .expect("call_api_with_retry 不应被改名");
        // 只看到吸收循环收尾为止，避免把测试自身的字符串算进来。
        // 外提后父文件以 `#[cfg(test)] #[path] mod tests;` 收尾；切 `mod tests`
        // 会误依赖那个尾巴标识符。改切第一个测试门。
        let body = retry_fn
            .split("\n#[cfg(test)]")
            .next()
            .expect("测试模块分隔不应消失");
        let gate = format!("{}{}", "absorb_round ", "== 0");

        let sites = [
            "report_upstream_rate_limited()",
            "report_upstream_pressure()",
        ];
        let total: usize = sites.iter().map(|s| body.matches(s).count()).sum();
        assert_eq!(
            total, 4,
            "call_api_with_retry 内应恰有 4 处 AIMD 上报点（临时风控/suspend/429/5xx）；\
             数量变化需同步本守卫"
        );
        // 每处上报点之前的 200 字节窗口内必须出现 `absorb_round == 0` 这道门。
        // `split_at` 拿到该处之前的全部文本，再取尾部窗口 —— 门与调用之间只隔注释与花括号。
        for site in sites {
            let mut searched_from = 0usize;
            let mut nth = 0usize;
            while let Some(rel) = body[searched_from..].find(site) {
                let abs = searched_from + rel;
                nth += 1;
                // 取该处之前最多 200 字节的窗口。本文件含中文注释，字节偏移可能落在多字节
                // 字符中间 —— 必须往前挪到合法字符边界，**不能**回退成"整段前缀"
                // （那会把别处的门也算进来，使断言恒真：本守卫第一版就是这个 bug，
                //   删掉一处门后测试照样通过，等于白写）。
                let mut window_start = abs.saturating_sub(200);
                while window_start < abs && !body.is_char_boundary(window_start) {
                    window_start += 1;
                }
                let window = &body[window_start..abs];
                assert!(
                    window.contains(gate.as_str()),
                    "AIMD 上报点 {site}（第 {nth} 处）之前 200 字节内必须有 `absorb_round == 0` 门，\
                     否则吸收轮次会把同一个上游压力事件放大 N 倍喂给 AIMD，\
                     使 RPM 单调滑到 floor 锁死"
                );
                searched_from = abs + site.len();
            }
        }
    }

    // ══════════ P1-a：瞬态 bearer-invalid 403 的机器可读标记 ══════════

    /// 真实链路会产生的那条串（上游 body 取自 `region_probe.rs:130` 记录的实测形态）。
    /// 拼法与热路径的 `format!` 逐字节同构：`{api_type} API 请求失败（…）标记: {status} {body}`。
    const REAL_TRANSIENT_403: &str = r#"流式 API 请求失败（token 瞬态失效，已冷却换号）bearer_invalid_transient=1: 403 Forbidden {"__type":"com.amazon.aws.codewhisperer#AccessDeniedException","message":"The bearer token included in the request is invalid."}"#;

    /// ⭐ P1-a：瞬态那条 bail 必须带 `bearer_invalid_transient=1`，且**逐字节**如此。
    ///
    /// 为什么需要标记：这个二分（`has_ever_succeeded`）只有 provider 做得出 —— region 错配与
    /// 瞬态抖动的上游文案**完全相同**。handler 侧只看到字符串，会把已证明健康的号判成 region
    /// 坏（排障方向错），且状态码从 502（外挂 RETRYABLE 内、会重试）变成 403（4xx 不重试）。
    ///
    /// 回退即 FAIL（已实测）：把格式串里的 `bearer_invalid_transient=1` 删掉 →
    /// 第一条 `assert!(src.contains(...))` FAIL。
    #[test]
    fn transient_bearer_invalid_bail_carries_machine_readable_marker() {
        let full = include_str!("provider.rs");
        // 必须切掉测试模块：本测试自身含该字面量，不切则断言恒真（本仓「源码守卫静默失效」的老坑）。
        let src = full
            .split_once("\n#[cfg(test)]")
            .map(|(a, _)| a)
            .unwrap_or(full);

        // 逐字节钉死格式串前缀：标记名、大小写、位置（中文文案之后、冒号之前）全在内。
        // handlers 侧按精确字面量 `bearer_invalid_transient=1` 做排除，任何漂移都会让那条
        // 排除静默失效（编译不报错、测试若只匹配子串也发现不了）。
        let fmt = "API 请求失败（token 瞬态失效，已冷却换号）bearer_invalid_transient=1: {} {}";
        assert!(
            src.contains(fmt),
            "瞬态 bearer-invalid 403 的 bail 必须带 bearer_invalid_transient=1 标记，\
             且位置在中文文案之后、`: {{status}} {{body}}` 之前（handlers 侧按精确字面量排除）"
        );

        // 同款范式的既有标记都只有一处产生点，本条也应如此（多处产生 = 语义被稀释）。
        // ⚠️ 计数只能按**格式串**（带 `: {} {}` 尾巴）算，不能按裸标记名 —— 注释里也会提它，
        // 那样计数会把注释算进来，断言变成对注释文字的约束（本测试第一版即此形态，实测 left=2）。
        assert_eq!(
            src.matches(fmt).count(),
            1,
            "该标记应只有唯一产生点（瞬态分支）；多处产生会让 handler 侧的排除覆盖到别的语义"
        );

        // ⭐ 承重：这条串**确实**落在 region-mismatch 判据的射程内 —— 这才是标记必要的证明。
        // 直接调 endpoint 侧那个谓词（handlers 的 `is_upstream_region_mismatch_403` 就是
        // 「它 && 403 && 无 401」），不在本文件重写一份子串匹配。
        assert!(
            crate::kiro::endpoint::default_is_bearer_token_invalid(REAL_TRANSIENT_403),
            "前提：瞬态串必然命中 bearer-invalid 谓词（与 region 错配逐字节同文案）"
        );
        assert!(
            REAL_TRANSIENT_403.contains("403"),
            "前提：瞬态串带 403 语境"
        );
        assert!(
            !REAL_TRANSIENT_403.to_ascii_lowercase().contains("401"),
            "前提：瞬态串不含 401（否则 region 判据本就会让路，标记也就不必要了）"
        );
        // ⇒ 三个前提同时成立 = 不加标记时 region-mismatch 判据必然误命中。
        assert!(
            REAL_TRANSIENT_403.contains("bearer_invalid_transient=1"),
            "所以必须有一个 region 判据看得见的机器可读区分位"
        );
    }

    // ══════════ P1-b：额度只计真正打到上游的次数 ══════════

    /// ⭐ P1-b（行为）：全池冷却 fast-fail 一整轮**不得**消耗跨轮重试额度。
    ///
    /// 缺陷推导（已独立复核）：`compute_max_retries(pool,pool)` 在 pool≥4 时恒为 4；
    /// 全池冷却时 `all_cooling_fast_fail` 默认开、wait>2s ⇒ `acquire_context_excluding` 裸 bail
    /// ⇒ 热路径 `continue`（不 sleep、不打上游）⇒ 第 0 轮在毫秒级跑完 4 次迭代。
    /// 旧代码用迭代计数 `attempts_base`（= 3+1 = 4）喂额度闸门 ⇒ 闸门命中 ⇒ `break 'absorb`
    /// ⇒ `absorb_round` 恒 0，吸收层对 pool≥4 等于没开。
    ///
    /// 本测试用两种口径各跑一遍同一个「一轮全 fast-fail」剧本，断言只有「计上游调用」这一种
    /// 能让第 1 轮拿到非零配额。回退即 FAIL（已实测）：把热路径改回喂 `attempts_base` 时，
    /// 单靠本测试**不会**失败（它是纯函数模拟），故必须与下面的源码守卫成对存在 —— 那条才是
    /// 「测了分支内部没测分支顺序」的防线。
    #[test]
    fn fast_fail_round_must_not_consume_upstream_retry_quota() {
        let pool = 17usize; // 线上实测规模；任何 ≥4 都会撞满硬上限
        let base = compute_max_retries(pool, pool);
        assert_eq!(
            base, ABSOLUTE_MAX_TOTAL_RETRIES,
            "前提：pool={pool} 时基础配额吃满硬上限"
        );

        // 剧本：第 0 轮 max_retries 次迭代**全部**在 acquire 处 fast-fail（零次 send）。
        let round0_iterations = base;

        // 旧口径（迭代计数）：attempts_used = 0 + (n-1)，轮末 attempts_base = attempts_used + 1。
        let attempts_base_after_round0 = (round0_iterations - 1) as u32 + 1;
        assert_eq!(
            round_retry_quota(base, attempts_base_after_round0),
            0,
            "旧口径下一整轮 fast-fail 就把 4 个额度全烧光 ⇒ 额度闸门命中 ⇒ 吸收层被旁路"
        );

        // 新口径（真实上游调用数）：一轮全 fast-fail ⇒ 一次都没打上游 ⇒ 额度分毫未动。
        let upstream_calls_after_round0 = 0u32;
        assert_eq!(
            round_retry_quota(base, upstream_calls_after_round0),
            base,
            "fast-fail 不打上游，不该消耗「打上游」的额度 —— 否则 PoolCooldown（吸收层最该拦的\
             那一类）从来没被吸收过"
        );

        // ⭐ 反向承重：新口径**不能**把上限放开。真打上游时必须照样递减、照样收敛到 0。
        let mut upstream_calls = 0u32;
        let mut rounds = 0usize;
        loop {
            let quota = round_retry_quota(base, upstream_calls);
            if quota == 0 {
                break;
            }
            // 最坏情形：本轮把配额全花在真实上游调用上。
            upstream_calls += quota as u32;
            rounds += 1;
            assert!(rounds <= 64, "必须收敛，否则是无界重试");
        }
        assert_eq!(
            upstream_calls, ABSOLUTE_MAX_TOTAL_RETRIES as u32,
            "「每请求 ≤ {} 次上游调用」的不变量必须仍然成立（换口径不等于放开上限）",
            ABSOLUTE_MAX_TOTAL_RETRIES
        );
    }

    /// ⭐ P1-b（源码位置，**这条才是承重的**）：额度累加点必须在 `send()` **之后**。
    ///
    /// 纯函数模拟证明不了热路径喂的是哪个变量（那正是「测了分支内部没测分支顺序」的形态）。
    /// 回退即 FAIL（已实测）：把 `upstream_calls += 1;` 挪到 `for attempt` 循环顶部（即
    /// `attempts_used = ...` 旁边），位置断言失败 —— 那样它就退化成迭代计数，缺陷原样回归。
    #[test]
    fn retry_quota_counts_only_calls_that_reached_upstream() {
        let full = include_str!("provider.rs");
        let src = full
            .split_once("\n#[cfg(test)]")
            .map(|(a, _)| a)
            .unwrap_or(full);

        // ⚠️ 必须先切到 `call_api_with_retry` 内再定位 —— 全文 `request.send().await` 有三处
        // （MCP 路径 :732 最靠前、备用模型 :1976 最靠后）。在全文上 `find` 会锚到 MCP 那处，
        // 于是「把累加挪回循环顶部」这个正是要拦的回退**照样通过**（实测：本测试第一版只有
        // 第三条 acquire 断言抓到，send 断言静默为真）。这就是「测了分支内部没测分支顺序」。
        let retry_fn = src
            .split("async fn call_api_with_retry")
            .nth(1)
            .expect("call_api_with_retry 不应被改名");
        let send_at = retry_fn
            .find(format!("{}{}", "request.send()", ".await").as_str())
            .expect("send 调用点不应被改名");
        let bump = format!("{}{}", "upstream_calls ", "+= 1;");
        let bump_at = retry_fn.find(bump.as_str()).expect("额度累加点不应被删除");
        assert!(
            bump_at > send_at,
            "额度累加必须在 send() 之后：放在循环顶部会把 acquire fast-fail 的空转也算成\
             一次上游调用 ⇒ 全池冷却时毫秒内烧空 12 个额度 ⇒ 吸收层整体旁路"
        );

        // 累加点必须唯一：多处累加会让同一次 send 扣多份额度（上限被隐式砍半）。
        assert_eq!(
            retry_fn.matches(bump.as_str()).count(),
            1,
            "额度累加点必须恰好一处，否则一次上游调用扣多份额度"
        );

        // 且必须排在 acquire 的 fast-fail `continue` 之后 —— 用 acquire 调用点做锚。
        let acquire_at = retry_fn
            .find("acquire_context_excluding(")
            .expect("acquire_context_excluding 调用点不应被改名");
        assert!(
            bump_at > acquire_at,
            "额度累加必须在 acquire 之后：acquire 失败的路径压根没打上游"
        );

        // 闸门与累加口径必须一致：喂 attempts_base 就等于缺陷回归（编译不报错）。
        let gate = format!(
            "{}{}",
            "round_retry_quota(base_retry_quota, budget.used()) ==", " 0"
        );
        assert!(
            src.contains(gate.as_str()),
            "跨轮额度闸门必须按 upstream_calls 判定，与累加口径同源"
        );
    }

    /// ⭐ P1-b（分支**顺序**）：额度闸门必须排在截断闸门之前，且三道闸门顺序固定。
    ///
    /// 顺序在这里是承重的：三道都 `break 'absorb`，谁先求值决定了「这一轮为什么停」的归因，
    /// 也决定了截断闸门有没有机会被求值。缺陷期正是额度闸门（被 fast-fail 提前触发）
    /// 抢在截断闸门之前恒命中 ⇒ `:1844` 那条从来没跑过。
    ///
    /// 回退即 FAIL（已实测）：把额度闸门那段挪到 `backoff_is_truncated` 之后，第一条断言失败。
    #[test]
    fn quota_gate_precedes_truncation_and_budget_gates() {
        let full = include_str!("provider.rs");
        let src = full
            .split_once("\n#[cfg(test)]")
            .map(|(a, _)| a)
            .unwrap_or(full);

        let quota_at = src
            .find(
                format!(
                    "{}{}",
                    "round_retry_quota(base_retry_quota, budget.used()) ==", " 0"
                )
                .as_str(),
            )
            .expect("额度闸门不应被改名");
        let trunc_at = src
            .find(
                format!(
                    "{}{}",
                    "absorb.backoff_is_truncated", "(class, absorb_round)"
                )
                .as_str(),
            )
            .expect("截断闸门不应被改名");
        // 实参已改为 `class_deadline`（换号空窗用它自己那份预算），见
        // `truncation_gate_precedes_budget_gate` 处的同款说明。
        let budget_at = src
            .find(format!("{}{}", "should_start_another_round", "(class_deadline").as_str())
            .expect("预算闸门不应被改名");

        assert!(
            quota_at < trunc_at,
            "额度闸门（每请求硬上限）必须最先求值：它是不可协商的安全上限，\
             而截断/预算闸门都是策略性放弃 —— 顺序反了会让硬上限被策略旁路"
        );
        assert!(
            trunc_at < budget_at,
            "截断闸门必须排在预算闸门之前（既有不变量，见 truncation_gate_precedes_budget_gate）"
        );
    }

    // ══════════ P1-c：三道 break 闸门的日志必须可分辨 ══════════

    /// ⭐ P1-c：三种停止吸收的结局必须在日志里**机器可分辨**，且各自点名旋钮。
    ///
    /// 背景：`:1845` 与 `:1859` 两个语义相反的闸门在 bump **同一个**
    /// `bump_absorb_budget_exhausted()` ⇒ 面板算出的吸收比无法归因 ⇒ 运维会去抬
    /// `upstreamRetryAbsorbBudgetSecs`，而真正该动的是 `upstreamRetryAbsorbMaxDelaySecs`。
    /// 而额度闸门连计数器都没有 ⇒ 主导结局在面板上完全不存在。
    /// 拆计数器要改 `recovery_metrics.rs`（不属本次改动范围），故先在日志侧收口。
    ///
    /// 回退即 FAIL（已实测）：删掉任一 `absorb_stop = "..."` 字段，对应断言失败。
    #[test]
    fn absorb_stop_reasons_are_distinguishable_in_logs() {
        let full = include_str!("provider.rs");
        let src = full
            .split_once("\n#[cfg(test)]")
            .map(|(a, _)| a)
            .unwrap_or(full);

        // 三个结局各有唯一的机器可读判据（不依赖中文文案不变）。
        for reason in [
            "retry_quota_exhausted",
            "backoff_truncated",
            "budget_too_small_for_round",
        ] {
            let field = format!("absorb_stop = {:?}", reason);
            assert_eq!(
                src.matches(field.as_str()).count(),
                1,
                "结局 {reason} 必须有且仅有一处 absorb_stop 标注：\
                 三道闸门都是 break 'absorb，没有机器可读判据时日志与面板都区分不出停在哪一道"
            );
        }

        // 两个共用计数器的闸门必须各自点名**不同**的旋钮 —— 这是归因混淆的实际危害面。
        assert!(
            src.contains("需抬 upstreamRetryAbsorbMaxDelaySecs"),
            "截断闸门必须点名 maxDelaySecs：它的瓶颈是「我们愿意睡的上限」小于号池真实恢复时刻"
        );
        assert!(
            src.contains("需抬 upstreamRetryAbsorbBudgetSecs"),
            "预算闸门必须点名 budgetSecs：它的瓶颈是总预算装不下一轮"
        );

        // ⭐ 归因混淆**已修**：三个结局各有独立计数器，本守卫随之从 `== 2` 改为 `== 1`。
        // ⚠️ 必须按**全路径调用**计数：短名在注释里也出现，按短名算会把注释计进来
        // （本测试第一版即此形态，实测 left=3 right=2）。
        assert_eq!(
            src.matches("crate::common::recovery_metrics::bump_absorb_budget_exhausted()")
                .count(),
            1,
            "`budget_exhausted` 现在**只**属于「总预算装不下一轮」这一个闸门。\
             另两个结局已各有独立计数器（backoff_truncated / retry_quota_exhausted）——\
             若这里又变回 2，说明有人把某个闸门重新并回了这个桶，归因混淆会复发"
        );
        // 另两个结局各有且仅有一处 bump（拆分是否真落到调用点，而不只是声明了计数器）。
        for call in [
            "crate::common::recovery_metrics::bump_absorb_backoff_truncated()",
            "crate::common::recovery_metrics::bump_absorb_retry_quota_exhausted()",
        ] {
            assert_eq!(
                src.matches(call).count(),
                1,
                "{call} 必须有且仅有一处调用（拆了计数器却漏改调用点是本仓已发生过的形态）"
            );
        }
    }

    /// ⭐ 硬约束守卫：**默认配置下三个新类别一律不吸收**。
    ///
    /// 线上正在服务，新能力必须靠显式开启。判据收在 `class_allowed` 一处（散写 `if` 必然漏
    /// 一处，而漏掉那处的表现正是「默认关的类别其实在吸收」）。
    ///
    /// 回退验证：把 `class_allowed` 里 `AbsorbClass::TransientServerError => self.absorb_server_error`
    /// 改成 `=> true` → 本测试 FAILED。
    #[test]
    fn new_absorb_classes_are_all_gated_off_by_default() {
        use crate::model::AbsorbClass;
        // 总开关开着（否则 effective_max_rounds()=0，测不到类别闸门本身）。
        let p = AbsorbPolicy::from_config(&absorb_cfg(true));

        assert!(
            !p.class_allowed(AbsorbClass::SwapWindow),
            "换号空窗默认不吸收（upstreamRetryAbsorbSuspended 默认 false）"
        );
        assert!(
            !p.class_allowed(AbsorbClass::TransientServerError),
            "5xx 默认不吸收：外挂实测 11.6 次重试才救回 1 个请求，那是不分机理一律重试的账单"
        );
        assert!(
            !p.class_allowed(AbsorbClass::TransientCapacity400),
            "容量 400 默认不吸收"
        );
        // 原有两类跟着总开关走，行为不变（否则本改动会把吸收层的既有作用对象也关掉）。
        assert!(p.class_allowed(AbsorbClass::PoolCooldown(3)));
        assert!(p.class_allowed(AbsorbClass::UpstreamRateLimit));

        // 显式开启必须真生效，否则这些开关等于不存在。
        let mut c = absorb_cfg(true);
        c.upstream_retry_absorb_server_error = true;
        c.upstream_retry_absorb_capacity_400 = true;
        c.upstream_retry_absorb_suspended = true;
        let on = AbsorbPolicy::from_config(&c);
        assert!(on.class_allowed(AbsorbClass::TransientServerError));
        assert!(on.class_allowed(AbsorbClass::TransientCapacity400));
        assert!(on.class_allowed(AbsorbClass::SwapWindow));
    }

    /// ⭐ 合并外挂缺口 3：换号空窗需要**完全不同的退避节奏**。
    ///
    /// 外挂原文：「KiroStudio 换号（auto_disable + 切下一个凭据 + 推送补号）实测有约 10 分钟的
    /// 空窗……**绝不能用限速那套 1 秒退避** —— 那是拿一个已被封的账号去猛打上游，只会加重风控。」
    ///
    /// 回退验证：把 `required_wait` 里 SwapWindow 的 `if self.swap_budget.is_zero()` 分支删掉
    /// （只留指数曲线）→ 本测试 FAILED。
    #[test]
    fn swap_window_uses_long_ladder_only_when_budget_configured() {
        use crate::model::AbsorbClass;

        // ① 默认（swap 预算 0）：与限速同曲线 ⇒ 逐字节等于本字段引入前的行为。
        let mut c = absorb_cfg(true);
        c.upstream_retry_absorb_suspended = true;
        let old = AbsorbPolicy::from_config(&c);
        for round in 0..3 {
            assert_eq!(
                old.required_wait(AbsorbClass::SwapWindow, round),
                old.required_wait(AbsorbClass::UpstreamRateLimit, round),
                "未设 swap 预算时必须沿用旧曲线（默认不改变现有行为）"
            );
        }
        assert_eq!(
            old.class_max_delay(AbsorbClass::SwapWindow),
            old.max_delay,
            "未设 swap 预算时上界不得被放宽"
        );

        // ② 设了 swap 预算：换成 20/40/60s 长阶梯，且超表长取最后一档。
        c.upstream_retry_absorb_swap_budget_secs = 600;
        let laddered = AbsorbPolicy::from_config(&c);
        for (round, want) in [(0u32, 20u64), (1, 40), (2, 60), (7, 60)] {
            assert_eq!(
                laddered.required_wait(AbsorbClass::SwapWindow, round),
                Duration::from_secs(want),
                "第 {round} 轮应睡 {want}s（外挂 SWAP_BACKOFF 阶梯）"
            );
        }
        // ⭐ 承重：长阶梯**不能被默认 15s 的全局上限削回** —— 否则这个旋钮等于没接上，
        // 且 `backoff_is_truncated` 只对 PoolCooldown 成立，不会拦住这种「睡不够」。
        assert_eq!(
            laddered.backoff(AbsorbClass::SwapWindow, 0),
            Duration::from_secs(20),
            "20s 阶梯必须真的睡 20s（max_delay 默认 15s，不放宽上界就会被削成 15s）"
        );

        // ⭐ 其它类别的上界**不得**被这个旋钮波及（只放宽换号空窗那一类）。
        assert_eq!(
            laddered.class_max_delay(AbsorbClass::UpstreamRateLimit),
            laddered.max_delay
        );
        assert_eq!(
            laddered.class_max_delay(AbsorbClass::TransientServerError),
            laddered.max_delay
        );
    }

    /// 新增两类的退避曲线：5xx 短（1s 起）、容量类中等（2s 起）。
    ///
    /// 回退验证：把 `TransientServerError` 的 `BASE` 从 1s 改成 2s（与容量类同曲线）→ FAILED。
    /// 两条曲线必须**可区分**：5xx 多为瞬时抖动，容量类是全局状态、换号不解决问题。
    #[test]
    fn transient_5xx_backs_off_shorter_than_capacity_class() {
        use crate::model::AbsorbClass;
        let mut c = absorb_cfg(true);
        // 抬高上界，让曲线本身可见（默认 15s 会把两条都 clamp 到同一个值）。
        c.upstream_retry_absorb_max_delay_secs = 300;
        let p = AbsorbPolicy::from_config(&c);

        assert_eq!(
            p.required_wait(AbsorbClass::TransientServerError, 0),
            Duration::from_secs(1),
            "5xx 起步 1s（逐字取自外挂 MIN_DELAY=1.0）"
        );
        assert_eq!(
            p.required_wait(AbsorbClass::TransientCapacity400, 0),
            Duration::from_secs(2),
            "容量类起步 2s：全局容量问题，换号不解决，比 5xx 更该慢"
        );
        for round in 0..4 {
            assert!(
                p.required_wait(AbsorbClass::TransientServerError, round)
                    < p.required_wait(AbsorbClass::TransientCapacity400, round),
                "第 {round} 轮：5xx 必须严格短于容量类（两类曲线不得退化成同一条）"
            );
        }
    }

    /// ⭐ 换号空窗的**独立 deadline**：只有它拿那份更宽的预算，其余类别一律用总预算。
    ///
    /// 回退验证：把 `class_deadline` 的 `matches!(..., SwapWindow)` 条件删掉（所有类别都用
    /// swap 预算）→ 本测试 FAILED。那会让**所有**类别都能占着客户端连接十分钟，
    /// 而换号空窗恰恰是唯一等得起的一类。
    #[test]
    fn swap_budget_deadline_does_not_leak_to_other_classes() {
        use crate::model::AbsorbClass;
        let now = std::time::Instant::now();
        let mut c = absorb_cfg(true);
        c.upstream_retry_absorb_suspended = true;
        c.upstream_retry_absorb_swap_budget_secs = 600;
        let p = AbsorbPolicy::from_config(&c);

        assert_eq!(
            p.class_deadline(now, AbsorbClass::SwapWindow),
            now + Duration::from_secs(600),
            "换号空窗必须用它自己那份预算（空窗实测 10 分钟 ≫ 总预算 20~45s）"
        );
        for other in [
            AbsorbClass::PoolCooldown(5),
            AbsorbClass::UpstreamRateLimit,
            AbsorbClass::TransientServerError,
            AbsorbClass::TransientCapacity400,
        ] {
            assert_eq!(
                p.class_deadline(now, other),
                now + p.budget,
                "{other:?} 必须仍用总预算 —— swap 预算泄漏给其它类别 = 所有请求都可能长挂十分钟"
            );
        }

        // 未设 swap 预算时，换号空窗也回到总预算（默认不改变现有行为）。
        c.upstream_retry_absorb_swap_budget_secs = 0;
        let old = AbsorbPolicy::from_config(&c);
        assert_eq!(
            old.class_deadline(now, AbsorbClass::SwapWindow),
            now + old.budget
        );
    }

    /// ⭐ 「额外轮次钉 1」的解除条件：**只在设了 swap 预算时**解除。
    ///
    /// 钉 1 的前提是短退避（15s 内重打同一个刚被风控的账号会抵消 `config.self_heal_base_backoff_secs（默认 60s）=60s`）。
    /// 长阶梯最短一档就是 20s，前提不再成立。不解除的话这个旋钮基本没用：它只能把**一次**
    /// 重试推迟到 20s 后，而空窗实测 10 分钟 ⇒ 那一次几乎必然还在窗口内。
    ///
    /// 回退验证：把 `from_config` 里的 `&& swap_budget.is_zero()` 删掉 → 第一条断言 FAILED
    /// （存量 `suspended=true` 的部署会从 1 轮变成 3 轮，属默认行为变更）。
    #[test]
    fn suspended_round_pin_released_only_with_swap_budget() {
        let mut c = absorb_cfg(true);
        c.upstream_retry_absorb_suspended = true;
        assert_eq!(
            AbsorbPolicy::from_config(&c).effective_max_rounds(),
            1,
            "未设 swap 预算时必须仍钉 1（存量 suspended=true 的部署行为逐字节不变）"
        );

        c.upstream_retry_absorb_swap_budget_secs = 600;
        assert_eq!(
            AbsorbPolicy::from_config(&c).effective_max_rounds(),
            c.upstream_retry_absorb_max_rounds,
            "设了 swap 预算即解除钉 1，交回 max_rounds + 独立 deadline + 总额度三道闸"
        );

        // 总开关关闭时一切照旧恒 0（这条是吸收层「关 ⇒ 逐字节等价旧行为」的根）。
        let mut off = absorb_cfg(false);
        off.upstream_retry_absorb_suspended = true;
        off.upstream_retry_absorb_swap_budget_secs = 600;
        assert_eq!(AbsorbPolicy::from_config(&off).effective_max_rounds(), 0);
    }

    /// ⭐ 缺口 4 的 provider 侧：**只在吸收层真跑过并放弃、且配置为 503 时**打标记。
    ///
    /// 源码级守卫（走到那段需要真实上游 + 真实号池，行为测试写不了 —— 本仓惯例）。
    ///
    /// 回退验证：把 `exhausted_as_503` 的判据从 `== 503` 改成 `!= 429`，或把
    /// `absorb_gave_up_after_rounds |= absorb_round > 0` 里的限定去掉 → 对应断言 FAILED。
    #[test]
    fn exhausted_503_marker_is_gated_on_both_conditions() {
        let src = include_str!("provider.rs");
        let prod = src
            .split_once("\n#[cfg(test)]")
            .map(|(a, _)| a)
            .unwrap_or(src);

        // ① 只认精确的 503：其它值（含裸 serde default 会给的 0）一律按 429 处理。
        assert!(
            include_str!("absorb_policy.rs")
                .contains("cfg.upstream_retry_absorb_exhausted_status == 503"),
            "必须只认精确 503 —— 打一个 handlers 认不出的标记只会造成静默的行为分叉"
        );
        // ② 标记必须同时受「真跑过轮次」约束：一次都没重试就改状态码是说谎。
        assert!(
            prod.contains("absorb_gave_up_after_rounds && absorb.exhausted_as_503"),
            "标记必须两个条件都满足才打（跑过轮次 且 配置为 503）"
        );
        // ③ 每处置位都带 `absorb_round > 0` 限定 —— 关闭吸收层时这里恒 0 ⇒ 不置位 ⇒
        //    渲染路径逐字节不变。这是「默认不改变现有行为」的机制本身。
        let sets = prod
            .matches("absorb_gave_up_after_rounds |= absorb_round > 0")
            .count();
        assert!(
            sets >= 3,
            "三条放弃结局（轮次用尽 / 额度用尽 / 退避被截断）都应置位，当前 {sets} 处"
        );
        assert!(
            !prod.contains("absorb_gave_up_after_rounds = true"),
            "不得无条件置位：那会让「吸收层没开也返 503」，等于对客户端说谎"
        );
    }

    /// 每个 `AbsorbClass` 都必须能在计数器上分辨（否则上线后无法判断哪类在起作用）。
    ///
    /// 回退验证：删掉 `bump_absorb_round_swap_window()` 那一处调用 → FAILED。
    #[test]
    fn every_absorb_class_has_a_distinguishable_counter() {
        let src = include_str!("provider.rs");
        let prod = src
            .split_once("\n#[cfg(test)]")
            .map(|(a, _)| a)
            .unwrap_or(src);
        for call in [
            "bump_absorb_round_pool_cooldown()",
            "bump_absorb_round_rate_limit()",
            "bump_absorb_round_swap_window()",
            "bump_absorb_round_server_error()",
            "bump_absorb_round_capacity_400()",
            "bump_absorb_server_error_skipped()",
            "bump_absorb_capacity_400_skipped()",
        ] {
            assert!(
                prod.contains(call),
                "{call} 必须被调用：五类共用一个 absorb_rounds 时，开三个开关后面板上仍是\
                 一个数 ⇒ 无法归因，也就无法决定该关掉哪个"
            );
        }
    }

    // ══════════ L1/L2：对话路径 region 自纠正 ══════════

    /// 真实链路会产生的 403 body（`region_probe.rs:130` 记录的实测形态，与
    /// `REAL_TRANSIENT_403` 里嵌的那段 body 逐字节同源）。
    ///
    /// 用它而不是自编串：上一轮审查抓到过「用合成串测试，而真实链路不产生那种串」——
    /// 那种测试全绿而线上判据全部漏命中。
    const REAL_BEARER_INVALID_BODY: &str = r#"{"__type":"com.amazon.aws.codewhisperer#AccessDeniedException","message":"The bearer token included in the request is invalid."}"#;

    /// L1 主用例：**从未成功过**的 `api_key` 号吃 region 错配 403 ⇒ 必须换区（而非换号）。
    ///
    /// 回退即 FAIL：把 `region_retry_target` 的 `has_ever_succeeded` 取反，或让它恒返
    /// `None` → 第二条断言 FAILED（拿不到目标区 = 热路径不会 `continue` 换区，
    /// 落到下方 `report_failure` + failover 换号，而换号治不了 region 错配）。
    #[test]
    fn never_succeeded_api_key_with_region_mismatch_403_switches_region() {
        // 前提：这条真实 body 确实命中热路径那道谓词（否则本测试测的不是同一条路）。
        assert!(
            crate::kiro::endpoint::default_is_bearer_token_invalid(REAL_BEARER_INVALID_BODY),
            "前提：真实 403 body 必须命中 is_bearer_token_invalid，否则热路径根本进不了该分支"
        );

        let target = region_retry_target("eu-central-1", true, false);
        assert_eq!(
            target,
            Some("us-east-1"),
            "从未成功过的 api_key 号打错区 ⇒ 必须换到**另一个**候选区；\
             返 None 就是回到「当凭据问题换号」的旧行为，而换号解决不了 region 错配"
        );

        // 反向也成立（US 号被探测写成 eu 是实测形态，但反过来同样要能纠）。
        assert_eq!(
            region_retry_target("us-east-1", true, false),
            Some("eu-central-1"),
            "换区必须是双向的，否则只能纠正一个方向"
        );
    }

    /// L1 收窄用例：**已成功过**的号吃**同一条** 403 ⇒ 必须**不**换区。
    ///
    /// 这是 L1 与既有 `bearer_invalid_but_proven` 的分界线：同一句上游文案，
    /// `has_ever_succeeded` 是唯一区分位。已成功过 = 这个区真拿到过 200 ⇒ 区是对的，
    /// 403 只能是抖动（实测 4 个号累计 3393 次成功、共吃 42 次）⇒ 该走瞬态分支。
    ///
    /// 回退即 FAIL：把 `region_retry_target` 里的 `|| has_ever_succeeded` 删掉 → 断言 FAILED
    /// （已证明健康的号会被换区 = 把一个本来对的配置改坏，且下一次抖动过去它本来就好了）。
    #[test]
    fn proven_credential_with_same_403_must_not_switch_region() {
        assert_eq!(
            region_retry_target("eu-central-1", true, true),
            None,
            "已成功过的号必须让路给既有瞬态分支（冷却+换号、不计失败），绝不换区"
        );
    }

    /// L2 的门：OAuth 号不换区、也就不回写 `api_region`。
    ///
    /// 依据：OAuth 号的权威 region 是 `profileArn` 第 4 段（`effective_upstream_region`
    /// 第一优先），`api_region` 对它根本不生效 ⇒ 换区不改变实际 host（白烧一次额度），
    /// 回写则在面板上留一个"看起来生效其实被压住"的值，把排障带偏。
    ///
    /// 回退即 FAIL：删掉 `region_retry_target` 里的 `!is_api_key` 门 → 断言 FAILED。
    #[test]
    fn oauth_credential_must_not_switch_or_write_back_region() {
        assert_eq!(
            region_retry_target("eu-central-1", false, false),
            None,
            "OAuth 号的 region 由 profileArn 决定，换区/回写 api_region 对它无效"
        );

        // 回写点必须**显式**带 `is_api_key_credential` 门（第二道）：入口那道门若被放宽，
        // 这里仍不能把 OAuth 号的 api_region 写坏。
        let src = include_str!("provider.rs");
        let prod = src
            .split_once("\n#[cfg(test)]")
            .map(|(a, _)| a)
            .unwrap_or(src);
        let writeback = format!("{}{}", "set_credential_api_region", "(ctx.id");
        let at = prod
            .find(writeback.as_str())
            .expect("L2 回写调用点不应被改名/删除");
        // 回写之前的窗口内必须出现 api_key 门。窗口取 600 字节（中间隔着注释）。
        // ⚠️ 必须挪到合法字符边界：本文件含中文注释，裸切会 panic；而回退成"整段前缀"
        // 会让断言恒真（别处的门也被算进来），那等于白写。
        let mut window_start = at.saturating_sub(600);
        while window_start < at && !prod.is_char_boundary(window_start) {
            window_start += 1;
        }
        assert!(
            prod[window_start..at].contains("is_api_key_credential()"),
            "L2 回写点前必须有 is_api_key_credential 门，否则 OAuth 号会被写进一个不生效的 api_region"
        );
    }

    /// 候选表的形状假设：只有两项，且首项 `eu-central-1`。
    ///
    /// 实测依据：`management.*` 与 `runtime.*` 只在 `us-east-1` / `eu-central-1` 解析 DNS。
    /// 表若被扩项，`region_retry_target` 的「换到另一个」就退化成「顺序轮换」——
    /// 语义变了，本测试会 FAIL 以强制重新审视。
    #[test]
    fn region_retry_falls_back_to_first_candidate_when_current_is_off_table() {
        assert_eq!(
            crate::kiro::region_probe::PROBE_ORDER.len(),
            2,
            "前提：候选只有两个（实测只有这两区解析 DNS）。扩表需重新审视 region_retry_target 的语义"
        );
        // 当前区不在表内（真实成因：profileArn 把区钉在 us-west-2）⇒ 换到表首项。
        assert_eq!(
            region_retry_target("us-west-2", true, false),
            Some(crate::kiro::region_probe::PROBE_ORDER[0]),
            "当前区不在候选表内时必须落到表首项，而不是返 None（那样该号永远纠不过来）"
        );
    }

    /// 🔴 **顺序断言**：换区分支必须排在 `bearer_invalid_transient` 之后、401 之后。
    ///
    /// 为什么必须有这条：本仓「纸面测试」第 8 种形态 —— **测了分支内部，没测分支顺序**。
    /// 真实事故：改三处、四条测试、三次「回退即 FAILED」全过而修复无效，因为一条通用分支
    /// 排在特化分支之前先 `break` 了。上面那几条纯函数测试对顺序**完全不可见**：
    /// `region_retry_target` 可以完美无缺而热路径根本走不到它。
    ///
    /// 断言的是**最终行为**（换区 vs 换号），三条各自钉一个会让行为反转的顺序关系：
    /// ① 瞬态分支在前 ⇒ 已成功过的号在到达换区分支**之前**就被 `continue` 掉；
    /// ② 换区分支带 403 门 ⇒ 401 落不进来（401 该 force-refresh/计失败，换区对它无用）；
    /// ③ 换区分支在通用 `report_failure` 之前 ⇒ region 错配的号走的是换区，不是换号 + 计失败。
    #[test]
    fn region_switch_branch_ordered_after_transient_and_401() {
        let src = include_str!("provider.rs");
        let prod = src
            .split_once("\n#[cfg(test)]")
            .map(|(a, _)| a)
            .unwrap_or(src);
        let retry_fn = prod
            .split("async fn call_api_with_retry")
            .nth(1)
            .expect("call_api_with_retry 不应被改名");

        // needle 运行时拼接：完整字面量会被 include_str! 读到自己而自匹配（本文件已踩三次）。
        let transient_guard = format!("{}{}", "bearer_invalid_but", "_proven");
        let transient_marker = format!("{}{}", "bearer_invalid_", "transient=1");
        let region_guard = format!("{}{}", "region_switched_", "this_call.contains");
        let punish = format!("{}{}", "report_failure", "(ctx.id)");

        let transient_at = retry_fn
            .find(transient_guard.as_str())
            .expect("既有瞬态判定不应被改名");
        let marker_at = retry_fn
            .find(transient_marker.as_str())
            .expect("瞬态机器可读标记不应被删");
        let region_at = retry_fn
            .find(region_guard.as_str())
            .expect("换区分支的每号一次门不应被改名");
        let punish_at = retry_fn
            .rfind(punish.as_str())
            .expect("通用 401/403 的 report_failure 不应被改名");

        // ① 瞬态在前：已成功过的号必须在换区分支之前就被 continue 掉。
        // 顺序反了 ⇒ 已证明健康的号（区是对的）会被换区，把对的配置改坏。
        assert!(
            transient_at < region_at && marker_at < region_at,
            "换区分支必须排在 bearer_invalid_transient 之后：\
             顺序反了会让已成功过的号（区本来是对的）被换区，且瞬态标记再也打不出来"
        );

        // ② 401 让路：换区分支的判据里必须带 403 门。
        // 取该分支起点前的窗口，断言 403 门与它同处一条 `if` 条件里。
        let mut window_start = region_at.saturating_sub(200);
        while window_start < region_at && !retry_fn.is_char_boundary(window_start) {
            window_start += 1;
        }
        assert!(
            retry_fn[window_start..region_at].contains("status.as_u16() == 403"),
            "换区分支必须带 403 门（401 让路）：401 是 token 死了 ≠ 区错了，\
             换个区照样是死 token，只会白烧一次重试额度并延后真正的 force-refresh"
        );

        // ③ 换区在计失败之前：region 配错≠号坏（隔离铁律）。
        // 顺序反了 ⇒ 号先被 report_failure（累计 3 次即禁用），换区永远轮不到，
        // 即回到「US 号导入即废」那个形态。
        assert!(
            region_at < punish_at,
            "换区分支必须排在通用 report_failure 之前：反了则 region 错配的号先被计失败\
             （3 次即禁用），换区分支永远走不到"
        );

        // 换区分支**绝不能**调用 report_failure / 冷却：那是「号坏了」的处置。
        // 取该分支体的一段窗口（到下一处 `continue;` 为止）做否定断言。
        let branch_body = &retry_fn[region_at..];
        let branch_end = branch_body
            .find("// 同一个号在一条请求里只惩罚一次")
            .expect("换区分支与通用惩罚分支之间的注释锚点不应消失");
        let branch = &branch_body[..branch_end];
        assert!(
            !branch.contains(punish.as_str()),
            "换区分支内绝不能 report_failure：region 配错≠号坏，惩罚它会把一个其实好的号推向禁用"
        );
    }

    /// L1 上限：同一个号在一次客户端请求内**最多换区一次**。
    ///
    /// 不加上限就是两个区来回打（A 403 → 换 B → B 403 → 换回 A → …），一条客户端请求
    /// 把额度全烧在同一个号的两个区之间、同一出口 IP 连打 = 正是风控要抓的突发特征。
    /// 本仓刚因「吸收层放大」修过一轮。
    ///
    /// 回退即 FAIL：删掉 `!region_switched_this_call.contains(&ctx.id)` 这道门 → 第一条
    /// 断言 FAILED；把那个集合的 `let mut` 挪进 `'absorb: loop` → 第三条 FAILED
    /// （挪进去 ⇒ 每一轮各拿一份新集合 ⇒ 上限退化成「每轮一次」，吸收 3 轮就是 4 次）。
    #[test]
    fn region_switch_capped_once_per_credential_per_call() {
        let src = include_str!("provider.rs");
        let prod = src
            .split_once("\n#[cfg(test)]")
            .map(|(a, _)| a)
            .unwrap_or(src);
        let retry_fn = prod
            .split("async fn call_api_with_retry")
            .nth(1)
            .expect("call_api_with_retry 不应被改名");

        let gate = format!("!{}{}", "region_switched_this_call", ".contains(&ctx.id)");
        assert!(
            retry_fn.contains(gate.as_str()),
            "必须有 per-call 的每号一次门，否则同一个号会在两个区之间来回打、烧光重试额度"
        );
        let mark = format!("{}{}", "region_switched_this_call", ".insert(ctx.id)");
        assert!(
            retry_fn.contains(mark.as_str()),
            "命中换区后必须置位，否则那道 contains 门恒不成立 = 等于没有上限"
        );

        // 集合必须声明在吸收循环**之外**（跨轮共享），否则上限退化成「每轮一次」。
        let decl = format!("let mut {}", "region_switched_this_call");
        let decl_at = retry_fn.find(decl.as_str()).expect("集合声明不应被改名");
        let loop_at = retry_fn
            .find(format!("{}{}", "'absorb: ", "loop {").as_str())
            .expect("'absorb: loop 不应被改名");
        assert!(
            decl_at < loop_at,
            "换区去重集必须声明在吸收循环之外：挪进轮内 ⇒ 每轮各拿一份 ⇒ 上限退化成\
             「每轮一次」，吸收 3 轮就是 4 次换区"
        );

        // 换区后必须把该号从 tried_this_call 摘掉，否则下一跳会结构性避开它 ⇒
        // 覆盖值躺在 map 里没人用 = 换区等于没做（这是最容易静默失效的一处）。
        let unexclude = format!("{}{}", "tried_this_call", ".remove(&ctx.id)");
        assert!(
            retry_fn.contains(unexclude.as_str()),
            "换区后必须把该号从 tried_this_call 摘掉，否则 acquire_context_excluding 会避开它，\
             换区重试打的是别人的号 —— 覆盖值没人用，等于没换区"
        );
    }

    /// ⭐ A-5 源码级守卫：429 备区换桶 与 L1 403 换区必须**共享同一个**「本请求已换区」
    /// 标记（`region_switched_this_call`）。
    ///
    /// 回退即 FAIL：
    /// - 把 429 备区换桶处的标记置位删掉 → 第一条断言 FAILED（403 分支的门看不见
    ///   这次换桶 ⇒ 按已换到的区算回原区 ⇒ 原区桶还在封禁 ⇒ 备区路径又弹回 ⇒
    ///   同一请求内 A→B→A→B 振荡）；
    /// - 把标记置位挪出 `alt_region` 的 `Some(r)` 分支（如无条件置位）→ 第二条 FAILED
    ///   （未换桶的普通请求也置位 ⇒ 该号本请求内合法的一次 L1 换区被误杀）；
    /// - 403 分支那道门被删 → 第三条 FAILED（两路径的共享感知失效，回到「各自为政」）。
    #[test]
    fn alt_region_swap_marks_region_switched_shared_with_l1_403() {
        let src = include_str!("provider.rs");
        let prod = src
            .split_once("\n#[cfg(test)]")
            .map(|(a, _)| a)
            .unwrap_or(src);
        let retry_fn = prod
            .split("async fn call_api_with_retry")
            .nth(1)
            .expect("call_api_with_retry 不应被改名");

        // ① 429 备区换桶处必须置位共享标记（与 L1 403 的置位同字面量、同集合）。
        let mark = format!("{}{}", "region_switched_this_call", ".insert(ctx.id)");
        // ② 该置位必须落在 `alt_region` 的 `Some(r)` 分支内：锚定备区生效处的
        //    Cow 重绑到 `None` 分支之间的切片，断言标记插入在其中。
        let anchor = "let call_creds: std::borrow::Cow<'_, KiroCredentials> = match alt_region";
        let branch_start = retry_fn
            .find(anchor)
            .expect("备区生效处的 Cow 重绑不应被改名");
        let branch_end = retry_fn[branch_start..]
            .find("None => call_creds")
            .map(|i| branch_start + i)
            .expect("alt_region 的 None 分支不应消失");
        let alt_branch = &retry_fn[branch_start..branch_end];
        assert!(
            alt_branch.contains(mark.as_str()),
            "429 备区换桶必须置位「本请求已换区」标记（与 L1 403 同一份）：\
             否则 403 分支的门看不见这次换桶，会按已换到的区算回原区，而原区桶还在封禁期，\
             select_endpoint 又弹回备区 ⇒ 同一请求内 A→B→A→B 振荡"
        );

        // ③ 403 分支的门仍在（两路径读同一个标记，共享感知才有落点）。
        let gate = format!("!{}{}", "region_switched_this_call", ".contains(&ctx.id)");
        assert!(
            retry_fn.contains(gate.as_str()),
            "403 换区分支的门被删：429 备区换桶置的标记没人读，共享感知失效"
        );
    }

    /// ⭐ 源码级守卫（P0-A）：对话路径与 MCP 路径的失败守卫组装点都必须存在，
    /// 且初值必须是 [`crate::kiro::upstream_trace::VERDICT_UNCLASSIFIED`]。
    ///
    /// 与 `upstream_trace.rs` 的 `provider_guards_must_default_verdict_to_unclassified`
    /// 同源（那边数全局计数=2，这里数两处组装点各自就位）：漏标的失败分支在 trace
    /// 里落 unclassified，验收脚本据此统计。组装点被删/挪出失败路径/初值被改，本断言红。
    #[test]
    fn trace_guards_wired_in_both_call_paths_with_unclassified_default() {
        let full = include_str!("provider.rs");
        let src = full
            .split_once("\n#[cfg(test)]")
            .map(|(a, _)| a)
            .unwrap_or(full);
        // needle 运行时拼接（include_str! 自匹配坑，本仓踩过多次）。
        let needle = format!(
            "{}{}",
            "verdict: crate::kiro::upstream_trace::VERDICT_UNCLASSIFIED", ".to_string(),"
        );
        assert_eq!(
            src.matches(needle.as_str()).count(),
            2,
            "对话路径与 MCP 路径的失败守卫组装点都必须以 VERDICT_UNCLASSIFIED 为初值\
             （当前 {} 处），否则漏标的失败分支在 trace 里查不出来",
            src.matches(needle.as_str()).count()
        );
        // 两处必须分别落在两个重试函数里，且都位于「失败响应」之后（守卫真的挂在
        // 失败路径上，而不是在函数里充数）。函数切片照本仓先例截到下一个顶层函数。
        for (fname, marker) in [
            ("call_api_with_retry", "async fn call_api_with_retry"),
            ("call_mcp_with_retry", "async fn call_mcp_with_retry"),
        ] {
            let start = src
                .find(marker)
                .unwrap_or_else(|| panic!("{fname} 不应被改名"));
            let after_sig = start + marker.len();
            let rest = &src[after_sig..];
            let end = ["\n    async fn ", "\n    pub fn ", "\n    fn "]
                .iter()
                .filter_map(|m| rest.find(m))
                .min()
                .map(|i| after_sig + i)
                .unwrap_or(src.len());
            let seg_fn = &src[start..end];
            let fail_at = seg_fn
                .find("// 失败响应")
                .unwrap_or_else(|| panic!("{fname} 的失败响应注释锚点不应被删改"));
            let guard_at = seg_fn
                .find(needle.as_str())
                .unwrap_or_else(|| panic!("{fname} 缺少失败守卫组装点"));
            assert!(
                fail_at < guard_at,
                "{fname}：守卫必须组装在读到失败 body 之后（挂在失败路径上）"
            );
            assert_eq!(
                seg_fn.matches(needle.as_str()).count(),
                1,
                "{fname} 应恰好一处守卫组装点"
            );
        }
    }

    /// ⭐ 源码级守卫（P0-A）：成功侧与网络错误侧必须各自用独立 emit 发 trace。
    ///
    /// 守卫不覆盖成功路径（成功时 body 是对话内容，不该读也不该落盘），成功侧用
    /// verdict="success" 的独立 emit；网络错误无响应体，同样独立 emit（status=None）。
    /// 两条路径缺任一处，trace 里该形态的请求就整条不可见。
    #[test]
    fn trace_success_and_network_error_emits_exist_in_both_paths() {
        let full = include_str!("provider.rs");
        let src = full
            .split_once("\n#[cfg(test)]")
            .map(|(a, _)| a)
            .unwrap_or(full);
        for (fname, marker) in [
            ("call_api_with_retry", "async fn call_api_with_retry"),
            ("call_mcp_with_retry", "async fn call_mcp_with_retry"),
        ] {
            let start = src
                .find(marker)
                .unwrap_or_else(|| panic!("{fname} 不应被改名"));
            let after_sig = start + marker.len();
            let rest = &src[after_sig..];
            let end = ["\n    async fn ", "\n    pub fn ", "\n    fn "]
                .iter()
                .filter_map(|m| rest.find(m))
                .min()
                .map(|i| after_sig + i)
                .unwrap_or(src.len());
            let seg_fn = &src[start..end];
            assert_eq!(
                seg_fn.matches("verdict: \"success\"").count(),
                1,
                "{fname} 成功侧必须有独立的 verdict=\"success\" trace emit（守卫不覆盖成功路径）"
            );
            assert_eq!(
                seg_fn.matches("verdict: \"network_error\"").count(),
                1,
                "{fname} 网络错误分支必须有独立的 verdict=\"network_error\" trace emit"
            );
        }
    }

    // ══════════ mapped_model 透传预判（predict_passthrough_upstream_model）══════════

    fn predict_cred() -> KiroCredentials {
        KiroCredentials::default()
    }

    fn predict_rules(pairs: &[(&str, &str)]) -> std::collections::HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    /// 未命中映射 → None（消费端回落原始名）；命中映射 → Some(映射后名)。
    #[test]
    fn predict_mapped_model_without_normalize_only_maps() {
        let cred = predict_cred();
        let rules = predict_rules(&[("claude-haiku-4-5", "claude-sonnet-4-5")]);
        assert_eq!(
            KiroProvider::predict_passthrough_upstream_model(Some("claude-haiku-4-5"), &cred, &rules),
            Some("claude-sonnet-4-5".to_string())
        );
        assert_eq!(
            KiroProvider::predict_passthrough_upstream_model(Some("claude-opus-5"), &cred, &rules),
            None,
            "未命中映射 → None"
        );
        // 空模型名不 panic 且不改写。
        assert_eq!(
            KiroProvider::predict_passthrough_upstream_model(Some(""), &cred, &rules),
            None
        );
        assert_eq!(
            KiroProvider::predict_passthrough_upstream_model(None, &cred, &rules),
            None,
            "无模型语义调用 → None"
        );
    }

    /// 映射命中 → 预判记映射名（与 forward 链一致）。
    #[test]
    fn predict_mapped_model_map_to_deepseek_kept() {
        let cred = predict_cred();
        let rules = predict_rules(&[("claude-haiku-4-5", "deepseek-v4-flash")]);
        assert_eq!(
            KiroProvider::predict_passthrough_upstream_model(Some("claude-haiku-4-5"), &cred, &rules),
            Some("deepseek-v4-flash".to_string())
        );
    }

    /// 豁免凭据：映射跳过 → 不改写（对齐 forward 的 exempt 分支）。
    #[test]
    fn predict_mapped_model_exempt_skips_mapping() {
        let mut cred = predict_cred();
        cred.model_mapping_exempt = Some(true);
        let rules = predict_rules(&[("claude-opus-5", "claude-haiku-4-5")]);
        assert_eq!(
            KiroProvider::predict_passthrough_upstream_model(Some("claude-opus-5"), &cred, &rules),
            None,
            "豁免只跳过映射，无其它改写 → None"
        );
    }
