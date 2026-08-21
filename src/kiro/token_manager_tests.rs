    use super::*;

    // ===== MCP 无号直连：acquire_mcp_direct_token（纯逻辑）=====

    /// 造一个 custom_api 代挂号（只有 base_url + api_key，无 Kiro token）。
    fn mk_direct_custom(id: u64) -> KiroCredentials {
        let mut c = KiroCredentials::default();
        c.id = Some(id);
        c.auth_method = Some("custom_api".to_string());
        c.base_url = Some(format!("https://relay{id}.example.invalid"));
        c.api_key = Some(format!("sk-relay-{id}"));
        c
    }

    /// 造一个 api_key（ksk_）号。
    fn mk_direct_kiro(id: u64) -> KiroCredentials {
        let mut c = KiroCredentials::default();
        c.id = Some(id);
        c.auth_method = Some("api_key".to_string());
        c.kiro_api_key = Some(format!("sk-kiro-{id}"));
        c
    }

    /// 造一个 OAuth 号（access_token）。
    fn mk_direct_oauth(id: u64) -> KiroCredentials {
        let mut c = KiroCredentials::default();
        c.id = Some(id);
        c.auth_method = Some("social".to_string());
        c.access_token = Some(format!("oauth-tok-{id}"));
        c
    }

    /// 纯 custom_api 池（线上现状）：没有任何 Kiro token → 直连无可用凭据。
    ///
    /// ⭐ 承重：这是「无号直连」的边界 —— 纯透传池下直连必须返回 None（由调用方
    /// 降级现状错误），**绝不能**把 custom_api 的 api_key（中转站密钥）当 Kiro token
    /// 拿去打 runtime.*.kiro.dev/mcp。
    #[test]
    fn mcp_direct_no_token_in_pure_custom_api_pool() {
        let mgr = MultiTokenManager::new(
            Config::default(),
            vec![
                mk_direct_custom(1),
                mk_direct_custom(2),
                mk_direct_custom(3),
            ],
            None,
            None,
            false,
        )
        .unwrap();
        assert!(
            mgr.acquire_mcp_direct_token().is_none(),
            "纯 custom_api 池必须无可用 Kiro token（api_key 是中转站密钥，不算）"
        );
    }

    /// 混池：直连 URL 是 IDE MCP（runtime.*.kiro.dev），必须选 OAuth，不得抢 ksk_。
    /// 回退即 FAIL：把 ksk_ 改回命中即返回，断言变红。
    #[test]
    fn mcp_direct_prefers_oauth_over_ksk_for_ide_mcp() {
        let mgr = MultiTokenManager::new(
            Config::default(),
            vec![mk_direct_oauth(1), mk_direct_kiro(2)],
            None,
            None,
            false,
        )
        .unwrap();
        let (id, cred, token) = mgr.acquire_mcp_direct_token().expect("应有 OAuth 号");
        assert_eq!(id, 1, "OAuth 号必须优先于 ksk_ 号（IDE MCP 主机）");
        assert_eq!(token, "oauth-tok-1");
        assert_eq!(cred.access_token.as_deref(), Some("oauth-tok-1"));
    }

    /// 池里只有 ksk_ 时仍可用（OAuth 优先不等于丢掉 ksk_ 回退）。
    #[test]
    fn mcp_direct_falls_back_to_ksk_when_no_oauth() {
        let mgr = MultiTokenManager::new(
            Config::default(),
            vec![mk_direct_kiro(2)],
            None,
            None,
            false,
        )
        .unwrap();
        let (id, cred, token) = mgr.acquire_mcp_direct_token().expect("应有 ksk_ 号");
        assert_eq!(id, 2);
        assert_eq!(token, "sk-kiro-2");
        assert_eq!(cred.kiro_api_key.as_deref(), Some("sk-kiro-2"));
    }

    /// 只有 OAuth 号时直连可用其 access_token（实现**不检查**冷却状态——冷却不是
    /// 惩罚，token 有效即可直连；disabled 会跳过，见 mcp_direct_skips_disabled_credentials）。
    #[test]
    fn mcp_direct_falls_back_to_oauth_token() {
        let oauth = mk_direct_oauth(7);
        let mgr = MultiTokenManager::new(Config::default(), vec![oauth], None, None, false).unwrap();
        let (id, _, token) = mgr.acquire_mcp_direct_token().expect("OAuth token 应可用");
        assert_eq!(id, 7);
        assert_eq!(token, "oauth-tok-7");
    }

    /// 空 token（空串）不算可用；custom_api 号带 access_token 时（推号方填了 Kiro
    /// 字段）仍可直连——判据只看 token 字段本身，不看 auth_method。
    #[test]
    fn mcp_direct_empty_token_not_usable_but_custom_with_token_is() {
        let mut empty = mk_direct_kiro(1);
        empty.kiro_api_key = Some("   ".to_string());
        let mut custom_with_tok = mk_direct_custom(2);
        custom_with_tok.access_token = Some("real-oauth".to_string());
        let mgr = MultiTokenManager::new(
            Config::default(),
            vec![empty, custom_with_tok],
            None,
            None,
            false,
        )
        .unwrap();
        let (id, _, token) = mgr.acquire_mcp_direct_token().expect("custom 号带 token 应可用");
        assert_eq!(id, 2);
        assert_eq!(token, "real-oauth");
    }

    /// 空池：无凭据 → None（不 panic）。
    #[test]
    fn mcp_direct_empty_pool_returns_none() {
        let mgr = MultiTokenManager::new(Config::default(), vec![], None, None, false).unwrap();
        assert!(mgr.acquire_mcp_direct_token().is_none());
    }

    /// M3：disabled 号不参与直连——禁用是网关自己的惩罚决策（风控/额度/连败），
    /// 直连绕过它自相矛盾（用被惩罚的 token 满速打上游 = 风控窗口加流量）。
    #[test]
    fn mcp_direct_skips_disabled_credentials() {
        let mut dis = mk_direct_kiro(1);
        dis.disabled = true;
        let mgr = MultiTokenManager::new(
            Config::default(),
            vec![dis, mk_direct_kiro(2)],
            None,
            None,
            false,
        )
        .unwrap();
        let (id, _, _) = mgr.acquire_mcp_direct_token().expect("未禁用号应可用");
        assert_eq!(id, 2, "禁用号不得被选中，必须顺延到未禁用号");
    }

    /// M3：全池禁用时直连必须 None（降级回池子错误），而不是拿被惩罚的 token 直连。
    #[test]
    fn mcp_direct_all_disabled_returns_none() {
        let mut a = mk_direct_kiro(1);
        a.disabled = true;
        let mut b = mk_direct_oauth(2);
        b.disabled = true;
        let mgr = MultiTokenManager::new(Config::default(), vec![a, b], None, None, false).unwrap();
        assert!(
            mgr.acquire_mcp_direct_token().is_none(),
            "全池禁用时直连必须无可用 token（不得绕过惩罚决策）"
        );
    }

    /// M3 轮转：多 OAuth 候选时优先「曾成功过」的号（success_count > 0 =
    /// has_ever_succeeded，token 更可能仍有效），避免确定性首匹配把并发全压到
    /// 第一个号（同 token 并发轰炸）。
    #[test]
    fn mcp_direct_prefers_ever_succeeded_oauth() {
        let mgr = MultiTokenManager::new(
            Config::default(),
            vec![mk_direct_oauth(1), mk_direct_oauth(2)],
            None,
            None,
            false,
        )
        .unwrap();
        {
            let mut entries = mgr.entries.lock();
            entries.iter_mut().find(|e| e.id == 2).unwrap().success_count = 5;
        }
        let (id, _, _) = mgr.acquire_mcp_direct_token().expect("应有 OAuth 候选");
        assert_eq!(id, 2, "曾成功过的号必须优先于从未成功过的号");
    }

    #[test]
    fn mcp_direct_excluding_skips_given_ids() {
        let mgr = MultiTokenManager::new(
            Config::default(),
            vec![mk_direct_oauth(1), mk_direct_oauth(2)],
            None,
            None,
            false,
        )
        .unwrap();
        {
            let mut entries = mgr.entries.lock();
            entries.iter_mut().find(|e| e.id == 1).unwrap().success_count = 5;
            entries.iter_mut().find(|e| e.id == 2).unwrap().success_count = 5;
        }
        let first = mgr.acquire_mcp_direct_token().expect("应有号").0;
        let mut skip = std::collections::HashSet::new();
        skip.insert(first);
        let second = mgr
            .acquire_mcp_direct_token_excluding(&skip)
            .expect("排除后应落到另一号")
            .0;
        assert_ne!(first, second, "同请求 401 后必须换号，不得钉死同一 id");
        skip.insert(second);
        assert!(
            mgr.acquire_mcp_direct_token_excluding(&skip).is_none(),
            "两个都排除后应无候选"
        );
    }

    // ===== B8：号池全灭告警去抖门（纯逻辑）=====

    #[test]
    fn test_pool_exhaustion_gate_window_and_threshold() {
        let t0 = Instant::now();
        let mut gate = PoolExhaustionGate::default();

        // 第 1、2 次：未达阈值不告警。
        assert!(!gate.record(t0), "第 1 次无候选不应告警");
        assert!(
            !gate.record(t0 + StdDuration::from_secs(1)),
            "第 2 次不应告警"
        );
        // 第 3 次（窗口内）：达阈值告警。
        assert!(
            gate.record(t0 + StdDuration::from_secs(2)),
            "窗口内第 3 次必须告警"
        );
        // reset 后重新计数。
        gate.reset();
        assert!(
            !gate.record(t0 + StdDuration::from_secs(3)),
            "reset 后重新计数"
        );
    }

    #[test]
    fn test_pool_exhaustion_gate_window_expiry_resets_count() {
        let t0 = Instant::now();
        let mut gate = PoolExhaustionGate::default();
        gate.record(t0);
        gate.record(t0 + StdDuration::from_secs(1));
        // 第 3 次发生在窗口过期（30s）后：计数重置，本发不告警。
        assert!(
            !gate.record(t0 + StdDuration::from_secs(31)),
            "窗口过期后第 1 次不应告警（计数已重置）"
        );
        // 新窗口内再两次即达阈值。
        assert!(!gate.record(t0 + StdDuration::from_secs(32)));
        assert!(
            gate.record(t0 + StdDuration::from_secs(33)),
            "新窗口内连续 3 次应告警"
        );
    }

    // ===== M2：select_highest_priority 行为测试（2026-08-15 补）=====

    /// 多号不同优先级 → 选中优先级最小（最高）的未禁用号。
    #[test]
    fn test_select_highest_priority_picks_min_priority_enabled() {
        let mut low = KiroCredentials::default(); // id=1, priority=30
        low.priority = 30;
        let mut best = KiroCredentials::default(); // id=2, priority=10
        best.priority = 10;
        let mut mid = KiroCredentials::default(); // id=3, priority=20
        mid.priority = 20;
        let manager = MultiTokenManager::new(
            Config::default(),
            vec![low, best, mid],
            None,
            None,
            true,
        )
        .expect("构造 manager");

        manager.select_highest_priority();
        assert_eq!(
            *manager.current_id.lock(),
            2,
            "必须选中优先级最小的未禁用号"
        );
    }

    /// 最高优先级号被禁用 → 落到次高；再禁 → 依次下降；全禁用 → 保持不动。
    #[test]
    fn test_select_highest_priority_skips_disabled_and_keeps_last_when_all_disabled() {
        let mut c1 = KiroCredentials::default(); // id=1, priority=10
        c1.priority = 10;
        let mut c2 = KiroCredentials::default(); // id=2, priority=20
        c2.priority = 20;
        let manager =
            MultiTokenManager::new(Config::default(), vec![c1, c2], None, None, true)
                .expect("构造 manager");
        assert_eq!(*manager.current_id.lock(), 1, "初始即选中最高优先级");

        // 禁用 #1 → 切到 #2。
        manager.entries.lock()[0].disabled = true;
        manager.select_highest_priority();
        assert_eq!(
            *manager.current_id.lock(),
            2,
            "最高优先级被禁用后应切到次高"
        );

        // 全禁用 → current_id 保持不变（无可用号不瞎切）。
        manager.entries.lock()[1].disabled = true;
        manager.select_highest_priority();
        assert_eq!(
            *manager.current_id.lock(),
            2,
            "全禁用时不得把 current_id 切成 0"
        );
    }

    /// 同优先级 tie-break：min_by_key 稳定保留 entries 中先出现的那个。
    #[test]
    fn test_select_highest_priority_same_priority_keeps_first() {
        let mut a = KiroCredentials::default(); // id=1, priority=5
        a.priority = 5;
        let mut b = KiroCredentials::default(); // id=2, priority=5
        b.priority = 5;
        let manager =
            MultiTokenManager::new(Config::default(), vec![a, b], None, None, true)
                .expect("构造 manager");
        manager.select_highest_priority();
        assert_eq!(
            *manager.current_id.lock(),
            1,
            "同优先级必须保留 entries 中先出现的号（稳定 tie-break）"
        );
    }

    /// 空池：select_highest_priority 必须无副作用（current_id 保持 0）。
    #[test]
    fn test_select_highest_priority_empty_pool_noop() {
        let manager = MultiTokenManager::new(Config::default(), vec![], None, None, true)
            .expect("构造 manager");
        assert_eq!(*manager.current_id.lock(), 0);
        manager.select_highest_priority();
        assert_eq!(*manager.current_id.lock(), 0, "空池调用必须无副作用");
    }

    // ===== External IdP 验活层：纯逻辑单测 =====

    #[test]
    fn test_classify_probe_200_usable() {
        assert_eq!(
            classify_profile_probe(200, r#"{"subscriptionInfo":{}}"#),
            ProfileProbeOutcome::Usable {
                subscription_title: None
            }
        );
        assert_eq!(
            classify_profile_probe(204, ""),
            ProfileProbeOutcome::Usable {
                subscription_title: None
            }
        );
    }

    #[test]
    fn test_classify_probe_403_feature_not_supported() {
        // 实测 us-east-1 未开通号的真实症状
        let body = r#"{"__type":"AccessDeniedException","message":"FEATURE_NOT_SUPPORTED"}"#;
        assert_eq!(
            classify_profile_probe(403, body),
            ProfileProbeOutcome::FeatureNotSupported
        );
    }

    #[test]
    fn test_classify_probe_403_other_is_not_feature() {
        // 403 但不含 FEATURE_NOT_SUPPORTED → OtherError（不判死 region）
        match classify_profile_probe(403, "some other 403 reason") {
            ProfileProbeOutcome::OtherError(_) => {}
            other => panic!("期望 OtherError，得到 {:?}", other),
        }
    }

    #[test]
    fn test_classify_probe_401_unauthorized() {
        assert_eq!(
            classify_profile_probe(401, "invalid token"),
            ProfileProbeOutcome::Unauthorized
        );
    }

    #[test]
    fn test_classify_probe_429_is_other_error_not_dead() {
        // 铁律：429 归 OtherError（暂时不可用），绝不因限流判死一个 region
        match classify_profile_probe(429, "Too Many Requests") {
            ProfileProbeOutcome::OtherError(_) => {}
            other => panic!("429 必须是 OtherError，得到 {:?}", other),
        }
    }

    #[test]
    fn test_classify_probe_5xx_is_other_error() {
        match classify_profile_probe(502, "bad gateway") {
            ProfileProbeOutcome::OtherError(_) => {}
            other => panic!("期望 OtherError，得到 {:?}", other),
        }
    }

    fn mk_candidate(usable: bool, title: Option<&str>) -> ProfileCandidate {
        ProfileCandidate {
            arn: "arn:aws:codewhisperer:eu-central-1:1:profile/x".to_string(),
            region: "eu-central-1".to_string(),
            account: "1".to_string(),
            usable,
            subscription_title: title.map(|s| s.to_string()),
            reason: if usable {
                "usable"
            } else {
                "feature_not_supported"
            },
            current: false,
        }
    }

    #[test]
    fn test_candidate_rank_usable_before_unusable() {
        let usable = mk_candidate(true, None);
        let unusable = mk_candidate(false, None);
        assert!(candidate_rank(&usable) < candidate_rank(&unusable));
    }

    #[test]
    fn test_candidate_rank_paid_before_free() {
        let paid = mk_candidate(true, Some("KIRO POWER"));
        let free = mk_candidate(true, Some("KIRO FREE"));
        let none = mk_candidate(true, None);
        assert!(candidate_rank(&paid) < candidate_rank(&free));
        assert!(candidate_rank(&paid) < candidate_rank(&none));
    }

    #[test]
    fn test_candidate_sort_orders_usable_paid_first() {
        let mut v = vec![
            mk_candidate(false, None),
            mk_candidate(true, Some("KIRO FREE")),
            mk_candidate(true, Some("KIRO POWER")),
        ];
        v.sort_by_key(candidate_rank);
        assert_eq!(v[0].subscription_title.as_deref(), Some("KIRO POWER"));
        assert_eq!(v[1].subscription_title.as_deref(), Some("KIRO FREE"));
        assert!(!v[2].usable);
    }

    #[test]
    fn test_account_from_arn() {
        assert_eq!(
            account_from_arn("arn:aws:codewhisperer:eu-central-1:155119901513:profile/abc"),
            "155119901513"
        );
        assert_eq!(account_from_arn("garbage"), "");
    }

    #[test]
    fn test_write_atomic_writes_content_and_no_tmp_residue() {
        let dir = std::env::temp_dir().join(format!("kiro-atomic-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("credentials.json");

        write_atomic(&path, b"hello-atomic").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"hello-atomic");

        // 目录下不应残留 .credentials.json.*.tmp
        let has_tmp = std::fs::read_dir(&dir).unwrap().any(|e| {
            e.ok()
                .and_then(|e| e.file_name().into_string().ok())
                .map(|n| n.ends_with(".tmp"))
                .unwrap_or(false)
        });
        assert!(!has_tmp, "临时文件不应残留");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn test_write_atomic_overwrites_existing_file() {
        let dir = std::env::temp_dir().join(format!("kiro-atomic-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("data.json");

        std::fs::write(&path, b"old-content-longer").unwrap();
        write_atomic(&path, b"new").unwrap();
        // 覆盖后内容必须是新内容，不能残留旧内容尾巴
        assert_eq!(std::fs::read(&path).unwrap(), b"new");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn test_is_token_expired_with_expired_token() {
        let mut credentials = KiroCredentials::default();
        credentials.expires_at = Some("2020-01-01T00:00:00Z".to_string());
        assert!(is_token_expired(&credentials));
    }

    #[test]
    fn test_is_token_expired_with_valid_token() {
        let mut credentials = KiroCredentials::default();
        let future = Utc::now() + Duration::hours(1);
        credentials.expires_at = Some(future.to_rfc3339());
        assert!(!is_token_expired(&credentials));
    }

    #[test]
    fn test_is_token_expired_within_5_minutes() {
        let mut credentials = KiroCredentials::default();
        let expires = Utc::now() + Duration::minutes(3);
        credentials.expires_at = Some(expires.to_rfc3339());
        assert!(is_token_expired(&credentials));
    }

    #[test]
    fn test_is_token_expired_no_expires_at() {
        let credentials = KiroCredentials::default();
        assert!(is_token_expired(&credentials));
    }

    #[test]
    fn test_is_token_expiring_soon_within_10_minutes() {
        let mut credentials = KiroCredentials::default();
        let expires = Utc::now() + Duration::minutes(8);
        credentials.expires_at = Some(expires.to_rfc3339());
        assert!(is_token_expiring_soon(&credentials));
    }

    #[test]
    fn test_is_token_expiring_soon_beyond_10_minutes() {
        let mut credentials = KiroCredentials::default();
        let expires = Utc::now() + Duration::minutes(15);
        credentials.expires_at = Some(expires.to_rfc3339());
        assert!(!is_token_expiring_soon(&credentials));
    }

    #[test]
    fn test_validate_refresh_token_missing() {
        let credentials = KiroCredentials::default();
        let result = validate_refresh_token(&credentials);
        assert!(result.is_err());
    }

    #[test]
    fn test_validate_refresh_token_valid() {
        let mut credentials = KiroCredentials::default();
        credentials.refresh_token = Some("a".repeat(150));
        let result = validate_refresh_token(&credentials);
        assert!(result.is_ok());
    }

    #[test]
    fn test_sha256_hex() {
        let result = sha256_hex("test");
        assert_eq!(
            result,
            "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08"
        );
    }

    /// SSRF 回归：External IdP token_endpoint 只放行 Microsoft 登录域，其余一律拒绝。
    #[test]
    fn test_validate_microsoft_token_endpoint() {
        // 合法：官方域及其租户子路径
        for ok in [
            "https://login.microsoftonline.com/9d76.../oauth2/v2.0/token",
            "https://login.microsoftonline.us/tid/oauth2/v2.0/token",
            "https://login.partner.microsoftonline.cn/tid/oauth2/v2.0/token",
        ] {
            assert!(
                validate_microsoft_token_endpoint(ok).is_ok(),
                "应放行: {ok}"
            );
        }
        // 非法：攻击者域 / 内网 / http / userinfo 混淆 / 相似域后缀伪装
        for bad in [
            "https://evil.com/token",
            "https://10.0.0.1/token",                 // 内网 IP（SSRF 应拒）
            "http://login.microsoftonline.com/token", // 非 https
            "https://login.microsoftonline.com@evil.com/token", // userinfo 混淆
            "https://login.microsoftonline.com.evil.com/token", // 后缀伪装
            "https://notmicrosoftonline.com/token",
        ] {
            assert!(
                validate_microsoft_token_endpoint(bad).is_err(),
                "应拒绝: {bad}"
            );
        }
    }

    #[test]
    fn test_credentials_due_for_refresh_selects_expiring_only() {
        // 长度 >=100 的假 refresh_token，绕过 validate_refresh_token 截断判据
        let rt = "r".repeat(120);

        // #1 即将过期（8 分钟）→ 应入选
        let mut expiring = KiroCredentials::default();
        expiring.refresh_token = Some(rt.clone());
        expiring.expires_at = Some((Utc::now() + Duration::minutes(8)).to_rfc3339());

        // #2 仍充裕（1 小时）→ 不入选
        let mut fresh = KiroCredentials::default();
        fresh.refresh_token = Some(rt.clone());
        fresh.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());

        // #3 API Key 凭据 → 永不入选
        let mut api_key = KiroCredentials::default();
        api_key.kiro_api_key = Some("ksk_test_key_123".to_string());
        api_key.auth_method = Some("api_key".to_string());

        let manager = MultiTokenManager::new(
            Config::default(),
            vec![expiring, fresh, api_key],
            None,
            None,
            true,
        )
        .expect("构造 manager");

        let due = manager.credentials_due_for_refresh(10);
        // 仅 #1（id 从 1 起分配）
        assert_eq!(due, vec![1], "只应选中将在 10 分钟内过期的可刷新凭据");
    }

    #[tokio::test]
    async fn test_prefetch_skips_when_token_not_expiring() {
        // token 还有 1 小时才过期 → 预刷新的条件检查应在任何网络调用前跳过
        let rt = "r".repeat(120);
        let mut fresh = KiroCredentials::default();
        fresh.refresh_token = Some(rt);
        fresh.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());

        let manager = MultiTokenManager::new(Config::default(), vec![fresh], None, None, true)
            .expect("构造 manager");

        // conditional_lead=Some(10)：token 不在 10 分钟内过期 → Skipped，不触发刷新
        let outcome = manager
            .refresh_token_locked(1, Some(10))
            .await
            .expect("跳过路径不应返回错误");
        assert_eq!(
            outcome,
            RefreshOutcome::Skipped,
            "token 未临近过期时预刷新应跳过而非发起刷新"
        );
    }

    #[tokio::test]
    async fn test_refresh_token_rejects_api_key_credential() {
        let config = Config::default();
        let mut credentials = KiroCredentials::default();
        credentials.kiro_api_key = Some("ksk_test_key_123".to_string());
        credentials.auth_method = Some("api_key".to_string());

        let result = refresh_token(&credentials, &config, None).await;

        assert!(result.is_err(), "API Key 凭据应被 refresh_token 拒绝");
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("API Key 凭据不支持刷新"),
            "期望错误消息包含 'API Key 凭据不支持刷新'，实际: {}",
            err_msg
        );
    }

    /// ⭐ 2026-08-15（M5 修复）：刷新可重试性判定必须**结构化**，不再裸子串匹配
    /// 状态码数字。
    ///
    /// 旧实现 `contains("500")` 的两类误判：
    /// 1. 把策略性失败当 5xx 重试 —— 错误串里的 URL 端口（`:5000`）/错误体数字
    ///    （`"error_code": 5000`）/毫秒时间戳都含 "500" 子串，429/403 会被退避重试；
    /// 2. 黑名单式 is_network 的反向误判（不含码的真 5xx 被当网络错误，反之亦然）。
    ///
    /// 现在：带 `RefreshHttpError` 的按 `status` 字段分（仅 5xx 可重试），
    /// 本地校验/配置类（`RefreshValidationError`）不重试，
    /// 其余无状态码的错误（网络/JSON）才一律可重试。
    #[test]
    fn refresh_error_retryable_is_structured_not_substring() {
        // 5xx → 可重试（瞬态）
        assert!(refresh_error_retryable(&RefreshHttpError {
            status: 503,
            message: "服务器错误，AWS OAuth 服务暂时不可用: 503 Service Unavailable".into(),
        }
        .into()));
        assert!(refresh_error_retryable(&RefreshHttpError {
            status: 500,
            message: "500".into(),
        }
        .into()));
        // 策略性失败 → 不重试
        assert!(!refresh_error_retryable(&RefreshHttpError {
            status: 429,
            message: "请求过于频繁，已被限流: 429 Too Many Requests".into(),
        }
        .into()));
        assert!(!refresh_error_retryable(&RefreshHttpError {
            status: 403,
            message: "权限不足，无法刷新 Token: 403 Forbidden".into(),
        }
        .into()));
        assert!(!refresh_error_retryable(&RefreshHttpError {
            status: 400,
            message: "Token 刷新失败: 400 Bad Request".into(),
        }
        .into()));
        // ⭐ 裸子串时代的两类误判，结构化后必须消失：
        // 1) message 里含 "5000"/":5000"（URL 端口、错误体数字）但 status=429 → 仍不重试
        //    （旧 `contains("500")` 会命中 → 把 429 当 5xx 退避重试 3 次）。
        assert!(!refresh_error_retryable(&RefreshHttpError {
            status: 429,
            message: "429 Too Many Requests: upstream http://127.0.0.1:5000 err 5000".into(),
        }
        .into()));
        // 2) 无状态码的网络错误（reqwest 连接/超时）→ 可重试
        assert!(refresh_error_retryable(&anyhow::anyhow!(
            "error sending request for url (http://127.0.0.1:5000/): connection reset"
        )));
        // 3) 本地校验/配置类错误（refreshToken 缺失/截断、构建客户端失败）→ 不重试：
        //    与凭据内容/本机配置绑定，重试必败 —— 白等 1s+2s 且每轮多计一次失败。
        assert!(!refresh_error_retryable(&RefreshValidationError::new("缺少 refreshToken").into()));
        assert!(!refresh_error_retryable(
            &RefreshValidationError::new("refreshToken 已被截断（长度: 100 字符）。").into()
        ));
        assert!(!refresh_error_retryable(
            &RefreshValidationError::new("构建刷新客户端失败: TLS 后端不可用").into()
        ));
    }

    /// ⭐ 回归：api_key 号的"不支持刷新"必须**快速失败**，不得被当瞬态错误退避重试。
    ///
    /// 缺陷（本轮多开时线上暴露）：刷新层的瞬态判据是**黑名单式**——
    /// 错误串不含 400/401/403/404/410/422/429/invalid_grant 就当"网络瞬态错误"。
    /// 而 "API Key 凭据不支持刷新 Token" 一个都不含 → 被判瞬态 →
    /// 退避重试满 3 次（白等 1s + 2s），且每轮计一次失败。
    ///
    /// 线上后果：api_key 号遇 403 后每轮白等约 3 秒、连计 3 次失败即判死号自动禁用，
    /// 相当于**把它的死亡速度放大三倍**（实测 #421-424 就是这样被反复禁用的）。
    ///
    /// 用耗时做判据而非日志：重试是可观测的时间代价（1s + 2s ≥ 3s），
    /// 而快速失败 < 1s。回退分类器里那条排除条件即 FAIL。
    #[tokio::test]
    async fn api_key_refresh_rejection_must_fail_fast_not_retry() {
        let mut cred = KiroCredentials::default();
        cred.id = Some(1);
        cred.kiro_api_key = Some("ksk_fail_fast_probe".to_string());
        cred.auth_method = Some("api_key".to_string());
        let mgr = MultiTokenManager::new(Config::default(), vec![cred], None, None, false).unwrap();

        let started = std::time::Instant::now();
        let err = mgr
            .refresh_token_locked(1, None)
            .await
            .expect_err("api_key 号刷新必须失败");
        let elapsed = started.elapsed();

        assert!(
            err.to_string().contains("API Key 凭据不支持刷新"),
            "错误应是契约级拒绝，实际: {err}"
        );
        assert!(
            elapsed < std::time::Duration::from_millis(900),
            "必须快速失败：结构上不可能成功的刷新被退避重试了（耗时 {elapsed:?}，\
             重试 3 次会花 1s+2s）。这会让 api_key 号每轮白等 3 秒并加速判死"
        );
    }

    #[tokio::test]
    async fn test_ensure_valid_token_returns_api_key_without_refresh() {
        // API Key 凭据：ensure_valid_token 直接返回 kiroApiKey，绝不触发刷新（无网络）。
        let mut api_key_cred = KiroCredentials::default();
        api_key_cred.kiro_api_key = Some("ksk_ensure_valid_123".to_string());
        api_key_cred.auth_method = Some("api_key".to_string());

        let manager =
            MultiTokenManager::new(Config::default(), vec![api_key_cred], None, None, true)
                .expect("构造 manager");

        let (creds, token) = manager
            .ensure_valid_token(1)
            .await
            .expect("API Key 凭据应直接返回，不报错");
        assert_eq!(
            token, "ksk_ensure_valid_123",
            "应返回 kiroApiKey 作为 token"
        );
        assert!(creds.is_api_key_credential(), "返回的应是同一 API Key 凭据");
    }

    #[tokio::test]
    async fn test_ensure_valid_token_hot_path_no_refresh_for_fresh_token() {
        // token 还有 1 小时才过期：ensure_valid_token 走热路径直接返回现有 access_token，
        // 不碰 refresh_lock、不发起任何网络刷新（refresh_token 是废串，若真去刷会失败）。
        let mut fresh = KiroCredentials::default();
        fresh.refresh_token = Some("r".repeat(120));
        fresh.access_token = Some("hot_path_token".to_string());
        fresh.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());

        let manager = MultiTokenManager::new(Config::default(), vec![fresh], None, None, true)
            .expect("构造 manager");

        let (creds, token) = manager
            .ensure_valid_token(1)
            .await
            .expect("未过期 token 热路径不应报错");
        assert_eq!(
            token, "hot_path_token",
            "未过期时应直接返回现有 access_token"
        );
        assert_eq!(
            creds.access_token.as_deref(),
            Some("hot_path_token"),
            "返回凭据应携带原 access_token"
        );
    }

    #[tokio::test]
    async fn test_ensure_valid_token_expired_delegates_to_refresh() {
        // token 已过期：ensure_valid_token 委托 refresh_token_locked 走真实刷新实现。
        // 这里用 refresh_token 长度不足 100 的凭据，让底层 validate_refresh_token 在
        // 任何网络调用前就 bail——从而在无网络的单测里确认「过期 → 确实进入刷新委托路径」
        // （热路径/API Key 分流都不会命中该错误）。
        let mut expired = KiroCredentials::default();
        expired.refresh_token = Some("short".to_string()); // < 100 → validate 阶段即失败
        expired.access_token = Some("stale_token".to_string());
        expired.expires_at = Some("2020-01-01T00:00:00Z".to_string()); // 已过期

        let manager = MultiTokenManager::new(Config::default(), vec![expired], None, None, true)
            .expect("构造 manager");

        let err = manager
            .ensure_valid_token(1)
            .await
            .expect_err("过期 token 应委托刷新，且因 refresh_token 被截断而失败");
        assert!(
            err.to_string().contains("refreshToken 已被截断"),
            "应命中刷新委托路径的 validate 报错（证明进入了刷新而非热路径），实际: {}",
            err
        );
    }

    /// 🔴 磁盘级数据丢失回归：`apply_refresh_result_fields` 必须逐字段合并,
    /// 绝不能整体替换 `entry.credentials = new_creds`。
    ///
    /// 场景复现:刷新是跨 `.await` 的网络往返(最坏约 183s,含重试退避),期间余额刷新环
    /// （每 30 分钟一次,写 `subscription_title`）可能已经把活的 `entry.credentials`
    /// 改成了比"刷新发起前快照"更新的值。若刷新写回时整体替换,这次并发写入会被
    /// 陈旧快照回退,且随后 `persist_credentials()` 会把回退结果**写进磁盘**。
    ///
    /// 断言:合并后 token 字段（access_token/refresh_token/expires_at）取刷新结果,
    /// 而并发修改的非 token 字段（subscription_title）保留新值、不被回退。
    #[test]
    fn test_apply_refresh_result_fields_preserves_concurrently_modified_field() {
        // 刷新发起前的快照（对应 refresh_token_locked 顶部 `credentials.clone()`）。
        let mut snapshot = KiroCredentials::default();
        snapshot.access_token = Some("old_access".to_string());
        snapshot.refresh_token = Some("old_refresh".to_string());
        snapshot.expires_at = Some("2020-01-01T00:00:00Z".to_string());
        snapshot.subscription_title = Some("KIRO FREE".to_string());

        // 刷新产物:三个 refresh_*_token 函数的行为都是 `credentials.clone()` 后
        // 只按响应体条件改 access_token/refresh_token/expires_at/profile_arn 这 4 个
        // 字段,其余字段（含 subscription_title）原样保留快照里的旧值。
        let mut new_creds = snapshot.clone();
        new_creds.access_token = Some("new_access".to_string());
        new_creds.refresh_token = Some("new_refresh".to_string());
        new_creds.expires_at = Some("2030-01-01T00:00:00Z".to_string());
        // new_creds.subscription_title 仍是快照里的 "KIRO FREE"（未被刷新链路改过）。

        // 活的 entry:与快照出发点相同,但在本次刷新的网络往返期间,余额刷新环
        // 已经把 subscription_title 更新成了新值（并发写入,快照看不到）。
        let mut entry_credentials = snapshot.clone();
        entry_credentials.subscription_title = Some("KIRO PRO+".to_string());

        // 夹具自检:确保确实构造出了"分歧",否则无论合并对不对测试都会通过
        // （对应 CLAUDE.md 记录的「夹具不含被判据匹配的子串」纸面测试形态）。
        assert_ne!(
            entry_credentials.subscription_title, new_creds.subscription_title,
            "夹具自检失败:entry 与 new_creds 的 subscription_title 必须不同,否则测不出回退"
        );
        assert_ne!(
            entry_credentials.access_token, new_creds.access_token,
            "夹具自检失败:entry 与 new_creds 的 access_token 必须不同,否则测不出未更新"
        );

        apply_refresh_result_fields(&mut entry_credentials, &new_creds);

        // token 字段必须取刷新结果。
        assert_eq!(
            entry_credentials.access_token.as_deref(),
            Some("new_access"),
            "access_token 应取刷新结果"
        );
        assert_eq!(
            entry_credentials.refresh_token.as_deref(),
            Some("new_refresh"),
            "refresh_token 应取刷新结果"
        );
        assert_eq!(
            entry_credentials.expires_at.as_deref(),
            Some("2030-01-01T00:00:00Z"),
            "expires_at 应取刷新结果"
        );

        // 非 token 字段必须保留并发写入的新值,不能被陈旧快照回退。
        assert_eq!(
            entry_credentials.subscription_title.as_deref(),
            Some("KIRO PRO+"),
            "回退即 FAIL:subscription_title 被刷新产物的陈旧快照覆盖了（磁盘级数据丢失复发）"
        );
    }

    /// 源码级守卫：`refresh_token_locked` 的写回临界区必须调用 `apply_refresh_result_fields`
    /// 逐字段合并,绝不能整体替换 `entry.credentials = new_creds`。
    ///
    /// 上一条测试（`test_apply_refresh_result_fields_preserves_concurrently_modified_field`）
    /// 只验证了 `apply_refresh_result_fields` 这个纯函数本身的合并逻辑对不对,但**测不出
    /// 写回临界区是否真的调用了它**——`refresh_token_locked` 完全可以在旁边留一个没人调用的
    /// 合并函数,自己仍然整体替换,上面那条测试照样全绿(纸面测试:测了分支内部,没测分支
    /// 是否真被接线)。本测试直接读源码文本,钉死写回临界区调用点是合并函数而非整体赋值。
    #[test]
    fn refresh_token_locked_writeback_uses_field_merge_not_whole_struct_replace() {
        let src = include_str!("token_refresh_http.rs");
        let locked_fn = src
            .split("async fn refresh_token_locked")
            .nth(1)
            .expect("refresh_token_locked 不应被改名");
        // 截到该函数结束（下一个同级 fn/pub fn 定义前）附近的写回临界区注释,缩小定位范围。
        let writeback_section = locked_fn
            .split("// 更新 entries 中对应凭据")
            .nth(1)
            .expect("写回临界区的定位注释不应被删改")
            .split("drop(_guard)")
            .next()
            .expect("drop(_guard) 标记不应被删改");

        assert!(
            writeback_section.contains("apply_refresh_result_fields(&mut entry.credentials"),
            "写回临界区必须调用 apply_refresh_result_fields 做逐字段合并,\
             实际内容: {writeback_section}"
        );
        assert!(
            !writeback_section.contains("entry.credentials = new_creds"),
            "🔴 回归:写回临界区不得整体替换 entry.credentials(会把并发写入的\
             非 token 字段——如余额环写的 subscription_title——回退并持久化到磁盘)"
        );
    }

    #[tokio::test]
    async fn test_refresh_conditional_skips_when_peer_already_refreshed() {
        // A3/C2 回归:模拟惊群第二个 waiter——出队拿锁时 token 已被前一个 waiter 刷新成新鲜。
        // ensure_valid_token 现在传 Some(10),故拿锁后的条件重检应 Skipped、**不再重打上游刷新**。
        // (旧代码 ensure_valid 传 None → 无条件刷新 → 该测试若把 lead 换 None 会走网络失败。)
        let mut cred = KiroCredentials::default();
        cred.refresh_token = Some("r".repeat(120));
        cred.access_token = Some("already_fresh".to_string());
        // 关键:token 已新鲜(1 小时后过期),等价于"前一个 waiter 刚刷好"。
        cred.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());

        let manager = MultiTokenManager::new(Config::default(), vec![cred], None, None, true)
            .expect("构造 manager");

        // 与 ensure_valid_token 同阈值 Some(10):新鲜 → Skipped,无网络。
        let outcome = manager
            .refresh_token_locked(1, Some(10))
            .await
            .expect("新鲜 token 条件刷新应跳过,不报错");
        assert_eq!(
            outcome,
            RefreshOutcome::Skipped,
            "惊群出队者遇已刷新的新鲜 token 应跳过(消除重复刷新)"
        );
    }

    #[tokio::test]
    async fn test_refresh_conditional_does_not_skip_on_unknown_expiry() {
        // A3/C2 边界:expires_at 不可解析时,条件重检必须**不跳过**(unwrap_or(true)),
        // 与热路径 is_token_expired(unwrap_or=true) 同口径——否则 expiry 未知的凭据会被
        // 误跳过、该刷不刷、返回陈旧 token。旧实现 unwrap_or(false) 会错误 Skipped,此测试会失败。
        // 用短 refresh_token(<100)让底层 validate 在任何网络前 bail,从而无网络地证明"进入了刷新"。
        let mut cred = KiroCredentials::default();
        cred.refresh_token = Some("short".to_string()); // <100 → validate 阶段即 bail
        cred.access_token = Some("stale".to_string());
        cred.expires_at = Some("not-a-date".to_string()); // 不可解析 → expiring_within=None

        let manager = MultiTokenManager::new(Config::default(), vec![cred], None, None, true)
            .expect("构造 manager");

        let err = manager
            .refresh_token_locked(1, Some(10))
            .await
            .expect_err("expiry 未知应进入刷新(而非 Skipped),因 refresh_token 截断而失败");
        assert!(
            err.to_string().contains("refreshToken 已被截断"),
            "应进入真实刷新路径的 validate 报错(证明未被条件重检误跳过),实际: {}",
            err
        );
    }

    #[tokio::test]
    async fn test_add_credential_reject_duplicate_refresh_token() {
        let config = Config::default();

        let mut existing = KiroCredentials::default();
        existing.refresh_token = Some("a".repeat(150));

        let manager = MultiTokenManager::new(config, vec![existing], None, None, false).unwrap();

        let mut duplicate = KiroCredentials::default();
        duplicate.refresh_token = Some("a".repeat(150));

        let result = manager.add_credential(duplicate).await;
        assert!(result.is_err());
        assert!(result.err().unwrap().to_string().contains("凭据已存在"));
    }

    /// ⭐ 全池自愈必须**限频**，且一次成功要让退避回到灵敏状态。
    ///
    /// 缺陷（用户直接反馈 + 线上实测）：自愈此前无任何退避 ——
    /// 只要选不出号且有可自愈的禁用号就立刻复活全池，实测 41 分钟触发 **36 次**
    /// （约每 68 秒）。而 403 `temporarily is suspended` 是上游刚下的惩罚，
    /// 每次复活都立刻再打一轮 → **加深封禁**。用户原话：
    /// 「他们已经 403 封号了，不知道为什么一直被自动开启」。
    ///
    /// 本测试锁两件事：
    /// 1. 连续自愈会累加 streak（驱动指数退避）；
    /// 2. 一次 `report_success` 即清零 —— 没有这条，退避会涨到上限（15 分钟）
    ///    并永远停在那里，即使号池早已恢复。那正是本仓反复出现的"单向棘轮"形态。
    ///
    /// 只断言 streak 的可观测行为，不断言具体等待时长（那依赖真实时钟，
    /// 会让测试变慢且脆弱）。退避公式本身是纯算术，由 streak 唯一决定。
    #[test]
    fn self_heal_streak_accumulates_and_resets_on_success() {
        let mut c = KiroCredentials::default();
        c.id = Some(1);
        c.auth_method = Some("api_key".to_string());
        c.kiro_api_key = Some("ksk_self_heal_probe".to_string());
        let mgr = MultiTokenManager::new(Config::default(), vec![c], None, None, false).unwrap();

        assert_eq!(
            mgr.self_heal_streak.load(Ordering::Relaxed),
            0,
            "初始 streak 应为 0（首次自愈不等待）"
        );

        // 模拟连续自愈（生产里由 acquire_context 的自愈分支递增）
        mgr.self_heal_streak.fetch_add(1, Ordering::Relaxed);
        mgr.self_heal_streak.fetch_add(1, Ordering::Relaxed);
        mgr.self_heal_streak.fetch_add(1, Ordering::Relaxed);
        assert_eq!(
            mgr.self_heal_streak.load(Ordering::Relaxed),
            3,
            "连续自愈应累加 streak，使退避指数增长"
        );

        // ⭐ 判据一（新增，防「指数退避从未生效」）：**未被自愈复活**的号成功，不清零。
        //
        // 原判据是「任意号成功即清零」。线上池子成功率 99.7%，成功持续不断 ⇒ streak 每次
        // 自增后立刻被清回 0 ⇒ 退避恒为 `BASE × 2^0` = 60s ⇒ 死号每 60 秒被复活一次。
        // 实测日志坐实：`执行自愈` 间隔全部聚集在恰好 60.0s、`连续第 N 次` 70 次落 N=1。
        //
        // 把 `report_success` 改回无条件 `store(0)` → 本断言必 FAILED。
        mgr.report_success(1);
        assert_eq!(
            mgr.self_heal_streak.load(Ordering::Relaxed),
            3,
            "未被自愈复活的号成功不该清零 —— 否则健康号的持续成功会让指数退避永不生效"
        );

        // ⭐ 判据二（保留原不变量，防「单向棘轮」）：**被自愈复活**的号成功，必须清零。
        //
        // 这是原测试守的东西，不能丢：streak 只增不减会让退避爬到 900s 上限并永远停在
        // 那里，即使号池早已恢复（与 health.rs 那批『单向棘轮』缺陷同型）。
        // 号池真恢复时，被复活的号自然会成功 → 仍能解棘轮。
        mgr.self_heal_revived.lock().insert(1);
        mgr.report_success(1);
        assert_eq!(
            mgr.self_heal_streak.load(Ordering::Relaxed),
            0,
            "被自愈复活的号成功必须清零；否则号池恢复后退避仍卡在上限（15 分钟）"
        );

        // ⭐ 判据三：命中后必须移出集合，否则后续每次成功都重复清零 = 退回原判据。
        mgr.self_heal_streak.fetch_add(5, Ordering::Relaxed);
        mgr.report_success(1);
        assert_eq!(
            mgr.self_heal_streak.load(Ordering::Relaxed),
            5,
            "同一批复活只应打断一次 —— 留在集合里会让每次成功都清零，等价于退回原判据"
        );
    }

    /// 多开：同一账号可导入多份，且**每份 machineId 必须不同**。
    ///
    /// 这是「一个号导入多次、每次机器码不同、再各配代理试探并发」这个需求的承重测试。
    /// 三条断言分别锁住它成立的三个前提：
    ///
    /// 1. 默认路径**仍然去重**（多开不能把误双击上号的护栏一起拆掉）；
    /// 2. 显式路径能绕过去重；
    /// 3. 两份的 machineId **不相同** —— 若相同，上游会按设备指纹把它们关联封禁，
    ///    多开反而变成"把两份一起烧掉"。
    ///
    /// 第 3 条的机制：`generate_from_credentials` 对 api_key 号是
    /// `sha256("KiroAPIKey/" + key)`（**确定性**），故两份派生出同一个指纹，
    /// 随后入池处的撞车检测把第 2 份轮换成独立随机值。测试直接验最终状态而不验机制，
    /// 这样将来换实现（比如改成入池即随机）也不会误报失败。
    #[tokio::test]
    async fn test_multi_open_allows_duplicate_with_distinct_machine_ids() {
        const KEY: &str = "ksk_multi_open_probe_key";
        let manager = MultiTokenManager::new(Config::default(), vec![], None, None, false).unwrap();

        let mk = || {
            let mut c = KiroCredentials::default();
            c.auth_method = Some("api_key".to_string());
            c.kiro_api_key = Some(KEY.to_string());
            c
        };

        let id1 = manager.add_credential(mk()).await.expect("第 1 份应成功");

        // ① 默认路径仍去重：多开不得削弱误操作护栏。
        let dup = manager.add_credential(mk()).await;
        assert!(
            dup.is_err()
                && dup
                    .as_ref()
                    .err()
                    .unwrap()
                    .to_string()
                    .contains("凭据已存在"),
            "默认路径必须仍然拒绝重复 kiroApiKey（否则误双击上号会静默多出一条号）"
        );

        // ② 显式多开路径放行。
        let id2 = manager
            .add_credential_allowing_duplicate(mk())
            .await
            .expect("显式多开必须允许同 key 再入池");
        assert_ne!(id1, id2, "两份必须是独立凭据（各自独立的 id）");

        // ③ machineId 必须不同 —— 这是多开有意义的前提。
        let m1 = manager
            .export_credential(id1)
            .and_then(|c| c.machine_id)
            .expect("第 1 份应已冻结 machineId");
        let m2 = manager
            .export_credential(id2)
            .and_then(|c| c.machine_id)
            .expect("第 2 份应已冻结 machineId");
        assert_ne!(
            m1, m2,
            "两份 machineId 相同 → 上游按设备指纹关联封禁，多开等于把两份一起烧掉"
        );
        assert_eq!(m2.len(), 64, "轮换后的指纹应是 64 位 hex");
    }

    /// 多开的第二个前提：每份**各自独立成族**，不会被族级连坐一锅端。
    ///
    /// `family_key` 是限流/健康的分组单位。若多开的 N 份共享族键，则一份被 403 风控
    /// 会让整族退避 —— 多开就完全失去意义（等于 N 份同生共死）。
    /// api_key/idc/social 号返回 `cred:{id}` 各自独立；只有 M365 external_idp 才共享
    /// `m365:{tenant}`。本测试锁住 api_key 这条路径。
    #[tokio::test]
    async fn test_multi_open_copies_are_in_separate_families() {
        const KEY: &str = "ksk_family_isolation_probe";
        let manager = MultiTokenManager::new(Config::default(), vec![], None, None, false).unwrap();

        let mk = || {
            let mut c = KiroCredentials::default();
            c.auth_method = Some("api_key".to_string());
            c.kiro_api_key = Some(KEY.to_string());
            c
        };

        let id1 = manager.add_credential(mk()).await.expect("第 1 份");
        let id2 = manager
            .add_credential_allowing_duplicate(mk())
            .await
            .expect("第 2 份");

        let f1 = manager.family_key_of(id1);
        let f2 = manager.family_key_of(id2);
        assert_ne!(
            f1, f2,
            "多开的两份共享了族键 → 一份被 403 风控会让整族退避，多开失去意义"
        );
        assert_eq!(f1, format!("cred:{id1}"), "api_key 号应各自独立成族");
        assert_eq!(f2, format!("cred:{id2}"));
    }

    /// 回归：导入一个标了 disabled 的凭据，必须以**禁用态**入池。
    ///
    /// 事故现场：此前 add_credential 硬编码 `disabled: false`，于是重新导入已知被上游
    /// 封禁的号会让它以启用态回池；而 persist_credentials 从内存全量重写
    /// credentials.json，一次导入还会把同批次其它号刚落盘的禁用状态一起刷掉，
    /// 表现为「第二次导入后全部凭据都启用了」。
    #[tokio::test]
    async fn test_add_credential_preserves_disabled_flag() {
        let manager = MultiTokenManager::new(Config::default(), vec![], None, None, false).unwrap();

        let mut cred = KiroCredentials::default();
        cred.kiro_api_key = Some("ksk_banned_account_key".to_string());
        cred.auth_method = Some("api_key".to_string());
        cred.disabled = true;

        let id = manager.add_credential(cred).await.unwrap();

        let entries = manager.entries.lock();
        let entry = entries.iter().find(|e| e.id == id).expect("凭据应已入池");
        assert!(entry.disabled, "标了 disabled 的凭据必须以禁用态入池");
        assert_eq!(
            entry.disabled_reason,
            Some(DisabledReason::Manual),
            "调用方显式要求的禁用应记为 Manual，与上游封禁等自动判定区分"
        );
    }

    /// 对照组：未指定 disabled 时仍默认启用（serde default = false），确保无回归。
    #[tokio::test]
    async fn test_add_credential_defaults_to_enabled() {
        let manager = MultiTokenManager::new(Config::default(), vec![], None, None, false).unwrap();

        let mut cred = KiroCredentials::default();
        cred.kiro_api_key = Some("ksk_normal_account_key".to_string());
        cred.auth_method = Some("api_key".to_string());
        // 不设 disabled，走 Default

        let id = manager.add_credential(cred).await.unwrap();

        let entries = manager.entries.lock();
        let entry = entries.iter().find(|e| e.id == id).expect("凭据应已入池");
        assert!(!entry.disabled, "未指定 disabled 的新号应默认启用");
        assert_eq!(entry.disabled_reason, None);
    }

    #[tokio::test]
    async fn test_add_credential_api_key_success() {
        let config = Config::default();
        let manager = MultiTokenManager::new(config, vec![], None, None, false).unwrap();

        let mut api_key_cred = KiroCredentials::default();
        api_key_cred.kiro_api_key = Some("ksk_test_key_123".to_string());
        api_key_cred.auth_method = Some("api_key".to_string());

        let result = manager.add_credential(api_key_cred).await;
        assert!(result.is_ok());
        let id = result.unwrap();
        assert!(id > 0);
        assert_eq!(manager.total_count(), 1);
        assert_eq!(manager.available_count(), 1);
    }

    #[tokio::test]
    async fn test_add_credential_reject_duplicate_api_key() {
        let config = Config::default();

        let mut existing = KiroCredentials::default();
        existing.kiro_api_key = Some("ksk_existing_key".to_string());
        existing.auth_method = Some("api_key".to_string());

        let manager = MultiTokenManager::new(config, vec![existing], None, None, false).unwrap();

        let mut duplicate = KiroCredentials::default();
        duplicate.kiro_api_key = Some("ksk_existing_key".to_string());
        duplicate.auth_method = Some("api_key".to_string());

        let result = manager.add_credential(duplicate).await;
        assert!(result.is_err());
        assert!(
            result
                .err()
                .unwrap()
                .to_string()
                .contains("kiroApiKey 重复")
        );
    }

    /// 🔴 并发导入同一个 ksk key 时，**只能有一条入池**（TOCTOU 收口回归）。
    ///
    /// # 这条守的是什么
    ///
    /// `add_credential_inner` 的查重与插入分处两把**不同**的锁：查重块取锁读完即释放，
    /// `push` 才重新取锁。api_key 号在第 3 步走 `new_cred.clone()` 分支、**不执行**那次
    /// `refresh_token().await` ⇒ 从查重到插入是一段纯同步代码，两个 worker 线程可真并行
    /// 走完，无需任何 await 交错。
    ///
    /// 而 `import_keys` 正是用 `Semaphore(IMPORT_MAX_IN_FLIGHT)` **并发**派发每条、跑在
    /// 多线程运行时上 ⇒ 同一批里含 N 个相同 key 时，N 条会全部通过查重、全部入池。
    /// 后果是同一账号在池中裂成 N 条共用一份上游配额，而上游按**账号**算风控
    /// （CLAUDE.md 记载过 11 个号连坐被 suspiciousActivityAuto 禁用的事故形态）。
    ///
    /// # 为什么用 `multi_thread` 而不是默认单线程 runtime
    ///
    /// 默认 `#[tokio::test]` 是**单线程**运行时：两个 task 之间只在 `.await` 点切换，
    /// 而这条路径上查重到插入之间没有 await ⇒ **单线程下竞态不可能复现**，测试会假绿。
    /// 必须显式要求多 worker 才能真正并行。
    ///
    /// 注：即便如此，能否稳定命中窗口仍依赖调度时序，所以断言写成「最终只有 1 条」
    /// （这在修复后**恒成立**），而不是「必须观察到一次并发拦截」（那依赖运气）。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_import_of_same_api_key_must_insert_only_one() {
        let config = Config::default();
        let manager = std::sync::Arc::new(
            MultiTokenManager::new(config, vec![], None, None, false).unwrap(),
        );

        // 8 个任务并发导入**同一把** key。
        let mut tasks = tokio::task::JoinSet::new();
        for _ in 0..8 {
            let mgr = std::sync::Arc::clone(&manager);
            tasks.spawn(async move {
                let mut c = KiroCredentials::default();
                c.kiro_api_key = Some("ksk_same_key_concurrent".to_string());
                c.auth_method = Some("api_key".to_string());
                mgr.add_credential(c).await
            });
        }
        let mut ok = 0usize;
        while let Some(joined) = tasks.join_next().await {
            if joined.expect("任务不应 panic").is_ok() {
                ok += 1;
            }
        }

        assert_eq!(ok, 1, "并发导入同一 key 只应有 1 条成功，实际 {ok} 条");
        assert_eq!(
            manager.total_count(),
            1,
            "池中必须只有 1 条 —— 裂成多条会让同一账号共用配额并触发上游账号级风控"
        );
    }

    /// 多开（`add_credential_allowing_duplicate`）不受去重复检影响：
    /// 它的语义就是**显式允许**同 key 多份，复检必须与初检一样对它整段跳过。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn allow_duplicate_still_permits_multiple_copies_after_recheck() {
        let config = Config::default();
        let manager =
            MultiTokenManager::new(config, vec![], None, None, false).unwrap();

        let mut first = KiroCredentials::default();
        first.kiro_api_key = Some("ksk_multi_open".to_string());
        first.auth_method = Some("api_key".to_string());
        manager
            .add_credential(first)
            .await
            .expect("首条应成功");

        // 第二份走多开入口：即便 key 完全相同也必须成功（否则多开功能被复检打死）。
        let mut second = KiroCredentials::default();
        second.kiro_api_key = Some("ksk_multi_open".to_string());
        second.auth_method = Some("api_key".to_string());
        manager
            .add_credential_allowing_duplicate(second)
            .await
            .expect("多开入口必须绕过去重复检");

        assert_eq!(manager.total_count(), 2, "多开应得到 2 份");
    }

    #[tokio::test]
    async fn test_add_credential_api_key_empty_rejected() {
        let config = Config::default();
        let manager = MultiTokenManager::new(config, vec![], None, None, false).unwrap();

        let mut cred = KiroCredentials::default();
        cred.kiro_api_key = Some(String::new());
        cred.auth_method = Some("api_key".to_string());

        let result = manager.add_credential(cred).await;
        assert!(result.is_err());
        assert!(
            result
                .err()
                .unwrap()
                .to_string()
                .contains("kiroApiKey 为空")
        );
    }

    #[tokio::test]
    async fn test_add_credential_api_key_missing_key_rejected() {
        let config = Config::default();
        let manager = MultiTokenManager::new(config, vec![], None, None, false).unwrap();

        let mut cred = KiroCredentials::default();
        cred.auth_method = Some("api_key".to_string());
        // kiro_api_key is None

        let result = manager.add_credential(cred).await;
        assert!(result.is_err());
        assert!(
            result
                .err()
                .unwrap()
                .to_string()
                .contains("缺少 kiroApiKey")
        );
    }

    #[tokio::test]
    async fn test_add_credential_api_key_and_oauth_coexist() {
        let config = Config::default();

        let mut oauth_cred = KiroCredentials::default();
        oauth_cred.refresh_token = Some("a".repeat(150));

        let manager = MultiTokenManager::new(config, vec![oauth_cred], None, None, false).unwrap();

        let mut api_key_cred = KiroCredentials::default();
        api_key_cred.kiro_api_key = Some("ksk_new_key".to_string());
        api_key_cred.auth_method = Some("api_key".to_string());

        let result = manager.add_credential(api_key_cred).await;
        assert!(result.is_ok());
        assert_eq!(manager.total_count(), 2);
        assert_eq!(manager.available_count(), 2);
    }

    #[tokio::test]
    async fn test_credential_id_never_reused_after_purge() {
        // 回归:删号→从回收站彻底清除(purge)→再加号,新号绝不复用被清除的 id。
        // 旧算法 max(entries∪trash)+1 会在 purge 后回落复用 id,使新号继承死号残留的
        // cooldown/model_blocklist 内存态。单调 id 计数器根治之。custom_api 号免网络校验。
        let config = Config::default();
        let mk = |url: &str| {
            let mut c = KiroCredentials::default();
            c.auth_method = Some("custom_api".to_string());
            c.base_url = Some(url.to_string());
            c
        };
        let mgr = MultiTokenManager::new(config, vec![], None, None, false).unwrap();
        let id1 = mgr
            .add_credential(mk("https://a.example.invalid"))
            .await
            .unwrap();
        let id2 = mgr
            .add_credential(mk("https://b.example.invalid"))
            .await
            .unwrap();
        assert!(id2 > id1, "id 应单调递增: #{id1} → #{id2}");

        // 删除最高 id 的号并从回收站彻底清除。
        mgr.set_disabled(id2, true).unwrap();
        mgr.delete_credential(id2).unwrap();
        mgr.purge_credential(id2).unwrap();

        // 此刻 entries∪trash 的 max 已回落到 id1;旧算法会把 id2 分配给新号(复用),
        // 计数器则继续给 id2 之后的值。
        let id3 = mgr
            .add_credential(mk("https://c.example.invalid"))
            .await
            .unwrap();
        assert!(
            id3 > id2,
            "purge 后新号 id 必须 > 已清除的 id,不得复用(新号 #{id3},已清除 #{id2})"
        );
    }

    #[tokio::test]
    async fn test_delete_clears_per_id_cooldown_and_restore_is_clean() {
        // 回归:删号应清掉其 per-id 调度内存态(cooldown 等);从回收站按原 id 恢复的号
        // 不得继承删除前的长冷却而被静默跳过。
        use crate::kiro::cooldown::CooldownReason;
        let config = Config::default();
        let mut c = KiroCredentials::default();
        c.auth_method = Some("custom_api".to_string());
        c.base_url = Some("https://relay.example.invalid".to_string());
        let mgr = MultiTokenManager::new(config, vec![], None, None, false).unwrap();
        let id = mgr.add_credential(c).await.unwrap();

        // 打一个长冷却(账户暂停=24h,测试期内不会自然到期)。
        mgr.cooldown
            .set_cooldown(id, CooldownReason::AccountSuspended);
        assert!(
            mgr.cooldown_snapshot()
                .iter()
                .any(|i| i.credential_id == id),
            "冷却应已设置"
        );

        // 禁用 + 删除:delete_credential 应清掉该号的 per-id 冷却态。
        mgr.set_disabled(id, true).unwrap();
        mgr.delete_credential(id).unwrap();

        // 从回收站恢复(id 不变):不应再背着删除前的长冷却。
        mgr.restore_credential(id, false).unwrap();
        assert!(
            !mgr.cooldown_snapshot()
                .iter()
                .any(|i| i.credential_id == id),
            "restore 后不应继承删除前的冷却(#{id} 仍在冷却快照 = 泄漏)"
        );
    }

    // MultiTokenManager 测试

    #[test]
    fn test_multi_token_manager_new() {
        let config = Config::default();
        let mut cred1 = KiroCredentials::default();
        cred1.priority = 0;
        let mut cred2 = KiroCredentials::default();
        cred2.priority = 1;

        let manager =
            MultiTokenManager::new(config, vec![cred1, cred2], None, None, false).unwrap();
        assert_eq!(manager.total_count(), 2);
        assert_eq!(manager.available_count(), 2);
    }

    #[test]
    fn test_duplicate_machine_id_auto_rotated() {
        // 两个凭据显式共用同一 machineId → 入池时应把重复者轮换成独立指纹(防关联)。
        let config = Config::default();
        let shared = "a".repeat(64); // 合法 64-hex 格式,两个凭据故意相同
        let mut c1 = KiroCredentials::default();
        c1.id = Some(1);
        c1.machine_id = Some(shared.clone());
        let mut c2 = KiroCredentials::default();
        c2.id = Some(2);
        c2.machine_id = Some(shared.clone());

        let mgr = MultiTokenManager::new(config, vec![c1, c2], None, None, false).unwrap();
        let m1 = mgr.export_credential(1).unwrap().machine_id.unwrap();
        let m2 = mgr.export_credential(2).unwrap().machine_id.unwrap();
        assert_ne!(m1, m2, "重复 machineId 应被自动轮换成不同值");
        // 第一个保留原值,第二个被轮换(64 hex)
        assert_eq!(m1, shared, "首个保留原 machineId");
        assert_eq!(m2.len(), 64, "轮换后应为 64 hex");
        assert!(m2.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn test_select_custom_api_priority_and_failover_exclude() {
        // custom_api 池内调度:①优先级小先选 ②exclude 排除已试号 failover 到下一个
        // ③全部 exclude → None(供上层落 Kiro 主力)。
        use std::collections::HashSet;
        let config = Config::default();
        let mk = |id: u64, prio: u32| {
            let mut c = KiroCredentials::default();
            c.id = Some(id);
            c.auth_method = Some("custom_api".to_string());
            c.base_url = Some(format!("https://relay{id}.example.invalid"));
            c.api_key = Some(format!("sk-{id}"));
            c.priority = prio;
            c
        };
        // #1 prio0, #2 prio0, #3 prio1
        let mgr = MultiTokenManager::new(
            config,
            vec![mk(1, 0), mk(2, 0), mk(3, 1)],
            None,
            None,
            false,
        )
        .unwrap();

        let empty = HashSet::new();
        // 初选:priority 最小(0)的 #1/#2 之一(同级按 RPM 均衡,初始 RPM 全 0 → 取 id 最小 #1)。
        let first = mgr.select_custom_api(&empty, None).expect("应选到 custom_api 号");
        assert!(
            first.0 == 1 || first.0 == 2,
            "应先选 priority=0 的号,得到 #{}",
            first.0
        );

        // failover:排除 #1、#2 后应落到 priority=1 的 #3(仍在 custom_api 池内,不跳类型)。
        let mut ex: HashSet<u64> = HashSet::new();
        ex.insert(1);
        ex.insert(2);
        let third = mgr
            .select_custom_api(&ex, None)
            .expect("排除两个 prio0 后应选 #3");
        assert_eq!(third.0, 3, "failover 应落到 priority=1 的 #3");

        // 全部排除 → None(上层据此落 Kiro 主力路径)。
        ex.insert(3);
        assert!(
            mgr.select_custom_api(&ex, None).is_none(),
            "全部 custom_api 排除后应返回 None"
        );
    }

    /// 并发上限硬门（迁移差距 P1）：inflight 达 [`CREDENTIAL_MAX_CONCURRENCY`] 的号
    /// **不可选**（镜像 kiro-rs-admin `is_concurrency_exceeded` 的硬门语义）。
    /// ①超上限的号被跳过选别的号；②全部达限 → None（上层 failover/429 背压），
    /// 绝不回退到"灌爆它"；③guard Drop 释放名额后该号恢复可选（guard 与硬门联动）。
    #[test]
    fn test_select_custom_api_skips_credential_at_max_concurrency() {
        use std::collections::HashSet;
        let mk = |id: u64| {
            let mut c = KiroCredentials::default();
            c.id = Some(id);
            c.auth_method = Some("custom_api".to_string());
            c.base_url = Some(format!("https://relay{id}.example.invalid"));
            c.api_key = Some(format!("sk-{id}"));
            c
        };
        let mgr = MultiTokenManager::new(Config::default(), vec![mk(1), mk(2)], None, None, false)
            .unwrap();
        let empty = HashSet::new();

        // #1 达上限 → 被跳过，选 #2。
        mgr.entries
            .lock()
            .iter_mut()
            .find(|e| e.id == 1)
            .unwrap()
            .inflight
            .store(CREDENTIAL_MAX_CONCURRENCY, Ordering::Relaxed);
        let sel = mgr.select_custom_api(&empty, None).expect("池内还有 #2 可选");
        assert_eq!(sel.0, 2, "达上限的 #1 必须被跳过，选 #2");

        // #2 也达上限 → 全部达限 → None（硬门，不灌爆）。
        mgr.entries
            .lock()
            .iter_mut()
            .find(|e| e.id == 2)
            .unwrap()
            .inflight
            .store(CREDENTIAL_MAX_CONCURRENCY, Ordering::Relaxed);
        assert!(
            mgr.select_custom_api(&empty, None).is_none(),
            "全部达并发上限时应返回 None（背压信号，客户端退避后自然释放）"
        );

        // guard Drop 释放名额 → 号恢复可选（guard 是持有期标记、硬门是上限，联动正确）。
        drop(sel.2);
        assert!(
            mgr.select_custom_api(&empty, None).is_some(),
            "guard 释放后 #2 应恢复可选"
        );
    }

    /// 并发上限硬门也作用于 **Kiro 主路径**（`is_entry_selectable_inner` 硬门 +
    /// `is_sticky_reuse_healthy` 亲和复用封堵）：达上限的 Kiro 号不可选。
    #[test]
    fn test_kiro_select_skips_credential_at_max_concurrency() {
        use std::collections::HashSet;
        let mut config = Config::default();
        config.affinity_enabled = false; // 隔离亲和路径，只测通用选号硬门
        let mk = |id: u64| {
            let mut c = KiroCredentials::default();
            c.id = Some(id);
            c.auth_method = Some("api_key".to_string());
            c.kiro_api_key = Some(format!("sk-kiro-{id}"));
            c
        };
        let mgr = MultiTokenManager::new(config, vec![mk(1), mk(2)], None, None, false).unwrap();
        let empty = HashSet::new();

        // #1 达上限 → 跳过，选 #2。
        mgr.entries
            .lock()
            .iter_mut()
            .find(|e| e.id == 1)
            .unwrap()
            .inflight
            .store(CREDENTIAL_MAX_CONCURRENCY, Ordering::Relaxed);
        let (id, _, _) = mgr
            .select_next_credential(None, None, &empty)
            .expect("池内还有 #2 可选");
        assert_eq!(id, 2, "达上限的 #1 必须被跳过，选 #2");

        // 全部达限 → None（由 acquire_context 的 ConcurrencyFull 短等重试兜住，非忙等）。
        mgr.entries
            .lock()
            .iter_mut()
            .find(|e| e.id == 2)
            .unwrap()
            .inflight
            .store(CREDENTIAL_MAX_CONCURRENCY, Ordering::Relaxed);
        assert!(
            mgr.select_next_credential(None, None, &empty).is_none(),
            "全部达并发上限时 Kiro 选号应返 None"
        );
    }

    /// P2-8：两号池上，批量 p_avail 选号必须与逐个 `p_avail_with_load_ref` 预言机同胜者。
    ///
    /// 12 键本身不动；本条只锁「读路径批量化不得翻盘」。其它 10 键在两号全平局时
    /// 不参与比较（同 starved / prio / inflight / 白名单），胜负只由 p_avail 派生位
    /// （unusable / health_tier / neg_p_fine）和末位 `(success_count, id)` 决定。
    #[test]
    fn test_select_batched_p_avail_same_winner_as_one_by_one_oracle() {
        use std::collections::HashSet;
        let mut config = Config::default();
        config.affinity_enabled = false;
        let mgr = MultiTokenManager::new(
            config,
            vec![mk_direct_kiro(1), mk_direct_kiro(2)],
            None,
            None,
            false,
        )
        .expect("构造 manager");
        let empty = HashSet::new();
        let fam1 = mgr.family_key_of(1);
        let fam2 = mgr.family_key_of(2);
        let load_ref = crate::kiro::health::adaptive_load_ref(0, 2);
        let oracle = |p1: f64, p2: f64| -> u64 {
            let key = |p: f64, id: u64| {
                let unusable = u8::from(p <= 0.0);
                let health_tier = crate::kiro::health::health_tier(p);
                let neg_p_fine = -((p * 1000.0) as i64);
                (
                    unusable,
                    1u8,
                    0u32,
                    health_tier,
                    0u8,
                    1u8,
                    0u32,
                    0u32,
                    0u32,
                    0u32,
                    neg_p_fine,
                    (0u64, id),
                )
            };
            if key(p1, 1) <= key(p2, 2) {
                1
            } else {
                2
            }
        };

        let p1 = mgr
            .health
            .p_avail_with_load_ref(&fam1, 0, 0, 30, load_ref);
        let p2 = mgr
            .health
            .p_avail_with_load_ref(&fam2, 0, 0, 30, load_ref);
        let (id, _, _) = mgr
            .select_next_credential(None, None, &empty)
            .expect("两号都可选");
        assert_eq!(
            id,
            oracle(p1, p2),
            "健康两号：批量选号必须与逐个 p_avail 预言机同胜者"
        );

        for _ in 0..crate::kiro::health::TRIP_THRESHOLD {
            mgr.health.on_429(&fam1);
        }
        let p1 = mgr
            .health
            .p_avail_with_load_ref(&fam1, 1, 0, 30, load_ref);
        let p2 = mgr
            .health
            .p_avail_with_load_ref(&fam2, 0, 0, 30, load_ref);
        let (id, _, _) = mgr
            .select_next_credential(None, None, &empty)
            .expect("熔断后仍有 #2");
        assert_eq!(
            id,
            oracle(p1, p2),
            "熔断后批量选号必须与逐个 p_avail 预言机同胜者"
        );
        assert_eq!(id, 2, "熔断 #1 后必须选 #2");
    }

    /// transient_wait_outcome 的并发上限镜像（与 `is_entry_selectable_inner` 逐条对齐，
    /// 否则 select 返 None + 本函数判 Available → 忙等热循环）：
    /// 全部达限时返回 `Wait(_, ConcurrencyFull)`（短固定等待后重试），绝不 `Available`。
    #[test]
    fn test_transient_wait_reports_concurrency_full_when_all_at_max() {
        let mut config = Config::default();
        config.load_balancing_mode = "balanced".to_string();
        config.affinity_enabled = false;
        let mut c = KiroCredentials::default();
        c.auth_method = Some("api_key".to_string());
        c.kiro_api_key = Some("sk-kiro-1".to_string());
        let mgr = MultiTokenManager::new(config, vec![c], None, None, false).unwrap();

        // 全部（唯一）达限 → 不是立即可用，而是 Wait(ConcurrencyFull)。
        mgr.entries
            .lock()
            .iter_mut()
            .find(|e| e.id == 1)
            .unwrap()
            .inflight
            .store(CREDENTIAL_MAX_CONCURRENCY, Ordering::Relaxed);
        match mgr.transient_wait_outcome(None) {
            WaitOutcome::Wait(d, WaitReason::ConcurrencyFull) => {
                assert!(
                    d <= StdDuration::from_secs(2),
                    "并发释放是毫秒级的，等待应短（实际 {:?}）",
                    d
                );
            }
            other => panic!("全达限应返回 Wait(ConcurrencyFull)，实际 {:?}", other),
        }
    }

    /// 纯 custom_api 池全部达并发上限：wait_outcome 是 ConcurrencyFull（不是 Available，
    /// 也不是 Kiro 那条 `transient_wait_outcome`——那边跳过代挂号会判 NoCandidate）。
    #[test]
    fn test_custom_api_wait_outcome_concurrency_full_when_all_at_max() {
        use std::collections::HashSet;
        let mk = |id: u64| {
            let mut c = KiroCredentials::default();
            c.id = Some(id);
            c.auth_method = Some("custom_api".to_string());
            c.base_url = Some(format!("https://relay{id}.example.invalid"));
            c.api_key = Some(format!("sk-{id}"));
            c
        };
        let mgr = MultiTokenManager::new(Config::default(), vec![mk(1)], None, None, false)
            .unwrap();
        mgr.entries
            .lock()
            .iter_mut()
            .find(|e| e.id == 1)
            .unwrap()
            .inflight
            .store(CREDENTIAL_MAX_CONCURRENCY, Ordering::Relaxed);
        match mgr.custom_api_wait_outcome(&HashSet::new(), None) {
            WaitOutcome::Wait(d, WaitReason::ConcurrencyFull) => {
                assert!(
                    d <= StdDuration::from_secs(2),
                    "并发释放是毫秒级的，等待应短（实际 {:?}）",
                    d
                );
            }
            other => panic!("纯代挂全达限应 Wait(ConcurrencyFull)，实际 {:?}", other),
        }
        // 两池隔离：Kiro 侧仍把代挂号当无候选。
        assert_eq!(
            mgr.transient_wait_outcome(None),
            WaitOutcome::NoCandidate,
            "transient_wait_outcome 不得把 custom_api 并发满算进 Kiro 池"
        );
        assert!(
            !mgr.has_kiro_selectable(None),
            "纯代挂池不得报有 Kiro 可选"
        );
    }

    /// 纯代挂池全满：短等后 inflight 释放 → 选到号。不得立刻 None。
    #[tokio::test]
    async fn test_select_custom_api_or_wait_concurrency_full_then_success() {
        use std::collections::HashSet;
        let mut c = KiroCredentials::default();
        c.id = Some(1);
        c.auth_method = Some("custom_api".to_string());
        c.base_url = Some("https://relay.example.invalid".to_string());
        c.api_key = Some("sk-relay".to_string());
        let mgr = MultiTokenManager::new(Config::default(), vec![c], None, None, false).unwrap();
        let inflight = mgr
            .entries
            .lock()
            .iter()
            .find(|e| e.id == 1)
            .unwrap()
            .inflight
            .clone();
        inflight.store(CREDENTIAL_MAX_CONCURRENCY, Ordering::Release);
        assert!(
            mgr.select_custom_api(&HashSet::new(), None).is_none(),
            "同步选号在全满时仍必须立刻 None（wait 在 or_wait 层）"
        );

        let inflight2 = inflight.clone();
        tokio::spawn(async move {
            tokio::time::sleep(StdDuration::from_millis(40)).await;
            inflight2.store(0, Ordering::Release);
        });
        let sel = tokio::time::timeout(
            StdDuration::from_secs(3),
            mgr.select_custom_api_or_wait(&HashSet::new(), None),
        )
        .await
        .expect("纯代挂满并发短等不得挂死")
        .expect("inflight 释放后应选到号");
        assert_eq!(sel.0, 1);
    }

    /// 混池：custom_api 全满但有可选 Kiro → or_wait 立刻 None（分流 Kiro），不睡。
    #[tokio::test]
    async fn test_select_custom_api_or_wait_mixed_pool_none_immediately() {
        use std::collections::HashSet;
        let mk_custom = || {
            let mut c = KiroCredentials::default();
            c.id = Some(1);
            c.auth_method = Some("custom_api".to_string());
            c.base_url = Some("https://relay.example.invalid".to_string());
            c.api_key = Some("sk-relay".to_string());
            c
        };
        let mk_kiro = || {
            let mut c = KiroCredentials::default();
            c.id = Some(2);
            c.auth_method = Some("api_key".to_string());
            c.kiro_api_key = Some("sk-kiro-2".to_string());
            c
        };
        let mut config = Config::default();
        config.affinity_enabled = false;
        let mgr = MultiTokenManager::new(config, vec![mk_custom(), mk_kiro()], None, None, false)
            .unwrap();
        mgr.entries
            .lock()
            .iter_mut()
            .find(|e| e.id == 1)
            .unwrap()
            .inflight
            .store(CREDENTIAL_MAX_CONCURRENCY, Ordering::Relaxed);
        assert!(
            mgr.has_kiro_selectable(None),
            "混池必须看见可选 Kiro"
        );
        let started = Instant::now();
        let sel = mgr
            .select_custom_api_or_wait(&HashSet::new(), None)
            .await;
        assert!(sel.is_none(), "混池 custom 满必须立刻 None 分流 Kiro");
        assert!(
            started.elapsed() < StdDuration::from_millis(150),
            "混池不得短等 ConcurrencyFull，实际 {:?}",
            started.elapsed()
        );
    }

    /// 造一个代挂号管理器（单号，供透传惩罚策略测试用）。
    fn mk_passthrough_mgr() -> MultiTokenManager {
        let mut c = KiroCredentials::default();
        c.id = Some(1);
        c.auth_method = Some("custom_api".to_string());
        c.base_url = Some("https://relay.example.invalid".to_string());
        c.api_key = Some("sk-relay".to_string());
        MultiTokenManager::new(Config::default(), vec![c], None, None, false).unwrap()
    }

    /// 回归（dwgx 明确要求）：代挂号**偶尔 429 绝不受任何惩罚**。
    ///
    /// 语义依据：代挂号是用户自购的付费第三方中转站，**没有"被风控"这个状态**，
    /// 429 只代表"它现在忙"。把它按下去既不能让它变快，又白白缩小可用池
    /// （两个号轮流 429 就会两个都被冷却 → 整池不可用 → 回落 Kiro，而 Kiro 侧此刻
    /// 可能正被风控烧号）。
    ///
    /// **旧代码为何 FAIL**：`provider.rs` 的透传 failover 对 429 给 30s 冷却
    /// （`429 => 30`），于是一次 429 就让该号在 `select_custom_api` 里被跳过 30 秒。
    #[test]
    fn test_passthrough_occasional_429_incurs_no_cooldown_and_no_penalty() {
        use std::collections::HashSet;
        let mgr = mk_passthrough_mgr();

        // 连打若干次 429（远超任何失败阈值），中间不成功。
        for _ in 0..10 {
            mgr.record_passthrough_result(1, crate::usage::RequestOutcome::RateLimited);
        }

        assert_eq!(
            mgr.available_count(),
            1,
            "偶尔/连续 429 无论持续多久都不得禁用代挂号"
        );
        assert!(
            mgr.select_custom_api(&HashSet::new(), None).is_some(),
            "429 不得进入惩罚系统：号必须仍可被选中（旧代码 429=>30s 惩罚性冷却，此处会是 None）"
        );
        // 边界说明：provider 的透传 failover 另有一个 **5s 调度级跳过**（不进 health、不计失败、
        // 不影响自动禁用判据），用于避免 100% 429 的站被每个新请求重新撞一次。
        // 那不是惩罚，且不经过本函数 —— 故本测试只锁「惩罚系统不得介入」这条。
    }

    /// 用户硬约束：custom_api 是代挂上游，不是 Kiro 凭据。所有上游结果都只能记录或
    /// 引发临时 failover，不得改变管理员设置的 enabled 状态。
    #[test]
    fn test_passthrough_upstream_results_never_auto_disable() {
        let mgr = mk_passthrough_mgr();

        use crate::usage::RequestOutcome as RO;
        let every_failure = [
            RO::RateLimited,
            RO::AuthFailed,
            RO::QuotaExhausted,
            RO::AccountSuspended,
            RO::ServerError,
            RO::BadRequest,
            RO::NetworkError,
            RO::OtherError,
            RO::ModelUnavailable,
            RO::EmptyResponse,
            RO::Interrupted,
        ];
        for outcome in every_failure {
            for _ in 0..(MAX_PASSTHROUGH_FAILURES * 4) {
                mgr.record_passthrough_result(1, outcome);
            }
        }

        assert_eq!(mgr.available_count(), 1, "代挂号必须始终保持启用");
        let snap = mgr.snapshot();
        let e = snap.entries.iter().find(|e| e.id == 1).unwrap();
        assert!(!e.disabled);
        assert_eq!(e.disabled_reason, None, "任何上游结果都不得写禁用原因");
    }

    /// 源码级守卫（2026-08-15，M6 决策收口）：`consecutive_passthrough_failures` 是
    /// 死字段 —— 只能在成功/429/认证类分支里**清零**，任何分支都不得**累加**它，
    /// 更不得据它写 disabled。此前 provider.rs 透传循环的注释还承诺「自动禁用由
    /// record_passthrough_result 的连续失败计数负责」——那正是与 token_manager 侧
    /// 「绝不 auto-disable」矛盾的假承诺（M6 已删）。本守卫把「死字段」语义钉死：
    /// 未来若有人想接线自动禁用（累加计数 + 达阈值禁用），必须先拆掉这条守卫。
    #[test]
    fn passthrough_failure_counter_must_stay_observational() {
        let src = include_str!("token_manager.rs");
        let body = src
            .split("pub fn record_passthrough_result")
            .nth(1)
            .expect("record_passthrough_result 不应被改名");
        let body = body
            .split("pub fn available_count")
            .next()
            .expect("函数体窗口不应被删改");
        // needle 运行时拼接（include_str! 会把测试自身的字面量也读进来）。
        let add_assign = format!("{} {}", "consecutive_passthrough_failures", "+=");
        let saturating = format!(
            "{}{}",
            "consecutive_passthrough_failures", ".saturating_add"
        );
        assert!(
            !body.contains(&add_assign),
            "record_passthrough_result 不得累加 consecutive_passthrough_failures：\
             代挂号绝不据连续失败自动禁用（字段只为内存结构/旧测试兼容）"
        );
        assert!(
            !body.contains(&saturating),
            "record_passthrough_result 不得累加 consecutive_passthrough_failures（`{}` 形态），\
             该字段只允许清零（= 0）",
            saturating
        );
    }

    /// 成功/失败混合只更新观测计数，不改 enabled。
    #[test]
    fn test_passthrough_mixed_results_only_update_observability() {
        let mgr = mk_passthrough_mgr();

        mgr.record_passthrough_result(1, crate::usage::RequestOutcome::AuthFailed);
        mgr.record_passthrough_result(1, crate::usage::RequestOutcome::Success);
        mgr.record_passthrough_result(1, crate::usage::RequestOutcome::ServerError);

        let e = mgr.snapshot().entries.into_iter().find(|e| e.id == 1).unwrap();
        assert!(!e.disabled);
        assert_eq!(e.disabled_reason, None);
        assert_eq!(e.success_count, 1);
        assert_eq!(e.request_count, 1);
        assert_eq!(e.failure_count, 2, "失败数只供观测，不是禁用阈值");
    }

    /// 防御性回归：即使调用方把 custom_api id 误送进 Kiro 的自动处置入口，
    /// 也不得关闭代挂站。这锁住的是「不管遇到什么」的边界。
    #[test]
    fn custom_api_is_immune_to_every_generic_auto_disable_entrypoint() {
        let mgr = mk_passthrough_mgr();

        for _ in 0..16 {
            assert!(mgr.report_failure(1));
            mgr.report_suspicious_activity(1);
            assert!(mgr.report_refresh_failure(1));
        }
        assert!(mgr.report_quota_exhausted(1));
        assert!(mgr.report_account_suspended(1));
        assert!(mgr.report_refresh_token_invalid(1));
        assert!(mgr.report_refresh_failure_classified(1, &anyhow::anyhow!("invalid_grant")));
        mgr.mark_region_probe_failed(
            1,
            &crate::kiro::region_probe::ProbeOutcome::NoUsableRegion,
        );
        mgr.mark_region_probe_failed(1, &crate::kiro::region_probe::ProbeOutcome::TokenDead);

        let e = mgr.snapshot().entries.into_iter().find(|e| e.id == 1).unwrap();
        assert!(!e.disabled, "通用 Kiro 禁用入口不得改变 custom_api 状态");
        assert_eq!(e.disabled_reason, None);
    }

    /// 禁止的是「自动」关闭；管理员的手动开关仍是权威。
    #[test]
    fn custom_api_manual_disable_and_enable_still_work() {
        let mgr = mk_passthrough_mgr();

        mgr.set_disabled(1, true).unwrap();
        let disabled = mgr.snapshot().entries.into_iter().find(|e| e.id == 1).unwrap();
        assert!(disabled.disabled);
        assert_eq!(disabled.disabled_reason.as_deref(), Some("Manual"));

        mgr.set_disabled(1, false).unwrap();
        let enabled = mgr.snapshot().entries.into_iter().find(|e| e.id == 1).unwrap();
        assert!(!enabled.disabled);
        assert_eq!(enabled.disabled_reason, None);
    }

    /// 旧数据可能用 `api_key + baseUrl` 表示代挂站；缺 kiroApiKey 不能让它在启动
    /// 校验里被当成 Kiro 凭据自动禁用。
    #[test]
    fn legacy_custom_api_is_not_auto_disabled_as_invalid_kiro_config() {
        let mut c = KiroCredentials::default();
        c.id = Some(1);
        c.auth_method = Some("api_key".to_string());
        c.base_url = Some("https://relay.example.invalid".to_string());
        c.api_key = Some("sk-relay".to_string());

        let mgr = MultiTokenManager::new(Config::default(), vec![c], None, None, false).unwrap();
        let e = mgr.snapshot().entries.into_iter().find(|e| e.id == 1).unwrap();
        assert!(!e.disabled);
        assert_eq!(e.disabled_reason, None);
    }

    /// 升级迁移：旧版已经写入磁盘的自动禁用要复活并持久化；管理员明确的 Manual 必须保留。
    #[test]
    fn startup_reenables_and_persists_only_auto_disabled_custom_api() {
        let dir = std::env::temp_dir().join(format!(
            "kiro-custom-api-migration-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let cred_path = dir.join("credentials.json");
        std::fs::write(
            &cred_path,
            r#"[
                {"id":1,"authMethod":"custom_api","baseUrl":"https://one.invalid","disabled":true,"disabledReason":"passthroughFailed"},
                {"id":2,"authMethod":"custom_api","baseUrl":"https://two.invalid","disabled":true,"disabledReason":"manual"},
                {"id":3,"authMethod":"custom_api","baseUrl":"https://three.invalid","disabled":true,"disabledReason":"tooManyFailures"}
            ]"#,
        )
        .unwrap();
        let creds = crate::kiro::model::credentials::CredentialsConfig::load(&cred_path)
            .unwrap()
            .into_sorted_credentials();

        let mgr = MultiTokenManager::new(
            Config::default(),
            creds,
            None,
            Some(cred_path.clone()),
            true,
        )
        .unwrap();
        let state = |id| {
            let e = mgr
                .snapshot()
                .entries
                .into_iter()
                .find(|e| e.id == id)
                .unwrap();
            (e.disabled, e.disabled_reason)
        };
        assert_eq!(state(1), (false, None), "代挂专属旧自动禁用必须复活");
        assert_eq!(state(2), (true, Some("Manual".to_string())), "人工禁用必须保留");
        assert_eq!(state(3), (false, None), "误入 Kiro 通用路径的旧自动禁用也必须复活");
        drop(mgr);

        // 必须写回磁盘，否则下一次重启又会恢复成旧禁用态。
        let persisted = crate::kiro::model::credentials::CredentialsConfig::load(&cred_path)
            .unwrap()
            .into_sorted_credentials();
        let persisted_state = |id| {
            let c = persisted.iter().find(|c| c.id == Some(id)).unwrap();
            (c.disabled, c.disabled_reason)
        };
        assert_eq!(persisted_state(1), (false, None));
        assert_eq!(
            persisted_state(2),
            (true, Some(DisabledReason::Manual))
        );
        assert_eq!(persisted_state(3), (false, None));

        std::fs::remove_dir_all(&dir).ok();
    }

    /// 代挂站即使脏数据里同时带有 kiroApiKey，也必须从 Kiro region 探测链排除。
    #[test]
    fn custom_api_is_excluded_from_region_probe_candidates() {
        let mut c = KiroCredentials::default();
        c.id = Some(1);
        c.auth_method = Some("custom_api".to_string());
        c.base_url = Some("https://relay.example.invalid".to_string());
        c.api_key = Some("sk-relay".to_string());
        c.kiro_api_key = Some("ksk_must_not_probe".to_string());

        let mgr = MultiTokenManager::new(Config::default(), vec![c], None, None, false).unwrap();
        assert!(mgr.ids_needing_region_probe().is_empty());
    }

    /// 回归（实测驱动）：`bad_request` 绝不计入号的健康 —— 那是坏请求，不是坏号。
    ///
    /// **依据是线上真实数据**，不是推理：代挂号 #216 成功率 **80.3%**（2910/3622）是个健康号，
    /// 却有 712 次 `bad_request`，其中 **119 次达到 ≥3 连**、最长 6 连。若把它计入连续失败
    /// 计数（阈值 3），这个健康号会被**误禁 119 次**。
    ///
    /// 更关键：代挂号历史 429 次数为 **0**，失败形态**全是** bad_request —— 所以"把 400 当号坏了"
    /// 恰好命中唯一真实存在的失败形态，是最容易踩的那个坑。
    ///
    /// 本测试锁住这条边界，防止将来有人"顺手"把 4xx 一起算进健康判据。
    #[test]
    fn test_passthrough_bad_request_never_counts_against_credential_health() {
        let mgr = mk_passthrough_mgr();

        // 远超阈值的连续 400：一次都不该惩罚（换号也一样错）。
        for _ in 0..(MAX_PASSTHROUGH_FAILURES * 4) {
            mgr.record_passthrough_result(1, crate::usage::RequestOutcome::BadRequest);
        }
        assert_eq!(
            mgr.available_count(),
            1,
            "客户端请求错误(400/404/422)不得禁用代挂号 —— 实测健康号有 119 次 ≥3 连 bad_request"
        );

        // 且不得污染连续计数：紧接着来 阈值-1 次真·非瞬态失败，仍不该被禁。
        for _ in 0..(MAX_PASSTHROUGH_FAILURES - 1) {
            mgr.record_passthrough_result(1, crate::usage::RequestOutcome::AuthFailed);
        }
        assert_eq!(
            mgr.available_count(),
            1,
            "bad_request 不得把连续计数垫高，否则等效于降低了真实阈值"
        );
    }

    /// 回归：瞬态失败（5xx / 网络错误）永不进连续计数、永不自动禁用。
    ///
    /// 中转站抖一下不代表它坏了，failover 换号即可。只有"再试无用"的信号才该累加。
    #[test]
    fn test_passthrough_transient_failures_never_auto_disable() {
        let mgr = mk_passthrough_mgr();

        for _ in 0..(MAX_PASSTHROUGH_FAILURES * 5) {
            mgr.record_passthrough_result(1, crate::usage::RequestOutcome::ServerError);
            mgr.record_passthrough_result(1, crate::usage::RequestOutcome::NetworkError);
        }

        assert_eq!(
            mgr.available_count(),
            1,
            "5xx/网络错误是瞬态信号，不得触发自动禁用（否则上游抖动会清空代挂池）"
        );
    }

    #[test]
    fn test_select_custom_api_skips_cooldown() {
        // failover 给失败号设了冷却后,select_custom_api 应跳过冷却中的号。
        use std::collections::HashSet;
        let config = Config::default();
        let mut c1 = KiroCredentials::default();
        c1.id = Some(1);
        c1.auth_method = Some("custom_api".to_string());
        c1.base_url = Some("https://relay1.example.invalid".to_string());
        c1.priority = 0;
        let mut c2 = KiroCredentials::default();
        c2.id = Some(2);
        c2.auth_method = Some("custom_api".to_string());
        c2.base_url = Some("https://relay2.example.invalid".to_string());
        c2.priority = 0;
        let mgr = MultiTokenManager::new(config, vec![c1, c2], None, None, false).unwrap();

        // 给 #1 设冷却(模拟它 403 认证失败被 failover 冷却)→ 选号应跳过 #1 选 #2。
        mgr.cooldown_custom_api(
            1,
            180,
            crate::kiro::cooldown::CooldownReason::AuthTransient,
        );
        let empty = HashSet::new();
        let sel = mgr.select_custom_api(&empty, None).expect("应选到未冷却的 #2");
        assert_eq!(sel.0, 2, "#1 冷却中,应选 #2");
    }

    /// S4：面板冷却标签 —— `cooldown_custom_api` 写入的 reason 必须原样出现在
    /// 面板展示链路（`cooldown_snapshot` → `CooldownInfo.reason` → description/code，
    /// admin 侧 `CredentialStatusItem.cooldown_reason/cooldown_code` 即这两者）。
    ///
    /// 回退即 FAIL：`cooldown_custom_api` 硬编码 `RateLimitExceeded`（S4 前的缺陷，
    /// 401/403 在面板显示「速率限制」误导排障）→ 原因/标签断言失败。
    #[test]
    fn test_passthrough_cooldown_label_reflects_reason() {
        use crate::kiro::cooldown::CooldownReason;
        let config = Config::default();
        let mut c1 = KiroCredentials::default();
        c1.id = Some(1);
        c1.auth_method = Some("custom_api".to_string());
        c1.base_url = Some("https://relay1.example.invalid".to_string());
        c1.priority = 0;
        let mgr = MultiTokenManager::new(config, vec![c1], None, None, false).unwrap();

        // 401/403 类：AuthTransient —— 面板应显示「认证瞬态失败」(cooldownCode=auth_transient)。
        mgr.cooldown_custom_api(1, 180, CooldownReason::AuthTransient);
        let snap = mgr.cooldown_snapshot();
        let info = snap
            .iter()
            .find(|c| c.credential_id == 1)
            .expect("冷却后快照中应有 #1");
        assert_eq!(info.reason, CooldownReason::AuthTransient, "reason 必须原样保存");
        assert_eq!(info.reason.description(), "认证瞬态失败", "面板 cooldownReason 文案");
        assert_eq!(info.reason.code(), "auth_transient", "面板 cooldownCode（前端 i18n 键）");

        // 5xx 类：ServerError —— 面板显示「服务器错误」。
        mgr.cooldown_custom_api(1, 5, CooldownReason::ServerError);
        let info = mgr
            .cooldown_snapshot()
            .into_iter()
            .find(|c| c.credential_id == 1)
            .expect("冷却后快照中应有 #1");
        assert_eq!(info.reason, CooldownReason::ServerError);
        assert_eq!(info.reason.description(), "服务器错误");
        assert_eq!(info.reason.code(), "server_error");
    }

    /// 🔴 N1 回归（2026-08-16）：透传池死号（恒 502）失败后不得再被恒选——
    /// **即使 `cooldownEnabled=false`**（线上现状，整条冷却体系被门控）。
    ///
    /// 线上实测根因链：#3 cursorapi 恒 502 却每请求都被第一个选中（低负载时 RPM
    /// 滑窗归零，排序键前 5 键全平局，min_by_key 恒选 Vec 头部 = 死号），白打一跳
    /// 才 failover 到健康号 #2。透传池 5xx 不冷却（provider.rs `_ => 0` 分支）+
    /// cooldownEnabled=false 门控 ⇒ 没有任何跨请求失败记忆。
    ///
    /// 本测试复现线上形态：两号同 priority、RPM 记账被人工拉平（每轮给未选中的号
    /// 补记一次，模拟「请求间隔 >60s、RPM 恒为 0」的低负载全平局），排序键只剩
    /// failure_recency 一个区分维度。
    ///
    /// 回退即 FAIL：删掉排序键的 failure_recency 位 → 每轮全平局恒选 #1，
    /// `dead_picks <= 1` 断言失败。
    #[test]
    fn test_passthrough_dead_credential_not_re_picked_with_cooldown_disabled() {
        use std::collections::HashSet;
        let mut config = Config::default();
        config.cooldown_enabled = false; // ⭐ 复现线上配置（冷却体系整体被门控）
        let mut c1 = KiroCredentials::default();
        c1.id = Some(1);
        c1.auth_method = Some("custom_api".to_string());
        c1.base_url = Some("https://relay1.example.invalid".to_string());
        c1.priority = 0;
        let mut c2 = KiroCredentials::default();
        c2.id = Some(2);
        c2.auth_method = Some("custom_api".to_string());
        c2.base_url = Some("https://relay2.example.invalid".to_string());
        c2.priority = 0;
        let mgr = MultiTokenManager::new(config, vec![c1, c2], None, None, false).unwrap();

        let empty = HashSet::new();
        let mut dead_picks = 0u32;
        for round in 0..10u32 {
            let sel = mgr.select_custom_api(&empty, None).expect("池内必有候选");
            let chosen = sel.0;
            drop(sel.2); // 释放 inflight，排除排序键 inflight 维度的干扰
            if chosen == 1 {
                // 死号被选中 = 白打一跳：记失败时刻（模拟 502 后的 failover 标记）。
                dead_picks += 1;
                mgr.mark_passthrough_failure(1);
                mgr.rpm.record(2); // 拉平 RPM（见测试文档：模拟低负载全平局）
            } else {
                mgr.rpm.record(1); // 拉平 RPM
            }
            assert_eq!(
                mgr.rpm.count(1),
                mgr.rpm.count(2),
                "第 {round} 轮：RPM 必须拉平，否则排序键的 rpm 位先分高下，\
                 测不到 failure_recency 的贡献"
            );
        }
        assert!(
            dead_picks <= 1,
            "死号 #1 只应被白打首轮一跳（失败余温降权后恒选健康号 #2），\
             实际 {dead_picks} 次——cooldownEnabled=false 下也必须换号"
        );
    }

    /// 🔴 N1 回归（2026-08-16）：失败余温窗口过期后，死号恢复平权（可被重新选中
    /// 探测复活），瞬态抖动的号不因一次失败被永久压住。
    ///
    /// 构造与上一条相同（两号 RPM 拉平、全平局），手动把 #1 的 last_failure_at
    /// 回拨到窗口外（`PASSTHROUGH_FAILURE_DECAY_SECS + 1s` 前）模拟「61 秒前的
    /// 一次失败」——不 sleep 真实时间（CI 友好，直接改写进程内时间戳）。
    ///
    /// 回退即 FAIL：把余温判断改成「永不恢复」（如窗口取无限大）→ 回拨后仍选 #2，
    /// 本条「应选回 #1」断言失败。
    #[test]
    fn test_passthrough_failure_warmth_expires_after_window() {
        use std::collections::HashSet;
        let mut config = Config::default();
        config.cooldown_enabled = false;
        let mut c1 = KiroCredentials::default();
        c1.id = Some(1);
        c1.auth_method = Some("custom_api".to_string());
        c1.base_url = Some("https://relay1.example.invalid".to_string());
        c1.priority = 0;
        let mut c2 = KiroCredentials::default();
        c2.id = Some(2);
        c2.auth_method = Some("custom_api".to_string());
        c2.base_url = Some("https://relay2.example.invalid".to_string());
        c2.priority = 0;
        let mgr = MultiTokenManager::new(config, vec![c1, c2], None, None, false).unwrap();

        // 预置：两号 RPM 各记一次拉平（模拟低负载下 RPM 恒 0 的全平局形态）。
        mgr.rpm.record(1);
        mgr.rpm.record(2);
        let empty = HashSet::new();

        // 窗口内的失败 → 死号被降权，选健康号 #2。
        mgr.mark_passthrough_failure(1);
        let sel = mgr.select_custom_api(&empty, None).expect("池内必有候选");
        assert_eq!(sel.0, 2, "余温窗口内：死号 #1 必须被降权，选健康号 #2");
        drop(sel.2);

        // 手动回拨 #1 的失败时刻到窗口外（模拟 61s 流逝）→ 余温过期恢复平权。
        {
            let mut entries = mgr.entries.lock();
            let e = entries
                .iter_mut()
                .find(|e| e.id == 1)
                .expect("构造的 #1 必须存在");
            e.last_failure_at.set(Some(
                Instant::now() - StdDuration::from_secs(PASSTHROUGH_FAILURE_DECAY_SECS + 1),
            ));
        }
        // 上一次 select 选中 #2 时给它记了 rpm → 重新拉平（#1 补记一次），
        // 否则 rpm 位先分高下，测不到 failure_recency 过期的贡献。
        mgr.rpm.record(1);
        let sel2 = mgr.select_custom_api(&empty, None).expect("池内必有候选");
        assert_eq!(
            sel2.0, 1,
            "余温过期后 #1 恢复平权：全平局下应重新按 Vec 序选中（探测复活，\
             瞬态抖动不误杀）"
        );
    }

    /// 🔴 M1.1 回归（2026-08-16 对抗审查 MAJOR）：透传**成功立即清失败余温**——
    /// 上游 30s 恢复后该号不该仍被排除到 60s 窗口尾（此前成功不清热，恢复的号白等）。
    ///
    /// 构造与余温测试相同（两号 RPM 拉平、全平局）：mark → #1 被排除；给它记一次
    /// Success → **无需回拨时钟**、无需等 60s，下一轮 select 立即回到全平局 →
    /// 按 id 选中 #1。若成功分支不清热，#1 仍带余温被过滤 → 选 #2 → 断言红。
    ///
    /// 回退即 FAIL：删掉 `record_passthrough_result` Success 分支的
    /// `last_failure_at.set(None)` → 此断言选回 #2。
    #[test]
    fn test_passthrough_success_clears_failure_warmth_immediately() {
        use std::collections::HashSet;
        let mut config = Config::default();
        config.cooldown_enabled = false;
        let mut c1 = KiroCredentials::default();
        c1.id = Some(1);
        c1.auth_method = Some("custom_api".to_string());
        c1.base_url = Some("https://relay1.example.invalid".to_string());
        c1.priority = 0;
        let mut c2 = KiroCredentials::default();
        c2.id = Some(2);
        c2.auth_method = Some("custom_api".to_string());
        c2.base_url = Some("https://relay2.example.invalid".to_string());
        c2.priority = 0;
        let mgr = MultiTokenManager::new(config, vec![c1, c2], None, None, false).unwrap();

        mgr.rpm.record(1);
        mgr.rpm.record(2);
        let empty = HashSet::new();

        // 失败余温窗口内：#1 被过滤，选健康号 #2。
        mgr.mark_passthrough_failure(1);
        let sel = mgr.select_custom_api(&empty, None).expect("池内必有候选");
        assert_eq!(sel.0, 2, "余温窗口内：#1 必须被过滤（失败后不立即回选）");
        drop(sel.2);

        // 上游恢复：#1 成功 → 余温立清（选中 #2 时给它记过 rpm，这里补记拉平）。
        mgr.record_passthrough_result(1, crate::usage::RequestOutcome::Success);
        mgr.rpm.record(1);
        let sel2 = mgr.select_custom_api(&empty, None).expect("池内必有候选");
        assert_eq!(
            sel2.0, 1,
            "成功清热后 #1 立即可选（无需等 60s 余温过期）：全平局下按 id 恢复平权"
        );
    }

    /// 🔴 M1.3 回归（2026-08-16 对抗审查 MAJOR）：全池余温逃生舱——**所有**候选都带
    /// 余温（系统性抖动：上游整体压限/集体短暂故障）时不再「无候选 → 503」，
    /// 按**最老余温**号（失败最早 = 最接近恢复）硬试一次。对照 Kiro 主路径
    /// `select_ignoring_cooldown` 兜底先例：拿真实上游错误好过网关自造 503。
    ///
    /// mark(1) 先于 mark(2) → #1 的失败时刻更早（Instant 更小）→ 逃生舱选中 #1。
    /// 回退即 FAIL：删掉 `select_custom_api_inner` 的逃生舱分支 → 全余温无候选 →
    /// select 返 None → `expect` 直接 panic。
    #[test]
    fn test_passthrough_all_warm_escape_hatch_tries_oldest_warmth() {
        use std::collections::HashSet;
        let mut config = Config::default();
        config.cooldown_enabled = false;
        let mut c1 = KiroCredentials::default();
        c1.id = Some(1);
        c1.auth_method = Some("custom_api".to_string());
        c1.base_url = Some("https://relay1.example.invalid".to_string());
        c1.priority = 0;
        let mut c2 = KiroCredentials::default();
        c2.id = Some(2);
        c2.auth_method = Some("custom_api".to_string());
        c2.base_url = Some("https://relay2.example.invalid".to_string());
        c2.priority = 0;
        let mgr = MultiTokenManager::new(config, vec![c1, c2], None, None, false).unwrap();

        // 两号都失败：#1 先于 #2 → #1 余温更老（最接近恢复）。
        mgr.mark_passthrough_failure(1);
        mgr.mark_passthrough_failure(2);

        let empty = HashSet::new();
        let sel = mgr
            .select_custom_api(&empty, None)
            .expect("全池余温必须由逃生舱兜底硬试一次，不得返 None（=503）");
        assert_eq!(
            sel.0, 1,
            "逃生舱必须选**最老余温**号（#1 失败时刻早于 #2，最接近恢复）"
        );
    }

    /// 模型黑名单（2026-08-14 根治）：上游说「不支持该模型」后，选号跳过该号×该模型；
    /// 但该号对别的模型仍可服务（粒度是号×模型，不做号级冷却）。
    #[test]
    fn test_model_blacklist_skips_unsupported_pair_only() {
        let mut c1 = KiroCredentials::default();
        c1.id = Some(1);
        c1.auth_method = Some("custom_api".to_string());
        c1.base_url = Some("https://relay1.example.invalid".to_string());
        c1.priority = 0;
        let mut c2 = KiroCredentials::default();
        c2.id = Some(2);
        c2.auth_method = Some("custom_api".to_string());
        c2.base_url = Some("https://relay2.example.invalid".to_string());
        c2.priority = 0;
        let mgr = MultiTokenManager::new(Config::default(), vec![c1, c2], None, None, false).unwrap();

        // #1 声明不支持 claude-opus-5 → 记黑名单。
        mgr.mark_model_unsupported(1, "claude-opus-5");
        assert!(mgr.is_model_blacklisted(1, "claude-opus-5"), "应命中黑名单");

        // 请求 claude-opus-5 → 跳过 #1 选 #2。
        let empty = HashSet::new();
        let sel = mgr
            .select_custom_api(&empty, Some("claude-opus-5"))
            .expect("应有候选");
        assert_eq!(sel.0, 2, "黑名单内的号×模型组合不得被选");

        // 请求别的模型 → #1 仍可服务（粒度是号×模型）。
        let sel2 = mgr
            .select_custom_api(&empty, Some("gpt-5.6-sol"))
            .expect("应有候选");
        assert_eq!(sel2.0, 1, "#1 对未黑名单的模型仍可被选（不得号级连坐）");

        // 空模型不参与黑名单。
        let sel3 = mgr.select_custom_api(&empty, None).expect("应有候选");
        assert_eq!(sel3.0, 1, "无模型语义的请求不受黑名单影响");
    }

    /// #9 合并守卫：两池**共用一张** `model_blocklist` 表。
    ///
    /// Kiro 写口（`report_model_invalid`）与 custom_api 写口（`mark_model_unsupported`）
    /// 写同一张表，任一查询口（`is_model_blacklisted` / `is_model_blocked`）都可见。
    /// 合并前两张表并存、改一张漏一张编译不报错（#9 结构绊脚石）——本测试钉死同表，
    /// 未来若再分叉成两张表，此断言红。
    #[test]
    fn model_blacklist_is_one_shared_table_across_pools() {
        let mgr =
            MultiTokenManager::new(Config::default(), vec![], None, None, false).unwrap();

        // custom_api 写口写入 → Kiro 查询口必须可见（同表才可能）。
        mgr.mark_model_unsupported(1, "claude-opus-5");
        assert!(
            mgr.is_model_blocked(1, "claude-opus-5"),
            "custom_api 写口写入后 Kiro 查询口必须可见——两表分叉时此断言红"
        );

        // Kiro 写口写入 → custom_api 查询口必须可见（同表才可能）。
        mgr.report_model_invalid(2, Some("claude-sonnet-4-5"));
        assert!(
            mgr.is_model_blacklisted(2, "claude-sonnet-4-5"),
            "Kiro 写口写入后 custom_api 查询口必须可见——两表分叉时此断言红"
        );

        // 各池查询口互不串扰（号级隔离仍成立）：#1 的条目不影响 #2 的判定，反之亦然。
        assert!(
            !mgr.is_model_blocked(2, "claude-opus-5"),
            "#2 未被加黑 claude-opus-5，Kiro 查询口不得串到 #1 的条目"
        );
        assert!(
            !mgr.is_model_blacklisted(1, "claude-sonnet-4-5"),
            "#1 未被加黑 claude-sonnet-4-5，custom_api 查询口不得串到 #2 的条目"
        );
    }

    /// #9 合并守卫：删号清理**一次覆盖两池**的条目。
    ///
    /// 合并前 `model_blacklist`（custom_api 表）在 `delete_credential` 里**不被清理**——
    /// 从回收站 restore 同 id 的号会背着残留黑名单被静默跳过。合并后一张表
    /// 一处 `retain` 全清（`delete_credential_forced` 的清理块）。
    #[test]
    fn model_blacklist_entries_purged_on_delete_for_both_pools() {
        let dir = std::env::temp_dir().join(format!(
            "kiro-modelbl-purge-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let cred_path = dir.join("credentials.json");

        let c = mk_custom(1, 0, None);
        let mgr = MultiTokenManager::new(
            Config::default(),
            vec![c],
            None,
            Some(cred_path.clone()),
            true,
        )
        .unwrap();

        mgr.mark_model_unsupported(1, "claude-opus-5");
        assert!(mgr.is_model_blacklisted(1, "claude-opus-5"), "先加黑");

        mgr.delete_credential_forced(1, true).unwrap();
        assert!(
            !mgr.is_model_blacklisted(1, "claude-opus-5"),
            "删号后 custom_api 黑名单条目必须被清（合并前此断言红：该表不在删号清理内）"
        );
        assert!(
            !mgr.is_model_blocked(1, "claude-opus-5"),
            "同一张表，Kiro 查询口同样必须看不到残留"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// 造一个 custom_api 代挂号（priority 可指定，可选凭据级 custom_api_first 覆盖）。
    fn mk_custom(id: u64, priority: u32, first: Option<bool>) -> KiroCredentials {
        let mut c = KiroCredentials::default();
        c.id = Some(id);
        c.auth_method = Some("custom_api".to_string());
        c.base_url = Some(format!("https://relay{id}.example.invalid"));
        c.api_key = Some(format!("sk-relay-{id}"));
        c.priority = priority;
        c.custom_api_first = first;
        c
    }

    /// 造一个 Kiro（api_key 型）号，priority 可指定。
    fn mk_kiro(id: u64, priority: u32) -> KiroCredentials {
        let mut c = KiroCredentials::default();
        c.id = Some(id);
        c.auth_method = Some("api_key".to_string());
        c.kiro_api_key = Some(format!("sk-kiro-{id}"));
        c.priority = priority;
        c
    }

    /// 🔴 2026-08-16 重写（deepseek 归一化移除后）：透传池选号白名单按**原始模型名**
    /// 判定（`allows_model`，支持通配符）——请求不再被改写，客户端原始名就是实际
    /// 发给上游的名，判定键 = 发送键。此前（1439 时代）白名单按改写后名（fallback
    /// 预判）判定，那套口径随归一化一并消亡。
    #[test]
    fn select_custom_api_whitelist_uses_original_model() {
        use std::collections::HashSet;
        let empty = HashSet::new();

        // 场景 A：白名单 [deepseek-v4-flash] + 请求 claude-sonnet-4-5 → 原始名不在
        // 白名单 → 必须被过滤（唯一号 → 无候选）。若此处被放行 = 白名单硬门失效。
        let mut a1 = mk_custom(1, 0, None);
        a1.allowed_models = Some(vec!["deepseek-v4-flash".to_string()]);
        let mgr_a =
            MultiTokenManager::new(Config::default(), vec![a1], None, None, false).unwrap();
        assert!(
            mgr_a
                .select_custom_api(&empty, Some("claude-sonnet-4-5"))
                .is_none(),
            "原始名不在白名单必须过滤"
        );

        // 场景 B：白名单 [claude-sonnet-4-5]（含原始名）→ 命中 → 选中。
        let mut b1 = mk_custom(1, 0, None);
        b1.allowed_models = Some(vec!["claude-sonnet-4-5".to_string()]);
        let mgr_b =
            MultiTokenManager::new(Config::default(), vec![b1], None, None, false).unwrap();
        let sel_b = mgr_b
            .select_custom_api(&empty, Some("claude-sonnet-4-5"))
            .expect("白名单含原始名必须选中");
        assert_eq!(sel_b.0, 1, "白名单按原始名命中");

        // 场景 C：通配符白名单 [claude-*] → claude-opus-5 命中通配 → 选中
        //（与场景 A 形成对照：判定键是客户端原始名，通配符直接作用于它）。
        let mut c1 = mk_custom(1, 0, None);
        c1.allowed_models = Some(vec!["claude-*".to_string()]);
        let mgr_c =
            MultiTokenManager::new(Config::default(), vec![c1], None, None, false).unwrap();
        let sel_c = mgr_c
            .select_custom_api(&empty, Some("claude-opus-5"))
            .expect("通配 claude-* 白名单命中原始名");
        assert_eq!(sel_c.0, 1, "通配白名单按原始名命中");
    }

    /// 模型黑名单键一致性：mark 与 check 都用**客户端原始模型名**
    /// （provider.rs 埋点传选号入参 model，filter 用同一入参）——自洽，只漏不误伤。
    ///
    /// 现状钉住：上游拒绝该原始模型名（model_not_found）时，黑名单记原始名 →
    /// 不同原始名**不被**黑名单覆盖（漏判，宁可多打一跳也不误伤健康号）。
    /// （2026-08-16：deepseek 归一化移除后无「改写后名」概念，键就是发送名。）
    #[test]
    fn blacklist_key_with_mapping_scenario() {
        use std::collections::HashSet;
        let empty = HashSet::new();

        // 候选需过白名单：allowed_models 同时列两个模型（黑名单挡一个不误伤另一个）。
        let mut c = mk_custom(1, 0, None);
        c.allowed_models = Some(vec![
            "claude-opus-5".to_string(),
            "claude-sonnet-4-5".to_string(),
        ]);
        let mgr = MultiTokenManager::new(Config::default(), vec![c], None, None, false).unwrap();

        // mark 用**原始名**（与 provider.rs 埋点一致）。
        mgr.mark_model_unsupported(1, "claude-opus-5");
        assert!(
            mgr.is_model_blacklisted(1, "claude-opus-5"),
            "mark/check 同键自洽：记原始名、判原始名"
        );

        // 同原始名 → 该号×该模型组合被挡（唯一号 → 无候选）。
        assert!(
            mgr.select_custom_api(&empty, Some("claude-opus-5")).is_none(),
            "黑名单内的原始名必须被过滤"
        );

        // 不同原始名 → 不命中（漏判现状钉住，测试头注释说明 intended）。
        let sel = mgr
            .select_custom_api(&empty, Some("claude-sonnet-4-5"))
            .expect("不同原始名不应被黑名单误伤（只漏不误伤）");
        assert_eq!(sel.0, 1);
    }

    /// 黑名单边界：空模型短路（mark 直接 return、check 直接 false，都不落键）；
    /// 键大小写敏感现状钉住（HashMap 精确匹配，与白名单的 eq_ignore_ascii_case 不同）。
    #[test]
    fn blacklist_empty_and_case_sensitivity() {
        use std::collections::HashSet;
        let empty = HashSet::new();
        let mgr = MultiTokenManager::new(
            Config::default(),
            vec![mk_custom(1, 0, None)],
            None,
            None,
            false,
        )
        .unwrap();

        // 空模型短路。
        mgr.mark_model_unsupported(1, "");
        assert!(
            !mgr.is_model_blacklisted(1, ""),
            "空模型不参与黑名单（mark 短路 + check 短路）"
        );
        assert!(
            mgr.select_custom_api(&empty, Some("")).is_some(),
            "空模型请求不受黑名单影响"
        );

        // 大小写不互命中（HashMap 精确匹配，现状钉住）。
        mgr.mark_model_unsupported(1, "claude-opus-5");
        assert!(mgr.is_model_blacklisted(1, "claude-opus-5"));
        assert!(
            !mgr.is_model_blacklisted(1, "Claude-Opus-5"),
            "黑名单键大小写敏感（与白名单 eq_ignore_ascii_case 不同，现状钉住）"
        );
    }

    #[test]
    fn test_cross_pool_priority_kiro_wins_when_lower() {
        // ⭐用户实测反馈的核心场景:"优先级设置了 kiro 的 apikey 更小,还是会优先调度上游的 apikey"。
        // 历史行为把「custom_api 优先」写死在分派顺序里(handlers 一进来就先试透传),而
        // select_custom_api 只在代挂号**子集内**比 priority → 跨池优先级从未被比较过。
        // 修复后:默认(custom_api_first=false)按 priority 全局公平比较,Kiro 更优则不先走透传。
        let config = Config::default();
        assert!(
            !config.custom_api_first,
            "全局默认必须是 false(priority 全局统一比较),否则又回到代挂号绝对优先"
        );
        // kiro priority=0 优于 relay priority=5
        let mgr = MultiTokenManager::new(
            config,
            vec![mk_custom(1, 5, None), mk_kiro(2, 0)],
            None,
            None,
            false,
        )
        .unwrap();
        assert!(
            !mgr.should_try_custom_api_first(),
            "Kiro 号 priority(0) 更小时应先走 Kiro,不该先试代挂透传"
        );
    }

    #[test]
    fn test_cross_pool_priority_custom_wins_when_lower_or_equal() {
        let config = Config::default();
        // 代挂 priority=0 优于 kiro priority=5 → 先走透传
        let mgr = MultiTokenManager::new(
            config.clone(),
            vec![mk_custom(1, 0, None), mk_kiro(2, 5)],
            None,
            None,
            false,
        )
        .unwrap();
        assert!(
            mgr.should_try_custom_api_first(),
            "代挂 priority 更小时应先走透传"
        );

        // priority 相同 → 维持"代挂在前"的既有习惯(用 <= 而非 <),避免纯升级场景行为突变
        let mgr2 = MultiTokenManager::new(
            config,
            vec![mk_custom(1, 3, None), mk_kiro(2, 3)],
            None,
            None,
            false,
        )
        .unwrap();
        assert!(
            mgr2.should_try_custom_api_first(),
            "priority 相同时保持代挂在前(兼容既有部署)"
        );
    }

    #[test]
    fn test_cross_pool_per_credential_override_wins() {
        // 凭据级 custom_api_first=Some(true) 必须覆盖全局 false:
        // 即便该代挂号 priority 明显更差,它也要求无条件优先。
        // 这是用户要的"每个上游账号 apikey 都可以自定义"。
        let config = Config::default();
        let mgr = MultiTokenManager::new(
            config,
            vec![mk_custom(1, 99, Some(true)), mk_kiro(2, 0)],
            None,
            None,
            false,
        )
        .unwrap();
        assert!(
            mgr.should_try_custom_api_first(),
            "凭据级 custom_api_first=true 必须覆盖全局,无条件优先"
        );
    }

    #[test]
    fn test_cross_pool_global_switch_restores_legacy_behavior() {
        // 全局 custom_api_first=true 恢复历史行为:代挂号无条件优先(供"就是要中转兜底在前"的部署)。
        let mut config = Config::default();
        config.custom_api_first = true;
        let mgr = MultiTokenManager::new(
            config,
            vec![mk_custom(1, 99, None), mk_kiro(2, 0)],
            None,
            None,
            false,
        )
        .unwrap();
        assert!(
            mgr.should_try_custom_api_first(),
            "全局开关为 true 时应恢复代挂号绝对优先的历史行为"
        );
    }

    #[test]
    fn test_cross_pool_per_credential_false_overrides_global_true() {
        // 反向覆盖也必须生效:全局 true,但该号显式 false → 参与公平比较。
        let mut config = Config::default();
        config.custom_api_first = true;
        let mgr = MultiTokenManager::new(
            config,
            vec![mk_custom(1, 99, Some(false)), mk_kiro(2, 0)],
            None,
            None,
            false,
        )
        .unwrap();
        assert!(
            !mgr.should_try_custom_api_first(),
            "凭据级显式 false 必须覆盖全局 true,回到 priority 公平比较"
        );
    }

    #[test]
    fn test_cross_pool_edge_cases() {
        let config = Config::default();

        // 无代挂号 → 不必尝试透传（也省掉一次无谓的 select_custom_api）
        let only_kiro =
            MultiTokenManager::new(config.clone(), vec![mk_kiro(1, 0)], None, None, false).unwrap();
        assert!(
            !only_kiro.should_try_custom_api_first(),
            "池中无代挂号时不该尝试透传"
        );

        // 只有代挂号 → 只能走透传（不管 priority 多大）
        let only_custom = MultiTokenManager::new(
            config.clone(),
            vec![mk_custom(1, 999, None)],
            None,
            None,
            false,
        )
        .unwrap();
        assert!(
            only_custom.should_try_custom_api_first(),
            "只有代挂号时必须走透传"
        );

        // 空池 → 不尝试
        let empty = MultiTokenManager::new(config.clone(), vec![], None, None, false).unwrap();
        assert!(!empty.should_try_custom_api_first(), "空池不该尝试透传");

        // 被禁用的号不参与仲裁：唯一的代挂号被禁用 → 视为无代挂号
        let mgr = MultiTokenManager::new(
            config,
            vec![mk_custom(1, 0, None), mk_kiro(2, 5)],
            None,
            None,
            false,
        )
        .unwrap();
        assert!(
            mgr.should_try_custom_api_first(),
            "禁用前:代挂 priority 更小 → 走透传"
        );
        mgr.set_disabled(1, true).unwrap();
        assert!(
            !mgr.should_try_custom_api_first(),
            "代挂号被禁用后应视为无代挂号,不再尝试透传"
        );
    }

    #[test]
    fn test_cross_pool_cooldown_excluded_from_arbitration() {
        // 冷却中的代挂号此刻选不出来,不该影响路径决策:
        // 唯一的(且 priority 更优的)代挂号在冷却中 → 应先走 Kiro,而不是先试一次必然失败的透传。
        let config = Config::default();
        let mgr = MultiTokenManager::new(
            config,
            vec![mk_custom(1, 0, None), mk_kiro(2, 5)],
            None,
            None,
            false,
        )
        .unwrap();
        assert!(mgr.should_try_custom_api_first(), "冷却前应走透传");
        mgr.cooldown_custom_api(1, 300, crate::kiro::cooldown::CooldownReason::RateLimitExceeded);
        assert!(
            !mgr.should_try_custom_api_first(),
            "唯一代挂号在冷却中时应先走 Kiro(避免白试一次注定失败的透传)"
        );
    }

    /// 🔴 纯代挂池 + 上游持续失败 ⇒ **必须最终给出终态**（2026-08-10 对抗评审抓出的缺陷）。
    ///
    /// 缺陷成因：纯 custom_api 池下 `available` 恒 0（选号已排除代挂号），而
    /// `any_healable` 恒 true（只要有未禁用代挂号就算"等一会儿会好"）。又因为
    /// 「透传失败绝不 auto-disable 号」是有实测依据的刻意设计（健康号 #216 曾被误禁 119 次），
    /// 号永远不会变成 disabled ⇒ 那条 429 永远带 `retry_after_secs=` 而不带
    /// `pool_permanently_exhausted=1` ⇒ 吸收层一直吸收、客户端**无限重试**、拿不到终态。
    ///
    /// 修复：进程级 `consecutive_pool_unavailable` 计数，达阈值后升级为
    /// `pool_permanently_exhausted=1`（吸收层据此停止吸收）；成功选号即清零。
    #[tokio::test]
    async fn pool_unavailable_escalates_to_permanent_after_repeats() {
        let config = Config::default();
        let mut c = KiroCredentials::default();
        c.id = Some(1);
        c.auth_method = Some("custom_api".to_string());
        c.base_url = Some("https://relay.example.invalid".to_string());
        let mgr = MultiTokenManager::new(config, vec![c], None, None, false).unwrap();

        // 反复走「Kiro 路径无可用凭据」这条 bail。阈值是 20，多跑几次确保跨过。
        let mut saw_retryable = false;
        let mut saw_permanent = false;
        for i in 1..=25 {
            let e = tokio::time::timeout(
                std::time::Duration::from_secs(3),
                mgr.acquire_context(Some("claude-sonnet-4.5"), None),
            )
            .await
            .expect("不该挂死");
            // CallContext 未实现 Debug，不能用 expect_err（那要求 Ok 侧可打印）
            let e = match e {
                Err(e) => e,
                Ok(_) => panic!("纯代挂池下 Kiro 主路径必须失败，却成功选到号"),
            };
            let msg = e.to_string();
            if msg.contains("pool_permanently_exhausted=1") {
                saw_permanent = true;
                assert!(
                    i >= 20,
                    "第 {i} 次就升级为永久态，早于阈值 20 —— 会把中转站的瞬时抖动误判为永久故障"
                );
                break;
            }
            // 阈值之前必须仍是**可重试**语义（带 Retry-After、不带永久标记）
            assert!(
                msg.contains("retry_after_secs="),
                "第 {i} 次的错误必须带 retry_after_secs= 才能让客户端正确退避: {msg}"
            );
            saw_retryable = true;
        }
        assert!(saw_retryable, "阈值之前应当先出现可重试的 429");
        assert!(
            saw_permanent,
            "连续 25 轮全池不可用后仍未升级为 pool_permanently_exhausted=1 ⇒ \
             客户端会无限重试、永远拿不到终态（这正是本测试要防的回归）"
        );

        // 成功选号必须清零：调 select_custom_api（与 Kiro 主路径 commit_selection
        // 同为清零点，任一路径成功都证明池子可用）。
        // 该号没有冷却，所以能被选中。
        let picked = mgr.select_custom_api(&std::collections::HashSet::new(), None);
        assert!(picked.is_some(), "该代挂号未被禁用也未冷却，应能选中");
        drop(picked);
        let after = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            mgr.acquire_context(Some("claude-sonnet-4.5"), None),
        )
        .await
        .expect("不该挂死");
        let after = match after {
            Err(e) => e,
            Ok(_) => panic!("仍无 Kiro 号，不该成功"),
        };
        assert!(
            !after.to_string().contains("pool_permanently_exhausted=1"),
            "成功选到代挂号后计数必须清零，否则中转站恢复了客户端却仍被判永久故障: {after}"
        );
    }

    #[tokio::test]
    async fn test_acquire_context_no_busy_loop_with_only_custom_api() {
        // ⭐⭐ CPU 死循环回归(旧代码会挂死+烧满一核,本测试靠超时兜底):
        //
        // is_entry_selectable 会过滤 custom_api 号(两池隔离铁律:Kiro 路径永不碰代挂号),
        // 但 transient_wait_outcome 旧代码**漏了这道过滤**。于是当池中只有 custom_api 号时:
        //   - select_next_credential → None(全被 is_entry_selectable 过滤)
        //   - transient_wait_outcome → Available(它看不出这些号不可选,判为"立即可用")
        //   - acquire_context 的 `WaitOutcome::Available => continue` 既不 sleep 也不递增
        //     attempt_count(该分支语义是"竞态,立刻重选")
        //   → 循环顶部的 attempt_count >= max_attempts 永远不成立 → **无退出条件的忙等热循环**,
        //     请求永不返回且烧满一个 CPU 核。
        //
        // 真实触发路径:try_custom_api_passthrough 在 custom_api 全部冷却后返回 None,
        // 随即回落 Kiro 主路径调 acquire_context;以及 MCP/WebSearch 等不走透传的调用。
        //
        // 修复后应**快速返回 Err**(NoCandidate → "所有凭据均已禁用"类错误),而不是挂死。
        let config = Config::default();
        let mut c = KiroCredentials::default();
        c.id = Some(1);
        c.auth_method = Some("custom_api".to_string());
        c.base_url = Some("https://relay.example.invalid".to_string());
        let mgr = MultiTokenManager::new(config, vec![c], None, None, false).unwrap();

        let r = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            mgr.acquire_context(Some("claude-sonnet-4.5"), None),
        )
        .await;

        match r {
            Err(_) => panic!(
                "⭐acquire_context 在【池中只有 custom_api 号】时挂死(忙等热循环)。\
                 说明 transient_wait_outcome 与 is_entry_selectable 的硬门条件不对齐。"
            ),
            Ok(Ok(_)) => panic!("custom_api 号绝不该被 Kiro 主路径选中(两池隔离铁律被破坏)"),
            Ok(Err(e)) => {
                // 预期:快速失败。错误应指向无可用凭据,而非竞态收敛失败。
                let msg = e.to_string();
                assert!(
                    !msg.contains("竞态无法收敛"),
                    "不该退化到竞态兜底上限才结束,说明 transient_wait_outcome 仍未正确过滤: {msg}"
                );
                // 🔴 **错误分类断言**（2026-08-10 补，旧代码必 FAIL）。
                //
                // 本测试走的场景（池里只有 custom_api 号）**正是** `available` 口径 bug 的
                // 触发条件，但改前它只断言「不是竞态收敛失败」、**完全不检查错误分类** ⇒
                // 那个致命 bug 从这张网底下溜了过去。
                //
                // 旧代码：`available = 1`（把代挂号算进去）≠ 0 ⇒ 跳过真耗尽分支 ⇒ 落
                // `model_unsupported_by_pool=1` ⇒ handlers 映射 404 无 Retry-After ⇒ 断会话。
                // 修好后：`available == 0` ⇒ 带 `retry_after_secs=` ⇒ 429 + Retry-After ⇒
                // 客户端退避重试（人工补号后确实会好）。
                assert!(
                    msg.contains("retry_after_secs="),
                    "纯 custom_api 池必须报**可重试**的池耗尽（带 retry_after_secs= → 429），\
                     否则客户端拿到 404 会当场断会话。实际: {msg}"
                );
                assert!(
                    !msg.contains("model_unsupported_by_pool=1"),
                    "纯 custom_api 池**不是**「模型不被号池支持」——那个标记会被映射成 404 且\
                     刻意无 Retry-After（永久态语义）。真实原因是「池里没有任何 Kiro 号」，\
                     两回事。若命中此断言，说明 `available` 又把 custom_api 号算进去了。实际: {msg}"
                );
            }
        }
    }

    #[tokio::test]
    async fn test_acquire_context_no_busy_loop_when_model_blocked_for_all() {
        // ⭐ 同一死循环的第二条触发路径:某模型被池中**所有**号加进 model_blocklist
        //（每个号都曾对该模型返回 INVALID_MODEL_ID,TTL 1800s）之后,再来一个同模型请求。
        // is_entry_selectable 会因 is_model_blocked 过滤掉全部号 → select 返 None;
        // 而 transient_wait_outcome 旧代码不检查 model_blocklist → 判 Available → 忙等挂死。
        let config = Config::default();
        let mut c = KiroCredentials::default();
        c.id = Some(1);
        c.auth_method = Some("api_key".to_string());
        c.kiro_api_key = Some("sk-test-key".to_string());
        let mgr = MultiTokenManager::new(config, vec![c], None, None, false).unwrap();

        const MODEL: &str = "claude-opus-4.5";
        // 把该模型对唯一的号拉黑(模拟上游回过 INVALID_MODEL_ID)。
        mgr.report_model_invalid(1, Some(MODEL));

        let r = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            mgr.acquire_context(Some(MODEL), None),
        )
        .await;

        match r {
            Err(_) => panic!(
                "⭐acquire_context 在【该模型已被全池拉黑】时挂死(忙等热循环)。\
                 说明 transient_wait_outcome 缺少 is_model_blocked 过滤。"
            ),
            Ok(Ok(_)) => panic!("该模型已被唯一的号拉黑,不该还能选出号"),
            Ok(Err(e)) => assert!(
                !e.to_string().contains("竞态无法收敛"),
                "不该退化到竞态兜底上限才结束,说明过滤仍不对齐: {e}"
            ),
        }
    }

    /// 回归（审查发现）：**永久性**模型硬门（订阅档位/成本白名单）不得报成可重试的 429。
    ///
    /// **首版修复为何不够**：我原先对拿不到 blocklist TTL 的情形一律 `unwrap_or(MODEL_BLOCK_TTL)`
    /// 兜底 → 带 `retry_after_secs`。但 `allowed_models` 白名单不含该模型、或 FREE 档请求 opus，
    /// 是**永久**状态（等多久都不会变）。带退避秒数会让客户端每 5 分钟重试一次直到永远
    /// （下游还会把秒数 clamp 到 300）—— 只是把 502 死循环换成了 429 死循环。
    ///
    /// 本测试用 `allowed_models` 白名单造永久门，断言错误里**没有** retry_after 标记、
    /// 且带 `model_unsupported_by_pool=1`（供 map_provider_error 映射成不可重试的 404）。
    #[tokio::test]
    async fn test_permanent_model_gate_is_not_reported_as_retryable() {
        let config = Config::default();
        let mut c = KiroCredentials::default();
        c.id = Some(1);
        c.auth_method = Some("api_key".to_string());
        c.kiro_api_key = Some("sk-test-key".to_string());
        // 成本白名单只允许一个便宜模型 → 请求其它模型时是**永久**不可用，不是限时
        c.allowed_models = Some(vec!["qwen3-coder-next".to_string()]);
        let mgr = MultiTokenManager::new(config, vec![c], None, None, false).unwrap();

        let msg = match tokio::time::timeout(
            std::time::Duration::from_secs(3),
            mgr.acquire_context(Some("claude-opus-5"), None),
        )
        .await
        {
            Err(_) => panic!("不应挂死"),
            Ok(Ok(_)) => panic!("白名单不含该模型，不该选出号"),
            Ok(Err(e)) => e.to_string(),
        };

        assert!(
            !msg.contains("retry_after_secs="),
            "永久性硬门绝不能带退避秒数（会让客户端无限重试一个永不成功的请求）: {msg}"
        );
        assert!(
            msg.contains("model_unsupported_by_pool=1"),
            "须带显式标记供 map_provider_error 映射成不可重试的 404（不靠中文文案匹配）: {msg}"
        );
    }

    /// 回归：模型级硬门挡掉全部号时，错误必须是**可重试的模型态**，而不是"号池已禁用"。
    ///
    /// **旧代码为何 FAIL**：`WaitOutcome::NoCandidate` 分支对两种成因报同一句
    /// `所有凭据均已禁用（{available}/{total}）`。号没被禁用、只是被 `model_blocklist` 挡掉时，
    /// 它会产出自相矛盾的 `所有凭据均已禁用（1/1）`，且该串**匹配不上 `map_provider_error`
    /// 的任何分支**（无 429/QUOTA 类关键词、无 `retry_after_secs` 标记）→ 落末尾兜底
    /// **502 BAD_GATEWAY 且无 Retry-After**。
    ///
    /// 而 `model_blocklist` 是**限时态**（`MODEL_BLOCK_TTL` = 1800s，到期自动放行重试探），
    /// 报成 502 等于把可重试的临时态表达成服务端故障：客户端（Claude Code）既不退避也不换模型，
    /// 原样重发 → 再 502。线上 24h 实测 577 次此类假报（订阅不含的 gpt-5.6-* / deepseek / glm 各 ~85，
    /// claude-opus-5 88），同时污染"号池耗尽"统计口径，使真实耗尽（3221 次 available=0）无法评估。
    ///
    /// 本测试的两条断言分别锁住"不再谎报禁用"与"必须带可退避标记"，去掉修复中任一半都会 FAIL。
    #[tokio::test]
    async fn test_model_gated_pool_reports_retryable_model_error_not_pool_exhausted() {
        let config = Config::default();
        let mut c = KiroCredentials::default();
        c.id = Some(1);
        c.auth_method = Some("api_key".to_string());
        c.kiro_api_key = Some("sk-test-key".to_string());
        let mgr = MultiTokenManager::new(config, vec![c], None, None, false).unwrap();

        const MODEL: &str = "gpt-5.6-sol";
        // 唯一的号对该模型返 INVALID_MODEL_ID → 进模型级黑名单。号本身**未被禁用**。
        mgr.report_model_invalid(1, Some(MODEL));
        assert_eq!(
            mgr.available_count(),
            1,
            "前提：号未被禁用，只是被模型硬门挡住"
        );

        // 用 match 而非 expect_err：CallContext 不实现 Debug，且不该为测试便利给生产类型加 derive。
        let msg = match tokio::time::timeout(
            std::time::Duration::from_secs(3),
            mgr.acquire_context(Some(MODEL), None),
        )
        .await
        {
            Err(_) => panic!("acquire_context 不应挂死"),
            Ok(Ok(_)) => panic!("该模型已被唯一的号拉黑，不该还能选出号"),
            Ok(Err(e)) => e.to_string(),
        };

        assert!(
            !msg.contains("所有凭据均已禁用"),
            "号未被禁用却谎报'所有凭据均已禁用'（旧代码即如此，且该串会落 502 兜底）: {msg}"
        );
        assert!(
            msg.contains("retry_after_secs="),
            "模型级黑名单是限时态，必须带 retry_after_secs 才能被 map_provider_error 映射成 \
             可重试的 429（旧代码无此标记 → 502 无 Retry-After）: {msg}"
        );
    }

    /// ⭐ 致命缺陷回归（去掉 bail 串里的 `retry_after_secs=` 即 FAIL）：
    /// **号池真耗尽**（`available == 0`）必须带可退避标记。
    ///
    /// 与上面 `test_model_gated_pool_reports_retryable_model_error_not_pool_exhausted` 是
    /// 同一类缺陷的两个实例。0.7.45 修了情形②（模型硬门），情形①「真耗尽」当时漏了 ——
    /// 而线上量最大的是①：2026-08-03 01:55–02:10 号池被上游风控烧空的 15 分钟窗口里，
    /// `所有凭据均已禁用（0/0）` 产生 **2082 次**，单个 5 分钟桶峰值 937 次，
    /// 且该窗口内未识别兜底 502 **全部**是这一种。
    ///
    /// 不带标记时该串逐条穿过 `map_provider_error` 的所有分支 → 落末尾兜底 →
    /// 502 无 Retry-After → 客户端（Claude Code）把它当服务端故障、退避逻辑不启动、
    /// 原样重发 → 又 502（放大了耗尽窗口内的请求量）。
    ///
    /// 用 `AccountSuspended` 禁用而非 `TooManyFailures`：后者在
    /// `is_self_healable_reason` 覆盖范围内，会触发全池自愈把号重新启用 →
    /// 走不到 bail 分支。`AccountSuspended` 被刻意排除在自愈之外（见该函数文档），
    /// 故能稳定复现"池子确实空且不会自愈"这个终态。
    #[tokio::test]
    async fn test_truly_exhausted_pool_bail_carries_retry_after_marker() {
        let config = Config::default();
        let mut c = KiroCredentials::default();
        c.id = Some(1);
        c.auth_method = Some("api_key".to_string());
        c.kiro_api_key = Some("sk-test-key".to_string());
        let mgr = MultiTokenManager::new(config, vec![c], None, None, false).unwrap();

        // 唯一的号被真封禁用（不可自愈）→ available 归零。
        mgr.report_account_suspended(1);
        assert_eq!(mgr.available_count(), 0, "前提：池子必须真的空");

        let msg = match tokio::time::timeout(
            std::time::Duration::from_secs(3),
            mgr.acquire_context(None, None),
        )
        .await
        {
            Err(_) => panic!("acquire_context 不应挂死"),
            Ok(Ok(_)) => panic!("池子已空，不该还能选出号"),
            Ok(Err(e)) => e.to_string(),
        };

        assert!(
            msg.contains("所有凭据均已禁用"),
            "真耗尽应如实报耗尽（这条是与模型硬门情形的分界，不该混淆）: {msg}"
        );
        assert!(
            msg.contains("retry_after_secs="),
            "真耗尽是可自愈的临时态，必须带 retry_after_secs 才能被 map_provider_error \
             映射成 429 + Retry-After（旧代码无此标记 → 502 无 Retry-After → 客户端不退避 \
             → 线上 15 分钟内 2082 次）: {msg}"
        );
        // ⭐ 生产侧断言：`AccountSuspended` **不在** `is_self_healable_reason` 内，
        // 所以这个池等多久都不会自动恢复 → 必须带 `pool_permanently_exhausted=1`，
        // 让吸收层跳过它（否则会拿满 45s 预算对一个永不恢复的池空转，
        // 客户端从 <2s 拿到 429 变成 45s 才拿到，且这 45s 一直占着连接）。
        assert!(
            msg.contains("pool_permanently_exhausted=1"),
            "全是终态禁用原因时必须带永久耗尽标记: {msg}"
        );
    }

    /// ⭐ 与上一条配对：**可自愈**的全禁用**不得**带永久耗尽标记。
    ///
    /// 两条一起才钉住这个判据 —— 只有上面那条时，把
    /// `if any_healable` 写成 `if !any_healable`（或恒真/恒假）都测不出来。
    ///
    /// 回退即 FAIL：反转那个判断 → 本条的池（`TooManyFailures`，在自愈覆盖范围内）
    /// 会被打上永久标记 → 吸收层对**最大的一类可恢复失败**（线上 24h 池空类占
    /// 16.5%）直接放弃，等于吸收层对它完全失效。
    #[tokio::test]
    async fn test_healable_exhausted_pool_bail_has_no_permanent_marker() {
        let config = Config::default();
        let mut c = KiroCredentials::default();
        c.id = Some(1);
        c.auth_method = Some("api_key".to_string());
        c.kiro_api_key = Some("sk-healable".to_string());
        let mgr = MultiTokenManager::new(config, vec![c], None, None, false).unwrap();

        // 用 report_failure 打到阈值 → TooManyFailures（**在**自愈覆盖范围内）。
        for _ in 0..MAX_FAILURES_PER_CREDENTIAL {
            mgr.report_failure(1);
        }
        assert_eq!(mgr.available_count(), 0, "前提：池子必须已空");

        // 先手动把自愈时刻标成"刚刚"，让 heal_allowed=false，从而走到 NoCandidate
        // 的 bail 而不是被自愈复活（复活后拿假 token 刷新失败会改成别的原因）。
        *mgr.last_self_heal_at.lock() = Some(std::time::Instant::now());
        mgr.self_heal_streak.store(1, Ordering::Relaxed);

        let msg = match tokio::time::timeout(
            std::time::Duration::from_secs(3),
            mgr.acquire_context(None, None),
        )
        .await
        {
            Err(_) => panic!("acquire_context 不应挂死"),
            Ok(Ok(_)) => panic!("池子已空，不该还能选出号"),
            Ok(Err(e)) => e.to_string(),
        };

        assert!(msg.contains("所有凭据均已禁用"), "应如实报耗尽: {msg}");
        assert!(
            !msg.contains("pool_permanently_exhausted=1"),
            "池内存在可自愈的号时**不得**打永久标记，否则吸收层放弃最大的一类可恢复失败: {msg}"
        );
        assert!(
            msg.contains("retry_after_secs="),
            "仍必须带 retry_after_secs 让客户端退避: {msg}"
        );
    }

    #[test]
    fn test_distinct_machine_ids_untouched() {
        // 已各自独立的 machineId 不应被改动。
        let config = Config::default();
        let mut c1 = KiroCredentials::default();
        c1.id = Some(1);
        c1.machine_id = Some("a".repeat(64));
        let mut c2 = KiroCredentials::default();
        c2.id = Some(2);
        c2.machine_id = Some("b".repeat(64));

        let mgr = MultiTokenManager::new(config, vec![c1, c2], None, None, false).unwrap();
        assert_eq!(
            mgr.export_credential(1).unwrap().machine_id.unwrap(),
            "a".repeat(64)
        );
        assert_eq!(
            mgr.export_credential(2).unwrap().machine_id.unwrap(),
            "b".repeat(64)
        );
    }

    #[tokio::test]
    async fn test_add_credential_freezes_machine_id() {
        // 上号入池(machine_id=None)应在 add 时固化稳定指纹,而非留 None 靠请求路径现算
        // (现算会随 refreshToken 轮换漂移,是防关联隐患)。
        let config = Config::default();
        let manager = MultiTokenManager::new(config, vec![], None, None, false).unwrap();
        let mut cred = KiroCredentials::default();
        cred.kiro_api_key = Some("ksk_freeze_test".to_string());
        cred.auth_method = Some("api_key".to_string());
        let id = manager.add_credential(cred).await.unwrap();
        let mid = manager
            .export_credential(id)
            .unwrap()
            .machine_id
            .expect("入池后 machineId 应已固化");
        assert_eq!(mid.len(), 64);
        assert!(mid.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[tokio::test]
    async fn test_add_credential_rotates_colliding_machine_id() {
        // 新号指纹与池中已有号撞车时,入池应轮换成独立指纹(防上游按设备指纹关联封禁)。
        let config = Config::default();
        let shared = "d".repeat(64);
        let mut existing = KiroCredentials::default();
        existing.kiro_api_key = Some("ksk_existing".to_string());
        existing.auth_method = Some("api_key".to_string());
        existing.machine_id = Some(shared.clone());
        let manager = MultiTokenManager::new(config, vec![existing], None, None, false).unwrap();
        let mut newcomer = KiroCredentials::default();
        newcomer.kiro_api_key = Some("ksk_newcomer".to_string());
        newcomer.auth_method = Some("api_key".to_string());
        newcomer.machine_id = Some(shared.clone());
        let id = manager.add_credential(newcomer).await.unwrap();
        let stored_mid = manager.export_credential(id).unwrap().machine_id.unwrap();
        assert_ne!(stored_mid, shared, "撞车指纹必须被轮换成独立值");
        assert_eq!(stored_mid.len(), 64);
    }

    #[test]
    fn test_multi_token_manager_empty_credentials() {
        let config = Config::default();
        let result = MultiTokenManager::new(config, vec![], None, None, false);
        // 支持 0 个凭据启动（可通过管理面板添加）
        assert!(result.is_ok());
        let manager = result.unwrap();
        assert_eq!(manager.total_count(), 0);
        assert_eq!(manager.available_count(), 0);
    }

    #[test]
    fn test_multi_token_manager_duplicate_ids() {
        let config = Config::default();
        let mut cred1 = KiroCredentials::default();
        cred1.id = Some(1);
        let mut cred2 = KiroCredentials::default();
        cred2.id = Some(1); // 重复 ID

        let result = MultiTokenManager::new(config, vec![cred1, cred2], None, None, false);
        assert!(result.is_err());
        let err_msg = result.err().unwrap().to_string();
        assert!(
            err_msg.contains("重复的凭据 ID"),
            "错误消息应包含 '重复的凭据 ID'，实际: {}",
            err_msg
        );
    }

    #[test]
    fn test_multi_token_manager_api_key_missing_kiro_api_key_auto_disabled() {
        let config = Config::default();

        // auth_method=api_key 但缺少 kiro_api_key → 应被自动禁用
        let mut bad_cred = KiroCredentials::default();
        bad_cred.auth_method = Some("api_key".to_string());
        // kiro_api_key 保持 None

        let mut good_cred = KiroCredentials::default();
        good_cred.refresh_token = Some("valid_token".to_string());

        let manager =
            MultiTokenManager::new(config, vec![bad_cred, good_cred], None, None, false).unwrap();
        assert_eq!(manager.total_count(), 2);
        assert_eq!(manager.available_count(), 1); // bad_cred 被禁用，只剩 1 个可用
    }

    #[test]
    fn test_multi_token_manager_api_key_with_kiro_api_key_not_disabled() {
        let config = Config::default();

        // auth_method=api_key 且有 kiro_api_key → 不应被禁用
        let mut cred = KiroCredentials::default();
        cred.auth_method = Some("api_key".to_string());
        cred.kiro_api_key = Some("ksk_test123".to_string());

        let manager = MultiTokenManager::new(config, vec![cred], None, None, false).unwrap();
        assert_eq!(manager.total_count(), 1);
        assert_eq!(manager.available_count(), 1);
    }

    #[test]
    fn test_multi_token_manager_report_failure() {
        let config = Config::default();
        let cred1 = KiroCredentials::default();
        let cred2 = KiroCredentials::default();

        let manager =
            MultiTokenManager::new(config, vec![cred1, cred2], None, None, false).unwrap();

        // 凭据会自动分配 ID（从 1 开始）
        // 前两次失败不会禁用（使用 ID 1）
        assert!(manager.report_failure(1));
        assert!(manager.report_failure(1));
        assert_eq!(manager.available_count(), 2);

        // 第三次失败会禁用第一个凭据
        assert!(manager.report_failure(1));
        assert_eq!(manager.available_count(), 1);

        // 继续失败第二个凭据（使用 ID 2）
        assert!(manager.report_failure(2));
        assert!(manager.report_failure(2));
        assert!(!manager.report_failure(2)); // 所有凭据都禁用了
        assert_eq!(manager.available_count(), 0);
    }

    #[test]
    fn test_multi_token_manager_report_success() {
        let config = Config::default();
        let cred = KiroCredentials::default();

        let manager = MultiTokenManager::new(config, vec![cred], None, None, false).unwrap();

        // 失败两次（使用 ID 1）
        manager.report_failure(1);
        manager.report_failure(1);

        // 成功后重置计数（使用 ID 1）
        manager.report_success(1);

        // 再失败两次不会禁用
        manager.report_failure(1);
        manager.report_failure(1);
        assert_eq!(manager.available_count(), 1);
    }

    #[test]
    fn test_multi_token_manager_switch_to_next() {
        let config = Config::default();
        let mut cred1 = KiroCredentials::default();
        cred1.refresh_token = Some("token1".to_string());
        let mut cred2 = KiroCredentials::default();
        cred2.refresh_token = Some("token2".to_string());

        let manager =
            MultiTokenManager::new(config, vec![cred1, cred2], None, None, false).unwrap();

        let initial_id = manager.snapshot().current_id;

        // 切换到下一个
        assert!(manager.switch_to_next());
        assert_ne!(manager.snapshot().current_id, initial_id);
    }

    #[test]
    fn test_set_load_balancing_mode_persists_to_config_file() {
        let config_path =
            std::env::temp_dir().join(format!("kiro-load-balancing-{}.json", uuid::Uuid::new_v4()));
        std::fs::write(&config_path, r#"{"loadBalancingMode":"priority"}"#).unwrap();

        let config = Config::load(&config_path).unwrap();
        let manager =
            MultiTokenManager::new(config, vec![KiroCredentials::default()], None, None, false)
                .unwrap();

        manager
            .set_load_balancing_mode("balanced".to_string())
            .unwrap();

        let persisted = Config::load(&config_path).unwrap();
        assert_eq!(persisted.load_balancing_mode, "balanced");
        assert_eq!(manager.get_load_balancing_mode(), "balanced");

        std::fs::remove_file(&config_path).unwrap();
    }

    #[tokio::test]
    async fn test_multi_token_manager_acquire_context_auto_recovers_all_disabled() {
        let config = Config::default();
        let mut cred1 = KiroCredentials::default();
        cred1.access_token = Some("t1".to_string());
        cred1.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());
        let mut cred2 = KiroCredentials::default();
        cred2.access_token = Some("t2".to_string());
        cred2.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());

        let manager =
            MultiTokenManager::new(config, vec![cred1, cred2], None, None, false).unwrap();

        // 凭据会自动分配 ID（从 1 开始）
        for _ in 0..MAX_FAILURES_PER_CREDENTIAL {
            manager.report_failure(1);
        }
        for _ in 0..MAX_FAILURES_PER_CREDENTIAL {
            manager.report_failure(2);
        }

        assert_eq!(manager.available_count(), 0);

        // 应触发自愈：重置失败计数并重新启用，避免必须重启进程
        let ctx = manager.acquire_context(None, None).await.unwrap();
        assert!(ctx.token == "t1" || ctx.token == "t2");
        assert_eq!(manager.available_count(), 2);
    }

    #[tokio::test]
    async fn test_multi_token_manager_acquire_context_balanced_retries_until_bad_credential_disabled()
     {
        let mut config = Config::default();
        config.load_balancing_mode = "balanced".to_string();

        let mut bad_cred = KiroCredentials::default();
        bad_cred.priority = 0;
        bad_cred.refresh_token = Some("bad".to_string());

        let mut good_cred = KiroCredentials::default();
        good_cred.priority = 1;
        good_cred.access_token = Some("good-token".to_string());
        good_cred.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());

        let manager =
            MultiTokenManager::new(config, vec![bad_cred, good_cred], None, None, false).unwrap();

        let ctx = manager.acquire_context(None, None).await.unwrap();
        assert_eq!(ctx.id, 2);
        assert_eq!(ctx.token, "good-token");
    }

    /// 构造 N 个都带有效 token（无需刷新）的 balanced 管理器
    fn make_balanced_manager(n: usize) -> MultiTokenManager {
        let mut config = Config::default();
        config.load_balancing_mode = "balanced".to_string();
        // 关闭亲和性：本组测试要验证纯负载分摊，不要 session 粘性干扰
        config.affinity_enabled = false;
        let creds: Vec<KiroCredentials> = (0..n)
            .map(|i| {
                let mut c = KiroCredentials::default();
                c.priority = i as u32;
                c.access_token = Some(format!("tok-{}", i));
                c.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());
                c
            })
            .collect();
        MultiTokenManager::new(config, creds, None, None, false).unwrap()
    }

    /// 🔴 `observed_upstream_rpm` 必须统计**上游尝试数**，且与入站实测是两个不同的量。
    ///
    /// 这条钉死本次修复的核心事实：整形层在 failover 循环**之外**每客户端请求取 1 个令牌，
    /// 而 `rpm.record` 在**选号时**记账 ⇒ 每次 failover 尝试各记一次。两者量纲不同，
    /// 比值即重试放大倍数（2026-08-06 线上实测 4.59×：1317 客户端请求 → 6040 次上游尝试）。
    ///
    /// 混着读的后果已经发生过：面板显示 500、客户端实际 50~70、运维看到逐号之和 600，
    /// 三个数字互相矛盾，而运维据此做过两次限流分析、差点改线上 `inboundTargetRpm`。
    #[test]
    fn observed_upstream_rpm_counts_attempts_not_client_requests() {
        let manager = make_balanced_manager(3);
        let ids: Vec<u64> = manager.entries.lock().iter().map(|e| e.id).collect();
        assert_eq!(ids.len(), 3, "夹具自检：应有 3 个号");

        assert_eq!(
            manager.observed_upstream_rpm(),
            0,
            "零流量时上游尝试数必须是 0"
        );

        // 模拟「一个客户端请求 failover 打了 3 个号」：入站是 1，上游尝试是 3。
        for id in &ids {
            manager.rpm.record(*id);
        }
        assert_eq!(
            manager.observed_upstream_rpm(),
            3,
            "3 次选号 = 3 次上游尝试，即便它们来自同一个客户端请求"
        );

        // 入站实测此时仍是 0 —— 因为这些 record 没有走 acquire()。
        // 这正是两个量纲必须分开的证据：若它们是同一个量，这里不可能一个是 3 一个是 0。
        assert_eq!(
            manager.observed_inbound_rpm(),
            0,
            "上游尝试不该被算成客户端请求；两者若相等说明量纲又混了"
        );
    }

    /// 禁用号不计入上游 RPM 之和。
    ///
    /// 否则面板会把已被移出调度的号的残留窗口算进"上游正在承受的速率"，
    /// 在批量封号场景下（实测 2026-08-06 22:03 一次禁用 14 个）读数会显著虚高，
    /// 而那正是最需要准确读数的时刻。
    #[test]
    fn observed_upstream_rpm_excludes_disabled() {
        let manager = make_balanced_manager(3);
        let ids: Vec<u64> = manager.entries.lock().iter().map(|e| e.id).collect();
        for id in &ids {
            manager.rpm.record(*id);
        }
        assert_eq!(manager.observed_upstream_rpm(), 3);

        {
            let mut entries = manager.entries.lock();
            entries[0].disabled = true;
        }
        assert_eq!(
            manager.observed_upstream_rpm(),
            2,
            "禁用号的残留窗口不该计入上游承受速率"
        );
    }

    // ============ 余额加权分流(0.7.24,旧代码上会失败:旧代码无 balance_factor/set_balance_snapshots)============

    fn mk_bal_snap(remaining: f64, effective_limit: f64, used_at_cache: f64) -> BalanceSnapshot {
        BalanceSnapshot {
            remaining_at_cache: remaining,
            effective_limit,
            credits_used_at_cache: used_at_cache,
        }
    }

    /// 余额加权因子数学:满额→1.0、半额→floor+0.5×(1-floor)、耗尽→floor;缺快照/关闭→中性 1.0。
    #[test]
    fn test_balance_factor_math_and_neutral() {
        let m = make_balanced_manager(1);
        // 默认 floor=50 → 0.5。满额(remaining=eff)→ factor=1.0。
        m.set_balance_snapshots(HashMap::from([(1, mk_bal_snap(100.0, 100.0, 0.0))]));
        assert!((m.balance_factor(1, 0.0) - 1.0).abs() < 1e-9, "满额 → 1.0");
        // 半额(remaining=50/100)→ 0.5 + 0.5×0.5 = 0.75。
        m.set_balance_snapshots(HashMap::from([(1, mk_bal_snap(50.0, 100.0, 0.0))]));
        assert!(
            (m.balance_factor(1, 0.0) - 0.75).abs() < 1e-9,
            "半额 → 0.75"
        );
        // 耗尽(remaining=0)→ floor=0.5。
        m.set_balance_snapshots(HashMap::from([(1, mk_bal_snap(0.0, 100.0, 0.0))]));
        assert!(
            (m.balance_factor(1, 0.0) - 0.5).abs() < 1e-9,
            "耗尽 → floor 0.5"
        );
        // 缺快照 → 中性 1.0(新号不被惩罚)。
        m.set_balance_snapshots(HashMap::new());
        assert!(
            (m.balance_factor(1, 0.0) - 1.0).abs() < 1e-9,
            "缺快照 → 中性 1.0"
        );
        // 加权关 → 恒中性 1.0(退回纯 0.7.23)。
        m.set_balance_snapshots(HashMap::from([(1, mk_bal_snap(0.0, 100.0, 0.0))]));
        m.balance_weight_enabled.store(false, Ordering::Relaxed);
        assert!(
            (m.balance_factor(1, 0.0) - 1.0).abs() < 1e-9,
            "开关关 → 中性 1.0"
        );
    }

    /// 本地累加修正:快照后本地花费增量拉低估算剩余 → 因子下降(比纯 30 分钟旧快照更准)。
    #[test]
    fn test_balance_factor_local_correction() {
        let m = make_balanced_manager(1);
        // 快照:满额 100,基线花费 200(生命周期累计)。
        m.set_balance_snapshots(HashMap::from([(1, mk_bal_snap(100.0, 100.0, 200.0))]));
        // 当前累计花费仍 200(无新增)→ 满额 → 1.0。
        assert!(
            (m.balance_factor(1, 200.0) - 1.0).abs() < 1e-9,
            "无新增 → 满额"
        );
        // 当前累计花费 250(快照后新花 50)→ est_remaining=100-50=50 → 半额 → 0.75。
        assert!(
            (m.balance_factor(1, 250.0) - 0.75).abs() < 1e-9,
            "快照后花 50 → 估半额 0.75"
        );
        // 花费退回(重置/负增量)钳到 0 → 满额,不因时钟/重置乱跳。
        assert!(
            (m.balance_factor(1, 150.0) - 1.0).abs() < 1e-9,
            "负增量钳 0 → 满额"
        );
    }

    /// 性质守卫：**全新号池（所有排序键平局）必须把流量铺开**，不得钉在下标最小的号上。
    ///
    /// 平局条件（缺一个就会被别的键打破、测不出 jitter 的作用）：
    /// - `credential_rpm_limit` 设极大 → `rpm_usage_permille = rpm*1000/cap` 恒为 0
    ///   （否则 `commit_selection` 的 `rpm.record` 会立刻让它分叉）；
    /// - 每次取号后立即 drop → `inflight` 归零；
    /// - 不调 `report_success` → `success_count` 恒 0；
    /// - 同 priority、同健康（全新号 EWMA 均为乐观初值）。
    ///
    /// 这是"刚补一批新号、池子空闲"时的真实形态（新号 success_count 全为 0）。
    ///
    /// ⚠️ 本测试**不断言实现方式**，只断言可观测性质。历史注记：曾为此加过一个随机
    /// 打散键 `tie_break_jitter`，但两次尝试都无法构造出"移除它即失败"的测试
    /// （本测试在有无该键时都通过），说明平局在实际调用序列里已被其它键打破。
    /// 按"无证据支撑的改动不保留"的原则该键已删除，本测试作为性质回归留下。
    #[tokio::test]
    async fn test_jitter_breaks_full_tie_among_fresh_credentials() {
        let mut config = Config::default();
        config.load_balancing_mode = "balanced".to_string();
        config.affinity_enabled = false;
        // 关键：容量极大，让 rpm_usage_permille 恒为 0，不去打破平局
        config.credential_rpm_limit = 1_000_000;
        let creds: Vec<KiroCredentials> = (0..4)
            .map(|i| {
                let mut c = KiroCredentials::default();
                c.priority = 0;
                c.access_token = Some(format!("tok-{i}"));
                c.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());
                c
            })
            .collect();
        let manager = MultiTokenManager::new(config, creds, None, None, false).unwrap();

        let mut hits: std::collections::HashMap<u64, usize> = std::collections::HashMap::new();
        for _ in 0..80 {
            let g = manager.acquire_context(None, None).await.unwrap();
            *hits.entry(g.id).or_default() += 1;
            drop(g); // inflight 归零，下一轮重新全平局
        }
        assert!(
            hits.len() >= 3,
            "全新号池应把流量铺开到多个号，实际只用了 {} 个：{hits:?}",
            hits.len()
        );
    }

    /// 回归（S2 封号原因持久化）：自动禁用原因**不得**在重启后退化成「手动禁用」。
    ///
    /// **旧代码为何失败**：`KiroCredentials` 只有 `disabled: bool`、没有原因字段；
    /// `persist_credentials` 只写 `cred.disabled`；而加载路径对**所有** disabled 号
    /// 一律回填 `Some(DisabledReason::Manual)`。于是 `QuotaExceeded` / `AccountSuspended` /
    /// `SuspiciousActivityAuto` / `TooManyFailures` 等自动禁用原因重启即全部丢失
    /// —— 用户明确要求的「认定封号必须标明原因」当前是重启即失效。
    /// 并且以 reason 为判据的自愈逻辑会把自动禁用误判成人工禁用。
    ///
    /// 本测试走**真实的序列化往返**（serde_json），断言原因与时刻都能穿过重启。
    /// 旧代码下 `KiroCredentials` 连这两个字段都没有，反序列化后 `disabled_reason` 为 None
    /// → 加载路径回填 Manual → 断言失败。
    #[test]
    fn test_disabled_reason_survives_persist_roundtrip() {
        let mut cred = KiroCredentials::default();
        cred.id = Some(7);
        cred.disabled = true;
        cred.disabled_reason = Some(DisabledReason::AccountSuspended);
        cred.disabled_at = Some("2026-07-29T10:00:00+00:00".to_string());

        // 真实往返：写盘格式 → 读回
        let json = serde_json::to_string(&cred).expect("序列化");
        let back: KiroCredentials = serde_json::from_str(&json).expect("反序列化");

        assert_eq!(
            back.disabled_reason,
            Some(DisabledReason::AccountSuspended),
            "自动禁用原因必须穿过持久化往返（旧代码无此字段 → 恒 None → 加载时被回填成 Manual）"
        );
        assert_eq!(
            back.disabled_at.as_deref(),
            Some("2026-07-29T10:00:00+00:00"),
            "禁用时刻必须持久化，运维需要它判断『这号坏了多久』"
        );
        assert!(back.disabled, "disabled 本身当然也要保留");

        // 线格式必须是稳定的 camelCase（外部脚本/前端按它读）
        assert!(
            json.contains("\"disabledReason\":\"accountSuspended\""),
            "线格式应为 camelCase 稳定命名，实际 json={json}"
        );
    }

    /// 回归（S2 向后兼容）：旧凭据文件（无 `disabledReason` 字段）必须仍能加载。
    ///
    /// 新增字段用 `#[serde(default)]`，所以旧文件读回来是 `None`，
    /// 加载路径再回落 `Manual` —— 这是**刻意保留**的降级行为，不是缺陷。
    #[test]
    fn test_legacy_credentials_without_reason_still_load() {
        let legacy = r#"{"id":9,"accessToken":"t","refreshToken":"r","disabled":true}"#;
        let cred: KiroCredentials = serde_json::from_str(legacy).expect("旧格式必须仍可解析");
        assert!(cred.disabled);
        assert_eq!(
            cred.disabled_reason, None,
            "旧文件无该字段 → None（加载路径回落 Manual）"
        );
        assert_eq!(cred.disabled_at, None);
        // 新字段同样 serde default：旧文件无该字段 → None（跨月恢复视为可恢复）。
        assert_eq!(cred.quota_exhausted_at, None);
    }

    // ===== #10 双份状态四件套：三处同步契约守卫 =====
    //
    // 双份字段：entry 真源（disabled / disabled_reason / disabled_at /
    // quota_exhausted_at）↔ credentials 持久化镜像。同步契约三处：
    // ① load 回填（MultiTokenManager::new）② persist 全量写盘（persist_credentials）
    // ③ set_disabled 收口（含自动禁用路径的 persist_disabled_state）。
    // 以下行为测试钉死三处；最后一个 needle 守卫钉死「同步代码仍存在」。

    /// #10 三处同步契约之「load 回填」守卫：持久化四件套必须完整回填进 entry，
    /// 非 Manual 原因不得被吞（W5「重启变 Manual」的回归点）。
    #[test]
    fn load_backfills_all_four_disabled_fields() {
        let mut cred = KiroCredentials::default();
        cred.id = Some(1);
        cred.access_token = Some("t1".to_string());
        cred.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());
        cred.disabled = true;
        cred.disabled_reason = Some(DisabledReason::QuotaExceeded);
        cred.disabled_at = Some("2026-07-29T10:00:00+00:00".to_string());
        // ⚠️ quota_exhausted_at 必须落在**当月**：new() 启动期会跑跨月配额恢复
        // （recover_expired_quota_disables），上月时间戳的 QuotaExceeded 号当场被复活，
        // 本测试就测不到 load 回填本身了。固定用当月 2 日，任何真实日期运行都不触发恢复。
        let now = Utc::now();
        cred.quota_exhausted_at =
            Some(format!("{:04}-{:02}-02T10:00:00+00:00", now.year(), now.month()));

        let mgr =
            MultiTokenManager::new(Config::default(), vec![cred], None, None, false).unwrap();
        let entries = mgr.entries.lock();
        let e = entries.iter().find(|e| e.id == 1).expect("凭据应入池");
        assert!(e.disabled, "disabled 必须回填");
        assert_eq!(
            e.disabled_reason,
            Some(DisabledReason::QuotaExceeded),
            "非 Manual 原因不得被吞成 Manual（W5 回归点）"
        );
        assert_eq!(
            e.disabled_at.as_deref(),
            Some("2026-07-29T10:00:00+00:00"),
            "禁用时刻必须回填"
        );
        // ⚠️ quota_exhausted_at 的 fixture 是「当月 2 日」（见上，防跨月恢复抢先），
        // 断言必须与 fixture 同步（不能写死 2026-07-29）。
        let now = Utc::now();
        let expected_quota_ts = format!("{:04}-{:02}-02T10:00:00+00:00", now.year(), now.month());
        assert_eq!(
            e.quota_exhausted_at.as_deref(),
            Some(expected_quota_ts.as_str()),
            "额度耗尽判定时刻必须回填（跨月恢复判据）"
        );
    }

    /// #10 三处同步契约之「persist 全量写盘」守卫：entry 四件套必须整组落盘，
    /// 新管理器读回时完全一致（真实序列化往返 + 二次加载闭环）。
    #[test]
    fn persist_writes_all_four_disabled_fields_and_roundtrips() {
        let dir = std::env::temp_dir().join(format!(
            "kiro-disabled-quad-persist-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let cred_path = dir.join("credentials.json");

        let mut cred = KiroCredentials::default();
        cred.id = Some(1);
        cred.access_token = Some("t1".to_string());
        cred.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());
        let mgr = MultiTokenManager::new(
            Config::default(),
            vec![cred],
            None,
            Some(cred_path.clone()),
            true,
        )
        .unwrap();

        // 模拟自动禁用路径（与 report_failure 等相同的 entry 直写 + 落盘收口）。
        // quota_exhausted_at 用**当月**时间戳：mgr2 二次加载时启动期跨月配额恢复
        // 会把上月时间戳的 QuotaExceeded 号当场复活，往返断言测的就不是持久化而是恢复。
        let now = Utc::now();
        let current_month_quota_ts =
            format!("{:04}-{:02}-02T10:00:00+00:00", now.year(), now.month());
        {
            let mut entries = mgr.entries.lock();
            let e = entries.iter_mut().find(|e| e.id == 1).unwrap();
            e.disabled = true;
            e.disabled_reason = Some(DisabledReason::QuotaExceeded);
            e.disabled_at = Some("2026-07-29T10:00:00+00:00".to_string());
            e.quota_exhausted_at = Some(current_month_quota_ts.clone());
        }
        mgr.persist_credentials().unwrap();

        // 磁盘断言四件套。
        let back = crate::kiro::model::credentials::CredentialsConfig::load(&cred_path)
            .unwrap()
            .into_sorted_credentials();
        let b = back.iter().find(|c| c.id == Some(1)).unwrap();
        assert!(b.disabled, "disabled 必须落盘");
        assert_eq!(
            b.disabled_reason,
            Some(DisabledReason::QuotaExceeded),
            "自动禁用原因必须落盘（否则重启变 Manual）"
        );
        assert_eq!(b.disabled_at.as_deref(), Some("2026-07-29T10:00:00+00:00"));
        assert_eq!(
            b.quota_exhausted_at.as_deref(),
            Some(current_month_quota_ts.as_str()),
            "quota_exhausted_at 必须与 entry 直写同值落盘"
        );

        // 二次加载：entry 回填与磁盘一致（三处同步闭环）。
        let mgr2 = MultiTokenManager::new(
            Config::default(),
            back,
            None,
            Some(cred_path.clone()),
            true,
        )
        .unwrap();
        let e2 = mgr2.entries.lock();
        let entry2 = e2.iter().find(|e| e.id == 1).unwrap();
        assert_eq!(
            entry2.disabled_reason,
            Some(DisabledReason::QuotaExceeded),
            "重启往返后原因不得退化成 Manual"
        );
        drop(e2);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// #10 三处同步契约之「set_disabled 收口」守卫：手动禁用/启用一步到位
    /// 写 entry 四件套 + 落盘，内存与磁盘双向一致。
    #[test]
    fn set_disabled_syncs_quad_and_disk() {
        let dir = std::env::temp_dir().join(format!(
            "kiro-disabled-quad-set-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let cred_path = dir.join("credentials.json");

        let mut cred = KiroCredentials::default();
        cred.id = Some(1);
        cred.access_token = Some("t1".to_string());
        cred.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());
        let mgr = MultiTokenManager::new(
            Config::default(),
            vec![cred],
            None,
            Some(cred_path.clone()),
            true,
        )
        .unwrap();

        // 禁用：entry 四件套 + 盘上一致。
        mgr.set_disabled(1, true).unwrap();
        {
            let e = mgr.entries.lock();
            let entry = e.iter().find(|e| e.id == 1).unwrap();
            assert!(entry.disabled);
            assert_eq!(entry.disabled_reason, Some(DisabledReason::Manual));
            assert!(entry.disabled_at.is_some(), "手动禁用必须盖时刻");
            assert_eq!(entry.quota_exhausted_at, None);
        }
        let back = crate::kiro::model::credentials::CredentialsConfig::load(&cred_path)
            .unwrap()
            .into_sorted_credentials();
        let b = back.iter().find(|c| c.id == Some(1)).unwrap();
        assert!(b.disabled);
        assert_eq!(
            b.disabled_reason,
            Some(DisabledReason::Manual),
            "手动禁用原因必须落盘"
        );
        assert!(b.disabled_at.is_some(), "禁用时刻必须落盘");

        // 启用：四件套清空 + 盘上一致。
        mgr.set_disabled(1, false).unwrap();
        {
            let e = mgr.entries.lock();
            let entry = e.iter().find(|e| e.id == 1).unwrap();
            assert!(!entry.disabled);
            assert_eq!(entry.disabled_reason, None);
            assert_eq!(entry.disabled_at, None);
            assert_eq!(
                entry.quota_exhausted_at,
                None,
                "启用必须清额度判定时刻（残留会干扰跨月恢复）"
            );
        }
        let back2 = crate::kiro::model::credentials::CredentialsConfig::load(&cred_path)
            .unwrap()
            .into_sorted_credentials();
        let b2 = back2.iter().find(|c| c.id == Some(1)).unwrap();
        assert!(!b2.disabled);
        assert_eq!(b2.disabled_reason, None, "启用后原因不得残留盘上");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// 源码守卫（#10）：双份字段「三处同步」契约必须持续存在——
    /// ① load 回填（new 里优先读持久化原因）② persist 全量写盘（四件套回写）
    /// ③ set_disabled 收口（启用清 reason/at/quota + 落盘）。任何一处被删或绕过，
    /// 「重启变 Manual」或「盘内存分叉」类缺陷会以新形式回来。
    /// needle 运行时拼接 + 截断测试段（本仓守卫纪律：注释/测试不写被守卫代码字面量）。
    #[test]
    fn disabled_quad_three_sync_points_stay_wired() {
        let src = include_str!("token_manager.rs");
        let persist = include_str!("credential_persist.rs");
        let cut = src.find("#[cfg(test)]").unwrap_or(src.len());
        let prod_owned = format!("{}\n{}", &src[..cut], persist);
        let prod = prod_owned.as_str();

        // ② persist 全量写盘：四件套回写必须在 persist_credentials 内。
        let n_persist_reason = format!("cred.disabled_reason = e.{}", "disabled_reason");
        assert!(
            prod.contains(&n_persist_reason),
            "persist_credentials 必须回写 disabled_reason 镜像——删掉则自动禁用原因重启变 Manual"
        );
        let n_persist_quota = format!("cred.{}", "quota_exhausted_at = e.quota_exhausted_at");
        assert!(
            prod.contains(&n_persist_quota),
            "persist_credentials 必须回写 quota_exhausted_at——删掉则跨月恢复判据重启即丢"
        );

        // ① load 回填：加载时优先用持久化原因（旧文件才回落 Manual）。
        let n_load = format!("cred.disabled_reason.{}", "unwrap_or");
        assert!(
            prod.contains(&n_load),
            "load 必须优先读持久化原因而非无条件 Manual（W5 回归点）"
        );

        // ③ set_disabled 收口：启用路径必须清 quota_exhausted_at + 落盘。
        let head = format!("pub fn set_disabled(&self, id: u64, disabled: bool){}", " ->");
        let start = prod
            .find(&head)
            .expect("set_disabled 不应被改名/删除");
        let end = prod[start..]
            .find("\n    /// 设置凭据优先级")
            .expect("set_disabled 的后继函数不应被改名")
            + start;
        let body = &prod[start..end];
        let n_clear_quota = format!("entry.{}", "quota_exhausted_at = None");
        assert!(
            body.contains(&n_clear_quota),
            "set_disabled 启用路径必须清 quota_exhausted_at（残留旧月份时间戳干扰跨月恢复）"
        );
        let n_clear_disabled_at = format!("entry.{}", "disabled_at = None");
        assert!(
            body.contains(&n_clear_disabled_at),
            "set_disabled 启用路径必须清 disabled_at（残留旧禁用时刻随 persist 落盘，运维误判）"
        );
        let n_invalidate = format!("self.{}", "invalidate_model_catalog(id)");
        assert!(
            body.contains(&n_invalidate),
            "set_disabled 启用路径必须失效模型目录缓存（Review3 m5：禁用期残留 Confirmed 重启用后白打一跳）"
        );
        let n_persist = format!("self.{}", "persist_credentials()?;");
        assert!(
            body.contains(&n_persist),
            "set_disabled 必须落盘（否则重启回旧状态）"
        );
    }

    /// persist_credentials 锁序守卫：必须先 persist_lock 再 entries。反向会与
    /// 「调用方先放 entries 再 persist」的约定对撞成死锁。
    #[test]
    fn persist_credentials_takes_persist_lock_before_entries() {
        let persist = include_str!("credential_persist.rs");
        let head = format!("fn persist_{}", "credentials");
        let start = persist
            .find(&head)
            .expect("persist_credentials 不应被改名");
        let end = persist[start..]
            .find("fn trash_path")
            .expect("persist_credentials 的后继不应被改名")
            + start;
        let body = &persist[start..end];
        let n_persist = format!("self.{}", "persist_lock.lock()");
        let n_entries = format!("self.{}", "entries.lock()");
        let p = body
            .find(&n_persist)
            .expect("persist_credentials 必须持 persist_lock 串行化写盘");
        let e = body
            .find(&n_entries)
            .expect("persist_credentials 必须在 persist_lock 内快照 entries");
        assert!(
            p < e,
            "锁序必须 persist_lock → entries（反向会死锁）"
        );
    }

    /// credentials_path=None 时 persist 是 no-op（测试纯内存池、无磁盘）。
    #[test]
    fn persist_credentials_skips_without_path() {
        let mgr = MultiTokenManager::new(
            Config::default(),
            vec![KiroCredentials::default()],
            None,
            None,
            false,
        )
        .unwrap();
        assert_eq!(
            mgr.persist_credentials().unwrap(),
            false,
            "无路径时不得写盘"
        );
    }

    /// persist 写串行化：主线程先持 persist_lock 再派 T1 落盘 —— T1 必被挡在
    /// 快照之前。等待期间改非密钥字段（禁用），放行后 T1 取到的必须是最新快照。
    /// 修复前无此锁，T1 立即落盘旧启用态。
    #[test]
    fn persist_credentials_serializes_through_persist_lock() {
        let dir = std::env::temp_dir().join(format!(
            "kiro-persist-lock-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let cred_path = dir.join("credentials.json");

        let mut cred = KiroCredentials::default();
        cred.id = Some(1);
        cred.access_token = Some("t1".to_string());
        cred.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());
        let mgr = std::sync::Arc::new(
            MultiTokenManager::new(
                Config::default(),
                vec![cred],
                None,
                Some(cred_path.clone()),
                true,
            )
            .unwrap(),
        );
        mgr.persist_credentials().unwrap();

        let guard = mgr.persist_lock.lock();
        let t1 = {
            let mgr = std::sync::Arc::clone(&mgr);
            std::thread::spawn(move || mgr.persist_credentials())
        };
        {
            let mut entries = mgr.entries.lock();
            let e = entries.iter_mut().find(|e| e.id == 1).unwrap();
            e.disabled = true;
            e.disabled_reason = Some(DisabledReason::QuotaExceeded);
            e.disabled_at = Some("2026-08-21T00:00:00+00:00".to_string());
        }
        drop(guard);
        t1.join().unwrap().unwrap();

        let back = crate::kiro::model::credentials::CredentialsConfig::load(&cred_path)
            .unwrap()
            .into_sorted_credentials();
        let b = back.iter().find(|c| c.id == Some(1)).unwrap();
        assert!(
            b.disabled,
            "排队中的 persist 必须取到最新快照（含等待期间的禁用）"
        );
        assert_eq!(
            b.disabled_reason,
            Some(DisabledReason::QuotaExceeded),
            "排队落盘不得把刚禁用的号写回启用"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// persist_trash 锁序守卫：必须先 persist_lock 再 trash。反向会与
    /// 「调用方先放 trash 再 persist」的约定对撞成死锁；也不得先 trash 再
    /// persist_lock（与 persist_credentials 的 persist_lock → entries 构成 ABBA）。
    #[test]
    fn persist_trash_takes_persist_lock_before_trash() {
        let persist = include_str!("credential_persist.rs");
        let head = format!("fn persist_{}", "trash");
        let start = persist.find(&head).expect("persist_trash 不应被改名");
        let body = &persist[start..];
        let n_persist = format!("self.{}", "persist_lock.lock()");
        let n_trash = format!("self.{}", "trash.lock()");
        let p = body
            .find(&n_persist)
            .expect("persist_trash 必须持 persist_lock 串行化写盘");
        let t = body
            .find(&n_trash)
            .expect("persist_trash 必须在 persist_lock 内快照 trash");
        assert!(
            p < t,
            "锁序必须 persist_lock → trash（反向会死锁）"
        );
    }

    /// credentials_path=None 时 trash_path 也为 None，persist_trash 是 no-op。
    #[test]
    fn persist_trash_skips_without_path() {
        let mgr = MultiTokenManager::new(
            Config::default(),
            vec![KiroCredentials::default()],
            None,
            None,
            false,
        )
        .unwrap();
        assert_eq!(
            mgr.persist_trash().unwrap(),
            false,
            "无路径时不得写盘"
        );
    }

    /// persist_trash 写串行化：主线程先持 persist_lock 再派 T1 落盘 —— T1 必被挡在
    /// 快照之前。等待期间改 trash 条目，放行后 T1 取到的必须是最新快照。
    #[test]
    fn persist_trash_serializes_through_persist_lock() {
        let dir = std::env::temp_dir().join(format!(
            "kiro-persist-trash-lock-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let cred_path = dir.join("credentials.json");

        let mut cred = KiroCredentials::default();
        cred.id = Some(7);
        cred.access_token = Some("t-trash".to_string());
        let mgr = std::sync::Arc::new(
            MultiTokenManager::new(
                Config::default(),
                vec![KiroCredentials::default()],
                None,
                Some(cred_path),
                true,
            )
            .unwrap(),
        );
        {
            let mut trash = mgr.trash.lock();
            trash.push(TrashEntry {
                credentials: cred,
                deleted_at: "2026-08-21T00:00:00+00:00".to_string(),
                success_count: 0,
                total_credits_used: 0.0,
                last_used_at: None,
            });
        }

        let guard = mgr.persist_lock.lock();
        let t1 = {
            let mgr = std::sync::Arc::clone(&mgr);
            std::thread::spawn(move || mgr.persist_trash())
        };
        {
            let mut trash = mgr.trash.lock();
            trash[0].success_count = 99;
        }
        drop(guard);
        t1.join().unwrap().unwrap();

        let trash_path = dir.join("trash.json");
        let raw = std::fs::read(&trash_path).expect("persist_trash 必须写出 trash.json");
        let back: Vec<TrashEntry> = serde_json::from_slice(&raw).expect("trash.json 必须是明文 JSON 数组");
        assert_eq!(
            back.len(),
            1,
            "排队落盘不得丢条目"
        );
        assert_eq!(
            back[0].success_count, 99,
            "排队中的 persist_trash 必须取到最新快照"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    // ===== 跨月配额自动恢复（P0）：recover_expired_quota_disables =====

    /// 固定参考时刻（2026-08-15，距 8 月 1 日 ≥ 12h，避开月初缓冲窗口）。
    ///
    /// 恢复判定依赖「当前时刻」，直接跑会受真实时间影响：真实时刻落在当月
    /// 1 日 12h 内时，月初缓冲会挡掉本应恢复的跨月号，测试随机红。
    /// 故全部恢复测试传固定 now（`recover_expired_quota_disables(Some(now))`）。
    fn fixed_recovery_now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-08-15T12:00:00Z")
            .expect("固定测试时刻恒合法")
            .with_timezone(&Utc)
    }

    /// 造一个「已禁用 + QuotaExceeded」的凭据（quota_exhausted_at 可指定）。
    fn mk_quota_exhausted_cred(id: u64, quota_ts: Option<&str>) -> KiroCredentials {
        let mut c = KiroCredentials::default();
        c.id = Some(id);
        c.access_token = Some(format!("tok-{id}"));
        c.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());
        c.disabled = true;
        c.disabled_reason = Some(DisabledReason::QuotaExceeded);
        c.quota_exhausted_at = quota_ts.map(|s| s.to_string());
        c
    }

    /// 把已构造 manager 里的某凭据重置为「QuotaExceeded 禁用」态。
    ///
    /// ⚠️ `MultiTokenManager::new` 内部会跑启动期 `recover_expired_quota_disables`
    /// （跨月/缺失时间戳的号当场已被复活），所以测试不能在构造参数里放旧时间戳
    /// 指望它保持禁用——必须在 new 之后显式重置，再对恢复函数做断言。
    fn re_set_quota_disabled(manager: &MultiTokenManager, id: u64, quota_ts: Option<&str>) {
        let mut entries = manager.entries.lock();
        let e = entries.iter_mut().find(|e| e.id == id).unwrap();
        e.disabled = true;
        e.disabled_reason = Some(DisabledReason::QuotaExceeded);
        e.quota_exhausted_at = quota_ts.map(|s| s.to_string());
        e.failure_count = 0;
    }

    /// 跨自然月回拨：quota_exhausted_at 在上月 → 自动恢复（禁用、原因、时刻、计数全清）。
    #[test]
    fn test_recover_expired_quota_disables_cross_month() {
        let now = fixed_recovery_now();
        // 40 天前必然落在上一个月（>31 天，跨月边界一定成立）。
        let prev_month = (now - Duration::days(40)).to_rfc3339();
        let manager = MultiTokenManager::new(
            Config::default(),
            vec![mk_quota_exhausted_cred(1, Some(&prev_month))],
            None,
            None,
            false,
        )
        .unwrap();
        // new() 内的启动恢复已把旧时间戳号复活，显式重置回禁用态再测。
        re_set_quota_disabled(&manager, 1, Some(&prev_month));
        // 预先打脏 failure_count，验证恢复会走 clear_transient_counters 收口清零。
        {
            let mut entries = manager.entries.lock();
            entries.iter_mut().find(|e| e.id == 1).unwrap().failure_count = 7;
        }

        assert_eq!(manager.recover_expired_quota_disables(Some(now)), 1, "应恢复 1 个");

        let entries = manager.entries.lock();
        let e = entries.iter().find(|e| e.id == 1).unwrap();
        assert!(!e.disabled, "恢复后必须回到可用态");
        assert_eq!(e.disabled_reason, None, "恢复后禁用原因必须清空");
        assert_eq!(e.quota_exhausted_at, None, "恢复后额度耗尽时刻必须清空");
        assert_eq!(e.failure_count, 0, "恢复后失败计数必须清零（clear_transient_counters）");
    }

    /// 当月禁用 → 不恢复（配额还没重置，复活只会白撞一次 402）。
    #[test]
    fn test_recover_expired_quota_disables_same_month_not_recovered() {
        let now = fixed_recovery_now();
        let now_ts = now.to_rfc3339();
        let manager = MultiTokenManager::new(
            Config::default(),
            vec![mk_quota_exhausted_cred(1, Some(&now_ts))],
            None,
            None,
            false,
        )
        .unwrap();

        assert_eq!(manager.recover_expired_quota_disables(Some(now)), 0, "当月不得恢复");

        let entries = manager.entries.lock();
        let e = entries.iter().find(|e| e.id == 1).unwrap();
        assert!(e.disabled, "当月额度耗尽的号必须保持禁用");
        assert_eq!(e.disabled_reason, Some(DisabledReason::QuotaExceeded));
    }

    /// 缺失时间戳（旧版本数据，未持久化 quota_exhausted_at）→ 视为可恢复，避免永久钉死。
    #[test]
    fn test_recover_expired_quota_disables_missing_timestamp_recovered() {
        let now = fixed_recovery_now();
        let manager = MultiTokenManager::new(
            Config::default(),
            vec![mk_quota_exhausted_cred(1, None)],
            None,
            None,
            false,
        )
        .unwrap();
        // new() 内的启动恢复已把缺失时间戳号复活，显式重置回禁用态再测。
        re_set_quota_disabled(&manager, 1, None);

        assert_eq!(manager.recover_expired_quota_disables(Some(now)), 1, "缺失时间戳必须可恢复");

        let entries = manager.entries.lock();
        let e = entries.iter().find(|e| e.id == 1).unwrap();
        assert!(!e.disabled);
        assert_eq!(e.disabled_reason, None);
    }

    /// 幂等 + 作用域：恢复后重复调用天然跳过；Manual 禁用、AccountSuspended、
    /// 当月 QuotaExceeded 一律不碰。
    #[test]
    fn test_recover_expired_quota_disables_idempotent_and_scoped() {
        let now = fixed_recovery_now();
        let prev_month = (now - Duration::days(40)).to_rfc3339();
        let now_ts = now.to_rfc3339();
        let mut manual = KiroCredentials::default();
        manual.id = Some(2);
        manual.access_token = Some("tok-2".to_string());
        manual.disabled = true;
        manual.disabled_reason = Some(DisabledReason::Manual);
        let mut suspended = KiroCredentials::default();
        suspended.id = Some(3);
        suspended.access_token = Some("tok-3".to_string());
        suspended.disabled = true;
        suspended.disabled_reason = Some(DisabledReason::AccountSuspended);

        let manager = MultiTokenManager::new(
            Config::default(),
            vec![
                mk_quota_exhausted_cred(1, Some(&prev_month)),
                mk_quota_exhausted_cred(4, Some(&now_ts)),
                manual,
                suspended,
            ],
            None,
            None,
            false,
        )
        .unwrap();
        // new() 内的启动恢复已把跨月的 #1 复活，显式重置回禁用态再测。
        re_set_quota_disabled(&manager, 1, Some(&prev_month));

        assert_eq!(manager.recover_expired_quota_disables(Some(now)), 1, "只有跨月的 #1 可恢复");
        // 幂等：恢复后 #1 已不再是 disabled+QuotaExceeded，重复调用天然跳过。
        assert_eq!(manager.recover_expired_quota_disables(Some(now)), 0, "重复调用必须幂等");

        let entries = manager.entries.lock();
        let e1 = entries.iter().find(|e| e.id == 1).unwrap();
        assert!(!e1.disabled, "#1 已恢复");
        let e4 = entries.iter().find(|e| e.id == 4).unwrap();
        assert!(e4.disabled, "#4 当月 QuotaExceeded 不得恢复");
        let e2 = entries.iter().find(|e| e.id == 2).unwrap();
        assert!(e2.disabled && e2.disabled_reason == Some(DisabledReason::Manual));
        let e3 = entries.iter().find(|e| e.id == 3).unwrap();
        assert!(e3.disabled && e3.disabled_reason == Some(DisabledReason::AccountSuspended));
    }

    /// 闭环：恢复 → 上游 402 → 重禁用（盖**当月** quota_exhausted_at）→ 当月不再恢复。
    ///
    /// 这是月初缓冲要防的整月失效场景：恢复判定若只在「月份不同」即放行，UTC 月初
    /// 恢复会白撞一次还没重置的 402 → 时间戳被盖成当月 → 后续整月判定同月永不恢复。
    #[test]
    fn test_quota_recover_402_redisable_closed_loop() {
        let now = fixed_recovery_now();
        let prev_month = (now - Duration::days(40)).to_rfc3339();
        let manager = MultiTokenManager::new(
            Config::default(),
            vec![mk_quota_exhausted_cred(1, Some(&prev_month))],
            None,
            None,
            false,
        )
        .unwrap();
        re_set_quota_disabled(&manager, 1, Some(&prev_month));

        // 跨月恢复成功（号回到可用态）。
        assert_eq!(manager.recover_expired_quota_disables(Some(now)), 1);
        assert!(!manager.entries.lock().iter().find(|e| e.id == 1).unwrap().disabled);

        // 模拟上游 402 → 立即重禁用并盖**当月**时间戳（等价 report_quota_exhausted 的状态；
        // 不用真实函数：它内部用 Utc::now() 盖戳，真实月份与固定 now 不一致时判定会漂）。
        let current_month_ts = now.to_rfc3339();
        re_set_quota_disabled(&manager, 1, Some(&current_month_ts));
        {
            let entries = manager.entries.lock();
            let e = entries.iter().find(|e| e.id == 1).unwrap();
            assert!(e.disabled && e.disabled_reason == Some(DisabledReason::QuotaExceeded));
        }

        // 当月内再次恢复 → 0（时间戳已是当月，不会无限循环；下月 1 日 +12h 后才放行）。
        assert_eq!(manager.recover_expired_quota_disables(Some(now)), 0, "当月不得再次恢复");
        assert!(
            manager.entries.lock().iter().find(|e| e.id == 1).unwrap().disabled,
            "号必须保持禁用，直到跨月"
        );
    }

    /// 月初缓冲：now 距当月 1 日 < 12h（如 UTC-8 上游的月首）→ 即使 quota 在上月也不恢复。
    #[test]
    fn test_quota_recovery_month_start_buffer_blocks() {
        // 2026-08-01T06:00Z：距 8 月 1 日 00:00 仅 6h（< 12h 缓冲），最坏时区差（UTC-8）
        // 下上游 8 月额度此时尚未重置，此刻恢复必白撞 402。
        let month_start_now = DateTime::parse_from_rfc3339("2026-08-01T06:00:00Z")
            .expect("固定测试时刻恒合法")
            .with_timezone(&Utc);
        let prev_month = (month_start_now - Duration::days(40)).to_rfc3339();
        let manager = MultiTokenManager::new(
            Config::default(),
            vec![mk_quota_exhausted_cred(1, Some(&prev_month))],
            None,
            None,
            false,
        )
        .unwrap();
        re_set_quota_disabled(&manager, 1, Some(&prev_month));

        assert_eq!(
            manager.recover_expired_quota_disables(Some(month_start_now)),
            0,
            "月初 12h 缓冲内不得恢复（即便时间戳在上月）"
        );
        let entries = manager.entries.lock();
        let e = entries.iter().find(|e| e.id == 1).unwrap();
        assert!(e.disabled, "缓冲期内号必须保持禁用");
        assert_eq!(e.quota_exhausted_at.as_deref(), Some(prev_month.as_str()), "时间戳不得被改动");
        drop(entries);

        // 同一时间戳，参考时刻离开月初窗口（8 月 15 日）→ 恢复放行（对照臂）。
        let later = fixed_recovery_now();
        assert_eq!(
            manager.recover_expired_quota_disables(Some(later)),
            1,
            "远离月初时同一时间戳必须可恢复"
        );
    }

    /// 持久化往返：quota_exhausted_at 必须穿过 serde（与 disabled_reason 同款契约），
    /// 否则重启后全部退化成「缺失时间戳 → 跨月恢复每次重启都重探一遍」。
    #[test]
    fn test_quota_exhausted_at_survives_persist_roundtrip() {
        let mut cred = KiroCredentials::default();
        cred.id = Some(7);
        cred.disabled = true;
        cred.disabled_reason = Some(DisabledReason::QuotaExceeded);
        cred.disabled_at = Some("2026-07-29T10:00:00+00:00".to_string());
        cred.quota_exhausted_at = Some("2026-07-29T10:05:00+00:00".to_string());

        let json = serde_json::to_string(&cred).expect("序列化");
        let back: KiroCredentials = serde_json::from_str(&json).expect("反序列化");

        assert_eq!(
            back.quota_exhausted_at.as_deref(),
            Some("2026-07-29T10:05:00+00:00"),
            "额度耗尽判定时刻必须穿过持久化往返（跨月恢复判据依赖它）"
        );
        assert_eq!(
            back.disabled_reason,
            Some(DisabledReason::QuotaExceeded),
            "禁用原因必须同往返保留"
        );
        // 线格式：camelCase 稳定命名 + None 不序列化（旧文件无该字段可读）。
        assert!(json.contains("\"quotaExhaustedAt\":"), "线格式应为 camelCase：{json}");
        let without_field: KiroCredentials =
            serde_json::from_str(r#"{"id":7,"disabled":true,"disabledReason":"quotaExceeded"}"#)
                .expect("旧格式（无 quotaExhaustedAt）必须仍可解析");
        assert_eq!(without_field.quota_exhausted_at, None);
    }

    /// 回归（G2 反饥饿强制探测 · 结构性兜底）：健康分最差的可选号也不得被永久排除。
    ///
    /// **旧代码为何失败**：`health_tier` 在排序键的高位，最差档号只在高档全部不可用时才被选到。
    /// 而健康分的上升路径依赖 `on_success` —— 拿不到请求就没有成功 → 永久留在最差档。
    /// 线上实测：6 号池 4 个号进 T2 且 `rpm=0 inflight=0` 空转，有效容量 6→3，全程零 429。
    ///
    /// 本测试用**两个号 + 极少轮次**保证对照干净：
    /// - `credential_rpm_limit` 设很大，避免多轮选号把 RPM 打饱和从而污染排序（那会让
    ///   两个号都变 `unusable`、测试失去区分度 —— 这正是本测试第一版失败的原因）；
    /// - 只跑 6 轮，远小于任何饱和阈值；
    /// - #1 打成"健康分最差 + 已饥饿"，#2 保持健康。
    ///
    /// 有反饥饿键时 #1 至少被探测到一次；把 `starved` 常量化（模拟旧代码）后 #1 恒 0 次。
    #[tokio::test]
    async fn test_starved_credential_is_force_probed() {
        let mut config = Config::default();
        config.load_balancing_mode = "balanced".to_string();
        config.affinity_enabled = false;
        // 关键：抬高 RPM 容量，确保 6 轮选号绝不触发饱和（饱和会让两个号都 unusable）
        config.credential_rpm_limit = 100_000;
        let creds: Vec<KiroCredentials> = (0..2)
            .map(|i| {
                let mut c = KiroCredentials::default();
                c.priority = 0;
                c.access_token = Some(format!("tok-{i}"));
                c.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());
                c
            })
            .collect();
        let manager = MultiTokenManager::new(config, creds, None, None, false).unwrap();

        // #1 健康分打到最差档（连续 429），但**不让它熔断沉底**：
        // 熔断 Open 会让 p_avail=0 → unusable=1 → 本就该沉底，那不是饥饿场景。
        // 故只打到 TRIP_THRESHOLD-1 次，保持 Closed 但健康分很低。
        let fam1 = manager.family_key_of(1);
        for _ in 0..(crate::kiro::health::TRIP_THRESHOLD - 1) {
            manager.health.on_429(&fam1);
        }
        {
            let snap = manager.health.snapshot(&fam1).unwrap();
            assert!(!snap.circuit_open, "前置：#1 不应熔断（否则测的不是饥饿）");
            assert!(
                snap.health < crate::kiro::health::HEALTH_TIER_HEALTHY_MIN,
                "前置：#1 应已跌出健康档，实际 {}",
                snap.health
            );
        }
        // 让 #1 处于"已饥饿"（上次被选中远在探测窗口外）
        {
            let entries = manager.entries.lock();
            let e1 = entries.iter().find(|e| e.id == 1).unwrap();
            e1.last_selected_at
                .set(Instant::now() - StdDuration::from_secs(STARVATION_PROBE_SECS + 60));
        }

        let mut hit_1 = 0usize;
        for _ in 0..6 {
            let g = manager.acquire_context(None, None).await.unwrap();
            if g.id == 1 {
                hit_1 += 1;
            }
            drop(g);
        }
        assert!(
            hit_1 > 0,
            "健康分最差但仍可选的饥饿号必须被强制探测到（旧代码 0 次），实际 {hit_1} 次"
        );
    }

    /// 回归（低负载分流偏斜 · 本轮修复）：请求归零后再选，不得恒偏向同一个号。
    ///
    /// **旧代码为何失败**：低负载下排序键的前几位会**全部平局** —— 全池健康（①②③ 恒等）、
    /// 每个请求处理完 inflight 即归零（④⑤ 恒 0）、`rpm_usage_permille` 用 60s 滑窗且
    /// 过期即归零（⑥ 追不上）。而 `min_by_key` 全平局时**恒返回第一个元素**，于是流量
    /// 持续偏向 `entries` 里下标靠前的号。
    ///
    /// 线上 6 号池实测（52 次请求全部成功、无坏号）：gini **0.378**、最热/最冷 **6.67x**，
    /// idx0 的 #208 拿 20 次而 idx5 的 #213 只拿 3 次。
    ///
    /// 本测试每次取号后**立即 drop guard**（inflight 归零），复现那个"每次都从全平局
    /// 开始"的低负载场景。旧代码下会 100% 选中 id=1，`distinct == 1` → 断言失败。
    #[tokio::test]
    async fn test_low_load_selection_does_not_always_pick_first_entry() {
        let manager = make_balanced_manager(6);
        // 同优先级才是真并列（make_balanced_manager 给的是 0..n）
        {
            let mut entries = manager.entries.lock();
            for e in entries.iter_mut() {
                e.credentials.priority = 0;
            }
        }
        let mut hits: std::collections::HashMap<u64, usize> = std::collections::HashMap::new();
        for _ in 0..120 {
            let g = manager.acquire_context(None, None).await.unwrap();
            *hits.entry(g.id).or_default() += 1;
            drop(g); // 立即归零 inflight —— 关键：让下一轮又从"全平局"开始
        }
        let distinct = hits.len();
        assert!(
            distinct >= 5,
            "6 号池 120 次低负载选号应铺开到几乎所有号，实际只用了 {distinct} 个：{hits:?}\
             （旧代码恒选下标最小的号，distinct==1）"
        );
        // 最热号不应垄断：完美均分是 20 次/号，给足容差但要挡住 6.67x 那种偏斜。
        let max = *hits.values().max().unwrap();
        assert!(max <= 60, "最热号拿了 {max}/120 次，偏斜过大：{hits:?}");
    }

    /// 余额加权作为末位 tie-break 生效:两号同优先级/同健康/同在途 0/同 RPM 时,余额多的先选。
    /// 旧代码无余额概念,两号完全并列靠 id 兜底(选 #1);新代码 #2 余额满、#1 半额 → 首取应是 #2。
    #[tokio::test]
    async fn test_balance_weight_breaks_tie_toward_richer() {
        let manager = make_balanced_manager(2);
        // 同优先级(make_balanced 给的是 0/1,改成都 0 才是真并列)——直接改 entries 优先级。
        {
            let mut entries = manager.entries.lock();
            for e in entries.iter_mut() {
                e.credentials.priority = 0;
            }
        }
        // #1 半额(factor 0.75),#2 满额(factor 1.0)。其余全并列(在途 0、RPM 0、健康满)。
        manager.set_balance_snapshots(HashMap::from([
            (1, mk_bal_snap(50.0, 100.0, 0.0)),
            (2, mk_bal_snap(100.0, 100.0, 0.0)),
        ]));
        // 首取(此刻两号在途都 0)→ 余额是唯一区分键 → 选余额多的 #2。
        let g = manager.acquire_context(None, None).await.unwrap();
        assert_eq!(
            g.id, 2,
            "全并列时余额多的 #2 应先选(余额末位 tie-break),实际 #{}",
            g.id
        );
    }

    /// 余额加权是软偏置微调,**不掀翻在途分流**:#1 余额耗尽但在途少、#2 满额但在途多,
    /// 仍先选在途少的 #1(在途是第⑦位主键,余额只在第⑪位精细兜底)。证明不打架 0.7.23。
    #[tokio::test]
    async fn test_balance_weight_does_not_override_inflight() {
        let manager = make_balanced_manager(2);
        {
            let mut entries = manager.entries.lock();
            for e in entries.iter_mut() {
                e.credentials.priority = 0;
            }
        }
        // #1 余额耗尽(factor 0.5),#2 满额(factor 1.0)。
        manager.set_balance_snapshots(HashMap::from([
            (1, mk_bal_snap(0.0, 100.0, 0.0)),
            (2, mk_bal_snap(100.0, 100.0, 0.0)),
        ]));
        // 给 #2 压 3 个在途(直接改 inflight 原子,不走选号避免污染)。
        {
            let entries = manager.entries.lock();
            let e2 = entries.iter().find(|e| e.id == 2).unwrap();
            for _ in 0..3 {
                e2.inflight.fetch_add(1, Ordering::Release);
            }
        }
        // 选号:#1 在途 0、#2 在途 3。在途(第4位主键)先决 → 选 #1,尽管 #1 余额因子更低。
        let g = manager.acquire_context(None, None).await.unwrap();
        assert_eq!(
            g.id, 1,
            "在途少的 #1 应先选(在途主键压过余额末位键),实际 #{}",
            g.id
        );
    }

    /// T1(L2 回归,旧代码必挂):健康不对称下同优先级仍按负载分流,不被最健康的号吸走整轮。
    /// 旧代码 neg_p_bucket 首排:健康分高的号哪怕在途多也压过空闲的稍弱号 → 突发全扑一个号。
    /// L2 后:健康降 3 档粗门,同档内在途最少优先 → 3 个并发在途分摊到不同号。
    #[tokio::test]
    async fn test_l2_health_asymmetric_still_spreads() {
        let manager = make_balanced_manager(3);
        // 造同档内 p_avail 不对称:#1 满血(p=1.0),#2/#3 各记 3 次近窗 RPM 制造轻微 rpm_pressure
        // (cap 默认 25,pressure=3/25=0.12 → p≈0.88),三者都在 healthy 档(≥0.75,tier 同为 0)。
        // 旧代码:neg_p_bucket 首要连续键,#1 的 p 最高;每 +1 在途只压 ~6 分,压不过 #1 对 #2/#3 的
        // 健康优势 → 3 个并发全吸到 #1(惊群)。L2:同档内在途最少优先 → 分摊到 3 个号。
        manager.rpm.record(2);
        manager.rpm.record(2);
        manager.rpm.record(2);
        manager.rpm.record(3);
        manager.rpm.record(3);
        manager.rpm.record(3);
        // 连取 3 个 context 不释放(guard 持有 = 模拟 3 个并发在途)。
        let a = manager.acquire_context(None, None).await.unwrap();
        let b = manager.acquire_context(None, None).await.unwrap();
        let c = manager.acquire_context(None, None).await.unwrap();
        let mut ids = [a.id, b.id, c.id];
        ids.sort();
        assert_eq!(
            ids,
            [1, 2, 3],
            "3 个并发在途应分摊到 3 个不同号(同档内在途最少优先,不吸附到最健康的 #1),实际 {:?}",
            ids
        );
    }

    /// T2(L4 硬门回归,旧代码必挂):常载下 RPM 不越限——两趟选号只在非饱和候选里选。
    /// 旧代码饱和只是软降权,整池未满时也可能把 burst 拍到排序靠前的号使其越限;L4 硬门过滤饱和号。
    #[tokio::test]
    async fn test_l4_hard_gate_no_overshoot() {
        let mut config = Config::default();
        config.load_balancing_mode = "balanced".to_string();
        config.affinity_enabled = false;
        config.credential_rpm_limit = 3;
        config.rpm_headroom_factor = 100; // 隔离 headroom,只验硬门:阈值恰为 3
        let creds: Vec<KiroCredentials> = (1..=2)
            .map(|i| {
                let mut c = KiroCredentials::default();
                c.priority = 0;
                c.access_token = Some(format!("tok-{}", i));
                c.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());
                c
            })
            .collect();
        let manager = MultiTokenManager::new(config, creds, None, None, false).unwrap();

        // 取 6 次(=2 号 × 3),每次释放 guard(只累积 rpm.record)。硬门保证不越各号 limit=3。
        for _ in 0..6 {
            let ctx = manager.acquire_context(None, None).await.unwrap();
            drop(ctx);
        }
        let snap = manager.snapshot();
        for e in &snap.entries {
            assert!(
                manager.rpm.count(e.id) <= 3,
                "L4 硬门:#{} 近窗 RPM {} 不应越过 limit=3",
                e.id,
                manager.rpm.count(e.id)
            );
        }
        // 总量恰好 6(2×3),证明两号都被用满而非一个越限、一个闲置。
        let total: u32 = snap.entries.iter().map(|e| manager.rpm.count(e.id)).sum();
        assert_eq!(total, 6, "两号各 3 次共 6,负载均摊到容量");
    }

    /// T2b(L4 背压路径):背压开 + 整池 RPM 饱和时,transient_wait_outcome 返回
    /// Wait(≤60s, RpmRecovery),使 acquire_context 等待 RPM 恢复而非误判"所有凭据均已禁用"bail。
    /// 背压关时饱和号算立即可用候选 → Available(不等待,保持默认行为)。
    #[tokio::test]
    async fn test_l4_backpressure_waits_on_rpm_saturation() {
        let mut config = Config::default();
        config.load_balancing_mode = "balanced".to_string();
        config.affinity_enabled = false;
        config.credential_rpm_limit = 2;
        config.rpm_headroom_factor = 100;
        config.rpm_hard_gate_overload_wait = true; // 开背压
        let mut c = KiroCredentials::default();
        c.access_token = Some("tok-1".to_string());
        c.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());
        let manager = MultiTokenManager::new(config, vec![c], None, None, false).unwrap();

        // 打到饱和(limit=2)。
        manager.rpm.record(1);
        manager.rpm.record(1);
        assert!(manager.is_rpm_saturated_with_limit(1, None), "应已饱和");
        // 背压开:该号是"将恢复的等待候选",返回 Wait(≤60s, RpmRecovery)——原因是 RPM 恢复而非冷却,
        // 调用方据此绝不 cooling-fast-fail、绝不报"已禁用"(D1 修复核心)。
        match manager.transient_wait_outcome(None) {
            WaitOutcome::Wait(d, reason) => {
                assert_eq!(
                    reason,
                    WaitReason::RpmRecovery,
                    "饱和的等待原因应是 RpmRecovery 而非 Cooling"
                );
                assert!(
                    d <= StdDuration::from_secs(60),
                    "恢复窗口不超过 60s 窗口长度"
                );
            }
            other => panic!("背压开 + 饱和应返回 Wait(RpmRecovery),实际 {:?}", other),
        }

        // 对照:背压关时,饱和号仍算立即可用候选 → Available(不等待,保持默认行为)。
        manager
            .rpm_hard_gate_overload_wait
            .store(false, Ordering::Relaxed);
        assert_eq!(
            manager.transient_wait_outcome(None),
            WaitOutcome::Available,
            "背压关:饱和号是立即可选候选,不等待(保持默认行为)"
        );
    }

    /// D1 回归(旧代码上会失败):RPM 饱和的等待原因是 RpmRecovery(非 Cooling),确保调用方不会把
    /// "整池 RPM 饱和"误当"全在冷却"而 cooling-fast-fail、也不会因去饱和竞态误报"已禁用"。
    /// 旧实现 transient_wait_duration 返回无类型 Option<Duration>,冷却/RPM 饱和不可区分——本测坐实已分型。
    #[tokio::test]
    async fn test_l4_backpressure_wait_reason_is_rpm_not_cooling() {
        let mut config = Config::default();
        config.load_balancing_mode = "balanced".to_string();
        config.affinity_enabled = false;
        config.credential_rpm_limit = 2;
        config.rpm_headroom_factor = 100;
        config.rpm_hard_gate_overload_wait = true;
        config.cooldown_enabled = true; // 冷却开着,但该号没被冷却——只是 RPM 饱和
        let mut c = KiroCredentials::default();
        c.access_token = Some("tok-1".to_string());
        c.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());
        let manager = MultiTokenManager::new(config, vec![c], None, None, false).unwrap();
        manager.rpm.record(1);
        manager.rpm.record(1);
        // 无冷却记录 → 若原因被误判为 Cooling,长窗口下会 cooling-fast-fail;分型后必为 RpmRecovery。
        assert!(
            matches!(
                manager.transient_wait_outcome(None),
                WaitOutcome::Wait(_, WaitReason::RpmRecovery)
            ),
            "未冷却、仅 RPM 饱和 → 等待原因必须是 RpmRecovery(不得误当 Cooling)"
        );
    }

    // ===== TIER2 配置热重载：后台任务 abort+respawn 回归 =====

    /// 造一个可控 proactive_token_refresh 的单号 manager（带有效 token）。
    fn make_manager_with_proactive(proactive: bool) -> Arc<MultiTokenManager> {
        let mut config = Config::default();
        config.proactive_token_refresh = proactive;
        config.token_refresh_interval_secs = 5;
        let mut c = KiroCredentials::default();
        c.priority = 0;
        c.access_token = Some("tok-0".to_string());
        c.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());
        Arc::new(MultiTokenManager::new(config, vec![c], None, None, false).unwrap())
    }

    #[tokio::test]
    async fn test_respawn_refresh_task_disabled_stores_no_handle() {
        // proactive=false：respawn 后任务槽应为空（不起后台任务）
        let mgr = make_manager_with_proactive(false);
        mgr.respawn_refresh_task();
        assert!(
            mgr.refresh_task.lock().is_none(),
            "proactive_token_refresh=false 时不应存在任务句柄"
        );
    }

    #[tokio::test]
    async fn test_respawn_refresh_task_enabled_stores_handle() {
        // proactive=true：respawn 后任务槽应持有一个运行中的句柄
        let mgr = make_manager_with_proactive(true);
        mgr.respawn_refresh_task();
        let slot = mgr.refresh_task.lock();
        let handle = slot.as_ref().expect("proactive=true 应存在任务句柄");
        assert!(!handle.is_finished(), "新起的预刷新任务应在运行中");
    }

    #[tokio::test]
    async fn test_respawn_refresh_task_idempotent_aborts_old() {
        // 幂等：重复 respawn 应 abort 旧任务、只保留一个新句柄（不泄漏累积）
        let mgr = make_manager_with_proactive(true);
        mgr.respawn_refresh_task();
        // 取出旧句柄的克隆引用用于观测（AbortHandle 不便克隆，改为记录 abort 后 is_finished）
        let old_finished_before = {
            let slot = mgr.refresh_task.lock();
            slot.as_ref().unwrap().is_finished()
        };
        assert!(!old_finished_before, "第一次 respawn 的任务应在运行");

        // 第二次 respawn：内部会 abort 旧任务并换新句柄
        mgr.respawn_refresh_task();
        // 让被 abort 的旧任务有机会真正结束
        tokio::task::yield_now().await;
        let slot = mgr.refresh_task.lock();
        let handle = slot.as_ref().expect("重挂后应仍有一个任务句柄");
        assert!(!handle.is_finished(), "重挂后的新任务应在运行中");
    }

    #[tokio::test]
    async fn test_respawn_refresh_task_toggle_off_aborts() {
        // 开→关：先起任务，再把 proactive 改 false 并 respawn，句柄应清空
        let mgr = make_manager_with_proactive(true);
        mgr.respawn_refresh_task();
        assert!(mgr.refresh_task.lock().is_some(), "开启后应有句柄");

        // 原子换成关闭态的 config（模拟 reload_config 后再 respawn）
        let mut off = (*mgr.config()).clone();
        off.proactive_token_refresh = false;
        mgr.config.store(Arc::new(off));
        mgr.respawn_refresh_task();
        assert!(
            mgr.refresh_task.lock().is_none(),
            "关闭 proactive 后 respawn 应清空任务句柄"
        );
    }

    #[tokio::test]
    async fn test_inflight_spreads_concurrent_load_no_thundering_herd() {
        // 惊群回归：持有多个未完成请求的上下文（guard 未 Drop）时，
        // balanced 选号必须把后续请求分摊到不同的号，而不是全部扑向同一个。
        let manager = make_balanced_manager(3);

        // 连续获取 3 个上下文且都不释放（模拟 3 个并发在途请求）
        let c1 = manager.acquire_context(None, None).await.unwrap();
        let c2 = manager.acquire_context(None, None).await.unwrap();
        let c3 = manager.acquire_context(None, None).await.unwrap();

        // 三个在途请求应分别落在 3 个不同的凭据上（inflight 升序天然分摊）
        let mut ids = [c1.id, c2.id, c3.id];
        ids.sort_unstable();
        assert_eq!(
            ids,
            [1, 2, 3],
            "3 个并发在途请求应分摊到 3 个不同的号，实际 {:?}",
            ids
        );
    }

    #[tokio::test]
    async fn test_inflight_guard_release_frees_credential_for_reuse() {
        // 单个号：拿到上下文后 inflight=1，释放后归零，可被再次选中且负载记账正确
        let manager = make_balanced_manager(1);

        {
            let _ctx = manager.acquire_context(None, None).await.unwrap();
            let snap = manager.snapshot();
            let e = snap.entries.iter().find(|e| e.id == 1).unwrap();
            assert_eq!(e.inflight, 1, "持有上下文时 inflight 应为 1");
        }
        // 上下文出作用域 → guard Drop → inflight -1
        let snap = manager.snapshot();
        let e = snap.entries.iter().find(|e| e.id == 1).unwrap();
        assert_eq!(e.inflight, 0, "释放后 inflight 应归零");
    }

    #[tokio::test]
    async fn test_inflight_prefers_least_loaded_after_releases() {
        // 先让 #1 背上 2 个未完成请求，再取一次：应避开 #1，选到空闲的号
        let manager = make_balanced_manager(2);

        // 手动制造 #1 高在途：直接对其计数器加压（等价于两个未完成请求都落在 #1）
        // 通过连续 acquire 并保留：第一次可能落 #1 或 #2，用显式方式验证升序即可
        let held_a = manager.acquire_context(None, None).await.unwrap();
        let first_id = held_a.id;
        let held_b = manager.acquire_context(None, None).await.unwrap();
        let second_id = held_b.id;
        // 两个在途分属不同号
        assert_ne!(first_id, second_id);

        // 释放第二个号的请求 → 它变回空闲；下一次选号应命中刚释放的那个
        drop(held_b);
        let next = manager.acquire_context(None, None).await.unwrap();
        assert_eq!(
            next.id, second_id,
            "释放后应优先选回在途最少（=0）的号 #{}，实际 #{}",
            second_id, next.id
        );
    }

    #[tokio::test]
    async fn test_balanced_spreads_by_recent_rpm_not_lifetime_success() {
        // ⭐分流回归：balanced 应按**近 60s RPM**（即时负载）均衡分摊，而非终身 success_count。
        // 线上真实症状：#53/#54 终身 6000+、#56/#58/#59 终身几百，若按终身计数选号会持续
        // 只灌新号（把负载集中在 1-2 个号，老号闲置=部分号"不动"，且单号 RPM 高触发风控）。
        // 正确行为：串行放号应轮流命中不同的号（每次都选近窗 RPM 最少者），负载均匀铺开。
        let manager = make_balanced_manager(3);

        // 模拟 #1 已被大量使用（终身成功数很高），但当前窗口无负载。
        // 用 rpm.record 制造近窗负载差异，验证选号看的是"当下 RPM"而非"终身总量"。
        // 先给 #1 记 3 次近窗命中（当前最忙），#2 记 1 次，#3 记 0 次。
        manager.rpm.record(1);
        manager.rpm.record(1);
        manager.rpm.record(1);
        manager.rpm.record(2);

        // 立即放号（不保留在途，避免 inflight 干扰）：应选近窗 RPM 最少的 #3。
        let c = manager.acquire_context(None, None).await.unwrap();
        assert_eq!(
            c.id, 3,
            "应选近 60s RPM 最少的 #3（0 次），而非按终身或优先级，实际 #{}",
            c.id
        );
        drop(c); // 释放在途；此次放号已给 #3 记了 1 次 RPM（commit_selection），#3 现为 1 次

        // 现在窗口负载：#1=3、#2=1、#3=1。再放一次应命中 #2 或 #3（并列最少=1），绝不选最忙的 #1。
        let c2 = manager.acquire_context(None, None).await.unwrap();
        assert_ne!(
            c2.id, 1,
            "最忙的 #1（近窗 3 次）不应被选中，实际 #{}",
            c2.id
        );
    }

    /// 回归（newapi「首选凭据组」的软版）：模型在**某号白名单里显式列出**时，该号优先
    /// 于未设白名单的「通吃号」；白名单外的模型仍能落到通吃号（硬门放行 + 无白名单命中）。
    ///
    /// **旧代码为何 FAIL**：白名单此前只是 `is_entry_selectable` 的硬门（设了才过滤），
    /// 不参与排序 —— 显式路由的号与通吃号在排序键里完全并列，同优先级下平局恒选
    /// 下标最小的号，白名单配置对分流零影响，无法实现「这个模型优先走这些号」。
    #[tokio::test]
    async fn test_whitelisted_model_channel_preferred_over_catch_all() {
        let manager = make_balanced_manager(2);
        {
            let mut entries = manager.entries.lock();
            for e in entries.iter_mut() {
                e.credentials.priority = 0;
                if e.id == 1 {
                    // #1 显式白名单含该模型（显式路由），#2 未设白名单（通吃）。
                    e.credentials.allowed_models = Some(vec!["claude-sonnet-4-5".to_string()]);
                }
            }
        }
        // 其余全并列（同优先级/同健康/在途 0/RPM 0/模型级 0）→ 白名单命中的 #1 应被首选。
        let g = manager.acquire_context(Some("claude-sonnet-4-5"), None).await.unwrap();
        assert_eq!(
            g.id, 1,
            "白名单显式列出的 #1 应优先于通吃号 #2（显式路由软因子），实际 #{}",
            g.id
        );
        drop(g);
        // 白名单外的模型：#1 被硬门挡下（白名单不含它），#2 通吃放行 → 仍能选到 #2，
        // 显式路由不能把白名单外的模型堵死。
        let g2 = manager.acquire_context(Some("claude-opus-4-8"), None).await.unwrap();
        assert_eq!(
            g2.id, 2,
            "白名单外模型应落到通吃号（硬门放行 + 无白名单命中），实际 #{}",
            g2.id
        );
    }

    /// 回归（模型级 RPM 分流）：同一模型正在猛灌某号时，应优先选**该模型近期调用数少**的号，
    /// 把爆款模型摊到整池，防单模型把部分号顶到饱和而其它号平局分不到。
    ///
    /// **旧代码为何 FAIL**：排序键只有每凭据 RPM / inflight —— 模型级计数为零差异时，
    /// 两号在途同为 0 即全平局，恒选下标最小的号；爆款模型连续打同一号直到 RPM 饱和，
    /// 期间其它号即使该模型零调用也不参与分流。
    #[tokio::test]
    async fn test_model_rpm_spreads_hot_model_across_pool() {
        let manager = make_balanced_manager(2);
        {
            let mut entries = manager.entries.lock();
            for e in entries.iter_mut() {
                e.credentials.priority = 0;
            }
        }
        // 模拟爆款模型正在猛灌 #1（5 次模型级命中），#2 该模型零调用；两者每凭据 RPM 都为 0。
        manager.rpm.record_model(1, "claude-sonnet-4-5");
        manager.rpm.record_model(1, "claude-sonnet-4-5");
        manager.rpm.record_model(1, "claude-sonnet-4-5");
        manager.rpm.record_model(1, "claude-sonnet-4-5");
        manager.rpm.record_model(1, "claude-sonnet-4-5");
        let g = manager.acquire_context(Some("claude-sonnet-4-5"), None).await.unwrap();
        assert_eq!(
            g.id, 2,
            "该模型近期调用少的 #2 应优先（把爆款模型摊到整池），实际 #{}",
            g.id
        );
        drop(g);
        // 另一模型无近期调用差异 → 回到常规分流：每凭据 RPM 少的 #1（0 次 vs #2 的 1 次）。
        let g2 = manager.acquire_context(Some("claude-opus-4-8"), None).await.unwrap();
        assert_eq!(
            g2.id, 1,
            "另一模型无近期调用差异时走常规 RPM 分流，实际 #{}",
            g2.id
        );
    }

    #[tokio::test]
    async fn test_rpm_saturation_deprioritizes_credential() {
        // 配置 RPM 软上限=2：把 #1 打到饱和后，选号应降权 #1、优先未饱和的 #2
        let mut config = Config::default();
        config.load_balancing_mode = "balanced".to_string();
        config.affinity_enabled = false;
        config.credential_rpm_limit = 2;

        let mut c1 = KiroCredentials::default();
        c1.priority = 0; // 优先级更高，若无 RPM 降权会被优先选中
        c1.access_token = Some("tok1".to_string());
        c1.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());
        let mut c2 = KiroCredentials::default();
        c2.priority = 1;
        c2.access_token = Some("tok2".to_string());
        c2.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());

        let manager = MultiTokenManager::new(config, vec![c1, c2], None, None, false).unwrap();

        // 把 #1 的 RPM 打到软上限（record 2 次），并立即释放在途避免 inflight 干扰
        manager.rpm.record(1);
        manager.rpm.record(1);
        assert!(manager.is_rpm_saturated_with_limit(1, None), "#1 应已 RPM 饱和");
        assert!(!manager.is_rpm_saturated_with_limit(2, None), "#2 未饱和");

        // 选号：#1 虽优先级更高，但 RPM 饱和被降权 → 应选未饱和的 #2
        let ctx = manager.acquire_context(None, None).await.unwrap();
        assert_eq!(ctx.id, 2, "RPM 饱和的 #1 应被降权，改选未饱和的 #2");
    }

    #[tokio::test]
    async fn test_per_credential_rpm_capacity_overrides_global() {
        // per-cred rpm_limit 覆盖全局:#1 设自己的容量 5(体质好),全局是 2。
        // 打 3 次 RPM:按全局(2)会饱和,但 #1 自己容量 5 未到 → 不饱和。
        let mut config = Config::default();
        config.load_balancing_mode = "balanced".to_string();
        config.affinity_enabled = false;
        config.credential_rpm_limit = 2; // 全局软上限 2

        let mut c1 = KiroCredentials::default();
        c1.priority = 0;
        c1.rpm_limit = Some(5); // 本号容量 5,高于全局
        c1.access_token = Some("tok1".to_string());
        c1.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());
        let mut c2 = KiroCredentials::default();
        c2.priority = 1;
        c2.access_token = Some("tok2".to_string()); // 无 per-cred,用全局 2
        c2.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());

        let manager = MultiTokenManager::new(config, vec![c1, c2], None, None, false).unwrap();

        // #1 打 3 次:全局阈值 2 会判饱和,但 #1 自己容量 5 → 不饱和
        manager.rpm.record(1);
        manager.rpm.record(1);
        manager.rpm.record(1);
        assert!(
            !manager.is_rpm_saturated_with_limit(1, Some(5)),
            "#1 有 per-cred 容量 5,打 3 次不应饱和"
        );
        // #2 无 per-cred,用全局 2:打 2 次即饱和
        manager.rpm.record(2);
        manager.rpm.record(2);
        assert!(manager.is_rpm_saturated_with_limit(2, None), "#2 用全局容量 2,打 2 次应饱和");
    }

    #[tokio::test]
    async fn test_affinity_sticks_session_to_same_credential_in_balanced() {
        // balanced 模式下，同一 session 的连续请求应粘在同一凭据上
        let mut config = Config::default();
        config.load_balancing_mode = "balanced".to_string();
        config.affinity_enabled = true;

        let mut c1 = KiroCredentials::default();
        c1.priority = 0;
        c1.access_token = Some("tok1".to_string());
        c1.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());
        let mut c2 = KiroCredentials::default();
        c2.priority = 1;
        c2.access_token = Some("tok2".to_string());
        c2.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());

        let manager = MultiTokenManager::new(config, vec![c1, c2], None, None, false).unwrap();

        // 首次请求绑定某凭据
        let first = manager
            .acquire_context(None, Some("session-A"))
            .await
            .unwrap();
        let bound = first.id;
        drop(first);
        // 同会话后续多次请求应始终命中同一凭据，即便 balanced 的 least-used 会倾向另一个
        for _ in 0..5 {
            let ctx = manager
                .acquire_context(None, Some("session-A"))
                .await
                .unwrap();
            assert_eq!(ctx.id, bound, "同会话应粘在同一凭据");
        }
    }

    #[tokio::test]
    async fn test_affinity_spills_to_idle_when_bound_saturated() {
        // 亲和绑定号 RPM 饱和时不再死粘,改走 balanced 分流到空闲号(retry 慢根因的修复)。
        let mut config = Config::default();
        config.load_balancing_mode = "balanced".to_string();
        config.affinity_enabled = true;
        config.credential_rpm_limit = 3; // 显式软上限 3,便于打饱和

        let mut c1 = KiroCredentials::default();
        c1.priority = 0;
        c1.access_token = Some("tok1".to_string());
        c1.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());
        let mut c2 = KiroCredentials::default();
        c2.priority = 1;
        c2.access_token = Some("tok2".to_string());
        c2.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());
        let manager = MultiTokenManager::new(config, vec![c1, c2], None, None, false).unwrap();

        // session-A 首次绑定某号
        let first = manager
            .acquire_context(None, Some("session-A"))
            .await
            .unwrap();
        let bound = first.id;
        drop(first);
        // 把绑定号打到 RPM 饱和(软上限 3)
        for _ in 0..3 {
            manager.rpm.record(bound);
        }
        assert!(manager.is_rpm_saturated_with_limit(bound, None), "绑定号应已饱和");
        // 同会话再来:绑定号饱和 → 应溢出到另一个空闲号,而非死粘饱和号
        let ctx = manager
            .acquire_context(None, Some("session-A"))
            .await
            .unwrap();
        assert_ne!(ctx.id, bound, "绑定号饱和时应溢出到空闲号,不再死粘");
    }

    #[tokio::test]
    async fn test_default_saturation_fallback_spreads_load() {
        // 默认配置(credential_rpm_limit=0 未设)也要最优:回退高水位 30 判饱和,不再恒不饱和。
        // L3 headroom 默认 factor=85 → 兜底 30 打折为 floor(30×0.85)=25,饱和阈值提前到 25(留 15% 缓冲)。
        let config = Config::default(); // credential_rpm_limit=0, rpm_headroom_factor=85
        let mut c1 = KiroCredentials::default();
        c1.access_token = Some("tok1".to_string());
        c1.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());
        let manager = MultiTokenManager::new(config, vec![c1], None, None, false).unwrap();
        // 默认兜底 30 × 0.85 = 25:打 24 次不饱和,第 25 次达 headroom 后阈值饱和。
        for _ in 0..24 {
            manager.rpm.record(1);
        }
        assert!(
            !manager.is_rpm_saturated_with_limit(1, None),
            "默认兜底 30×0.85=25,打 24 次不应饱和"
        );
        manager.rpm.record(1);
        assert!(
            manager.is_rpm_saturated_with_limit(1, None),
            "打到 25(headroom 后阈值)应触发饱和"
        );
    }

    // ============ rpm_saturation_gate_active(虚假饱和告警修复,旧代码没有此函数,
    // 下面的"旧行为"用等价断言复现) ============

    /// ⭐⭐ 调度归一化回归（**旧代码必失败**）：priority 模式下 RPM 饱和硬门**必须真正生效**，
    /// 饱和的高优先级号必须优雅溢出到下一优先级层。
    ///
    /// 本测试的前身断言的是相反的事（"priority 模式下饱和不影响选号，仍选 #1"）——那记录的是
    /// 归一化前的**缺陷行为**：priority 分支只有 `min_by_key(|e| e.credentials.priority)` 一行，
    /// 不看饱和/熔断/inflight，于是 #1 被打爆后流量仍全压在它身上，旁边空闲的 #2 一个都接不到。
    /// 归一化（priority ≡ balanced + priority_in_balanced=true）后，priority 语义保留为
    /// "按优先级**分层**"，但排序键第①位 `unusable`（含饱和判定）会让打爆的整层沉底 → 溢出到 #2。
    #[tokio::test]
    async fn test_priority_mode_saturated_credential_spills_over() {
        let mut config = Config::default();
        config.load_balancing_mode = "priority".to_string(); // 出厂默认值，写明意图
        config.affinity_enabled = false;
        config.credential_rpm_limit = 2;
        config.rpm_headroom_factor = 100; // 隔离 headroom，让阈值恰为 2

        let mut c1 = KiroCredentials::default();
        c1.priority = 0; // 高优先级
        c1.access_token = Some("tok1".to_string());
        c1.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());
        let mut c2 = KiroCredentials::default();
        c2.priority = 1; // 低优先级
        c2.access_token = Some("tok2".to_string());
        c2.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());

        let manager = MultiTokenManager::new(config, vec![c1, c2], None, None, false).unwrap();

        // 把 #1 打到远超阈值 → 饱和
        for _ in 0..6 {
            manager.rpm.record(1);
        }
        assert!(manager.is_rpm_saturated_with_limit(1, None), "前置条件:#1 已超过软上限");

        // 归一化后硬门在两种模式下都生效（面板据此报告 rpmSaturated 才与调度一致）
        assert!(
            manager.rpm_saturation_gate_active(),
            "归一化后 priority 模式同样走排序键，饱和硬门必须生效"
        );

        // ⭐核心断言：应溢出到未饱和的 #2，而不是死磕已打爆的高优先级 #1。
        let ctx = manager.acquire_context(None, None).await.unwrap();
        assert_eq!(
            ctx.id, 2,
            "高优先级号已饱和时必须优雅溢出到 #2（旧代码恒返 1：裸 min_by_key(priority) 不看饱和）"
        );
    }

    /// ⭐ 零回归对照：priority 语义**必须保留** —— 高优先级号健康时绝不提前用低优先级号。
    /// 与上一个测试构成一对，证明归一化不是"把 priority 变成纯均衡"，而是"分层 + 层内均衡"。
    #[tokio::test]
    async fn test_priority_mode_healthy_high_priority_not_bypassed() {
        let mut config = Config::default();
        config.load_balancing_mode = "priority".to_string();
        config.affinity_enabled = false;
        config.credential_rpm_limit = 100; // 阈值放大，保证不饱和

        let mut c1 = KiroCredentials::default();
        c1.priority = 0;
        c1.access_token = Some("tok1".to_string());
        c1.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());
        let mut c2 = KiroCredentials::default();
        c2.priority = 1;
        c2.access_token = Some("tok2".to_string());
        c2.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());

        let manager = MultiTokenManager::new(config, vec![c1, c2], None, None, false).unwrap();

        let ctx = manager.acquire_context(None, None).await.unwrap();
        assert_eq!(
            ctx.id, 1,
            "高优先级号健康时必须优先使用，priority 语义不能丢"
        );
    }

    /// ⭐ 归一化回归（**旧代码必失败**）：同优先级多号必须**分流**，不能恒选下标最小那个。
    ///
    /// 旧代码 `min_by_key(|e| e.credentials.priority)` 遇平局取第一个遇到的元素，而 `available`
    /// 的顺序 = 凭据入池顺序，于是同 priority 的号里**恒定选中最早创建的那个**，与运行时负载无关。
    /// 实测后果：5 号池 priority 全为 0 时，某号 rpm=23 而另一号 rpm=1（负载差 23 倍）。
    #[tokio::test]
    async fn test_priority_mode_same_priority_spreads_load() {
        let mut config = Config::default();
        config.load_balancing_mode = "priority".to_string();
        config.affinity_enabled = false;
        config.credential_rpm_limit = 1000; // 不触发饱和，纯看负载分流

        // 三个号 priority 全为 0（平局）——正是实机现场的配置
        let creds: Vec<KiroCredentials> = (0..3)
            .map(|i| {
                let mut c = KiroCredentials::default();
                c.priority = 0;
                c.access_token = Some(format!("tok{i}"));
                c.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());
                c
            })
            .collect();

        let manager = MultiTokenManager::new(config, creds, None, None, false).unwrap();

        // 连续取 6 次上下文，**持有不释放**（inflight 累积），观察是否分摊。
        let mut ctxs = Vec::new();
        for _ in 0..6 {
            ctxs.push(manager.acquire_context(None, None).await.unwrap());
        }
        let mut hits = std::collections::HashMap::new();
        for c in &ctxs {
            *hits.entry(c.id).or_insert(0u32) += 1;
        }
        assert!(
            hits.len() >= 2,
            "同优先级多号必须分流到至少 2 个号，实际只用了 {:?}（旧代码恒选下标最小的那一个）",
            hits
        );
        // 排序键第⑦位是 inflight 最少优先 → 3 个号 6 次应各 2 次，最热号不该独吞过半。
        let max_hit = hits.values().copied().max().unwrap();
        assert!(
            max_hit <= 3,
            "最热号占了 {max_hit}/6 次，分流失效（in-flight 维度未生效）：{hits:?}"
        );
    }

    /// ⭐回归:balanced 模式 + 池号数 >1 时硬门才真正生效(与 test_rpm_saturation_deprioritizes_credential
    /// 描述的调度行为一致)。
    #[test]
    fn test_rpm_saturation_gate_active_in_balanced_multi_credential() {
        let mut config = Config::default();
        config.load_balancing_mode = "balanced".to_string();

        let mut c1 = KiroCredentials::default();
        c1.access_token = Some("tok1".to_string());
        c1.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());
        let mut c2 = KiroCredentials::default();
        c2.access_token = Some("tok2".to_string());
        c2.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());

        let manager = MultiTokenManager::new(config, vec![c1, c2], None, None, false).unwrap();
        assert!(
            manager.rpm_saturation_gate_active(),
            "balanced 模式 + 池号数>1 应报硬门生效"
        );
    }

    /// ⭐回归:即便是 balanced 模式，只要池里只有 1 个号，"分流"这个概念本身就不适用
    /// (无处可分)，硬门也不该报生效——否则 UI 会显示"建议分流"却根本没有第二个号。
    #[test]
    fn test_rpm_saturation_gate_inactive_with_single_credential() {
        let mut config = Config::default();
        config.load_balancing_mode = "balanced".to_string(); // 即便开了 balanced

        let mut c1 = KiroCredentials::default();
        c1.access_token = Some("tok1".to_string());
        c1.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());

        let manager = MultiTokenManager::new(config, vec![c1], None, None, false).unwrap();
        assert!(
            !manager.rpm_saturation_gate_active(),
            "单号池下无分流对象，硬门不应报生效，即便是 balanced 模式"
        );
    }

    /// ⭐ 归一化回归（**旧代码必失败**）：priority 模式下硬门同样生效，与 balanced 无差别。
    ///
    /// 本测试的前身断言相反（"priority 模式下硬门不生效"）——那记录的是归一化前的事实：
    /// 裸 priority 分支不读饱和，所以那个阈值确实从未拦过请求（面板据此报"已达软上限"即虚假告警）。
    /// 归一化后两种模式共用同一套排序键，饱和硬门真实生效，面板报告与调度重新对齐。
    #[test]
    fn test_rpm_saturation_gate_active_in_priority_mode_after_normalization() {
        let mut config = Config::default();
        config.load_balancing_mode = "priority".to_string();

        let mut c1 = KiroCredentials::default();
        c1.access_token = Some("tok1".to_string());
        c1.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());
        let mut c2 = KiroCredentials::default();
        c2.access_token = Some("tok2".to_string());
        c2.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());

        let manager = MultiTokenManager::new(config, vec![c1, c2], None, None, false).unwrap();
        assert!(
            manager.rpm_saturation_gate_active(),
            "归一化后 priority 模式也走排序键，饱和硬门必须生效（旧代码此处为 false）"
        );
    }

    /// ⭐ 零回归：单号池下无论何种模式，硬门都不该报生效（无分流对象，"饱和"概念不适用）。
    /// 这条不受归一化影响，用于确认归一化没把单号池的特例判断一起吃掉。
    #[test]
    fn test_rpm_saturation_gate_still_inactive_for_single_credential_in_priority() {
        let mut config = Config::default();
        config.load_balancing_mode = "priority".to_string();

        let mut c1 = KiroCredentials::default();
        c1.access_token = Some("tok1".to_string());
        c1.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());

        let manager = MultiTokenManager::new(config, vec![c1], None, None, false).unwrap();
        assert!(
            !manager.rpm_saturation_gate_active(),
            "单号池无分流对象，硬门恒不生效（归一化不改变这条特例）"
        );
    }

    // ============ L3 headroom 折扣(旧代码上会失败:旧代码无折扣恒等于 base)============

    #[test]
    fn test_rpm_headroom_discount() {
        let config = Config::default();
        let manager = MultiTokenManager::new(config, vec![], None, None, false).unwrap();
        // 默认 factor=85:base 30 → 25;base 100 → 85。
        assert_eq!(
            manager.effective_saturation_limit(Some(30)),
            25,
            "30×0.85=25"
        );
        assert_eq!(
            manager.effective_saturation_limit(Some(100)),
            85,
            "100×0.85=85"
        );
        // factor=100(不打折):= base。
        manager.rpm_headroom_factor.store(100, Ordering::Relaxed);
        assert_eq!(
            manager.effective_saturation_limit(Some(30)),
            30,
            "factor=100 不打折"
        );
        // factor=0 视为不打折(防误配把号打成恒饱和)。
        manager.rpm_headroom_factor.store(0, Ordering::Relaxed);
        assert_eq!(
            manager.effective_saturation_limit(Some(30)),
            30,
            "factor=0 视为不打折"
        );
        // reserve_slots 叠加:base 30 × 0.85=25,再减 3 = 22。
        manager.rpm_headroom_factor.store(85, Ordering::Relaxed);
        manager.rpm_reserve_slots.store(3, Ordering::Relaxed);
        assert_eq!(manager.effective_saturation_limit(Some(30)), 22, "25-3=22");
        // 边界:base 1 × 0.85 = floor 0 → max(1)=1(绝不 0,否则恒饱和)。
        manager.rpm_reserve_slots.store(0, Ordering::Relaxed);
        assert_eq!(
            manager.effective_saturation_limit(Some(1)),
            1,
            "base 1 折后下限 1,不得 0"
        );
    }

    #[test]
    fn test_rpm_headroom_preserves_percred_priority() {
        // per-cred rpm_limit(>0) 优先级不被 headroom 破坏:#1 设 100、#2 未设(用全局/兜底)。
        let mut config = Config::default();
        config.credential_rpm_limit = 0; // 全局未设 → #2 走兜底 30
        config.rpm_headroom_factor = 100; // 隔离 headroom 变量,只验优先级选取
        let manager = MultiTokenManager::new(config, vec![], None, None, false).unwrap();
        assert_eq!(
            manager.effective_saturation_limit(Some(100)),
            100,
            "per-cred 100 优先"
        );
        assert_eq!(
            manager.effective_saturation_limit(None),
            30,
            "未设 → 兜底 30"
        );
    }

    /// 🔴 回归：排除集必须让 failover 真的换号，**即使 `cooldownEnabled=false`**。
    ///
    /// 旧行为：`acquire_context` 无排除入参，唯一阻止重选刚失败号的机制是
    /// `is_entry_selectable` 里那道冷却硬门 ⇒ `cooldownEnabled=false`（线上实际值）时
    /// failover 事实上不存在，下一跳可以立刻重选同一个刚被 429 的号，一个真实限流被
    /// 放大成连环 429。
    ///
    /// 回退即 FAIL：把 `select_next_credential` 里的排除 filter 删掉 → 本条断言
    /// 「第二次不得选回 #1」失败。
    #[tokio::test]
    async fn test_exclude_set_forces_failover_even_with_cooldown_disabled() {
        let mut config = Config::default();
        config.cooldown_enabled = false; // ⭐ 复现线上配置
        // ⚠️ 亲和**关闭**：本条要隔离的是 `select_next_credential` 里的排除 filter。
        // 亲和开着时那条 return 旁路自己也会排除，两条路径互相掩护 ⇒ 删掉任一条
        // 本测试都仍绿（我第一版就是这样写的，回退验证没能 FAIL）。
        // 亲和旁路由下一条 `..._respects_exclusion_on_affinity_bypass` 单独钉。
        config.affinity_enabled = false;
        config.load_balancing_mode = "balanced".to_string();

        let mut c1 = KiroCredentials::default();
        c1.priority = 0;
        c1.access_token = Some("tok1".to_string());
        c1.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());
        let mut c2 = KiroCredentials::default();
        c2.priority = 1;
        c2.access_token = Some("tok2".to_string());
        c2.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());
        let manager = MultiTokenManager::new(config, vec![c1, c2], None, None, false).unwrap();

        // 两个号的排序键此刻完全相同（inflight=0 / success=0 / rpm=0），所以选中的是
        // 第一个。反复选 5 次都必须避开它 —— 不带排除集时这 5 次会全落同一个号。
        let first = manager.acquire_context(None, None).await.unwrap();
        let first_id = first.id;
        drop(first); // 释放 inflight，排除 inflight 排序键的干扰

        let mut tried: HashSet<u64> = HashSet::new();
        tried.insert(first_id);
        for round in 0..5 {
            let next = manager
                .acquire_context_excluding(None, None, &tried)
                .await
                .unwrap();
            assert_ne!(
                next.id, first_id,
                "第 {round} 轮：排除集里的号绝不该被重选\
                 （cooldownEnabled=false 下这是唯一的换号保证）"
            );
            drop(next);
        }
    }

    /// 亲和旁路的排除 filter 是**纵深防御**（源码级守卫，不是行为断言）。
    ///
    /// # 为什么这里只能是源码守卫
    ///
    /// 我先写的是行为测试，回退验证时发现它**删掉 filter 也照样绿** —— 因为亲和查找
    /// 走的是 `available.iter().find(|e| e.id == bound_id)`，而 `available` 已经被上面
    /// 那道 `fresh` filter 剔掉了被排除的号 ⇒ 这里 `find` 必然 `None` ⇒ 自然落到下方
    /// 排序键。也就是说两道 filter 在当前结构下是**串联冗余**的，行为上无法区分。
    ///
    /// 保留亲和那道的理由：它把「排除」的语义显式写在旁路入口。将来若有人把
    /// 亲和查找改成直接查 `entries`（绕过 `available`，这是个很自然的"优化"），
    /// 那道 filter 就成了唯一防线。没有它时该改动会静默恢复「failover 立刻选回
    /// 绑定号」的旧行为，而线上 100% 带 session_id + 亲和默认开 ⇒ 排除集等于没接线。
    ///
    /// 与其写一个删掉也绿的假测试，不如如实钉住源码形态并写明理由。
    #[test]
    fn test_affinity_bypass_has_exclusion_guard_source() {
        let src = include_str!("token_manager.rs");
        let cut = src.find("#[cfg(test)]").unwrap_or(src.len());
        let prod = &src[..cut];
        // needle 运行时拼接：include_str! 会把测试自己的字面量也读进来（本仓库踩过三次）。
        let needle = format!(
            "self.affinity.get(uid){}",
            ".filter(|id| !excluded.contains(id))"
        );
        assert!(
            prod.contains(&needle),
            "亲和旁路必须显式排除本请求已试过的号（纵深防御，见本测试注释）"
        );
    }

    /// 与上一条配对：**全部候选都被排除时必须退化成允许重选，绝不报池子耗尽**。
    ///
    /// 只有上一条时，把排除写成硬门（`fresh.is_empty()` 时返回 None）也能通过 ——
    /// 那会让单号池、或「一轮试完」的请求被报成「所有凭据均已禁用」，
    /// 把一个纯粹需要重试的请求变成假的池空错误。
    #[tokio::test]
    async fn test_exclude_set_degrades_when_all_excluded() {
        let mut config = Config::default();
        config.cooldown_enabled = false;
        let mut c1 = KiroCredentials::default();
        c1.access_token = Some("tok1".to_string());
        c1.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());
        let manager = MultiTokenManager::new(config, vec![c1], None, None, false).unwrap();

        // 单号池 + 该号已被排除 = 排除集覆盖全池。
        let mut tried: HashSet<u64> = HashSet::new();
        tried.insert(1);
        let ctx = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            manager.acquire_context_excluding(None, None, &tried),
        )
        .await
        .expect("绝不该挂死")
        .expect("全池被排除时必须退化成允许重选，而不是报池子耗尽");
        assert_eq!(ctx.id, 1, "退化后应仍能选出唯一那个号");
    }

    /// 第三条配对：**排除集降级必须穿透 RPM 饱和硬门**（否则确定性忙等 64 轮后 bail）。
    ///
    /// 上一条（`test_exclude_set_degrades_when_all_excluded`）只覆盖「排除集吃掉全部
    /// 可选号」——那时 `fresh` 为空，`available` 已等于全集，降级在 `fresh.is_empty()`
    /// 那一处就发生了。覆盖不到的是这条：`fresh` **非空**，但 fresh 里的号全部 RPM
    /// 饱和，而**被排除**的号里还有未饱和的。RPM 饱和是唯一排在排除集之后的硬门，
    /// 于是 `select_next_credential` 返 None，而 `transient_wait_outcome` 按设计不吃
    /// 排除集、遍历全池看见那个未饱和号 ⇒ 返回 `Available` ⇒ `acquire_context` 的
    /// `Available` 分支零 sleep、零 attempt 递增 ⇒ 忙等到撞 `MAX_RACE_RESELECT` 才 bail
    /// 「选号竞态无法收敛」。线上 2h 内 351 次。
    ///
    /// 忙等期间 `commit_selection` 从不被调用 ⇒ RPM 计数不变 ⇒ 每轮判定完全相同，
    /// 所以这是**确定性**复现，不依赖任何时序。
    ///
    /// ⚠️ 必须打 `acquire_context_excluding` 这个真实入口而**不是**内部的
    /// `select_next_credential`：缺陷的后果（忙等）产生在
    /// 「select 返 None → 自愈块 → transient_wait_outcome → Available 分支」这段链条上，
    /// 只断言纯函数返回 Some 会漏掉链条上任何更靠前的短路分支。
    #[tokio::test]
    async fn test_exclude_set_degrades_when_fresh_all_rpm_saturated() {
        let mut config = Config::default();
        config.load_balancing_mode = "balanced".to_string();
        config.affinity_enabled = false; // 避开亲和旁路（那是一条 return）
        config.cooldown_enabled = false; // 让被排除的 #1 仍留在 selectable 里
        config.credential_rpm_limit = 2;
        config.rpm_headroom_factor = 100; // 隔离 headroom 折扣，阈值恰为 2
        config.rpm_hard_gate_overload_wait = true; // 🔴 复现硬前提：背压关时软门恒返 Some
        let creds: Vec<KiroCredentials> = (1..=2)
            .map(|i| {
                let mut c = KiroCredentials::default();
                c.access_token = Some(format!("tok-{i}"));
                c.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());
                c
            })
            .collect();
        let manager = MultiTokenManager::new(config, creds, None, None, false).unwrap();

        // #2 打到饱和（limit=2），#1 保持 rpm=0 未饱和。
        // 直接 rpm.record 而不是先 acquire 一次：后者会把计数记到**被选中**的号上，状态不可控。
        manager.rpm.record(2);
        manager.rpm.record(2);
        assert!(
            manager.is_rpm_saturated_with_limit(2, None),
            "#2 应已饱和（前提不成立则本测试无意义）"
        );
        assert!(
            !manager.is_rpm_saturated_with_limit(1, None),
            "#1 应未饱和（前提不成立则本测试无意义）"
        );

        // 方向不可反：排除**未饱和**的 #1，留下**已饱和**的 #2 作为唯一 fresh 候选。
        // 反过来（排除 #2）时 fresh={#1} 非空且未饱和 ⇒ 正常选出 ⇒ 测不到缺陷。
        let mut tried: HashSet<u64> = HashSet::new();
        tried.insert(1);
        let ctx = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            manager.acquire_context_excluding(None, None, &tried),
        )
        .await
        .expect("绝不该挂死（忙等时护栏会 bail，真挂死说明护栏也失效了）")
        .expect(
            "fresh 全饱和但被排除的号里有未饱和的 ⇒ 必须降级回全体可选号重选，\
             而不是 bail「选号竞态无法收敛」",
        );
        assert_eq!(
            ctx.id, 1,
            "应降级重选那个未饱和的已试过号 #1，而不是选已饱和的 #2 或失败"
        );
    }

    /// 与上一条配对，防「降级写成无条件放行」：**全池真饱和时降级绝不能触发**，
    /// 必须保持返回 None 让背压去等 RPM 恢复。
    ///
    /// 只有上一条时，把降级写成「hard_gate 开且 fresh 全饱和就无条件回退全集」也能通过——
    /// 那会在整池真饱和时放行一个已饱和号，等于把 L4 背压硬门整体拆掉。
    #[tokio::test]
    async fn test_exclude_degrade_does_not_fire_when_pool_truly_saturated() {
        let mut config = Config::default();
        config.load_balancing_mode = "balanced".to_string();
        config.affinity_enabled = false;
        config.cooldown_enabled = false;
        config.credential_rpm_limit = 2;
        config.rpm_headroom_factor = 100;
        config.rpm_hard_gate_overload_wait = true;
        let creds: Vec<KiroCredentials> = (1..=2)
            .map(|i| {
                let mut c = KiroCredentials::default();
                c.access_token = Some(format!("tok-{i}"));
                c.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());
                c
            })
            .collect();
        let manager = MultiTokenManager::new(config, creds, None, None, false).unwrap();

        // 两号都打到饱和 ⇒ 全池真饱和。
        for id in [1u64, 2u64] {
            manager.rpm.record(id);
            manager.rpm.record(id);
        }
        let mut tried: HashSet<u64> = HashSet::new();
        tried.insert(1);
        assert!(
            manager.select_next_credential(None, None, &tried).is_none(),
            "全池真饱和 + 背压开 ⇒ 硬门必须返回 None（等 RPM 恢复），降级不得越过它放行饱和号"
        );
    }

    /// RPM 恢复等待的 release_index 精确化（limit 热调低场景）。
    ///
    /// 旧公式等「最老一条过期」（fresh=5、limit=2 时窗口内仍剩 4 条 > limit），
    /// 回给客户端的 Retry-After 偏小。新公式等第 `fresh - limit + 1` 老过期：
    /// - fresh > limit：等待必须比旧公式更长；
    /// - fresh == limit：k=1，与旧公式完全一致（零回归锚点）。
    #[test]
    fn test_transient_wait_rpm_recovery_uses_release_index() {
        let mut config = Config::default();
        config.load_balancing_mode = "balanced".to_string();
        config.affinity_enabled = false;
        config.cooldown_enabled = false;
        config.rate_limit_enabled = false;
        config.credential_rpm_limit = 0; // 用 per-cred limit，与选号路径同口径
        config.rpm_headroom_factor = 100; // 隔离 headroom 折扣，阈值恰为 per-cred 值
        config.rpm_hard_gate_overload_wait = true;
        let mut c = KiroCredentials::default();
        c.id = Some(1);
        c.access_token = Some("tok-1".to_string());
        c.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());
        c.rpm_limit = Some(2); // 饱和阈值恰为 2
        let manager = MultiTokenManager::new(config, vec![c], None, None, false).unwrap();

        // ===== 场景 1：limit 被热调低（fresh_count > limit）→ 等待必须变长 =====
        for _ in 0..5 {
            manager.rpm.record(1);
            std::thread::sleep(std::time::Duration::from_millis(15));
        }
        let fresh = manager.rpm.count(1);
        assert_eq!(fresh, 5, "前置：窗口内应有 5 次命中");
        assert!(manager.is_rpm_saturated_with_limit(1, Some(2)), "前置：应已饱和");

        let old_wait = manager
            .rpm
            .oldest_age(1)
            .map(|age| manager.rpm.window().saturating_sub(age))
            .expect("必有最老命中");
        let release_index = fresh - 2 + 1; // k = fresh - limit + 1 = 4
        let expected = manager
            .rpm
            .kth_oldest_age(1, release_index)
            .map(|age| manager.rpm.window().saturating_sub(age))
            .expect("窗口内 5 条，k=4 必存在");

        match manager.transient_wait_outcome(None) {
            WaitOutcome::Wait(dur, WaitReason::RpmRecovery) => {
                let d = dur.as_secs_f64();
                let old = old_wait.as_secs_f64();
                assert!(
                    d > old,
                    "limit 热调低后恢复等待必须长于旧公式：new={dur:?} 应 > old={old_wait:?}"
                );
                let expect_f = expected.as_secs_f64();
                assert!(
                    (d - expect_f).abs() < 0.05,
                    "新等待应等于第 k 老过期的精确值：got={dur:?} expected≈{expected:?}"
                );
            }
            other => panic!("应返回 RpmRecovery 等待，实际 {other:?}"),
        }

        // ===== 场景 2：fresh == limit → 与旧公式一致（零回归） =====
        let manager2 = {
            let mut c2 = KiroCredentials::default();
            c2.id = Some(1);
            c2.access_token = Some("tok-1".to_string());
            c2.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());
            c2.rpm_limit = Some(2);
            let mut cfg2 = Config::default();
            cfg2.load_balancing_mode = "balanced".to_string();
            cfg2.affinity_enabled = false;
            cfg2.cooldown_enabled = false;
            cfg2.rate_limit_enabled = false;
            cfg2.rpm_headroom_factor = 100;
            cfg2.rpm_hard_gate_overload_wait = true;
            MultiTokenManager::new(cfg2, vec![c2], None, None, false).unwrap()
        };
        manager2.rpm.record(1);
        std::thread::sleep(std::time::Duration::from_millis(15));
        manager2.rpm.record(1); // fresh == limit == 2 → k = 1
        assert!(manager2.is_rpm_saturated_with_limit(1, Some(2)), "前置：2 次命中 == 阈值 2 应判饱和");
        let old_wait2 = manager2
            .rpm
            .oldest_age(1)
            .map(|age| manager2.rpm.window().saturating_sub(age))
            .expect("必有最老命中");
        match manager2.transient_wait_outcome(None) {
            WaitOutcome::Wait(dur, WaitReason::RpmRecovery) => {
                let d = dur.as_secs_f64();
                let old = old_wait2.as_secs_f64();
                assert!(
                    (d - old).abs() < 0.05,
                    "fresh == limit 时必须与旧公式一致：got={dur:?} expected≈{old_wait2:?}"
                );
            }
            other => panic!("应返回 RpmRecovery 等待，实际 {other:?}"),
        }
    }

    /// 排除集**不得**污染 `transient_wait_outcome` 的池健康判定。
    ///
    /// 那个函数回答「池里还有没有号将要恢复」，与「本请求还想不想再试它」是两个问题。
    /// 若给它也加排除集，「一轮试完」后它会返回 `NoCandidate` → bail
    /// 「所有凭据均已禁用」，而池子明明健康 —— 这是本改动最容易引入的回归。
    #[test]
    fn test_wait_outcome_ignores_exclusion_source_guard() {
        // 源码级守卫：transient_wait_outcome 的签名里不得出现 excluded 参数。
        // needle 运行时拼接，避免 include_str! 自匹配（本仓库踩过三次）。
        let src = include_str!("token_manager.rs");
        let cut = src.find("#[cfg(test)]").unwrap_or(src.len());
        let prod = &src[..cut];
        let head = format!("fn transient_wait_outcome{}", "(");
        let start = prod.find(&head).expect("transient_wait_outcome 不应被改名");
        let sig_end = prod[start..].find('{').expect("函数签名应以 { 结束") + start;
        let sig = &prod[start..sig_end];
        assert!(
            !sig.contains("excluded"),
            "transient_wait_outcome 的签名不得带排除集参数（见 acquire_context_excluding \
             注释里的不变量 2：加了会让「一轮试完」误判 NoCandidate → 假报池子耗尽）。实际签名: {sig}"
        );
        // 配对：select_next_credential 则**必须**带它，否则排除集等于没接线。
        let sel = format!("fn select_next_credential{}", "(");
        let s0 = prod.find(&sel).expect("select_next_credential 不应被改名");
        let s1 = prod[s0..].find(')').expect("签名应有右括号") + s0;
        assert!(
            prod[s0..s1].contains("excluded"),
            "select_next_credential 必须接收排除集，否则 failover 不会真的换号"
        );
    }

    #[tokio::test]
    async fn test_affinity_disabled_falls_back_to_normal_selection() {
        // 关闭亲和性时不应固定，balanced 的 least-used 应能切换凭据
        let mut config = Config::default();
        config.load_balancing_mode = "balanced".to_string();
        config.affinity_enabled = false;

        let mut c1 = KiroCredentials::default();
        c1.priority = 0;
        c1.access_token = Some("tok1".to_string());
        c1.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());
        let mut c2 = KiroCredentials::default();
        c2.priority = 1;
        c2.access_token = Some("tok2".to_string());
        c2.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());

        let manager = MultiTokenManager::new(config, vec![c1, c2], None, None, false).unwrap();

        // 第一次成功后 success_count 增加，least-used 应在第二次切到另一个凭据
        let first = manager
            .acquire_context(None, Some("session-A"))
            .await
            .unwrap();
        manager.report_success(first.id);
        let second = manager
            .acquire_context(None, Some("session-A"))
            .await
            .unwrap();
        assert_ne!(first.id, second.id, "关闭亲和性后应按 least-used 切换");
    }

    #[test]
    fn test_multi_token_manager_report_refresh_failure() {
        let config = Config::default();
        let cred1 = KiroCredentials::default();
        let cred2 = KiroCredentials::default();

        let manager =
            MultiTokenManager::new(config, vec![cred1, cred2], None, None, false).unwrap();

        assert_eq!(manager.available_count(), 2);
        for _ in 0..(MAX_FAILURES_PER_CREDENTIAL - 1) {
            assert!(manager.report_refresh_failure(1));
        }
        assert_eq!(manager.available_count(), 2);

        assert!(manager.report_refresh_failure(1));
        assert_eq!(manager.available_count(), 1);

        let snapshot = manager.snapshot();
        let first = snapshot.entries.iter().find(|e| e.id == 1).unwrap();
        assert!(first.disabled);
        assert_eq!(first.refresh_failure_count, MAX_FAILURES_PER_CREDENTIAL);
        assert_eq!(snapshot.current_id, 2);
    }

    /// 🔴 回归：**瞬态**刷新失败（上游 5xx / 网络）绝不能累加 `refresh_failure_count`。
    ///
    /// 旧行为：`report_refresh_failure` 被无条件调用 → 3 次即 `TooManyRefreshFailures`
    /// 禁用 + 落盘。而 `refresh_token_locked` 内部**已经**对 5xx/网络退避重试过 3 次
    /// 才上报一次，所以上报的错误里上游抖动占绝大多数 —— 一次几十秒的 token 端点抖动
    /// 就能永久烧掉健康号（用户报的「号是正常的却被禁用了好几次」的成因之一）。
    ///
    /// 回退即 FAIL：把 `report_refresh_failure_classified` 的分流改回无条件
    /// `self.report_refresh_failure(id)` → 本条第一个断言（计数必须仍为 0）立刻失败。
    #[test]
    fn test_transient_refresh_error_does_not_count_toward_disable() {
        let config = Config::default();
        let cred1 = KiroCredentials::default();
        let cred2 = KiroCredentials::default();
        let manager =
            MultiTokenManager::new(config, vec![cred1, cred2], None, None, false).unwrap();

        // 远超阈值的次数，全部是瞬态错误。
        for _ in 0..(MAX_FAILURES_PER_CREDENTIAL * 4) {
            let e = anyhow::anyhow!("刷新请求失败: 503 Service Unavailable");
            assert!(
                manager.report_refresh_failure_classified(1, &e),
                "瞬态失败不该让池子变成不可用"
            );
        }
        let snap = manager.snapshot();
        let first = snap.entries.iter().find(|e| e.id == 1).unwrap();
        assert_eq!(
            first.refresh_failure_count, 0,
            "上游 5xx 是瞬态错误，绝不能累加刷新失败计数（旧行为在此计到 12 次并禁用）"
        );
        assert!(!first.disabled, "瞬态刷新失败绝不能禁用凭据");
        assert_eq!(manager.available_count(), 2, "池子容量不该因上游抖动缩小");

        // 网络层错误（无状态码）同样是瞬态。
        for _ in 0..(MAX_FAILURES_PER_CREDENTIAL * 2) {
            let e = anyhow::anyhow!("error sending request: connection timed out");
            manager.report_refresh_failure_classified(1, &e);
        }
        let snap = manager.snapshot();
        let first = snap.entries.iter().find(|e| e.id == 1).unwrap();
        assert_eq!(first.refresh_failure_count, 0, "网络超时同样不该计数");
        assert!(!first.disabled);
    }

    /// 与上一条配对：**凭据级**刷新失败仍必须计数并最终禁用。
    ///
    /// 只有上一条时，把 `is_refresh_error_credential_level` 写成恒 `false`
    /// （即"一律不计数"）也能通过 —— 那会造出真废的号永不被禁用的僵尸。
    /// 两条一起才钉住这个判据。
    #[test]
    fn test_credential_level_refresh_error_still_disables() {
        let config = Config::default();
        let cred1 = KiroCredentials::default();
        let cred2 = KiroCredentials::default();
        let manager =
            MultiTokenManager::new(config, vec![cred1, cred2], None, None, false).unwrap();

        for _ in 0..MAX_FAILURES_PER_CREDENTIAL {
            let e: anyhow::Error = RefreshHttpError {
                status: 401,
                message: "刷新失败: 401 Unauthorized".into(),
            }
            .into();
            manager.report_refresh_failure_classified(1, &e);
        }
        let snap = manager.snapshot();
        let first = snap.entries.iter().find(|e| e.id == 1).unwrap();
        assert!(
            first.disabled,
            "token 端点明确回 401 = 凭据级问题，仍必须计数并禁用（否则真废的号成僵尸）"
        );
        assert_eq!(
            first.disabled_reason.as_deref(),
            Some(DisabledReason::TooManyRefreshFailures.as_str()),
            "原因须是 TooManyRefreshFailures（运维据此判断该去查凭据而非上游）"
        );
        assert_eq!(manager.available_count(), 1);
    }

    /// 🔴 回归：一个号**第一次**成功必须立刻落盘（绕过 debounce）。
    ///
    /// 这不是统计精度问题，而是**烧号防线**：`success_count > 0` 是
    /// `has_ever_succeeded()` 的判据，而那是 provider 区分「bearer-invalid 403 =
    /// 瞬态抖动」与「真 region 错配」的唯一依据。
    ///
    /// 线上实测的完整链：debounce 窗口内的成功增量只在内存 → 进程被 SIGKILL
    /// （今天 41 次 SIGTERM 里 39 次走到 SIGKILL）→ 重启后该号 `success_count=0`
    /// ⇒ 判成"从未成功过" ⇒ 三次瞬态 403 即 `TooManyFailures`。
    /// **20:20:30 启动、20:20:32 就把 93.9% 成功率的 #483 打死。**
    ///
    /// 回退即 FAIL：把 `report_success` 末尾的 `if first_success { self.save_stats() }`
    /// 改回无条件 `save_stats_debounced()` → 本条第二个断言（重载后仍有成功记录）失败。
    #[test]
    fn test_first_success_persists_immediately_for_has_ever_succeeded() {
        let dir = std::env::temp_dir().join(format!("ks_first_succ_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cred_path = dir.join("credentials.json");

        let mut c = KiroCredentials::default();
        c.access_token = Some("tok".to_string());
        c.refresh_token = Some("rt".to_string());
        let mut c2 = KiroCredentials::default();
        c2.access_token = Some("tok2".to_string());
        c2.refresh_token = Some("rt2".to_string());
        let creds = vec![c.clone(), c2.clone()];
        std::fs::write(&cred_path, serde_json::to_string(&creds).unwrap()).unwrap();

        let config = Config::default();
        let mgr = MultiTokenManager::new(
            config.clone(),
            creds.clone(),
            None,
            Some(cred_path.clone()),
            true,
        )
        .unwrap();

        // ⚠️ 必须先「预热」debounce 时钟：`last_stats_save_at` 初始是 `None`，此时
        // `save_stats_debounced` 会**立刻**落盘 —— 于是不预热的话，无条件 debounce
        // 与本修复行为完全相同，测试测不出差别（我第一版就是这样，回退验证没能 FAIL）。
        // 用 #2 的一次失败把时钟推到"刚刚保存过"，之后 #1 的首次成功若走 debounce
        // 就会被压住不写。
        mgr.report_failure(2);

        assert!(!mgr.has_ever_succeeded(1), "前提：新号必须是「从未成功过」");
        mgr.report_success(1);
        assert!(mgr.has_ever_succeeded(1), "成功后内存态必须为真");

        // ⭐ 关键断言：**不给任何 flush 机会**（模拟 SIGKILL）就新建一个 manager 重读
        // stats —— 首次成功若只进了 debounce 队列，这里读到的就是 0。
        let mgr2 =
            MultiTokenManager::new(config, creds, None, Some(cred_path.clone()), true).unwrap();
        assert!(
            mgr2.has_ever_succeeded(1),
            "首次成功必须已落盘 —— 否则进程被硬杀后该号会被判成「从未成功过」，\
             三次瞬态 403 即被禁用（线上 #483 就是这样被打死的）"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `flush_stats_now` 必须被停机路径调用，且必须在 drain 宽限期**开始之前**。
    ///
    /// 源码级守卫：停机是进程级行为，单测跑不到（要真发 SIGTERM 并观察落盘）。
    /// 位置断言是承重的 —— 放在宽限期之后时，线上 `TimeoutStopSec=10` 会让它
    /// 大概率压根执行不到（实测 39/41 次走到 SIGKILL）。
    ///
    /// ⚠️ 锚点已随 #22 的修复更新：宽限期的 `sleep` 从本函数**移到了**
    /// `race_serve_against_drain_cap`（原先放在这里两个承诺都不成立 ——
    /// 本 future 返回只意味着"停止接新连接"，`serve().await` 之后仍无上限地等）。
    /// 故这里改为断言 `flush_stats_now` 在 `notify_one()` 之前：后者才是"宽限期
    /// 从此刻起算"的信号。**不变量本身没变，只是它的锚点搬了家。**
    #[test]
    fn test_shutdown_flushes_stats_before_drain_sleep() {
        let src = include_str!("../main.rs");
        let cut = src.find("#[cfg(test)]").unwrap_or(src.len());
        let prod = &src[..cut];
        let fn_start = prod
            .find("async fn shutdown_with_drain_cap")
            .expect("停机函数不应被改名");
        let body = &prod[fn_start..];
        let body_end = body.find("\n}").map(|i| i + fn_start).unwrap_or(prod.len());
        let body = &prod[fn_start..body_end];
        // 只看真代码：注释里提到 needle 会让位置比较失真（本仓踩过五次「锚点选到散文」）。
        let body: String = body
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");

        // needle 运行时拼接，避免 include_str! 自匹配（本仓库踩过三次）。
        let flush = format!("flush_stats_now{}", "()");
        let arm = format!("notify_one{}", "()");
        let fi = body.find(&flush).expect(
            "停机路径必须强制落盘凭据统计（否则 debounce 窗口内的成功增量被 SIGKILL 丢掉 \
             → 重启后健康号被判「从未成功过」→ 三次瞬态 403 即禁用）",
        );
        let ai = body.find(&arm).expect(
            "停机路径必须通知外层竞速『宽限期从此刻起算』（见 race_serve_against_drain_cap）",
        );
        assert!(
            fi < ai,
            "flush_stats_now 必须在宽限期起算**之前**：线上 TimeoutStopSec=10，放在之后\
             有很大概率执行不到（实测 41 次 SIGTERM 里 39 次走到 SIGKILL）"
        );

        // 承重：宽限期的 sleep 必须**确实存在于**竞速函数里，否则上限根本不生效
        // （#22 的原缺陷正是"注释承诺了、代码里没有"）。
        let race_start = prod
            .find("async fn race_serve_against_drain_cap")
            .expect("竞速函数不应被改名 —— drain 上限只能在对 serve() 竞速处生效");
        let race_body = &prod[race_start..];
        let race_body = &race_body[..race_body.find("\n}").unwrap_or(race_body.len())];
        let sleep = format!("tokio::time::sleep{}", "(");
        assert!(
            race_body.contains(&sleep),
            "竞速函数里必须有宽限期 sleep：没有它 serve().await 就是无上限等待，\
             长流式 SSE 会把停机拖到 systemd SIGKILL（实测单次部署 74s 停服 / 167 次 502）"
        );
    }

    /// 🔴 P0-1 回归：**全池冷却时必须兜底放行**，而不是 bail 出网关自造的 429。
    ///
    /// 实测代价：`credential_id IS NULL` 逐小时占比 20 点 8.2% / 21 点 9.5% /
    /// **22 点 15.3%** —— 那些请求上游压根没被调用过，纯粹是网关自己造的失败。
    ///
    /// 回退即 FAIL：删掉 `Cooling` 分支顶部那段 `select_ignoring_cooldown` 兜底
    /// → 本条会拿到 bail 而非 Ok。
    #[tokio::test]
    async fn test_all_cooling_falls_back_instead_of_self_inflicted_429() {
        let mut config = Config::default();
        config.cooldown_enabled = true; // 必须开，否则冷却设不上
        // 开 fast-fail 让它**立刻**走到 bail 分支（不然要等 MAX_TRANSIENT_WAIT 20s）。
        config.all_cooling_fast_fail = true;
        let mut c1 = KiroCredentials::default();
        c1.access_token = Some("tok1".to_string());
        c1.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());
        let mut c2 = KiroCredentials::default();
        c2.access_token = Some("tok2".to_string());
        c2.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());
        let mgr = MultiTokenManager::new(config, vec![c1, c2], None, None, true).unwrap();

        // 把两个号都打进长冷却（远超 FAST_FAIL_THRESHOLD）。
        for id in [1u64, 2] {
            mgr.cooldown.set_cooldown_with_duration(
                id,
                CooldownReason::RateLimitExceeded,
                Some(StdDuration::from_secs(600)),
            );
            assert!(!mgr.cooldown.is_available(id), "前提：#{id} 必须真的在冷却");
        }

        let ctx = tokio::time::timeout(StdDuration::from_secs(5), mgr.acquire_context(None, None))
            .await
            .expect("不应挂死")
            .expect(
                "全池冷却时必须兜底放行（拿真实上游 429 好过网关自造 429）—— \
             bail 会让 8~15% 的请求变成上游从未被调用的自造失败",
            );
        assert!(
            ctx.id == 1 || ctx.id == 2,
            "兜底应放行池内某个冷却号，实得 #{}",
            ctx.id
        );
    }

    /// 与上一条配对：**全池 `disabled`（非冷却）时仍必须 bail**。
    ///
    /// 只有上一条时，把兜底写成「放行任何号」也能通过 —— 那会绕过配额耗尽 /
    /// 账号封禁 / refreshToken 失效这些**终态**判定，变成反复打已死的号。
    /// 两条一起才钉住「只放宽冷却这一道」。
    #[tokio::test]
    async fn test_fallback_never_releases_disabled_credentials() {
        let mut config = Config::default();
        config.cooldown_enabled = true;
        config.all_cooling_fast_fail = true;
        let mut c1 = KiroCredentials::default();
        c1.access_token = Some("tok1".to_string());
        c1.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());
        let mgr = MultiTokenManager::new(config, vec![c1], None, None, true).unwrap();

        // 终态禁用（不可自愈）+ 同时在冷却 —— 兜底绝不能放行它。
        mgr.report_account_suspended(1);
        mgr.cooldown.set_cooldown_with_duration(
            1,
            CooldownReason::RateLimitExceeded,
            Some(StdDuration::from_secs(600)),
        );
        assert_eq!(mgr.available_count(), 0, "前提：池子必须真的空");

        let err = tokio::time::timeout(StdDuration::from_secs(5), mgr.acquire_context(None, None))
            .await
            .expect("不应挂死")
            .err()
            .expect("全池 disabled 时绝不能放行 —— 那会反复打已封禁/额度耗尽的死号");
        let s = err.to_string();
        assert!(
            s.contains("retry_after_secs="),
            "仍必须带 retry_after_secs 让客户端退避: {s}"
        );
    }

    /// 兜底选号**只放宽冷却**，其余硬门逐条保留（这里验模型级黑名单那道）。
    ///
    /// 判据函数复用是刻意的：若在 `select_ignoring_cooldown` 里重写一遍过滤，
    /// 两处条件会漂移，而漂移过的历史后果是 `acquire_context` 忙等死循环
    /// （见 `transient_wait_outcome` 的长注释）。
    #[test]
    fn test_fallback_preserves_non_cooldown_gates() {
        let mut config = Config::default();
        config.cooldown_enabled = true;
        let mut c1 = KiroCredentials::default();
        c1.access_token = Some("tok1".to_string());
        c1.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());
        let mgr = MultiTokenManager::new(config, vec![c1], None, None, true).unwrap();

        const MODEL: &str = "claude-sonnet-4.5";
        mgr.cooldown.set_cooldown_with_duration(
            1,
            CooldownReason::RateLimitExceeded,
            Some(StdDuration::from_secs(600)),
        );
        // 只冷却 ⇒ 兜底应能选出它。
        let empty: HashSet<u64> = HashSet::new();
        assert!(
            mgr.select_ignoring_cooldown(Some(MODEL), &empty).is_some(),
            "仅冷却的号，兜底应放行"
        );
        // 再把它对该模型加进黑名单 ⇒ 模型级硬门必须仍然拦住它。
        mgr.report_model_invalid(1, Some(MODEL));
        assert!(
            mgr.select_ignoring_cooldown(Some(MODEL), &empty).is_none(),
            "模型级黑名单是非冷却硬门，兜底绝不能放宽它（否则会把 INVALID_MODEL_ID 的号反复选回来）"
        );
        // 但对**别的**模型它仍应可被兜底放行（黑名单是模型级、不是号级）。
        assert!(
            mgr.select_ignoring_cooldown(Some("claude-opus-4.1"), &empty)
                .is_some(),
            "模型级黑名单只针对该模型，别的模型不该受影响"
        );
    }

    /// 🔴 H4 回归：**兜底放行必须轮转，不能反复命中同一个 id。**
    ///
    /// 实测代价：#578 近 3 小时拿到 128 次兜底放行、单分钟峰值 63 —— 兜底的用意是
    /// 「拿真实上游 429 好过网关自造 429」，全压在一个号上等于专挑一个号打爆，
    /// 而池里其它同样在冷却的号一次都没被试过。
    ///
    /// # 回退即 FAIL（已验证）
    ///
    /// 把排序键换回 `min_by_key(冷却剩余)`：三个号的冷却是**依次**设的，到期时刻
    /// 严格递增且此后不再变，于是每次都返回同一个 id ⇒ 下面 `distinct` 只会有 1 个。
    ///
    /// # 为什么断言的是「多次调用命中多个 id」而不是「相邻两次不同」
    ///
    /// 相邻不同是实现细节（若将来改成别的打散方式，例如按分档内最少放行次数，
    /// 相邻两次也可能相同但整体仍是均摊的）。可观测的性质是**不聚集**。
    /// 同时也断言了「一次只放行一个号」——每次调用只拿回一个 guard。
    #[test]
    fn test_fallback_rotates_across_ids_instead_of_pinning_one() {
        let mut config = Config::default();
        config.cooldown_enabled = true;
        // 容量给大，免得 commit_selection 里的 rpm.record 把某个号排除掉
        // （本条测的是轮转，不该被 RPM 饱和门干扰）。
        config.credential_rpm_limit = 1_000_000;
        let creds: Vec<KiroCredentials> = (0..3)
            .map(|i| {
                let mut c = KiroCredentials::default();
                c.access_token = Some(format!("tok{i}"));
                c.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());
                c
            })
            .collect();
        let mgr = MultiTokenManager::new(config, creds, None, None, true).unwrap();

        // 三个号全部打进**同档**冷却（同一 reason 同一时长 ⇒ 同一分档），
        // 但设置时刻依次靠后 ⇒ 剩余秒数严格递增 ⇒ 旧实现恒选第一个。
        for id in [1u64, 2, 3] {
            mgr.cooldown.set_cooldown_with_duration(
                id,
                CooldownReason::RateLimitExceeded,
                Some(StdDuration::from_secs(30)),
            );
            assert!(!mgr.cooldown.is_available(id), "前提：#{id} 必须真的在冷却");
        }

        let empty: HashSet<u64> = HashSet::new();
        let mut hits: Vec<u64> = Vec::new();
        for _ in 0..9 {
            let (id, _c, guard) = mgr
                .select_ignoring_cooldown(None, &empty)
                .expect("全在冷却时兜底必须放行一个号");
            hits.push(id);
            drop(guard); // 立刻归还在途名额，下一轮重新全平局
        }

        let distinct: HashSet<u64> = hits.iter().copied().collect();
        assert_eq!(
            distinct.len(),
            3,
            "兜底放行必须轮转到全部 3 个同档冷却号，实际只命中 {:?}（序列 {:?}）—— \
             聚集单号就是 #578 那次 3 小时 128 次的形态",
            distinct,
            hits
        );
        // 轮转是确定性的：给定 (候选集, 游标) 结果唯一，所以序列必须严格是 1,2,3 循环。
        // 这一条同时钉住「按 id 轮转而非随机」（随机打散键 tie_break_jitter 因不可复现被删）。
        assert_eq!(
            hits,
            vec![1, 2, 3, 1, 2, 3, 1, 2, 3],
            "同档内应按 id 升序从游标之后继续、到尾回绕（确定性，可复盘）"
        );
    }

    /// 兜底轮转**不得越过深度档**：会自愈的号优先于不可自愈的深冷却号。
    ///
    /// 纯 id 轮转会把请求送给一个 86400s 冷却（`AuthenticationFailed`）的号，
    /// 而池里可能有个几秒后就恢复的限流号 —— 那不是"摊开打"，是白扔一次上游往返。
    ///
    /// # 夹具走真实链路
    ///
    /// #1 用 `report_auth_cooldown`（`provider.rs:819/1582/1613/1644` 在上游 401/403
    /// bearer 失效时调的就是它）→ `AuthenticationFailed` → 不可自愈 → 86400s，
    /// **且不禁用该号**，所以它确实会进兜底候选集。
    /// #2 用 `report_rate_limited_with_retry_after(2, None)`（裸 429 的真实路径）
    /// → `RateLimitExceeded` 固定基线 15s → 可自愈。
    ///
    /// 回退即 FAIL：把排序键里的 `tier` 去掉（只留 id 轮转）→ 第一次就会选到 #1。
    #[test]
    fn test_fallback_prefers_recoverable_tier_over_rotation() {
        let mut config = Config::default();
        config.cooldown_enabled = true;
        config.credential_rpm_limit = 1_000_000;
        let creds: Vec<KiroCredentials> = (0..2)
            .map(|i| {
                let mut c = KiroCredentials::default();
                c.access_token = Some(format!("tok{i}"));
                c.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());
                c
            })
            .collect();
        let mgr = MultiTokenManager::new(config, creds, None, None, true).unwrap();

        // id 更小的是那个坏的，所以纯 id 轮转会先选 #1 —— 正是本条要挡住的。
        mgr.report_auth_cooldown(1);
        mgr.report_rate_limited_with_retry_after(2, None);
        assert_eq!(
            mgr.fallback_cooldown_tier(1),
            FallbackCooldownTier::Deep,
            "前提：认证失败必须落 Deep（不可自愈）"
        );
        assert_eq!(
            mgr.fallback_cooldown_tier(2),
            FallbackCooldownTier::Shallow,
            "前提：裸 429 必须落 Shallow（会自愈）"
        );

        let empty: HashSet<u64> = HashSet::new();
        for round in 0..4 {
            let (id, _c, guard) = mgr
                .select_ignoring_cooldown(None, &empty)
                .expect("兜底必须放行");
            assert_eq!(
                id, 2,
                "第 {round} 轮：轮转只在**同档内**打散，会自愈的 #2 必须一直优先于 \
                 认证失败 86400s 的 #1（否则兜底会把请求扔给铁定失败的深冷却号）"
            );
            drop(guard);
        }
    }

    /// 🔴 P6-a 回归：**轮转不能只在「同一个 60s 剩余档」内生效。**
    ///
    /// 上一版排序键第一维是 `冷却剩余 / 60`，前提是「所有会自愈的原因基线都 ≤ 60s」。
    /// 该前提在真实链路上不成立：429 的冷却时长**由上游 `Retry-After` 给**
    /// （`provider.rs:1770` 取响应头/body → 本文件钳到 600s），所以一个号拿 15s
    /// （裸 429）、另一个拿 90s（上游指定）是常态。
    ///
    /// 后果与旧实现同症状：只剩 #1 在第 0 档 ⇒ 第一维**重新把它钉住**，
    /// 而且它每被放行一次就再吃一个 429、重新拿一段短冷却继续留在第 0 档（自我维持）。
    /// 实测形态：#578 近 3h 拿 128 次兜底、单分钟峰值 63。
    ///
    /// # 回退即 FAIL（已实测）
    ///
    /// 把 `fallback_cooldown_tier` 换回 `remaining_secs / 60`：#1 落 0 档、#2#3 落 1 档
    /// ⇒ 九次全命中 #1，`distinct.len()==1`。
    ///
    /// # 夹具全部走真实入口
    ///
    /// 三个号都只经 `report_rate_limited_with_retry_after`（provider 上游 429 唯一入口）：
    /// `None` = 裸 429（固定基线 15s），`Some(90)` = 上游指定 90s。不直接摆冷却时长。
    #[test]
    fn test_fallback_rotation_spans_recoverable_credentials_of_unequal_cooldown() {
        let mut config = Config::default();
        config.cooldown_enabled = true;
        // 容量给大，免得 commit_selection 里的 rpm.record 把某个号排除掉。
        config.credential_rpm_limit = 1_000_000;
        let creds: Vec<KiroCredentials> = (0..3)
            .map(|i| {
                let mut c = KiroCredentials::default();
                c.access_token = Some(format!("tok{i}"));
                c.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());
                c
            })
            .collect();
        let mgr = MultiTokenManager::new(config, creds, None, None, true).unwrap();

        // #1 裸 429 → 15s（旧实现的第 0 档）；#2#3 上游给 90s（旧实现的第 1 档）。
        mgr.report_rate_limited_with_retry_after(1, None);
        mgr.report_rate_limited_with_retry_after(2, Some(90));
        mgr.report_rate_limited_with_retry_after(3, Some(90));
        // 夹具前提只用**与实现无关**的量表达：三个号真的在冷却，且按旧的 60s 档宽
        // 确实会被拆到不同档。故意不断言 FallbackCooldownTier —— 那是实现内部命名，
        // 断言它会让「回退即 FAIL」停在前提行，看不出承重断言到底有没有被守住。
        let rem = |id: u64| {
            mgr.cooldown
                .check_cooldown(id)
                .expect("前提：必须真的在冷却")
                .1
                .as_secs()
        };
        assert_ne!(
            rem(1) / 60,
            rem(2) / 60,
            "夹具前提：按旧的 60s 档宽算，#1 与 #2 必须落在不同档（实得 {}s vs {}s）",
            rem(1),
            rem(2)
        );

        let empty: HashSet<u64> = HashSet::new();
        let mut hits: Vec<u64> = Vec::new();
        for _ in 0..9 {
            let (id, _c, guard) = mgr
                .select_ignoring_cooldown(None, &empty)
                .expect("全在冷却时兜底必须放行一个号");
            hits.push(id);
            drop(guard); // 立刻归还在途名额，下一轮重新全平局
        }
        let distinct: HashSet<u64> = hits.iter().copied().collect();
        assert_eq!(
            distinct.len(),
            3,
            "冷却剩余不等但都会自愈的号必须一起参与轮转，实际只命中 {:?}（序列 {:?}）—— \
             只在同 60s 档内轮转 = 池里只剩一个号在最前档时它被重新钉住",
            distinct,
            hits
        );
        assert_eq!(
            hits,
            vec![1, 2, 3, 1, 2, 3, 1, 2, 3],
            "同档内仍按 id 升序从游标之后继续、到尾回绕（确定性，可复盘）"
        );
    }

    /// REST 端点候选：按 SSO region 前缀选主端点，另一个作 403 回退。
    ///
    /// # 为什么这条重要
    ///
    /// `management.{region}.kiro.dev` **只在 `us-east-1` / `eu-central-1` 解析**
    /// （其余 13 区 DNS 不通）。所以 SSO region 是 `eu-west-1` 之类的账号
    /// （Enterprise / IdC 常见）按自己 region 拼 host 必然失败，上游回
    /// `403 {"message":"Invalid token"}` —— 那个文案会让人误判成 token 坏了。
    ///
    /// 用**前缀**而非精确匹配：`eu-west-1` / `eu-north-1` 的账号虽然端点在
    /// `eu-central-1`，但它们是欧洲账号，先试欧洲命中率更高。
    ///
    /// 回退即 FAIL：把 `starts_with("eu-")` 改成 `== "eu-central-1"` →
    /// `eu-west-1` 那条断言失败（那正是 Enterprise 账号被 403 的形态）。
    #[test]
    fn test_rest_api_region_candidates_prefers_matching_continent() {
        // 欧洲账号（含非 eu-central-1 的欧洲区）→ 先 eu。
        for r in ["eu-central-1", "eu-west-1", "eu-north-1", "eu-south-2"] {
            assert_eq!(
                rest_api_region_candidates(r),
                ["eu-central-1", "us-east-1"],
                "{r} 是欧洲账号，应先试 eu-central-1 再回退 us-east-1"
            );
        }
        // 其余一律先 us（含空串与 ap-*：端点不存在，先试命中率更高的 us）。
        for r in ["us-east-1", "us-west-2", "ap-northeast-1", "", "unknown"] {
            assert_eq!(
                rest_api_region_candidates(r),
                ["us-east-1", "eu-central-1"],
                "{r} 应先试 us-east-1 再回退 eu-central-1"
            );
        }
        // 两个候选必须互不相同，否则回退等于重试同一个端点。
        for r in ["eu-west-1", "us-east-1"] {
            let c = rest_api_region_candidates(r);
            assert_ne!(c[0], c[1], "回退候选不能与主端点相同");
        }
    }

    /// `get_usage_limits` 必须**只对 403** 回退端点，且必须真的有回退循环。
    ///
    /// 源码级守卫：该函数打真实上游，穿它的成功/403 路径测不了。
    /// 「只对 403」是承重的 —— 401 是 token 真废、429 是限流，换端点都没意义，
    /// 对它们回退只会把每次失败的上游往返翻倍。
    #[test]
    fn test_usage_limits_falls_back_only_on_403() {
        let src = include_str!("token_manager.rs");
        let cut = src.find("#[cfg(test)]").unwrap_or(src.len());
        let prod = &src[..cut];
        let start = prod
            .find("pub(crate) async fn get_usage_limits(")
            .expect("get_usage_limits 不该被改名");
        // 取到下一个顶层 fn 为止，避免匹配到后面别的函数。
        let body_rest = &prod[start..];
        let end = body_rest[1..]
            .find("\npub(crate) async fn ")
            .or_else(|| body_rest[1..].find("\nfn "))
            .map(|i| i + 1 + start)
            .unwrap_or(prod.len());
        let body = &prod[start..end];

        // needle 运行时拼接（include_str! 自匹配坑，本仓库踩过三次）。
        let cands = format!("rest_api_region_candidates{}", "(region)");
        assert!(
            body.contains(&cands),
            "必须用 rest_api_region_candidates 取候选端点，否则单一 region 错了就直接失败"
        );
        // 状态码现由 fetch_usage_limits_once 以 Err((Option<u16>, String)) 回传，
        // 故门控写成 `status == Some(403)`（原字面量 `status.as_u16() == 403` 已不存在）。
        // 语义未变：仍是「403 且仍有候选」才回退。
        let guard = format!("status == Some(403) && idx + 1 <{}", " candidates.len()");
        assert!(
            body.contains(&guard),
            "回退必须门控在 403 且仍有候选：401(token 真废)/429(限流)换端点都没意义，\
             对它们回退只会把失败的上游往返翻倍"
        );
        // 单区查询必须存在：本函数的回退循环就靠它做「一次一个区」。
        // ⚠️ 它**不再**服务 region 探测（2026-08-06 起探测改打 q.* 真实对话端点，
        // 见本文件 get_usage_limits 上方那段注释），所以别再把「探测依赖它」
        // 当作保留理由 —— 现在的理由只是本函数的回退循环需要它。
        let single = format!("async fn fetch_usage_limits_once{}", "(");
        assert!(
            prod.contains(&single),
            "get_usage_limits 的 403 换区回退依赖单区查询 fetch_usage_limits_once，\
             把它内联回去就没法「一次一个区」了"
        );
    }

    /// `ids_needing_region_probe` 的判据表：只挑「api_key 且完全无 region 字段」的。
    ///
    /// 每一条排除都有代价理由：
    /// - 带 region 的号 → 那是推号方/运维的明确意图，探测覆盖它就是擅自改配置。
    /// - OAuth 号 → region 由 profileArn 决定（`effective_upstream_region` 第一优先），
    ///   探它既无必要又会与那条路径打架。
    /// - 已禁用的号 → 探一个不参与调度的号纯属白打上游往返。
    #[test]
    fn test_ids_needing_region_probe_selects_only_regionless_api_keys() {
        let mut want = KiroCredentials::default(); // ✅ 应被选中
        want.auth_method = Some("api_key".to_string());
        want.kiro_api_key = Some("ksk_want".to_string());

        let mut has_api_region = KiroCredentials::default(); // ❌ 已有 apiRegion
        has_api_region.auth_method = Some("api_key".to_string());
        has_api_region.kiro_api_key = Some("ksk_has_api".to_string());
        has_api_region.api_region = Some("us-east-1".to_string());

        let mut has_region = KiroCredentials::default(); // ❌ 已有 region
        has_region.auth_method = Some("api_key".to_string());
        has_region.kiro_api_key = Some("ksk_has_region".to_string());
        has_region.region = Some("eu-central-1".to_string());

        let mut has_auth_region = KiroCredentials::default(); // ❌ 已有 authRegion
        has_auth_region.auth_method = Some("api_key".to_string());
        has_auth_region.kiro_api_key = Some("ksk_has_auth".to_string());
        has_auth_region.auth_region = Some("us-west-2".to_string());

        let mut oauth = KiroCredentials::default(); // ❌ 非 api_key
        oauth.refresh_token = Some("rt".to_string());

        let mgr = MultiTokenManager::new(
            Config::default(),
            vec![want, has_api_region, has_region, has_auth_region, oauth],
            None,
            None,
            true,
        )
        .unwrap();

        let ids = mgr.ids_needing_region_probe();
        assert_eq!(
            ids,
            vec![1],
            "只有「api_key 且三个 region 字段全空」的 #1 该被探；实际: {ids:?}"
        );

        // 已禁用的号不该再被探（白打上游往返）。
        mgr.set_disabled(1, true).unwrap();
        assert!(
            mgr.ids_needing_region_probe().is_empty(),
            "已禁用的号不参与调度，探它没有意义"
        );
    }

    /// region 探测必须接在 `add_credential` 里、且在 `get_usage_limits_for` **之前**。
    ///
    /// 源码级守卫的理由：`add_credential` 会打真实上游（`get_usage_limits_for` /
    /// `probe_api_region` 都是网络往返），穿它的成功路径测不了。
    ///
    /// 顺序断言是承重的：`get_usage_limits_for` 打的正是
    /// `management.{region}.kiro.dev` —— region 错时它自己就会 403。放在它之后等于
    /// 先让订阅探测白失败一次，而那次失败还会置位 FEATURE_NOT_SUPPORTED 标记。
    #[test]
    fn test_region_probe_wired_before_usage_limits_in_add_credential() {
        let src = include_str!("../admin/service.rs");
        let cut = src.find("#[cfg(test)]").unwrap_or(src.len());
        let prod = &src[..cut];
        // needle 运行时拼接：include_str! 会把测试自己的字面量也读进来（本仓库踩过三次）。
        let probe = format!("probe_and_persist_api_region{}", "(credential_id)");
        let usage = format!("get_usage_limits_for{}", "(credential_id)");
        let pi = prod
            .find(&probe)
            .expect("add_credential 必须调 probe_and_persist_api_region（否则新号仍靠 config.region 赌运气）");
        // ⚠️ 从 probe **之后**找 usage：SSO 导入路径（service.rs 更早处）也有一次
        // get_usage_limits_for(credential_id)（拉订阅等级，SSO 带 region 参数无需探测），
        // find-first 会被它污染——顺序断言只关心 add_credential_with_intent 内的
        // probe→usage 相对顺序。
        let ui = prod[pi..]
            .find(&usage)
            .expect("订阅等级探测调用点不该被改名")
            + pi;
        assert!(
            pi < ui,
            "region 探测必须在 get_usage_limits_for 之前：后者打的就是 \
             management.{{region}}.kiro.dev，region 错时它自己会 403"
        );
    }

    /// 存量号回填必须是**后台 spawn + 串行 + 有间隔**，不能进启动关键路径。
    ///
    /// 源码守卫：这是进程启动行为，单测跑不到。三条都断言，因为每一条被违反都有
    /// 具体代价 —— 不 spawn 会拖慢启动（服务无法立刻收流量）；并发探会在同出口 IP
    /// 上打出一批 management 请求（风控要抓的突发特征）。
    #[test]
    fn test_region_backfill_is_backgrounded_and_serialized() {
        let src = include_str!("../main.rs");
        let cut = src.find("#[cfg(test)]").unwrap_or(src.len());
        let prod = &src[..cut];
        let marker = format!("ids_needing_region_probe{}", "()");
        let mi = prod.find(&marker).expect("启动路径必须有存量 region 回填");
        // 回填块内必须有 spawn（后台）与 sleep（间隔）。取该标记前后一段窗口来判断。
        let win_start = prod[..mi]
            .rfind("tokio::spawn")
            .expect("回填必须在 tokio::spawn 内");
        let win = &prod[win_start..];
        let win_end = win.find("\n    }").map(|i| i).unwrap_or(win.len());
        let block = &win[..win_end];
        let sleep_needle = format!("tokio::time::sleep{}", "(");
        assert!(
            block.matches(&sleep_needle).count() >= 2,
            "回填块内应有两处 sleep：启动后延迟 + 每号之间的间隔（避免同 IP 突发探测）"
        );
        let probe_call = format!("probe_and_persist_api_region{}", "(id)");
        assert!(block.contains(&probe_call), "回填块必须真的逐个调探测");
        // 串行守卫：块内不得再出现 spawn（那意味着把每个号的探测并发出去了）。
        assert!(
            !block[probe_call.len()..].contains("tokio::spawn"),
            "存量回填必须串行 —— 并发探测会在同出口 IP 上打出一批 management 请求"
        );
    }

    /// 🔴 爬坡档必须排在 `inflight` **之前** —— 这是治正反馈的关键。
    ///
    /// # 为什么顺序是承重的（线上实测）
    ///
    /// 429 在 ~1s 返回，成功要 3s+。所以**正在被打爆的号 inflight 反而恒低**：
    /// 实测 507（97% 429）inflight=1，而健康的 508（0% 429）inflight=13。
    /// `inflight` 是升序键 ⇒ 若它排在爬坡档之前，失败越快的号越显得空闲、
    /// 越被优先选中 —— **失败本身让它在排序里变好看**，正反馈。
    ///
    /// 这与 ZyphrZero v0.7.1 打的那个补丁是同一类缺陷（他们那条是 `success_count`：
    /// 被限流的号从没成功过、恒为 0，反而成了全场"最少使用"）。
    ///
    /// 回退即 FAIL：把 `ramp_tier` 挪到 `inflight_now` 之后 → 本条失败。
    #[test]
    fn test_ramp_tier_outranks_inflight_in_sort_key() {
        let src = include_str!("token_manager.rs");
        let cut = src.find("#[cfg(test)]").unwrap_or(src.len());
        let prod = &src[..cut];
        // ⚠️ 判据**不能带注释里的对齐空格**：本条曾写成
        // `format!("ramp_tier,{}", "               // ⑤")`，而实际源码是 14 个空格 ——
        // needle 从未命中过，`find` 直接 panic，于是这条「顺序守卫」上线即失效。
        // 现在改成「在排序键元组这一段里比较各字段名的出现位置」，
        // 与注释编号、缩进、rustfmt 全部无关，只与真正承重的那件事（顺序）有关。
        //
        // 必须**先把窗口收到元组内**：`inflight_now,` 在闭包上方还有一处
        // （作为 `p_avail_with_load_ref` 的实参），不收窗口会比到那一处上去。
        // 元组首尾两个字段名在生产段各只出现一次，故这两个锚点是唯一的。
        let tuple_start = prod
            .find("unusable,")
            .expect("排序键元组第①位必须是 unusable");
        // ⚠️ 2026-08-14 同步：⑬ priority_tiebreaker 已删除（与③ prio_key 同一表达
        // 式，数学上不改变排序结果——全面 review MAJOR-2），元组恢复 12 位，
        // 末位锚点改为唯一出现的 e.success_count。
        // ⚠️ 2026-08-16 同步：Q8 契约化把 ⑬ e.id 收进第⑫位二元组
        // `(e.success_count, e.id)`（显式 tie-break，id=创建序=下标序）。
        // 窗口末锚仍用 e.success_count（元组内唯一出现）；本守卫只钉窗口内
        // 各字段的相对顺序。12 位 Ord 结构由
        // `select_sort_key_tuple_still_has_twelve_components_including_success_count` 另钉。
        let tuple_end = prod
            .find("e.success_count,")
            .expect("排序键元组第⑫位必须是 e.success_count");
        assert!(
            tuple_start < tuple_end,
            "锚点顺序异常，说明排序键元组结构已大改，本守卫需重写"
        );
        let tuple = &prod[tuple_start..tuple_end];

        let pos = |field: &str| -> usize {
            tuple
                .find(field)
                .unwrap_or_else(|| panic!("排序键元组里必须有 {field}"))
        };
        let hi = pos("health_tier,");
        let ri = pos("ramp_tier,");
        let ii = pos("inflight_now,");
        assert!(
            ri < ii,
            "ramp_tier 必须排在 inflight_now 之前：429 在 ~1s 返回而成功要 3s+，\
             正在被打爆的号 inflight 反而恒低，让 inflight 先排会形成\
             「失败越快越被优先选」的正反馈"
        );
        // 同时钉住它排在 health_tier **之后**：真坏号仍先沉档，爬坡只在同健康档内分流。
        assert!(
            hi < ri,
            "health_tier 必须仍排在 ramp_tier 之前：坏号沉档优先于爬坡分流"
        );
        // 2026-08-14 新增两位的顺序契约（与排序键闭包内的注释同步）：
        // - whitelist_hit（白名单显式路由软因子）夹在 ramp_tier 与 inflight_now 之间：
        //   健康/爬坡仍是主键，白名单命中只在同健康同爬坡档内做路由偏好，坏号不因
        //   白名单命中而插队；同时它又必须先于 inflight，保证显式路由优先于负载平局。
        // - model_calls_now（模型级近期调用数）排在 inflight_now 之后：
        //   总在途仍是抗惊群主键，模型维度只做同档细分，不能掀翻总在途分流。
        let wi = pos("whitelist_hit,");
        let mi = pos("model_calls_now,");
        assert!(
            ri < wi && wi < ii,
            "whitelist_hit 必须排在 ramp_tier 之后、inflight_now 之前：健康/爬坡优先于\
             显式路由偏好，且路由偏好优先于负载平局"
        );
        assert!(
            ii < mi,
            "model_calls_now 必须排在 inflight_now 之后：总在途仍是抗惊群主键，\
             模型维度只做同档细分"
        );
        // 2026-08-15 补钉：⑨⑩⑪ 三位（高基数负载维度）的相对顺序 —— 在途/自身容量
        // 千分比（容量归一压力）先于 RPM 已用率（速率分流），p_avail 精细兜底恒在
        // 其后（只剩 ⑫ success_count 垫底）。回退即 FAIL：把三者任一挪位，对应断言失败。
        let si = pos("slot_pressure_permille,");
        let rui = pos("rpm_usage_permille,");
        let npi = pos("neg_p_fine,");
        assert!(
            mi < si,
            "slot_pressure_permille 必须排在 model_calls_now 之后（⑧⑨ 相邻，负载细分归堆）"
        );
        assert!(
            si < rui,
            "slot_pressure_permille 必须排在 rpm_usage_permille 之前：\
             容量归一压力先于速率已用率（与 ⑨⑩ 注释同步）"
        );
        assert!(
            rui < npi,
            "rpm_usage_permille 必须排在 neg_p_fine 之前：p_avail 精细值恒为末段兜底，\
             不能掀翻容量/速率分流"
        );
    }

    /// P2-8：min_by_key 仍是 12 分量，末位含 `e.success_count`（与 id 合成二元组）。
    /// 回退即 FAIL：增删键、把 success_count 挪出元组、或拆成第 13 位。
    #[test]
    fn select_sort_key_tuple_still_has_twelve_components_including_success_count() {
        let src = include_str!("token_manager.rs");
        let cut = src.find("#[cfg(test)]").unwrap_or(src.len());
        let prod = &src[..cut];
        let start = prod
            .find("unusable,")
            .expect("排序键元组第①位必须是 unusable");
        let sc = format!("e.success_count{}", ",");
        let end = prod.find(&sc).expect("排序键元组第⑫位必须含 e.success_count");
        assert!(
            start < end,
            "锚点顺序异常，说明排序键元组结构已大改，本守卫需重写"
        );
        let win = &prod[start..end + sc.len()];
        let fields = [
            format!("unusable{}", ","),
            format!("starved{}", ","),
            format!("prio_key{}", ","),
            format!("health_tier{}", ","),
            format!("ramp_tier{}", ","),
            format!("whitelist_hit{}", ","),
            format!("inflight_now{}", ","),
            format!("model_calls_now{}", ","),
            format!("slot_pressure_permille{}", ","),
            format!("rpm_usage_permille{}", ","),
            format!("neg_p_fine{}", ","),
            sc.clone(),
        ];
        let mut last = 0usize;
        for (i, f) in fields.iter().enumerate() {
            let at = win
                .find(f.as_str())
                .unwrap_or_else(|| panic!("第{}位必须有 {f}", i + 1));
            assert!(
                at >= last,
                "第{}位 {f} 相对顺序被改（at={at} last={last}）",
                i + 1
            );
            last = at;
        }
        let id_bit = format!("e.id{}", ")");
        let after_sc = &prod[end..];
        let id_at = after_sc
            .find(&id_bit)
            .expect("⑫ 必须是 (e.success_count, e.id) 二元组");
        assert!(
            id_at < 80,
            "e.id 必须紧挨 success_count 组成第⑫位，不能另开第 13 键"
        );
    }

    /// ⭐ 源码级守卫（2026-08-15，M11）：透传池排序键必须含爬坡压力档，且排在
    /// `priority` 之后、`rpm_of` 之前（与主路径「ramp 先于 rpm/inflight」同序）。
    ///
    /// 历史缺陷：透传选号键只有 (priority, rpm, model_calls, inflight) —— rpm 是
    /// **绝对速率**，而上游惩罚的是**速率的跃升**（slew-rate，实测 ≥5x 跃升 48.3%
    /// 429 vs 平稳 0.7%），正在被猛灌的中转站绝对速率可能并不高，会被当"空闲"
    /// 继续选中。主路径 2026-08-04 已加爬坡档，透传池漏了（两池排序键分叉）。
    ///
    /// 行为测试不可行：`RpmTracker::record` 不带时间戳，灌不出「近 10s 猛灌但 60s
    /// 均量平稳」的历史分布，故用源码守卫钉「键存在 + 相对顺序」。
    ///
    /// ⚠️ 2026-08-16 同步（模型感知正向路由 S2）：元组起点锚从 `priority` 升为
    /// `support_rank,`——正向路由把支持档放在首位（设计文档 §3），此守卫顺带钉住
    /// 「support_rank 存在且在最前」；ramp/rpm 相对顺序断言不变。
    #[test]
    fn passthrough_sort_key_includes_ramp_tier_before_rpm() {
        let src = include_str!("token_manager.rs");
        let cut = src.find("#[cfg(test)]").unwrap_or(src.len());
        let prod = &src[..cut];
        // 窗口 = select_custom_api_inner 函数体（fn 锚 → 下一个 pub fn 锚）。
        let zone = prod
            .split("fn select_custom_api_inner")
            .nth(1)
            .expect("select_custom_api_inner 不应被改名");
        let zone = zone
            .split("pub fn has_other_custom_api_candidate")
            .next()
            .expect("函数体窗口锚不应被删改");
        // 元组窗口：起止锚在透传键内各只出现一次（该函数没有第二个 min_by_key）。
        // `support_rank,`（带逗号）在窗口内只出现在元组里（let 绑定是等号形态）。
        let start = zone
            .find("support_rank,")
            .expect("透传排序键首键必须是 support_rank（模型感知正向路由 S2）");
        let end = zone
            .find("e.inflight.load(Ordering::Acquire)")
            .expect("透传排序键必须含 inflight 键");
        // ⚠️ 2026-08-16 同步：透传键已扩到 8 位（support_rank 首位 + inflight 之后
        // 有失败余温位 + 末位 e.id 显式 tie-break），inflight 不再是末键；end 锚仍
        // 用它（元组内唯一出现），本守卫只钉窗口内 ramp_tier 与 rpm_of 的相对顺序，
        // 不受影响。
        let tuple = &zone[start..end];
        let ramp_at = tuple
            .find("ramp_tier,")
            .unwrap_or_else(|| panic!("透传排序键必须含 ramp_tier（与主路径同款爬坡档，防两池排序键分叉）"));
        let rpm_at = tuple
            .find("rpm_of(e.id)")
            .expect("透传排序键的 rpm 键不应被改名");
        assert!(
            ramp_at < rpm_at,
            "ramp_tier 必须排在 rpm_of 之前：正在被猛灌的中转站绝对速率可能并不高，\
             会当\"空闲\"被继续选中 —— 与主路径「ramp 先于 rpm_usage/inflight」同序"
        );
    }

    /// 爬坡档的判据表：样本不足不判、平稳=0、2~5x=1、≥5x=2。
    ///
    /// 「样本不足不判」是承重的：新入池的号窗口内只有几个请求，比值会剧烈抖动
    /// （1→3 就是 3x）。对它判高档只会压住它不给流量，而新号**应该**被逐步加量。
    #[test]
    fn test_ramp_counts_and_tier_thresholds() {
        use crate::kiro::scheduling::{RAMP_MIN_SAMPLES, RAMP_RECENT_SECS, RpmTracker};
        let t = RpmTracker::new();
        // 空追踪器：两个计数都是 0 ⇒ 样本不足 ⇒ 不判。
        let m = t.ramp_counts_for(&[1], std::time::Duration::from_secs(10));
        assert_eq!(m.get(&1).copied(), Some((0, 0)), "空号应返回 (0,0)");

        // 灌 30 次（全部落在近 10s 内）⇒ recent=30, total=30。
        for _ in 0..30 {
            t.record(1);
        }
        let (recent, total) = t.ramp_counts_for(&[1], std::time::Duration::from_secs(10))[&1];
        assert_eq!(total, 30, "窗口内应有 30 次");
        assert_eq!(recent, 30, "刚记的 30 次都应落在近 10s 内");
        // 折算：30 × (60/10) = 180，base=30 ⇒ 180 >= 30*5 ⇒ 档位 2（≥5x）。
        // 这正是「突然灌满」的形态：整个窗口的量全挤在最近 10 秒。
        // 与生产共用同一折算常量（RPM_WINDOW_SECS / RAMP_RECENT_SECS）。
        let projected = recent as u64 * (crate::kiro::scheduling::RPM_WINDOW_SECS
            / RAMP_RECENT_SECS as u64);
        assert!(
            projected >= total.max(1) as u64 * 5,
            "全部请求挤在近 10s ⇒ 必须判最高爬坡档（这就是 507 在 23:08 的形态）"
        );

        // 样本数低于阈值时不该判（新号保护）。
        let t2 = RpmTracker::new();
        for _ in 0..(RAMP_MIN_SAMPLES - 1) {
            t2.record(9);
        }
        let (_, total2) = t2.ramp_counts_for(&[9], std::time::Duration::from_secs(10))[&9];
        assert!(
            total2 < RAMP_MIN_SAMPLES,
            "低于 RAMP_MIN_SAMPLES 的样本必须走「不判」分支，否则新号被压住拿不到流量"
        );
    }

    /// ⭐ 守卫 #8（模型感知正向路由，S2）：透传池排序键里 support_rank **首位**
    /// （优先级之前）——目录 Confirmed 的号先于 priority 更优的 Unknown 号。
    /// 命名对齐 `test_ramp_tier_outranks_inflight_in_sort_key` 先例（CURRENT.md 守卫清单）。
    ///
    /// 行为断言：号 A priority=10 但目录 Confirmed；号 B priority=0 但目录明确不含
    /// （Unsupported）。若 support_rank 不生效，B（priority 0 更优）必胜；生效则 A 胜。
    /// 设计文档 §3：正向证据（目录）比静态配置（priority）更接近「该号能服务该模型」
    /// 的事实，support_rank 压过 priority 是有意行为变化。
    #[test]
    fn test_support_rank_outranks_priority_in_passthrough_sort_key() {
        use std::collections::HashSet;
        let empty = HashSet::new();

        // 号 A：priority=10（差），目录含 gpt-5.6-sol → Confirmed。
        let a = mk_custom(1, 10, None);
        let mgr = MultiTokenManager::new(
            Config::default(),
            vec![a.clone(), mk_custom(2, 0, None)],
            None,
            None,
            false,
        )
        .unwrap();
        // 直接写目录（巡检之外的测试注入，行为等价 store_model_catalog 产物）。
        mgr.model_catalog_cache.lock().insert(
            1,
            ModelCatalogEntry {
                models: vec!["gpt-5.6-sol".to_string()],
                refreshed_at: Instant::now(),
            },
        );
        // 号 B（id=2）无目录 → Unknown；A 目录含 → Confirmed（rank 0）胜出。
        let sel = mgr
            .select_custom_api(&empty, Some("gpt-5.6-sol"))
            .expect("有 Confirmed 号时必须选出");
        assert_eq!(sel.0, 1, "support_rank 首位：Confirmed(0) 必须先于 priority 更优的 Unknown");
        assert!(
            mgr.model_support(1, "gpt-5.6-sol") == ModelSupport::Confirmed
                && mgr.model_support(2, "gpt-5.6-sol") == ModelSupport::Unknown,
            "前置状态：A Confirmed、B Unknown"
        );
        // 对照：同目录下 B 明确不含 → Unsupported(2)，A 仍胜。
        mgr.model_catalog_cache.lock().insert(
            2,
            ModelCatalogEntry {
                models: vec!["deepseek-chat".to_string()],
                refreshed_at: Instant::now(),
            },
        );
        let sel2 = mgr
            .select_custom_api(&empty, Some("gpt-5.6-sol"))
            .expect("仍有 Confirmed 号时必须选出");
        assert_eq!(
            sel2.0, 1,
            "Confirmed(0) < Unknown(1) < Unsupported(2)：A 恒胜，B 目录明确不含只压后不出局"
        );
    }

    /// 三态排序语义（S2 验收 5）：Confirmed < Unknown < Unsupported，同档内 priority
    /// 仍生效（原排序键语义不变）。
    #[test]
    fn support_rank_tiers_and_priority_within_tier() {
        use std::collections::HashSet;
        let empty = HashSet::new();

        // 三个号：A Confirmed（priority=2）、B Confirmed（priority=1）、C Unsupported（priority=0）。
        let a = mk_custom(1, 2, None);
        let b = mk_custom(2, 1, None);
        let c = mk_custom(3, 0, None);
        let mgr = MultiTokenManager::new(
            Config::default(),
            vec![a.clone(), b.clone(), c.clone()],
            None,
            None,
            false,
        )
        .unwrap();
        mgr.model_catalog_cache.lock().insert(
            1,
            ModelCatalogEntry {
                models: vec!["gpt-5.6-sol".to_string()],
                refreshed_at: Instant::now(),
            },
        );
        mgr.model_catalog_cache.lock().insert(
            2,
            ModelCatalogEntry {
                models: vec!["gpt-5.6-sol".to_string()],
                refreshed_at: Instant::now(),
            },
        );
        mgr.model_catalog_cache.lock().insert(
            3,
            ModelCatalogEntry {
                models: vec!["deepseek-chat".to_string()],
                refreshed_at: Instant::now(),
            },
        );
        // 同 Confirmed 档内按 priority 升序：B（priority=1）先于 A（priority=2）。
        let sel = mgr
            .select_custom_api(&empty, Some("gpt-5.6-sol"))
            .expect("必须有候选");
        assert_eq!(sel.0, 2, "同档内 priority 语义不变：Confirmed 档内 B(1) < A(2)");
        // 排除 B 后选 A（仍是 Confirmed）；排除 A/B 后只剩 C（Unsupported 压后但**仍可选**）。
        let mut ex = HashSet::new();
        ex.insert(2);
        let sel2 = mgr
            .select_custom_api(&ex, Some("gpt-5.6-sol"))
            .expect("Confirmed 号 A 必须被选");
        assert_eq!(sel2.0, 1);
        let mut ex2 = HashSet::new();
        ex2.insert(1);
        ex2.insert(2);
        let sel3 = mgr
            .select_custom_api(&ex2, Some("gpt-5.6-sol"))
            .expect("全 Unsupported 退化放行：Unsupported 号不出局，必须仍可选");
        assert_eq!(sel3.0, 3, "Unsupported 只是压后，不返 None、不落 Kiro（验收 3）");
    }

    /// 验收 3 单独钉死：全候选 Unsupported 时 select 仍返回号（不返 None、不落 Kiro）。
    #[test]
    fn support_rank_all_unsupported_degrades_open() {
        use std::collections::HashSet;
        let empty = HashSet::new();
        let mgr = MultiTokenManager::new(
            Config::default(),
            vec![mk_custom(1, 0, None), mk_custom(2, 1, None)],
            None,
            None,
            false,
        )
        .unwrap();
        // 两号目录都不含目标 → 全 Unsupported。
        for id in [1u64, 2] {
            mgr.model_catalog_cache.lock().insert(
                id,
                ModelCatalogEntry {
                    models: vec!["deepseek-chat".to_string()],
                    refreshed_at: Instant::now(),
                },
            );
        }
        let sel = mgr.select_custom_api(&empty, Some("gpt-5.6-sol"));
        assert!(
            sel.is_some(),
            "全 Unsupported 必须退化放行（排序压后不是 filter 出局）——\
             返回 None 会让唯一候选场景错误地落 Kiro"
        );
        // 同档（全 Unsupported）内原 7 键语义不变：priority 升序，号 1（0）胜。
        assert_eq!(sel.unwrap().0, 1, "全 Unsupported 同档内原 7 键语义不变");
    }

    /// 验收 4：黑名单（负向硬门）优先于正向 Confirmed——filter 先于排序，
    /// 黑名单命中的号即使 Confirmed 也出局。
    #[test]
    fn blacklist_still_hard_gate_over_support_rank() {
        use std::collections::HashSet;
        let empty = HashSet::new();
        let a = mk_custom(1, 0, None);
        let mgr = MultiTokenManager::new(
            Config::default(),
            vec![a.clone(), mk_custom(2, 0, None)],
            None,
            None,
            false,
        )
        .unwrap();
        mgr.model_catalog_cache.lock().insert(
            1,
            ModelCatalogEntry {
                models: vec!["gpt-5.6-sol".to_string()],
                refreshed_at: Instant::now(),
            },
        );
        // 号 A Confirmed 但被黑名单（原始名键，与 mark 侧一致）。
        mgr.mark_model_unsupported(1, "gpt-5.6-sol");
        let sel = mgr
            .select_custom_api(&empty, Some("gpt-5.6-sol"))
            .expect("号 2（Unknown）必须被选");
        assert_eq!(
            sel.0, 2,
            "黑名单 > 正向缓存：Confirmed 号被 filter 出局后 support_rank 根本看不到它"
        );
    }

    /// 正向判定键 = 改写后名（map_target；exempt 号回落原始名）——请求实际打到
    /// 上游的名字才与目录可比（设计文档 §4 核心决策）。
    #[test]
    fn support_rank_uses_mapped_target_name() {
        use std::collections::HashSet;
        let empty = HashSet::new();
        let mut cfg = Config::default();
        cfg.model_mapping
            .insert("gpt-5.6-sol".to_string(), "deepseek-v4-flash".to_string());
        // 号 A（priority=1）：目录含改写后名 deepseek-v4-flash → Confirmed。
        // 号 B（priority=0）：目录含原始名 gpt-5.6-sol → 对 A 的判定键（改写后名）不含
        // → B Unsupported。断言选 A：证明判定用的是改写后名（用原始名 B 会 Confirmed 胜出）。
        let mgr = MultiTokenManager::new(
            cfg.clone(),
            vec![mk_custom(1, 1, None), mk_custom(2, 0, None)],
            None,
            None,
            false,
        )
        .unwrap();
        mgr.model_catalog_cache.lock().insert(
            1,
            ModelCatalogEntry {
                models: vec!["deepseek-v4-flash".to_string()],
                refreshed_at: Instant::now(),
            },
        );
        mgr.model_catalog_cache.lock().insert(
            2,
            ModelCatalogEntry {
                models: vec!["gpt-5.6-sol".to_string()],
                refreshed_at: Instant::now(),
            },
        );
        let sel = mgr
            .select_custom_api(&empty, Some("gpt-5.6-sol"))
            .expect("必须有候选");
        assert_eq!(
            sel.0, 1,
            "判定键是改写后名：A（目录含 deepseek-v4-flash）Confirmed 胜于 B（原始名不参与）"
        );
        // exempt 号回落原始名：号 1 设 exempt → 判定键 = gpt-5.6-sol（原始名），
        // 目录（deepseek-v4-flash）不命中 → Unsupported；号 2 判定键 = 改写后名
        // deepseek-v4-flash，目录（gpt-5.6-sol）同样不命中 → Unsupported。
        // 同档内 priority 号 2（0）胜。若 exempt 失效（号 1 仍用改写后名判定），
        // 号 1 会 Confirmed（rank 0）胜出 → 断言失败，正确钉住回落语义。
        let mgr2 = MultiTokenManager::new(
            cfg,
            vec![mk_custom(1, 1, None), mk_custom(2, 0, None)],
            None,
            None,
            false,
        )
        .unwrap();
        mgr2.set_credential_model_mapping_exempt(1, Some(true)).unwrap();
        mgr2.model_catalog_cache.lock().insert(
            1,
            ModelCatalogEntry {
                models: vec!["deepseek-v4-flash".to_string()],
                refreshed_at: Instant::now(),
            },
        );
        mgr2.model_catalog_cache.lock().insert(
            2,
            ModelCatalogEntry {
                models: vec!["gpt-5.6-sol".to_string()],
                refreshed_at: Instant::now(),
            },
        );
        let sel2 = mgr2
            .select_custom_api(&empty, Some("gpt-5.6-sol"))
            .expect("必须有候选");
        assert_eq!(
            sel2.0, 2,
            "exempt 号判定键回落原始名：号 1 不因改写后名目录命中而 Confirmed"
        );
    }

    /// S1 纯判定：目录含 → Confirmed；不含 → Unsupported；大小写不敏感。
    #[test]
    fn support_for_three_state_transition() {
        let models = vec!["gpt-5.6-sol".to_string(), "deepseek-v4-flash".to_string()];
        assert_eq!(
            support_for("gpt-5.6-sol", &models),
            ModelSupport::Confirmed
        );
        assert_eq!(
            support_for("GPT-5.6-SOL", &models),
            ModelSupport::Confirmed,
            "判定必须大小写不敏感（eq_ignore_ascii_case）"
        );
        assert_eq!(
            support_for("claude-opus-5", &models),
            ModelSupport::Unsupported
        );
    }

    /// S1 查询层：无条目 → Unknown；新鲜条目 → Confirmed/Unsupported；
    /// 过期条目 → Unknown（TTL 语义，不依赖真实时间——直接构造过期条目）。
    #[test]
    fn model_support_ttl_and_missing_entry() {
        let mgr = MultiTokenManager::new(
            Config::default(),
            vec![mk_custom(1, 0, None)],
            None,
            None,
            false,
        )
        .unwrap();
        assert_eq!(
            mgr.model_support(1, "gpt-5.6-sol"),
            ModelSupport::Unknown,
            "无缓存条目 → Unknown"
        );
        mgr.model_catalog_cache.lock().insert(
            1,
            ModelCatalogEntry {
                models: vec!["gpt-5.6-sol".to_string()],
                refreshed_at: Instant::now(),
            },
        );
        assert_eq!(
            mgr.model_support(1, "gpt-5.6-sol"),
            ModelSupport::Confirmed,
            "新鲜条目含目标 → Confirmed"
        );
        assert_eq!(
            mgr.model_support(1, "claude-opus-5"),
            ModelSupport::Unsupported,
            "新鲜条目明确不含 → Unsupported"
        );
        // 过期条目：refreshed_at 人为拨旧超过 TTL。
        let stale = Instant::now() - StdDuration::from_secs(MODEL_CATALOG_TTL_SECS + 1);
        mgr.model_catalog_cache.lock().insert(
            1,
            ModelCatalogEntry {
                models: vec!["gpt-5.6-sol".to_string()],
                refreshed_at: stale,
            },
        );
        assert_eq!(
            mgr.model_support(1, "gpt-5.6-sol"),
            ModelSupport::Unknown,
            "条目过期 → 惰性判 Unknown（不依赖真实时间，构造过期条目）"
        );
    }

    /// S4 统一收口：invalidate 清缓存 + 退避 + 单飞锁三件套。
    #[test]
    fn invalidate_model_catalog_clears_all_state() {
        let mgr = MultiTokenManager::new(
            Config::default(),
            vec![mk_custom(1, 0, None)],
            None,
            None,
            false,
        )
        .unwrap();
        mgr.model_catalog_cache.lock().insert(
            1,
            ModelCatalogEntry {
                models: vec!["gpt-5.6-sol".to_string()],
                refreshed_at: Instant::now(),
            },
        );
        mgr.model_catalog_backoff
            .lock()
            .insert(1, CatalogBackoff { failures: 3, until: Instant::now() });
        mgr.model_catalog_locks
            .lock()
            .insert(1, Arc::new(TokioMutex::new(())));
        mgr.invalidate_model_catalog(1);
        assert!(
            mgr.model_catalog_cache.lock().is_empty()
                && mgr.model_catalog_backoff.lock().is_empty()
                && mgr.model_catalog_locks.lock().is_empty(),
            "失效必须清缓存 + 退避 + 单飞锁（防内存残留与旧锁无意义）"
        );
        assert_eq!(mgr.model_support(1, "gpt-5.6-sol"), ModelSupport::Unknown);
    }

    /// S4：改 base_url → 旧目录立即失效（Unknown）；改 api_key 同效。
    #[tokio::test]
    async fn set_custom_api_config_invalidates_catalog() {
        let mgr = MultiTokenManager::new(
            Config::default(),
            vec![mk_custom(1, 0, None)],
            None,
            None,
            false,
        )
        .unwrap();
        mgr.model_catalog_cache.lock().insert(
            1,
            ModelCatalogEntry {
                models: vec!["gpt-5.6-sol".to_string()],
                refreshed_at: Instant::now(),
            },
        );
        mgr.set_custom_api_config(1, Some("https://new.example.invalid".to_string()), None, None, false)
            .await
            .unwrap();
        assert_eq!(
            mgr.model_support(1, "gpt-5.6-sol"),
            ModelSupport::Unknown,
            "换上游后旧目录必须失效（Unknown），否则目录对新高地误导判定"
        );
    }

    /// S4：删号清目录缓存（防内存残留；restore 后重新巡检）。
    #[test]
    fn delete_credential_clears_model_catalog() {
        use std::collections::HashSet;
        let dir = std::env::temp_dir().join(format!(
            "kiro-mcat-del-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let cred_path = dir.join("credentials.json");
        let mgr = MultiTokenManager::new(
            Config::default(),
            vec![mk_custom(1, 0, None)],
            None,
            Some(cred_path.clone()),
            true,
        )
        .unwrap();
        mgr.model_catalog_cache.lock().insert(
            1,
            ModelCatalogEntry {
                models: vec!["gpt-5.6-sol".to_string()],
                refreshed_at: Instant::now(),
            },
        );
        mgr.model_catalog_backoff
            .lock()
            .insert(1, CatalogBackoff { failures: 2, until: Instant::now() });
        mgr.delete_credential_forced(1, true).unwrap();
        let empty = HashSet::new();
        assert!(
            mgr.select_custom_api(&empty, Some("gpt-5.6-sol")).is_none(),
            "号已删无候选"
        );
        assert!(
            mgr.model_catalog_cache.lock().is_empty()
                && mgr.model_catalog_backoff.lock().is_empty(),
            "删号必须清目录缓存与退避（防内存残留）"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// S3：mock fetch 成功（非空）→ 写缓存 + 重置退避；model_support 转 Confirmed。
    #[tokio::test]
    async fn probe_round_success_writes_catalog_and_resets_backoff() {
        let mgr = MultiTokenManager::new(
            Config::default(),
            vec![mk_custom(1, 0, None)],
            None,
            None,
            false,
        )
        .unwrap();
        // 先制造退避残留，验证成功重置。
        mgr.model_catalog_backoff
            .lock()
            .insert(1, CatalogBackoff { failures: 5, until: Instant::now() });
        let written = mgr
            .probe_model_catalog_round(|_cred| async {
                Ok(vec!["gpt-5.6-sol".to_string(), "deepseek-v4-flash".to_string()])
            })
            .await;
        assert_eq!(written, 1, "1 个号写缓存成功");
        assert_eq!(
            mgr.model_support(1, "gpt-5.6-sol"),
            ModelSupport::Confirmed,
            "成功巡检后目录含目标 → Confirmed"
        );
        assert!(
            mgr.model_catalog_backoff.lock().get(&1).is_none(),
            "成功（非空）必须重置退避"
        );
    }

    /// 守卫 #6：mock fetch 返空 → 不写缓存（保持 Unknown）、不退避、不重置退避。
    #[tokio::test]
    async fn probe_round_empty_list_does_not_write_cache() {
        let mgr = MultiTokenManager::new(
            Config::default(),
            vec![mk_custom(1, 0, None)],
            None,
            None,
            false,
        )
        .unwrap();
        let written = mgr
            .probe_model_catalog_round(|_cred| async { Ok(vec![]) })
            .await;
        assert_eq!(written, 0, "空列表不算写缓存成功");
        assert!(
            mgr.model_catalog_cache.lock().is_empty()
                && mgr.model_support(1, "gpt-5.6-sol") == ModelSupport::Unknown,
            "空列表必须不写缓存：固化成「无模型」会让目录失真永久化（守卫 #6）"
        );
        assert!(
            mgr.model_catalog_backoff.lock().get(&1).is_none(),
            "空列表不算失败：不引入退避（也不重置，下周期照常再探）"
        );
    }

    /// S3：mock fetch 失败 → 退避增长 + 缓存维持 Unknown。
    #[tokio::test]
    async fn probe_round_failure_bumps_backoff_and_keeps_unknown() {
        let mgr = MultiTokenManager::new(
            Config::default(),
            vec![mk_custom(1, 0, None)],
            None,
            None,
            false,
        )
        .unwrap();
        let written = mgr
            .probe_model_catalog_round(|_cred| async { Err(anyhow::anyhow!("上游 500")) })
            .await;
        assert_eq!(written, 0);
        assert!(
            mgr.model_catalog_in_backoff(1),
            "失败必须进退避（该号维持 Unknown = 排序中性，不惩罚）"
        );
        assert_eq!(mgr.model_support(1, "gpt-5.6-sol"), ModelSupport::Unknown);
        // 退避中再调一轮：跳过 fetch（计数不增），且退避状态保留到到期。
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let c2 = calls.clone();
        mgr.probe_model_catalog_round(move |_cred| {
            c2.fetch_add(1, Ordering::Relaxed);
            async { Err(anyhow::anyhow!("不应被调用")) }
        })
        .await;
        assert_eq!(calls.load(Ordering::Relaxed), 0, "退避中必须整轮跳过该号");
    }

    /// S3 验收 8：连续失败指数退避 60 → 120 → 240s → … → 上限 1800s；成功重置。
    ///
    /// 直调 `bump_model_catalog_backoff` 而非走 probe_round 循环：退避中的号会被
    /// probe_round 整轮跳过（设计语义：退避期间该号维持 Unknown、本轮不探），
    /// 指数增长只发生在「不在退避中的失败」，故 bump 本身单独测。
    #[test]
    fn probe_backoff_exponential_growth_and_cap() {
        let mgr = MultiTokenManager::new(
            Config::default(),
            vec![mk_custom(1, 0, None)],
            None,
            None,
            false,
        )
        .unwrap();
        let wait_of = |mgr: &MultiTokenManager| -> StdDuration {
            let b = mgr.model_catalog_backoff.lock();
            let s = b.get(&1).expect("必有退避");
            s.until.saturating_duration_since(Instant::now())
        };
        // 1 次失败：60s；2 次：120s；3 次：240s（2^(n-1) × 60）。
        mgr.bump_model_catalog_backoff(1);
        let w1 = wait_of(&mgr);
        mgr.bump_model_catalog_backoff(1);
        let w2 = wait_of(&mgr);
        mgr.bump_model_catalog_backoff(1);
        let w3 = wait_of(&mgr);
        assert!(w1 >= StdDuration::from_secs(60) - StdDuration::from_secs(5) && w1 <= StdDuration::from_secs(60));
        assert!(w2 >= StdDuration::from_secs(120) - StdDuration::from_secs(5) && w2 <= StdDuration::from_secs(120));
        assert!(w3 >= StdDuration::from_secs(240) - StdDuration::from_secs(5) && w3 <= StdDuration::from_secs(240));
        // 灌满到封顶：上限 30min 且不再增长。
        for _ in 0..10 {
            mgr.bump_model_catalog_backoff(1);
        }
        let w_cap = wait_of(&mgr);
        assert!(
            w_cap <= StdDuration::from_secs(MODEL_CATALOG_BACKOFF_MAX_SECS)
                && w_cap >= StdDuration::from_secs(MODEL_CATALOG_BACKOFF_MAX_SECS)
                    - StdDuration::from_secs(5),
            "退避必须封顶 30min（实测 {:?}）",
            w_cap
        );
    }

    /// S3 验收 9：并发两次 probe_round 只打一次网络（单飞锁 + 新鲜度 double-check）。
    #[tokio::test]
    async fn probe_round_singleflight_only_fetches_once() {
        let mgr = std::sync::Arc::new(
            MultiTokenManager::new(
                Config::default(),
                vec![mk_custom(1, 0, None)],
                None,
                None,
                false,
            )
            .unwrap(),
        );
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mk_round = |mgr: std::sync::Arc<MultiTokenManager>, calls: std::sync::Arc<std::sync::atomic::AtomicUsize>| {
            async move {
                mgr.probe_model_catalog_round(move |_cred| {
                    calls.fetch_add(1, Ordering::Relaxed);
                    async { Ok(vec!["gpt-5.6-sol".to_string()]) }
                })
                .await
            }
        };
        let (r1, r2) = tokio::join!(
            mk_round(mgr.clone(), calls.clone()),
            mk_round(mgr.clone(), calls.clone())
        );
        assert_eq!(calls.load(Ordering::Relaxed), 1, "并发两轮只打一次网络（单飞）");
        assert_eq!(r1 + r2, 1, "只有一路真正写缓存，另一路看到新鲜目录跳过");
        assert_eq!(
            mgr.model_support(1, "gpt-5.6-sol"),
            ModelSupport::Confirmed
        );
    }

    /// S3：禁用号不在巡检列表；ksk（api_key）号结构性排除（正向路由只做透传池）。
    #[tokio::test]
    async fn probe_round_skips_disabled_and_kiro_ids() {
        let kiro = mk_kiro(1, 0);
        let disabled_custom = mk_custom(2, 0, None);
        let mgr = MultiTokenManager::new(
            Config::default(),
            vec![kiro.clone(), disabled_custom.clone()],
            None,
            None,
            false,
        )
        .unwrap();
        mgr.set_disabled(2, true).unwrap();
        assert_eq!(
            mgr.ids_needing_model_probe(),
            Vec::<u64>::new(),
            "禁用号 + ksk 号都不该进巡检列表"
        );
    }

    /// 源码守卫（S3 接线守卫，对齐 CURRENT.md 守卫纪律：needle 运行时拼接 +
    /// 截断测试段）：巡检任务必须存在且形态钉死——30min 周期 + Skip + 首轮延迟 +
    /// 走 probe_model_catalog_round（内含 ids_needing_model_probe 数据源）+
    /// fetch_upstream_models + 退避。
    #[test]
    fn model_catalog_probe_task_is_wired() {
        let src = include_str!("token_manager.rs");
        let cut = src.find("#[cfg(test)]").unwrap_or(src.len());
        let prod = &src[..cut];
        // 窗口 = probe_model_catalog_round（循环主体 + 退避）→ spawn_model_catalog_probe
        // （任务形态）→ probe_and_persist_api_region（后继函数锚）。
        let fn_head = format!("pub async fn probe_model_catalog_round{}", "<");
        let start = prod
            .find(&fn_head)
            .expect("probe_model_catalog_round 不应被改名或删除");
        let tail = "pub async fn probe_and_persist_api_region";
        let end = prod[start..]
            .find(tail)
            .expect("巡检函数组后继不应被删改")
            + start;
        let body = &prod[start..end];
        let interval_needle = format!(
            "tokio::time::interval(StdDuration::from_secs(MODEL_CATALOG_TTL_SECS))"
        );
        assert!(
            body.contains(&interval_needle),
            "巡检任务周期必须是 MODEL_CATALOG_TTL_SECS（30min）"
        );
        assert!(
            body.contains("MissedTickBehavior::Skip"),
            "巡检任务必须 Skip 防唤醒后连刷"
        );
        let sleep_needle = format!("MODEL_CATALOG_PROBE_START_DELAY_SECS");
        assert!(
            body.contains(&sleep_needle),
            "巡检任务必须有首轮延迟（避开启动期上游往返）"
        );
        let round_needle = format!("probe_model_catalog_round{}", "(");
        assert!(
            body.contains(&round_needle),
            "巡检必须走 probe_model_catalog_round（单飞锁 + 空列表不写都在其中）"
        );
        assert!(
            body.contains("ids_needing_model_probe"),
            "巡检数据源必须是 ids_needing_model_probe（ksk/禁用号排除）"
        );
        assert!(
            body.contains("fetch_upstream_models"),
            "巡检数据源必须是 fetch_upstream_models（零新增网络代码）"
        );
        assert!(
            body.contains("bump_model_catalog_backoff"),
            "失败退避必须存在（指数退避，设计文档 §5 值）"
        );
    }

    /// 判据表逐项钉死：哪些算凭据级、哪些算瞬态。
    ///
    /// 带 [`RefreshHttpError`] 的按 `status` 精判 4xx（排除 429）。
    /// 无状态码才走字符串兜底（OAuth 永久拒绝 / api_key 不可刷新）。
    /// 特别锁住 **429 必须判瞬态** —— 那是限流，号是好的；把它算成凭据级
    /// 等于「上游拥堵 3 次 → 烧一个号」。
    #[test]
    fn test_refresh_error_classification_table() {
        for (status, s) in [
            (400u16, "400 Bad Request"),
            (401, "401 Unauthorized"),
            (403, "403 Forbidden"),
            (404, "404 Not Found"),
            (410, "410 Gone"),
            (422, "422 Unprocessable"),
        ] {
            let e: anyhow::Error = RefreshHttpError {
                status,
                message: s.to_string(),
            }
            .into();
            assert!(
                is_refresh_error_credential_level(&e),
                "{s}（status={status}）应判为凭据级"
            );
        }
        for s in [
            "invalid_grant",
            "invalid_client",
            "unauthorized_client",
            "API Key 凭据不支持刷新",
        ] {
            assert!(
                is_refresh_error_credential_level(&anyhow::anyhow!("{}", s)),
                "{s} 应判为凭据级"
            );
        }
        for s in [
            "500 Internal Server Error",
            "502 Bad Gateway",
            "503 Service Unavailable",
            "504 Gateway Timeout",
            "429 Too Many Requests",
            "400 Bad Request",
            "401 Unauthorized",
            "error sending request: connection reset by peer",
            "operation timed out",
            "dns error: failed to lookup address",
            "服务器错误",
            "暂时不可用",
        ] {
            assert!(
                !is_refresh_error_credential_level(&anyhow::anyhow!("{}", s)),
                "{s} 无 RefreshHttpError 时应判为瞬态（不得裸匹配码数字）"
            );
        }
        let too_many: anyhow::Error = RefreshHttpError {
            status: 429,
            message: "429 Too Many Requests".into(),
        }
        .into();
        assert!(
            !is_refresh_error_credential_level(&too_many),
            "429 RefreshHttpError 必须是瞬态（限流不是号坏）"
        );
        let upstream_5xx: anyhow::Error = RefreshHttpError {
            status: 503,
            message: "503 Service Unavailable".into(),
        }
        .into();
        assert!(
            !is_refresh_error_credential_level(&upstream_5xx),
            "5xx RefreshHttpError 必须是瞬态"
        );
    }

    /// URL 端口 / 字节数 / 毫秒含 4xx 数字的瞬态错误不得判凭据级。
    ///
    /// 回退即 FAIL：把 `is_refresh_error_credential_level` 改回 `s.contains("400")`
    /// → 本条立刻把 `127.0.0.1:4000` 当 400。
    #[test]
    fn refresh_error_port_in_url_is_not_credential_level() {
        let e = anyhow::anyhow!(
            "error sending request for url (http://127.0.0.1:4000/oauth2/token): \
             connection reset by peer (took 1400ms, 401 bytes)"
        );
        assert!(
            !is_refresh_error_credential_level(&e),
            "端口/字节数/毫秒里的 400/401 不得算凭据级"
        );
        let manager = MultiTokenManager::new(
            Config::default(),
            vec![KiroCredentials::default(), KiroCredentials::default()],
            None,
            None,
            false,
        )
        .unwrap();
        for _ in 0..(MAX_FAILURES_PER_CREDENTIAL * 2) {
            manager.report_refresh_failure_classified(1, &e);
        }
        let snap = manager.snapshot();
        let first = snap.entries.iter().find(|e| e.id == 1).unwrap();
        assert_eq!(
            first.refresh_failure_count, 0,
            "端口含 4xx 数字的瞬态错误不得入 refresh_failure_count"
        );
        assert!(!first.disabled, "端口误判不得禁用凭据");
    }

    /// 真 401 必须靠 `RefreshHttpError.status` 判凭据级（Display 里夹端口也不动摇）。
    #[test]
    fn refresh_http_error_401_is_credential_level() {
        let e: anyhow::Error = RefreshHttpError {
            status: 401,
            message: "Token 刷新失败: 401 Unauthorized http://idp.example:4000/".into(),
        }
        .into();
        assert!(
            is_refresh_error_credential_level(&e),
            "status=401 的 RefreshHttpError 必须是凭据级"
        );
    }

    /// 源码守卫：分类必须 downcast `RefreshHttpError`，禁止 Display 子串匹配码数字。
    #[test]
    fn refresh_error_classification_uses_refresh_http_status_not_digit_substring() {
        let src = include_str!("token_manager.rs");
        let cut = src.find("#[cfg(test)]").unwrap_or(src.len());
        let prod = &src[..cut];
        let start = prod
            .find("fn is_refresh_error_credential_level")
            .expect("is_refresh_error_credential_level 不应被改名或删除");
        let end = prod[start..]
            .find("struct StatsEntry")
            .expect("分类函数后继不应被删改")
            + start;
        let body = &prod[start..end];
        let n_downcast = format!("downcast_ref::<{}>()", "RefreshHttpError");
        assert!(
            body.contains(&n_downcast),
            "必须 downcast RefreshHttpError 按 status 精判"
        );
        let n_400 = format!("{}.contains(\"{}\")", "s", "400");
        let n_401 = format!("{}.contains(\"{}\")", "s", "401");
        assert!(
            !body.contains(&n_400) && !body.contains(&n_401),
            "禁止 Display 子串匹配状态码数字（端口/字节数/毫秒会误判烧号）"
        );
        assert!(
            body.contains("invalid_grant"),
            "无状态码时仍须兜底 invalid_grant"
        );
    }

    /// `TooManyRefreshFailures` 必须在全池自愈覆盖范围内。
    ///
    /// 旧行为：它不在 `is_self_healable_reason` 里 ⇒ 一次上游 token 端点抖动把全池
    /// 刷成该原因后，**自愈也救不回来**，必须人工去面板点启用。而它与
    /// `InvalidRefreshToken`（上游明确 `invalid_grant`）是两个不同信号，不该同等对待。
    ///
    /// 回退即 FAIL：从 `is_self_healable_reason` 里删掉该变体 → 本条失败。
    #[test]
    fn test_too_many_refresh_failures_is_self_healable() {
        assert!(
            is_self_healable_reason(Some(DisabledReason::TooManyRefreshFailures)),
            "连续刷新失败达阈值多半是上游抖动，必须可自愈"
        );
        // 配对：真作废的 refreshToken 绝不可自愈（复活只会白撞上游）。
        assert!(
            !is_self_healable_reason(Some(DisabledReason::InvalidRefreshToken)),
            "invalid_grant 是永久信号，绝不能被自愈复活"
        );
        assert!(!is_self_healable_reason(Some(
            DisabledReason::AccountSuspended
        )));
        assert!(!is_self_healable_reason(Some(DisabledReason::Manual)));
    }

    /// 全池被刷新失败打满时，`acquire_context` 必须报**可重试**的临时态（带
    /// `retry_after_secs=`），而不是落到无标记的兜底（那会被 `map_provider_error`
    /// 映射成 502 无 Retry-After，客户端不退避、原样重发）。
    ///
    /// # 语义变更说明（2026-08-04）
    ///
    /// 本测试原名 `..._is_not_auto_recovered`，断言错误串里含「所有凭据均已禁用」——
    /// 那锁的是旧行为：`TooManyRefreshFailures` **不在** `is_self_healable_reason` 内，
    /// 所以全池被它打满后永不自愈，只能人工去面板点启用。
    ///
    /// 该原因现已纳入自愈覆盖（见 `test_too_many_refresh_failures_is_self_healable`
    /// 的理由：走到阈值的典型成因是上游 token 端点抖了几十秒，凭据本身完好）。
    /// 于是这里会先被自愈复活、再因假 token 刷新失败落进 `TokenRefreshFailed` 冷却，
    /// 错误串变成「所有凭据均在冷却」。**两者都是可重试临时态**，所以断言改为钉
    /// 「必须带 retry_after_secs」这个真正承重的性质，而不是钉具体中文文案
    /// （文案一改分类就失效，这正是本仓库反复踩的那类坑）。
    #[tokio::test]
    async fn test_refresh_failure_exhausted_pool_reports_retryable() {
        let mut config = Config::default();
        // 全池冷却时立刻 bail 而不是等满 MAX_TRANSIENT_WAIT_SECS(20s)——本测试只关心
        // bail 出来的**错误串形态**，不关心它等多久。（自愈复活后拿假 token 刷新失败
        // 会落 TokenRefreshFailed 冷却 60s，默认配置下 acquire_context 会先等 20s。）
        config.all_cooling_fast_fail = true;
        let cred1 = KiroCredentials::default();
        let cred2 = KiroCredentials::default();

        let manager =
            MultiTokenManager::new(config, vec![cred1, cred2], None, None, false).unwrap();

        for _ in 0..MAX_FAILURES_PER_CREDENTIAL {
            manager.report_refresh_failure(1);
            manager.report_refresh_failure(2);
        }
        assert_eq!(manager.available_count(), 0);

        let err = tokio::time::timeout(
            std::time::Duration::from_secs(25),
            manager.acquire_context(None, None),
        )
        .await
        .expect("不应挂死")
        .err()
        .expect("池子已空，不该选出号")
        .to_string();

        assert!(
            err.contains("retry_after_secs="),
            "必须带 retry_after_secs 才能被 map_provider_error 映射成 429 + Retry-After\
             （无标记会落 502 兜底 → 客户端不退避 → 原样重发）: {err}"
        );
        assert!(
            !err.contains("pool_permanently_exhausted=1"),
            "TooManyRefreshFailures 已属可自愈原因，不得打永久耗尽标记\
             （否则吸收层会对一个几十秒后就会自己好的池直接放弃）: {err}"
        );
    }

    #[test]
    fn test_multi_token_manager_report_quota_exhausted() {
        let config = Config::default();
        let cred1 = KiroCredentials::default();
        let cred2 = KiroCredentials::default();

        let manager =
            MultiTokenManager::new(config, vec![cred1, cred2], None, None, false).unwrap();

        // 凭据会自动分配 ID（从 1 开始）
        assert_eq!(manager.available_count(), 2);
        assert!(manager.report_quota_exhausted(1));
        assert_eq!(manager.available_count(), 1);

        // 再禁用第二个后，无可用凭据
        assert!(!manager.report_quota_exhausted(2));
        assert_eq!(manager.available_count(), 0);
    }

    /// 源码守卫：配额耗尽告警必须在 402 路径、不得在 403 风控禁用路径。
    /// needle 运行时拼接 + 截断测试段。
    #[test]
    fn quota_exhausted_alert_bump_lives_in_report_quota_exhausted() {
        let src = include_str!("token_manager.rs");
        let cut = src.find("#[cfg(test)]").unwrap_or(src.len());
        let prod = &src[..cut];
        let needle = format!("{}{}{}", "bump(\"", "quota_exhausted", "\")");
        let cred_needle = format!("{}{}{}", "bump(\"", "credential_disabled", "\")");

        let q_head = format!("pub fn report_{}", "quota_exhausted");
        let q_start = prod
            .find(&q_head)
            .expect("report_quota_exhausted 不应被改名");
        let q_end = prod[q_start..]
            .find("fn recover_expired_quota_disables")
            .expect("report_quota_exhausted 的后继不应被改名")
            + q_start;
        let q_body = &prod[q_start..q_end];
        assert!(
            q_body.contains(&needle),
            "402 路径必须 bump 配额耗尽告警（否则真 402 永不触发）"
        );
        assert!(
            q_body.contains(&cred_needle),
            "402 禁用路径必须同时 bump credential_disabled"
        );

        let s_head = format!("pub fn report_{}", "suspicious_activity");
        let s_start = prod
            .find(&s_head)
            .expect("report_suspicious_activity 不应被改名");
        let s_end = prod[s_start..]
            .find("pub fn report_auth_cooldown")
            .expect("report_suspicious_activity 的后继不应被改名")
            + s_start;
        let s_body = &prod[s_start..s_end];
        assert!(
            !s_body.contains(&needle),
            "风控禁用路径不得 bump 配额耗尽告警（整族误报）"
        );
        assert!(
            s_body.contains(&cred_needle),
            "风控禁用路径必须保留 credential_disabled bump"
        );
    }

    /// 402 路径：真正禁用 + 落盘（告警 bump 由源码守卫钉位置；alerting 进程级
    /// 状态需 webhook init，本测不碰以免污染并行测试）。
    #[test]
    fn report_quota_exhausted_disables_and_persists() {
        let dir = std::env::temp_dir().join(format!(
            "kiro-quota-persist-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let cred_path = dir.join("credentials.json");

        let mut cred = KiroCredentials::default();
        cred.id = Some(1);
        cred.access_token = Some("t1".to_string());
        cred.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());
        let mgr = MultiTokenManager::new(
            Config::default(),
            vec![cred],
            None,
            Some(cred_path.clone()),
            true,
        )
        .unwrap();

        assert!(
            !mgr.report_quota_exhausted(1),
            "单号池额度用尽后应无可用凭据"
        );
        {
            let e = mgr.entries.lock();
            let entry = e.iter().find(|e| e.id == 1).unwrap();
            assert!(entry.disabled);
            assert_eq!(
                entry.disabled_reason,
                Some(DisabledReason::QuotaExceeded)
            );
            assert!(entry.quota_exhausted_at.is_some());
        }
        let back = crate::kiro::model::credentials::CredentialsConfig::load(&cred_path)
            .unwrap()
            .into_sorted_credentials();
        let b = back.iter().find(|c| c.id == Some(1)).unwrap();
        assert!(b.disabled, "402 禁用必须落盘");
        assert_eq!(
            b.disabled_reason,
            Some(DisabledReason::QuotaExceeded),
            "402 禁用原因必须落盘"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_multi_token_manager_report_account_suspended() {
        let config = Config::default();
        let cred1 = KiroCredentials::default();
        let cred2 = KiroCredentials::default();

        let manager =
            MultiTokenManager::new(config, vec![cred1, cred2], None, None, false).unwrap();

        assert_eq!(manager.available_count(), 2);

        // 封禁凭据 1：立即禁用并切换，仍有凭据 2 可用
        assert!(manager.report_account_suspended(1));
        assert_eq!(manager.available_count(), 1);

        // 封禁凭据 2 后无可用凭据
        assert!(!manager.report_account_suspended(2));
        assert_eq!(manager.available_count(), 0);
    }

    /// 回归（死号自动禁用 · 本轮核心）：连续账户级风控且期间零成功 → 自动禁用。
    ///
    /// **旧代码为何失败**：判据是 `cooldown.trigger_count(id) >= 10`，而
    /// `cooldown_enabled=false` 时 `set_cooldown` 根本不被调用 → `trigger_count`
    /// 恒 0 → 阈值永不可达。且整个自动禁用块被 `if cooldown_enabled` 包住，
    /// 该开关关闭时连计数都不发生。线上实测正是这个组合：8 个成功率恒 0% 的死号
    /// 跑几小时仍全部 `disabled=false`，每个请求都在它们身上白撞。
    #[test]
    fn test_suspicious_auto_disable_works_with_cooldown_disabled() {
        let mut config = Config::default();
        // 关键：冷却关闭。自动禁用**不得**依赖这个不相关的开关。
        config.cooldown_enabled = false;
        let manager = MultiTokenManager::new(
            config,
            vec![KiroCredentials::default(), KiroCredentials::default()],
            None,
            None,
            false,
        )
        .unwrap();

        // 阈值前一次：仍在池内（临时风控不能一见 403 就禁，会误伤健康号）
        for _ in 0..(MAX_CONSECUTIVE_SUSPICIOUS_BEFORE_DISABLE - 1) {
            manager.report_suspicious_activity(1);
        }
        assert_eq!(
            manager.available_count(),
            2,
            "未达阈值前不得禁用（403 是临时态，历史上误判成永久封禁造成过生产事故）"
        );

        // 达阈值：自动禁用，移出调度
        manager.report_suspicious_activity(1);
        assert_eq!(
            manager.available_count(),
            1,
            "连续 {} 次风控且零成功应自动禁用（旧代码 trigger_count 恒 0，永不触发）",
            MAX_CONSECUTIVE_SUSPICIOUS_BEFORE_DISABLE
        );
    }

    /// 回归（健康号永不误禁）：一次成功即清零连续风控计数。
    ///
    /// **旧代码为何失败**：计数依赖 `cooldown.trigger_count`，而 `report_success`
    /// 调 `clear_cooldown` → `entries.remove()` 删掉整个冷却条目 → 计数归零。
    /// 表面上"成功会清零"是对的，但副作用是**半死号也永远回不到阈值**，
    /// 于是自动禁用整体失效。新实现把计数挂在凭据条目上，与冷却条目解耦：
    /// 成功仍然清零（本测试），但失败能持续累加（上一个测试）。
    #[test]
    fn test_success_resets_suspicious_counter_so_healthy_cred_never_disabled() {
        let config = Config::default();
        let manager =
            MultiTokenManager::new(config, vec![KiroCredentials::default()], None, None, false)
                .unwrap();

        // 模拟实测中的健康号：偶发 403，但成功率 90~100%。
        // 循环远超阈值；若成功没能清零，早就被误禁了。
        for _ in 0..(MAX_CONSECUTIVE_SUSPICIOUS_BEFORE_DISABLE * 5) {
            for _ in 0..(MAX_CONSECUTIVE_SUSPICIOUS_BEFORE_DISABLE - 1) {
                manager.report_suspicious_activity(1);
            }
            manager.report_success(1); // 一次成功即清零
        }
        assert_eq!(
            manager.available_count(),
            1,
            "健康号（偶发风控但持续有成功）绝不能被自动禁用——这正是 403 不可按\
             『见过即封』处理的原因"
        );
    }

    /// 多开分身的族键必须是同一个 `clone:{group}`，而不是各自的 `cred:{id}`。
    ///
    /// 上游按账号记账（403 body 的 User ID 与 cred id 实测 N:1），多开只是把同一份
    /// 配额切成 N 份。若各自独立成族，一次账户级 suspend 要白挨 6×N 次上游 403。
    #[test]
    fn test_multi_open_copies_share_one_family() {
        const KEY: &str = "ksk_family_isolation_probe";
        const GROUP: &str = "00000000-0000-0000-0000-0000000000cc";

        let mk = |id: u64, mid: &str| {
            let mut c = KiroCredentials::default();
            c.id = Some(id);
            c.auth_method = Some("api_key".to_string());
            c.kiro_api_key = Some(KEY.to_string());
            c.clone_group = Some(GROUP.to_string());
            // 各份指纹不同（多开的真实价值所在），但这**不**让上游把它们当成两个账号。
            c.machine_id = Some(mid.to_string());
            c
        };
        let manager = MultiTokenManager::new(
            Config::default(),
            vec![
                mk(1306, &"a".repeat(64)),
                mk(1307, &"b".repeat(64)),
            ],
            None,
            None,
            true,
        )
        .unwrap();

        let f1 = manager.family_key_of(1306);
        let f2 = manager.family_key_of(1307);
        assert_eq!(
            f1, f2,
            "多开的两份必须同族键：上游按账号记账（403 body 的 User ID 与 cred id 实测 N:1），\
             各自独立成族只会让一次账户级 suspend 白挨 6×N 次上游 403"
        );
        assert_eq!(f1, format!("clone:{GROUP}"), "族键应取 clone:{{clone_group}}");
        assert!(
            !f1.starts_with("cred:"),
            "分身族键不应回退到 cred:{{id}}，否则收族未生效"
        );
    }

    /// 构造一个「同一把 key 的 N 份分身」号池（等价于线上 `credentials.json` 的形状）。
    ///
    /// 线上实测：17 份共享 keyhash `7d747fc003c9` 与同一个 `cloneGroup`，各自
    /// machineId / 代理不同。这里复刻该形状，`n` 份、同组、指纹各异。
    fn clone_family_pool(n: u64) -> Vec<KiroCredentials> {
        const GROUP: &str = "00000000-0000-0000-0000-0000000000dd";
        (0..n)
            .map(|i| {
                let mut c = KiroCredentials::default();
                c.id = Some(1306 + i);
                c.auth_method = Some("api_key".to_string());
                c.kiro_api_key = Some("ksk_one_account_many_copies".to_string());
                c.clone_group = Some(GROUP.to_string());
                // 指纹各异（多开的真实价值），但上游仍按账号记账。
                c.machine_id = Some(format!("{:064x}", i + 1));
                c
            })
            .collect()
    }

    /// ⭐ 核心回归（2026-08-07 线上事故）：一个上游账号被 403 suspend 时，
    /// **整族分身只需数满一次阈值**即全部退出调度 —— 而不是每份各自数满 6 次。
    ///
    /// # 事故复现（实测数据）
    ///
    /// 线上 17 份分身共享一把 key。按号计数时一次账户级 suspend 要白挨
    /// `6 × 17 = 102` 次上游 403 才能把池清空；期间客户端全部拿 429，且全池自愈
    /// 会把整族复活再来一轮（当天 `判定为死号并自动禁用` 231 次 / `执行自愈` 14 次）。
    /// 该窗口占 4h 内客户端 429 的 95.5%（2080/2177）。
    ///
    /// **旧代码在本测试下必红**：按号计数时，对 #1306 报 6 次只会禁用 #1306 一个，
    /// `available_count()` 仍是 `n-1`。
    #[test]
    fn test_suspicious_counting_is_family_scoped_for_clones() {
        const N: u64 = 5;
        let manager = MultiTokenManager::new(
            Config::default(),
            clone_family_pool(N),
            None,
            None,
            true,
        )
        .unwrap();
        assert_eq!(manager.available_count(), N as usize, "前置：N 份全部可用");

        // 阈值前一次：整族都还在池内（403 是临时态，不得一见就禁）
        for _ in 0..(MAX_CONSECUTIVE_SUSPICIOUS_BEFORE_DISABLE - 1) {
            manager.report_suspicious_activity(1306);
        }
        assert_eq!(
            manager.available_count(),
            N as usize,
            "未达阈值前整族都不得被禁用"
        );

        // 达阈值：**整族**一起退出调度，不需要再对另外 N-1 份各打 6 次。
        manager.report_suspicious_activity(1306);
        assert_eq!(
            manager.available_count(),
            0,
            "同 clone_group 的分身共享一个上游账号，一次账户级 suspend 应让整族\
             （{N} 份）一起退出调度；按号计数时这里会是 {}（旧代码即如此），\
             等于要白挨 6×{N} 次上游 403",
            N - 1
        );
    }

    /// 🔴 反向（OVER-REACH 控制）：**无** `clone_group` 的号必须仍逐个独立禁用。
    ///
    /// 若把收族写成"是 api_key 就并族"，则线上所有未多开的 `ksk_` 号会并成一族 →
    /// 一号被风控整池连坐，比不收族更糟。本测试与上一个测试构成对照：
    /// 同样报满阈值，这里只应掉 1 个。
    #[test]
    fn test_suspicious_counting_stays_per_credential_without_clone_group() {
        let mk = |id: u64, key: &str| {
            let mut c = KiroCredentials::default();
            c.id = Some(id);
            c.auth_method = Some("api_key".to_string());
            c.kiro_api_key = Some(key.to_string());
            // clone_group 刻意留 None —— 这是本测试的全部要点。
            c
        };
        let manager = MultiTokenManager::new(
            Config::default(),
            vec![mk(1, "ksk_solo_a"), mk(2, "ksk_solo_b"), mk(3, "ksk_solo_c")],
            None,
            None,
            true,
        )
        .unwrap();

        for _ in 0..MAX_CONSECUTIVE_SUSPICIOUS_BEFORE_DISABLE {
            manager.report_suspicious_activity(1);
        }
        assert_eq!(
            manager.available_count(),
            2,
            "无 clone_group 的号必须各自独立禁用；若整池连坐这里会是 0"
        );
    }

    /// 承重配对：族级计数必须配族级清零。
    ///
    /// 若累加按族、清零只清本号，则同族其它分身停在高位（如 5/6），下一次 403 会从
    /// 那个高位 +1 直接把整族推过阈值 —— 表现为「刚成功过的账号立刻被判死号」。
    /// 把 `report_success` 里的族级清零循环删掉即变红。
    #[test]
    fn test_family_success_clears_whole_family_suspicious_counter() {
        const N: u64 = 4;
        let manager = MultiTokenManager::new(
            Config::default(),
            clone_family_pool(N),
            None,
            None,
            true,
        )
        .unwrap();

        // 把整族的计数推到阈值前一格（对 #1306 报，族内共享 → 全员 5/6）
        for _ in 0..(MAX_CONSECUTIVE_SUSPICIOUS_BEFORE_DISABLE - 1) {
            manager.report_suspicious_activity(1306);
        }
        // 族内**另一份**成功 → 必须把整族计数清零（账号恢复了）
        manager.report_success(1307);

        // 于是再来 阈值-1 次仍不该禁用：若清零只清了 #1307，
        // 此刻 #1306 还停在 5，下面第一次上报就会把整族推到 6 → available_count()==0。
        for _ in 0..(MAX_CONSECUTIVE_SUSPICIOUS_BEFORE_DISABLE - 1) {
            manager.report_suspicious_activity(1306);
        }
        assert_eq!(
            manager.available_count(),
            N as usize,
            "族内任一份成功必须清零**整族**计数；否则刚成功过的账号会被立刻判死号"
        );
    }

    /// 回归（**回滚安全**，发布前 review 抓到的 BLOCKER）：
    /// 未知的 `disabledReason` 必须退化成 `Unknown`，**绝不能让整个凭据文件解析失败**。
    ///
    /// **为什么这是 BLOCKER**：`DisabledReason` 每次新增变体（本轮加了
    /// `PassthroughFailed` / `PassthroughOverloaded`），新版本就会把它写进
    /// `credentials.json`。而**旧版本**读到未知变体时 serde 报
    /// `unknown variant 'passthroughFailed', expected one of ...` →
    /// `CredentialsConfig::load` 返 Err → `main.rs` 直接 `std::process::exit(1)`
    /// （刻意的 fail-safe：宁可拒绝启动也不用空池覆盖真实凭据）。
    ///
    /// 于是 **回滚到旧二进制 = 服务起不来**，且必须在生产压力下手工编辑 JSON 才能恢复。
    /// 这条实测验证过：去掉 `#[serde(other)]` 后本测试 FAIL 并给出上述 serde 报错。
    ///
    /// ⚠️ 本测试同时守着「未知变体不得让**其它字段**丢失」——
    /// 退化成 Unknown 后 disabled 等字段必须照常读出来。
    #[test]
    fn test_unknown_disabled_reason_degrades_instead_of_failing_load() {
        // 模拟"未来版本"写出的凭据文件（含本版本不认识的 reason）
        let json = r#"[{
            "authMethod": "api_key",
            "kiroApiKey": "ksk_future",
            "disabled": true,
            "disabledReason": "someFutureReasonFromNewerVersion",
            "disabledAt": "2026-07-31T00:00:00+00:00"
        }]"#;
        let creds: Vec<KiroCredentials> = serde_json::from_str(json)
            .expect("未知 reason 绝不能让整个凭据文件解析失败（回滚会因此起不来）");
        assert_eq!(creds.len(), 1);
        assert_eq!(
            creds[0].disabled_reason,
            Some(DisabledReason::Unknown),
            "未知变体应退化成 Unknown"
        );
        // 关键：退化不得连带丢掉其它字段
        assert!(
            creds[0].disabled,
            "disabled 必须照常读出（退化不应影响其它字段）"
        );
        assert!(creds[0].disabled_at.is_some(), "disabled_at 必须照常读出");
        assert_eq!(creds[0].kiro_api_key.as_deref(), Some("ksk_future"));
    }

    /// 回归（回滚安全 · 对照组）：**已知**变体必须精确解析，不得被 `Unknown` 吞掉。
    ///
    /// 防止"修过头"——`#[serde(other)]` 若误配成捕获全部，所有原因都会变成 Unknown，
    /// 那等于把「标明封号原因」这个需求整体废掉。
    #[test]
    fn test_known_disabled_reasons_still_parse_exactly() {
        for (wire, expect) in [
            ("manual", DisabledReason::Manual),
            ("quotaExceeded", DisabledReason::QuotaExceeded),
            ("passthroughFailed", DisabledReason::PassthroughFailed),
            (
                "passthroughOverloaded",
                DisabledReason::PassthroughOverloaded,
            ),
        ] {
            let got: DisabledReason = serde_json::from_str(&format!("\"{wire}\""))
                .unwrap_or_else(|e| panic!("{wire} 应可解析: {e}"));
            assert_eq!(
                got, expect,
                "{wire} 被解析错了（serde(other) 不应吞掉已知变体）"
            );
        }
    }

    /// 回归（E3 · 复活必须清全部惩罚计数）：`reset_and_enable` 后再来一次风控不得秒禁。
    ///
    /// **旧代码为何失败**：`reset_and_enable` 只清 `failure_count` /
    /// `refresh_failure_count` / `request_count`，**漏了 `consecutive_suspicious`**
    /// （该字段唯一清零点是 `report_success`）。于是被 `SuspiciousActivityAuto` 禁用的号
    /// （计数已达阈值）人工「重置并启用」后计数仍在阈值上，**下一次风控即再次禁用**，
    /// 「重置」形同虚设。且自动禁用落盘后该秒禁重启也回不来。
    #[test]
    fn test_reset_and_enable_clears_suspicious_counter_so_revived_cred_survives_one_hit() {
        let config = Config::default();
        let manager = MultiTokenManager::new(
            config,
            vec![KiroCredentials::default(), KiroCredentials::default()],
            None,
            None,
            false,
        )
        .unwrap();

        // 打到阈值 → 自动禁用
        for _ in 0..MAX_CONSECUTIVE_SUSPICIOUS_BEFORE_DISABLE {
            manager.report_suspicious_activity(1);
        }
        assert_eq!(manager.available_count(), 1, "达阈值应已自动禁用");

        // 人工复活
        manager.reset_and_enable(1).unwrap();
        assert_eq!(manager.available_count(), 2, "重置并启用后应回池");

        // 复活后再来一次风控：绝不能立即再禁（计数应已归零，距阈值还差 N-1 次）
        manager.report_suspicious_activity(1);
        assert_eq!(
            manager.available_count(),
            2,
            "复活后一次风控就秒禁 = consecutive_suspicious 没被清零（旧代码即如此）"
        );
    }

    /// 回归（E3 · 同上，覆盖 `set_disabled(false)` 这条复活路径）。
    ///
    /// 三条复活路径此前各自手写清零列表且都漏同一个字段，故三条都要有守卫。
    #[test]
    fn test_set_disabled_false_clears_suspicious_counter() {
        let config = Config::default();
        let manager = MultiTokenManager::new(
            config,
            vec![KiroCredentials::default(), KiroCredentials::default()],
            None,
            None,
            false,
        )
        .unwrap();

        for _ in 0..MAX_CONSECUTIVE_SUSPICIOUS_BEFORE_DISABLE {
            manager.report_suspicious_activity(1);
        }
        assert_eq!(manager.available_count(), 1, "达阈值应已自动禁用");

        // 走 Admin 的启用开关复活
        manager.set_disabled(1, false).unwrap();
        assert_eq!(manager.available_count(), 2, "启用后应回池");

        manager.report_suspicious_activity(1);
        assert_eq!(
            manager.available_count(),
            2,
            "set_disabled(false) 同样必须清 consecutive_suspicious"
        );
    }

    /// 回归（🔴 会让整池永久死锁）：全池被 403 风控打成 `SuspiciousActivityAuto` 后必须能自愈。
    ///
    /// **旧代码为何 FAIL**：自愈的判定条件只匹配 `TooManyFailures`，
    /// 而 403 风控走的是 `SuspiciousActivityAuto`。于是一次 IP 级风控把全池打死后
    /// **没有任何自动恢复路径** —— 只能人工介入或重启。
    ///
    /// 而 403 `TEMPORARILY_SUSPENDED` 是**临时态**（代码注释与历史事故都明确记录：
    /// 曾被当永久封禁处理 → 12h 内 88 次误禁 + 36 次全池活锁 → 逐小时拒绝率升到 100%）。
    ///
    /// 线上实测（48h）：`判定为死号并自动禁用` **46 次**，而 `执行自愈` **0 次**
    /// —— 坐实自愈对这个原因从未生效过。
    #[tokio::test]
    async fn test_pool_wide_suspicious_auto_disable_can_self_heal() {
        let config = Config::default();
        let manager =
            MultiTokenManager::new(config, vec![KiroCredentials::default()], None, None, false)
                .unwrap();

        // 打到阈值 → 以 SuspiciousActivityAuto 自动禁用（全池只有这一个号）
        for _ in 0..MAX_CONSECUTIVE_SUSPICIOUS_BEFORE_DISABLE {
            manager.report_suspicious_activity(1);
        }
        assert_eq!(manager.available_count(), 0, "前提：全池应已被自动禁用");

        // 自愈：acquire_context 应把它复活并重试。
        //
        // ⚠️ 断言口径很关键：**不能**在调用后查 `available_count()`。
        // 测试凭据是假的（没有真 token），复活后必然刷新失败并被重新禁用为
        // `TooManyRefreshFailures` —— 那是**预期行为**，不是自愈没生效。
        // 所以判据取"最终禁用原因已不再是 SuspiciousActivityAuto"：
        // 说明它确实被复活过并走到了刷新阶段（旧代码下会原地卡在 SuspiciousActivityAuto）。
        let _ = manager.acquire_context(None, None).await;
        let snap = manager.snapshot();
        let entry = snap
            .entries
            .iter()
            .find(|e| e.id == 1)
            .expect("号应仍在池中");
        assert_ne!(
            entry.disabled_reason.as_deref(),
            Some("SuspiciousActivityAuto"),
            "全池被 403 风控打死后必须能自愈并重新参与调度（旧代码只认 TooManyFailures → \
             原地死锁在 SuspiciousActivityAuto，永无恢复路径）。实际原因: {:?}",
            entry.disabled_reason
        );
    }

    /// ⭐ P0 回归：`mark_region_probe_failed` 必须真的禁用凭据 + 写对原因 + **不可自愈**。
    ///
    /// # 三条断言各自防什么
    ///
    /// 1. **`disabled=true`** —— 这是 P0 修复的落点。不置位则号仍接流量、在错区恒 403，
    ///    3 次即被 `TooManyFailures` 打死（实测 #536–550 十五个号两分钟内全灭，各只跑
    ///    1~6 个请求、0 成功）。
    /// 2. **原因可归因** —— 停在 `Manual` 会让运维在面板上看到「手动禁用」，而没人手动禁过它；
    ///    而两个新原因的处置方向不同（`RegionProbeFailed` 查 region 授权范围、
    ///    `RegionProbeTokenDead` 查 token 来源）。
    /// 3. **不可自愈** —— `is_self_healable_reason` 是白名单，新变体天然被排除。这条是承重的：
    ///    实测自愈 24h 内跑了 **44 次**（退避已升到第 5 级），若能捞回，禁用等于没做 ——
    ///    号会被反复放回池子重演一遍。
    ///
    /// 把 `mark_region_probe_failed` 里的 `entry.disabled = true` 删掉，或把新变体加进
    /// `is_self_healable_reason` 的白名单 → 本测试必 FAILED。
    #[tokio::test]
    async fn region_probe_failure_disables_and_is_not_self_healable() {
        use crate::kiro::region_probe::ProbeOutcome;

        for (outcome, want_reason) in [
            (ProbeOutcome::NoUsableRegion, "RegionProbeFailed"),
            (ProbeOutcome::TokenDead, "RegionProbeTokenDead"),
        ] {
            let manager = MultiTokenManager::new(
                Config::default(),
                vec![KiroCredentials::default()],
                None,
                None,
                false,
            )
            .unwrap();
            assert_eq!(manager.available_count(), 1, "前提：号应先是可用的");

            manager.mark_region_probe_failed(1, &outcome);

            assert_eq!(
                manager.available_count(),
                0,
                "{outcome:?} 后号必须不可用 —— 否则它会在错区接流量并被 TooManyFailures 打死"
            );
            let snap = manager.snapshot();
            let entry = snap
                .entries
                .iter()
                .find(|e| e.id == 1)
                .expect("号应仍在池中");
            assert!(entry.disabled, "disabled 必须置位");
            assert_eq!(
                entry.disabled_reason.as_deref(),
                Some(want_reason),
                "原因必须可归因（停在 Manual 会让运维以为是人工禁的）"
            );
        }

        // 承重：两个新原因都**不在**自愈白名单里。直接测判据函数，
        // 因为自愈只在"全池被自动禁用"时触发，构造那个状态会引入别的失败原因干扰。
        assert!(
            !is_self_healable_reason(Some(DisabledReason::RegionProbeFailed)),
            "RegionProbeFailed 不得可自愈 —— 自愈 24h 跑 44 次，捞回等于禁用没做"
        );
        assert!(
            !is_self_healable_reason(Some(DisabledReason::RegionProbeTokenDead)),
            "RegionProbeTokenDead 不得可自愈（token 已废，换区无用）"
        );
        // 对照：既有的可自愈原因不得因新增变体而失效。
        assert!(is_self_healable_reason(Some(
            DisabledReason::TooManyFailures
        )));
        assert!(is_self_healable_reason(Some(
            DisabledReason::SuspiciousActivityAuto
        )));
    }

    /// ⭐ P0 回归：`Usable` / `Skipped` 传进 `mark_region_probe_failed` 必须是 **no-op**。
    ///
    /// 调用方写错时（把成功或跳过也传进来）不得禁用凭据。选择静默返回而非 panic：
    /// 一个调用方的逻辑错误不该打死正在服务的进程。
    ///
    /// 把那条 `_ => return` 改成 fallthrough（给任意 outcome 都禁用）→ 本测试必 FAILED。
    #[tokio::test]
    async fn mark_region_probe_failed_ignores_success_and_skip() {
        use crate::kiro::region_probe::ProbeOutcome;

        for outcome in [
            ProbeOutcome::Usable("eu-central-1".to_string()),
            ProbeOutcome::Skipped,
        ] {
            let manager = MultiTokenManager::new(
                Config::default(),
                vec![KiroCredentials::default()],
                None,
                None,
                false,
            )
            .unwrap();
            manager.mark_region_probe_failed(1, &outcome);
            assert_eq!(
                manager.available_count(),
                1,
                "{outcome:?} 不是失败判决，绝不能据此禁用凭据"
            );
        }
    }

    #[tokio::test]
    async fn test_account_suspended_is_not_auto_recovered() {
        // 封禁属不可自动恢复原因：即使全部凭据被封，acquire_context 也不应把它们复活
        let config = Config::default();
        let cred1 = KiroCredentials::default();

        let manager = MultiTokenManager::new(config, vec![cred1], None, None, false).unwrap();
        assert!(!manager.report_account_suspended(1));
        assert_eq!(manager.available_count(), 0);

        // 封禁的凭据不应被自动恢复机制复活
        let ctx = manager.acquire_context(None, None).await;
        assert!(
            ctx.is_err(),
            "被封禁的凭据不应自动恢复为可用（AccountSuspended 不可自动恢复）"
        );
    }

    #[test]
    fn test_account_suspended_clears_affinity() {
        // 验证 G-7 闭环：封禁凭据时清除其会话亲和性绑定
        let config = Config::default();
        let cred1 = KiroCredentials::default();
        let cred2 = KiroCredentials::default();

        let manager =
            MultiTokenManager::new(config, vec![cred1, cred2], None, None, false).unwrap();

        // 建立会话 -> 凭据1 的亲和绑定
        manager.affinity.set("session-abc", 1);
        assert_eq!(manager.affinity.get("session-abc"), Some(1));

        // 封禁凭据1后，亲和绑定应被清除，不再指向已封的号
        manager.report_account_suspended(1);
        assert_eq!(
            manager.affinity.get("session-abc"),
            None,
            "封禁凭据后应清除其会话亲和性绑定"
        );
    }

    #[tokio::test]
    async fn test_multi_token_manager_quota_disabled_is_not_auto_recovered() {
        let config = Config::default();
        let cred1 = KiroCredentials::default();
        let cred2 = KiroCredentials::default();

        let manager =
            MultiTokenManager::new(config, vec![cred1, cred2], None, None, false).unwrap();

        manager.report_quota_exhausted(1);
        manager.report_quota_exhausted(2);
        assert_eq!(manager.available_count(), 0);

        let err = manager
            .acquire_context(None, None)
            .await
            .err()
            .unwrap()
            .to_string();
        assert!(
            err.contains("所有凭据均已禁用"),
            "错误应提示所有凭据禁用，实际: {}",
            err
        );
        assert_eq!(manager.available_count(), 0);
    }

    // ============ 凭据级 Region 优先级测试 ============

    #[test]
    fn test_credential_region_priority_uses_credential_auth_region() {
        // 凭据配置了 auth_region 时，应使用凭据的 auth_region
        let mut config = Config::default();
        config.region = "us-west-2".to_string();

        let mut credentials = KiroCredentials::default();
        credentials.auth_region = Some("eu-west-1".to_string());

        let region = credentials.effective_auth_region(&config);
        assert_eq!(region, "eu-west-1");
    }

    #[test]
    fn test_credential_region_priority_fallback_to_credential_region() {
        // 凭据未配置 auth_region 但配置了 region 时，应回退到凭据.region
        let mut config = Config::default();
        config.region = "us-west-2".to_string();

        let mut credentials = KiroCredentials::default();
        credentials.region = Some("eu-central-1".to_string());

        let region = credentials.effective_auth_region(&config);
        assert_eq!(region, "eu-central-1");
    }

    #[test]
    fn test_credential_region_priority_fallback_to_config() {
        // 凭据未配置 auth_region 和 region 时，应回退到 config
        let mut config = Config::default();
        config.region = "us-west-2".to_string();

        let credentials = KiroCredentials::default();
        assert!(credentials.auth_region.is_none());
        assert!(credentials.region.is_none());

        let region = credentials.effective_auth_region(&config);
        assert_eq!(region, "us-west-2");
    }

    #[test]
    fn test_multiple_credentials_use_respective_regions() {
        // 多凭据场景下，不同凭据使用各自的 auth_region
        let mut config = Config::default();
        config.region = "ap-northeast-1".to_string();

        let mut cred1 = KiroCredentials::default();
        cred1.auth_region = Some("us-east-1".to_string());

        let mut cred2 = KiroCredentials::default();
        cred2.region = Some("eu-west-1".to_string());

        let cred3 = KiroCredentials::default(); // 无 region，使用 config

        assert_eq!(cred1.effective_auth_region(&config), "us-east-1");
        assert_eq!(cred2.effective_auth_region(&config), "eu-west-1");
        assert_eq!(cred3.effective_auth_region(&config), "ap-northeast-1");
    }

    #[test]
    fn test_idc_oidc_endpoint_uses_credential_auth_region() {
        // 验证 IdC OIDC endpoint URL 使用凭据 auth_region
        let mut config = Config::default();
        config.region = "us-west-2".to_string();

        let mut credentials = KiroCredentials::default();
        credentials.auth_region = Some("eu-central-1".to_string());

        let region = credentials.effective_auth_region(&config);
        let refresh_url = format!("https://oidc.{}.amazonaws.com/token", region);

        assert_eq!(refresh_url, "https://oidc.eu-central-1.amazonaws.com/token");
    }

    #[test]
    fn test_social_refresh_endpoint_uses_credential_auth_region() {
        // 验证 Social refresh endpoint URL 使用凭据 auth_region
        let mut config = Config::default();
        config.region = "us-west-2".to_string();

        let mut credentials = KiroCredentials::default();
        credentials.auth_region = Some("ap-southeast-1".to_string());

        let region = credentials.effective_auth_region(&config);
        let refresh_url = format!("https://prod.{}.auth.desktop.kiro.dev/refreshToken", region);

        assert_eq!(
            refresh_url,
            "https://prod.ap-southeast-1.auth.desktop.kiro.dev/refreshToken"
        );
    }

    #[test]
    fn test_api_call_uses_effective_api_region() {
        // 验证 API 调用使用 effective_api_region
        let mut config = Config::default();
        config.region = "us-west-2".to_string();

        let mut credentials = KiroCredentials::default();
        credentials.region = Some("eu-west-1".to_string());

        // 凭据.region 不参与 api_region 回退链
        let api_region = credentials.effective_api_region(&config);
        let api_host = format!("q.{}.amazonaws.com", api_region);

        assert_eq!(api_host, "q.us-west-2.amazonaws.com");
    }

    #[test]
    fn test_api_call_uses_credential_api_region() {
        // 凭据配置了 api_region 时，API 调用应使用凭据的 api_region
        let mut config = Config::default();
        config.region = "us-west-2".to_string();

        let mut credentials = KiroCredentials::default();
        credentials.api_region = Some("eu-central-1".to_string());

        let api_region = credentials.effective_api_region(&config);
        let api_host = format!("q.{}.amazonaws.com", api_region);

        assert_eq!(api_host, "q.eu-central-1.amazonaws.com");
    }

    #[test]
    fn test_credential_region_empty_string_treated_as_set() {
        // 安全(H3)行为变更:空字符串/非法 region 不再被"视为已设置",而是过白名单不命中
        // → 回退到 config。旧行为让空串拼出坏 host(runtime..kiro.dev),现修正为回退可信 config。
        let mut config = Config::default();
        config.region = "us-west-2".to_string();

        let mut credentials = KiroCredentials::default();
        credentials.auth_region = Some("".to_string());

        let region = credentials.effective_auth_region(&config);
        // 空字符串不命中白名单 → 回退 config.region
        assert_eq!(region, "us-west-2");
    }

    #[test]
    fn test_auth_and_api_region_independent() {
        // auth_region 和 api_region 互不影响（用真实 AWS region,过白名单）
        let mut config = Config::default();
        config.region = "us-east-1".to_string();

        let mut credentials = KiroCredentials::default();
        credentials.auth_region = Some("eu-west-1".to_string());
        credentials.api_region = Some("ap-northeast-1".to_string());

        assert_eq!(credentials.effective_auth_region(&config), "eu-west-1");
        assert_eq!(credentials.effective_api_region(&config), "ap-northeast-1");
    }

    // ============ 凭据回收站测试 ============

    /// 软删除后：凭据不在 entries、在 trash
    #[test]
    fn test_delete_moves_credential_to_trash() {
        let config = Config::default();
        let mut c1 = KiroCredentials::default();
        c1.refresh_token = Some("refresh-1".to_string());
        let mut c2 = KiroCredentials::default();
        c2.refresh_token = Some("refresh-2".to_string());

        let manager = MultiTokenManager::new(config, vec![c1, c2], None, None, false).unwrap();

        // 必须先禁用才能删除
        manager.set_disabled(1, true).unwrap();
        manager.delete_credential(1).unwrap();

        // 不在 entries
        let snapshot = manager.snapshot();
        assert_eq!(snapshot.total, 1);
        assert!(snapshot.entries.iter().all(|e| e.id != 1));

        // 在 trash
        let trash = manager.list_trash();
        assert_eq!(trash.len(), 1);
        assert_eq!(trash[0].id, 1);
        assert!(!trash[0].deleted_at.is_empty());
    }

    /// 回归：删号进回收站必须保留禁用状态三元组，恢复时不得把真实原因抹成 `Manual`。
    ///
    /// **旧代码为何 FAIL**：`disabled` / `disabled_reason` / `disabled_at` 的权威副本在
    /// `CredentialEntry` 上，`KiroCredentials` 里那份只在 `persist_credentials()` 落盘时同步。
    /// 而 `delete_credential` 直接把 `removed.credentials` 塞进 `TrashEntry`，**绕过那次同步**
    /// → 回收站条目恒为 `(false, None, None)`，第一个断言即 FAIL。
    ///
    /// 实测：线上 07-30 之后删的 31 个号（reason 持久化已上线）在 trash.json 里三字段全空，
    /// 175 条回收站记录无一条带原因。用户明确要求过「认定封号必须标明原因」，而这恰在最需要
    /// 该信息的时刻（判断换号还是申诉）丢失；且 `restore_credential` 因读不到原因只能一律落
    /// `Manual`，是批 2 修掉的「自动原因变手动」在回收站路径上的同型漏修。
    #[test]
    fn test_trash_preserves_disable_reason_and_restore_does_not_downgrade_to_manual() {
        let config = Config::default();
        let mut c1 = KiroCredentials::default();
        c1.refresh_token = Some("refresh-trash-reason".to_string());
        let manager = MultiTokenManager::new(config, vec![c1], None, None, false).unwrap();

        // 自动禁用（额度耗尽），而非手动禁用 —— 这是原因会不会被抹掉的关键区别。
        manager.report_quota_exhausted(1);
        manager.delete_credential(1).unwrap();

        // ① 回收站快照必须带上真实原因与时刻
        let trash = manager.list_trash();
        assert_eq!(trash.len(), 1);
        assert_eq!(
            trash[0].disabled_reason,
            Some(DisabledReason::QuotaExceeded),
            "回收站丢了禁用原因（旧代码即如此：绕过 persist_credentials 的同步块）"
        );
        assert!(
            trash[0].disabled_at.is_some(),
            "回收站丢了禁用时刻，无法区分「刚坏就删」与「坏了很久」"
        );

        // ② 恢复后原因不得被降级成 Manual（仍保持 disabled，交由 Admin 手动启用）
        manager.restore_credential(1, false).unwrap();
        let snap = manager.snapshot();
        let e = snap.entries.iter().find(|e| e.id == 1).expect("应已回池");
        assert!(e.disabled, "恢复后应仍为禁用态，不自动回池");
        // 注意：面板快照里 disabled_reason 是字符串（前端 i18n 用），非枚举。
        assert_eq!(
            e.disabled_reason.as_deref(),
            Some("QuotaExceeded"),
            "恢复把自动禁用原因抹成了 Manual —— 运维据此无法判断该不该启用"
        );
    }

    /// 回归（用户要求的「强制删除」）：`force=true` 可直接删掉**未禁用**的号，且仍进回收站。
    ///
    /// **旧代码为何 FAIL**：`delete_credential` 无条件 bail
    /// `只能删除已禁用的凭据（请先禁用凭据 #N）`，删一个号必须两次调用（禁用 + 删除），
    /// 批量删 N 个 = **2N 次往返**；而"号卡住了要拔掉"正是强制删除的核心动机。
    ///
    /// 同时锁住**不能修过头**：force 只绕"必须先禁用"这道门，**不跳过回收站** ——
    /// adminKey 在 sessionStorage（登录时清 localStorage 残留），Admin UI 有 CSP
    /// （含 frame-ancestors 'none'）。XSS 面比「明文 localStorage 且无 CSP」小，
    /// 回收站仍是被打穿后的兜底。物理删除仍走 `purge_credential`。
    #[test]
    fn test_force_delete_bypasses_disable_gate_but_still_goes_to_trash() {
        let config = Config::default();
        let mut c1 = KiroCredentials::default();
        c1.refresh_token = Some("refresh-force".to_string());
        let manager = MultiTokenManager::new(config, vec![c1], None, None, false).unwrap();

        // 前提：号是**启用**状态 —— 非 force 路径会被拒绝。
        assert_eq!(manager.available_count(), 1);
        let err = manager.delete_credential(1).unwrap_err().to_string();
        assert!(
            err.contains("只能删除已禁用的凭据"),
            "非 force 必须保持原有保守语义（旧客户端不发 force 时行为不变）: {err}"
        );

        // force：直接删掉，无需先禁用。
        manager
            .delete_credential_forced(1, true)
            .expect("force 应能删除未禁用的号");
        assert_eq!(manager.total_count(), 0, "号应已从调度池移出");

        // 仍在回收站里 → 可恢复（force 不等于 purge）。
        let trash = manager.list_trash();
        assert_eq!(
            trash.len(),
            1,
            "force 删除仍须进回收站（XSS 兜底不能被绕过）"
        );
        assert_eq!(trash[0].id, 1);
        manager
            .restore_credential(1, false)
            .expect("回收站里的号应可恢复");
        assert_eq!(manager.total_count(), 1, "恢复后应回到凭据池");
    }

    /// 删除未禁用凭据应被拒绝，且不进入回收站
    #[test]
    fn test_delete_requires_disabled() {
        let config = Config::default();
        let mut c1 = KiroCredentials::default();
        c1.refresh_token = Some("refresh-1".to_string());

        let manager = MultiTokenManager::new(config, vec![c1], None, None, false).unwrap();

        let err = manager.delete_credential(1).unwrap_err().to_string();
        assert!(err.contains("只能删除已禁用的凭据"), "实际: {}", err);
        assert_eq!(manager.list_trash().len(), 0);
        assert_eq!(manager.total_count(), 1);
    }

    /// ⭐ 回归（用户直接反馈）：**分身能从回收站恢复**，且真重复仍被拦。
    ///
    /// 缺陷：恢复的去重判据原先**只看 key**，而多开造出的分身与主凭据**必然同 key**。
    /// 于是删掉的分身永远恢复不了（池里还有主凭据），主凭据也恢复不了（池里还有分身）。
    /// 面板反复弹「凭据已存在（kiroApiKey 重复），无法恢复」。
    ///
    /// 修法：去重键改为 **key + machineId**。同 key 不同机器码是多开刻意区分的两个
    /// 独立凭据（machineId 进 CLI 端点 User-Agent，是上游看到的设备身份）。
    ///
    /// 本测试两条断言分别锁「分身可恢复」与「真重复仍拦」，缺任一半都说明改错了。
    #[test]
    fn clone_can_be_restored_while_true_duplicate_still_rejected() {
        const KEY: &str = "ksk_restore_dedup_probe";
        // 主凭据 + 一个分身：同 key、不同 machineId（多开的真实形态）
        let mut parent = KiroCredentials::default();
        parent.auth_method = Some("api_key".to_string());
        parent.kiro_api_key = Some(KEY.to_string());
        parent.machine_id = Some("a".repeat(64));
        let mut clone = KiroCredentials::default();
        clone.auth_method = Some("api_key".to_string());
        clone.kiro_api_key = Some(KEY.to_string());
        clone.machine_id = Some("b".repeat(64));

        let mgr = MultiTokenManager::new(Config::default(), vec![parent, clone], None, None, false)
            .unwrap();

        // 删掉分身（id=2）→ 进回收站
        mgr.set_disabled(2, true).unwrap();
        mgr.delete_credential(2).unwrap();
        assert_eq!(mgr.list_trash().len(), 1);

        // ① 默认路径（force=false）仍拒 —— 误操作护栏保留，且文案要能指导操作。
        let err = mgr
            .restore_credential(2, false)
            .expect_err("默认路径应仍按 key 去重（护栏不能因多开而拆掉）");
        assert!(
            err.to_string().contains("强制恢复"),
            "拒绝文案必须提示可用强制恢复，否则用户以为凭据坏了。实际: {err}"
        );
        assert_eq!(mgr.list_trash().len(), 1, "被拒后应仍留在回收站");

        // ② ⭐ 承重断言：force=true 必须能恢复分身。
        //    没有这个出口，删掉的分身**永远**恢复不了（池里还有主凭据、同 key）。
        mgr.restore_credential(2, true)
            .expect("强制恢复必须放行：分身与主凭据同 key 是多开的正常形态");
        assert_eq!(mgr.snapshot().total, 2, "分身应已回到 entries");

        // ③ 恢复后必须是**禁用态** —— 这是强制恢复安全的前提
        //    （不会跳过运维确认就直接投入调度）。
        let snap = mgr.snapshot();
        let restored = snap
            .entries
            .iter()
            .find(|e| e.id == 2)
            .expect("id=2 应存在");
        assert!(restored.disabled, "强制恢复后必须仍是禁用态");
    }

    /// 恢复后：回 entries 且 id 不变。
    ///
    /// ⚠️ 这条曾**从未运行过**：它的 `#[test]` 与文档注释被写在上一条测试
    /// （`clone_can_be_restored_while_true_duplicate_still_rejected`）的文档块**之前**，
    /// 于是属性挂到了上一条身上（那条因此有两个 `#[test]`），本条退化成 `mod tests`
    /// 里一个普通私有函数 —— 无调用者、不被 harness 收集，`cargo test` 里既不出现
    /// 也不报错。同型缺陷靠 `warning: duplicated attribute` 可以找到。
    #[test]
    fn test_restore_returns_to_entries_id_unchanged() {
        let config = Config::default();
        let mut c1 = KiroCredentials::default();
        c1.refresh_token = Some("refresh-1".to_string());
        let mut c2 = KiroCredentials::default();
        c2.refresh_token = Some("refresh-2".to_string());

        let manager = MultiTokenManager::new(config, vec![c1, c2], None, None, false).unwrap();

        manager.set_disabled(2, true).unwrap();
        manager.delete_credential(2).unwrap();
        assert_eq!(manager.list_trash().len(), 1);

        // 恢复
        manager.restore_credential(2, false).unwrap();

        // 回到 entries，id 保持 2
        let snapshot = manager.snapshot();
        assert_eq!(snapshot.total, 2);
        let restored = snapshot.entries.iter().find(|e| e.id == 2);
        assert!(restored.is_some(), "id=2 应回到 entries");
        // 恢复为禁用态
        assert!(restored.unwrap().disabled);
        // 回收站已清空该条目
        assert_eq!(manager.list_trash().len(), 0);
    }

    /// 恢复重复 refreshToken 被拒
    #[test]
    fn test_restore_duplicate_refresh_token_rejected() {
        let config = Config::default();
        // 两个凭据故意使用相同 refreshToken
        let mut c1 = KiroCredentials::default();
        c1.refresh_token = Some("same-refresh".to_string());
        let mut c2 = KiroCredentials::default();
        c2.refresh_token = Some("same-refresh".to_string());

        let manager = MultiTokenManager::new(config, vec![c1, c2], None, None, false).unwrap();

        // 删除 id=1（进入回收站）；id=2 仍在 entries，持有相同 refreshToken
        manager.set_disabled(1, true).unwrap();
        manager.delete_credential(1).unwrap();

        // 恢复 id=1 应因 refreshToken 与 id=2 重复而被拒
        let err = manager
            .restore_credential(1, false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("refreshToken 重复"), "实际: {}", err);
        // 仍留在回收站，未误入 entries
        assert_eq!(manager.list_trash().len(), 1);
        assert_eq!(manager.total_count(), 1);
    }

    /// new_id 分配跳过 trash 里的 id，防撞号
    #[tokio::test]
    async fn test_new_id_skips_trash_id() {
        let config = Config::default();
        // 用 API Key 凭据，add_credential 无需网络刷新
        let mut c1 = KiroCredentials::default();
        c1.auth_method = Some("api_key".to_string());
        c1.kiro_api_key = Some("ksk_first_credential_key".to_string());

        let manager = MultiTokenManager::new(config, vec![c1], None, None, false).unwrap();

        // 删除 id=1 → 进回收站，entries 空
        manager.set_disabled(1, true).unwrap();
        manager.delete_credential(1).unwrap();
        assert_eq!(manager.total_count(), 0);
        assert_eq!(manager.list_trash().len(), 1);

        // 新增凭据：即便 entries 为空，new_id 也须跳过回收站里的 id=1
        let mut new_cred = KiroCredentials::default();
        new_cred.auth_method = Some("api_key".to_string());
        new_cred.kiro_api_key = Some("ksk_second_credential_key".to_string());
        let new_id = manager.add_credential(new_cred).await.unwrap();

        assert_eq!(new_id, 2, "new_id 必须跳过回收站里的 id=1");
    }

    /// purge：从回收站彻底删除后不可恢复
    #[test]
    fn test_purge_removes_from_trash() {
        let config = Config::default();
        let mut c1 = KiroCredentials::default();
        c1.refresh_token = Some("refresh-1".to_string());
        let mut c2 = KiroCredentials::default();
        c2.refresh_token = Some("refresh-2".to_string());

        let manager = MultiTokenManager::new(config, vec![c1, c2], None, None, false).unwrap();

        manager.set_disabled(1, true).unwrap();
        manager.delete_credential(1).unwrap();
        assert_eq!(manager.list_trash().len(), 1);

        manager.purge_credential(1).unwrap();
        assert_eq!(manager.list_trash().len(), 0);

        // 已彻底删除，恢复应报不存在
        let err = manager
            .restore_credential(1, false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("回收站中不存在"), "实际: {}", err);
    }

    /// purge_expired_trash：按保留期清理，0 表示永久保留
    #[test]
    fn test_purge_expired_trash_retention() {
        let config = Config::default();
        let mut c1 = KiroCredentials::default();
        c1.refresh_token = Some("refresh-1".to_string());

        let manager = MultiTokenManager::new(config, vec![c1], None, None, false).unwrap();

        manager.set_disabled(1, true).unwrap();
        manager.delete_credential(1).unwrap();

        // 把删除时间改成 40 天前
        {
            let mut trash = manager.trash.lock();
            trash[0].deleted_at = (Utc::now() - Duration::days(40)).to_rfc3339();
        }

        // retention=0：永久保留，不清理
        assert_eq!(manager.purge_expired_trash(0), 0);
        assert_eq!(manager.list_trash().len(), 1);

        // retention=30：40 天前的条目应被清理
        assert_eq!(manager.purge_expired_trash(30), 1);
        assert_eq!(manager.list_trash().len(), 0);
    }

    /// 上游 5xx 必须给该号设冷却，否则失败的号下一轮立刻又被选中。
    ///
    /// 回归背景：`CooldownReason::ServerError`（30s，自动恢复）早已定义，但在**生产路径上
    /// 从未被设置过** —— 唯一调用方是 admin 的手工冷却接口。于是 500 风暴时（实测一小时
    /// 408 次 500）请求只 sleep 200ms~2s 就换号，在同一批坏号之间来回打。
    #[test]
    fn should_set_cooldown_on_upstream_server_error() {
        let config = Config::default();
        let mut c1 = KiroCredentials::default();
        c1.refresh_token = Some("refresh-1".to_string());
        let manager = MultiTokenManager::new(config, vec![c1], None, None, false).unwrap();

        assert!(
            !manager
                .cooldown_snapshot()
                .iter()
                .any(|i| i.credential_id == 1),
            "初始不应有冷却"
        );

        manager.report_server_error(1);

        let info = manager
            .cooldown_snapshot()
            .into_iter()
            .find(|i| i.credential_id == 1)
            .expect("5xx 后该号必须处于冷却中（此前完全不设冷却）");
        assert_eq!(
            info.reason,
            crate::kiro::cooldown::CooldownReason::ServerError,
            "应使用早已定义却从未在生产路径被用过的 ServerError 原因"
        );
        assert!(
            info.reason.is_auto_recoverable(),
            "5xx 是上游整体故障，必须能自动恢复，不可要求人工介入"
        );
    }

    /// `cooldownEnabled=false` 时 5xx 不设冷却（尊重全局开关，不绕过门禁）。
    #[test]
    fn should_skip_server_error_cooldown_when_cooldown_disabled() {
        let mut config = Config::default();
        config.cooldown_enabled = false;
        let mut c1 = KiroCredentials::default();
        c1.refresh_token = Some("refresh-1".to_string());
        let manager = MultiTokenManager::new(config, vec![c1], None, None, false).unwrap();

        manager.report_server_error(1);
        assert!(
            !manager
                .cooldown_snapshot()
                .iter()
                .any(|i| i.credential_id == 1),
            "冷却总开关关闭时不应设冷却"
        );
    }

    /// 重试预算必须只数 Kiro 路径**真正可选**的号：排除 disabled 与 custom_api。
    ///
    /// 回归背景：预算此前按 `total_count()`（= entries.len()）× 3 计算，把永不可选的
    /// custom_api 与已禁用号也算进去，凭空抬高预算 —— 生产日志 `尝试 8/36` 即由此而来。
    #[test]
    fn should_count_only_kiro_selectable_credentials_for_retry_budget() {
        let config = Config::default();
        let mut kiro1 = KiroCredentials::default();
        kiro1.refresh_token = Some("refresh-1".to_string());
        let mut kiro2 = KiroCredentials::default();
        kiro2.refresh_token = Some("refresh-2".to_string());
        // custom_api 代挂号：is_entry_selectable 永远拒绝它走 Kiro 路径
        let mut custom = KiroCredentials::default();
        custom.auth_method = Some("custom_api".to_string());
        custom.base_url = Some("https://relay.invalid".to_string());
        custom.api_key = Some("sk-x".to_string());

        let manager =
            MultiTokenManager::new(config, vec![kiro1, kiro2, custom], None, None, false).unwrap();

        assert_eq!(manager.total_count(), 3, "entries 总数含 custom_api");
        assert_eq!(
            manager.kiro_selectable_count(),
            2,
            "Kiro 可选数必须排除 custom_api"
        );

        // 禁用其中一个 Kiro 号 → 可选数再降
        manager.set_disabled(1, true).unwrap();
        assert_eq!(
            manager.kiro_selectable_count(),
            1,
            "disabled 号不可选，必须从预算基数里剔除"
        );
        assert_eq!(manager.total_count(), 3, "total_count 仍含全部条目（对照）");
    }

    /// 面板「全部清空」必须能清掉**刚删除**的条目。
    ///
    /// 这是历史缺陷的回归守卫：按天数的接口无法表达「立即全清」——传 0 被解释成
    /// 永久保留（返回 0），传 N 又清不掉 N 天内新删的条目。于是面板点清理时 67 条
    /// 刚删的凭据一条都清不掉，只提示「共移除 0 项」。
    #[test]
    fn should_purge_all_trash_including_freshly_deleted_entries() {
        let config = Config::default();
        let mut c1 = KiroCredentials::default();
        c1.refresh_token = Some("refresh-1".to_string());
        let mut c2 = KiroCredentials::default();
        c2.refresh_token = Some("refresh-2".to_string());

        let manager = MultiTokenManager::new(config, vec![c1, c2], None, None, false).unwrap();
        for id in [1, 2] {
            manager.set_disabled(id, true).unwrap();
            manager.delete_credential(id).unwrap();
        }
        assert_eq!(manager.list_trash().len(), 2, "两条都应在回收站");

        // 按天数的两条路径都清不掉刚删的条目（这正是缺陷本身）
        assert_eq!(
            manager.purge_expired_trash(0),
            0,
            "0 被解释为永久保留，清不掉"
        );
        assert_eq!(
            manager.purge_expired_trash(30),
            0,
            "刚删除的条目未超过 30 天，清不掉"
        );
        assert_eq!(manager.list_trash().len(), 2, "此时回收站仍有 2 条");

        // 全清入口必须真的清空
        assert_eq!(manager.purge_all_trash(), 2, "全清应返回被清条目数");
        assert_eq!(manager.list_trash().len(), 0, "回收站应为空");
    }

    /// 空回收站上全清应返回 0 且不报错（幂等，前端可重复点）。
    #[test]
    fn should_return_zero_when_purging_empty_trash() {
        let config = Config::default();
        let mut c1 = KiroCredentials::default();
        c1.refresh_token = Some("refresh-1".to_string());
        let manager = MultiTokenManager::new(config, vec![c1], None, None, false).unwrap();
        assert_eq!(manager.purge_all_trash(), 0);
        assert_eq!(manager.purge_all_trash(), 0, "重复调用仍为 0，不 panic");
    }

    /// trash.json 持久化往返：多凭据格式下删除落盘，重建后回收站仍在
    #[test]
    fn test_trash_persists_and_reloads() {
        let dir = std::env::temp_dir().join(format!("kiro-trash-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let cred_path = dir.join("credentials.json");
        std::fs::write(
            &cred_path,
            r#"[{"id":1,"refreshToken":"refresh-1"},{"id":2,"refreshToken":"refresh-2"}]"#,
        )
        .unwrap();

        let creds = vec![
            {
                let mut c = KiroCredentials::default();
                c.id = Some(1);
                c.refresh_token = Some("refresh-1".to_string());
                c
            },
            {
                let mut c = KiroCredentials::default();
                c.id = Some(2);
                c.refresh_token = Some("refresh-2".to_string());
                c
            },
        ];

        let manager = MultiTokenManager::new(
            Config::default(),
            creds,
            None,
            Some(cred_path.clone()),
            true,
        )
        .unwrap();

        manager.set_disabled(1, true).unwrap();
        manager.delete_credential(1).unwrap();

        // trash.json 应已写入
        let trash_file = dir.join("trash.json");
        assert!(trash_file.exists(), "trash.json 应已落盘");

        // 用同一凭据文件重建 manager（此时 credentials.json 已移除 id=1）
        let reload_creds = crate::kiro::model::credentials::CredentialsConfig::load(&cred_path)
            .unwrap()
            .into_sorted_credentials();
        let manager2 = MultiTokenManager::new(
            Config::default(),
            reload_creds,
            None,
            Some(cred_path.clone()),
            true,
        )
        .unwrap();

        // 回收站从磁盘恢复
        let trash = manager2.list_trash();
        assert_eq!(trash.len(), 1);
        assert_eq!(trash[0].id, 1);
        // entries 只剩 id=2
        assert_eq!(manager2.total_count(), 1);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_custom_api_request_count_persists_without_auto_disable() {
        // request_count 仍是跨重启的观测计数，但 request_limit 不再是自动关闭开关。
        use crate::usage::RequestOutcome;

        let dir = std::env::temp_dir().join(format!("kiro-reqcount-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let cred_path = dir.join("credentials.json");
        std::fs::write(
            &cred_path,
            r#"[{"id":1,"authMethod":"custom_api","baseUrl":"https://up.example.invalid","requestLimit":2}]"#,
        )
        .unwrap();

        let cred = {
            let mut c = KiroCredentials::default();
            c.id = Some(1);
            c.auth_method = Some("custom_api".to_string());
            c.base_url = Some("https://up.example.invalid".to_string());
            c.request_limit = Some(2);
            c
        };

        let manager = MultiTokenManager::new(
            Config::default(),
            vec![cred],
            None,
            Some(cred_path.clone()),
            true,
        )
        .unwrap();

        // 超过 request_limit=2 仍保持启用。
        manager.record_passthrough_result(1, RequestOutcome::Success);
        manager.record_passthrough_result(1, RequestOutcome::Success);
        manager.record_passthrough_result(1, RequestOutcome::Success);
        manager.save_stats();

        // 显式落盘后 stats 文件应写入 request_count。
        let stats_file = dir.join("kiro_stats.json");
        assert!(stats_file.exists(), "kiro_stats.json 应已落盘");
        let stats_json = std::fs::read_to_string(&stats_file).unwrap();
        assert!(
            stats_json.contains("\"request_count\""),
            "stats 文件应含 request_count 字段"
        );

        // 用同一目录重建 manager，模拟进程重启：reload_creds 从 credentials.json 读回，
        // load_stats 从 kiro_stats.json 读回 request_count。
        let reload_creds = crate::kiro::model::credentials::CredentialsConfig::load(&cred_path)
            .unwrap()
            .into_sorted_credentials();
        let manager2 = MultiTokenManager::new(
            Config::default(),
            reload_creds,
            None,
            Some(cred_path.clone()),
            true,
        )
        .unwrap();

        let snap = manager2
            .snapshot()
            .entries
            .into_iter()
            .find(|c| c.id == 1)
            .expect("id=1 应仍在池中");
        assert_eq!(
            snap.request_count, 3,
            "request_count 应跨重启保留为 3，不回退归零"
        );
        assert!(!snap.disabled, "超过 request_limit 也不得自动关闭代挂站");
        assert_eq!(snap.disabled_reason, None);

        std::fs::remove_dir_all(&dir).ok();
    }

    /// N5：`reset_count` 清零必须与 `success_count` 成对——旧实现只清 request_count，
    /// 面板出现「成功数 > 请求数」的自相矛盾（线上实测 #1 669/495、#2 477/396，
    /// 两号都在 08-14 初被清过一次；success_count 与用量库逐日对账完全一致，是
    /// request_count 被不对称清零）。本函数只放行 custom_api 号，清零 success_count
    /// 不影响 `has_ever_succeeded()`（其调用点全在 Kiro 主路径，custom_api 被排除）。
    #[test]
    fn test_custom_api_reset_count_clears_success_and_request_together() {
        use crate::usage::RequestOutcome;

        let dir = std::env::temp_dir().join(format!("kiro-resetcount-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let cred_path = dir.join("credentials.json");
        std::fs::write(&cred_path, r#"[]"#).unwrap();

        let cred = {
            let mut c = KiroCredentials::default();
            c.id = Some(1);
            c.auth_method = Some("custom_api".to_string());
            c.base_url = Some("https://up.example.invalid".to_string());
            c
        };

        let manager = MultiTokenManager::new(
            Config::default(),
            vec![cred],
            None,
            Some(cred_path.clone()),
            true,
        )
        .unwrap();

        // 先积累两个成功：success_count 与 request_count 同步为 2。
        manager.record_passthrough_result(1, RequestOutcome::Success);
        manager.record_passthrough_result(1, RequestOutcome::Success);
        let before = manager
            .snapshot()
            .entries
            .into_iter()
            .find(|c| c.id == 1)
            .expect("id=1 应仍在池中");
        assert_eq!(before.success_count, 2);
        assert_eq!(before.request_count, 2);

        // 换 key 清零：base_url 传 None（不改）→ 跳过 SSRF 校验；reset_count=true。
        tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(manager.set_custom_api_config(1, None, None, None, true))
            .unwrap();

        let after = manager
            .snapshot()
            .entries
            .into_iter()
            .find(|c| c.id == 1)
            .expect("id=1 应仍在池中");
        assert_eq!(
            after.success_count, 0,
            "success_count 必须随 request_count 一起清零（N5 对称性，否则面板成功数>请求数）"
        );
        assert_eq!(after.request_count, 0);

        std::fs::remove_dir_all(&dir).ok();
    }

    // ============ 服务端防呆:setter 越界自动修补(不信任前端校验)============

    /// 优先级超上界 → clamp 到 MAX_PRIORITY(直打 admin API 的极值不污染排序)。
    #[test]
    fn test_set_priority_clamps_to_max() {
        let config = Config::default();
        let mut c = KiroCredentials::default();
        c.refresh_token = Some("r-clamp-prio".to_string());
        let mgr = MultiTokenManager::new(config, vec![c], None, None, false).unwrap();

        mgr.set_priority(1, u32::MAX).unwrap();
        let snap = mgr
            .snapshot()
            .entries
            .into_iter()
            .find(|e| e.id == 1)
            .unwrap();
        assert_eq!(snap.priority, MAX_PRIORITY, "越界优先级应 clamp 到上界");

        // 界内值不动。
        mgr.set_priority(1, 5).unwrap();
        let snap2 = mgr
            .snapshot()
            .entries
            .into_iter()
            .find(|e| e.id == 1)
            .unwrap();
        assert_eq!(snap2.priority, 5);
    }

    /// RPM 上限:0→None(继承全局),极值→clamp 到 MAX_RPM_LIMIT。
    #[test]
    fn test_set_rpm_limit_normalizes_and_clamps() {
        let config = Config::default();
        let mut c = KiroCredentials::default();
        c.refresh_token = Some("r-clamp-rpm".to_string());
        let mgr = MultiTokenManager::new(config, vec![c], None, None, false).unwrap();

        // 0 → None
        mgr.set_rpm_limit(1, Some(0)).unwrap();
        let snap = mgr
            .snapshot()
            .entries
            .into_iter()
            .find(|e| e.id == 1)
            .unwrap();
        assert_eq!(snap.rpm_limit, None, "0 应归一为 None(继承全局)");

        // 极值 → clamp 到上界
        mgr.set_rpm_limit(1, Some(u32::MAX)).unwrap();
        let snap2 = mgr
            .snapshot()
            .entries
            .into_iter()
            .find(|e| e.id == 1)
            .unwrap();
        assert_eq!(
            snap2.rpm_limit,
            Some(MAX_RPM_LIMIT),
            "越界 RPM 应 clamp 到上界"
        );

        // 界内值不动
        mgr.set_rpm_limit(1, Some(60)).unwrap();
        let snap3 = mgr
            .snapshot()
            .entries
            .into_iter()
            .find(|e| e.id == 1)
            .unwrap();
        assert_eq!(snap3.rpm_limit, Some(60));
    }

    /// 别名超长 → 按字符截断到 MAX_NAME_CHARS(多字节安全,不切坏 UTF-8);空白清除。
    #[test]
    fn test_set_credential_name_truncates_and_trims() {
        let config = Config::default();
        let mut c = KiroCredentials::default();
        c.refresh_token = Some("r-clamp-name".to_string());
        let mgr = MultiTokenManager::new(config, vec![c], None, None, false).unwrap();

        // 超长中文(每字符多字节)→ 截断到 MAX_NAME_CHARS 个 char,不 panic 不切坏。
        let long = "中".repeat(100);
        mgr.set_credential_name(1, Some(long)).unwrap();
        let snap = mgr
            .snapshot()
            .entries
            .into_iter()
            .find(|e| e.id == 1)
            .unwrap();
        let name = snap.name.expect("应有别名");
        assert_eq!(
            name.chars().count(),
            MAX_NAME_CHARS,
            "超长别名应截断到上界字符数"
        );

        // 纯空白 → 清除。
        mgr.set_credential_name(1, Some("   ".to_string())).unwrap();
        let snap2 = mgr
            .snapshot()
            .entries
            .into_iter()
            .find(|e| e.id == 1)
            .unwrap();
        assert_eq!(snap2.name, None, "纯空白别名应清除");
    }

    // ============ 熔断/健康快照暴露(Phase 2:此前后端算好但无出口)============

    /// 从未被选过的号无健康记录 → 不在 health_snapshots 表中(调用方按缺省满血处理)。
    /// 连续 429 跳闸后 → 表中出现该号且 circuit_open=true、健康分被拉低。
    #[test]
    fn test_health_snapshots_reflects_circuit_state() {
        let mut config = Config::default();
        config.cooldown_enabled = true;
        let mut c = KiroCredentials::default();
        c.refresh_token = Some("r-health-snap".to_string());
        let mgr = MultiTokenManager::new(config, vec![c], None, None, false).unwrap();

        // 初始:无健康记录(从未 429/成功过)→ 不在表中。
        assert!(mgr.health_snapshots().get(&1).is_none(), "初始无健康记录");

        // 连续裸 429 跳闸(TRIP_THRESHOLD 次以上),触发熔断 Open。
        for _ in 0..5 {
            mgr.report_rate_limited_with_retry_after(1, None);
        }
        let snaps = mgr.health_snapshots();
        let h = snaps.get(&1).expect("跳闸后应有健康记录");
        assert!(h.circuit_open, "连续 429 应跳闸 circuit Open");
        assert!(h.health < 1.0, "健康分应被 429 拉低");
        assert!(h.consecutive_429 >= 5, "连续 429 计数应累加");
    }

    // ============ at-rest 加密:落盘密文 + 重载解密全链路 ============

    /// 开启加密后:credentials.json 落盘是密文(带 magic、不含明文 token);重载能透明解密还原。
    #[test]
    fn test_at_rest_encryption_roundtrip_on_disk() {
        let dir = std::env::temp_dir().join(format!("kiro-enc-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let cred_path = dir.join("credentials.json");

        let mut config = Config::default();
        config.encrypt_credentials_at_rest = true;

        let mut c = KiroCredentials::default();
        c.refresh_token = Some("super-secret-refresh-token-xyz".to_string());
        let mgr =
            MultiTokenManager::new(config, vec![c], None, Some(cred_path.clone()), true).unwrap();

        // 触发一次 persist(改个字段即回写)。
        mgr.set_priority(1, 3).unwrap();

        // 磁盘上应是密文:带 magic、绝不含明文 refresh_token。
        let raw = std::fs::read(&cred_path).unwrap();
        assert!(
            crate::common::secret_store::is_encrypted(&raw),
            "开启加密后落盘应为密文"
        );
        let raw_str = String::from_utf8_lossy(&raw);
        assert!(
            !raw_str.contains("super-secret-refresh-token-xyz"),
            "密文不应含明文 token"
        );

        // 重载能解密还原(透明迁移的反向:密文→明文→解析)。
        let reloaded = crate::kiro::model::credentials::CredentialsConfig::load(&cred_path)
            .unwrap()
            .into_sorted_credentials();
        assert_eq!(reloaded.len(), 1);
        assert_eq!(
            reloaded[0].refresh_token.as_deref(),
            Some("super-secret-refresh-token-xyz")
        );
        assert_eq!(reloaded[0].priority, 3);

        std::fs::remove_dir_all(&dir).ok();
    }

    /// 迁移兼容:关闭加密(默认)时落盘仍是明文;已有明文文件能被读(直通,不因加密崩)。
    #[test]
    fn test_at_rest_disabled_stays_plaintext_and_reads_legacy() {
        let dir = std::env::temp_dir().join(format!("kiro-enc-off-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let cred_path = dir.join("credentials.json");
        // 预置一个老的明文 credentials.json(模拟升级前用户)。
        std::fs::write(
            &cred_path,
            r#"[{"id":1,"refreshToken":"legacy-plain-token"}]"#,
        )
        .unwrap();

        // 明文照旧能读(直通)。
        let loaded = crate::kiro::model::credentials::CredentialsConfig::load(&cred_path)
            .unwrap()
            .into_sorted_credentials();
        assert_eq!(
            loaded[0].refresh_token.as_deref(),
            Some("legacy-plain-token")
        );

        // 加密关(默认)→ persist 后仍是明文(不惊扰现有用户)。
        let mgr = MultiTokenManager::new(
            Config::default(),
            loaded,
            None,
            Some(cred_path.clone()),
            true,
        )
        .unwrap();
        mgr.set_priority(1, 7).unwrap();
        let raw = std::fs::read(&cred_path).unwrap();
        assert!(
            !crate::common::secret_store::is_encrypted(&raw),
            "加密关时应保持明文"
        );
        assert!(String::from_utf8_lossy(&raw).contains("legacy-plain-token"));

        std::fs::remove_dir_all(&dir).ok();
    }

    /// TEMP ADVERSARIAL REPRO (F1): 池中只有 custom_api 号时 acquire_context 是否忙等不返回。
    #[test]
    fn temp_repro_f1_custom_api_only_busy_loop() {
        let (tx, rx) = std::sync::mpsc::channel::<&'static str>();
        std::thread::spawn(move || {
            let mut config = Config::default();
            config.load_balancing_mode = "balanced".to_string();
            let mut c = KiroCredentials::default();
            c.id = Some(1);
            c.auth_method = Some("custom_api".to_string());
            c.base_url = Some("https://relay.example.invalid".to_string());
            c.api_key = Some("sk-1".to_string());
            let mgr = MultiTokenManager::new(config, vec![c], None, None, false).unwrap();
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let r = rt.block_on(async { mgr.acquire_context(None, None).await });
            let _ = tx.send(if r.is_ok() { "ok" } else { "err" });
        });
        match rx.recv_timeout(std::time::Duration::from_secs(5)) {
            Ok(v) => println!("F1-REPRO: acquire_context 返回了 {v}（未忙等）"),
            Err(_) => println!("F1-REPRO: acquire_context 5s 内未返回（忙等热循环成立）"),
        }
    }

    /// TEMP ADVERSARIAL REPRO (F1b): 所有号对该模型进 model_blocklist 时是否忙等。
    #[test]
    fn temp_repro_f1b_model_blocked_busy_loop() {
        let (tx, rx) = std::sync::mpsc::channel::<&'static str>();
        std::thread::spawn(move || {
            let mut config = Config::default();
            config.load_balancing_mode = "balanced".to_string();
            let mut c = KiroCredentials::default();
            c.id = Some(1);
            c.access_token = Some("tok-1".to_string());
            c.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());
            let mgr = MultiTokenManager::new(config, vec![c], None, None, false).unwrap();
            mgr.report_model_invalid(1, Some("claude-opus-4.8"));
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let r = rt.block_on(async { mgr.acquire_context(Some("claude-opus-4.8"), None).await });
            let _ = tx.send(if r.is_ok() { "ok" } else { "err" });
        });
        match rx.recv_timeout(std::time::Duration::from_secs(5)) {
            Ok(v) => println!("F1b-REPRO: acquire_context 返回了 {v}（未忙等）"),
            Err(_) => println!("F1b-REPRO: acquire_context 5s 内未返回（忙等热循环成立）"),
        }
    }

    /// ⭐ 承重：并发预留同一分身组的序号**绝不重号**。
    ///
    /// 回退即 FAIL：把 `reserve_clone_seqs` 改回「只扫 entries 取 max + 1」
    /// （即删掉高水位表的前进），下面的唯一性断言必失败 —— 本次预留的份还没入池，
    /// 所以后来者扫到的 max 与前一位完全相同，8 个任务全部拿到 `1`，
    /// 同一组里出现 8 个「分身 #1」，管理页无法区分、删除时无法指名。
    ///
    /// 注意这条**不依赖线程调度**：待建的份从头到尾都不在 entries 里，
    /// 所以旧算法是**确定性**重号，而非偶发竞态。
    #[test]
    fn concurrent_clone_seq_reservations_never_overlap() {
        let mut c = KiroCredentials::default();
        c.id = Some(1);
        c.auth_method = Some("api_key".to_string());
        c.kiro_api_key = Some("ksk_seq".to_string());
        let mgr = std::sync::Arc::new(
            MultiTokenManager::new(Config::default(), vec![c], None, None, false).unwrap(),
        );

        const TASKS: u32 = 8;
        const PER_TASK: u32 = 4;
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()
            .unwrap();
        let starts: Vec<u32> = rt.block_on(async {
            let mut handles = Vec::new();
            for _ in 0..TASKS {
                let mgr = mgr.clone();
                handles.push(tokio::spawn(async move {
                    mgr.reserve_clone_seqs("grp-a", PER_TASK)
                }));
            }
            let mut out = Vec::new();
            for h in handles {
                out.push(h.await.expect("预留任务不应 panic"));
            }
            out
        });

        // 把每个任务拿到的号段摊平，断言全局无重号。
        let mut all: Vec<u32> = starts
            .iter()
            .flat_map(|s| (*s..*s + PER_TASK).collect::<Vec<u32>>())
            .collect();
        let total = all.len();
        all.sort_unstable();
        all.dedup();
        assert_eq!(
            all.len(),
            total,
            "并发预留出现重号：起始号 {starts:?}（旧算法下 8 个任务全部返回 1，\
             同一组里会有 8 个「分身 #1」）"
        );
        // 号段必须连续覆盖 1..=TASKS*PER_TASK：既不重号也不留空洞
        // （留空洞会让管理页出现 `#1 #2 #5 #6` 这种看起来像"丢了两份"的编号）。
        assert_eq!(all.first().copied(), Some(1), "首号应为 1");
        assert_eq!(
            all.last().copied(),
            Some(TASKS * PER_TASK),
            "号段应无空洞地覆盖到 {}",
            TASKS * PER_TASK
        );
    }

    /// 🔴 真并发（非顺序模拟）：透传池选号在 N 任务并发下的记账守恒与无撕裂。
    ///
    /// # 为什么必须多 OS 线程 + Barrier
    /// `select_custom_api` 是纯同步函数（候选过滤、选中、inflight 占位、RPM 记账全在
    /// 同一 entries 锁临界区，:3481-3482 注释背书）。单线程下 N 个调用串行执行锁内
    /// 代码，竞态不可复现 → 测试假绿（仿 concurrent_import 先例，2026-08-15 审计
    /// 判定选号并发是「高」空白区）。用 `std::thread` 真并行 + Barrier 对齐起跑点
    /// （同步函数无需 tokio runtime；若放 tokio::spawn 里用 std Barrier 阻塞 worker
    /// 会在 8 任务 × 4 worker 下死锁——任务数 > worker 数）。
    ///
    /// # 两个阶段
    /// **阶段 A（并发互见）**：8 线程 Barrier 对齐后各选 1 次、guard 全部持有。
    /// 排序键末位是 inflight——锁内占位让后到的线程**看得见**前一个线程的占位，
    /// 8 次选号必须铺满 8 个不同号：每个 entry 的 inflight ≤ 1。
    /// 回退即 FAIL：把占位移出临界区（旧 bug 形态：选号与记账之间隔一个上游 RTT
    /// 的惊群窗口）→ 并发下多个线程都以为自己是唯一选中者 → 某号 inflight ≥ 2。
    ///
    /// **阶段 B（高量级守恒）**：8 线程各 100 次选号、guard 全部持有（模拟 800 个
    /// 在途请求），断言总 inflight == 成功选号总次数（每次恰好 +1，无丢失/双计），
    /// 全部 drop 后归零（无泄漏、无下溢回绕成天文数字——saturating_sub 只防回绕，
    /// 泄漏仍会被归零断言抓到），且 RPM 各号 60s 窗口计数之和 == 成功选号总次数
    /// （每次成功恰好 record 一次；测试毫秒级完成，窗口内无过期 prune）。
    ///
    /// 时长：纯内存（无磁盘无网络），毫秒级。
    #[test]
    fn concurrent_select_custom_api_never_overlaps_or_leaks() {
        use std::collections::HashSet;
        use std::sync::Barrier;

        const POOL: usize = 8;
        const TASKS: usize = 8;
        const PER_TASK: usize = 100;
        const BASE_ID: u64 = 9000; // 唯一 id 段：与全仓其它测试隔离（rg 核过无占用）

        let mk = |id: u64| {
            let mut c = KiroCredentials::default();
            c.id = Some(id);
            c.auth_method = Some("custom_api".to_string());
            c.base_url = Some(format!("https://relay{id}.example.invalid"));
            c.api_key = Some(format!("sk-{id}"));
            c
        };
        let pool_ids: Vec<u64> = (0..POOL).map(|i| BASE_ID + i as u64).collect();
        let mgr = std::sync::Arc::new(
            MultiTokenManager::new(
                Config::default(),
                pool_ids.iter().map(|&id| mk(id)).collect(),
                None,
                None,
                false,
            )
            .unwrap(),
        );

        // ── 阶段 A：并发单次选号，占位必须互见（每号 inflight ≤ 1）──
        let barrier = std::sync::Arc::new(Barrier::new(TASKS));
        let mut handles = Vec::new();
        for _ in 0..TASKS {
            let mgr = mgr.clone();
            let barrier = barrier.clone();
            handles.push(std::thread::spawn(move || {
                barrier.wait(); // 真并发对齐起跑点
                let empty = HashSet::new();
                mgr.select_custom_api(&empty, Some("claude-sonnet-4.5"))
            }));
        }
        let mut stage_a: Vec<(u64, InflightGuard)> = Vec::new();
        for h in handles {
            stage_a.push(
                h.join()
                    .expect("并发选号线程不应 panic")
                    .map(|(id, _cred, guard)| (id, guard))
                    .expect("池内 8 号必可选到"),
            );
        }
        assert_eq!(stage_a.len(), TASKS, "8 个并发选号必须全部成功");
        let entries = mgr.entries.lock();
        for e in entries.iter() {
            assert!(
                e.inflight.load(Ordering::Acquire) <= 1,
                "并发单次选号后凭据 #{} inflight={}（>1）：占位与选号分离，\
                 后到线程看不见前一个线程的占位（旧 bug 形态）",
                e.id,
                e.inflight.load(Ordering::Acquire)
            );
        }
        drop(entries);
        // RPM 基线：阶段 A 恰好 TASKS 次成功选号 → 恰好 TASKS 次 record。
        let rpm_after_a: u32 = pool_ids.iter().map(|id| mgr.rpm.count(*id)).sum();
        assert_eq!(rpm_after_a as usize, TASKS, "阶段 A 的记账必须精确");
        drop(stage_a); // 归零，进入阶段 B

        // ── 阶段 B：高量级并发选号，记账守恒 ──
        let barrier = std::sync::Arc::new(Barrier::new(TASKS));
        let mut handles = Vec::new();
        for _ in 0..TASKS {
            let mgr = mgr.clone();
            let barrier = barrier.clone();
            handles.push(std::thread::spawn(move || {
                barrier.wait(); // 真并发对齐起跑点
                let empty = HashSet::new();
                let mut picked: Vec<(u64, InflightGuard)> = Vec::with_capacity(PER_TASK);
                for _ in 0..PER_TASK {
                    if let Some((id, _cred, guard)) =
                        mgr.select_custom_api(&empty, Some("claude-sonnet-4.5"))
                    {
                        picked.push((id, guard));
                    }
                }
                picked
            }));
        }
        let mut results: Vec<Vec<(u64, InflightGuard)>> = Vec::new();
        for h in handles {
            results.push(h.join().expect("并发选号线程不应 panic"));
        }

        // 选中 id 全在池内（结果都是合法凭据，无幻影号）。
        let mut total_picked = 0usize;
        for picked in &results {
            for (id, _) in picked {
                assert!(
                    pool_ids.contains(id),
                    "选中了池外凭据 #{id}（合法段 {BASE_ID}..{}）",
                    BASE_ID + POOL as u64 - 1
                );
            }
            total_picked += picked.len();
        }

        // 记账守恒：guard 全 drop 前总 inflight == 成功选号总次数（无丢失/双计）。
        let inflight_total: u32 = mgr
            .entries
            .lock()
            .iter()
            .map(|e| e.inflight.load(Ordering::Acquire))
            .sum();
        assert_eq!(
            inflight_total as usize,
            total_picked,
            "inflight 记账不守恒：占位 {inflight_total} vs 成功选号 {total_picked}"
        );

        // 归零守恒：guard 全部 drop 后每个 entry 的 inflight == 0（无泄漏/回绕）。
        drop(results);
        let entries = mgr.entries.lock();
        for e in entries.iter() {
            assert_eq!(
                e.inflight.load(Ordering::Acquire),
                0,
                "凭据 #{} inflight 未归零：记账泄漏或下溢回绕",
                e.id
            );
        }

        // RPM 记账守恒：阶段 B 增量 == 阶段 B 成功选号总次数（基线已在阶段 A 后记录）。
        let rpm_total: u32 = pool_ids.iter().map(|id| mgr.rpm.count(*id)).sum();
        assert_eq!(
            (rpm_total - rpm_after_a) as usize,
            total_picked,
            "RPM 记账不守恒：阶段 B 记录 {} 次 vs 成功选号 {total_picked} 次",
            rpm_total - rpm_after_a
        );
    }

    /// 🔴 真并发（非顺序模拟）：同一凭据的条件刷新被并发触发时，防惊群语义保持——
    /// 拿锁后的条件重检对**新鲜 token** 一律 Skipped，零网络分发。
    ///
    /// # 为什么必须 multi_thread + Barrier
    /// `refresh_token_locked` 的 per-credential TokioMutex 只在多 worker 下才有真实
    /// 锁竞争；单线程下 waiter 依序出队，条件重检路径串行跑完，测不出并发问题。
    ///
    /// # 可观测性评估（2026-08-15，诚实边界）
    /// 「刷新恰好执行一次」的完整断言在无网络测试下**不可构造**：刷新成功路径的
    /// endpoint 硬编码 `https://prod.{region}.auth.desktop.kiro.dev`（无注入点），
    /// 失败路径设计上不跳过（刷新失败 → token 未更新 → 后续 waiter 条件重检仍过期
    /// → 各自再刷，这是正确行为）。因此锁的防惊群语义落在两个可观测断言上：
    /// ① 新鲜 token 并发条件刷新 → 全部 Skipped、零网络（本测试）；
    /// ② 过期 token 并发刷新 → 锁不 panic、结果一致（兄弟测试）。
    /// 若回归把条件重检删掉（改回无条件刷新），本测试的 120 字符合法 refresh_token
    /// 会真发网络（成功/失败都是非 Skipped 结果）→ 断言红。
    ///
    /// 时长：零网络（全 Skipped 路径），毫秒级。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_conditional_refresh_of_fresh_token_all_skip() {
        use tokio::sync::Barrier;

        const TASKS: usize = 8;
        const ID: u64 = 9201; // 唯一 id 段：与全仓其它测试隔离

        let mut c = KiroCredentials::default();
        c.id = Some(ID);
        c.refresh_token = Some("r".repeat(120)); // 合法长度：重检若坏会真发网络
        c.access_token = Some("already_fresh".to_string());
        c.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339()); // 新鲜

        let mgr = std::sync::Arc::new(
            MultiTokenManager::new(Config::default(), vec![c], None, None, false).unwrap(),
        );
        let barrier = std::sync::Arc::new(Barrier::new(TASKS));

        let mut tasks = tokio::task::JoinSet::new();
        for _ in 0..TASKS {
            let mgr = mgr.clone();
            let barrier = barrier.clone();
            tasks.spawn(async move {
                barrier.wait().await; // 真并发对齐起跑点
                mgr.refresh_token_locked(ID, Some(10)).await
            });
        }
        let mut outcomes = Vec::new();
        while let Some(joined) = tasks.join_next().await {
            let outcome = joined.expect("并发条件刷新任务不应 panic");
            outcomes.push(outcome.expect("新鲜 token 条件刷新应 Skipped 而非报错"));
        }
        assert_eq!(outcomes.len(), TASKS);
        assert!(
            outcomes.iter().all(|o| *o == RefreshOutcome::Skipped),
            "并发触发条件刷新必须全部 Skipped（零网络）：防惊群语义在锁竞争下丢失 → 实际 {:?}",
            outcomes
        );
    }

    /// 🔴 真并发（非顺序模拟）：过期凭据被并发触发刷新时，per-credential 刷新锁
    /// 串行化不 panic、结果一致（可观测性边界见兄弟测试注释）。
    ///
    /// 用短 refresh_token（<100，validate 阶段即 bail）保证**零网络**（毫秒级）：
    /// 8 个任务并发走完「拿锁 → 条件重检（过期通过）→ validate bail」全程，
    /// 断言所有任务拿到同一个 validate 错误——锁竞争下结果撕裂/panic 即红。
    ///
    /// 时长：零网络（validate bail），毫秒级。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_refresh_of_expired_credential_errors_consistently() {
        use tokio::sync::Barrier;

        const TASKS: usize = 8;
        const ID: u64 = 9202; // 唯一 id 段：与全仓其它测试隔离

        let mut c = KiroCredentials::default();
        c.id = Some(ID);
        c.refresh_token = Some("short".to_string()); // <100 → validate 阶段即 bail
        c.access_token = Some("stale".to_string());
        c.expires_at = Some("2020-01-01T00:00:00Z".to_string()); // 已过期

        let mgr = std::sync::Arc::new(
            MultiTokenManager::new(Config::default(), vec![c], None, None, false).unwrap(),
        );
        let barrier = std::sync::Arc::new(Barrier::new(TASKS));

        let mut tasks = tokio::task::JoinSet::new();
        for _ in 0..TASKS {
            let mgr = mgr.clone();
            let barrier = barrier.clone();
            tasks.spawn(async move {
                barrier.wait().await; // 真并发对齐起跑点
                mgr.refresh_token_locked(ID, Some(10)).await
            });
        }
        let mut errs = Vec::new();
        while let Some(joined) = tasks.join_next().await {
            let r = joined.expect("并发刷新任务不应 panic");
            errs.push(
                r.expect_err("过期凭据并发刷新必须全部报错（refreshToken 截断）")
                    .to_string(),
            );
        }
        assert_eq!(errs.len(), TASKS);
        for e in &errs {
            assert!(
                e.contains("refreshToken 已被截断"),
                "所有并发刷新必须拿到同一个 validate 错误（锁竞争下结果撕裂?）实际: {e}"
            );
        }
    }

    /// 🔴 真并发（非顺序模拟）：多线程同时 set_disabled/启用同一凭据 → 锁内原子
    /// 更新、无 panic、最终状态是某一次操作的完整结果（无撕裂）。
    ///
    /// set_disabled 的#10 四件套（disabled / disabled_reason / disabled_at /
    /// quota_exhausted_at）在 entries 锁内一次更新，并发 toggle 后最终状态必然落在
    /// 两种合法组合之一：(true, Some(Manual), Some(_)) 或 (false, None, None)。
    /// 若锁纪律回归成字段分步写，并发下能观察到撕裂组合 → 断言红。
    ///
    /// 收尾：并发结束后主线程再跑一次完整 set_disabled(id, true)，断言四件套齐全，
    /// 证明并发竞争未损坏结构；随后复位启用。
    ///
    /// `set_disabled` 是同步函数，用 `std::thread` 真并行（不用 tokio::spawn +
    /// std Barrier——任务数 > worker 数时会死锁）。
    ///
    /// 时长：纯内存（credentials_path=None → persist 直接 Ok(false)，零磁盘），毫秒级。
    #[test]
    fn concurrent_set_disabled_toggles_leave_consistent_state() {
        use std::sync::atomic::AtomicU32;
        use std::sync::Barrier;

        const TASKS: usize = 8;
        const ROUNDS: u32 = 50;
        const ID: u64 = 9101; // 唯一 id 段：与全仓其它测试隔离

        let mut c = KiroCredentials::default();
        c.id = Some(ID);
        c.auth_method = Some("custom_api".to_string());
        c.base_url = Some("https://relay.example.invalid".to_string());
        c.api_key = Some("sk-relay".to_string());
        let mgr = std::sync::Arc::new(
            MultiTokenManager::new(Config::default(), vec![c], None, None, false).unwrap(),
        );

        let barrier = std::sync::Arc::new(Barrier::new(TASKS));
        let done = std::sync::Arc::new(AtomicU32::new(0));
        let mut handles = Vec::new();
        for t in 0..TASKS {
            let mgr = mgr.clone();
            let barrier = barrier.clone();
            let done = done.clone();
            handles.push(std::thread::spawn(move || {
                barrier.wait(); // 真并发对齐起跑点
                // 每线程以不同相位交替 toggle（50 轮内禁用/启用都被反复写入）。
                for i in 0..ROUNDS {
                    mgr.set_disabled(ID, (t as u32 + i) % 2 == 0).unwrap();
                }
                done.fetch_add(1, Ordering::Release);
            }));
        }
        for h in handles {
            h.join().expect("并发 toggle 线程不应 panic");
        }
        assert_eq!(done.load(Ordering::Acquire), TASKS as u32, "所有线程应完成");

        // 最终状态必须是合法组合之一（锁内原子更新，无撕裂）。
        let entries = mgr.entries.lock();
        let e = entries.iter().find(|e| e.id == ID).expect("凭据应在池内");
        match (e.disabled, &e.disabled_reason, &e.disabled_at) {
            (true, Some(DisabledReason::Manual), Some(_)) => {}
            (false, None, None) => {}
            other => panic!("并发 toggle 后四件套撕裂: {:?}", other),
        }
        drop(entries);

        // 收尾：并发后执行一次完整禁用，四件套必须齐全（结构未被并发写坏）。
        mgr.set_disabled(ID, true).unwrap();
        let entries = mgr.entries.lock();
        let e = entries.iter().find(|e| e.id == ID).unwrap();
        assert_eq!(e.disabled, true, "收尾禁用应生效");
        assert_eq!(e.disabled_reason, Some(DisabledReason::Manual), "禁用原因必须为 Manual");
        assert!(e.disabled_at.is_some(), "禁用时刻必须落盘（#10 四件套契约）");
        drop(entries);
        mgr.set_disabled(ID, false).unwrap(); // 复位，防污染其它测试
    }

    /// 重启后（高水位表为空）必须从 entries 里既有 seq 的**之后**接着发号。
    ///
    /// 回退即 FAIL：删掉 `reserve_clone_seqs` 里那句 `.max(floor)` —— 重启后第一次
    /// 加分身会从 1 重新发，与磁盘上已有的 #1/#2 撞号。
    #[test]
    fn reservation_resumes_after_restart_from_persisted_seqs() {
        let mk = |id: u64, seq: u32| {
            let mut c = KiroCredentials::default();
            c.id = Some(id);
            c.auth_method = Some("api_key".to_string());
            c.kiro_api_key = Some("ksk_resume".to_string());
            c.clone_group = Some("grp-b".to_string());
            c.clone_seq = Some(seq);
            c
        };
        // 模拟"重启后加载到磁盘上已有 #1 #2 #3"：高水位表此刻是空的。
        let mgr = MultiTokenManager::new(
            Config::default(),
            vec![mk(1, 1), mk(2, 2), mk(3, 3)],
            None,
            None,
            false,
        )
        .unwrap();
        assert_eq!(
            mgr.reserve_clone_seqs("grp-b", 2),
            4,
            "重启后必须接着已持久化的 max(3) 发号，而不是从 1 重来"
        );
        // 第二次预留接着走（此刻走的是内存高水位，而非 entries —— 上一段的 #4 #5 还没入池）。
        assert_eq!(mgr.reserve_clone_seqs("grp-b", 1), 6);
        // 不同组各自独立记账，互不影响。
        assert_eq!(mgr.reserve_clone_seqs("grp-c", 1), 1, "另一个组应从 1 起");
    }

    /// 🔴 回归（T1）：`report_auth_transient_cooldown` 必须落**短且可自愈**的冷却，
    /// 与 `report_auth_cooldown` 的 24h 硬窗形成可观测的差异。
    ///
    /// **旧代码为何 FAIL**：这个入口此前不存在，provider 的四处 401/403 全走
    /// `report_auth_cooldown` ⇒ 一律 `AuthenticationFailed`（`is_auto_recoverable=false`
    /// ⇒ `calculate_cooldown_duration` 走 `long_cooldown_secs` = 86400s）。
    /// 把本函数改回调用 `report_auth_cooldown` 后，reason 与时长两个断言都会 FAIL。
    #[test]
    fn transient_auth_cooldown_is_short_and_auto_recoverable() {
        let mgr = MultiTokenManager::new(Config::default(), vec![], None, None, false).unwrap();

        mgr.report_auth_transient_cooldown(1);
        let (reason, remaining) = mgr
            .cooldown
            .check_cooldown(1)
            .expect("瞬态认证失败必须真的落一段冷却（否则调度会立刻选回它空撞）");
        assert_eq!(
            reason,
            CooldownReason::AuthTransient,
            "必须用 AuthTransient 而非 AuthenticationFailed：后者 is_auto_recoverable=false，\
             实际落 86400s 硬窗 = 面板上的冷却僵尸"
        );
        assert!(
            reason.is_auto_recoverable(),
            "瞬态认证失败必须可自愈（决定 fallback_cooldown_tier 判 Shallow 而非 Deep）"
        );
        assert!(
            remaining <= std::time::Duration::from_secs(120),
            "瞬态冷却必须是秒级，实得 {remaining:?}"
        );

        // 对照组：永久型入口仍必须是 24h 量级的不可自愈冷却（防「把两者一起改短」）。
        mgr.report_auth_cooldown(2);
        let (r2, rem2) = mgr.cooldown.check_cooldown(2).expect("永久型也应落冷却");
        assert_eq!(r2, CooldownReason::AuthenticationFailed);
        assert!(
            rem2 > std::time::Duration::from_secs(3600),
            "AuthenticationFailed 必须仍是长硬窗（真废掉的号不该每 20s 回池猛打上游），实得 {rem2:?}"
        );
    }

    /// 🔴 回归（T1，**分支顺序**级）：provider 的 401/403 路径里，
    /// `report_auth_cooldown`（24h 硬窗）只允许出现在 `has_ever_succeeded` 的 **else** 侧。
    ///
    /// 为什么用源码守卫而非行为测试：这四个点位都在 `call_api_with_retry` /
    /// MCP 循环内部，要走到它们必须打真实上游（本仓铁律禁止测试依赖网络）。
    /// 而本缺陷本身就在「哪一行调哪个函数」，源码断言足以锁死。
    ///
    /// **旧代码为何 FAIL**：四处全是裸 `report_auth_cooldown`，
    /// `report_auth_transient_cooldown` 在 provider.rs 里 0 命中 —— 第一个断言即失败。
    #[test]
    fn provider_401_403_paths_must_use_transient_cooldown_where_proven() {
        let src = include_str!("provider.rs");
        let cut = src.find("#[cfg(test)]").unwrap_or(src.len());
        let prod = &src[..cut];

        // needle 运行时拼接，避免 include_str! 把本测试自己的字面量算进匹配。
        let transient = format!("report_auth_transient_cooldown{}", "(ctx.id)");
        let permanent = format!("report_auth_cooldown{}", "(ctx.id)");
        let n_transient = prod.matches(&transient).count();
        let n_permanent = prod.matches(&permanent).count();

        assert_eq!(
            n_transient, 4,
            "四处 401/403 瞬态路径都必须走 transient 入口（两处 force-refresh 失败但已成功过、\
             一处 bearer_invalid_but_proven、一处 403 FEATURE_NOT_SUPPORTED 后台重探中），实得 {n_transient}"
        );
        assert_eq!(
            n_permanent, 2,
            "24h 硬窗只允许保留在两处 force-refresh 失败的 else 侧（该号从未成功过 ⇒ \
             refreshToken 大概率真废了），实得 {n_permanent}"
        );

        // 顺序/嵌套承重：每个 24h 调用点**上文**必须有 has_ever_succeeded 这道门，
        // 否则「二分」退化成「无条件硬冻」而上面的计数断言仍然通过。
        //
        // ⚠️ 窗口起点必须落在 char 边界上：本文件注释是中文（多字节），
        // 裸 `&s[a..b]` 切进字符中间会 panic（本仓已记录的"纸面测试"形态之一）。
        let proven_gate = format!("has_ever_succeeded{}", "(ctx.id) {");
        for (n, (pos, _)) in prod.match_indices(&permanent).enumerate() {
            let mut start = pos.saturating_sub(600);
            while start < pos && !prod.is_char_boundary(start) {
                start += 1;
            }
            assert!(
                prod[start..pos].contains(&proven_gate),
                "第 {} 处 report_auth_cooldown 的上文没有 has_ever_succeeded 门 —— \
                 24h 硬冻必须只作用于「从未成功过」的号",
                n + 1
            );
        }
    }

    /// 🔴 回归（K4）：`reload_config` 必须把 `default_endpoint` 一起 restore 成启动固化值。
    ///
    /// **旧代码为何 FAIL**：该字段漏出了 restore 白名单（表里当时只有 proxy/tls/host/port/
    /// region/callback/adminKey/apiKey 十项），而 `admin/service.rs` 又把它 push 进
    /// `restart_fields`（= 声明「改它要重启」）。于是改配置时只要同批动了任何热字段触发
    /// reload：ArcSwap 换成**新**端点，而 `KiroProvider` 仍持构造时（`main.rs`）传入的
    /// 拷贝 ⇒ 对话路径走旧端点，`region_probe` 的探测与余额/验活路径（活读
    /// `config().default_endpoint`）走新端点 = split-brain，持续到重启。
    /// 两个端点按凭据类型绑定、不可互换（打错恒 403）。
    ///
    /// 第二个断言是承重的对照组：它坐实 reload **真的跑了**（热字段确实换成了新值）。
    /// 没有它的话，`Config::load` 失败提前 return Err 也会让第一个断言"通过"。
    #[test]
    fn reload_config_must_restore_default_endpoint_to_startup_value() {
        let dir = std::env::temp_dir().join(format!("ks_reload_ep_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");

        // 启动态：端点 ide + 热字段 credentialRpmLimit=111。
        std::fs::write(
            &path,
            r#"{"defaultEndpoint":"ide","credentialRpmLimit":111}"#,
        )
        .unwrap();
        let boot = Config::load(&path).unwrap();
        assert_eq!(boot.default_endpoint, "ide");
        let mgr = MultiTokenManager::new(boot, vec![], None, None, false).unwrap();

        // 磁盘上同批改了：restart-only 的端点 + 一个热字段。
        std::fs::write(
            &path,
            r#"{"defaultEndpoint":"cli","credentialRpmLimit":222}"#,
        )
        .unwrap();
        mgr.reload_config().expect("热重载不应失败");

        assert_eq!(
            mgr.config().default_endpoint,
            "ide",
            "default_endpoint 是 restart-only 固化项：reload 后必须仍等于启动值，\
             否则对话路径（provider 构造时的拷贝）与探测/余额路径（活读 config）分叉 = split-brain"
        );
        assert_eq!(
            mgr.config().credential_rpm_limit,
            222,
            "对照组：热字段必须真的换成新值（否则第一个断言可能只是因为 reload 根本没跑）"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 🔴 回归（K5，blockers #3）：reload 必须把反代安全五件套 restore 成启动固化值。
    ///
    /// **旧代码为何 FAIL**：这 5 项（corsAllowedOrigins / ipAllowlist / trustForwardedHeader /
    /// ingressRateLimitPerMin / maxBodyBytes）进了 `restart_fields`（面板说「要重启」）却漏出
    /// restore 白名单 ⇒ 同批改任何热字段触发 reload 后，ArcSwap 拿到磁盘**新**值、快照显示
    /// 新值，而运行态（CORS layer / DefaultBodyLimit / security 中间件）仍是启动固化旧值 =
    /// 「改了但没生效」的快照说谎（与 proxy split-brain 同型）。修复后 ArcSwap 的这五项
    /// 恒 == 启动值，快照诚实显示运行态真实值。
    #[test]
    fn reload_config_must_restore_reverse_proxy_five_to_startup_value() {
        let dir = std::env::temp_dir().join(format!("ks_reload_rp_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");

        // 启动态：反代五件套旧值 + 热字段 credentialRpmLimit=111。
        std::fs::write(
            &path,
            r#"{"corsAllowedOrigins":["https://a.example"],"ipAllowlist":["1.2.3.4"],"trustForwardedHeader":false,"ingressRateLimitPerMin":10,"maxBodyBytes":1024,"credentialRpmLimit":111}"#,
        )
        .unwrap();
        let boot = Config::load(&path).unwrap();
        assert_eq!(boot.cors_allowed_origins, vec!["https://a.example".to_string()]);
        let mgr = MultiTokenManager::new(boot, vec![], None, None, false).unwrap();

        // 磁盘上同批改了：反代五件套新值 + 一个热字段。
        std::fs::write(
            &path,
            r#"{"corsAllowedOrigins":["https://b.example"],"ipAllowlist":["5.6.7.8"],"trustForwardedHeader":true,"ingressRateLimitPerMin":20,"maxBodyBytes":2048,"credentialRpmLimit":222}"#,
        )
        .unwrap();
        mgr.reload_config().expect("热重载不应失败");

        assert_eq!(
            mgr.config().cors_allowed_origins,
            vec!["https://a.example".to_string()],
            "corsAllowedOrigins 是构造时固化项（build_cors_layer）：reload 后必须仍等于启动值"
        );
        assert_eq!(
            mgr.config().ip_allowlist,
            vec!["1.2.3.4".to_string()],
            "ipAllowlist 是构造时固化项（SecurityState）：reload 后必须仍等于启动值"
        );
        assert_eq!(
            mgr.config().trust_forwarded_header,
            false,
            "trustForwardedHeader 是启动固化项（安全中间件 + 业务镜像）：reload 后必须仍等于启动值"
        );
        assert_eq!(
            mgr.config().ingress_rate_limit_per_min,
            10,
            "ingressRateLimitPerMin 是构造时固化项（IngressRateLimiter）：reload 后必须仍等于启动值"
        );
        assert_eq!(
            mgr.config().max_body_bytes,
            1024,
            "maxBodyBytes 是构造时固化项（DefaultBodyLimit）：reload 后必须仍等于启动值"
        );
        assert_eq!(
            mgr.config().credential_rpm_limit,
            222,
            "对照组：热字段必须真的换成新值（否则上面的断言可能只是因为 reload 根本没跑）"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 🔴 回归（blockers #1）：reload 必须把 compression 播进 handlers 进程镜像。
    ///
    /// **旧代码为何 FAIL**：压缩配置消费点全部读 `handlers::current_compression()` 镜像
    /// （handlers.rs 压缩热路径 5 处），而镜像只有 main 启动播种（router.rs:62），reload
    /// **不播** ⇒ 手改 config.json 的 compression + 面板保存任何热字段触发 reload 后，
    /// ArcSwap config 已是新值（快照说改了）但热路径仍读启动旧镜像（实际没生效）——
    /// error_messages 曾踩过的「只接一半」同型。
    ///
    /// 断言锚用 `trigger_bytes` 而非 `enabled`：handlers.rs 的 roundtrip 测试会翻转
    /// `enabled`，并行跑时用 enabled 断言会被它干扰（全局镜像无锁）。
    /// 同一条 roundtrip 收尾 `set_compression(default())` 会把 `trigger_bytes` 踩回
    /// 4MiB（4194304）——全量并行 T5 曾因此红成 left=4194304 / right=222222，
    /// 看起来像 reload 在播 Default。镜像断言必须仍钉 222222；被踩后只允许再走
    /// `reload_config` 重播（测试里不得 `set_compression(222222)` 顶上去）。
    #[test]
    fn reload_config_must_broadcast_compression_to_handler_mirror() {
        let dir = std::env::temp_dir().join(format!("ks_reload_cmp_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");

        // 启动态：压缩 trigger_bytes=111111 + 热字段 credentialRpmLimit=111；
        // 镜像播种启动值（模拟 main 播种）。
        std::fs::write(
            &path,
            r#"{"compression":{"enabled":false,"triggerBytes":111111},"credentialRpmLimit":111}"#,
        )
        .unwrap();
        let boot = Config::load(&path).unwrap();
        assert_eq!(boot.compression.trigger_bytes, 111111);
        crate::anthropic::handlers::set_compression(boot.compression.clone());
        let mgr = MultiTokenManager::new(boot, vec![], None, None, false).unwrap();

        // 磁盘上同批改了：压缩 trigger_bytes=222222 + 一个热字段。
        std::fs::write(
            &path,
            r#"{"compression":{"enabled":true,"triggerBytes":222222},"credentialRpmLimit":222}"#,
        )
        .unwrap();
        mgr.reload_config().expect("热重载不应失败");

        assert_eq!(
            mgr.config().compression.trigger_bytes,
            222222,
            "reload 必须把磁盘 triggerBytes 换进 ArcSwap（不走全局镜像；否则镜像断言无法区分没 load 和被别的测试踩了）"
        );
        assert_eq!(
            mgr.config().credential_rpm_limit,
            222,
            "对照组：热字段必须真的换成新值（否则上面的断言可能只是因为 reload 根本没跑）"
        );

        let mut got = crate::anthropic::handlers::current_compression().trigger_bytes;
        for _ in 0..64 {
            if got == 222222 {
                break;
            }
            mgr.reload_config().expect("热重载不应失败");
            got = crate::anthropic::handlers::current_compression().trigger_bytes;
        }
        assert_eq!(
            got,
            222222,
            "reload 必须把新 compression 播进 handlers 镜像（消费点读镜像不读 config）"
        );

        // 复位镜像默认，避免影响其它测试（与 handlers 内 roundtrip 测试同款收尾）。
        crate::anthropic::handlers::set_compression(
            crate::model::config::CompressionConfig::default(),
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 配套源码守卫（K4）：restore 块里必须有 `default_endpoint` 的赋值行。
    ///
    /// 与上面的行为测试互补：行为测试证明「当前语义对」，这条锁住「赋值语句还在原处」——
    /// 将来有人把 restore 块重构成宏/列表时，行为测试可能因构造方式变化而被顺手改掉，
    /// 而这条直接盯着源文本。
    #[test]
    fn reload_config_restore_list_must_contain_default_endpoint() {
        let src = include_str!("token_manager.rs");
        let cut = src.find("#[cfg(test)]").unwrap_or(src.len());
        let prod = &src[..cut];
        let fi = prod
            .find("pub fn reload_config")
            .expect("reload_config 不该被改名");
        // restore 块以「刷新热路径原子镜像」注释为界（块结束后就是热字段镜像赋值）。
        let end = prod[fi..]
            .find("刷新热路径原子镜像")
            .map(|i| i + fi)
            .expect("restore 块后的锚点注释不该被删");
        let block = &prod[fi..end];
        // needle 运行时拼接，避免 include_str! 把本测试自己的字面量算进匹配。
        let needle = format!("new.default_endpoint = old.default_endpoint{}", ".clone()");
        assert!(
            block.contains(&needle),
            "restore 块必须把 default_endpoint 覆盖回启动值：\
             它已在 admin/service.rs 被声明为 restart-only，漏掉即 split-brain"
        );
    }

    /// 配套源码守卫（blockers #1）：`reload_config` 必须调 `set_compression` 播压缩镜像。
    ///
    /// 与上面的行为测试互补：行为测试证明「当前语义对」，这条锁住「调用还在 reload_config
    /// 里、且没有被注释掉」——将来有人删掉/注释掉这行，热路径会退回启动旧镜像而测试
    /// （行为测试）不一定红（可能没有 compression 相关的其它测试）。needle 运行时拼接 +
    /// 剔注释行：注释掉调用后守卫必须 FAIL（防 include_str 自证绿，守卫纪律见 CURRENT.md）。
    #[test]
    fn reload_config_must_broadcast_compression_setter() {
        let src = include_str!("token_manager.rs");
        let cut = src.find("#[cfg(test)]").unwrap_or(src.len());
        let prod = &src[..cut];
        let fi = prod
            .find("pub fn reload_config")
            .expect("reload_config 不该被改名");
        // 函数尾锚点：reload_config 的下一个函数。限定在函数体内，防止将来调用被
        // 挪出 reload_config 后守卫仍然绿（误绿比报错更危险）。
        let fn_end = prod[fi..]
            .find("pub fn respawn_refresh_task")
            .map(|i| i + fi)
            .expect("respawn_refresh_task 不该被改名或挪位");
        let block_raw = &prod[fi..fn_end];
        // 剔注释行：include_str! 读的是原始源文本（含注释），不剔则「注释掉的调用」也算匹配。
        let block: String = block_raw
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        // needle 运行时拼接：include_str! 会把本测试自己的字面量也读进来。
        let needle = format!("set_compression(new.compression{}", ".clone())");
        assert!(
            block.contains(&needle),
            "reload_config 必须调 set_compression 把新 compression 播进 handlers 镜像：\
             压缩消费点全部读镜像（handlers.rs current_compression）不读 config，\
             漏播 ⇒ 手改 config.json + 面板保存触发 reload 后热路径仍读启动旧镜像"
        );
    }

    /// 🔴 **通用映射守卫**：凡进 `restart_fields` 的字段，必须**要么**在 reload 的 restore 块里、
    /// **要么**在本测试的豁免名单里（附理由）。
    ///
    /// # 为什么需要这条（它的价值超过它守的任何单个字段）
    ///
    /// `admin/service.rs` 的 `restart_fields.push(...)` 与 `reload_config` 的 restore 块是
    /// **两份手工维护的名单**，靠人记住同步。实测已漏过**四项**：
    /// - `default_endpoint`（对话走旧端点，而 region 探测/余额活读新端点）
    /// - `kiro_version` / `system_version` / `node_version`（`endpoint/ide.rs` 每请求活读，
    ///   且它们是上游请求指纹的组成部分 —— 指纹在飞行中途变化正是风控关注的形态）
    ///
    /// 这类遗漏**没有任何测试能发现**：两边各自都"看起来对"，缺陷只存在于映射关系里。
    /// 本守卫把那个映射变成显式断言，将来加 restart-only 字段时必须做一次自觉决策。
    ///
    /// # 判据：为什么不是「凡 restart_fields 必须 restore」
    ///
    /// restore 的目的是**消除 split-brain**：某条路径读启动固化值、另一条活读 ArcSwap。
    /// 只有**存在请求路径活读**的字段才需要 restore。本名单当前**为空**——曾豁免的反代
    /// 安全五件套（corsAllowedOrigins / ipAllowlist / trustForwardedHeader /
    /// ingressRateLimitPerMin / maxBodyBytes）2026-08-15 已补进 restore 表：虽然它们同样
    /// 只在 main.rs 启动时固化进 router layer / 运行态镜像、无请求路径活读，但 ArcSwap
    /// 那份还喂**面板快照**——不 restore 则混改触发 reload 后快照显示磁盘新值而运行态
    /// 仍是旧值（「改了但没生效」的快照说谎，与 proxy split-brain 同型，blockers #3）。
    /// 将来再加 restart-only 字段必须做一次自觉决策：有活读 → 进 restore 表；
    /// 无活读也无快照说谎问题 → 才可写进下面的豁免名单并附理由。
    #[test]
    fn every_restart_only_field_is_restored_or_explicitly_exempt() {
        // 豁免名单：字段名用 `restart_fields.push()` 里的 camelCase 原样写。
        // 当前为空（曾豁免的反代安全五件套已进 restore 表，见本测试文档注释）。
        const EXEMPT: &[&str] = &[];

        let svc = [
            include_str!("../admin/service.rs"),
            include_str!("../admin/config_update.rs"),
        ]
        .concat();
        // 从 service.rs / config_update.rs 抽出全部 restart-only 字段名。
        // needle 运行时拼接，避免把本测试自己的字面量算进匹配。
        let push_marker = format!("restart_fields{}push(\"", ".");
        let mut declared: Vec<&str> = Vec::new();
        for seg in svc.split(&push_marker).skip(1) {
            if let Some(end) = seg.find('"') {
                let name = &seg[..end];
                // 只收合法字段名（camelCase 标识符）。service.rs 自己的守卫测试也用
                // `restart_fields.push("{}` 拼 needle（如 7849/7892 行），其原始文本会
                // 被本提取误抽成 `{}` 假名——不过滤会让守卫对 `new.{} = old.{}` 恒失败。
                if !name.is_empty()
                    && name.chars().all(|c| c.is_ascii_alphanumeric())
                    && !declared.contains(&name)
                {
                    declared.push(name);
                }
            }
        }
        assert!(
            declared.len() >= 10,
            "只抽出 {} 个 restart-only 字段，push 的写法可能变了 —— \
             本守卫失效比它报错更危险，故此处硬失败。抽到的：{:?}",
            declared.len(),
            declared
        );

        // 取 restore 块（与上一条测试同款边界）。
        let src = include_str!("token_manager.rs");
        let cut = src.find("#[cfg(test)]").unwrap_or(src.len());
        let prod = &src[..cut];
        let fi = prod
            .find("pub fn reload_config")
            .expect("reload_config 不该被改名");
        let end = prod[fi..]
            .find("刷新热路径原子镜像")
            .map(|i| i + fi)
            .expect("restore 块后的锚点注释不该被删");
        // 🔴 **必须先剔掉注释行再匹配**。`include_str!` 读的是原始源文本（含注释），
        // 直接 `contains` 会匹配到**被注释掉**的赋值 ⇒ 把实现注释掉守卫仍然绿。
        // 这是本仓记录的「纸面测试」形态，写本测试时**实测踩到过**：注释掉三行版本串赋值后
        // 守卫仍报 ok，剔注释后才正确 FAIL。`provider.rs` 的源码守卫也是先剔注释行的。
        let block_raw = &prod[fi..end];
        let block: String = block_raw
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        let block = block.as_str();

        let mut missing: Vec<String> = Vec::new();
        for camel in &declared {
            if EXEMPT.contains(camel) {
                continue;
            }
            // camelCase → snake_case（Config 的字段名）。
            let mut snake = String::new();
            for ch in camel.chars() {
                if ch.is_ascii_uppercase() {
                    snake.push('_');
                    snake.push(ch.to_ascii_lowercase());
                } else {
                    snake.push(ch);
                }
            }
            // 只断言"该字段被从 old 覆盖回 new"，不限定有无 .clone()（Copy 类型如 port 没有）。
            let needle = format!("new.{} = old.{}", snake, snake);
            if !block.contains(&needle) {
                missing.push(format!("{camel} (期望 `{needle}`)"));
            }
        }

        assert!(
            missing.is_empty(),
            "以下字段已声明为 restart-only 却未在 reload 的 restore 块里覆盖回启动值，\
             也不在豁免名单中 ⇒ 改配置后只要同批动了任何热字段触发 reload，\
             ArcSwap 会拿到新值而启动固化的那条路径仍是旧值 = split-brain：\n  {}\n\
             修法二选一：① 在 restore 块加 `new.X = old.X`（若有请求路径活读）\
             ② 加进本测试的 EXEMPT 并写明为什么无需 restore（须核实确无活读）。",
            missing.join("\n  ")
        );
    }

    /// 源码级守卫：`所有凭据均无法获取有效 Token` 这条 bail 必须带 `retry_after_secs=`。
    ///
    /// 否则该串匹配不上 `map_provider_error` 的任何分支（无标记、无上游关键词）→ 落 502 无
    /// Retry-After，客户端（Claude Code）把 502 当「服务端故障」、退避逻辑不启动、原样重发 ——
    /// 与「所有凭据均已禁用」落 502 是同一类缺陷（见 acquire_context 内 NoCandidate 那两处注释）。
    #[test]
    fn bail_all_credentials_token_failure_carries_retry_after_marker() {
        let src = include_str!("token_manager.rs");
        let cut = src.find("#[cfg(test)]").unwrap_or(src.len());
        let prod = &src[..cut];
        // needle 运行时拼接：include_str! 会把测试自己的字面量也读进来（本仓库踩过三次）。
        let needle = format!(
            "所有凭据均无法获取有效 Token（可用: {{}}/{{}}）retry_after_secs={}",
            "{}"
        );
        assert!(
            prod.contains(&needle),
            "`所有凭据均无法获取有效 Token` bail 必须带 retry_after_secs= 标记，\
             否则 map_provider_error 匹配不上任何分支 → 502 无 Retry-After，\
             客户端把 502 当服务端故障、不退避直接重发"
        );
    }

    /// `count = 0` 只查询下一个可用号，不占号（供只想看编号的调用方用）。
    #[test]
    fn zero_count_reservation_does_not_consume() {
        let mgr = MultiTokenManager::new(Config::default(), vec![], None, None, false).unwrap();
        assert_eq!(mgr.reserve_clone_seqs("g", 0), 1);
        assert_eq!(mgr.reserve_clone_seqs("g", 0), 1, "count=0 不得推进高水位");
        assert_eq!(mgr.reserve_clone_seqs("g", 1), 1);
        assert_eq!(
            mgr.reserve_clone_seqs("g", 0),
            2,
            "真占号后下一个可用号应前进"
        );
    }
